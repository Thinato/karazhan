use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use std::io::Stdout;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

use crate::client::SupervisorClient;
use crate::commands::{self, CommandId, NewWorktreeModal, Palette, WorktreeChoice};
use crate::config::Config;
use crate::ipc::{BuiltinKind, ClientMsg, ProjectInfo, WorktreeView};
use crate::ui::grid::GridView;
use crate::ui::keymap::{clamp_selection, Motion};
use crate::ui::library::{LibraryMode, LibraryView, SelectedPrompt};
use crate::ui::terminal::split_grid_pane;
use crate::worktree::WorktreeStatus;

#[derive(Debug)]
pub enum AppEvent {
    Tick,
    #[allow(dead_code)] // reserved for background-task shutdown requests
    Quit,
    /// Full state snapshot from the daemon (handshake / Refresh / create / remove).
    Snapshot {
        /// Ordered projects (name + path); drives grid grouping, the project
        /// picker, and the per-project prompt stores in the library.
        projects: Vec<ProjectInfo>,
        worktrees: Vec<WorktreeView>,
    },
    /// Incremental status update for a single worktree from the daemon.
    WorktreeStatusChanged {
        worktree_path: PathBuf,
        status: WorktreeStatus,
        summary: Option<String>,
        /// Live agent action while Running (e.g. "Editing foo.rs").
        activity: Option<String>,
        /// Assistant turns so far in this run.
        turns: u32,
        /// Cumulative output tokens so far in this run.
        tokens: u64,
        /// Run start time; `Some` only while Running (drives the elapsed timer).
        run_started_at: Option<DateTime<Utc>>,
    },
    /// Non-fatal error surfaced by the daemon (gh failures, etc.).
    DaemonError {
        worktree_path: Option<PathBuf>,
        message: String,
    },
    /// The daemon connection dropped (daemon died or socket closed).
    DaemonDisconnected,
    /// The background watcher observed a change in a worktree's PR status.
    ///
    /// Emitted only by the daemon-side watcher → handled inside the daemon; the
    /// client never receives this variant, but the watcher → daemon channel is
    /// typed as `AppEvent`, so it stays defined here.
    PrStatusChanged {
        worktree_path: PathBuf,
        pr_status: crate::worktree::model::PrStatus,
        pr_number: Option<u64>,
        pr_url: Option<String>,
        pr_title: Option<String>,
        /// Count of UNRESOLVED PR review threads (open PRs only); `None` when no
        /// open PR or the count could not be fetched this tick.
        unresolved_comments: Option<u64>,
    },
}

/// Top-level view the application is currently showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Library,
    Grid,
}

/// Grid input sub-mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridMode {
    /// Normal vim-motion navigation.
    Normal,
    /// Typing a free-text prompt to run against the selected worktree.
    PromptInput,
    /// Typing a new name for the selected worktree.
    NameInput,
}

/// Which side of the Grid view receives keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The worktree grid (vim motions, commands).
    Nav,
    /// The terminal pane: every key except the prefix goes to the PTY.
    Term,
}

/// What a key resolves to after prefix handling (see [`route_key`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    /// Forward the key to the PTY.
    #[allow(dead_code)] // read once input reaches the PTY (phase d)
    Pty(KeyEvent),
    /// Forward a literal prefix key to the PTY (`prefix prefix`).
    PtyPrefix,
    FocusNav,
    FocusTerm,
    /// Create a session in the lowest free slot.
    Create,
    /// Kill the attached session.
    Kill,
    /// Switch to session slot `n` (1–9).
    Select(u8),
    /// Handle the key as a normal grid key.
    Nav(KeyEvent),
    /// Consume the key and do nothing.
    Swallow,
}

/// Route a key through the tmux-style prefix state machine.  `prefix` is the
/// letter of the Ctrl+letter prefix chord.  A pending prefix is cleared by
/// whatever key follows it.
pub fn route_key(focus: Focus, pending: &mut bool, key: KeyEvent, prefix: char) -> KeyAction {
    let is_prefix = key.code == KeyCode::Char(prefix) && key.modifiers == KeyModifiers::CONTROL;
    let term = focus == Focus::Term;
    if !std::mem::take(pending) {
        return if is_prefix {
            *pending = true;
            KeyAction::Swallow
        } else if term {
            KeyAction::Pty(key)
        } else if key.code == KeyCode::Enter {
            KeyAction::FocusTerm
        } else {
            KeyAction::Nav(key)
        };
    }
    if is_prefix {
        return if term {
            KeyAction::PtyPrefix
        } else {
            KeyAction::Swallow
        };
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return KeyAction::Swallow;
    }
    match key.code {
        KeyCode::Char('h') if term => KeyAction::FocusNav,
        KeyCode::Char('l') => KeyAction::FocusTerm,
        KeyCode::Char('c') => KeyAction::Create,
        KeyCode::Char('x') if term => KeyAction::Kill,
        KeyCode::Char(ch @ '1'..='9') => KeyAction::Select(ch as u8 - b'0'),
        _ => KeyAction::Swallow,
    }
}

/// The worktree pending deletion, held while the (y/N) confirmation modal is open.
pub struct DeleteTarget {
    /// Canonical filesystem path of the worktree to delete.
    pub path: PathBuf,
    /// Human-facing name shown in the confirmation modal.
    pub name: String,
}

/// State held while the "new worktree from prompt" confirm modal is open.
pub struct ConfirmNewWorktree {
    pub project: String,
    pub slug: String,
    pub title: String,
    pub body: String,
}

impl ConfirmNewWorktree {
    fn from_selected(sp: SelectedPrompt) -> Self {
        Self {
            project: sp.project,
            slug: sp.slug,
            title: sp.title,
            body: sp.body,
        }
    }
}

pub struct App {
    pub running: bool,
    pub view: View,
    event_rx: mpsc::Receiver<AppEvent>,
    library: LibraryView,
    grid: GridView,
    grid_mode: GridMode,
    prompt_input: String,
    /// Local cache of worktree views pushed by the daemon.
    worktrees: Vec<WorktreeView>,
    /// Authoritative ordered project list pushed by the daemon.  Drives grid
    /// grouping (so zero-worktree projects still get a header), the new-worktree
    /// modal's project picker, and the library's per-project prompt stores.
    projects: Vec<ProjectInfo>,
    /// Transient status message shown in the status line.
    /// Persists until replaced or explicitly cleared.
    status_message: Option<String>,
    /// Thin-client handle to the supervisor daemon.
    client: SupervisorClient,
    /// Loaded application config (prompt_dir + colors).
    #[allow(dead_code)] // retained for future color wiring
    config: Config,
    /// Whether the help overlay is currently shown.
    show_help: bool,
    /// Path of a prompt file queued to open in `$EDITOR` (set by the `e` key,
    /// drained by the run loop so the editor runs outside the event stream).
    pending_edit: Option<PathBuf>,
    /// Command palette modal; `Some` while open (Ctrl-P).  Intercepts all keys.
    palette: Option<Palette>,
    /// New-worktree modal; `Some` while open (`n` in Grid).  Intercepts all
    /// keys.  The modal itself owns project + choice selection.
    new_worktree: Option<NewWorktreeModal>,
    /// Add-project path input overlay; `Some` while open (`A`).  Holds the
    /// in-progress path string (prefilled with the cwd).  Intercepts all keys.
    add_project_input: Option<String>,
    /// Pending worktree deletion awaiting (y/N) confirmation.  `Some` while the
    /// confirmation modal is open; `None` otherwise.
    pending_delete: Option<DeleteTarget>,
    /// Pending "new worktree from prompt" awaiting (Y/n) confirmation.  `Some`
    /// while the modal is open; `None` otherwise.
    confirm_new_worktree: Option<ConfirmNewWorktree>,
    /// Monotonic counter advanced on every tick; drives the Running spinner
    /// animation.  Wraps harmlessly.
    spinner_frame: usize,
    /// Width of the Grid-view terminal pane in percent of the terminal width.
    /// Adjustable with `<`/`>` (±5) and `Ctrl-<`/`Ctrl->` (±1), within 10..=80.
    pane_pct: u16,
    /// Last rendered terminal width, captured each draw so the grid column math
    /// in key handlers (which have no frame) matches the rendered split.
    last_term_width: u16,
    /// Which Grid-view side receives keys.
    focus: Focus,
    /// The prefix key was pressed and the next key is a pane command.
    prefix_pending: bool,
    /// Letter of the Ctrl+letter terminal prefix (`a` → `^a`).
    prefix: char,
}

impl App {
    pub fn new(
        event_rx: mpsc::Receiver<AppEvent>,
        config: Config,
        client: SupervisorClient,
    ) -> Self {
        // The library starts empty and fills per-project on the first Snapshot
        // (same as the grid) — prompts now live at <project>/.karazhan/prompts/.
        let library = LibraryView::new();
        let prefix = crate::config::parse_prefix(&config.terminal_prefix);

        Self {
            running: true,
            view: View::Library,
            event_rx,
            library,
            grid: GridView::new(),
            grid_mode: GridMode::Normal,
            prompt_input: String::new(),
            worktrees: Vec::new(),
            projects: Vec::new(),
            status_message: None,
            client,
            config,
            show_help: false,
            pending_edit: None,
            palette: None,
            new_worktree: None,
            add_project_input: None,
            pending_delete: None,
            confirm_new_worktree: None,
            spinner_frame: 0,
            pane_pct: crate::ui::terminal::DEFAULT_PANE_PCT,
            last_term_width: 120,
            focus: Focus::Nav,
            prefix_pending: false,
            prefix,
        }
    }

    /// Display form of the terminal prefix, e.g. `^a`.
    fn prefix_label(&self) -> String {
        format!("^{}", self.prefix)
    }

    /// Set the transient status message (replaces any previous message).
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
    }

    pub async fn run(mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        let mut crossterm_events = EventStream::new();

        while self.running {
            terminal.draw(|frame| {
                let area = frame.area();
                // Remember terminal width so key handlers (no frame) compute the
                // same grid split as the renderer.
                self.last_term_width = area.width;
                match self.view {
                    View::Library => self.library.render(frame),
                    View::Grid => {
                        let [main, bar] =
                            Layout::vertical([Constraint::Min(0), Constraint::Length(1)])
                                .areas(area);
                        let (grid_area, pane_area) = split_grid_pane(main, self.pane_pct);

                        // The grid groups by project NAME; derive the ordered
                        // name list from the ProjectInfo snapshot.
                        let project_names: Vec<String> =
                            self.projects.iter().map(|p| p.name.clone()).collect();
                        self.grid.render(
                            frame,
                            grid_area,
                            &project_names,
                            &self.worktrees,
                            self.spinner_frame,
                        );

                        crate::ui::terminal::render_hint(frame, pane_area, &self.prefix_label());
                        frame.render_widget(
                            Paragraph::new(self.build_status_text())
                                .style(Style::default().fg(Color::DarkGray)),
                            bar,
                        );
                    }
                }

                // Render help overlay on top of whatever view is active.
                if self.show_help {
                    crate::ui::help::render_help(frame, area, &self.prefix_label());
                }

                // Render the command palette on top of everything else.
                if let Some(palette) = &self.palette {
                    crate::ui::palette::render_palette(frame, area, palette);
                }

                // Render the new-worktree modal last (above palette/help).
                if let Some(modal) = &self.new_worktree {
                    crate::ui::palette::render_new_worktree(frame, area, modal);
                }

                // Render the add-project path input on top of everything.
                if let Some(input) = &self.add_project_input {
                    crate::ui::palette::render_add_project(frame, area, input);
                }

                // Render the delete-confirmation modal last (topmost).
                if let Some(target) = &self.pending_delete {
                    crate::ui::palette::render_confirm_delete(frame, area, &target.name);
                }

                // Render the new-worktree-from-prompt confirmation modal (topmost).
                if let Some(cnw) = &self.confirm_new_worktree {
                    crate::ui::palette::render_confirm_new_worktree(
                        frame,
                        area,
                        &cnw.title,
                        &cnw.project,
                    );
                }
            })?;

            tokio::select! {
                maybe_event = crossterm_events.next() => {
                    match maybe_event {
                        Some(Ok(event)) => self.handle_crossterm_event(event).await,
                        Some(Err(e)) => {
                            tracing::error!("crossterm event error: {e}");
                        }
                        None => {
                            self.running = false;
                        }
                    }
                }
                maybe_app_event = self.event_rx.recv() => {
                    match maybe_app_event {
                        Some(event) => self.handle_app_event(event),
                        None => {
                            self.running = false;
                        }
                    }
                }
            }

            // Open the external editor outside the select! so the EventStream's
            // stdin reader does not compete with the editor for terminal input.
            // The stream is dropped before launching and recreated afterwards.
            if let Some(path) = self.pending_edit.take() {
                drop(crossterm_events);
                self.run_editor(terminal, &path).await;
                crossterm_events = EventStream::new();
            }
        }

        // Clean quit: just drop the client (closing the socket).  The daemon
        // keeps running — agent sessions and the watcher survive.
        Ok(())
    }

    /// Suspend the TUI, run `$EDITOR <path>` against the terminal, then restore
    /// the TUI and reload the prompt library.  Falls back to `$VISUAL`.
    async fn run_editor(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>, path: &Path) {
        let editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_default();
        let editor = editor.trim();
        if editor.is_empty() {
            self.library.status = Some("set $EDITOR to edit prompts".to_string());
            return;
        }

        // $EDITOR may carry arguments (e.g. "code -w"); the program is the first
        // whitespace-separated token, the rest are leading args before the path.
        let mut parts = editor.split_whitespace();
        let program = parts.next().unwrap_or(editor);
        let extra_args: Vec<&str> = parts.collect();

        // Leave the alternate screen + raw mode so the editor owns the terminal.
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);

        let status = tokio::process::Command::new(program)
            .args(&extra_args)
            .arg(path)
            .status()
            .await;

        // Re-enter the TUI.  The editor left the terminal in an unknown state
        // and ratatui caches the pre-edit frame, so a plain clear() can leave
        // stale cells.  Rebuild the Terminal from a fresh backend so its buffers
        // are empty and sized to the current screen, then force a full repaint.
        let _ = enable_raw_mode();
        let _ = execute!(std::io::stdout(), EnterAlternateScreen);
        if let Ok(new_terminal) = Terminal::new(CrosstermBackend::new(std::io::stdout())) {
            *terminal = new_terminal;
        }
        let _ = terminal.clear();

        match status {
            Ok(s) if s.success() => {
                self.library.reload_keep_selection();
                self.library.status = Some("prompt saved".to_string());
            }
            Ok(s) => {
                self.library.reload_keep_selection();
                self.library.status = Some(format!("editor exited with status {s}"));
            }
            Err(e) => {
                self.library.status = Some(format!("could not launch editor '{program}': {e}"));
            }
        }
    }

    // -----------------------------------------------------------------------
    // Status line
    // -----------------------------------------------------------------------

    fn build_status_text(&self) -> String {
        if self.grid_mode == GridMode::NameInput {
            return format!(
                "[daemon]  rename worktree: {}█  Enter: save  Esc: cancel",
                self.prompt_input
            );
        }
        if self.grid_mode == GridMode::PromptInput {
            return format!(
                "[daemon]  prompt: {}█  Enter: run  Esc: cancel",
                self.prompt_input
            );
        }
        if let Some(ref msg) = self.status_message {
            return format!("[daemon]  {msg}");
        }
        "[daemon]  Tab: switch view  ?: help  q: quit".to_string()
    }

    // -----------------------------------------------------------------------
    // Crossterm event routing
    // -----------------------------------------------------------------------

    async fn handle_crossterm_event(&mut self, event: Event) {
        if let Event::Key(key) = event {
            // A focused terminal routes every key itself, ahead of Ctrl-C, so
            // Ctrl-C reaches the shell instead of quitting.
            if self.view == View::Grid && self.focus == Focus::Term {
                let action = route_key(self.focus, &mut self.prefix_pending, key, self.prefix);
                self.handle_key_action(action).await;
                return;
            }
            let KeyEvent {
                code, modifiers, ..
            } = key;

            // Ctrl-C always quits regardless of mode/view.  Daemon survives.
            if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
                tracing::info!("quit requested via Ctrl-C");
                self.running = false;
                return;
            }

            // Add-project input: while open it intercepts ALL keys (any view).
            if self.add_project_input.is_some() {
                self.handle_add_project_key(code).await;
                return;
            }

            // Command palette: while open it intercepts ALL keys.
            if self.palette.is_some() {
                self.handle_palette_key(code, modifiers).await;
                return;
            }

            // New-worktree modal: while open it intercepts ALL keys.
            if self.new_worktree.is_some() {
                self.handle_new_worktree_key(code, modifiers).await;
                return;
            }

            // Delete-confirmation modal: while open intercepts ALL keys.
            // Only y/Y confirms; everything else (including N/n/Esc) cancels.
            if self.pending_delete.is_some() {
                match code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        if let Some(target) = self.pending_delete.take() {
                            self.client
                                .send(ClientMsg::RemoveWorktree {
                                    path: target.path,
                                    force: true,
                                })
                                .await;
                            self.set_status("deleting worktree…");
                        }
                    }
                    _ => {
                        self.pending_delete = None;
                        self.set_status("delete cancelled");
                    }
                }
                return;
            }

            // Confirm-new-worktree modal: while open intercepts ALL keys.
            // Enter/y/Y confirms (default YES); n/N/Esc cancels.
            if self.confirm_new_worktree.is_some() {
                match code {
                    KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                        if let Some(cnw) = self.confirm_new_worktree.take() {
                            self.client
                                .send(ClientMsg::NewWorktree {
                                    project: cnw.project,
                                    prompt_slug: Some(cnw.slug),
                                    prompt_body: Some(cnw.body),
                                })
                                .await;
                            self.set_status("creating worktree…");
                            self.view = View::Grid;
                        }
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        self.confirm_new_worktree = None;
                        self.set_status("cancelled");
                    }
                    _ => {} // other keys: modal stays open
                }
                return;
            }

            // Help overlay: ? toggles it; Esc/q close it while open.
            if self.show_help {
                match code {
                    KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                        self.show_help = false;
                    }
                    _ => {}
                }
                return;
            }

            // Global `?` opens help from any view/mode.
            if code == KeyCode::Char('?') {
                self.prefix_pending = false;
                self.show_help = true;
                return;
            }

            // Global Ctrl-P opens the command palette — but only from Normal
            // modes, so it does not steal a character from an active text input.
            if code == KeyCode::Char('p') && modifiers.contains(KeyModifiers::CONTROL) {
                let in_input =
                    self.library.mode != LibraryMode::Normal || self.grid_mode != GridMode::Normal;
                if !in_input {
                    self.prefix_pending = false;
                    self.palette = Some(commands::Palette::open(self.view == View::Grid));
                    return;
                }
            }

            match self.view {
                View::Library => self.handle_library_key(code, modifiers).await,
                View::Grid => self.handle_grid_key(code, modifiers).await,
            }
        }
    }

    // -----------------------------------------------------------------------
    // Command palette
    // -----------------------------------------------------------------------

    /// Handle a key while the command palette is open.  The palette intercepts
    /// every key: navigation, query editing, run (Enter), cancel (Esc).
    async fn handle_palette_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);

        // Ctrl-N / Ctrl-P move the cursor down / up.
        if ctrl {
            match code {
                KeyCode::Char('n') => {
                    if let Some(p) = self.palette.as_mut() {
                        p.move_cursor(1);
                    }
                    return;
                }
                KeyCode::Char('p') => {
                    if let Some(p) = self.palette.as_mut() {
                        p.move_cursor(-1);
                    }
                    return;
                }
                _ => {}
            }
        }

        match code {
            KeyCode::Esc => {
                self.palette = None;
            }
            KeyCode::Down => {
                if let Some(p) = self.palette.as_mut() {
                    p.move_cursor(1);
                }
            }
            KeyCode::Up => {
                if let Some(p) = self.palette.as_mut() {
                    p.move_cursor(-1);
                }
            }
            KeyCode::Enter => {
                let selected = self.palette.take().and_then(|p| p.selected());
                if let Some(id) = selected {
                    self.execute_command(id).await;
                }
            }
            KeyCode::Backspace => {
                if let Some(p) = self.palette.as_mut() {
                    p.query.pop();
                    p.refilter();
                }
            }
            KeyCode::Char(ch) if !ctrl => {
                if let Some(p) = self.palette.as_mut() {
                    p.query.push(ch);
                    p.refilter();
                }
            }
            _ => {}
        }
    }

    /// Handle a key while the new-worktree modal is open.  Mirrors the palette:
    /// navigation, query editing, create (Enter), cancel (Esc).
    async fn handle_new_worktree_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);

        if ctrl {
            match code {
                KeyCode::Char('n') => {
                    if let Some(m) = self.new_worktree.as_mut() {
                        m.move_cursor(1);
                    }
                    return;
                }
                KeyCode::Char('p') => {
                    if let Some(m) = self.new_worktree.as_mut() {
                        m.move_cursor(-1);
                    }
                    return;
                }
                _ => {}
            }
        }

        match code {
            KeyCode::Esc => {
                // Esc in PickChoice (with a project step) goes back to the
                // project picker; otherwise it closes the modal.
                let went_back = self
                    .new_worktree
                    .as_mut()
                    .map(|m| m.back_to_project())
                    .unwrap_or(false);
                if !went_back {
                    self.new_worktree = None;
                }
            }
            KeyCode::Down => {
                if let Some(m) = self.new_worktree.as_mut() {
                    m.move_cursor(1);
                }
            }
            KeyCode::Up => {
                if let Some(m) = self.new_worktree.as_mut() {
                    m.move_cursor(-1);
                }
            }
            KeyCode::Enter => {
                self.new_worktree_enter().await;
            }
            KeyCode::Backspace => {
                if let Some(m) = self.new_worktree.as_mut() {
                    m.query.pop();
                    m.refilter();
                }
            }
            KeyCode::Char(ch) if !ctrl => {
                if let Some(m) = self.new_worktree.as_mut() {
                    m.query.push(ch);
                    m.refilter();
                }
            }
            _ => {}
        }
    }

    /// Handle `Enter` in the new-worktree modal: in `PickProject` advance to the
    /// choice phase; in `PickChoice` send `NewWorktree` and close.
    async fn new_worktree_enter(&mut self) {
        use commands::NewWorktreePhase;

        let phase = match self.new_worktree.as_ref() {
            Some(m) => m.phase(),
            None => return,
        };

        match phase {
            NewWorktreePhase::PickProject => {
                // Resolve the highlighted project's prompts, then advance the
                // modal into PickChoice scoped to ONLY that project's prompts.
                let picked = self
                    .new_worktree
                    .as_ref()
                    .and_then(|m| m.selected_project_row().map(str::to_string));
                if let Some(project) = picked {
                    let choices = self.library.prompts_for_project(&project);
                    if let Some(m) = self.new_worktree.as_mut() {
                        m.set_choices(choices);
                        m.advance_to_choice();
                    }
                }
            }
            NewWorktreePhase::PickChoice => {
                let modal = self.new_worktree.take();
                let Some(modal) = modal else { return };
                let project = modal.selected_project().map(str::to_string);
                let choice = modal.selected_choice().cloned();
                if let (Some(project), Some(choice)) = (project, choice) {
                    let (prompt_slug, prompt_body) = match choice {
                        WorktreeChoice::Blank => (None, None),
                        WorktreeChoice::Prompt { slug, body, .. } => (Some(slug), Some(body)),
                    };
                    self.client
                        .send(ClientMsg::NewWorktree {
                            project,
                            prompt_slug,
                            prompt_body,
                        })
                        .await;
                    self.set_status("creating worktree…");
                }
            }
        }
    }

    /// Handle a key while the add-project path input is open.  Enter submits the
    /// `AddProject` command with the typed path; Esc cancels; typing/Backspace
    /// edit the path.
    async fn handle_add_project_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.add_project_input = None;
            }
            KeyCode::Enter => {
                if let Some(path) = self.add_project_input.take() {
                    let path = path.trim().to_string();
                    if path.is_empty() {
                        self.set_status("add project: empty path");
                    } else {
                        self.client
                            .send(ClientMsg::AddProject {
                                path: PathBuf::from(path),
                            })
                            .await;
                        self.set_status("adding project…");
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(s) = self.add_project_input.as_mut() {
                    s.pop();
                }
            }
            KeyCode::Char(ch) => {
                if let Some(s) = self.add_project_input.as_mut() {
                    s.push(ch);
                }
            }
            _ => {}
        }
    }

    /// Run a command.  Single implementation per command, shared by both the
    /// palette and the inline key handlers.
    ///
    /// The `match` is *exhaustive* (no wildcard): adding a [`CommandId`] variant
    /// without a handler here is a compile error, which is what forces every
    /// new command into the palette.
    async fn execute_command(&mut self, id: CommandId) {
        match id {
            CommandId::SwitchView => match self.view {
                View::Library => self.enter_grid_view(),
                View::Grid => self.view = View::Library,
            },
            CommandId::ToggleHelp => {
                self.show_help = !self.show_help;
            }
            CommandId::Quit => {
                tracing::info!("quit requested via command");
                self.running = false;
            }
            CommandId::StopDaemon => {
                tracing::info!("daemon shutdown requested via command");
                self.client.send(ClientMsg::Shutdown).await;
                self.running = false;
            }
            CommandId::NewPrompt => {
                self.view = View::Library;
                self.library.enter_new_prompt();
            }
            CommandId::EditPrompt => {
                if self.view == View::Library {
                    match self.library.selected_prompt_path() {
                        Some(path) => self.pending_edit = Some(path),
                        None => self.library.status = Some("no prompt selected".to_string()),
                    }
                } else {
                    self.set_status("edit prompt: switch to Library view first");
                }
            }
            CommandId::FilterPrompts => {
                self.view = View::Library;
                self.library.enter_filter();
            }
            CommandId::RefreshWorktrees => {
                self.client.send(ClientMsg::Refresh).await;
                self.set_status("refreshing…");
            }
            CommandId::RefreshPrompts => {
                self.view = View::Library;
                let before = self.library.prompt_count();
                self.library.reload_keep_selection();
                let after = self.library.prompt_count();
                let msg = match after.cmp(&before) {
                    std::cmp::Ordering::Greater => {
                        format!("prompts refreshed — {} new ({after} total)", after - before)
                    }
                    std::cmp::Ordering::Less => {
                        format!(
                            "prompts refreshed — {} removed ({after} total)",
                            before - after
                        )
                    }
                    std::cmp::Ordering::Equal => {
                        format!("prompts refreshed — no change ({after} total)")
                    }
                };
                self.library.status = Some(msg);
            }
            CommandId::RunCustomPrompt => {
                self.view = View::Grid;
                if self.selected_worktree_path().is_some() {
                    self.grid_mode = GridMode::PromptInput;
                    self.prompt_input.clear();
                } else {
                    self.set_status("no worktree selected");
                }
            }
            CommandId::AddressPrComments => {
                self.view = View::Grid;
                self.send_run_builtin(BuiltinKind::AddressPrComments).await;
            }
            CommandId::CheckCi => {
                self.view = View::Grid;
                self.send_run_builtin(BuiltinKind::CheckCi).await;
            }
            CommandId::ResumeSession => {
                self.view = View::Grid;
                self.send_resume_session().await;
            }
            CommandId::ToggleAutoContinue => {
                self.view = View::Grid;
                self.send_toggle_auto_continue().await;
            }
            CommandId::NewWorktree => {
                self.view = View::Grid;
                if self.projects.is_empty() {
                    self.set_status("add a project first (A)");
                } else {
                    // The modal owns project + choice selection.  With one
                    // project it opens straight to that project's choice list;
                    // with more it opens the project picker first and the
                    // choices are filled once a project is picked.
                    let names: Vec<String> = self.projects.iter().map(|p| p.name.clone()).collect();
                    let choices = if names.len() == 1 {
                        self.library.prompts_for_project(&names[0])
                    } else {
                        Vec::new()
                    };
                    self.new_worktree = Some(NewWorktreeModal::new(names, choices));
                }
            }
            CommandId::AddProject => {
                let cwd = std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                self.add_project_input = Some(cwd);
            }
            CommandId::RenameWorktree => {
                self.view = View::Grid;
                if self.selected_worktree_path().is_some() {
                    self.grid_mode = GridMode::NameInput;
                    self.prompt_input.clear();
                } else {
                    self.set_status("no worktree selected");
                }
            }
            CommandId::DeleteWorktree => {
                self.view = View::Grid;
                if let Some(wt) = self.worktrees.get(self.grid.selected) {
                    self.pending_delete = Some(DeleteTarget {
                        path: wt.path.clone(),
                        name: wt.name.clone(),
                    });
                } else {
                    self.set_status("no worktree selected");
                }
            }
            CommandId::OpenPr => {
                self.view = View::Grid;
                match self.worktrees.get(self.grid.selected) {
                    None => self.set_status("no worktree selected"),
                    Some(wt) => match &wt.pr_url {
                        None => self.set_status("no PR for this worktree"),
                        Some(url) => {
                            if let Err(e) = open::that(url) {
                                tracing::warn!("open PR in browser failed: {e}");
                                self.set_status(format!("could not open browser: {e}"));
                            }
                        }
                    },
                }
            }
            CommandId::CopyPrUrl => {
                self.view = View::Grid;
                match self.worktrees.get(self.grid.selected) {
                    None => self.set_status("no worktree selected"),
                    Some(wt) => match wt.pr_url.clone() {
                        None => self.set_status("no PR for this worktree"),
                        Some(url) => {
                            // Note: on X11/Wayland, arboard's clipboard contents may not
                            // survive after the process exits unless a clipboard manager
                            // holds them; while karazhan is running it works fine.
                            match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(url)) {
                                Ok(()) => self.set_status("PR URL copied to clipboard"),
                                Err(e) => {
                                    tracing::warn!("clipboard error: {e}");
                                    self.set_status(format!("clipboard error: {e}"));
                                }
                            }
                        }
                    },
                }
            }
            CommandId::CopyPrUrlWithTitle => {
                self.view = View::Grid;
                match self.worktrees.get(self.grid.selected) {
                    None => self.set_status("no worktree selected"),
                    Some(wt) => match wt.pr_url.clone() {
                        None => self.set_status("no PR for this worktree"),
                        Some(url) => {
                            let text = match &wt.pr_title {
                                Some(t) => format!("{url} - {t}"),
                                None => url.clone(),
                            };
                            match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text)) {
                                Ok(()) => self.set_status("PR URL + title copied to clipboard"),
                                Err(e) => {
                                    tracing::warn!("clipboard error: {e}");
                                    self.set_status(format!("clipboard error: {e}"));
                                }
                            }
                        }
                    },
                }
            }
            CommandId::WidenDetail => {
                self.view = View::Grid;
                self.adjust_pane_pct(5);
            }
            CommandId::NarrowDetail => {
                self.view = View::Grid;
                self.adjust_pane_pct(-5);
            }
            CommandId::CopyResumeCommand => {
                self.view = View::Grid;
                match self.worktrees.get(self.grid.selected) {
                    None => self.set_status("no worktree selected"),
                    Some(wt) => {
                        let path = wt.path.display().to_string();
                        let bin = &self.config.claude_bin;
                        // `--resume <id>` when we know the session; else bare `-c`
                        // (most recent session in that directory).
                        let resume = match &wt.session_id {
                            Some(id) => format!("{bin} --resume {id}"),
                            None => format!("{bin} -c"),
                        };
                        let cmd = format!("cd '{path}' && {resume}");
                        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(cmd)) {
                            Ok(()) => {
                                self.set_status("resume command copied — paste it in a terminal")
                            }
                            Err(e) => {
                                tracing::warn!("clipboard error: {e}");
                                self.set_status(format!("clipboard error: {e}"));
                            }
                        }
                    }
                }
            }
            CommandId::NewWorktreeFromPrompt => match self.library.selected_prompt() {
                Some(sp) => {
                    self.confirm_new_worktree = Some(ConfirmNewWorktree::from_selected(sp));
                }
                None => {
                    self.set_status("no prompt selected");
                }
            },
        }
    }

    /// Filesystem path of the currently selected worktree, if any.
    fn selected_worktree_path(&self) -> Option<PathBuf> {
        self.worktrees
            .get(self.grid.selected)
            .map(|wt| wt.path.clone())
    }

    // -----------------------------------------------------------------------
    // Library key handling
    // -----------------------------------------------------------------------

    async fn handle_library_key(&mut self, code: KeyCode, _modifiers: KeyModifiers) {
        // Clear transient status on any keypress.
        self.status_message = None;
        self.library.status = None;

        if code == KeyCode::Tab {
            self.execute_command(CommandId::SwitchView).await;
            return;
        }

        match self.library.mode {
            LibraryMode::Normal => match code {
                KeyCode::Enter => self.execute_command(CommandId::NewWorktreeFromPrompt).await,
                KeyCode::Char('q') => self.execute_command(CommandId::Quit).await,
                KeyCode::Char('j') | KeyCode::Down => self.library.move_down(),
                KeyCode::Char('k') | KeyCode::Up => self.library.move_up(),
                KeyCode::Char('/') => self.execute_command(CommandId::FilterPrompts).await,
                KeyCode::Char('n') | KeyCode::Char('a') => {
                    self.execute_command(CommandId::NewPrompt).await
                }
                KeyCode::Char('e') => self.execute_command(CommandId::EditPrompt).await,
                KeyCode::Char('r') => self.execute_command(CommandId::RefreshPrompts).await,
                KeyCode::Char('A') => self.execute_command(CommandId::AddProject).await,
                _ => {}
            },
            LibraryMode::Filter => match code {
                KeyCode::Esc => self.library.clear_filter(),
                KeyCode::Backspace => self.library.filter_pop(),
                KeyCode::Char(ch) => self.library.filter_push(ch),
                _ => {}
            },
            LibraryMode::NewPromptProject => match code {
                KeyCode::Esc => self.library.cancel_input(),
                KeyCode::Enter => self.library.confirm_new_prompt_project(),
                KeyCode::Char('j') | KeyCode::Down => self.library.new_prompt_project_move(1),
                KeyCode::Char('k') | KeyCode::Up => self.library.new_prompt_project_move(-1),
                _ => {}
            },
            LibraryMode::NewPrompt => match code {
                KeyCode::Esc => self.library.cancel_input(),
                KeyCode::Enter => {
                    if let Err(e) = self.library.confirm_new_prompt() {
                        tracing::warn!("new prompt error: {e}");
                        self.library.status = Some(format!("error: {e}"));
                        self.library.cancel_input();
                    }
                }
                KeyCode::Backspace => self.library.new_prompt_pop(),
                KeyCode::Char(ch) => self.library.new_prompt_push(ch),
                _ => {}
            },
        }
    }

    // -----------------------------------------------------------------------
    // Grid key handling
    // -----------------------------------------------------------------------

    async fn handle_grid_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        // Name-input sub-mode intercepts all keys first.
        if self.grid_mode == GridMode::NameInput {
            match code {
                KeyCode::Esc => {
                    self.grid_mode = GridMode::Normal;
                    self.prompt_input.clear();
                }
                KeyCode::Enter => {
                    let name = std::mem::take(&mut self.prompt_input);
                    self.grid_mode = GridMode::Normal;
                    if !name.trim().is_empty() {
                        self.send_set_worktree_name(name).await;
                    }
                }
                KeyCode::Backspace => {
                    self.prompt_input.pop();
                }
                KeyCode::Char(ch) => self.prompt_input.push(ch),
                _ => {}
            }
            return;
        }

        // Prompt-input sub-mode intercepts all keys first.
        if self.grid_mode == GridMode::PromptInput {
            match code {
                KeyCode::Esc => {
                    self.grid_mode = GridMode::Normal;
                    self.prompt_input.clear();
                }
                KeyCode::Enter => {
                    let prompt = std::mem::take(&mut self.prompt_input);
                    self.grid_mode = GridMode::Normal;
                    if !prompt.trim().is_empty() {
                        self.send_run_prompt(prompt).await;
                    }
                }
                KeyCode::Backspace => {
                    self.prompt_input.pop();
                }
                KeyCode::Char(ch) => self.prompt_input.push(ch),
                _ => {}
            }
            return;
        }

        // Clear transient status on any grid key.
        self.status_message = None;

        // Prefix routing runs before every grid binding (Ctrl-a would
        // otherwise hit the modifier-blind `a`).
        let key = KeyEvent::new(code, modifiers);
        let (code, modifiers) =
            match route_key(self.focus, &mut self.prefix_pending, key, self.prefix) {
                KeyAction::Nav(key) => (key.code, key.modifiers),
                action => return self.handle_key_action(action).await,
            };

        if code == KeyCode::Tab {
            self.execute_command(CommandId::SwitchView).await;
            return;
        }

        // Derive `cols` from the same split the renderer uses, so motion and
        // render agree on the column count.
        let cols = {
            let grid_w = split_grid_pane(Rect::new(0, 0, self.last_term_width, 1), self.pane_pct)
                .0
                .width;
            crate::ui::grid::GridView::cols_for_width(grid_w)
        };

        let project_names: Vec<String> = self.projects.iter().map(|p| p.name.clone()).collect();

        match code {
            KeyCode::Char(ch @ '0'..='9') => {
                let d = ch as u8 - b'0';
                self.grid.push_digit(d);
            }

            KeyCode::Char('h') | KeyCode::Left => {
                self.grid.clear_pending_count();
                self.grid
                    .apply(Motion::Left, &project_names, &self.worktrees, cols);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.grid.clear_pending_count();
                self.grid
                    .apply(Motion::Right, &project_names, &self.worktrees, cols);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.grid.clear_pending_count();
                self.grid
                    .apply(Motion::Down, &project_names, &self.worktrees, cols);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.grid.clear_pending_count();
                self.grid
                    .apply(Motion::Up, &project_names, &self.worktrees, cols);
            }
            KeyCode::Char('g') => {
                self.grid.clear_pending_count();
                self.grid
                    .apply(Motion::First, &project_names, &self.worktrees, cols);
            }
            KeyCode::Char('G') => {
                self.grid.apply(
                    Motion::Last { count: None },
                    &project_names,
                    &self.worktrees,
                    cols,
                );
            }

            KeyCode::Char('r') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::RefreshWorktrees).await;
            }

            KeyCode::Char('c') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::RunCustomPrompt).await;
            }

            KeyCode::Char('p') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::AddressPrComments).await;
            }

            KeyCode::Char('i') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::CheckCi).await;
            }

            KeyCode::Char('a') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::ToggleAutoContinue).await;
            }

            KeyCode::Char('n') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::NewWorktree).await;
            }

            KeyCode::Char('N') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::RenameWorktree).await;
            }

            KeyCode::Char('d') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::DeleteWorktree).await;
            }

            KeyCode::Char('o') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::OpenPr).await;
            }

            KeyCode::Char('y') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::CopyPrUrl).await;
            }

            KeyCode::Char('Y') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::CopyPrUrlWithTitle).await;
            }

            KeyCode::Char('R') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::ResumeSession).await;
            }

            KeyCode::Char('s') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::CopyResumeCommand).await;
            }

            // Terminal-pane resize.  `<` widens / `>` narrows by 5%; with Ctrl,
            // by 1% (fine).  `<` = wider (the divider moves left).
            KeyCode::Char('<') => {
                self.grid.clear_pending_count();
                if modifiers.contains(KeyModifiers::CONTROL) {
                    self.adjust_pane_pct(1);
                } else {
                    self.execute_command(CommandId::WidenDetail).await;
                }
            }
            KeyCode::Char('>') => {
                self.grid.clear_pending_count();
                if modifiers.contains(KeyModifiers::CONTROL) {
                    self.adjust_pane_pct(-1);
                } else {
                    self.execute_command(CommandId::NarrowDetail).await;
                }
            }

            KeyCode::Char('A') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::AddProject).await;
            }

            // Shift-Q stops the daemon entirely (sessions + watcher), then quits.
            KeyCode::Char('Q') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::StopDaemon).await;
            }

            KeyCode::Char('q') => {
                self.grid.clear_pending_count();
                self.execute_command(CommandId::Quit).await;
            }

            _ => {
                self.grid.clear_pending_count();
            }
        }
    }

    /// Act on a routed terminal-pane key.
    async fn handle_key_action(&mut self, action: KeyAction) {
        match action {
            KeyAction::FocusNav => self.focus = Focus::Nav,
            KeyAction::FocusTerm => self.focus_term(),
            // No sessions exist yet, so no slot is live.
            KeyAction::Select(n) => self.set_status(format!("no session {n}")),
            // wired in phase (d)
            KeyAction::Create | KeyAction::Kill | KeyAction::Pty(_) | KeyAction::PtyPrefix => {
                self.set_status("terminal sessions not wired yet")
            }
            KeyAction::Nav(_) | KeyAction::Swallow => {}
        }
    }

    /// Focus the terminal pane, but only onto a live session, so a dead pane
    /// can never swallow Ctrl-C.  Sessions arrive in phase (d); until then the
    /// guard always refuses.
    fn focus_term(&mut self) {
        let msg = format!("no session — press {} c to create one", self.prefix_label());
        self.set_status(msg);
    }

    // -----------------------------------------------------------------------
    // Command senders (client → daemon)
    // -----------------------------------------------------------------------

    async fn send_run_prompt(&mut self, prompt: String) {
        let Some(wt) = self.worktrees.get(self.grid.selected) else {
            return;
        };
        let worktree_path = wt.path.clone();
        self.client
            .send(ClientMsg::RunPrompt {
                worktree_path,
                prompt,
            })
            .await;
    }

    async fn send_run_builtin(&mut self, kind: BuiltinKind) {
        let Some(wt) = self.worktrees.get(self.grid.selected) else {
            return;
        };
        let worktree_path = wt.path.clone();
        self.client
            .send(ClientMsg::RunBuiltin {
                worktree_path,
                kind,
            })
            .await;
    }

    /// Ask the daemon to resume the selected worktree's session (`R`).
    async fn send_resume_session(&mut self) {
        let Some(wt) = self.worktrees.get(self.grid.selected) else {
            return;
        };
        let worktree_path = wt.path.clone();
        self.client
            .send(ClientMsg::ResumeSession { worktree_path })
            .await;
        self.set_status("resuming session…");
    }

    /// Nudge the terminal-pane width by `delta` percentage points, within 10..=80.
    fn adjust_pane_pct(&mut self, delta: i16) {
        self.pane_pct = (self.pane_pct as i16 + delta).clamp(10, 80) as u16;
        self.set_status(format!("pane width: {}%", self.pane_pct));
    }

    async fn send_set_worktree_name(&mut self, name: String) {
        let Some(wt) = self.worktrees.get(self.grid.selected) else {
            return;
        };
        let worktree_path = wt.path.clone();
        self.client
            .send(ClientMsg::SetWorktreeName {
                worktree_path,
                name,
            })
            .await;
    }

    async fn send_toggle_auto_continue(&mut self) {
        let Some(wt) = self.worktrees.get(self.grid.selected) else {
            return;
        };
        let worktree_path = wt.path.clone();
        let enabled = !wt.auto_continue_on_merge;
        self.client
            .send(ClientMsg::SetAutoContinue {
                worktree_path,
                enabled,
            })
            .await;
        let label = if enabled { "on" } else { "off" };
        self.set_status(format!("auto-continue: {label}"));
    }

    // -----------------------------------------------------------------------
    // App event handler (daemon → client)
    // -----------------------------------------------------------------------

    fn handle_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Quit => {
                self.running = false;
            }
            AppEvent::Tick => {
                self.spinner_frame = self.spinner_frame.wrapping_add(1);
            }
            AppEvent::Snapshot {
                projects,
                worktrees,
            } => {
                self.projects = projects;
                // Rebuild the library's per-project prompt stores from the new
                // project list (reads <project>/.karazhan/prompts/ per project).
                self.library.set_projects(&self.projects);
                self.worktrees = worktrees;
                self.grid.selected = clamp_selection(self.grid.selected, self.worktrees.len());
            }
            AppEvent::WorktreeStatusChanged {
                worktree_path,
                status,
                summary,
                activity,
                turns,
                tokens,
                run_started_at,
            } => {
                if let Some(view) = self.worktrees.iter_mut().find(|w| w.path == worktree_path) {
                    view.status = status;
                    if summary.is_some() {
                        view.last_summary = summary;
                    }
                    view.activity = activity;
                    view.turns = turns;
                    view.tokens = tokens;
                    view.run_started_at = run_started_at;
                }
            }
            AppEvent::DaemonError {
                worktree_path,
                message,
            } => {
                match &worktree_path {
                    Some(p) => {
                        tracing::warn!(worktree = %p.display(), "daemon error: {message}")
                    }
                    None => tracing::warn!("daemon error: {message}"),
                }
                self.set_status(format!("error: {message}"));
            }
            AppEvent::DaemonDisconnected => {
                tracing::warn!("daemon disconnected");
                self.set_status("daemon disconnected — restart karazhan to reconnect");
            }
            // Watcher-only variant the client never receives.
            AppEvent::PrStatusChanged { .. } => {}
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn enter_grid_view(&mut self) {
        self.view = View::Grid;
        tracing::info!("entered grid view ({} worktrees)", self.worktrees.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    /// Route `k` with the prefix already pending.
    fn after_prefix(focus: Focus, k: KeyEvent) -> KeyAction {
        route_key(focus, &mut true, k, 'a')
    }

    fn grid_app() -> (App, mpsc::Receiver<ClientMsg>) {
        let (client, rx) = SupervisorClient::test_pair();
        let (_tx, event_rx) = mpsc::channel(1);
        let mut app = App::new(event_rx, Config::default(), client);
        app.view = View::Grid;
        (app, rx)
    }

    #[tokio::test]
    async fn ctrl_c_forwarded_when_term_focused() {
        let mut pending = false;
        assert_eq!(
            route_key(Focus::Term, &mut pending, ctrl('c'), 'a'),
            KeyAction::Pty(ctrl('c'))
        );

        let (mut app, _rx) = grid_app();
        app.focus = Focus::Term;
        app.handle_crossterm_event(Event::Key(ctrl('c'))).await;
        assert!(
            app.running,
            "Ctrl-C must not quit while the terminal is focused"
        );
    }

    #[test]
    fn prefix_prefix_literal_only_in_term() {
        assert_eq!(after_prefix(Focus::Term, ctrl('a')), KeyAction::PtyPrefix);
        assert_eq!(after_prefix(Focus::Nav, ctrl('a')), KeyAction::Swallow);
    }

    #[test]
    fn prefix_h_focuses_nav() {
        let h = key(KeyCode::Char('h'));
        assert_eq!(after_prefix(Focus::Term, h), KeyAction::FocusNav);
        assert_eq!(after_prefix(Focus::Nav, h), KeyAction::Swallow);
        let l = key(KeyCode::Char('l'));
        assert_eq!(after_prefix(Focus::Term, l), KeyAction::FocusTerm);
        assert_eq!(after_prefix(Focus::Nav, l), KeyAction::FocusTerm);
        let c = key(KeyCode::Char('c'));
        assert_eq!(after_prefix(Focus::Term, c), KeyAction::Create);
        assert_eq!(after_prefix(Focus::Nav, c), KeyAction::Create);
        let three = key(KeyCode::Char('3'));
        assert_eq!(after_prefix(Focus::Term, three), KeyAction::Select(3));
        assert_eq!(after_prefix(Focus::Nav, three), KeyAction::Select(3));
    }

    #[test]
    fn prefix_x_ignored_in_nav() {
        let x = key(KeyCode::Char('x'));
        assert_eq!(after_prefix(Focus::Nav, x), KeyAction::Swallow);
        assert_eq!(after_prefix(Focus::Term, x), KeyAction::Kill);
    }

    #[test]
    fn unknown_prefix_key_swallowed() {
        for focus in [Focus::Nav, Focus::Term] {
            for k in [key(KeyCode::Char('z')), key(KeyCode::Char('0')), ctrl('c')] {
                assert_eq!(
                    after_prefix(focus, k),
                    KeyAction::Swallow,
                    "{focus:?} {k:?}"
                );
            }
        }
    }

    #[test]
    fn pending_cleared_after_any_key() {
        for focus in [Focus::Nav, Focus::Term] {
            let mut pending = false;
            assert_eq!(
                route_key(focus, &mut pending, ctrl('a'), 'a'),
                KeyAction::Swallow
            );
            assert!(pending);
            for k in [
                key(KeyCode::Char('z')),
                key(KeyCode::Char('c')),
                ctrl('a'),
                key(KeyCode::Enter),
            ] {
                pending = true;
                route_key(focus, &mut pending, k, 'a');
                assert!(!pending, "{focus:?} {k:?}");
            }
        }
        // Without a pending prefix, plain keys pass straight through.
        let mut pending = false;
        let j = key(KeyCode::Char('j'));
        assert_eq!(
            route_key(Focus::Nav, &mut pending, j, 'a'),
            KeyAction::Nav(j)
        );
        assert_eq!(
            route_key(Focus::Term, &mut pending, j, 'a'),
            KeyAction::Pty(j)
        );
    }

    #[tokio::test]
    async fn enter_without_session_stays_nav_with_hint() {
        let (mut app, mut rx) = grid_app();
        app.handle_crossterm_event(Event::Key(key(KeyCode::Enter)))
            .await;
        assert_eq!(app.focus, Focus::Nav);
        assert_eq!(
            app.status_message.as_deref(),
            Some("no session — press ^a c to create one")
        );
        assert!(rx.try_recv().is_err(), "nothing is sent to the daemon");
    }

    #[tokio::test]
    async fn help_key_clears_prefix_pending() {
        let (mut app, _rx) = grid_app();
        app.handle_crossterm_event(Event::Key(ctrl('a'))).await;
        assert!(app.prefix_pending);
        app.handle_crossterm_event(Event::Key(key(KeyCode::Char('?'))))
            .await;
        assert!(app.show_help);
        assert!(!app.prefix_pending);
    }
}

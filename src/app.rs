use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::Stdout;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

use crate::client::SupervisorClient;
use crate::commands::{self, CommandId, Palette};
use crate::config::Config;
use crate::ipc::{BuiltinKind, ClientMsg, WorktreeView};
use crate::prompts::PromptStore;
use crate::ui::detail::{split_grid_detail, DetailView};
use crate::ui::grid::GridView;
use crate::ui::keymap::{clamp_selection, Motion};
use crate::ui::library::{LibraryMode, LibraryView};
use crate::worktree::WorktreeStatus;

#[derive(Debug)]
pub enum AppEvent {
    Tick,
    #[allow(dead_code)] // reserved for background-task shutdown requests
    Quit,
    /// Full state snapshot from the daemon (handshake / Refresh / create / remove).
    Snapshot {
        worktrees: Vec<WorktreeView>,
    },
    /// Incremental status update for a single worktree from the daemon.
    WorktreeStatusChanged {
        worktree_path: PathBuf,
        status: WorktreeStatus,
        summary: Option<String>,
    },
    /// Non-fatal error surfaced by the daemon (gh failures, etc.).
    DaemonError {
        worktree_path: Option<PathBuf>,
        message: String,
    },
    /// The daemon connection dropped (daemon died or socket closed).
    DaemonDisconnected,
    /// The background watcher detected that a PR transitioned to merged.
    ///
    /// Emitted only by the daemon-side watcher → handled inside the daemon; the
    /// client never receives this variant, but the watcher → daemon channel is
    /// typed as `AppEvent`, so it stays defined here.
    PrMerged {
        worktree_path: PathBuf,
        pr: u64,
    },
    /// The background watcher detected a CI status change (daemon-side only).
    CiStatusChanged {
        worktree_path: PathBuf,
        all_passing: bool,
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
}

pub struct App {
    pub running: bool,
    pub view: View,
    event_rx: mpsc::Receiver<AppEvent>,
    library: LibraryView,
    grid: GridView,
    grid_mode: GridMode,
    prompt_input: String,
    detail: DetailView,
    /// Local cache of worktree views pushed by the daemon.
    worktrees: Vec<WorktreeView>,
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
}

impl App {
    pub fn new(
        event_rx: mpsc::Receiver<AppEvent>,
        prompt_dir: PathBuf,
        config: Config,
        client: SupervisorClient,
    ) -> Self {
        let store = PromptStore::new(prompt_dir);
        let library = LibraryView::new(store);

        Self {
            running: true,
            view: View::Library,
            event_rx,
            library,
            grid: GridView::new(),
            grid_mode: GridMode::Normal,
            prompt_input: String::new(),
            detail: DetailView::new(),
            worktrees: Vec::new(),
            status_message: None,
            client,
            config,
            show_help: false,
            pending_edit: None,
            palette: None,
        }
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
                match self.view {
                    View::Library => self.library.render(frame),
                    View::Grid => {
                        let (grid_area, detail_area) = split_grid_detail(area);

                        self.grid.render(frame, grid_area, &self.worktrees);

                        let selected_wt = self.worktrees.get(self.grid.selected);
                        let summary = selected_wt.and_then(|wt| wt.last_summary.as_deref());
                        let prompt_input = if self.grid_mode == GridMode::PromptInput {
                            Some(self.prompt_input.as_str())
                        } else {
                            None
                        };

                        let status_text = self.build_status_text();

                        self.detail.render(
                            frame,
                            detail_area,
                            selected_wt,
                            summary,
                            prompt_input,
                            Some(&status_text),
                        );
                    }
                }

                // Render help overlay on top of whatever view is active.
                if self.show_help {
                    crate::ui::help::render_help(frame, area);
                }

                // Render the command palette on top of everything else.
                if let Some(palette) = &self.palette {
                    crate::ui::palette::render_palette(frame, area, palette);
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
        if let Some(ref msg) = self.status_message {
            return format!("[daemon]  {msg}");
        }
        "[daemon]  Tab: switch view  ?: help  q: quit".to_string()
    }

    // -----------------------------------------------------------------------
    // Crossterm event routing
    // -----------------------------------------------------------------------

    async fn handle_crossterm_event(&mut self, event: Event) {
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event
        {
            // Ctrl-C always quits regardless of mode/view.  Daemon survives.
            if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
                tracing::info!("quit requested via Ctrl-C");
                self.running = false;
                return;
            }

            // Command palette: while open it intercepts ALL keys.
            if self.palette.is_some() {
                self.handle_palette_key(code, modifiers).await;
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
                self.show_help = true;
                return;
            }

            // Global Ctrl-P opens the command palette — but only from Normal
            // modes, so it does not steal a character from an active text input.
            if code == KeyCode::Char('p') && modifiers.contains(KeyModifiers::CONTROL) {
                let in_input =
                    self.library.mode != LibraryMode::Normal || self.grid_mode != GridMode::Normal;
                if !in_input {
                    self.palette = Some(commands::Palette::open(self.view == View::Grid));
                    return;
                }
            }

            match self.view {
                View::Library => self.handle_library_key(code, modifiers).await,
                View::Grid => self.handle_grid_key(code).await,
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
            CommandId::ToggleAutoContinue => {
                self.view = View::Grid;
                self.send_toggle_auto_continue().await;
            }
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
                KeyCode::Char('q') => self.execute_command(CommandId::Quit).await,
                KeyCode::Char('j') | KeyCode::Down => self.library.move_down(),
                KeyCode::Char('k') | KeyCode::Up => self.library.move_up(),
                KeyCode::Char('/') => self.execute_command(CommandId::FilterPrompts).await,
                KeyCode::Char('n') | KeyCode::Char('a') => {
                    self.execute_command(CommandId::NewPrompt).await
                }
                KeyCode::Char('e') => self.execute_command(CommandId::EditPrompt).await,
                _ => {}
            },
            LibraryMode::Filter => match code {
                KeyCode::Esc => self.library.clear_filter(),
                KeyCode::Backspace => self.library.filter_pop(),
                KeyCode::Char(ch) => self.library.filter_push(ch),
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

    async fn handle_grid_key(&mut self, code: KeyCode) {
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

        if code == KeyCode::Tab {
            self.execute_command(CommandId::SwitchView).await;
            return;
        }

        let cols = {
            let w = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
            crate::ui::grid::GridView::cols_for_width(w)
        };
        let count = self.worktrees.len();

        match code {
            KeyCode::Char(ch @ '0'..='9') => {
                let d = ch as u8 - b'0';
                self.grid.push_digit(d);
            }

            KeyCode::Char('h') | KeyCode::Left => {
                self.grid.clear_pending_count();
                self.grid.apply(Motion::Left, count, cols);
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.grid.clear_pending_count();
                self.grid.apply(Motion::Right, count, cols);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.grid.clear_pending_count();
                self.grid.apply(Motion::Down, count, cols);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.grid.clear_pending_count();
                self.grid.apply(Motion::Up, count, cols);
            }
            KeyCode::Char('g') => {
                self.grid.clear_pending_count();
                self.grid.apply(Motion::First, count, cols);
            }
            KeyCode::Char('G') => {
                self.grid.apply(Motion::Last { count: None }, count, cols);
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
            AppEvent::Tick => {}
            AppEvent::Snapshot { worktrees } => {
                self.worktrees = worktrees;
                self.grid.selected = clamp_selection(self.grid.selected, self.worktrees.len());
            }
            AppEvent::WorktreeStatusChanged {
                worktree_path,
                status,
                summary,
            } => {
                if let Some(view) = self.worktrees.iter_mut().find(|w| w.path == worktree_path) {
                    view.status = status;
                    if summary.is_some() {
                        view.last_summary = summary;
                    }
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
            // Watcher-only variants the client never receives.
            AppEvent::PrMerged { .. } | AppEvent::CiStatusChanged { .. } => {}
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

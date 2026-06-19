use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::agent::session::{run_session, StatusUpdate};
use crate::agent::{
    agent_status_to_worktree_status, claude_code::ClaudeCodeBackend, mock::MockBackend,
    AgentBackend, AgentStatus,
};
use crate::prompts::PromptStore;
use crate::ui::detail::{split_grid_detail, DetailView};
use crate::ui::grid::GridView;
use crate::ui::keymap::Motion;
use crate::ui::library::{LibraryMode, LibraryView};
use crate::worktree::{state, Worktree, WorktreeManager, WorktreeStatus};

// TODO Phase 5: add github events (PRMerged, CIFailed)
// TODO Phase 6: add watcher events (PollTick)
#[derive(Debug)]
#[allow(dead_code)] // Quit is reserved for background tasks (Phase 6+)
pub enum AppEvent {
    Tick,
    Quit,
    /// A running agent session posted a coarse status update.  Sent from agent
    /// tasks through the same channel the UI loop already drains.
    AgentStatusChanged {
        worktree_path: PathBuf,
        status: AgentStatus,
        summary: Option<String>,
    },
}

/// Top-level view the application is currently showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Library,
    Grid,
}

/// Grid input sub-mode (mirrors LibraryMode's input pattern).
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
    /// Clone of the event sender handed to spawned agent tasks so their status
    /// updates flow into the same loop the UI drains.
    event_tx: mpsc::Sender<AppEvent>,
    library: LibraryView,
    // Grid view state.
    grid: GridView,
    grid_mode: GridMode,
    /// Free-text prompt buffer used in `GridMode::PromptInput`.
    prompt_input: String,
    detail: DetailView,
    worktree_manager: WorktreeManager,
    /// Cached list of worktrees; refreshed when entering Grid view or on Tick.
    worktrees: Vec<Worktree>,
    /// True when the last worktree list refresh failed (non-git-repo, etc).
    worktree_error: Option<String>,
    /// Pluggable agent backend chosen at startup (Claude Code if `claude` is on
    /// PATH, else the offline mock).
    backend: Arc<dyn AgentBackend>,
    /// Latest agent summary per worktree path, surfaced in the detail pane.
    agent_summaries: HashMap<PathBuf, String>,
}

impl App {
    pub fn new(
        event_rx: mpsc::Receiver<AppEvent>,
        event_tx: mpsc::Sender<AppEvent>,
        prompt_dir: PathBuf,
    ) -> Self {
        let store = PromptStore::new(prompt_dir);
        let library = LibraryView::new(store);

        // Use the current working directory as the repo root for the
        // WorktreeManager.  If cwd is not a git repo, list() will error — we
        // catch and surface that gracefully rather than panicking.
        let repo_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let worktree_manager = WorktreeManager::new(repo_root);

        let backend = select_backend();

        let mut app = Self {
            running: true,
            view: View::Library,
            event_rx,
            event_tx,
            library,
            grid: GridView::new(),
            grid_mode: GridMode::Normal,
            prompt_input: String::new(),
            detail: DetailView::new(),
            worktree_manager,
            worktrees: Vec::new(),
            worktree_error: None,
            backend,
            agent_summaries: HashMap::new(),
        };

        // Pre-load worktrees so Grid view is populated immediately on first switch.
        app.refresh_worktrees();
        app
    }

    /// Refresh the cached worktree list.  Errors are logged and stored as an
    /// empty state + error message — we never panic on a missing/non-git repo.
    fn refresh_worktrees(&mut self) {
        match self.worktree_manager.list() {
            Ok(wts) => {
                self.worktrees = wts;
                self.worktree_error = None;
                // Clamp selection in case the list shrank.
                self.grid.clamp(self.worktrees.len());
            }
            Err(e) => {
                tracing::warn!("worktree list failed (not a git repo?): {e}");
                self.worktrees = Vec::new();
                self.worktree_error = Some(format!("{e}"));
                self.grid.clamp(0);
            }
        }
    }

    pub async fn run<B: ratatui::backend::Backend>(
        mut self,
        terminal: &mut Terminal<B>,
    ) -> Result<()>
    where
        B::Error: Send + Sync + 'static,
    {
        let mut crossterm_events = EventStream::new();

        while self.running {
            terminal.draw(|frame| {
                let area = frame.area();
                match self.view {
                    View::Library => self.library.render(frame),
                    View::Grid => {
                        let (grid_area, detail_area) = split_grid_detail(area);

                        // Show error hint in the grid area when the repo isn't valid.
                        if let Some(ref err) = self.worktree_error {
                            use ratatui::{
                                style::{Color, Style},
                                text::Line,
                                widgets::Paragraph,
                            };
                            let msg = Paragraph::new(vec![
                                Line::from(""),
                                Line::from(format!("  error: {err}")),
                                Line::from(""),
                                Line::from("  karazhan must be run from inside a git repository."),
                            ])
                            .style(Style::default().fg(Color::Red));
                            frame.render_widget(msg, grid_area);
                        } else {
                            self.grid.render(frame, grid_area, &self.worktrees);
                        }

                        let selected_wt = self.worktrees.get(self.grid.selected);
                        let summary = selected_wt
                            .and_then(|wt| self.agent_summaries.get(&wt.path))
                            .map(|s| s.as_str());
                        let prompt_input = if self.grid_mode == GridMode::PromptInput {
                            Some(self.prompt_input.as_str())
                        } else {
                            None
                        };
                        self.detail
                            .render(frame, detail_area, selected_wt, summary, prompt_input);
                    }
                }
            })?;

            tokio::select! {
                maybe_event = crossterm_events.next() => {
                    match maybe_event {
                        Some(Ok(event)) => self.handle_crossterm_event(event),
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
        }

        Ok(())
    }

    fn handle_crossterm_event(&mut self, event: Event) {
        if let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event
        {
            // Ctrl-C always quits regardless of mode/view.
            if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
                tracing::info!("quit requested via Ctrl-C");
                self.running = false;
                return;
            }

            match self.view {
                View::Library => self.handle_library_key(code, modifiers),
                View::Grid => self.handle_grid_key(code),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Library key handling (unchanged from P1)
    // -----------------------------------------------------------------------

    fn handle_library_key(&mut self, code: KeyCode, _modifiers: KeyModifiers) {
        // Clear one-shot status on any keypress.
        self.library.status = None;

        // Tab switches to Grid view.
        if code == KeyCode::Tab {
            self.enter_grid_view();
            return;
        }

        match self.library.mode {
            LibraryMode::Normal => match code {
                KeyCode::Char('q') => {
                    tracing::info!("quit requested via 'q'");
                    self.running = false;
                }
                KeyCode::Char('j') | KeyCode::Down => self.library.move_down(),
                KeyCode::Char('k') | KeyCode::Up => self.library.move_up(),
                KeyCode::Char('/') => self.library.enter_filter(),
                KeyCode::Char('n') | KeyCode::Char('a') => self.library.enter_new_prompt(),
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
    // Grid key handling — vim motions + view switch
    // -----------------------------------------------------------------------

    fn handle_grid_key(&mut self, code: KeyCode) {
        // Prompt-input sub-mode intercepts all keys first (mirrors LibraryMode).
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
                        self.run_agent(prompt);
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

        // Tab switches back to Library view.
        if code == KeyCode::Tab {
            self.view = View::Library;
            return;
        }

        let cols = {
            // We don't have a terminal handle here, so we read the cached terminal
            // size via crossterm.  Falls back to a safe default if unavailable.
            let w = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
            crate::ui::grid::GridView::cols_for_width(w)
        };
        let count = self.worktrees.len();

        match code {
            // Digit prefix for count+G.
            KeyCode::Char(ch @ '0'..='9') => {
                let d = ch as u8 - b'0';
                self.grid.push_digit(d);
                // Do NOT clear pending_count here — the digit IS the count accumulation.
            }

            // Vim motions.
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
                // pending_count is consumed inside GridView::apply for Last.
                self.grid.apply(Motion::Last { count: None }, count, cols);
            }

            // Refresh worktree list.
            KeyCode::Char('r') => {
                self.grid.clear_pending_count();
                self.refresh_worktrees();
            }

            // Run a custom free-text prompt against the selected worktree.
            // TODO P5: built-in "address PR comments" / "check CI" commands.
            KeyCode::Char('c') => {
                self.grid.clear_pending_count();
                if self.worktrees.get(self.grid.selected).is_some() {
                    self.grid_mode = GridMode::PromptInput;
                    self.prompt_input.clear();
                }
            }

            // Quit from grid view too.
            KeyCode::Char('q') => {
                tracing::info!("quit requested via 'q' in grid view");
                self.running = false;
            }

            // Any other key clears pending count.
            _ => {
                self.grid.clear_pending_count();
            }
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn enter_grid_view(&mut self) {
        self.view = View::Grid;
        self.refresh_worktrees();
        tracing::info!("entered grid view ({} worktrees)", self.worktrees.len());
    }

    // -----------------------------------------------------------------------
    // App event handler
    // -----------------------------------------------------------------------

    fn handle_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Quit => {
                self.running = false;
            }
            AppEvent::Tick => {
                // TODO Phase 6: trigger watcher poll
            }
            AppEvent::AgentStatusChanged {
                worktree_path,
                status,
                summary,
            } => {
                self.apply_agent_status(&worktree_path, status, summary);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Agent integration
    // -----------------------------------------------------------------------

    /// Spawn an agent session on the selected worktree for `prompt`.
    ///
    /// Immediately marks the worktree `Running` (cached + persisted) and spawns
    /// a tokio task that drives the session, forwarding status updates back into
    /// the event loop via the cloned sender.  Never blocks the UI thread.
    fn run_agent(&mut self, prompt: String) {
        let Some(wt) = self.worktrees.get(self.grid.selected) else {
            return;
        };
        let worktree_path = wt.path.clone();

        // Optimistically reflect Running in the UI + persist it.
        self.set_worktree_status(&worktree_path, WorktreeStatus::Running);
        self.agent_summaries.remove(&worktree_path);

        let backend = Arc::clone(&self.backend);
        let app_tx = self.event_tx.clone();
        let path = worktree_path.clone();

        tracing::info!(worktree = %worktree_path.display(), "running agent");

        tokio::spawn(async move {
            // Bridge session StatusUpdates -> AppEvents.
            let (status_tx, mut status_rx) = mpsc::channel::<StatusUpdate>(16);
            let forward_tx = app_tx.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(update) = status_rx.recv().await {
                    let _ = forward_tx
                        .send(AppEvent::AgentStatusChanged {
                            worktree_path: update.worktree_path,
                            status: update.status,
                            summary: update.summary,
                        })
                        .await;
                }
            });

            match backend.start(&path, &prompt).await {
                Ok(handle) => {
                    if let Err(e) = run_session(handle, status_tx).await {
                        tracing::error!("agent session runner failed: {e}");
                        let _ = app_tx
                            .send(AppEvent::AgentStatusChanged {
                                worktree_path: path.clone(),
                                status: AgentStatus::Error(format!("{e}")),
                                summary: None,
                            })
                            .await;
                    }
                }
                Err(e) => {
                    tracing::error!("failed to start agent: {e}");
                    let _ = app_tx
                        .send(AppEvent::AgentStatusChanged {
                            worktree_path: path.clone(),
                            status: AgentStatus::Error(format!("{e}")),
                            summary: None,
                        })
                        .await;
                }
            }

            let _ = forwarder.await;
        });
    }

    /// Apply an incoming agent status update: map to WorktreeStatus, persist,
    /// and store the summary for the detail pane.
    fn apply_agent_status(
        &mut self,
        worktree_path: &std::path::Path,
        status: AgentStatus,
        summary: Option<String>,
    ) {
        let wt_status = agent_status_to_worktree_status(&status);
        self.set_worktree_status(worktree_path, wt_status);

        if let Some(s) = summary {
            self.agent_summaries.insert(worktree_path.to_path_buf(), s);
        }
        tracing::info!(worktree = %worktree_path.display(), "agent status: {status:?}");
    }

    /// Update a worktree's status in the cached list and persist via state.
    fn set_worktree_status(&mut self, worktree_path: &std::path::Path, status: WorktreeStatus) {
        if let Some(wt) = self.worktrees.iter_mut().find(|w| w.path == worktree_path) {
            wt.status = status.clone();
        }
        let repo_root = &self.worktree_manager.repo_root;
        match state::load(repo_root) {
            Ok(mut st) => {
                st.set_status(worktree_path, status);
                if let Err(e) = state::save(repo_root, &st) {
                    tracing::warn!("failed to persist worktree status: {e}");
                }
            }
            Err(e) => tracing::warn!("failed to load state for status update: {e}"),
        }
    }
}

/// Choose the active agent backend at startup.
///
/// Uses [`ClaudeCodeBackend`] when the `claude` binary is resolvable on PATH;
/// otherwise falls back to the offline [`MockBackend`].  Logs the choice.
fn select_backend() -> Arc<dyn AgentBackend> {
    if claude_on_path() {
        tracing::info!("agent backend: ClaudeCodeBackend (claude found on PATH)");
        Arc::new(ClaudeCodeBackend::new())
    } else {
        tracing::warn!("agent backend: MockBackend (claude not found on PATH)");
        Arc::new(MockBackend::new())
    }
}

/// Return true if a `claude` binary is resolvable on PATH.
fn claude_on_path() -> bool {
    use std::process::Command;
    Command::new("claude")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

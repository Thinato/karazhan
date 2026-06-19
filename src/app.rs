use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::client::SupervisorClient;
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
        }
    }

    /// Set the transient status message (replaces any previous message).
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
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
        }

        // Clean quit: just drop the client (closing the socket).  The daemon
        // keeps running — agent sessions and the watcher survive.
        Ok(())
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

            match self.view {
                View::Library => self.handle_library_key(code, modifiers),
                View::Grid => self.handle_grid_key(code).await,
            }
        }
    }

    // -----------------------------------------------------------------------
    // Library key handling
    // -----------------------------------------------------------------------

    fn handle_library_key(&mut self, code: KeyCode, _modifiers: KeyModifiers) {
        // Clear transient status on any keypress.
        self.status_message = None;
        self.library.status = None;

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
            self.view = View::Library;
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
                self.client.send(ClientMsg::Refresh).await;
                self.set_status("refreshing…");
            }

            KeyCode::Char('c') => {
                self.grid.clear_pending_count();
                if self.worktrees.get(self.grid.selected).is_some() {
                    self.grid_mode = GridMode::PromptInput;
                    self.prompt_input.clear();
                }
            }

            KeyCode::Char('p') => {
                self.grid.clear_pending_count();
                self.send_run_builtin(BuiltinKind::AddressPrComments).await;
            }

            KeyCode::Char('i') => {
                self.grid.clear_pending_count();
                self.send_run_builtin(BuiltinKind::CheckCi).await;
            }

            KeyCode::Char('a') => {
                self.grid.clear_pending_count();
                self.send_toggle_auto_continue().await;
            }

            // Shift-Q stops the daemon entirely (sessions + watcher), then quits.
            KeyCode::Char('Q') => {
                tracing::info!("daemon shutdown requested via 'Q'");
                self.client.send(ClientMsg::Shutdown).await;
                self.running = false;
            }

            KeyCode::Char('q') => {
                tracing::info!("quit requested via 'q' in grid view");
                self.running = false;
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

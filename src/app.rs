use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;

use crate::agent::session::{run_session, StatusUpdate};
use crate::agent::{
    agent_status_to_worktree_status, claude_code::ClaudeCodeBackend, mock::MockBackend,
    AgentBackend, AgentStatus,
};
use crate::github::commands::{build_address_pr_comments_prompt, build_check_ci_prompt};
use crate::github::pr::pr_for_current_branch;
use crate::github::RealGh;
use crate::prompts::PromptStore;
use crate::ui::detail::{split_grid_detail, DetailView};
use crate::ui::grid::GridView;
use crate::ui::keymap::Motion;
use crate::ui::library::{LibraryMode, LibraryView};
use crate::watcher::{spawn_watcher, WatchItem, WatcherConfig};
use crate::worktree::{state, Worktree, WorktreeManager, WorktreeStatus};

#[derive(Debug)]
pub enum AppEvent {
    Tick,
    #[allow(dead_code)] // reserved for background-task shutdown requests
    Quit,
    /// A running agent session posted a coarse status update.  Sent from agent
    /// tasks through the same channel the UI loop already drains.
    AgentStatusChanged {
        worktree_path: PathBuf,
        status: AgentStatus,
        summary: Option<String>,
    },
    /// A built-in command (address PR comments / check CI) has composed its
    /// prompt text and is ready to be sent to the agent.  Posted from the
    /// async gh task back into the main event loop.
    RunComposedPrompt {
        worktree_path: PathBuf,
        prompt: String,
    },
    /// A built-in command failed (e.g. gh unavailable, no PR, no comments).
    /// The UI surfaces this as a status/toast rather than crashing.
    GhError {
        worktree_path: PathBuf,
        message: String,
    },
    /// The background watcher detected that a PR transitioned to merged.
    PrMerged {
        worktree_path: PathBuf,
        pr: u64,
    },
    /// The background watcher detected a CI status change.
    /// `all_passing = true` means CI recovered; `false` means it started failing.
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

/// Grid input sub-mode (mirrors LibraryMode's input pattern).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridMode {
    /// Normal vim-motion navigation.
    Normal,
    /// Typing a free-text prompt to run against the selected worktree.
    PromptInput,
}

/// Which built-in `gh`-backed command to execute on the selected worktree.
#[derive(Debug, Clone, Copy)]
enum GhCommand {
    /// Compose a prompt from all open review comments on the worktree's PR.
    AddressPrComments,
    /// Compose a prompt from the failing CI checks/logs on the worktree's PR.
    CheckCi,
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
    /// Short error message from the last failed built-in gh command, shown as
    /// a status toast in the detail pane.  Cleared on the next command key.
    gh_error: Option<String>,
    /// Pluggable agent backend chosen at startup (Claude Code if `claude` is on
    /// PATH, else the offline mock).
    backend: Arc<dyn AgentBackend>,
    /// Latest agent summary per worktree path, surfaced in the detail pane.
    agent_summaries: HashMap<PathBuf, String>,
    /// Shared list of (path, pr_number) pairs that the watcher polls each tick.
    /// Updated by `refresh_worktrees()`.  `None` when gh is unavailable (no watcher).
    watch_set: Option<Arc<Mutex<Vec<WatchItem>>>>,
    /// Sender side of the watcher shutdown signal.  `send(true)` stops the watcher.
    watcher_shutdown_tx: Option<watch::Sender<bool>>,
    /// JoinHandle for the background watcher task; awaited on clean shutdown.
    watcher_handle: Option<JoinHandle<()>>,
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
            gh_error: None,
            backend,
            agent_summaries: HashMap::new(),
            watch_set: None,
            watcher_shutdown_tx: None,
            watcher_handle: None,
        };

        // Pre-load worktrees so Grid view is populated immediately on first switch.
        app.refresh_worktrees();
        app
    }

    /// Spawn the background watcher when `gh` is available.
    ///
    /// Called from `main` after the async runtime is up.  Safe to call multiple
    /// times (subsequent calls are no-ops if the watcher is already running).
    pub fn start_watcher(&mut self) {
        if self.watcher_handle.is_some() {
            return; // already running
        }

        let watch_set = Arc::new(Mutex::new(Vec::<WatchItem>::new()));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let runner = Arc::new(RealGh::new());
        let cwd = self.worktree_manager.repo_root.clone();
        let event_tx = self.event_tx.clone();
        let ws = Arc::clone(&watch_set);

        let handle = spawn_watcher(
            runner,
            cwd,
            event_tx,
            ws,
            WatcherConfig::default(),
            shutdown_rx,
        );

        self.watch_set = Some(watch_set);
        self.watcher_shutdown_tx = Some(shutdown_tx);
        self.watcher_handle = Some(handle);

        // Populate the watch-set with the already-loaded worktrees.
        self.update_watch_set();

        tracing::info!("watcher started");
    }

    /// Stop the background watcher and (best-effort) await its JoinHandle.
    ///
    /// Called when the app is about to exit.  This is a synchronous helper that
    /// fires the signal; the actual join happens in the async `run()` exit path.
    fn signal_watcher_shutdown(&mut self) {
        if let Some(tx) = self.watcher_shutdown_tx.take() {
            let _ = tx.send(true);
        }
    }

    /// Rebuild the watch-set from the current cached worktree list.
    ///
    /// Called after every `refresh_worktrees()`.  Only worktrees that have a
    /// known `pr_number` are included — nothing to poll without a PR.
    fn update_watch_set(&self) {
        let Some(ws) = &self.watch_set else { return };
        let items: Vec<WatchItem> = self
            .worktrees
            .iter()
            .filter_map(|wt| {
                wt.pr_number.map(|pr| WatchItem {
                    worktree_path: wt.path.clone(),
                    pr_number: pr,
                })
            })
            .collect();

        // Use try_lock; if the watcher is mid-tick it will get the updated
        // list on the next tick. This avoids blocking the UI thread.
        if let Ok(mut guard) = ws.try_lock() {
            *guard = items;
        }
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
        // Keep the watcher's poll list in sync.
        self.update_watch_set();
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

        // Clean shutdown: signal the watcher to stop and wait for it.
        // Agent tasks are detached and we do NOT await them — they will
        // complete in the background or be dropped when the runtime exits.
        self.signal_watcher_shutdown();
        if let Some(handle) = self.watcher_handle.take() {
            // Best-effort: give the watcher 500 ms to exit, then abort.
            match tokio::time::timeout(std::time::Duration::from_millis(500), handle).await {
                Ok(_) => tracing::info!("watcher stopped cleanly"),
                Err(_) => tracing::warn!("watcher did not stop in time; forcibly aborted"),
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
            KeyCode::Char('c') => {
                self.grid.clear_pending_count();
                self.gh_error = None;
                if self.worktrees.get(self.grid.selected).is_some() {
                    self.grid_mode = GridMode::PromptInput;
                    self.prompt_input.clear();
                }
            }

            // Built-in: "address all PR comments" for the selected worktree.
            KeyCode::Char('p') => {
                self.grid.clear_pending_count();
                self.gh_error = None;
                self.spawn_gh_command(GhCommand::AddressPrComments);
            }

            // Built-in: "check CI for failures" for the selected worktree.
            KeyCode::Char('i') => {
                self.grid.clear_pending_count();
                self.gh_error = None;
                self.spawn_gh_command(GhCommand::CheckCi);
            }

            // Toggle auto-continue-on-merge for the selected worktree.
            KeyCode::Char('a') => {
                self.grid.clear_pending_count();
                self.toggle_auto_continue();
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

    /// Toggle the `auto_continue_on_merge` flag on the currently selected worktree.
    ///
    /// Persists the new value to `.karazhan/state.toml` immediately.
    /// The detail pane already reads this flag from the `Worktree` model and will
    /// reflect the change on the next render.
    fn toggle_auto_continue(&mut self) {
        let Some(wt) = self.worktrees.get(self.grid.selected) else {
            return;
        };
        let path = wt.path.clone();
        let new_value = !wt.auto_continue_on_merge;

        // Update cached list.
        if let Some(wt) = self.worktrees.iter_mut().find(|w| w.path == path) {
            wt.auto_continue_on_merge = new_value;
        }

        // Persist.
        let repo_root = &self.worktree_manager.repo_root;
        match state::load(repo_root) {
            Ok(mut st) => {
                st.set_auto_continue(&path, new_value);
                if let Err(e) = state::save(repo_root, &st) {
                    tracing::warn!("failed to persist auto_continue toggle: {e}");
                }
            }
            Err(e) => tracing::warn!("failed to load state for auto_continue toggle: {e}"),
        }

        tracing::info!(
            worktree = %path.display(),
            auto_continue = new_value,
            "auto_continue_on_merge toggled"
        );
    }

    /// The default prompt sent to the agent when auto-continue fires after a PR merge.
    const AUTO_CONTINUE_PROMPT: &'static str =
        "The PR for this worktree was merged. Continue with the next step of the task.";

    /// Spawn a `continue_session` call on `worktree_path` using the default
    /// auto-continue prompt.  Mirrors the same fire-and-forget pattern as
    /// `run_agent` but calls `backend.continue_session` instead of `start`.
    fn run_agent_continue(&mut self, worktree_path: PathBuf) {
        // Mark running immediately.
        self.set_worktree_status(&worktree_path, WorktreeStatus::Running);
        self.agent_summaries.remove(&worktree_path);

        let backend = Arc::clone(&self.backend);
        let app_tx = self.event_tx.clone();
        let path = worktree_path.clone();
        let prompt = Self::AUTO_CONTINUE_PROMPT.to_string();

        tracing::info!(worktree = %worktree_path.display(), "auto-continue: starting session");

        tokio::spawn(async move {
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

            match backend.continue_session(&path, &prompt).await {
                Ok(handle) => {
                    if let Err(e) = run_session(handle, status_tx).await {
                        tracing::error!("auto-continue session runner failed: {e}");
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
                    tracing::error!("failed to start auto-continue session: {e}");
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

    // -----------------------------------------------------------------------
    // App event handler
    // -----------------------------------------------------------------------

    fn handle_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Quit => {
                self.running = false;
            }
            AppEvent::Tick => {}
            AppEvent::AgentStatusChanged {
                worktree_path,
                status,
                summary,
            } => {
                self.apply_agent_status(&worktree_path, status, summary);
            }
            AppEvent::RunComposedPrompt {
                worktree_path,
                prompt,
            } => {
                // Re-select the worktree so run_agent picks the right one.
                if let Some(idx) = self.worktrees.iter().position(|w| w.path == worktree_path) {
                    self.grid.selected = idx;
                }
                self.run_agent(prompt);
            }
            AppEvent::GhError {
                worktree_path,
                message,
            } => {
                tracing::warn!(worktree = %worktree_path.display(), "gh command error: {message}");
                self.gh_error = Some(message);
            }
            AppEvent::PrMerged { worktree_path, pr } => {
                tracing::info!(
                    worktree = %worktree_path.display(),
                    pr,
                    "PR merged — setting status PRMerged"
                );
                self.set_worktree_status(&worktree_path, WorktreeStatus::PRMerged);

                // Auto-continue: if the worktree has the flag set, immediately
                // enqueue a continue-session call.  Off by default.
                let auto_continue = self
                    .worktrees
                    .iter()
                    .find(|w| w.path == worktree_path)
                    .map(|w| w.auto_continue_on_merge)
                    .unwrap_or(false);

                if auto_continue {
                    tracing::info!(
                        worktree = %worktree_path.display(),
                        "auto_continue_on_merge=true — enqueuing continue session"
                    );
                    self.run_agent_continue(worktree_path);
                }
            }
            AppEvent::CiStatusChanged {
                worktree_path,
                all_passing,
            } => {
                if all_passing {
                    // CI recovered: if it was CIFailing, move back to Idle
                    // (NeedsReview might be more appropriate if an agent finished,
                    // but we don't have enough context here — Idle is safe and
                    // the user can always re-run).
                    let was_failing = self
                        .worktrees
                        .iter()
                        .find(|w| w.path == worktree_path)
                        .map(|w| w.status == WorktreeStatus::CIFailing)
                        .unwrap_or(false);

                    if was_failing {
                        tracing::info!(
                            worktree = %worktree_path.display(),
                            "CI recovered — setting status Idle"
                        );
                        self.set_worktree_status(&worktree_path, WorktreeStatus::Idle);
                    }
                } else {
                    tracing::info!(
                        worktree = %worktree_path.display(),
                        "CI failing — setting status CIFailing"
                    );
                    self.set_worktree_status(&worktree_path, WorktreeStatus::CIFailing);
                }
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

    /// Resolve a worktree's PR number: use the cached value if present, or ask
    /// `gh` for the current branch's PR and cache the result in state.
    ///
    /// Returns `None` (non-fatally) when no PR is open; logs gh errors.
    async fn resolve_pr_number(
        runner: &RealGh,
        wt: &Worktree,
        repo_root: &std::path::Path,
    ) -> Option<u64> {
        if let Some(n) = wt.pr_number {
            return Some(n);
        }
        match pr_for_current_branch(runner, &wt.path).await {
            Ok(Some(n)) => {
                // Persist the discovered PR number so future calls are fast.
                if let Ok(mut st) = state::load(repo_root) {
                    st.set_pr_number(&wt.path, Some(n));
                    let _ = state::save(repo_root, &st);
                }
                Some(n)
            }
            Ok(None) => None,
            Err(e) => {
                tracing::warn!("pr_for_current_branch failed: {e}");
                None
            }
        }
    }

    /// Spawn an async task that builds the `gh`-composed prompt for `cmd` on
    /// the currently selected worktree, then posts the result back through the
    /// event channel as either `RunComposedPrompt` or `GhError`.
    ///
    /// Never blocks the UI thread.
    fn spawn_gh_command(&mut self, cmd: GhCommand) {
        let Some(wt) = self.worktrees.get(self.grid.selected) else {
            return;
        };
        let wt = wt.clone();
        let repo_root = self.worktree_manager.repo_root.clone();
        let app_tx = self.event_tx.clone();

        tokio::spawn(async move {
            let runner = RealGh::new();
            let worktree_path = wt.path.clone();

            let pr_opt = Self::resolve_pr_number(&runner, &wt, &repo_root).await;

            let result: Result<String> = match cmd {
                GhCommand::AddressPrComments => match pr_opt {
                    None => Err(anyhow::anyhow!(
                        "no open PR found for worktree {}",
                        wt.path.display()
                    )),
                    Some(pr) => build_address_pr_comments_prompt(&runner, &wt.path, pr).await,
                },
                GhCommand::CheckCi => match pr_opt {
                    None => Err(anyhow::anyhow!(
                        "no open PR found for worktree {}",
                        wt.path.display()
                    )),
                    Some(pr) => build_check_ci_prompt(&runner, &wt.path, pr).await,
                },
            };

            let event = match result {
                Ok(prompt) => AppEvent::RunComposedPrompt {
                    worktree_path,
                    prompt,
                },
                Err(e) => AppEvent::GhError {
                    worktree_path,
                    message: format!("{e}"),
                },
            };

            let _ = app_tx.send(event).await;
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

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
use crate::config::Config;
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
    /// A running agent session posted a coarse status update.
    AgentStatusChanged {
        worktree_path: PathBuf,
        status: AgentStatus,
        summary: Option<String>,
    },
    /// A built-in command has composed its prompt text and is ready to be sent.
    RunComposedPrompt {
        worktree_path: PathBuf,
        prompt: String,
    },
    /// A built-in command failed (e.g. gh unavailable, no PR, no comments).
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

/// Which built-in `gh`-backed command to execute on the selected worktree.
#[derive(Debug, Clone, Copy)]
enum GhCommand {
    AddressPrComments,
    CheckCi,
}

pub struct App {
    pub running: bool,
    pub view: View,
    event_rx: mpsc::Receiver<AppEvent>,
    event_tx: mpsc::Sender<AppEvent>,
    library: LibraryView,
    grid: GridView,
    grid_mode: GridMode,
    prompt_input: String,
    detail: DetailView,
    worktree_manager: WorktreeManager,
    worktrees: Vec<Worktree>,
    worktree_error: Option<String>,
    /// Transient status message shown in the status line (replaces gh_error).
    /// Persists until replaced or explicitly cleared.
    status_message: Option<String>,
    backend: Arc<dyn AgentBackend>,
    /// Name of the active backend for display in the status line.
    backend_name: &'static str,
    agent_summaries: HashMap<PathBuf, String>,
    watch_set: Option<Arc<Mutex<Vec<WatchItem>>>>,
    watcher_shutdown_tx: Option<watch::Sender<bool>>,
    watcher_handle: Option<JoinHandle<()>>,
    /// Loaded application config (bin names, auto-continue prompt, colors, …).
    config: Config,
    /// Whether the help overlay is currently shown.
    show_help: bool,
}

impl App {
    pub fn new(
        event_rx: mpsc::Receiver<AppEvent>,
        event_tx: mpsc::Sender<AppEvent>,
        prompt_dir: PathBuf,
        config: Config,
    ) -> Self {
        let store = PromptStore::new(prompt_dir);
        let library = LibraryView::new(store);

        let repo_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let worktree_manager = WorktreeManager::new(repo_root);

        let (backend, backend_name) = select_backend(&config.claude_bin);

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
            status_message: None,
            backend,
            backend_name,
            agent_summaries: HashMap::new(),
            watch_set: None,
            watcher_shutdown_tx: None,
            watcher_handle: None,
            config,
            show_help: false,
        };

        app.refresh_worktrees();
        app
    }

    /// Set the transient status message (replaces any previous message).
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some(msg.into());
    }

    /// Spawn the background watcher when `gh` is available.
    pub fn start_watcher(&mut self, config: WatcherConfig) {
        if self.watcher_handle.is_some() {
            return;
        }

        let watch_set = Arc::new(Mutex::new(Vec::<WatchItem>::new()));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let runner = Arc::new(RealGh {
            bin: self.config.gh_bin.clone(),
        });
        let cwd = self.worktree_manager.repo_root.clone();
        let event_tx = self.event_tx.clone();
        let ws = Arc::clone(&watch_set);

        let handle = spawn_watcher(runner, cwd, event_tx, ws, config, shutdown_rx);

        self.watch_set = Some(watch_set);
        self.watcher_shutdown_tx = Some(shutdown_tx);
        self.watcher_handle = Some(handle);

        self.update_watch_set();

        tracing::info!("watcher started");
    }

    fn signal_watcher_shutdown(&mut self) {
        if let Some(tx) = self.watcher_shutdown_tx.take() {
            let _ = tx.send(true);
        }
    }

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

        if let Ok(mut guard) = ws.try_lock() {
            *guard = items;
        }
    }

    fn refresh_worktrees(&mut self) {
        match self.worktree_manager.list() {
            Ok(wts) => {
                self.worktrees = wts;
                self.worktree_error = None;
                self.grid.clamp(self.worktrees.len());
            }
            Err(e) => {
                tracing::warn!("worktree list failed (not a git repo?): {e}");
                self.worktrees = Vec::new();
                self.worktree_error = Some(format!("{e}"));
                self.grid.clamp(0);
            }
        }
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

                        // Build status line text.
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

        self.signal_watcher_shutdown();
        if let Some(handle) = self.watcher_handle.take() {
            match tokio::time::timeout(std::time::Duration::from_millis(500), handle).await {
                Ok(_) => tracing::info!("watcher stopped cleanly"),
                Err(_) => tracing::warn!("watcher did not stop in time; forcibly aborted"),
            }
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Status line
    // -----------------------------------------------------------------------

    fn build_status_text(&self) -> String {
        if let Some(ref msg) = self.status_message {
            return format!("[{}]  {}", self.backend_name, msg);
        }
        format!(
            "[{}]  Tab: switch view  ?: help  q: quit",
            self.backend_name
        )
    }

    // -----------------------------------------------------------------------
    // Crossterm event routing
    // -----------------------------------------------------------------------

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
                View::Grid => self.handle_grid_key(code),
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

    fn handle_grid_key(&mut self, code: KeyCode) {
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
                self.refresh_worktrees();
                self.set_status("worktrees refreshed");
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
                self.spawn_gh_command(GhCommand::AddressPrComments);
            }

            KeyCode::Char('i') => {
                self.grid.clear_pending_count();
                self.spawn_gh_command(GhCommand::CheckCi);
            }

            KeyCode::Char('a') => {
                self.grid.clear_pending_count();
                self.toggle_auto_continue();
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
    // Helpers
    // -----------------------------------------------------------------------

    fn enter_grid_view(&mut self) {
        self.view = View::Grid;
        self.refresh_worktrees();
        tracing::info!("entered grid view ({} worktrees)", self.worktrees.len());
    }

    fn toggle_auto_continue(&mut self) {
        let Some(wt) = self.worktrees.get(self.grid.selected) else {
            return;
        };
        let path = wt.path.clone();
        let new_value = !wt.auto_continue_on_merge;

        if let Some(wt) = self.worktrees.iter_mut().find(|w| w.path == path) {
            wt.auto_continue_on_merge = new_value;
        }

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

        let label = if new_value { "on" } else { "off" };
        self.set_status(format!("auto-continue: {label}"));

        tracing::info!(
            worktree = %path.display(),
            auto_continue = new_value,
            "auto_continue_on_merge toggled"
        );
    }

    /// Prompt text used when auto-continue fires after a PR merge.
    /// Falls back to the config value (itself defaulting to the compile-time constant).
    fn auto_continue_prompt(&self) -> &str {
        &self.config.auto_continue_prompt
    }

    fn run_agent_continue(&mut self, worktree_path: PathBuf) {
        self.set_worktree_status(&worktree_path, WorktreeStatus::Running);
        self.agent_summaries.remove(&worktree_path);

        let backend = Arc::clone(&self.backend);
        let app_tx = self.event_tx.clone();
        let path = worktree_path.clone();
        let prompt = self.auto_continue_prompt().to_string();

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
                self.set_status(format!("gh error: {message}"));
            }
            AppEvent::PrMerged { worktree_path, pr } => {
                tracing::info!(
                    worktree = %worktree_path.display(),
                    pr,
                    "PR merged — setting status PRMerged"
                );
                self.set_worktree_status(&worktree_path, WorktreeStatus::PRMerged);
                self.set_status(format!("PR #{pr} merged"));

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
                        self.set_status("CI recovered");
                    }
                } else {
                    tracing::info!(
                        worktree = %worktree_path.display(),
                        "CI failing — setting status CIFailing"
                    );
                    self.set_worktree_status(&worktree_path, WorktreeStatus::CIFailing);
                    self.set_status("CI failing");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Agent integration
    // -----------------------------------------------------------------------

    fn run_agent(&mut self, prompt: String) {
        let Some(wt) = self.worktrees.get(self.grid.selected) else {
            return;
        };
        let worktree_path = wt.path.clone();

        self.set_worktree_status(&worktree_path, WorktreeStatus::Running);
        self.agent_summaries.remove(&worktree_path);

        let backend = Arc::clone(&self.backend);
        let app_tx = self.event_tx.clone();
        let path = worktree_path.clone();

        tracing::info!(worktree = %worktree_path.display(), "running agent");

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

    fn spawn_gh_command(&mut self, cmd: GhCommand) {
        let Some(wt) = self.worktrees.get(self.grid.selected) else {
            return;
        };
        let wt = wt.clone();
        let repo_root = self.worktree_manager.repo_root.clone();
        let app_tx = self.event_tx.clone();
        let gh_bin = self.config.gh_bin.clone();

        tokio::spawn(async move {
            let runner = RealGh { bin: gh_bin };
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
/// Uses [`ClaudeCodeBackend`] when `claude_bin` is resolvable on PATH;
/// otherwise falls back to the offline [`MockBackend`].
fn select_backend(claude_bin: &str) -> (Arc<dyn AgentBackend>, &'static str) {
    if claude_on_path(claude_bin) {
        tracing::info!("agent backend: ClaudeCodeBackend ({claude_bin} found on PATH)");
        let backend = ClaudeCodeBackend {
            bin: claude_bin.to_string(),
        };
        (Arc::new(backend), "ClaudeCode")
    } else {
        tracing::warn!("agent backend: MockBackend ({claude_bin} not found on PATH)");
        (Arc::new(MockBackend::new()), "Mock")
    }
}

/// Return true if `bin` is resolvable on PATH.
fn claude_on_path(bin: &str) -> bool {
    use std::process::Command;
    Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

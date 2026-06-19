use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::prompts::PromptStore;
use crate::ui::detail::{split_grid_detail, DetailView};
use crate::ui::grid::GridView;
use crate::ui::keymap::Motion;
use crate::ui::library::{LibraryMode, LibraryView};
use crate::worktree::{Worktree, WorktreeManager};

// TODO Phase 4: add agent status events (AgentStarted, AgentDone, AgentError)
// TODO Phase 5: add github events (PRMerged, CIFailed)
// TODO Phase 6: add watcher events (PollTick)
#[derive(Debug)]
#[allow(dead_code)] // Quit and future variants are reserved for background tasks (Phase 4+)
pub enum AppEvent {
    Tick,
    Quit,
    // TODO Phase 4+: agent/watcher events go here
}

/// Top-level view the application is currently showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Library,
    Grid,
}

pub struct App {
    pub running: bool,
    pub view: View,
    event_rx: mpsc::Receiver<AppEvent>,
    library: LibraryView,
    // Grid view state.
    grid: GridView,
    detail: DetailView,
    worktree_manager: WorktreeManager,
    /// Cached list of worktrees; refreshed when entering Grid view or on Tick.
    worktrees: Vec<Worktree>,
    /// True when the last worktree list refresh failed (non-git-repo, etc).
    worktree_error: Option<String>,
}

impl App {
    pub fn new(event_rx: mpsc::Receiver<AppEvent>, prompt_dir: PathBuf) -> Self {
        let store = PromptStore::new(prompt_dir);
        let library = LibraryView::new(store);

        // Use the current working directory as the repo root for the
        // WorktreeManager.  If cwd is not a git repo, list() will error — we
        // catch and surface that gracefully rather than panicking.
        let repo_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let worktree_manager = WorktreeManager::new(repo_root);

        let mut app = Self {
            running: true,
            view: View::Library,
            event_rx,
            library,
            grid: GridView::new(),
            detail: DetailView::new(),
            worktree_manager,
            worktrees: Vec::new(),
            worktree_error: None,
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
                        self.detail.render(frame, detail_area, selected_wt);
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
        }
    }
}

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use ratatui::Terminal;
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::prompts::PromptStore;
use crate::ui::library::{LibraryMode, LibraryView};

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
    // TODO Phase 3: Grid, Detail
}

pub struct App {
    pub running: bool,
    pub view: View,
    event_rx: mpsc::Receiver<AppEvent>,
    library: LibraryView,
}

impl App {
    pub fn new(event_rx: mpsc::Receiver<AppEvent>, prompt_dir: PathBuf) -> Self {
        let store = PromptStore::new(prompt_dir);
        let library = LibraryView::new(store);
        Self {
            running: true,
            view: View::Library,
            event_rx,
            library,
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
            terminal.draw(|frame| match self.view {
                View::Library => self.library.render(frame),
            })?;

            tokio::select! {
                // crossterm keyboard/mouse events
                maybe_event = crossterm_events.next() => {
                    match maybe_event {
                        Some(Ok(event)) => self.handle_crossterm_event(event),
                        Some(Err(e)) => {
                            tracing::error!("crossterm event error: {e}");
                        }
                        None => {
                            // stream exhausted
                            self.running = false;
                        }
                    }
                }
                // internal app events from background tasks
                maybe_app_event = self.event_rx.recv() => {
                    match maybe_app_event {
                        Some(event) => self.handle_app_event(event),
                        None => {
                            // channel closed, all senders dropped
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
            // Ctrl-C always quits regardless of mode.
            if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
                tracing::info!("quit requested via Ctrl-C");
                self.running = false;
                return;
            }

            match self.view {
                View::Library => self.handle_library_key(code, modifiers),
            }
        }
    }

    fn handle_library_key(&mut self, code: KeyCode, _modifiers: KeyModifiers) {
        // Clear one-shot status on any keypress.
        self.library.status = None;

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

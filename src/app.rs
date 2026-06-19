use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, BorderType, Borders, Paragraph},
    Terminal,
};
use tokio::sync::mpsc;

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

pub struct App {
    pub running: bool,
    event_rx: mpsc::Receiver<AppEvent>,
}

impl App {
    pub fn new(event_rx: mpsc::Receiver<AppEvent>) -> Self {
        Self {
            running: true,
            event_rx,
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
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(0)])
                    .split(area);

                let block = Block::default()
                    .title(" karazhan ")
                    .title_alignment(Alignment::Center)
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Cyan));

                let text = Paragraph::new("press q to quit")
                    .block(block)
                    .alignment(Alignment::Center);

                frame.render_widget(text, chunks[0]);
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
            match (code, modifiers) {
                (KeyCode::Char('q'), _) => {
                    tracing::info!("quit requested via 'q'");
                    self.running = false;
                }
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    tracing::info!("quit requested via Ctrl-C");
                    self.running = false;
                }
                _ => {}
            }
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

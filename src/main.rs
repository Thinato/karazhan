mod app;
mod config;
mod watcher;

// TODO Phase 1
mod prompts;
// TODO Phase 2
mod worktree;
// TODO Phase 4
mod agent;
// TODO Phase 5
mod github;
// TODO Phase 3
mod ui;

use anyhow::Result;
use clap::Parser;
use crossterm::{
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{io, panic, path::PathBuf, time::Duration};
use tokio::sync::mpsc;
use tracing_appender::rolling;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use app::{App, AppEvent};
use config::Config;
use watcher::WatcherConfig;

/// Karazhan — TUI prompt manager and agent orchestrator
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the project directory (defaults to current dir)
    #[arg(short, long)]
    project: Option<PathBuf>,
}

/// RAII guard that owns terminal raw mode + alternate screen.
/// Restores the terminal on Drop.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort restore — ignore errors during teardown.
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn install_panic_hook() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        // Restore terminal before printing the panic message so the user's
        // shell is not left in raw / alternate-screen mode.
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));
}

fn init_tracing() -> Result<tracing_appender::non_blocking::WorkerGuard> {
    // Write logs to .karazhan/karazhan.log (rolling daily) so tracing output
    // never corrupts the TUI on stdout/stderr.
    std::fs::create_dir_all(".karazhan")?;
    let file_appender = rolling::daily(".karazhan", "karazhan.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .init();

    Ok(guard)
}

#[tokio::main]
async fn main() -> Result<()> {
    let _args = Args::parse();

    // Initialise tracing before anything else (guard must live for the full
    // duration of main so the background writer thread keeps flushing).
    let _tracing_guard = init_tracing()?;

    tracing::info!("karazhan starting up");

    // Load config (missing or malformed file → defaults, never errors).
    let cfg = Config::load();
    tracing::info!(
        poll_interval_secs = cfg.poll_interval_secs,
        claude_bin = %cfg.claude_bin,
        gh_bin = %cfg.gh_bin,
        "config loaded"
    );

    // Install panic hook so terminal is restored on unexpected panics.
    install_panic_hook();

    // Enter terminal raw mode + alternate screen.  The guard restores both on
    // drop, ensuring cleanup even if an error propagates out of run().
    let _terminal_guard = TerminalGuard::enter()?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    // Create the internal event channel.  Background tasks (watcher, agent
    // sessions) will send AppEvents here.
    let (event_tx, event_rx) = mpsc::channel::<AppEvent>(64);

    // Clone the sender for the App so spawned agent tasks can post status
    // updates into the same event loop the UI drains.
    let app_event_tx = event_tx.clone();

    // Spawn a tick task.
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
        loop {
            interval.tick().await;
            if event_tx.send(AppEvent::Tick).await.is_err() {
                // Receiver (App) has been dropped — stop ticking.
                break;
            }
        }
    });

    // Resolve the prompt directory from config or fall back to <cwd>/prompts.
    let prompt_dir = cfg.prompt_dir.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("prompts")
    });

    let watcher_config = WatcherConfig {
        interval: Duration::from_secs(cfg.poll_interval_secs),
    };

    let mut app = App::new(event_rx, app_event_tx, prompt_dir, cfg);

    // Spawn the background watcher only when `gh` is available.
    if github::gh_available().await {
        app.start_watcher(watcher_config);
    } else {
        tracing::warn!("gh not available — background watcher disabled");
    }

    let result = app.run(&mut terminal).await;

    tracing::info!("karazhan shutting down");

    // _terminal_guard drops here, restoring the terminal.
    result
}

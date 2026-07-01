mod app;
pub mod client;
mod commands;
mod config;
pub mod daemon;
pub mod ipc;
mod pr_status_store;
mod project_config;
mod projects;
mod watcher;
mod watcher_proc;

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
use std::{io, panic, path::PathBuf};
use tokio::sync::mpsc;
use tracing_appender::rolling;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use app::{App, AppEvent};
use config::Config;

/// Karazhan — TUI prompt manager and agent orchestrator
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the project directory (defaults to current dir)
    #[arg(short, long)]
    project: Option<PathBuf>,

    /// Cleanly stop the running karazhan supervisor daemon and exit (does not
    /// start the TUI).
    #[arg(long)]
    stop_daemon: bool,
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
    // Write logs to .karazhan/logs/karazhan.log (rolling daily) so tracing
    // output never corrupts the TUI on stdout/stderr.  Kept in a `logs/`
    // subdir so users can gitignore just `.karazhan/logs/`.
    std::fs::create_dir_all(".karazhan/logs")?;
    let file_appender = rolling::daily(".karazhan/logs", "karazhan.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .init();

    Ok(guard)
}

fn main() -> Result<()> {
    // Supervisor branch: must be handled BEFORE constructing the client tokio
    // runtime, because `run_supervisor()` builds its OWN multi-thread runtime
    // (nesting runtimes panics).  When `--supervisor` is present we hand off to
    // the daemon entry point and never run any TUI/terminal setup.
    if std::env::args().any(|a| a == "--supervisor") {
        return daemon::run_supervisor();
    }

    // Watcher branch: the standalone GitHub-polling watcher process.  Like
    // `--supervisor`, it builds its OWN runtime, so it must be handled before the
    // client runtime is constructed (nesting runtimes panics).
    if std::env::args().any(|a| a == "--watcher") {
        return watcher_proc::run_watcher();
    }

    // Stop-daemon branch: cleanly stop any running supervisor and exit without
    // starting the TUI.  Handled before the main runtime/TUI setup.
    if std::env::args().any(|a| a == "--stop-daemon") {
        return stop_daemon();
    }

    // Otherwise build the client runtime and run the existing async TUI logic.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_client())
}

/// Cleanly stop the running supervisor daemon (the `--stop-daemon` flag).
///
/// Reports the PID it stopped, or that no daemon was running, then exits.
fn stop_daemon() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let sock_path = crate::ipc::resolve_socket_path();
        let pidfile = crate::ipc::pidfile_path(&sock_path);
        let pid = crate::ipc::read_pidfile(&pidfile);
        crate::client::stop_running_daemon().await?;
        match pid {
            Some(p) => println!("stopped karazhan daemon (pid {p})"),
            None => println!("no running daemon"),
        }
        Ok(())
    })
}

async fn run_client() -> Result<()> {
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

    // Create the internal event channel.  The daemon-client reader task and the
    // tick task post AppEvents here; the App's event loop drains them.
    let (event_tx, event_rx) = mpsc::channel::<AppEvent>(64);

    // Spawn a tick task.  250ms cadence drives the spinner animation and the
    // live elapsed timer on Running worktrees; AppEvent::Tick is otherwise a
    // no-op redraw, so a faster tick is safe.
    let tick_tx = event_tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(250));
        loop {
            interval.tick().await;
            if tick_tx.send(AppEvent::Tick).await.is_err() {
                // Receiver (App) has been dropped — stop ticking.
                break;
            }
        }
    });

    // Connect to the supervisor daemon (auto-spawning it on first launch).  The
    // daemon owns the agent backend, the watcher, and all state.toml writes.
    let client = client::connect(event_tx.clone()).await?;

    // The library now reads prompts per-project from each project's
    // <project>/.karazhan/prompts/ dir (populated on the first Snapshot), so no
    // global prompt_dir is resolved here.
    let app = App::new(event_rx, cfg, client);

    let result = app.run(&mut terminal).await;

    tracing::info!("karazhan shutting down");

    // _terminal_guard drops here, restoring the terminal.
    result
}

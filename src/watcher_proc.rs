//! Standalone watcher process (`karazhan --watcher`).
//!
//! The watcher polls GitHub for every managed worktree's PR/CI status and writes
//! the result to each project's `.karazhan/pr_status.toml` (see
//! [`crate::pr_status_store`]).  It runs as its OWN OS process, entirely separate
//! from the session daemon, so:
//!   - GitHub-polling churn (new PR/CI features) ships here, not in the daemon;
//!   - restarting or crashing the watcher never touches live agent sessions.
//!
//! It reuses [`crate::watcher::spawn_watcher`] — the same poll loop the daemon
//! used to host — but discovers worktrees itself from the persisted project
//! registry instead of an in-memory [`crate::daemon`] registry.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::github::{GhRunner, RealGh};
use crate::watcher::{spawn_watcher, WatchItem, WatcherConfig};
use crate::{ipc, projects};

/// Build the watch-set from the persisted project registry: EVERY worktree of
/// EVERY registered project, tagged with its owning project's root and GitHub
/// `(owner, repo)` coordinates (parsed once per project).
///
/// A project whose `git worktree list` fails is logged and skipped — one bad
/// repo never stops the others.
pub fn build_watch_set(projects: &[projects::Project]) -> Vec<WatchItem> {
    let mut items = Vec::new();
    for project in projects {
        let root = project.path.clone();
        let manager = crate::worktree::manager::WorktreeManager::new(root.clone());
        let list = match manager.list() {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(project = %project.name, "watcher: worktree list failed: {e}");
                continue;
            }
        };
        // Parse the GitHub remote once per project.
        let (owner, repo) = match projects::git_owner_repo(&root) {
            Some((o, r)) => (Some(o), Some(r)),
            None => (None, None),
        };
        for wt in list {
            items.push(WatchItem {
                worktree_path: wt.path,
                project_root: root.clone(),
                owner: owner.clone(),
                repo: repo.clone(),
            });
        }
    }
    items
}

/// Return true if a *different*, still-alive watcher already owns `pidfile`.
///
/// Reads the pid and probes it with signal 0.  A missing/garbage pidfile, or one
/// naming a dead pid or our own pid, is NOT a live conflict.
fn another_watcher_alive(pidfile: &Path) -> bool {
    match ipc::read_pidfile(pidfile) {
        Some(pid) => pid_is_alive(pid) && pid != std::process::id() as i32,
        None => false,
    }
}

/// Probe `pid` with signal 0 (existence/permission check, no signal delivered).
fn pid_is_alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// Entry point for `karazhan --watcher`.  Builds its own multi-thread runtime
/// (like the supervisor) and never returns until the process is signalled.
pub fn run_watcher() -> Result<()> {
    let sock_dir = ipc::ensure_sock_dir().context("failed to create socket directory")?;
    let sock_path = sock_dir.join("sock");
    let pidfile = ipc::watcher_pidfile_path(&sock_path);
    let logfile = ipc::watcher_logfile_path(&sock_path);

    let _tracing_guard = init_watcher_tracing(&logfile)?;

    // Single-instance guard: exit cleanly if a healthy watcher already runs.
    if another_watcher_alive(&pidfile) {
        tracing::warn!("watcher: another watcher is already running — exiting");
        return Ok(());
    }
    if let Err(e) = std::fs::write(&pidfile, std::process::id().to_string()) {
        tracing::warn!(
            "watcher: failed to write pidfile {}: {e}",
            pidfile.display()
        );
    }

    tracing::info!(pid = std::process::id(), "watcher process starting");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build watcher runtime")?;

    let result = runtime.block_on(serve_watcher());

    // Best-effort pidfile cleanup on exit.
    let _ = std::fs::remove_file(&pidfile);
    result
}

/// The async body: build the watch-set, spawn the poll loop, refresh the
/// watch-set every interval, and exit cleanly on SIGTERM/SIGINT.
async fn serve_watcher() -> Result<()> {
    let config = Config::load();
    let interval = Duration::from_secs(config.poll_interval_secs);

    let gh: Arc<dyn GhRunner> = Arc::new(RealGh {
        bin: config.gh_bin.clone(),
    });

    // The poll loop still emits AppEvents (for the in-daemon caller); here there
    // is no client, so drain-and-discard them.  Persistence to pr_status.toml
    // happens BEFORE the emit, so a dropped receiver never loses data.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move { while event_rx.recv().await.is_some() {} });

    let watch_set = Arc::new(Mutex::new(build_watch_set(&projects::load().projects)));

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handle = spawn_watcher(
        Arc::clone(&gh),
        event_tx,
        Arc::clone(&watch_set),
        WatcherConfig { interval },
        shutdown_rx,
    );

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("failed to install SIGTERM handler")?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("failed to install SIGINT handler")?;

    // Refresh the watch-set on the same cadence so newly-created / removed
    // worktrees are picked up without a restart.
    let mut refresh = tokio::time::interval(interval);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = refresh.tick() => {
                let next = build_watch_set(&projects::load().projects);
                *watch_set.lock().await = next;
            }
            _ = sigterm.recv() => {
                tracing::info!("watcher: SIGTERM — stopping");
                break;
            }
            _ = sigint.recv() => {
                tracing::info!("watcher: SIGINT — stopping");
                break;
            }
        }
    }

    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    Ok(())
}

/// Initialise tracing to the watcher's (non-rolling) logfile.
fn init_watcher_tracing(logfile: &Path) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let parent = logfile.parent().unwrap_or_else(|| Path::new("."));
    let file_name = logfile
        .file_name()
        .map(|f| f.to_owned())
        .unwrap_or_else(|| std::ffi::OsString::from("watcher.log"));
    let appender = tracing_appender::rolling::never(parent, file_name);
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .try_init();

    Ok(guard)
}

// ---------------------------------------------------------------------------
// Client-side lifecycle: spawn / ensure / stop the watcher process
// ---------------------------------------------------------------------------

/// Ensure a watcher process is running, spawning one if not.
///
/// Called by the TUI client on startup (after it attaches to the daemon).  The
/// watcher's lifetime is deliberately INDEPENDENT of the session daemon: it is
/// spawned detached (double-fork, no `kill_on_drop`) so restarting or crashing
/// the daemon never touches it, and vice-versa.  Double-spawns are harmless — the
/// watcher's own single-instance guard makes any extra instance exit.
pub fn ensure_watcher_running() -> Result<()> {
    let sock_dir = ipc::ensure_sock_dir().context("failed to create socket directory")?;
    let pidfile = ipc::watcher_pidfile_path(&sock_dir.join("sock"));
    if another_watcher_alive(&pidfile) {
        tracing::debug!("client: watcher already running");
        return Ok(());
    }
    tracing::info!("client: no watcher running — spawning one");
    spawn_watcher_process()
}

/// Spawn a detached `karazhan --watcher` process (double-fork + setsid so it is
/// reparented to init and outlives this process).  Mirrors
/// [`crate::daemon::spawn_supervisor`] but for the watcher, and — crucially — the
/// child is NOT `kill_on_drop`, so the watcher survives the client and the
/// daemon independently.
pub fn spawn_watcher_process() -> Result<()> {
    use nix::sys::wait::waitpid;
    use nix::unistd::{fork, ForkResult};

    let sock_dir = ipc::ensure_sock_dir().context("failed to create socket directory")?;
    let logfile = ipc::watcher_logfile_path(&sock_dir.join("sock"));

    // SAFETY: between fork() and exec()/_exit() only async-signal-safe operations
    // run (fork, setsid, open, dup2, exec) plus pre-resolved owned paths.
    match unsafe { fork() }.context("first fork failed")? {
        ForkResult::Parent { child } => {
            let _ = waitpid(child, None);
            Ok(())
        }
        ForkResult::Child => {
            if nix::unistd::setsid().is_err() {
                unsafe { libc_exit(1) };
            }
            match unsafe { fork() } {
                Ok(ForkResult::Parent { .. }) => unsafe { libc_exit(0) },
                Ok(ForkResult::Child) => {
                    redirect_std_to_log(&logfile);
                    let _ = exec_self_watcher();
                    unsafe { libc_exit(1) };
                }
                Err(_) => unsafe { libc_exit(1) },
            }
        }
    }
}

/// `exec` the current executable with `--watcher` (returns only on error).
fn exec_self_watcher() -> Result<()> {
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe().context("cannot resolve current_exe")?;
    let err = std::process::Command::new(exe).arg("--watcher").exec();
    Err(anyhow::anyhow!("exec --watcher failed: {err}"))
}

/// Redirect stdin←/dev/null and stdout/stderr→logfile in the current process.
fn redirect_std_to_log(logfile: &Path) {
    use std::os::unix::io::AsRawFd;

    if let Ok(devnull) = std::fs::OpenOptions::new().read(true).open("/dev/null") {
        let _ = nix::unistd::dup2(devnull.as_raw_fd(), 0);
    }
    if let Ok(log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logfile)
    {
        let fd = log.as_raw_fd();
        let _ = nix::unistd::dup2(fd, 1);
        let _ = nix::unistd::dup2(fd, 2);
        std::mem::forget(log);
    }
}

/// Async-signal-safe `_exit` for forked child branches.
unsafe fn libc_exit(code: i32) -> ! {
    nix::libc::_exit(code)
}

/// Cleanly stop a running watcher process, if any (`--stop-watcher`).
///
/// SIGTERMs the pid named in `watcher.pid`, polls ~2s for exit, escalates to
/// SIGKILL, then removes the pidfile.  Missing/dead pid is a no-op.
pub async fn stop_running_watcher() -> Result<()> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let sock_dir = ipc::ensure_sock_dir().context("failed to create socket directory")?;
    let pidfile = ipc::watcher_pidfile_path(&sock_dir.join("sock"));

    let pid_raw = match ipc::read_pidfile(&pidfile) {
        Some(p) => p,
        None => {
            tracing::info!("no watcher pidfile — nothing to stop");
            return Ok(());
        }
    };
    let pid = Pid::from_raw(pid_raw);

    if kill(pid, None).is_err() {
        let _ = std::fs::remove_file(&pidfile);
        return Ok(());
    }

    let _ = kill(pid, Signal::SIGTERM);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut exited = false;
    while std::time::Instant::now() < deadline {
        if kill(pid, None).is_err() {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !exited && kill(pid, None).is_ok() {
        let _ = kill(pid, Signal::SIGKILL);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = std::fs::remove_file(&pidfile);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_watch_set_empty_projects_is_empty() {
        assert!(build_watch_set(&[]).is_empty());
    }

    #[test]
    fn build_watch_set_skips_project_with_bad_path() {
        // A project pointing at a non-repo path: `git worktree list` fails and the
        // project is skipped, not fatal.
        let projects = vec![projects::Project {
            name: "ghost".to_string(),
            path: std::path::PathBuf::from("/nonexistent/definitely/not/a/repo"),
        }];
        assert!(build_watch_set(&projects).is_empty());
    }

    #[test]
    fn another_watcher_alive_false_for_missing_pidfile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("watcher.pid");
        assert!(!another_watcher_alive(&pidfile));
    }

    #[test]
    fn another_watcher_alive_false_for_our_own_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("watcher.pid");
        std::fs::write(&pidfile, std::process::id().to_string()).expect("write");
        // Our own pid is alive but must NOT count as a conflicting instance.
        assert!(!another_watcher_alive(&pidfile));
    }

    #[test]
    fn another_watcher_alive_false_for_dead_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("watcher.pid");
        // PID 2^31-1 is effectively never a live process.
        std::fs::write(&pidfile, "2147483647").expect("write");
        assert!(!another_watcher_alive(&pidfile));
    }
}

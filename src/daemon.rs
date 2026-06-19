//! Supervisor daemon (Stage 8b).
//!
//! The daemon OWNS the agent processes, the watcher, the worktree/session
//! registry, the `gh` integration, and all `.karazhan/state.toml` writes.  It
//! serves connected thin clients over a Unix domain socket and survives the TUI
//! closing.  Because the watcher and agent tasks live here, auto-continue-on-merge
//! fires with no client connected.
//!
//! Module layout:
//! - [`Registry`]  — authoritative in-memory worktree state + summaries.
//! - [`Shared`]    — everything a connection / internal emitter needs, behind `Arc`.
//! - [`run_supervisor`] — builds a multi-thread runtime, blocks on [`serve`].
//! - [`serve`]     — binds the socket, spawns the watcher, accepts connections.
//! - [`handle_conn`] — per-connection handshake + bidirectional select loop.
//! - command handlers — apply each [`ipc::ClientMsg`] to [`Shared`] and broadcast.
//! - [`spawn_supervisor`] — double-fork autostart helper (used by 8c).
//! - [`wait_for_socket`]  — poll-connect helper (used by 8c).
//!
//! Watcher event rehoming: the watcher still emits its pure [`watcher::diff_state`]
//! events, but instead of reaching the TUI's `App`, they reach a daemon-side
//! handler ([`handle_watch_event`]) via the App event channel the watcher already
//! speaks.  We drain that channel in [`serve`]'s select loop and apply the same
//! PR-merged / CI-status logic that previously lived in `app.rs`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::agent::session::{run_session, StatusUpdate};
use crate::agent::{
    agent_status_to_worktree_status, claude_code::ClaudeCodeBackend, mock::MockBackend,
    AgentBackend, AgentStatus,
};
use crate::app::AppEvent;
use crate::config::Config;
use crate::github::commands::{build_address_pr_comments_prompt, build_check_ci_prompt};
use crate::github::pr::pr_for_current_branch;
use crate::github::{GhRunner, RealGh};
use crate::ipc::{self, BuiltinKind, ClientMsg, HandshakeReq, HandshakeResp, SupervisorMsg};
use crate::watcher::{spawn_watcher, WatchItem, WatcherConfig};
use crate::worktree::{state, WorktreeManager, WorktreeStatus};

/// Capacity of the broadcast channel that fans `SupervisorMsg`s out to clients.
const BROADCAST_CAP: usize = 256;

// ---------------------------------------------------------------------------
// Registry — authoritative in-memory state
// ---------------------------------------------------------------------------

/// Authoritative worktree state held by the daemon.
///
/// `worktrees` is the live set keyed by canonical path; `summaries` holds the
/// most recent agent summary line per worktree (mirrors `App::agent_summaries`).
pub struct Registry {
    pub worktrees: HashMap<PathBuf, ipc::WorktreeView>,
    pub summaries: HashMap<PathBuf, String>,
}

impl Registry {
    /// Build a `Registry` from the worktree manager's overlaid list.
    fn from_manager(manager: &WorktreeManager) -> Self {
        let mut worktrees = HashMap::new();
        match manager.list() {
            Ok(list) => {
                for wt in &list {
                    worktrees.insert(wt.path.clone(), ipc::WorktreeView::from(wt));
                }
            }
            Err(e) => {
                tracing::warn!("daemon: initial worktree list failed (not a git repo?): {e}");
            }
        }
        Self {
            worktrees,
            summaries: HashMap::new(),
        }
    }

    /// Snapshot the current views as a sorted vec (stable order for clients).
    fn snapshot(&self) -> Vec<ipc::WorktreeView> {
        let mut views: Vec<ipc::WorktreeView> = self.worktrees.values().cloned().collect();
        views.sort_by(|a, b| a.path.cmp(&b.path));
        views
    }
}

// ---------------------------------------------------------------------------
// Shared — everything connections + emitters need
// ---------------------------------------------------------------------------

/// Shared state passed (behind `Arc`) to every connection task and internal
/// emitter (watcher handler, agent tasks).
pub struct Shared {
    pub registry: Mutex<Registry>,
    pub backend: Arc<dyn AgentBackend>,
    pub gh: Arc<dyn GhRunner>,
    pub manager: WorktreeManager,
    pub repo_root: PathBuf,
    pub config: Config,
    pub events: broadcast::Sender<SupervisorMsg>,
    pub watch_set: Arc<Mutex<Vec<WatchItem>>>,
}

impl Shared {
    /// Broadcast a `SupervisorMsg` to all connected clients + internal listeners.
    ///
    /// A send error simply means there are no subscribers right now; that is
    /// fine (the daemon runs headless).
    fn broadcast(&self, msg: SupervisorMsg) {
        let _ = self.events.send(msg);
    }

    /// Persist a worktree status change to `.karazhan/state.toml`.
    fn persist_status(&self, path: &Path, status: &WorktreeStatus) {
        match state::load(&self.repo_root) {
            Ok(mut st) => {
                st.set_status(path, status.clone());
                if let Err(e) = state::save(&self.repo_root, &st) {
                    tracing::warn!("daemon: failed to persist worktree status: {e}");
                }
            }
            Err(e) => tracing::warn!("daemon: failed to load state for status update: {e}"),
        }
    }

    /// Update registry + persist + broadcast a `StatusChanged` for one worktree.
    ///
    /// Mirrors `App::set_worktree_status`, additionally tracking the summary.
    async fn set_status(&self, path: &Path, status: WorktreeStatus, summary: Option<String>) {
        {
            let mut reg = self.registry.lock().await;
            if let Some(view) = reg.worktrees.get_mut(path) {
                view.status = status.clone();
                if let Some(s) = &summary {
                    view.last_summary = Some(s.clone());
                }
            }
            if let Some(s) = &summary {
                reg.summaries.insert(path.to_path_buf(), s.clone());
            }
        }
        self.persist_status(path, &status);
        self.broadcast(SupervisorMsg::StatusChanged {
            worktree_path: path.to_path_buf(),
            status,
            summary,
        });
    }

    /// Rebuild the shared watch-set from the registry (worktrees with a PR).
    async fn rebuild_watch_set(&self) {
        let items: Vec<WatchItem> = {
            let reg = self.registry.lock().await;
            reg.worktrees
                .values()
                .filter_map(|v| {
                    v.pr_number.map(|pr| WatchItem {
                        worktree_path: v.path.clone(),
                        pr_number: pr,
                    })
                })
                .collect()
        };
        let mut guard = self.watch_set.lock().await;
        *guard = items;
    }

    /// Re-scan worktrees via `manager.list()` and overlay into the registry,
    /// preserving cached summaries.  Returns the resulting snapshot.
    async fn rebuild_registry(&self) -> Vec<ipc::WorktreeView> {
        let list = match self.manager.list() {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!("daemon: worktree list failed: {e}");
                Vec::new()
            }
        };
        let mut reg = self.registry.lock().await;
        let mut next: HashMap<PathBuf, ipc::WorktreeView> = HashMap::new();
        for wt in &list {
            let summary = reg.summaries.get(&wt.path).cloned();
            next.insert(
                wt.path.clone(),
                ipc::WorktreeView::from_worktree(wt, summary),
            );
        }
        reg.worktrees = next;
        // Drop summaries for worktrees that no longer exist.
        let live: std::collections::HashSet<PathBuf> = reg.worktrees.keys().cloned().collect();
        reg.summaries.retain(|p, _| live.contains(p));
        reg.snapshot()
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Build a multi-thread tokio runtime and run [`serve`] to completion.
///
/// Called from `fn main()` BEFORE any client-side tokio runtime exists, so it
/// is free to construct its own runtime.
pub fn run_supervisor() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build supervisor tokio runtime")?;
    runtime.block_on(serve())
}

// ---------------------------------------------------------------------------
// serve — bind, spawn watcher, accept loop
// ---------------------------------------------------------------------------

async fn serve() -> Result<()> {
    // Resolve paths and ensure the socket directory exists.
    let sock_dir = ipc::ensure_sock_dir().context("failed to create socket directory")?;
    let sock_path = sock_dir.join("sock");
    let pidfile = ipc::pidfile_path(&sock_path);
    let logfile = ipc::logfile_path(&sock_path);

    // Init tracing to the logfile (the daemon has no TTY).  The non-blocking
    // worker guard must live for the whole `serve()` scope.
    let _tracing_guard = init_daemon_tracing(&logfile)?;

    tracing::info!(pid = std::process::id(), "supervisor daemon starting");

    // Write the pidfile.
    if let Err(e) = std::fs::write(&pidfile, std::process::id().to_string()) {
        tracing::warn!("daemon: failed to write pidfile {}: {e}", pidfile.display());
    }

    // Remove a stale socket file, then bind.
    if sock_path.exists() {
        let _ = std::fs::remove_file(&sock_path);
    }
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("failed to bind unix socket at {}", sock_path.display()))?;
    tracing::info!(socket = %sock_path.display(), "supervisor listening");

    // Construct Shared.
    let config = Config::load();
    let repo_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (backend, backend_name) = select_backend(&config.claude_bin);
    tracing::info!("daemon: agent backend = {backend_name}");
    let gh: Arc<dyn GhRunner> = Arc::new(RealGh {
        bin: config.gh_bin.clone(),
    });
    let manager = WorktreeManager::new(repo_root.clone());
    let registry = Registry::from_manager(&manager);
    let (events, _initial_rx) = broadcast::channel::<SupervisorMsg>(BROADCAST_CAP);
    let watch_set = Arc::new(Mutex::new(Vec::<WatchItem>::new()));

    let shared = Arc::new(Shared {
        registry: Mutex::new(registry),
        backend,
        gh,
        manager,
        repo_root,
        config,
        events,
        watch_set,
    });

    // Seed the watch-set from worktrees that already carry a PR number.
    shared.rebuild_watch_set().await;

    // Spawn the watcher.  It emits AppEvents into `watch_event_tx`; the daemon
    // drains them in the select loop below (rehoming the app.rs handling).
    let (watch_event_tx, mut watch_event_rx) = mpsc::channel::<AppEvent>(64);
    let (watcher_shutdown_tx, watcher_shutdown_rx) = tokio::sync::watch::channel(false);
    let watcher_handle = spawn_watcher(
        Arc::clone(&shared.gh),
        shared.repo_root.clone(),
        watch_event_tx,
        Arc::clone(&shared.watch_set),
        WatcherConfig {
            interval: std::time::Duration::from_secs(shared.config.poll_interval_secs),
        },
        watcher_shutdown_rx,
    );

    // Shutdown signalling: a ClientMsg::Shutdown handler sets this; the select
    // loop observes it, aborts the watcher, flushes, and exits the process.
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    loop {
        tokio::select! {
            // New client connection.
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => {
                        let conn_shared = Arc::clone(&shared);
                        let conn_shutdown = shutdown_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_conn(conn_shared, stream, conn_shutdown).await {
                                tracing::debug!("daemon: connection ended: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("daemon: accept error: {e}");
                    }
                }
            }

            // Watcher event → daemon-side handler.
            Some(ev) = watch_event_rx.recv() => {
                handle_watch_event(&shared, ev).await;
            }

            // Shutdown requested by a client.
            _ = shutdown_rx.recv() => {
                tracing::info!("daemon: shutdown requested — stopping");
                let _ = watcher_shutdown_tx.send(true);
                watcher_handle.abort();
                // Best-effort cleanup of socket + pidfile.
                let _ = std::fs::remove_file(&sock_path);
                let _ = std::fs::remove_file(&pidfile);
                // Flush tracing by dropping the guard, then exit.
                drop(_tracing_guard);
                std::process::exit(0);
            }
        }
    }
}

/// Initialise tracing to a single (non-rolling) logfile for the daemon.
fn init_daemon_tracing(logfile: &Path) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let parent = logfile.parent().unwrap_or_else(|| Path::new("."));
    let file_name = logfile
        .file_name()
        .map(|f| f.to_owned())
        .unwrap_or_else(|| std::ffi::OsString::from("supervisor.log"));
    let appender = tracing_appender::rolling::never(parent, file_name);
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .try_init();

    Ok(guard)
}

// ---------------------------------------------------------------------------
// Watcher event handling (rehomed from app.rs)
// ---------------------------------------------------------------------------

/// Apply a watcher [`AppEvent`] to the daemon's registry + state, broadcasting
/// any resulting status change.  This is the daemon-side replacement for the
/// `PrMerged` / `CiStatusChanged` arms of `App::handle_app_event`.
async fn handle_watch_event(shared: &Arc<Shared>, event: AppEvent) {
    match event {
        AppEvent::PrMerged { worktree_path, pr } => {
            tracing::info!(
                worktree = %worktree_path.display(),
                pr,
                "daemon: PR merged — setting status PRMerged"
            );
            shared
                .set_status(&worktree_path, WorktreeStatus::PRMerged, None)
                .await;

            let auto_continue = {
                let reg = shared.registry.lock().await;
                reg.worktrees
                    .get(&worktree_path)
                    .map(|v| v.auto_continue_on_merge)
                    .unwrap_or(false)
            };

            if auto_continue {
                tracing::info!(
                    worktree = %worktree_path.display(),
                    "daemon: auto_continue_on_merge=true — starting continue session"
                );
                run_continue_session(Arc::clone(shared), worktree_path);
            }
        }
        AppEvent::CiStatusChanged {
            worktree_path,
            all_passing,
        } => {
            if all_passing {
                let was_failing = {
                    let reg = shared.registry.lock().await;
                    reg.worktrees
                        .get(&worktree_path)
                        .map(|v| v.status == WorktreeStatus::CIFailing)
                        .unwrap_or(false)
                };
                if was_failing {
                    tracing::info!(
                        worktree = %worktree_path.display(),
                        "daemon: CI recovered — setting status Idle"
                    );
                    shared
                        .set_status(&worktree_path, WorktreeStatus::Idle, None)
                        .await;
                }
            } else {
                tracing::info!(
                    worktree = %worktree_path.display(),
                    "daemon: CI failing — setting status CIFailing"
                );
                shared
                    .set_status(&worktree_path, WorktreeStatus::CIFailing, None)
                    .await;
            }
        }
        // The daemon never receives these from the watcher; ignore.
        other => {
            tracing::trace!("daemon: ignoring non-watcher AppEvent: {other:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// handle_conn — per-connection task
// ---------------------------------------------------------------------------

async fn handle_conn(
    shared: Arc<Shared>,
    stream: UnixStream,
    shutdown_tx: mpsc::Sender<()>,
) -> Result<()> {
    let (mut read_half, mut write_half) = stream.into_split();

    // Handshake first.
    let req: HandshakeReq = ipc::read_frame_async(&mut read_half).await?;
    if req.protocol != ipc::PROTOCOL_VERSION {
        tracing::warn!(
            client = req.client_pid,
            client_proto = req.protocol,
            our_proto = ipc::PROTOCOL_VERSION,
            "daemon: protocol mismatch — rejecting client"
        );
        ipc::write_frame_async(
            &mut write_half,
            &HandshakeResp::ProtocolMismatch {
                supervisor: ipc::PROTOCOL_VERSION,
            },
        )
        .await?;
        return Ok(());
    }

    let snapshot = {
        let reg = shared.registry.lock().await;
        reg.snapshot()
    };
    ipc::write_frame_async(
        &mut write_half,
        &HandshakeResp::Ok {
            supervisor_pid: std::process::id(),
            worktrees: snapshot,
        },
    )
    .await?;
    tracing::info!(client = req.client_pid, "daemon: client attached");

    // Subscribe to the broadcast AFTER the handshake snapshot so the client's
    // baseline is the snapshot and every later delta is delivered exactly once.
    let mut sub = shared.events.subscribe();

    loop {
        tokio::select! {
            // Client → daemon command.
            incoming = ipc::read_frame_async::<_, ClientMsg>(&mut read_half) => {
                match incoming {
                    Ok(msg) => {
                        let is_shutdown = matches!(msg, ClientMsg::Shutdown);
                        handle_client_msg(&shared, msg).await;
                        if is_shutdown {
                            // Signal the serve() loop to exit the process.
                            let _ = shutdown_tx.send(()).await;
                            return Ok(());
                        }
                    }
                    Err(e) => {
                        // EOF / read error → client disconnected; daemon lives on.
                        tracing::info!(client = req.client_pid, "daemon: client disconnected: {e}");
                        return Ok(());
                    }
                }
            }

            // Daemon → client delta.
            broadcasted = sub.recv() => {
                match broadcasted {
                    Ok(msg) => {
                        if ipc::write_frame_async(&mut write_half, &msg).await.is_err() {
                            tracing::info!(client = req.client_pid, "daemon: write failed; dropping client");
                            return Ok(());
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(client = req.client_pid, "daemon: client lagged {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Ok(());
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ClientMsg handlers
// ---------------------------------------------------------------------------

/// Dispatch a single [`ClientMsg`], mutating [`Shared`] and broadcasting results.
async fn handle_client_msg(shared: &Arc<Shared>, msg: ClientMsg) {
    match msg {
        ClientMsg::Refresh => {
            let snapshot = shared.rebuild_registry().await;
            shared.rebuild_watch_set().await;
            shared.broadcast(SupervisorMsg::Snapshot {
                worktrees: snapshot,
            });
        }
        ClientMsg::RunPrompt {
            worktree_path,
            prompt,
        } => {
            run_prompt(Arc::clone(shared), worktree_path, prompt).await;
        }
        ClientMsg::RunBuiltin {
            worktree_path,
            kind,
        } => {
            run_builtin(Arc::clone(shared), worktree_path, kind);
        }
        ClientMsg::SetAutoContinue {
            worktree_path,
            enabled,
        } => {
            set_auto_continue(shared, &worktree_path, enabled).await;
        }
        ClientMsg::SetPrNumber { worktree_path, pr } => {
            set_pr_number(shared, &worktree_path, pr).await;
        }
        ClientMsg::CreateWorktree {
            prompt_slug,
            branch,
            path,
        } => {
            create_worktree(shared, prompt_slug, branch, path).await;
        }
        ClientMsg::RemoveWorktree { path, force } => {
            remove_worktree(shared, &path, force).await;
        }
        ClientMsg::Shutdown => {
            // Handled by the caller (signals serve() to exit); nothing here.
        }
    }
}

/// `RunPrompt` — set Running, then spawn a task that drives the agent session,
/// mirroring `App::run_agent` but writing to the registry + broadcast.
async fn run_prompt(shared: Arc<Shared>, worktree_path: PathBuf, prompt: String) {
    // Clear any stale summary and mark Running.
    {
        let mut reg = shared.registry.lock().await;
        reg.summaries.remove(&worktree_path);
    }
    shared
        .set_status(&worktree_path, WorktreeStatus::Running, None)
        .await;

    let backend = Arc::clone(&shared.backend);
    let task_shared = Arc::clone(&shared);
    let path = worktree_path.clone();

    tracing::info!(worktree = %worktree_path.display(), "daemon: running agent");

    tokio::spawn(async move {
        match backend.start(&path, &prompt).await {
            Ok(handle) => drive_session(task_shared, path, handle).await,
            Err(e) => {
                tracing::error!("daemon: failed to start agent: {e}");
                task_shared
                    .set_status(
                        &path,
                        agent_status_to_worktree_status(&AgentStatus::Error(format!("{e}"))),
                        None,
                    )
                    .await;
            }
        }
    });
}

/// Continue the most recent session (auto-continue on merge) — mirrors
/// `App::run_agent_continue`.
fn run_continue_session(shared: Arc<Shared>, worktree_path: PathBuf) {
    let backend = Arc::clone(&shared.backend);
    let prompt = shared.config.auto_continue_prompt.clone();
    let path = worktree_path.clone();

    tokio::spawn(async move {
        // Mark Running + clear summary.
        {
            let mut reg = shared.registry.lock().await;
            reg.summaries.remove(&path);
        }
        shared
            .set_status(&path, WorktreeStatus::Running, None)
            .await;

        match backend.continue_session(&path, &prompt).await {
            Ok(handle) => drive_session(Arc::clone(&shared), path.clone(), handle).await,
            Err(e) => {
                tracing::error!("daemon: failed to start auto-continue session: {e}");
                shared
                    .set_status(
                        &path,
                        agent_status_to_worktree_status(&AgentStatus::Error(format!("{e}"))),
                        None,
                    )
                    .await;
            }
        }
    });
}

/// Drive a [`SessionHandle`] to completion, mapping each [`StatusUpdate`] onto a
/// registry + broadcast `StatusChanged`.  Shared by `run_prompt` and
/// `run_continue_session`.
async fn drive_session(shared: Arc<Shared>, path: PathBuf, handle: crate::agent::SessionHandle) {
    let (status_tx, mut status_rx) = mpsc::channel::<StatusUpdate>(16);

    let runner = tokio::spawn(async move { run_session(handle, status_tx).await });

    while let Some(update) = status_rx.recv().await {
        let wt_status = agent_status_to_worktree_status(&update.status);
        shared
            .set_status(&update.worktree_path, wt_status, update.summary.clone())
            .await;
        tracing::info!(
            worktree = %update.worktree_path.display(),
            "daemon: agent status: {:?}",
            update.status
        );
    }

    if let Ok(Err(e)) = runner.await {
        tracing::error!("daemon: session runner failed: {e}");
        shared
            .set_status(
                &path,
                agent_status_to_worktree_status(&AgentStatus::Error(format!("{e}"))),
                None,
            )
            .await;
    }
}

/// `RunBuiltin` — resolve a PR number, compose the prompt via `gh`, then run it
/// through the same path as `RunPrompt`.  Mirrors `App::spawn_gh_command`.
fn run_builtin(shared: Arc<Shared>, worktree_path: PathBuf, kind: BuiltinKind) {
    tokio::spawn(async move {
        let pr_opt = resolve_pr_number(&shared, &worktree_path).await;

        let result: Result<String> = match pr_opt {
            None => Err(anyhow::anyhow!(
                "no open PR found for worktree {}",
                worktree_path.display()
            )),
            Some(pr) => match kind {
                BuiltinKind::AddressPrComments => {
                    build_address_pr_comments_prompt(shared.gh.as_ref(), &worktree_path, pr).await
                }
                BuiltinKind::CheckCi => {
                    build_check_ci_prompt(shared.gh.as_ref(), &worktree_path, pr).await
                }
            },
        };

        match result {
            Ok(prompt) => run_prompt(Arc::clone(&shared), worktree_path, prompt).await,
            Err(e) => {
                tracing::warn!(worktree = %worktree_path.display(), "daemon: gh command error: {e}");
                shared.broadcast(SupervisorMsg::Error {
                    worktree_path: Some(worktree_path),
                    message: format!("{e}"),
                });
            }
        }
    });
}

/// Resolve the PR number for a worktree: registry value, else `gh` lookup
/// (persisting + registry-updating on discovery).  Mirrors `App::resolve_pr_number`.
async fn resolve_pr_number(shared: &Arc<Shared>, worktree_path: &Path) -> Option<u64> {
    {
        let reg = shared.registry.lock().await;
        if let Some(view) = reg.worktrees.get(worktree_path) {
            if let Some(n) = view.pr_number {
                return Some(n);
            }
        }
    }

    match pr_for_current_branch(shared.gh.as_ref(), worktree_path).await {
        Ok(Some(n)) => {
            // Persist + reflect in registry.
            set_pr_number(shared, worktree_path, Some(n)).await;
            Some(n)
        }
        Ok(None) => None,
        Err(e) => {
            tracing::warn!("daemon: pr_for_current_branch failed: {e}");
            None
        }
    }
}

/// `SetAutoContinue` — update registry + persist + broadcast a fresh Snapshot.
async fn set_auto_continue(shared: &Arc<Shared>, path: &Path, enabled: bool) {
    {
        let mut reg = shared.registry.lock().await;
        if let Some(view) = reg.worktrees.get_mut(path) {
            view.auto_continue_on_merge = enabled;
        }
    }
    match state::load(&shared.repo_root) {
        Ok(mut st) => {
            st.set_auto_continue(path, enabled);
            if let Err(e) = state::save(&shared.repo_root, &st) {
                tracing::warn!("daemon: failed to persist auto_continue: {e}");
            }
        }
        Err(e) => tracing::warn!("daemon: failed to load state for auto_continue: {e}"),
    }
    let snapshot = {
        let reg = shared.registry.lock().await;
        reg.snapshot()
    };
    shared.broadcast(SupervisorMsg::Snapshot {
        worktrees: snapshot,
    });
}

/// `SetPrNumber` — update registry + persist + refresh watch-set + Snapshot.
async fn set_pr_number(shared: &Arc<Shared>, path: &Path, pr: Option<u64>) {
    {
        let mut reg = shared.registry.lock().await;
        if let Some(view) = reg.worktrees.get_mut(path) {
            view.pr_number = pr;
        }
    }
    match state::load(&shared.repo_root) {
        Ok(mut st) => {
            st.set_pr_number(path, pr);
            if let Err(e) = state::save(&shared.repo_root, &st) {
                tracing::warn!("daemon: failed to persist pr_number: {e}");
            }
        }
        Err(e) => tracing::warn!("daemon: failed to load state for pr_number: {e}"),
    }
    shared.rebuild_watch_set().await;
    let snapshot = {
        let reg = shared.registry.lock().await;
        reg.snapshot()
    };
    shared.broadcast(SupervisorMsg::Snapshot {
        worktrees: snapshot,
    });
}

/// `CreateWorktree` — create via the manager, refresh registry, broadcast.
async fn create_worktree(
    shared: &Arc<Shared>,
    prompt_slug: Option<String>,
    branch: String,
    path: PathBuf,
) {
    match shared.manager.create(prompt_slug, &branch, &path) {
        Ok(_) => {
            let snapshot = shared.rebuild_registry().await;
            shared.rebuild_watch_set().await;
            shared.broadcast(SupervisorMsg::Snapshot {
                worktrees: snapshot,
            });
        }
        Err(e) => {
            tracing::warn!("daemon: create worktree failed: {e}");
            shared.broadcast(SupervisorMsg::Error {
                worktree_path: None,
                message: format!("{e}"),
            });
        }
    }
}

/// `RemoveWorktree` — remove via the manager, refresh registry, broadcast.
async fn remove_worktree(shared: &Arc<Shared>, path: &Path, force: bool) {
    match shared.manager.remove(path, force) {
        Ok(()) => {
            let snapshot = shared.rebuild_registry().await;
            shared.rebuild_watch_set().await;
            shared.broadcast(SupervisorMsg::Snapshot {
                worktrees: snapshot,
            });
        }
        Err(e) => {
            tracing::warn!("daemon: remove worktree failed: {e}");
            shared.broadcast(SupervisorMsg::Error {
                worktree_path: Some(path.to_path_buf()),
                message: format!("{e}"),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Backend selection (mirrors app.rs)
// ---------------------------------------------------------------------------

/// Choose the active agent backend at startup: real `claude` if on PATH, else
/// the offline mock.
fn select_backend(claude_bin: &str) -> (Arc<dyn AgentBackend>, &'static str) {
    if claude_on_path(claude_bin) {
        tracing::info!("daemon: agent backend ClaudeCodeBackend ({claude_bin} found on PATH)");
        (
            Arc::new(ClaudeCodeBackend {
                bin: claude_bin.to_string(),
            }),
            "ClaudeCode",
        )
    } else {
        tracing::warn!("daemon: agent backend MockBackend ({claude_bin} not found on PATH)");
        (Arc::new(MockBackend::new()), "Mock")
    }
}

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

// ---------------------------------------------------------------------------
// Autostart: double-fork helper
// ---------------------------------------------------------------------------

/// Spawn a detached supervisor daemon via the classic double-fork dance.
///
/// 1. `fork()` → parent waits for the intermediate child and returns `Ok`.
/// 2. Intermediate child `setsid()` (new session, no controlling terminal),
///    then `fork()` again and exits, orphaning the grandchild to init.
/// 3. Grandchild redirects stdin from `/dev/null` and stdout/stderr to the
///    daemon logfile, then `exec`s the current executable with `--supervisor`.
///    `exec` replaces the process image, so this branch never returns.
///
/// Unix-only (the whole app targets unix / macOS).
pub fn spawn_supervisor() -> Result<()> {
    use nix::sys::wait::waitpid;
    use nix::unistd::{fork, ForkResult};

    // Resolve the logfile path up-front (cheap, no fork in between).
    let sock_dir = ipc::ensure_sock_dir().context("failed to create socket directory")?;
    let logfile = ipc::logfile_path(&sock_dir.join("sock"));

    // SAFETY: between fork() and exec()/_exit() in the children we only call
    // async-signal-safe operations (fork, setsid, open, dup2, exec) plus the
    // pre-resolved owned paths.  No allocation-heavy or lock-taking code runs in
    // the child before exec.
    match unsafe { fork() }.context("first fork failed")? {
        ForkResult::Parent { child } => {
            // Reap the intermediate child so it does not become a zombie.
            let _ = waitpid(child, None);
            Ok(())
        }
        ForkResult::Child => {
            // Intermediate child: detach into a new session.
            if nix::unistd::setsid().is_err() {
                // Cannot recover in the child; bail hard.
                unsafe { libc_exit(1) };
            }

            match unsafe { fork() } {
                Ok(ForkResult::Parent { .. }) => {
                    // Parent-of-grandchild exits immediately, orphaning the grandchild.
                    unsafe { libc_exit(0) };
                }
                Ok(ForkResult::Child) => {
                    // Grandchild: redirect std streams, then exec self.
                    redirect_std_to_log(&logfile);
                    let _ = exec_self_supervisor();
                    // exec only returns on error.
                    unsafe { libc_exit(1) };
                }
                Err(_) => unsafe { libc_exit(1) },
            }
        }
    }
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
        // Keep `log` alive long enough for the dup2 calls; leaking is fine since
        // we are about to exec (the new image inherits fds 1/2 already dup'd).
        std::mem::forget(log);
    }
}

/// `exec` the current executable with the `--supervisor` flag.  Returns only on
/// error (success replaces the process image).
fn exec_self_supervisor() -> Result<()> {
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe().context("cannot resolve current_exe")?;
    let err = std::process::Command::new(exe).arg("--supervisor").exec();
    Err(anyhow::anyhow!("exec --supervisor failed: {err}"))
}

/// Minimal `_exit` wrapper (async-signal-safe) for child branches.
unsafe fn libc_exit(code: i32) -> ! {
    // `std::process::exit` runs atexit handlers / flushes; in a forked child we
    // want the raw `_exit`.  nix re-exports libc, so use it directly.
    nix::libc::_exit(code)
}

// ---------------------------------------------------------------------------
// wait_for_socket — used by the client (8c)
// ---------------------------------------------------------------------------

/// Poll-connect to `path` until it accepts a connection or `timeout` elapses.
pub async fn wait_for_socket(path: &Path, timeout: std::time::Duration) -> Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match UnixStream::connect(path).await {
            Ok(_) => return Ok(()),
            Err(_) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "timed out waiting for socket {}: {e}",
                    path.display()
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::mock::MockGh;
    use std::process::Command;
    use std::time::Duration;

    // -- Test fixtures --------------------------------------------------------

    /// Create a real temporary git repository (mirrors the worktree-test fixture).
    fn make_temp_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap_or_else(|e| panic!("git {args:?} failed: {e}"));
            assert!(status.success(), "git {args:?} non-zero");
        };
        run(&["init"]);
        run(&["config", "user.email", "test@karazhan.test"]);
        run(&["config", "user.name", "Karazhan Test"]);
        std::fs::write(root.join("README.md"), "karazhan test\n").expect("write README");
        run(&["add", "README.md"]);
        run(&["commit", "-m", "initial"]);
        (dir, root)
    }

    /// Build a `Shared` over a temp repo with a fast MockBackend + MockGh.
    fn make_shared(root: PathBuf, gh: Arc<dyn GhRunner>) -> Arc<Shared> {
        let manager = WorktreeManager::new(root.clone());
        let registry = Registry::from_manager(&manager);
        let (events, _rx) = broadcast::channel::<SupervisorMsg>(BROADCAST_CAP);
        Arc::new(Shared {
            registry: Mutex::new(registry),
            backend: Arc::new(MockBackend {
                delay: Duration::from_millis(5),
            }),
            gh,
            manager,
            repo_root: root,
            config: Config::default(),
            events,
            watch_set: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Seed a single worktree directly into the registry + state for tests that
    /// don't want to spin a real `git worktree add`.
    async fn seed_worktree(
        shared: &Arc<Shared>,
        path: &Path,
        auto_continue: bool,
        pr: Option<u64>,
    ) {
        let wt = crate::worktree::model::Worktree {
            path: path.to_path_buf(),
            branch: "feat".to_string(),
            prompt_slug: None,
            pr_number: pr,
            auto_continue_on_merge: auto_continue,
            status: WorktreeStatus::Idle,
        };
        {
            let mut reg = shared.registry.lock().await;
            reg.worktrees
                .insert(path.to_path_buf(), ipc::WorktreeView::from(&wt));
        }
        let mut st = state::load(&shared.repo_root).expect("load state");
        st.upsert_worktree(wt);
        state::save(&shared.repo_root, &st).expect("save state");
    }

    // -- RunPrompt ------------------------------------------------------------

    #[tokio::test]
    async fn run_prompt_sets_running_then_needs_review() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root, gh);

        let path = PathBuf::from("/tmp/run-prompt-wt");
        seed_worktree(&shared, &path, false, None).await;

        let mut sub = shared.events.subscribe();

        handle_client_msg(
            &shared,
            ClientMsg::RunPrompt {
                worktree_path: path.clone(),
                prompt: "do the thing".to_string(),
            },
        )
        .await;

        // First broadcast: Running.
        let first = tokio::time::timeout(Duration::from_secs(2), sub.recv())
            .await
            .expect("timeout")
            .expect("recv");
        assert!(matches!(
            first,
            SupervisorMsg::StatusChanged {
                status: WorktreeStatus::Running,
                ..
            }
        ));

        // Collect subsequent updates; expect to eventually reach NeedsReview (Done).
        let mut reached_review = false;
        for _ in 0..5 {
            match tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
                Ok(Ok(SupervisorMsg::StatusChanged { status, .. })) => {
                    if status == WorktreeStatus::NeedsReview {
                        reached_review = true;
                        break;
                    }
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        assert!(reached_review, "expected NeedsReview after mock session");

        let reg = shared.registry.lock().await;
        assert_eq!(
            reg.worktrees.get(&path).map(|v| v.status.clone()),
            Some(WorktreeStatus::NeedsReview)
        );
    }

    // -- SetAutoContinue ------------------------------------------------------

    #[tokio::test]
    async fn set_auto_continue_persists_and_reflects() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root.clone(), gh);

        let path = PathBuf::from("/tmp/ac-wt");
        seed_worktree(&shared, &path, false, None).await;

        handle_client_msg(
            &shared,
            ClientMsg::SetAutoContinue {
                worktree_path: path.clone(),
                enabled: true,
            },
        )
        .await;

        // Registry reflects it.
        {
            let reg = shared.registry.lock().await;
            assert!(reg.worktrees.get(&path).unwrap().auto_continue_on_merge);
        }
        // State persists it.
        let st = state::load(&root).expect("load");
        assert!(
            st.worktrees
                .iter()
                .find(|w| w.path == path)
                .unwrap()
                .auto_continue_on_merge
        );
    }

    // -- SetPrNumber updates watch-set ---------------------------------------

    #[tokio::test]
    async fn set_pr_number_updates_watch_set() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root, gh);

        let path = PathBuf::from("/tmp/pr-wt");
        seed_worktree(&shared, &path, false, None).await;

        // No PR → watch-set empty.
        shared.rebuild_watch_set().await;
        assert!(shared.watch_set.lock().await.is_empty());

        handle_client_msg(
            &shared,
            ClientMsg::SetPrNumber {
                worktree_path: path.clone(),
                pr: Some(77),
            },
        )
        .await;

        let ws = shared.watch_set.lock().await;
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].pr_number, 77);
        assert_eq!(ws[0].worktree_path, path);
    }

    // -- Refresh rebuilds from a real temp repo ------------------------------

    #[tokio::test]
    async fn refresh_rebuilds_registry_from_repo() {
        let (_dir, root) = make_temp_repo();
        let wt_dir = tempfile::tempdir().expect("wt tempdir");
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root.clone(), gh);

        // Create a real worktree on disk via the manager.
        shared
            .manager
            .create(Some("slug".to_string()), "feat-x", wt_dir.path())
            .expect("create worktree");
        let canonical = wt_dir.path().canonicalize().expect("canonicalize");

        handle_client_msg(&shared, ClientMsg::Refresh).await;

        let reg = shared.registry.lock().await;
        assert!(
            reg.worktrees.contains_key(&canonical),
            "refresh should pick up the created worktree; got {:?}",
            reg.worktrees.keys().collect::<Vec<_>>()
        );
    }

    // -- Watcher PrMerged → auto-continue ------------------------------------

    #[tokio::test]
    async fn watcher_pr_merged_triggers_continue_for_auto_worktree() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root, gh);

        let path = PathBuf::from("/tmp/auto-merge-wt");
        seed_worktree(&shared, &path, /* auto_continue */ true, Some(5)).await;

        let mut sub = shared.events.subscribe();

        // Feed a PrMerged event as the watcher would.
        handle_watch_event(
            &shared,
            AppEvent::PrMerged {
                worktree_path: path.clone(),
                pr: 5,
            },
        )
        .await;

        // We expect: PRMerged, then Running (continue session start), then
        // eventually NeedsReview (mock session Done).
        let mut saw_pr_merged = false;
        let mut saw_running = false;
        let mut saw_review = false;
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
                Ok(Ok(SupervisorMsg::StatusChanged { status, .. })) => match status {
                    WorktreeStatus::PRMerged => saw_pr_merged = true,
                    WorktreeStatus::Running => saw_running = true,
                    WorktreeStatus::NeedsReview => {
                        saw_review = true;
                        break;
                    }
                    _ => {}
                },
                _ => break,
            }
        }
        assert!(saw_pr_merged, "expected PRMerged status");
        assert!(
            saw_running,
            "expected Running status from auto-continue session"
        );
        assert!(saw_review, "expected NeedsReview after continue session");
    }

    #[tokio::test]
    async fn watcher_pr_merged_no_continue_when_flag_off() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root, gh);

        let path = PathBuf::from("/tmp/no-auto-wt");
        seed_worktree(&shared, &path, /* auto_continue */ false, Some(9)).await;

        handle_watch_event(
            &shared,
            AppEvent::PrMerged {
                worktree_path: path.clone(),
                pr: 9,
            },
        )
        .await;

        // Status should be PRMerged and NOT progress to Running.
        let reg = shared.registry.lock().await;
        assert_eq!(
            reg.worktrees.get(&path).map(|v| v.status.clone()),
            Some(WorktreeStatus::PRMerged)
        );
    }
}

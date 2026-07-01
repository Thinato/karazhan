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
    agent_status_to_worktree_status, configured::ConfiguredBackend, mock::MockBackend,
    AgentBackend, AgentStatus,
};
use crate::app::AppEvent;
use crate::config::Config;
use crate::github::commands::{build_address_pr_comments_prompt, build_check_ci_prompt};
use crate::github::pr::pr_for_current_branch;
use crate::github::{GhRunner, RealGh};
use crate::ipc::{self, BuiltinKind, ClientMsg, HandshakeReq, HandshakeResp, SupervisorMsg};
use crate::project_config::{ProjectConfig, WorktreeSettings};
use crate::projects;
use crate::watcher::{spawn_watcher, WatchItem, WatcherConfig};
use crate::worktree::model::PrStatus;
use crate::worktree::{state, WorktreeManager, WorktreeStatus};

/// Capacity of the broadcast channel that fans `SupervisorMsg`s out to clients.
const BROADCAST_CAP: usize = 256;

// ---------------------------------------------------------------------------
// Registry — authoritative in-memory state
// ---------------------------------------------------------------------------

/// One managed project (git repo) with its own worktree manager, agent config,
/// and agent backend.  Each repo may configure a different agent.
pub struct ProjectRuntime {
    pub name: String,
    pub root: PathBuf,
    pub manager: WorktreeManager,
    pub project_config: ProjectConfig,
    pub backend: Arc<dyn AgentBackend>,
    pub backend_name: &'static str,
}

impl ProjectRuntime {
    /// Build a runtime for `project`: load its per-repo `.karazhan/config.toml`
    /// and select its agent backend (Mock fallback when the configured command
    /// is not on PATH).
    fn from_project(project: &projects::Project) -> Self {
        let root = project.path.clone();
        let project_config = ProjectConfig::load(&root);
        let (backend, backend_name) = select_backend(project_config.clone());
        tracing::info!(
            project = %project.name,
            root = %root.display(),
            "daemon: project backend = {backend_name}"
        );
        Self {
            name: project.name.clone(),
            root: root.clone(),
            manager: WorktreeManager::new(root),
            project_config,
            backend,
            backend_name,
        }
    }
}

/// Ephemeral per-run progress carried from the agent stream to the client.
#[derive(Default, Clone)]
pub struct Progress {
    pub activity: Option<String>,
    pub turns: u32,
    pub tokens: u64,
}

/// Authoritative worktree state held by the daemon.
///
/// `worktrees` is the live set keyed by canonical path; `summaries` holds the
/// most recent agent summary line per worktree.  `project_of` maps each
/// worktree path to its owning project's name, and `order` records the flat
/// snapshot order (project order, then per-project list order).
pub struct Registry {
    pub worktrees: HashMap<PathBuf, ipc::WorktreeView>,
    pub summaries: HashMap<PathBuf, String>,
    pub project_of: HashMap<PathBuf, String>,
    pub order: Vec<PathBuf>,
}

impl Registry {
    /// An empty registry.
    fn empty() -> Self {
        Self {
            worktrees: HashMap::new(),
            summaries: HashMap::new(),
            project_of: HashMap::new(),
            order: Vec::new(),
        }
    }

    /// Snapshot the current views in flat project order.
    fn snapshot(&self) -> Vec<ipc::WorktreeView> {
        self.order
            .iter()
            .filter_map(|p| self.worktrees.get(p).cloned())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Shared — everything connections + emitters need
// ---------------------------------------------------------------------------

/// Shared state passed (behind `Arc`) to every connection task and internal
/// emitter (watcher handler, agent tasks).
pub struct Shared {
    /// All managed projects.  `AddProject` mutates this, so it is behind a Mutex.
    pub projects: Mutex<Vec<ProjectRuntime>>,
    pub registry: Mutex<Registry>,
    pub gh: Arc<dyn GhRunner>,
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

    /// Log **and** broadcast a `SupervisorMsg::Error`.
    ///
    /// Every daemon error goes to BOTH the daemon log file (via `tracing::error!`)
    /// and the connected clients' status line (via broadcast).  Calling this
    /// helper guarantees neither channel is missed — errors vanish from the client
    /// toast when it clears but remain in `~/.cache/karazhan/supervisor.log`.
    fn error(&self, worktree_path: Option<PathBuf>, message: String) {
        tracing::error!(?worktree_path, %message, "daemon error");
        self.broadcast(SupervisorMsg::Error {
            worktree_path,
            message,
        });
    }

    /// Broadcast a full `Snapshot` of `worktrees` tagged with the current ordered
    /// project list (so clients can render zero-worktree projects too).
    async fn broadcast_snapshot(&self, worktrees: Vec<ipc::WorktreeView>) {
        let projects = self.project_infos().await;
        self.broadcast(SupervisorMsg::Snapshot {
            projects,
            worktrees,
        });
    }

    /// Resolve the owning project's root for a worktree `path`.
    ///
    /// First consults the registry's `project_of` map; falls back to matching
    /// the deepest project root that is an ancestor of `path`.
    async fn project_root_for(&self, path: &Path) -> Option<PathBuf> {
        let name = {
            let reg = self.registry.lock().await;
            reg.project_of.get(path).cloned()
        };
        let projects = self.projects.lock().await;
        if let Some(name) = name {
            if let Some(p) = projects.iter().find(|p| p.name == name) {
                return Some(p.root.clone());
            }
        }
        // Fallback: longest ancestor match.
        projects
            .iter()
            .filter(|p| path.starts_with(&p.root))
            .max_by_key(|p| p.root.as_os_str().len())
            .map(|p| p.root.clone())
    }

    /// Resolve the owning project's agent backend for a worktree `path`.
    async fn backend_for(&self, path: &Path) -> Option<Arc<dyn AgentBackend>> {
        let name = {
            let reg = self.registry.lock().await;
            reg.project_of.get(path).cloned()
        };
        let projects = self.projects.lock().await;
        let runtime = if let Some(name) = name {
            projects.iter().find(|p| p.name == name)
        } else {
            projects
                .iter()
                .filter(|p| path.starts_with(&p.root))
                .max_by_key(|p| p.root.as_os_str().len())
        };
        runtime.map(|p| Arc::clone(&p.backend))
    }

    /// Persist a worktree status change to the OWNING project's `state.toml`.
    async fn persist_status(&self, path: &Path, status: &WorktreeStatus) {
        let Some(root) = self.project_root_for(path).await else {
            tracing::warn!("daemon: no owning project for {} (status)", path.display());
            return;
        };
        match state::load(&root) {
            Ok(mut st) => {
                st.set_status(path, status.clone());
                if let Err(e) = state::save(&root, &st) {
                    tracing::warn!("daemon: failed to persist worktree status: {e}");
                }
            }
            Err(e) => tracing::warn!("daemon: failed to load state for status update: {e}"),
        }
    }

    /// Update registry + persist + broadcast a `StatusChanged` for one worktree.
    ///
    /// `progress` carries the live agent action / turn / token counters parsed
    /// from the stream.  On the FIRST transition into `Running` the run timer is
    /// stamped and the counters reset; on any terminal status the timer + live
    /// activity are cleared (counters are retained for post-run display).
    async fn set_status(
        &self,
        path: &Path,
        status: WorktreeStatus,
        summary: Option<String>,
        progress: Option<Progress>,
    ) {
        let now = chrono::Utc::now();
        let (activity, turns, tokens, run_started_at);
        {
            let mut reg = self.registry.lock().await;
            if let Some(view) = reg.worktrees.get_mut(path) {
                let was_running = matches!(view.status, WorktreeStatus::Running);
                view.status = status.clone();
                if let Some(s) = &summary {
                    view.last_summary = Some(s.clone());
                }
                if matches!(status, WorktreeStatus::Running) {
                    if !was_running {
                        // Fresh run: stamp the timer and reset live counters.
                        view.run_started_at = Some(now);
                        view.turns = 0;
                        view.tokens = 0;
                        view.activity = None;
                    }
                    if let Some(p) = &progress {
                        view.activity = p.activity.clone();
                        view.turns = p.turns;
                        view.tokens = p.tokens;
                    }
                } else {
                    view.run_started_at = None;
                    view.activity = None;
                    if let Some(p) = &progress {
                        view.turns = p.turns;
                        view.tokens = p.tokens;
                    }
                }
                activity = view.activity.clone();
                turns = view.turns;
                tokens = view.tokens;
                run_started_at = view.run_started_at;
            } else {
                activity = None;
                turns = 0;
                tokens = 0;
                run_started_at = None;
            }
            if let Some(s) = &summary {
                reg.summaries.insert(path.to_path_buf(), s.clone());
            }
        }
        self.persist_status(path, &status).await;
        self.broadcast(SupervisorMsg::StatusChanged {
            worktree_path: path.to_path_buf(),
            status,
            summary,
            activity,
            turns,
            tokens,
            run_started_at,
        });
    }

    /// Persist the agent `session_id` for a worktree to its project's state.toml
    /// (no `updated_at` bump — captured mid-run, not a user action).  Used so the
    /// session can later be resumed deterministically via `--resume <id>`.
    async fn persist_session_id(&self, path: &Path, session_id: &str) {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(root) = self.project_root_for(path).await {
            match state::load(&root) {
                Ok(mut st) => {
                    st.set_session_id_no_touch(&canonical, Some(session_id.to_string()));
                    st.set_session_id_no_touch(path, Some(session_id.to_string()));
                    if let Err(e) = state::save(&root, &st) {
                        tracing::warn!("daemon: failed to persist session_id: {e}");
                    }
                }
                Err(e) => tracing::warn!("daemon: failed to load state for session_id: {e}"),
            }
        }

        // Reflect in the in-memory view + push a Snapshot so the client's "copy
        // resume command" (`s`) can build a `--resume <id>` line promptly, without
        // waiting for the next registry rebuild.
        let snapshot = {
            let mut reg = self.registry.lock().await;
            for key in [path, canonical.as_path()] {
                if let Some(view) = reg.worktrees.get_mut(key) {
                    view.session_id = Some(session_id.to_string());
                }
            }
            reg.snapshot()
        };
        self.broadcast_snapshot(snapshot).await;
    }

    /// Read the last persisted agent `session_id` for a worktree (for `--resume`).
    async fn session_id_for(&self, path: &Path) -> Option<String> {
        let root = self.project_root_for(path).await?;
        let st = state::load(&root).ok()?;
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        st.worktrees
            .iter()
            .find(|w| w.path == path || w.path == canonical)
            .and_then(|w| w.session_id.clone())
    }

    /// Update a worktree's `pr_status` (+ optional `pr_number` + optional `pr_url`)
    /// in the registry and the OWNING project's `state.toml`, then broadcast a
    /// fresh Snapshot.
    ///
    /// Polling is NOT user/agent activity, so `updated_at` is deliberately left
    /// untouched.  Returns the PREVIOUS `pr_status` so the caller can detect the
    /// transition edge (e.g. `…→Merged`) for auto-continue.
    async fn set_pr_status(
        &self,
        path: &Path,
        pr_status: PrStatus,
        pr_number: Option<u64>,
        pr_url: Option<String>,
        pr_title: Option<String>,
        unresolved: Option<u64>,
    ) -> Option<PrStatus> {
        // No meaningful unresolved-comment count when there is no open PR.
        let unresolved = match pr_status {
            PrStatus::NoPr | PrStatus::Merged | PrStatus::Closed => None,
            _ => unresolved,
        };

        let prev = {
            let mut reg = self.registry.lock().await;
            if let Some(view) = reg.worktrees.get_mut(path) {
                let prev = view.pr_status;
                view.pr_status = pr_status;
                if let Some(n) = pr_number {
                    view.pr_number = Some(n);
                }
                // Always update pr_url (clears it when NoPr passes None).
                view.pr_url = pr_url.clone();
                // Always update pr_title (clears it when NoPr passes None).
                view.pr_title = pr_title.clone();
                // Always update the unresolved count (cleared for non-open PRs above).
                view.unresolved_comments = unresolved;
                Some(prev)
            } else {
                None
            }
        };

        if let Some(root) = self.project_root_for(path).await {
            match state::load(&root) {
                Ok(mut st) => {
                    st.set_pr_status(path, pr_status);
                    if let Some(n) = pr_number {
                        st.set_pr_number_no_touch(path, Some(n));
                    }
                    st.set_pr_url_no_touch(path, pr_url);
                    st.set_pr_title_no_touch(path, pr_title);
                    st.set_unresolved_no_touch(path, unresolved);
                    if let Err(e) = state::save(&root, &st) {
                        tracing::warn!("daemon: failed to persist pr_status: {e}");
                    }
                }
                Err(e) => tracing::warn!("daemon: failed to load state for pr_status: {e}"),
            }
        } else {
            tracing::warn!(
                "daemon: no owning project for {} (pr_status)",
                path.display()
            );
        }

        let snapshot = {
            let reg = self.registry.lock().await;
            reg.snapshot()
        };
        self.broadcast_snapshot(snapshot).await;

        prev
    }

    /// Rebuild the shared watch-set from the registry: EVERY worktree across
    /// ALL projects (PR discovery is by branch now, so no pr_number filter).
    ///
    /// The GitHub `(owner, repo)` coordinates are computed ONCE per worktree here
    /// (via `git config --get remote.origin.url` against the owning project root)
    /// and stored on each [`WatchItem`], so the watcher never shells `git` on the
    /// hot per-tick path.  Results are cached per project root within this rebuild
    /// so each project's remote is parsed at most once.
    async fn rebuild_watch_set(&self) {
        // Snapshot (path, owning-project-name) pairs without holding locks across
        // the blocking git calls below.
        let entries: Vec<(PathBuf, Option<String>)> = {
            let reg = self.registry.lock().await;
            reg.worktrees
                .values()
                .map(|v| (v.path.clone(), reg.project_of.get(&v.path).cloned()))
                .collect()
        };
        // name -> root for resolving the owning project's repo root.
        let roots: HashMap<String, PathBuf> = {
            let projects = self.projects.lock().await;
            projects
                .iter()
                .map(|p| (p.name.clone(), p.root.clone()))
                .collect()
        };

        // Cache (owner, repo) per project root so we parse each remote once.
        let mut owner_repo_cache: HashMap<PathBuf, Option<(String, String)>> = HashMap::new();
        let mut items: Vec<WatchItem> = Vec::with_capacity(entries.len());
        for (path, project_name) in entries {
            // Prefer the owning project's root; fall back to the worktree path
            // itself (a linked worktree shares the repo remote, so either works).
            let root = project_name
                .as_ref()
                .and_then(|n| roots.get(n).cloned())
                .unwrap_or_else(|| path.clone());

            let owner_repo = owner_repo_cache
                .entry(root.clone())
                .or_insert_with(|| projects::git_owner_repo(&root))
                .clone();
            let (owner, repo) = match owner_repo {
                Some((o, r)) => (Some(o), Some(r)),
                None => (None, None),
            };
            items.push(WatchItem {
                worktree_path: path,
                project_root: root,
                owner,
                repo,
            });
        }

        let mut guard = self.watch_set.lock().await;
        *guard = items;
    }

    /// The ordered list of managed projects (name + root path), in the same
    /// order the daemon uses for the registry/grid grouping (the `projects` vec
    /// order).
    async fn project_infos(&self) -> Vec<ipc::ProjectInfo> {
        self.projects
            .lock()
            .await
            .iter()
            .map(|p| ipc::ProjectInfo {
                name: p.name.clone(),
                path: p.root.clone(),
            })
            .collect()
    }

    /// Re-scan worktrees across ALL projects and overlay into the registry,
    /// tagging each view with its owning project's name and concatenating in
    /// project order (projects vec order, then per-project list order).
    /// Preserves cached summaries.  Returns the resulting snapshot.
    async fn rebuild_registry(&self) -> Vec<ipc::WorktreeView> {
        // Collect (project_name, worktree-list) per project, in project order.
        let listed: Vec<(String, Vec<crate::worktree::model::Worktree>)> = {
            let projects = self.projects.lock().await;
            projects
                .iter()
                .map(|p| {
                    let list = match p.manager.list() {
                        Ok(l) => l,
                        Err(e) => {
                            tracing::warn!(
                                project = %p.name,
                                "daemon: worktree list failed: {e}"
                            );
                            Vec::new()
                        }
                    };
                    (p.name.clone(), list)
                })
                .collect()
        };

        let mut reg = self.registry.lock().await;
        let mut next: HashMap<PathBuf, ipc::WorktreeView> = HashMap::new();
        let mut project_of: HashMap<PathBuf, String> = HashMap::new();
        let mut order: Vec<PathBuf> = Vec::new();

        for (project_name, list) in &listed {
            for wt in list {
                let summary = reg.summaries.get(&wt.path).cloned();
                let mut view =
                    ipc::WorktreeView::from_worktree(wt, project_name.clone(), summary);
                // `from_worktree` defaults the ephemeral progress fields, which
                // would wipe a worktree mid-run.  Preserve them when the previous
                // view was Running so the spinner/activity/timer survive a rebuild.
                if let Some(prev) = reg.worktrees.get(&wt.path) {
                    if matches!(prev.status, WorktreeStatus::Running) {
                        view.activity = prev.activity.clone();
                        view.turns = prev.turns;
                        view.tokens = prev.tokens;
                        view.run_started_at = prev.run_started_at;
                    }
                }
                next.insert(wt.path.clone(), view);
                project_of.insert(wt.path.clone(), project_name.clone());
                order.push(wt.path.clone());
            }
        }

        reg.worktrees = next;
        reg.project_of = project_of;
        reg.order = order;
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

    // Boot guard: refuse to start (and refuse to steal the socket) if another
    // daemon is already alive.  We test the pidfile's PID with signal 0 — if it
    // is still running, a healthy daemon owns the socket, so we exit cleanly
    // WITHOUT touching the socket or pidfile.
    if let Some(pid) = ipc::read_pidfile(&pidfile) {
        if pid_is_alive(pid) && pid != std::process::id() as i32 {
            tracing::warn!(
                existing_pid = pid,
                "daemon: another supervisor is already running — exiting without binding"
            );
            // Flush tracing before exit.
            drop(_tracing_guard);
            std::process::exit(0);
        }
    }

    // Write the pidfile.
    if let Err(e) = std::fs::write(&pidfile, std::process::id().to_string()) {
        tracing::warn!("daemon: failed to write pidfile {}: {e}", pidfile.display());
    }

    // Remove a stale socket file (no live daemon owns it), then bind.
    if sock_path.exists() {
        let _ = std::fs::remove_file(&sock_path);
    }
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("failed to bind unix socket at {}", sock_path.display()))?;
    tracing::info!(socket = %sock_path.display(), "supervisor listening");

    // Construct Shared.
    let config = Config::load();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Auto-register the launch cwd if it is a git repo and not already present.
    if projects::is_git_repo(&cwd) {
        match projects::add(&cwd) {
            Ok(p) => tracing::info!(project = %p.name, "daemon: auto-registered cwd project"),
            Err(e) => tracing::warn!("daemon: failed to auto-register cwd: {e}"),
        }
    } else {
        tracing::info!("daemon: launch cwd is not a git repo — not auto-registering");
    }

    // Build ProjectRuntimes from the persisted registry.
    let registry_file = projects::load();
    let runtimes: Vec<ProjectRuntime> = registry_file
        .projects
        .iter()
        .map(ProjectRuntime::from_project)
        .collect();
    tracing::info!(count = runtimes.len(), "daemon: managing projects");

    let gh: Arc<dyn GhRunner> = Arc::new(RealGh {
        bin: config.gh_bin.clone(),
    });
    let (events, _initial_rx) = broadcast::channel::<SupervisorMsg>(BROADCAST_CAP);
    let watch_set = Arc::new(Mutex::new(Vec::<WatchItem>::new()));

    let shared = Arc::new(Shared {
        projects: Mutex::new(runtimes),
        registry: Mutex::new(Registry::empty()),
        gh,
        config,
        events,
        watch_set,
    });

    // Build the initial registry across all projects + seed the watch-set.
    shared.rebuild_registry().await;
    shared.rebuild_watch_set().await;

    // Spawn the watcher.  It emits AppEvents into `watch_event_tx`; the daemon
    // drains them in the select loop below (rehoming the app.rs handling).
    let (watch_event_tx, mut watch_event_rx) = mpsc::channel::<AppEvent>(64);
    let (watcher_shutdown_tx, watcher_shutdown_rx) = tokio::sync::watch::channel(false);
    let watcher_handle = spawn_watcher(
        Arc::clone(&shared.gh),
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

    // OS signal streams for clean stop (SIGTERM from `--stop-daemon` /
    // `kill <pid>`, SIGINT from Ctrl-C if attached to a terminal).
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("failed to install SIGTERM handler")?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("failed to install SIGINT handler")?;

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
                graceful_shutdown(
                    "client request",
                    &watcher_shutdown_tx,
                    &watcher_handle,
                    &sock_path,
                    &pidfile,
                    _tracing_guard,
                );
            }

            // SIGTERM → clean stop.
            _ = sigterm.recv() => {
                graceful_shutdown(
                    "SIGTERM",
                    &watcher_shutdown_tx,
                    &watcher_handle,
                    &sock_path,
                    &pidfile,
                    _tracing_guard,
                );
            }

            // SIGINT → clean stop.
            _ = sigint.recv() => {
                graceful_shutdown(
                    "SIGINT",
                    &watcher_shutdown_tx,
                    &watcher_handle,
                    &sock_path,
                    &pidfile,
                    _tracing_guard,
                );
            }
        }
    }
}

/// Test whether `pid` is alive by sending signal 0 (`kill(pid, None)`), which
/// performs permission/existence checks without delivering a signal.
fn pid_is_alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// Perform the daemon's graceful shutdown and terminate the process.
///
/// Single reusable routine for every shutdown trigger (client `Shutdown`,
/// SIGTERM, SIGINT): signal the watcher to stop, abort its task, remove the
/// socket + pidfile, flush tracing by consuming the guard, then `exit(0)`.
/// Diverges (`-> !`) so `select!` arms need no fallthrough.
fn graceful_shutdown(
    reason: &str,
    watcher_shutdown_tx: &tokio::sync::watch::Sender<bool>,
    watcher_handle: &tokio::task::JoinHandle<()>,
    sock_path: &Path,
    pidfile: &Path,
    tracing_guard: tracing_appender::non_blocking::WorkerGuard,
) -> ! {
    tracing::info!("daemon: shutdown requested ({reason}) — stopping");
    let _ = watcher_shutdown_tx.send(true);
    watcher_handle.abort();
    // Best-effort cleanup of socket + pidfile.
    let _ = std::fs::remove_file(sock_path);
    let _ = std::fs::remove_file(pidfile);
    // Flush tracing by dropping the guard, then exit.
    drop(tracing_guard);
    std::process::exit(0);
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
/// the resulting Snapshot.  This is the daemon-side handler for the watcher's
/// `PrStatusChanged` events.
///
/// The PR status is a SEPARATE axis from the agent-activity [`WorktreeStatus`]
/// (which the grid border still reflects), so this only touches `pr_status` /
/// `pr_number` and never clobbers `status`.  Auto-continue fires on the
/// `…→Merged` transition edge only (detected via the previous `pr_status`).
async fn handle_watch_event(shared: &Arc<Shared>, event: AppEvent) {
    match event {
        AppEvent::PrStatusChanged {
            worktree_path,
            pr_status,
            pr_number,
            pr_url,
            pr_title,
            unresolved_comments,
        } => {
            tracing::info!(
                worktree = %worktree_path.display(),
                ?pr_status,
                ?pr_number,
                ?unresolved_comments,
                "daemon: PR status changed"
            );
            let prev = shared
                .set_pr_status(
                    &worktree_path,
                    pr_status,
                    pr_number,
                    pr_url,
                    pr_title,
                    unresolved_comments,
                )
                .await;

            // Auto-continue ONLY on the transition edge into Merged (not every
            // tick while it stays Merged), and only when the flag is set.
            let merged_edge = pr_status == PrStatus::Merged && prev != Some(PrStatus::Merged);
            if merged_edge {
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
                        "daemon: PR merged + auto_continue_on_merge=true — starting continue session"
                    );
                    run_continue_session(
                        Arc::clone(shared),
                        worktree_path,
                        shared.config.auto_continue_prompt.clone(),
                    );
                }
            }
        }
        // The daemon never receives the other variants from the watcher; ignore.
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

    let projects = shared.project_infos().await;
    let snapshot = {
        let reg = shared.registry.lock().await;
        reg.snapshot()
    };
    ipc::write_frame_async(
        &mut write_half,
        &HandshakeResp::Ok {
            supervisor_pid: std::process::id(),
            projects,
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
            shared.broadcast_snapshot(snapshot).await;
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
        ClientMsg::NewWorktree {
            project,
            prompt_slug,
            prompt_body,
        } => {
            new_worktree(shared, project, prompt_slug, prompt_body).await;
        }
        ClientMsg::AddProject { path } => {
            add_project(shared, &path).await;
        }
        ClientMsg::SetWorktreeName {
            worktree_path,
            name,
        } => {
            set_worktree_name(shared, &worktree_path, name).await;
        }
        ClientMsg::RemoveWorktree { path, force } => {
            // Detach: worktree removal can be slow (git + `rm -rf` of e.g.
            // node_modules).  Running it inline would freeze this connection's
            // message loop for the whole teardown — and a client that quits
            // mid-delete would leave a wedged handler.  The spawned task owns its
            // own `Arc<Shared>` clone, broadcasts "Deleting…" immediately, and
            // emits the final snapshot when done — independent of this client.
            let shared = Arc::clone(shared);
            tokio::spawn(async move {
                remove_worktree(&shared, &path, force).await;
            });
        }
        ClientMsg::ResumeSession { worktree_path } => {
            run_continue_session(
                Arc::clone(shared),
                worktree_path,
                shared.config.resume_prompt.clone(),
            );
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
        .set_status(&worktree_path, WorktreeStatus::Running, None, None)
        .await;

    let Some(backend) = shared.backend_for(&worktree_path).await else {
        shared.error(
            Some(worktree_path.clone()),
            "no owning project for this worktree".to_string(),
        );
        return;
    };
    let task_shared = Arc::clone(&shared);
    let path = worktree_path.clone();

    tracing::info!(worktree = %worktree_path.display(), "daemon: running agent");

    tokio::spawn(async move {
        match backend.start(&path, &prompt).await {
            Ok(handle) => {
                // Resumes (on recoverable failure) use the continue prompt, not
                // the original instruction — the session already has the context.
                let continue_prompt = task_shared.config.resume_prompt.clone();
                drive_with_retries(&task_shared, &path, &continue_prompt, backend.as_ref(), handle)
                    .await
            }
            Err(e) => {
                tracing::error!("daemon: failed to start agent: {e}");
                task_shared
                    .set_status(
                        &path,
                        agent_status_to_worktree_status(&AgentStatus::Error(format!("{e}"))),
                        None,
                        None,
                    )
                    .await;
            }
        }
    });
}

/// Continue the most recent session (auto-continue on merge) — mirrors
/// `App::run_agent_continue`.
fn run_continue_session(shared: Arc<Shared>, worktree_path: PathBuf, prompt: String) {
    let path = worktree_path.clone();

    tokio::spawn(async move {
        let Some(backend) = shared.backend_for(&path).await else {
            tracing::warn!(
                worktree = %path.display(),
                "daemon: no owning project backend for continue session"
            );
            return;
        };
        // Mark Running + clear summary.
        {
            let mut reg = shared.registry.lock().await;
            reg.summaries.remove(&path);
        }
        shared
            .set_status(&path, WorktreeStatus::Running, None, None)
            .await;

        // Resume the exact prior session when we have its id (deterministic);
        // otherwise fall back to bare `-c` (most recent in the dir).
        let session_id = shared.session_id_for(&path).await;
        match backend
            .continue_session(&path, session_id.as_deref(), &prompt)
            .await
        {
            Ok(handle) => {
                drive_with_retries(&shared, &path, &prompt, backend.as_ref(), handle).await
            }
            Err(e) => {
                tracing::error!("daemon: failed to start auto-continue session: {e}");
                shared
                    .set_status(
                        &path,
                        agent_status_to_worktree_status(&AgentStatus::Error(format!("{e}"))),
                        None,
                        None,
                    )
                    .await;
            }
        }
    });
}

/// Drive a [`SessionHandle`] to completion.
///
/// Broadcasts every LIVE (`Running`/progress) update via `set_status` so the
/// spinner/activity stay current, but does NOT broadcast the TERMINAL
/// (`Done`/`Error`) status — it returns it instead, so the caller can decide
/// whether to retry (transient failure) or commit the final status.  Also
/// persists the `session_id` once, the first time the stream reports it.
async fn drive_session(
    shared: &Arc<Shared>,
    handle: crate::agent::SessionHandle,
) -> (AgentStatus, Option<String>) {
    let (status_tx, mut status_rx) = mpsc::channel::<StatusUpdate>(16);

    let runner = tokio::spawn(async move { run_session(handle, status_tx).await });

    let mut persisted_sid: Option<String> = None;
    let mut terminal: Option<(AgentStatus, Option<String>)> = None;

    while let Some(update) = status_rx.recv().await {
        if let Some(sid) = &update.session_id {
            if persisted_sid.as_deref() != Some(sid.as_str()) {
                shared.persist_session_id(&update.worktree_path, sid).await;
                persisted_sid = Some(sid.clone());
            }
        }
        tracing::info!(
            worktree = %update.worktree_path.display(),
            "daemon: agent status: {:?}",
            update.status
        );
        match update.status {
            // Terminal: hold it; the caller commits or retries.  Do not broadcast.
            AgentStatus::Done | AgentStatus::Error(_) => {
                terminal = Some((update.status.clone(), update.summary.clone()));
            }
            // Live: broadcast progress so the UI stays alive.
            _ => {
                let progress = Progress {
                    activity: update.activity.clone(),
                    turns: update.turns,
                    tokens: update.tokens,
                };
                shared
                    .set_status(
                        &update.worktree_path,
                        agent_status_to_worktree_status(&update.status),
                        update.summary.clone(),
                        Some(progress),
                    )
                    .await;
            }
        }
    }

    if let Some(t) = terminal {
        return t;
    }
    // No terminal update arrived (stream closed early) — derive from the runner.
    match runner.await {
        Ok(Err(e)) => {
            tracing::error!("daemon: session runner failed: {e}");
            (AgentStatus::Error(format!("{e}")), None)
        }
        _ => (AgentStatus::Done, None),
    }
}

/// Max automatic retries for a session that ends in a TRANSIENT error.
const MAX_AGENT_RETRIES: u32 = 2;

/// Backoff before retry `attempt` (1-based).
fn retry_backoff(attempt: u32) -> std::time::Duration {
    let secs = match attempt {
        1 => 3,
        _ => 10,
    };
    std::time::Duration::from_secs(secs)
}

/// Heuristic: is this agent error worth an automatic retry?
///
/// Retries TRANSIENT infrastructure failures (rate limits, 5xx, connection /
/// network blips, dropped streams).  Never retries PERMANENT failures — auth
/// problems and context-limit errors, where a retry cannot help (context-limit
/// is handled separately by resuming, which compacts).
fn is_transient_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    const PERMANENT: &[&str] = &[
        "context",
        "too long",
        "maximum context",
        "401",
        "403",
        "unauthorized",
        "invalid api key",
        "authentication",
        "permission",
    ];
    if PERMANENT.iter().any(|p| m.contains(p)) {
        return false;
    }
    const TRANSIENT: &[&str] = &[
        "429",
        "rate limit",
        "rate_limit",
        "overloaded",
        "529",
        "502",
        "503",
        "504",
        "internal server error",
        "bad gateway",
        "service unavailable",
        "gateway timeout",
        "timeout",
        "timed out",
        "connection reset",
        "connection refused",
        "connection error",
        "econnreset",
        "broken pipe",
        "network",
        "temporarily unavailable",
        "fetch failed",
        "stream disconnected",
        "unexpected eof",
    ];
    TRANSIENT.iter().any(|t| m.contains(t))
}

/// Max automatic resumes after a CONTEXT-LIMIT error (claude compacts on resume).
const MAX_CONTEXT_RETRIES: u32 = 2;

/// Why a finished session is being auto-resumed.
enum RetryKind {
    /// Transient infra failure (rate limit / 5xx / network) — back off + resume.
    Transient,
    /// Ran out of context window — resume so the agent compacts and continues.
    ContextLimit,
}

/// Heuristic: did the agent fail because it ran out of context window?
///
/// Resuming compacts the session, so these are recoverable — distinct from the
/// transient infra errors handled by [`is_transient_error`] (which excludes
/// context errors) and from genuinely permanent failures.
fn is_context_limit_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("prompt is too long")
        || m.contains("maximum context")
        || m.contains("context_length_exceeded")
        || m.contains("context window")
        || (m.contains("context")
            && (m.contains("too long") || m.contains("exceed") || m.contains("limit")))
}

/// Drive a session through `drive_session`, auto-resuming on RECOVERABLE failures.
///
/// The initial `handle` was created by the caller (a fresh `start` for
/// `run_prompt`, or a `--resume` for `run_continue_session`).  When a run ends in
/// a recoverable error it is resumed via `continue_session` (with `continue_prompt`)
/// so context is preserved:
/// - TRANSIENT infra errors (rate limit / 5xx / network) → back off, up to
///   [`MAX_AGENT_RETRIES`] times.
/// - CONTEXT-LIMIT errors → resume immediately (claude compacts on resume), up to
///   [`MAX_CONTEXT_RETRIES`] times.
///
/// Any other terminal status (success, permanent error, or budget exhausted) is
/// committed via `set_status`.
async fn drive_with_retries(
    shared: &Arc<Shared>,
    path: &Path,
    continue_prompt: &str,
    backend: &dyn AgentBackend,
    mut handle: crate::agent::SessionHandle,
) {
    let mut transient_attempt: u32 = 0;
    let mut context_attempt: u32 = 0;

    loop {
        let (terminal, summary) = drive_session(shared, handle).await;

        let kind = if let AgentStatus::Error(msg) = &terminal {
            if transient_attempt < MAX_AGENT_RETRIES && is_transient_error(msg) {
                Some((RetryKind::Transient, msg.clone()))
            } else if context_attempt < MAX_CONTEXT_RETRIES && is_context_limit_error(msg) {
                Some((RetryKind::ContextLimit, msg.clone()))
            } else {
                None
            }
        } else {
            None
        };

        let Some((kind, err)) = kind else {
            // Commit the terminal status (success, non-recoverable, or exhausted).
            shared
                .set_status(
                    path,
                    agent_status_to_worktree_status(&terminal),
                    summary,
                    None,
                )
                .await;
            return;
        };

        let (delay, status_msg) = match kind {
            RetryKind::Transient => {
                transient_attempt += 1;
                (
                    retry_backoff(transient_attempt),
                    format!("transient error — retrying ({transient_attempt}/{MAX_AGENT_RETRIES})…"),
                )
            }
            RetryKind::ContextLimit => {
                context_attempt += 1;
                (
                    std::time::Duration::from_secs(1),
                    format!(
                        "context limit — compacting & continuing ({context_attempt}/{MAX_CONTEXT_RETRIES})…"
                    ),
                )
            }
        };

        tracing::warn!(
            worktree = %path.display(),
            "daemon: recoverable agent error, resuming in {}s: {err}",
            delay.as_secs()
        );
        shared
            .set_status(path, WorktreeStatus::Running, Some(status_msg), None)
            .await;
        tokio::time::sleep(delay).await;

        // Resume the (now-captured) session so the retry keeps full context.
        let session_id = shared.session_id_for(path).await;
        match backend
            .continue_session(path, session_id.as_deref(), continue_prompt)
            .await
        {
            Ok(h) => handle = h,
            Err(e) => {
                shared
                    .set_status(
                        path,
                        agent_status_to_worktree_status(&AgentStatus::Error(format!("{e}"))),
                        None,
                        None,
                    )
                    .await;
                return;
            }
        }
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
                shared.error(Some(worktree_path), format!("gh command error: {e}"));
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
    if let Some(root) = shared.project_root_for(path).await {
        match state::load(&root) {
            Ok(mut st) => {
                st.set_auto_continue(path, enabled);
                if let Err(e) = state::save(&root, &st) {
                    tracing::warn!("daemon: failed to persist auto_continue: {e}");
                }
            }
            Err(e) => tracing::warn!("daemon: failed to load state for auto_continue: {e}"),
        }
    } else {
        tracing::warn!(
            "daemon: no owning project for {} (auto_continue)",
            path.display()
        );
    }
    let snapshot = {
        let reg = shared.registry.lock().await;
        reg.snapshot()
    };
    shared.broadcast_snapshot(snapshot).await;
}

/// `SetPrNumber` — update registry + persist + refresh watch-set + Snapshot.
async fn set_pr_number(shared: &Arc<Shared>, path: &Path, pr: Option<u64>) {
    {
        let mut reg = shared.registry.lock().await;
        if let Some(view) = reg.worktrees.get_mut(path) {
            view.pr_number = pr;
        }
    }
    if let Some(root) = shared.project_root_for(path).await {
        match state::load(&root) {
            Ok(mut st) => {
                st.set_pr_number(path, pr);
                if let Err(e) = state::save(&root, &st) {
                    tracing::warn!("daemon: failed to persist pr_number: {e}");
                }
            }
            Err(e) => tracing::warn!("daemon: failed to load state for pr_number: {e}"),
        }
    } else {
        tracing::warn!(
            "daemon: no owning project for {} (pr_number)",
            path.display()
        );
    }
    shared.rebuild_watch_set().await;
    let snapshot = {
        let reg = shared.registry.lock().await;
        reg.snapshot()
    };
    shared.broadcast_snapshot(snapshot).await;
}

/// `NewWorktree` — generate a fresh UUID directory under the configured base,
/// namespaced as `<base>/<owner>/<project>/<uuid>`, create a detached worktree
/// there, record the prompt slug + name, refresh the registry, and broadcast.
/// When `prompt_body` is `Some`, additionally drive the agent on the new
/// worktree (same path as `RunPrompt`).
async fn new_worktree(
    shared: &Arc<Shared>,
    project: String,
    prompt_slug: Option<String>,
    prompt_body: Option<String>,
) {
    // Resolve the target project's base dir + worktree settings up-front (clone
    // what we need so we don't hold the projects lock across git/state I/O).
    let resolved: Option<(PathBuf, PathBuf, WorktreeSettings)> = {
        let projects = shared.projects.lock().await;
        projects.iter().find(|p| p.name == project).map(|p| {
            (
                p.root.clone(),
                p.project_config.worktrees_base(&p.root),
                p.project_config.worktree.clone(),
            )
        })
    };
    let Some((root, base, project_worktree)) = resolved else {
        shared.error(None, format!("unknown project: {project}"));
        return;
    };

    // Resolve the effective setup command + timeout (project over global over
    // built-in default).
    let (setup_command, setup_timeout) = resolve_setup(&project_worktree, &shared.config.worktree);

    // Build `<base>/<owner>/<project>/<uuid>` — consistent whether the base
    // comes from the XDG default or an explicit `worktrees_dir` override.
    let owner = projects::git_owner(&root);
    let uuid = uuid::Uuid::new_v4().to_string();
    let path = base.join(&owner).join(&project).join(&uuid);
    let parent = path.parent().expect("path always has a parent");

    if let Err(e) = std::fs::create_dir_all(parent) {
        shared.error(
            None,
            format!("cannot create worktrees dir {}: {e}", parent.display()),
        );
        return;
    }

    // Create the detached worktree using a fresh manager bound to this project's
    // root (manager is cheap; avoids holding the projects lock across git I/O).
    let manager = WorktreeManager::new(root.clone());
    let created = match manager.create_detached(&path) {
        Ok(wt) => wt,
        Err(e) => {
            shared.error(None, format!("create detached worktree failed: {e}"));
            return;
        }
    };
    let canonical = created.path.clone();

    // Record the prompt slug onto the project's persisted state (name stays
    // "Unnamed").
    if prompt_slug.is_some() {
        match state::load(&root) {
            Ok(mut st) => {
                if let Some(w) = st.worktrees.iter_mut().find(|w| w.path == canonical) {
                    w.prompt_slug = prompt_slug.clone();
                }
                if let Err(e) = state::save(&root, &st) {
                    tracing::warn!("daemon: failed to persist new worktree prompt_slug: {e}");
                }
            }
            Err(e) => tracing::warn!("daemon: failed to load state for new worktree: {e}"),
        }
    }

    let snapshot = shared.rebuild_registry().await;
    shared.rebuild_watch_set().await;
    shared.broadcast_snapshot(snapshot).await;

    // Setup-then-prompt sequence, in a spawned task so the daemon isn't blocked.
    //
    // 1. If a setup command is configured, run it non-interactively to
    //    completion or until the timeout (status → Running "running setup: …").
    //    On failure/timeout: surface an error but STILL proceed.
    // 2. Then, if a prompt was supplied, drive the agent on it; otherwise set
    //    the worktree back to Idle.
    let prompt_body = prompt_body.filter(|b| !b.trim().is_empty());
    let task_shared = Arc::clone(shared);
    tokio::spawn(async move {
        if let Some(command) = setup_command {
            task_shared
                .set_status(
                    &canonical,
                    WorktreeStatus::Running,
                    Some(format!("running setup: {}", truncate_command(&command))),
                    None,
                )
                .await;
            tracing::info!(
                worktree = %canonical.display(),
                "daemon: running worktree setup ({}s timeout)",
                setup_timeout.as_secs()
            );
            match run_setup(&command, &canonical, setup_timeout).await {
                Ok(()) => {
                    tracing::info!(
                        worktree = %canonical.display(),
                        "daemon: worktree setup succeeded"
                    );
                }
                Err(e) => {
                    // Log + surface, but continue to the prompt regardless.
                    task_shared.error(Some(canonical.clone()), format!("setup failed: {e}"));
                }
            }
        }

        match prompt_body {
            Some(body) => run_prompt(task_shared, canonical, body).await,
            None => {
                // No prompt → leave the worktree Idle once setup is done.
                task_shared
                    .set_status(&canonical, WorktreeStatus::Idle, None, None)
                    .await;
            }
        }
    });
}

/// `AddProject` — validate + register the git repo at `path`, build a
/// `ProjectRuntime`, push it into `Shared.projects`, rebuild the registry +
/// watch-set, and broadcast a Snapshot.  On failure broadcast an Error.
async fn add_project(shared: &Arc<Shared>, path: &Path) {
    match projects::add(path) {
        Ok(project) => {
            // Skip if this project is already managed (dedupe by canonical path).
            let already = {
                let runtimes = shared.projects.lock().await;
                runtimes.iter().any(|p| p.root == project.path)
            };
            if !already {
                let runtime = ProjectRuntime::from_project(&project);
                shared.projects.lock().await.push(runtime);
            }
            tracing::info!(project = %project.name, "daemon: added project");
            let snapshot = shared.rebuild_registry().await;
            shared.rebuild_watch_set().await;
            shared.broadcast_snapshot(snapshot).await;
        }
        Err(e) => {
            shared.error(None, format!("add project failed: {e}"));
        }
    }
}

/// `SetWorktreeName` — update the supervisor name dictionary (registry + state),
/// then broadcast a fresh Snapshot.
async fn set_worktree_name(shared: &Arc<Shared>, path: &Path, name: String) {
    {
        let mut reg = shared.registry.lock().await;
        if let Some(view) = reg.worktrees.get_mut(path) {
            view.name = name.clone();
        }
    }
    if let Some(root) = shared.project_root_for(path).await {
        match state::load(&root) {
            Ok(mut st) => {
                st.set_name(path, name);
                if let Err(e) = state::save(&root, &st) {
                    tracing::warn!("daemon: failed to persist worktree name: {e}");
                }
            }
            Err(e) => tracing::warn!("daemon: failed to load state for name update: {e}"),
        }
    } else {
        tracing::warn!("daemon: no owning project for {} (name)", path.display());
    }
    let snapshot = {
        let reg = shared.registry.lock().await;
        reg.snapshot()
    };
    shared.broadcast_snapshot(snapshot).await;
}

/// `RemoveWorktree` — remove via the manager (always force), drop state + registry,
/// rebuild watch-set, and broadcast a fresh Snapshot.
async fn remove_worktree(shared: &Arc<Shared>, path: &Path, _force: bool) {
    let Some(root) = shared.project_root_for(path).await else {
        shared.error(
            Some(path.to_path_buf()),
            "no owning project for this worktree".to_string(),
        );
        return;
    };

    // Flip the card to "Deleting…" before the (potentially slow) git + fs
    // removal so the user sees the teardown is underway.  Broadcast happens
    // before the blocking remove call below.
    shared
        .set_status(
            path,
            WorktreeStatus::Deleting,
            Some("removing worktree".to_string()),
            None,
        )
        .await;

    // The manager handles git + fs removal (always force per the spec).  This is
    // blocking I/O (git subprocess + recursive directory delete), so run it on
    // the blocking pool — never inline on an async worker — so it cannot stall
    // the runtime (handshakes, the watcher, other agents).
    let manager = WorktreeManager::new(root.clone());
    let rm_path = path.to_path_buf();
    let removal = tokio::task::spawn_blocking(move || manager.remove(&rm_path, true)).await;
    let remove_result = match removal {
        Ok(r) => r,
        Err(join_err) => Err(anyhow::anyhow!("removal task failed: {join_err}")),
    };
    match remove_result {
        Ok(()) => {
            tracing::info!("daemon: worktree removed from disk: {}", path.display());
        }
        Err(e) => {
            // Removal failed — don't leave the card stuck on "Deleting…".
            shared
                .set_status(
                    path,
                    WorktreeStatus::Error,
                    Some(format!("remove failed: {e}")),
                    None,
                )
                .await;
            shared.error(
                Some(path.to_path_buf()),
                format!("remove worktree failed: {e}"),
            );
            return;
        }
    }

    // Drop the entry from the owning project's state.toml.  Use the canonical
    // path if possible; fall back to the raw path so stale entries are pruned.
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    match state::load(&root) {
        Ok(mut st) => {
            st.remove_worktree(&canonical);
            // Also try the raw path in case canonicalization diverged.
            st.remove_worktree(path);
            if let Err(e) = state::save(&root, &st) {
                tracing::warn!("daemon: failed to persist worktree removal: {e}");
            }
        }
        Err(e) => tracing::warn!("daemon: failed to load state for removal: {e}"),
    }

    // Drop from in-memory registry maps so the snapshot is clean even before
    // rebuild_registry() re-scans git.
    {
        let mut reg = shared.registry.lock().await;
        reg.worktrees.remove(path);
        reg.worktrees.remove(&canonical);
        reg.project_of.remove(path);
        reg.project_of.remove(&canonical);
        reg.order.retain(|p| p != path && p != &canonical);
        reg.summaries.remove(path);
        reg.summaries.remove(&canonical);
    }

    let snapshot = shared.rebuild_registry().await;
    shared.rebuild_watch_set().await;
    shared.broadcast_snapshot(snapshot).await;
}

// ---------------------------------------------------------------------------
// Worktree setup: precedence resolver + non-interactive timed runner
// ---------------------------------------------------------------------------

/// Built-in default timeout (seconds) for the worktree setup command.
const DEFAULT_SETUP_TIMEOUT_SECS: u64 = 300;

/// Maximum number of stderr lines to retain from a setup command for error
/// reporting (mirrors the agent-session bound).
const SETUP_STDERR_MAX_LINES: usize = 100;
/// Maximum total bytes of setup stderr to retain (older lines dropped first).
const SETUP_STDERR_MAX_BYTES: usize = 8 * 1024;

/// Resolve the effective worktree-setup config for one project, applying
/// project-over-global-over-default precedence:
///
/// - `setup_command`  = `project.setup_command.or(global.setup_command)`
/// - `setup_timeout`  = `project.setup_timeout_seconds.or(global.setup_timeout_seconds)
///   .unwrap_or(300)` seconds.
fn resolve_setup(
    project: &WorktreeSettings,
    global: &WorktreeSettings,
) -> (Option<String>, std::time::Duration) {
    let command = project
        .setup_command
        .clone()
        .or_else(|| global.setup_command.clone());
    let timeout_secs = project
        .setup_timeout_seconds
        .or(global.setup_timeout_seconds)
        .unwrap_or(DEFAULT_SETUP_TIMEOUT_SECS);
    (command, std::time::Duration::from_secs(timeout_secs))
}

/// Failure from [`run_setup`]: carries a human-readable message (a stderr tail
/// for a non-zero exit, or a timeout notice).
#[derive(Debug)]
struct SetupError {
    message: String,
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Run a worktree setup command NON-INTERACTIVELY to completion or until
/// `timeout`.
///
/// The command is launched as `sh -c "<command>"` with `cwd` as the working
/// directory.  STDIN is closed (`/dev/null`) so an interactive/TUI command can
/// never block waiting on input.  stdout + stderr are PIPED and drained
/// concurrently (logged; a bounded stderr tail is retained, mirroring
/// [`crate::agent::session`]).  `kill_on_drop(true)` plus an explicit kill on
/// timeout guarantee no orphaned child survives.
///
/// stdout/stderr are NEVER inherited to the terminal.
///
/// - non-zero exit → `Err` carrying the captured stderr tail.
/// - timeout       → child is killed; `Err("timed out after {secs}s")`.
/// - success       → `Ok(())`.
async fn run_setup(
    command: &str,
    cwd: &Path,
    timeout: std::time::Duration,
) -> std::result::Result<(), SetupError> {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut child = match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return Err(SetupError {
                message: format!("failed to spawn setup command: {e}"),
            });
        }
    };

    let cwd_owned = cwd.to_path_buf();

    // Drain stdout concurrently (logged at debug; not retained).
    let stdout_task = child.stdout.take().map(|stdout| {
        let cwd = cwd_owned.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::debug!(worktree = %cwd.display(), "setup stdout: {line}");
            }
        })
    });

    // Drain stderr concurrently into a bounded tail.
    let stderr_task = child.stderr.take().map(|stderr| {
        let cwd = cwd_owned.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            let mut lines: std::collections::VecDeque<String> = std::collections::VecDeque::new();
            let mut total_bytes: usize = 0;
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::debug!(worktree = %cwd.display(), "setup stderr: {line}");
                total_bytes += line.len() + 1;
                lines.push_back(line);
                while lines.len() > SETUP_STDERR_MAX_LINES || total_bytes > SETUP_STDERR_MAX_BYTES {
                    if let Some(evicted) = lines.pop_front() {
                        total_bytes = total_bytes.saturating_sub(evicted.len() + 1);
                    } else {
                        break;
                    }
                }
            }
            lines.into_iter().collect::<Vec<_>>().join("\n")
        })
    });

    // Wait for exit, enforcing the timeout.
    match tokio::time::timeout(timeout, child.wait()).await {
        Err(_elapsed) => {
            // Timed out: kill the child (kill_on_drop also covers this) and fail.
            let _ = child.start_kill();
            let _ = child.wait().await;
            if let Some(t) = stdout_task {
                t.abort();
            }
            if let Some(t) = stderr_task {
                t.abort();
            }
            Err(SetupError {
                message: format!("timed out after {}s", timeout.as_secs()),
            })
        }
        Ok(wait_result) => {
            if let Some(t) = stdout_task {
                let _ = t.await;
            }
            let stderr_tail = if let Some(t) = stderr_task {
                t.await.unwrap_or_default()
            } else {
                String::new()
            };
            match wait_result {
                Ok(status) if status.success() => Ok(()),
                Ok(status) => {
                    let base = format!("setup command exited with status {status}");
                    Err(SetupError {
                        message: compose_setup_error(base, &stderr_tail),
                    })
                }
                Err(e) => Err(SetupError {
                    message: format!("failed to await setup command: {e}"),
                }),
            }
        }
    }
}

/// Compose a setup error from a base reason + optional stderr tail.
fn compose_setup_error(base: String, stderr_tail: &str) -> String {
    let tail = stderr_tail.trim();
    if tail.is_empty() {
        base
    } else {
        format!("{base}: {tail}")
    }
}

/// Truncate a command for a status summary line (keeps the UI legible).
fn truncate_command(command: &str) -> String {
    const MAX: usize = 80;
    let one_line = command.replace('\n', " ");
    if one_line.chars().count() > MAX {
        let t: String = one_line.chars().take(MAX).collect();
        format!("{t}…")
    } else {
        one_line
    }
}

// ---------------------------------------------------------------------------
// Backend selection
// ---------------------------------------------------------------------------

/// Choose the active agent backend at startup.
///
/// Uses the project config's `agent.command`.  If that command is runnable
/// (found on PATH via `--version` probe), returns a [`ConfiguredBackend`];
/// otherwise falls back to [`MockBackend`] with a warning.
fn select_backend(project_cfg: ProjectConfig) -> (Arc<dyn AgentBackend>, &'static str) {
    let command = &project_cfg.agent.command;
    if command_on_path(command) {
        tracing::info!("daemon: agent backend ConfiguredBackend ({command} found on PATH)");
        (
            Arc::new(ConfiguredBackend {
                agent: project_cfg.agent,
            }),
            "Configured",
        )
    } else {
        tracing::warn!("daemon: agent backend MockBackend ({command} not found on PATH)");
        (Arc::new(MockBackend::new()), "Mock")
    }
}

/// Best-effort check: run `<command> --version` and see if it exits cleanly.
fn command_on_path(command: &str) -> bool {
    use std::process::Command;
    Command::new(command)
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

    /// Build a `ProjectRuntime` over a temp repo with a fast MockBackend.
    fn make_runtime(name: &str, root: PathBuf) -> ProjectRuntime {
        ProjectRuntime {
            name: name.to_string(),
            root: root.clone(),
            manager: WorktreeManager::new(root),
            project_config: ProjectConfig::default(),
            backend: Arc::new(MockBackend {
                delay: Duration::from_millis(5),
            }),
            backend_name: "Mock",
        }
    }

    /// Build a `Shared` over one or more temp-repo projects with a MockGh.
    /// The registry is built across all supplied projects.
    async fn make_shared_with(projects: Vec<ProjectRuntime>, gh: Arc<dyn GhRunner>) -> Arc<Shared> {
        let (events, _rx) = broadcast::channel::<SupervisorMsg>(BROADCAST_CAP);
        let shared = Arc::new(Shared {
            projects: Mutex::new(projects),
            registry: Mutex::new(Registry::empty()),
            gh,
            config: Config::default(),
            events,
            watch_set: Arc::new(Mutex::new(Vec::new())),
        });
        shared.rebuild_registry().await;
        shared
    }

    /// Convenience: single-project `Shared` named "proj" over `root`.
    async fn make_shared(root: PathBuf, gh: Arc<dyn GhRunner>) -> Arc<Shared> {
        make_shared_with(vec![make_runtime("proj", root)], gh).await
    }

    /// The root of the first managed project (used by seed-based tests).
    async fn primary_root(shared: &Arc<Shared>) -> PathBuf {
        shared.projects.lock().await[0].root.clone()
    }

    /// The name of the first managed project.
    async fn primary_name(shared: &Arc<Shared>) -> String {
        shared.projects.lock().await[0].name.clone()
    }

    /// Seed a single worktree directly into the registry + the owning project's
    /// state for tests that don't want to spin a real `git worktree add`.  The
    /// worktree is tagged with the first project and registered in `project_of`
    /// so state-write resolution finds the right repo.
    async fn seed_worktree(
        shared: &Arc<Shared>,
        path: &Path,
        auto_continue: bool,
        pr: Option<u64>,
    ) {
        let project_name = primary_name(shared).await;
        let root = primary_root(shared).await;
        let now = chrono::Utc::now();
        let wt = crate::worktree::model::Worktree {
            path: path.to_path_buf(),
            name: "Unnamed".to_string(),
            branch: "feat".to_string(),
            prompt_slug: None,
            pr_number: pr,
            pr_url: None,
            pr_title: None,
            auto_continue_on_merge: auto_continue,
            status: WorktreeStatus::Idle,
            pr_status: crate::worktree::model::PrStatus::NoPr,
            unresolved_comments: None,
            created_at: now,
            updated_at: now,
            session_id: None,
        };
        {
            let mut reg = shared.registry.lock().await;
            reg.worktrees.insert(
                path.to_path_buf(),
                ipc::WorktreeView::from_worktree(&wt, project_name.clone(), None),
            );
            reg.project_of
                .insert(path.to_path_buf(), project_name.clone());
            if !reg.order.iter().any(|p| p == path) {
                reg.order.push(path.to_path_buf());
            }
        }
        let mut st = state::load(&root).expect("load state");
        st.upsert_worktree(wt);
        state::save(&root, &st).expect("save state");
    }

    // -- RunPrompt ------------------------------------------------------------

    #[tokio::test]
    async fn run_prompt_sets_running_then_needs_review() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root, gh).await;

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

    #[test]
    fn transient_error_classifier() {
        // Transient infra failures → retry.
        for m in [
            "API Error: 429 rate_limit_exceeded",
            "Overloaded",
            "API Error: 529",
            "503 Service Unavailable",
            "fetch failed: ECONNRESET",
            "connection reset by peer",
            "stream disconnected unexpectedly",
            "request timed out",
        ] {
            assert!(is_transient_error(m), "expected transient: {m}");
        }
        // Permanent failures → never retry.
        for m in [
            "prompt is too long: exceeds maximum context window",
            "401 Unauthorized",
            "invalid api key",
            "permission denied",
            "the agent did the wrong thing",
        ] {
            assert!(!is_transient_error(m), "expected NOT transient: {m}");
        }
    }

    #[test]
    fn context_limit_classifier() {
        for m in [
            "prompt is too long: 250000 tokens > 200000 maximum",
            "maximum context length exceeded",
            "context_length_exceeded",
            "the context window is full",
            "context limit reached",
        ] {
            assert!(is_context_limit_error(m), "expected context-limit: {m}");
            // Context-limit must NOT also be treated as a transient infra error
            // (they take different recovery paths).
            assert!(!is_transient_error(m), "context-limit must not be transient: {m}");
        }
        // Non-context errors.
        for m in ["429 rate limit", "the agent failed the task"] {
            assert!(!is_context_limit_error(m), "not context-limit: {m}");
        }
    }

    #[tokio::test]
    async fn resume_session_drives_running_then_needs_review() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root, gh).await;

        let path = PathBuf::from("/tmp/resume-wt");
        seed_worktree(&shared, &path, false, None).await;

        let mut sub = shared.events.subscribe();

        handle_client_msg(
            &shared,
            ClientMsg::ResumeSession {
                worktree_path: path.clone(),
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

        // Eventually reaches NeedsReview (mock continue_session → Done).
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
        assert!(reached_review, "expected NeedsReview after resumed session");
    }

    // -- SetAutoContinue ------------------------------------------------------

    #[tokio::test]
    async fn set_auto_continue_persists_and_reflects() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root.clone(), gh).await;

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

    // -- watch-set includes every worktree (discovery is by branch now) ------

    #[tokio::test]
    async fn watch_set_includes_every_worktree_by_path() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root, gh).await;

        let path = PathBuf::from("/tmp/pr-wt");
        // Seed a worktree with NO pr_number — it must still be watched (the
        // watcher resolves the PR from the branch each tick).
        seed_worktree(&shared, &path, false, None).await;

        shared.rebuild_watch_set().await;
        let ws = shared.watch_set.lock().await;
        // The seeded worktree is watched even though it has no pr_number.
        assert!(
            ws.iter().any(|item| item.worktree_path == path),
            "seeded worktree must be in the watch-set regardless of pr_number"
        );
    }

    // -- Refresh rebuilds from a real temp repo ------------------------------

    #[tokio::test]
    async fn refresh_rebuilds_registry_from_repo() {
        let (_dir, root) = make_temp_repo();
        let wt_dir = tempfile::tempdir().expect("wt tempdir");
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root.clone(), gh).await;

        // Create a real worktree on disk via a manager bound to the project root.
        WorktreeManager::new(root.clone())
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

    // -- NewWorktree ----------------------------------------------------------

    #[tokio::test]
    async fn new_worktree_blank_creates_detached_unnamed_under_base() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root.clone(), gh).await;

        handle_client_msg(
            &shared,
            ClientMsg::NewWorktree {
                project: "proj".to_string(),
                prompt_slug: None,
                prompt_body: None,
            },
        )
        .await;

        let reg = shared.registry.lock().await;
        // Filter for the newly-created detached worktree (path contains /local/proj/).
        let created: Vec<_> = reg
            .worktrees
            .values()
            .filter(|v| v.path.to_string_lossy().contains("/local/proj/"))
            .collect();
        assert_eq!(
            created.len(),
            1,
            "exactly one new detached worktree under /local/proj/"
        );
        let wt = created[0];
        assert_eq!(wt.name, "Unnamed");
        assert_eq!(wt.branch, "HEAD");
        assert!(wt.prompt_slug.is_none());

        // The leaf directory name is a parseable UUID v4.
        let dir_name = wt.path.file_name().unwrap().to_string_lossy();
        assert!(
            uuid::Uuid::parse_str(&dir_name).is_ok(),
            "leaf dir name should be a uuid, got {dir_name}"
        );
    }

    #[tokio::test]
    async fn new_worktree_with_prompt_drives_agent_to_running() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root.clone(), gh).await;

        let mut sub = shared.events.subscribe();

        handle_client_msg(
            &shared,
            ClientMsg::NewWorktree {
                project: "proj".to_string(),
                prompt_slug: Some("refactor".to_string()),
                prompt_body: Some("do the refactor".to_string()),
            },
        )
        .await;

        // Expect: the Snapshot after creation, then a Running StatusChanged.
        let mut saw_running = false;
        for _ in 0..6 {
            match tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
                Ok(Ok(SupervisorMsg::StatusChanged {
                    status: WorktreeStatus::Running,
                    ..
                })) => {
                    saw_running = true;
                    break;
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        assert!(saw_running, "expected Running status from the new worktree");

        // The prompt slug is recorded on the new worktree.
        let reg = shared.registry.lock().await;
        let slug = reg.worktrees.values().find_map(|v| v.prompt_slug.clone());
        assert_eq!(slug.as_deref(), Some("refactor"));
    }

    // -- SetWorktreeName ------------------------------------------------------

    #[tokio::test]
    async fn set_worktree_name_updates_registry_and_state() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root.clone(), gh).await;

        let path = PathBuf::from("/tmp/name-wt");
        seed_worktree(&shared, &path, false, None).await;

        handle_client_msg(
            &shared,
            ClientMsg::SetWorktreeName {
                worktree_path: path.clone(),
                name: "renamed".to_string(),
            },
        )
        .await;

        {
            let reg = shared.registry.lock().await;
            assert_eq!(reg.worktrees.get(&path).unwrap().name, "renamed");
        }
        let st = state::load(&root).expect("load");
        assert_eq!(
            st.worktrees.iter().find(|w| w.path == path).unwrap().name,
            "renamed"
        );
    }

    // -- RemoveWorktree -------------------------------------------------------

    #[tokio::test]
    async fn remove_worktree_disappears_from_registry_and_git() {
        let (_dir, root) = make_temp_repo();
        let wt_dir = tempfile::tempdir().expect("wt tempdir");
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root.clone(), gh).await;

        // Create a real worktree on disk.
        let manager = WorktreeManager::new(root.clone());
        manager
            .create_detached(wt_dir.path())
            .expect("create_detached");
        let canonical = wt_dir.path().canonicalize().expect("canonicalize");

        // Rebuild registry so the daemon knows about it.
        shared.rebuild_registry().await;
        {
            let reg = shared.registry.lock().await;
            assert!(
                reg.worktrees.contains_key(&canonical),
                "worktree should be in registry before removal"
            );
        }

        // Drive the removal directly.  (The `RemoveWorktree` dispatch arm now
        // just spawns this as a detached task so the connection loop stays
        // responsive; awaiting it here keeps the assertion deterministic while
        // still exercising the full git + fs + registry teardown path.)
        remove_worktree(&shared, &canonical, true).await;

        // Registry must no longer contain the worktree.
        {
            let reg = shared.registry.lock().await;
            assert!(
                !reg.worktrees.contains_key(&canonical),
                "worktree must be absent from registry after removal"
            );
        }

        // state.toml must not list it.
        let st = state::load(&root).expect("load state");
        assert!(
            st.worktrees.iter().all(|w| w.path != canonical),
            "removed worktree must not appear in state.toml"
        );

        // git worktree list must not contain the path.
        let output = std::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&root)
            .output()
            .expect("git worktree list");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.contains(canonical.to_string_lossy().as_ref()),
            "removed worktree must not appear in git worktree list"
        );
    }

    // -- Watcher PrStatusChanged → pr_status update + merge-edge auto-continue --

    #[tokio::test]
    async fn watcher_pr_status_changed_updates_registry_and_state() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root.clone(), gh).await;

        let path = PathBuf::from("/tmp/pr-status-wt");
        seed_worktree(&shared, &path, false, None).await;

        handle_watch_event(
            &shared,
            AppEvent::PrStatusChanged {
                worktree_path: path.clone(),
                pr_status: PrStatus::ChecksPassing,
                pr_number: Some(123),
                pr_url: Some("https://github.com/owner/repo/pull/123".to_string()),
                pr_title: None,
                unresolved_comments: None,
            },
        )
        .await;

        // Registry reflects the new pr_status + pr_number + pr_url; agent status untouched.
        {
            let reg = shared.registry.lock().await;
            let v = reg.worktrees.get(&path).unwrap();
            assert_eq!(v.pr_status, PrStatus::ChecksPassing);
            assert_eq!(v.pr_number, Some(123));
            assert_eq!(
                v.pr_url,
                Some("https://github.com/owner/repo/pull/123".to_string())
            );
            assert_eq!(v.status, WorktreeStatus::Idle);
        }
        // Persisted to state.toml.
        let st = state::load(&root).expect("load");
        let w = st.worktrees.iter().find(|w| w.path == path).unwrap();
        assert_eq!(w.pr_status, PrStatus::ChecksPassing);
        assert_eq!(w.pr_number, Some(123));
        assert_eq!(
            w.pr_url,
            Some("https://github.com/owner/repo/pull/123".to_string())
        );
    }

    #[tokio::test]
    async fn set_pr_status_stores_unresolved_and_clears_for_non_open() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root.clone(), gh).await;

        let path = PathBuf::from("/tmp/unresolved-wt");
        seed_worktree(&shared, &path, false, None).await;

        // Open PR with 3 unresolved comments → stored in registry + state.
        shared
            .set_pr_status(&path, PrStatus::Open, Some(7), None, None, Some(3))
            .await;
        {
            let reg = shared.registry.lock().await;
            assert_eq!(
                reg.worktrees.get(&path).unwrap().unresolved_comments,
                Some(3)
            );
        }
        let st = state::load(&root).expect("load");
        assert_eq!(
            st.worktrees
                .iter()
                .find(|w| w.path == path)
                .unwrap()
                .unresolved_comments,
            Some(3)
        );

        // Merged → unresolved is cleared to None even if a count is passed.
        shared
            .set_pr_status(&path, PrStatus::Merged, Some(7), None, None, Some(9))
            .await;
        {
            let reg = shared.registry.lock().await;
            assert_eq!(reg.worktrees.get(&path).unwrap().unresolved_comments, None);
        }

        // NoPr → also cleared.
        shared
            .set_pr_status(&path, PrStatus::Open, Some(7), None, None, Some(5))
            .await;
        shared
            .set_pr_status(&path, PrStatus::NoPr, None, None, None, Some(5))
            .await;
        {
            let reg = shared.registry.lock().await;
            assert_eq!(reg.worktrees.get(&path).unwrap().unresolved_comments, None);
        }
        let st = state::load(&root).expect("load");
        assert_eq!(
            st.worktrees
                .iter()
                .find(|w| w.path == path)
                .unwrap()
                .unresolved_comments,
            None
        );
    }

    #[tokio::test]
    async fn watcher_nopr_clears_pr_url() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root.clone(), gh).await;

        let path = PathBuf::from("/tmp/nopr-clears-url-wt");
        seed_worktree(&shared, &path, false, None).await;

        // First: set a pr_url via a ChecksPassing event.
        handle_watch_event(
            &shared,
            AppEvent::PrStatusChanged {
                worktree_path: path.clone(),
                pr_status: PrStatus::ChecksPassing,
                pr_number: Some(7),
                pr_url: Some("https://github.com/owner/repo/pull/7".to_string()),
                pr_title: None,
                unresolved_comments: None,
            },
        )
        .await;
        {
            let reg = shared.registry.lock().await;
            assert_eq!(
                reg.worktrees.get(&path).unwrap().pr_url,
                Some("https://github.com/owner/repo/pull/7".to_string())
            );
        }

        // Now NoPr (no url) should clear pr_url.
        handle_watch_event(
            &shared,
            AppEvent::PrStatusChanged {
                worktree_path: path.clone(),
                pr_status: PrStatus::NoPr,
                pr_number: None,
                pr_url: None,
                pr_title: None,
                unresolved_comments: None,
            },
        )
        .await;
        {
            let reg = shared.registry.lock().await;
            assert_eq!(reg.worktrees.get(&path).unwrap().pr_url, None);
        }
        let st = state::load(&root).expect("load");
        let w = st.worktrees.iter().find(|w| w.path == path).unwrap();
        assert_eq!(w.pr_url, None);
    }

    #[tokio::test]
    async fn watcher_pr_title_stored_and_cleared_on_nopr() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root.clone(), gh).await;

        let path = PathBuf::from("/tmp/pr-title-wt");
        seed_worktree(&shared, &path, false, None).await;

        // Set pr_title via a PrStatusChanged event.
        handle_watch_event(
            &shared,
            AppEvent::PrStatusChanged {
                worktree_path: path.clone(),
                pr_status: PrStatus::Open,
                pr_number: Some(42),
                pr_url: Some("https://github.com/owner/repo/pull/42".to_string()),
                pr_title: Some("My shiny PR".to_string()),
                unresolved_comments: Some(3),
            },
        )
        .await;

        // Registry reflects the title.
        {
            let reg = shared.registry.lock().await;
            let v = reg.worktrees.get(&path).unwrap();
            assert_eq!(v.pr_title, Some("My shiny PR".to_string()));
        }
        // Persisted to state.toml.
        let st = state::load(&root).expect("load");
        let w = st.worktrees.iter().find(|w| w.path == path).unwrap();
        assert_eq!(w.pr_title, Some("My shiny PR".to_string()));

        // NoPr clears the title.
        handle_watch_event(
            &shared,
            AppEvent::PrStatusChanged {
                worktree_path: path.clone(),
                pr_status: PrStatus::NoPr,
                pr_number: None,
                pr_url: None,
                pr_title: None,
                unresolved_comments: None,
            },
        )
        .await;

        {
            let reg = shared.registry.lock().await;
            assert_eq!(reg.worktrees.get(&path).unwrap().pr_title, None);
        }
        let st = state::load(&root).expect("load after nopr");
        let w = st.worktrees.iter().find(|w| w.path == path).unwrap();
        assert_eq!(w.pr_title, None);
    }

    #[tokio::test]
    async fn watcher_merge_edge_triggers_continue_for_auto_worktree() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root, gh).await;

        let path = PathBuf::from("/tmp/auto-merge-wt");
        seed_worktree(&shared, &path, /* auto_continue */ true, Some(5)).await;

        let mut sub = shared.events.subscribe();

        // Transition into Merged → continue session starts.
        handle_watch_event(
            &shared,
            AppEvent::PrStatusChanged {
                worktree_path: path.clone(),
                pr_status: PrStatus::Merged,
                pr_number: Some(5),
                pr_url: None,
                pr_title: None,
                unresolved_comments: None,
            },
        )
        .await;

        // The continue session broadcasts Running then NeedsReview (mock Done).
        let mut saw_running = false;
        let mut saw_review = false;
        for _ in 0..8 {
            match tokio::time::timeout(Duration::from_secs(2), sub.recv()).await {
                Ok(Ok(SupervisorMsg::StatusChanged { status, .. })) => match status {
                    WorktreeStatus::Running => saw_running = true,
                    WorktreeStatus::NeedsReview => {
                        saw_review = true;
                        break;
                    }
                    _ => {}
                },
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        assert!(
            saw_running,
            "expected Running status from auto-continue session"
        );
        assert!(saw_review, "expected NeedsReview after continue session");
    }

    #[tokio::test]
    async fn watcher_merge_no_continue_when_flag_off() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root, gh).await;

        let path = PathBuf::from("/tmp/no-auto-wt");
        seed_worktree(&shared, &path, /* auto_continue */ false, Some(9)).await;

        handle_watch_event(
            &shared,
            AppEvent::PrStatusChanged {
                worktree_path: path.clone(),
                pr_status: PrStatus::Merged,
                pr_number: Some(9),
                pr_url: None,
                pr_title: None,
                unresolved_comments: None,
            },
        )
        .await;

        // pr_status is Merged but agent status stays Idle (no continue session).
        let reg = shared.registry.lock().await;
        let v = reg.worktrees.get(&path).unwrap();
        assert_eq!(v.pr_status, PrStatus::Merged);
        assert_eq!(v.status, WorktreeStatus::Idle);
    }

    #[tokio::test]
    async fn watcher_no_reissue_continue_while_staying_merged() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root, gh).await;

        let path = PathBuf::from("/tmp/stay-merged-wt");
        seed_worktree(&shared, &path, /* auto_continue */ true, Some(11)).await;

        // First merge edge fires the continue session.
        handle_watch_event(
            &shared,
            AppEvent::PrStatusChanged {
                worktree_path: path.clone(),
                pr_status: PrStatus::Merged,
                pr_number: Some(11),
                pr_url: None,
                pr_title: None,
                unresolved_comments: None,
            },
        )
        .await;

        // Drain everything the first event produced.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut sub = shared.events.subscribe();

        // A second Merged observation (no transition) must NOT re-trigger a
        // continue session → no Running broadcast follows.
        handle_watch_event(
            &shared,
            AppEvent::PrStatusChanged {
                worktree_path: path.clone(),
                pr_status: PrStatus::Merged,
                pr_number: Some(11),
                pr_url: None,
                pr_title: None,
                unresolved_comments: None,
            },
        )
        .await;

        let mut saw_running = false;
        for _ in 0..4 {
            match tokio::time::timeout(Duration::from_millis(200), sub.recv()).await {
                Ok(Ok(SupervisorMsg::StatusChanged {
                    status: WorktreeStatus::Running,
                    ..
                })) => {
                    saw_running = true;
                    break;
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        assert!(
            !saw_running,
            "must not re-trigger continue while staying Merged"
        );
    }

    // -- Multi-project --------------------------------------------------------

    #[tokio::test]
    async fn rebuild_registry_tags_and_orders_by_project() {
        let (_d1, root1) = make_temp_repo();
        let (_d2, root2) = make_temp_repo();
        let wt1 = tempfile::tempdir().expect("wt1");
        let wt2 = tempfile::tempdir().expect("wt2");
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));

        // Create a worktree in each project's repo.
        WorktreeManager::new(root1.clone())
            .create(None, "feat-a", wt1.path())
            .expect("create in p1");
        WorktreeManager::new(root2.clone())
            .create(None, "feat-b", wt2.path())
            .expect("create in p2");

        let shared = make_shared_with(
            vec![
                make_runtime("alpha", root1.clone()),
                make_runtime("beta", root2.clone()),
            ],
            gh,
        )
        .await;

        let snapshot = shared.rebuild_registry().await;

        // Each created worktree is tagged with the right project.
        let c1 = wt1.path().canonicalize().unwrap();
        let c2 = wt2.path().canonicalize().unwrap();
        let v1 = snapshot.iter().find(|v| v.path == c1).expect("wt1 present");
        let v2 = snapshot.iter().find(|v| v.path == c2).expect("wt2 present");
        assert_eq!(v1.project, "alpha");
        assert_eq!(v2.project, "beta");

        // Ordering: all of alpha's worktrees come before all of beta's.
        let first_beta = snapshot.iter().position(|v| v.project == "beta").unwrap();
        let last_alpha = snapshot.iter().rposition(|v| v.project == "alpha").unwrap();
        assert!(last_alpha < first_beta, "alpha must precede beta in order");
    }

    #[tokio::test]
    async fn project_infos_carries_name_and_path_in_order() {
        let (_d1, root1) = make_temp_repo();
        let (_d2, root2) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared_with(
            vec![
                make_runtime("alpha", root1.clone()),
                make_runtime("beta", root2.clone()),
            ],
            gh,
        )
        .await;

        let infos = shared.project_infos().await;
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].name, "alpha");
        assert_eq!(infos[0].path, root1);
        assert_eq!(infos[1].name, "beta");
        assert_eq!(infos[1].path, root2);
    }

    #[tokio::test]
    async fn state_write_goes_to_owning_project() {
        let (_d1, root1) = make_temp_repo();
        let (_d2, root2) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared_with(
            vec![
                make_runtime("alpha", root1.clone()),
                make_runtime("beta", root2.clone()),
            ],
            gh,
        )
        .await;

        // Seed a worktree owned by "beta" (second project).
        let path = PathBuf::from("/tmp/beta-owned-wt");
        let now = chrono::Utc::now();
        let wt = crate::worktree::model::Worktree {
            path: path.clone(),
            name: "Unnamed".to_string(),
            branch: "feat".to_string(),
            prompt_slug: None,
            pr_number: None,
            pr_url: None,
            pr_title: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            pr_status: crate::worktree::model::PrStatus::NoPr,
            unresolved_comments: None,
            created_at: now,
            updated_at: now,
            session_id: None,
        };
        {
            let mut reg = shared.registry.lock().await;
            reg.worktrees.insert(
                path.clone(),
                ipc::WorktreeView::from_worktree(&wt, "beta".to_string(), None),
            );
            reg.project_of.insert(path.clone(), "beta".to_string());
            reg.order.push(path.clone());
        }
        // Pre-seed beta's state so set_name has an entry to update.
        let mut st = state::load(&root2).expect("load beta state");
        st.upsert_worktree(wt);
        state::save(&root2, &st).expect("save beta state");

        handle_client_msg(
            &shared,
            ClientMsg::SetWorktreeName {
                worktree_path: path.clone(),
                name: "renamed".to_string(),
            },
        )
        .await;

        // The rename lands in beta's state.toml, NOT alpha's.
        let beta = state::load(&root2).expect("load beta");
        assert_eq!(
            beta.worktrees.iter().find(|w| w.path == path).unwrap().name,
            "renamed"
        );
        let alpha = state::load(&root1).expect("load alpha");
        assert!(
            alpha.worktrees.iter().all(|w| w.path != path),
            "alpha state must not contain beta's worktree"
        );
    }

    #[tokio::test]
    async fn new_worktree_creates_under_named_project() {
        let (_d1, root1) = make_temp_repo();
        let (_d2, root2) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared_with(
            vec![
                make_runtime("alpha", root1.clone()),
                make_runtime("beta", root2.clone()),
            ],
            gh,
        )
        .await;

        handle_client_msg(
            &shared,
            ClientMsg::NewWorktree {
                project: "beta".to_string(),
                prompt_slug: None,
                prompt_body: None,
            },
        )
        .await;

        // The new worktree is tagged "beta" and the path contains /local/beta/
        // (no remote → owner = "local", project = "beta").
        let reg = shared.registry.lock().await;
        let created: Vec<_> = reg
            .worktrees
            .values()
            .filter(|v| v.path.to_string_lossy().contains("/local/beta/"))
            .collect();
        assert_eq!(
            created.len(),
            1,
            "exactly one new worktree under /local/beta/"
        );
        assert_eq!(created[0].project, "beta");
    }

    // -- NewWorktree path shape with a real remote ----------------------------

    #[tokio::test]
    async fn new_worktree_path_contains_owner_project_uuid() {
        let (_dir, root) = make_temp_repo();
        // Add a fake remote so git_owner can parse it.
        let status = std::process::Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "git@github.com:TestOrg/testrepo.git",
            ])
            .current_dir(&root)
            .status()
            .expect("git remote add");
        assert!(status.success());

        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root.clone(), gh).await;

        handle_client_msg(
            &shared,
            ClientMsg::NewWorktree {
                project: "proj".to_string(),
                prompt_slug: None,
                prompt_body: None,
            },
        )
        .await;

        let reg = shared.registry.lock().await;
        // Filter for the new detached worktree — path contains /TestOrg/proj/.
        let created: Vec<_> = reg
            .worktrees
            .values()
            .filter(|v| v.path.to_string_lossy().contains("/TestOrg/proj/"))
            .collect();
        assert_eq!(
            created.len(),
            1,
            "exactly one new worktree under /TestOrg/proj/"
        );
        let wt = created[0];

        // UUID leaf.
        let uuid_part = wt.path.file_name().unwrap().to_string_lossy();
        assert!(
            uuid::Uuid::parse_str(&uuid_part).is_ok(),
            "leaf should be a UUID, got {uuid_part}"
        );
    }

    #[tokio::test]
    async fn add_project_rejects_non_git_path() {
        let (_d1, root1) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared_with(vec![make_runtime("alpha", root1)], gh).await;

        let plain = tempfile::tempdir().expect("plain dir");
        let mut sub = shared.events.subscribe();

        handle_client_msg(
            &shared,
            ClientMsg::AddProject {
                path: plain.path().to_path_buf(),
            },
        )
        .await;

        // Expect an Error broadcast (not a Snapshot).
        let msg = tokio::time::timeout(Duration::from_secs(2), sub.recv())
            .await
            .expect("timeout")
            .expect("recv");
        match msg {
            SupervisorMsg::Error {
                worktree_path,
                message,
            } => {
                assert!(worktree_path.is_none());
                assert!(
                    message.contains("not a git repository") || message.contains("git"),
                    "unexpected error message: {message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }

        // The project count is unchanged (still just alpha).
        assert_eq!(shared.projects.lock().await.len(), 1);
    }

    // -- Worktree setup: precedence resolver ----------------------------------

    #[test]
    fn resolve_setup_project_wins_over_global() {
        let project = WorktreeSettings {
            setup_command: Some("project cmd".to_string()),
            setup_timeout_seconds: Some(42),
        };
        let global = WorktreeSettings {
            setup_command: Some("global cmd".to_string()),
            setup_timeout_seconds: Some(99),
        };
        let (cmd, timeout) = resolve_setup(&project, &global);
        assert_eq!(cmd.as_deref(), Some("project cmd"));
        assert_eq!(timeout, Duration::from_secs(42));
    }

    #[test]
    fn resolve_setup_global_is_fallback_when_project_unset() {
        let project = WorktreeSettings::default();
        let global = WorktreeSettings {
            setup_command: Some("global cmd".to_string()),
            setup_timeout_seconds: Some(150),
        };
        let (cmd, timeout) = resolve_setup(&project, &global);
        assert_eq!(cmd.as_deref(), Some("global cmd"));
        assert_eq!(timeout, Duration::from_secs(150));
    }

    #[test]
    fn resolve_setup_neither_set_yields_none_and_default_timeout() {
        let project = WorktreeSettings::default();
        let global = WorktreeSettings::default();
        let (cmd, timeout) = resolve_setup(&project, &global);
        assert!(cmd.is_none());
        assert_eq!(timeout, Duration::from_secs(DEFAULT_SETUP_TIMEOUT_SECS));
        assert_eq!(timeout, Duration::from_secs(300));
    }

    #[test]
    fn resolve_setup_mixed_command_global_timeout_project() {
        // Command only on global, timeout only on project.
        let project = WorktreeSettings {
            setup_command: None,
            setup_timeout_seconds: Some(10),
        };
        let global = WorktreeSettings {
            setup_command: Some("global cmd".to_string()),
            setup_timeout_seconds: None,
        };
        let (cmd, timeout) = resolve_setup(&project, &global);
        assert_eq!(cmd.as_deref(), Some("global cmd"));
        assert_eq!(timeout, Duration::from_secs(10));
    }

    // -- Worktree setup: non-interactive timed runner -------------------------

    #[tokio::test]
    #[cfg(unix)]
    async fn run_setup_success_is_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = run_setup("exit 0", dir.path(), Duration::from_secs(5)).await;
        assert!(r.is_ok(), "expected Ok, got {r:?}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn run_setup_nonzero_exit_carries_stderr() {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = run_setup("echo boom 1>&2; exit 1", dir.path(), Duration::from_secs(5)).await;
        match r {
            Err(e) => assert!(
                e.message.contains("boom"),
                "expected 'boom' in error, got: {}",
                e.message
            ),
            Ok(()) => panic!("expected Err for non-zero exit"),
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn run_setup_times_out_promptly_and_kills() {
        let dir = tempfile::tempdir().expect("tempdir");
        let start = std::time::Instant::now();
        // `sleep 2` with a 150ms timeout must fail quickly (well under 2s).
        let r = run_setup("sleep 2", dir.path(), Duration::from_millis(150)).await;
        let elapsed = start.elapsed();
        match r {
            Err(e) => assert!(
                e.message.contains("timed out"),
                "expected timeout error, got: {}",
                e.message
            ),
            Ok(()) => panic!("expected timeout Err"),
        }
        assert!(
            elapsed < Duration::from_millis(1500),
            "run_setup should return promptly on timeout, took {elapsed:?}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn run_setup_runs_in_cwd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("setup-ran");
        // Write a marker file in the cwd; succeeds only if cwd is correct.
        let r = run_setup("touch setup-ran", dir.path(), Duration::from_secs(5)).await;
        assert!(r.is_ok(), "expected Ok, got {r:?}");
        assert!(
            marker.exists(),
            "setup command should run in the worktree cwd"
        );
    }

    // -- NewWorktree runs setup before the (mock) agent ----------------------

    #[tokio::test]
    async fn new_worktree_runs_setup_before_prompt() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));

        // Configure a setup command on the project that drops a marker file.
        // The marker path is derived per-worktree below; here we use a command
        // that records the running-setup summary ordering via the broadcast.
        let mut runtime = make_runtime("proj", root.clone());
        runtime.project_config.worktree = WorktreeSettings {
            setup_command: Some("true".to_string()),
            setup_timeout_seconds: Some(5),
        };
        let shared = make_shared_with(vec![runtime], gh).await;

        let mut sub = shared.events.subscribe();

        handle_client_msg(
            &shared,
            ClientMsg::NewWorktree {
                project: "proj".to_string(),
                prompt_slug: Some("refactor".to_string()),
                prompt_body: Some("do the refactor".to_string()),
            },
        )
        .await;

        // The first StatusChanged must be the "running setup: …" Running update,
        // BEFORE the agent's own Running update (setup-first, agent-second).
        let mut saw_setup_summary = false;
        let mut saw_running = false;
        for _ in 0..10 {
            match tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
                Ok(Ok(SupervisorMsg::StatusChanged {
                    status: WorktreeStatus::Running,
                    summary,
                    ..
                })) => {
                    if let Some(s) = summary {
                        if s.starts_with("running setup:") {
                            saw_setup_summary = true;
                            continue;
                        }
                    }
                    // A Running with no setup summary = the agent run.
                    saw_running = true;
                    break;
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        assert!(
            saw_setup_summary,
            "expected a 'running setup:' Running status first"
        );
        assert!(saw_running, "expected the agent Running status after setup");
    }

    #[tokio::test]
    async fn new_worktree_setup_no_prompt_ends_idle() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));

        let mut runtime = make_runtime("proj", root.clone());
        runtime.project_config.worktree = WorktreeSettings {
            setup_command: Some("true".to_string()),
            setup_timeout_seconds: Some(5),
        };
        let shared = make_shared_with(vec![runtime], gh).await;

        let mut sub = shared.events.subscribe();

        handle_client_msg(
            &shared,
            ClientMsg::NewWorktree {
                project: "proj".to_string(),
                prompt_slug: None,
                prompt_body: None,
            },
        )
        .await;

        // After setup (no prompt) the worktree must end Idle.
        let mut saw_idle = false;
        for _ in 0..10 {
            match tokio::time::timeout(Duration::from_secs(3), sub.recv()).await {
                Ok(Ok(SupervisorMsg::StatusChanged {
                    status: WorktreeStatus::Idle,
                    ..
                })) => {
                    saw_idle = true;
                    break;
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        assert!(saw_idle, "expected Idle status after setup with no prompt");
    }

    // -- Shared::error helper -------------------------------------------------

    /// `Shared::error` must BOTH broadcast the Error message and log it (i.e.
    /// a subscriber sees exactly one Error variant with the expected fields).
    #[tokio::test]
    async fn shared_error_broadcasts_and_logs() {
        let (_dir, root) = make_temp_repo();
        let gh: Arc<dyn GhRunner> = Arc::new(MockGh::new(vec![]));
        let shared = make_shared(root, gh).await;

        let mut sub = shared.events.subscribe();

        let wt_path = PathBuf::from("/tmp/error-test-wt");
        shared.error(Some(wt_path.clone()), "something went wrong".to_string());

        let msg = tokio::time::timeout(Duration::from_secs(2), sub.recv())
            .await
            .expect("timeout waiting for broadcast")
            .expect("recv error");

        match msg {
            SupervisorMsg::Error {
                worktree_path,
                message,
            } => {
                assert_eq!(worktree_path.as_deref(), Some(wt_path.as_path()));
                assert_eq!(message, "something went wrong");
            }
            other => panic!("expected SupervisorMsg::Error, got {other:?}"),
        }
    }
}

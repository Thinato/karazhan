//! Thin-client connection to the supervisor daemon (Stage 8c).
//!
//! The TUI no longer owns the agent backend, the watcher, or `state.toml`
//! writes.  Instead it connects to the supervisor daemon over a Unix domain
//! socket, renders from daemon-pushed [`ipc::WorktreeView`]s, and sends
//! [`ipc::ClientMsg`] for every action.  When the TUI quits the daemon (and all
//! agent sessions + the watcher) keeps running.
//!
//! [`connect`] resolves the socket path and, if no daemon is listening yet,
//! auto-spawns one via [`crate::daemon::spawn_supervisor`] (double-fork) and
//! waits for the socket to come up.  It then performs the handshake, forwards
//! the initial snapshot as an [`AppEvent::Snapshot`], and spawns a reader task
//! (daemon → [`AppEvent`]) and a writer task ([`ClientMsg`] → socket).

use std::io::ErrorKind;

use anyhow::{bail, Result};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::app::AppEvent;
use crate::daemon;
use crate::ipc::{self, ClientMsg, HandshakeReq, HandshakeResp, SupervisorMsg, PROTOCOL_VERSION};

/// Capacity of the outbound `ClientMsg` channel feeding the writer task.
const CLIENT_TX_CAP: usize = 64;

/// Maximum time to wait for the daemon's handshake reply before treating it as
/// wedged and restarting it.  Generous enough for a healthy daemon to build its
/// snapshot (sub-second in practice, even with dozens of worktrees), short
/// enough that a stuck daemon never leaves the client hanging on a blank screen.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

/// Handle the TUI uses to talk to the supervisor daemon.
///
/// Cloning is not needed: the `App` owns a single client.  Outgoing commands
/// are pushed onto an mpsc channel drained by a background writer task, so the
/// async event loop never blocks on socket I/O.
pub struct SupervisorClient {
    tx: mpsc::Sender<ClientMsg>,
    /// PID of the daemon we are connected to (informational / status line).
    pub supervisor_pid: u32,
}

impl SupervisorClient {
    /// Send a `ClientMsg` to the daemon.  Awaits a free slot in the writer
    /// channel; if the writer task has gone away the send is logged and dropped.
    pub async fn send(&self, msg: ClientMsg) {
        if let Err(e) = self.tx.send(msg).await {
            tracing::warn!("client: failed to enqueue ClientMsg (writer gone): {e}");
        }
    }

    /// A client backed by a bare channel instead of a socket, so tests can
    /// drive the real `App` handlers and inspect what they send.
    #[cfg(test)]
    pub fn test_pair() -> (Self, mpsc::Receiver<ClientMsg>) {
        let (tx, rx) = mpsc::channel(CLIENT_TX_CAP);
        (
            Self {
                tx,
                supervisor_pid: 0,
            },
            rx,
        )
    }
}

/// Translate a daemon-pushed [`SupervisorMsg`] into the [`AppEvent`] the UI loop
/// consumes.  Pure (no I/O) so it can be unit-tested.
pub fn supervisor_msg_to_app_event(msg: SupervisorMsg) -> AppEvent {
    match msg {
        SupervisorMsg::Snapshot {
            projects,
            worktrees,
        } => AppEvent::Snapshot {
            projects,
            worktrees,
        },
        SupervisorMsg::StatusChanged {
            worktree_path,
            status,
            summary,
            activity,
            turns,
            tokens,
            run_started_at,
        } => AppEvent::WorktreeStatusChanged {
            worktree_path,
            status,
            summary,
            activity,
            turns,
            tokens,
            run_started_at,
        },
        SupervisorMsg::Error {
            worktree_path,
            message,
        } => AppEvent::DaemonError {
            worktree_path,
            message,
        },
        SupervisorMsg::TermScreen {
            worktree_path,
            slot,
            rows,
            cols,
            bytes,
        } => AppEvent::TermScreen {
            worktree_path,
            slot,
            rows,
            cols,
            bytes,
        },
        SupervisorMsg::TermDiff {
            worktree_path,
            slot,
            bytes,
        } => AppEvent::TermDiff {
            worktree_path,
            slot,
            bytes,
        },
        SupervisorMsg::TermSessions {
            worktree_path,
            slots,
            attention,
        } => AppEvent::TermSessions {
            worktree_path,
            slots,
            attention,
        },
        SupervisorMsg::TermError {
            worktree_path,
            message,
        } => AppEvent::TermError {
            worktree_path,
            message,
        },
    }
}

/// Connect to the supervisor daemon, auto-spawning it on first launch.
///
/// On success the initial snapshot is forwarded into `event_tx` as an
/// [`AppEvent::Snapshot`] and reader/writer tasks are spawned.
///
/// If the running daemon speaks a different protocol version (or is so old it
/// predates the `ProtocolMismatch` reply and the handshake frame fails to
/// decode), the stale daemon is cleanly stopped and respawned, and the
/// handshake is retried exactly once.  A second mismatch/failure is a hard
/// error (no infinite loop).
pub async fn connect(event_tx: mpsc::Sender<AppEvent>) -> Result<SupervisorClient> {
    let sock_path = ipc::resolve_socket_path();

    // First attempt: connect + handshake (auto-spawning if nothing listens).
    match connect_once(&sock_path).await? {
        HandshakeOutcome::Ok {
            read_half,
            write_half,
            supervisor_pid,
            projects,
            worktrees,
        } => {
            finish_connect(
                event_tx,
                read_half,
                write_half,
                supervisor_pid,
                projects,
                worktrees,
            )
            .await
        }
        HandshakeOutcome::Mismatch => {
            // Stale daemon (or undecodable handshake): stop it, respawn, retry once.
            stop_running_daemon().await?;
            daemon::spawn_supervisor()?;
            daemon::wait_for_socket(&sock_path, std::time::Duration::from_secs(2)).await?;
            match connect_once(&sock_path).await? {
                HandshakeOutcome::Ok {
                    read_half,
                    write_half,
                    supervisor_pid,
                    projects,
                    worktrees,
                } => {
                    finish_connect(
                        event_tx,
                        read_half,
                        write_half,
                        supervisor_pid,
                        projects,
                        worktrees,
                    )
                    .await
                }
                HandshakeOutcome::Mismatch => bail!(
                    "daemon still unreachable after a restart (protocol mismatch or no handshake \
                     reply): client speaks v{PROTOCOL_VERSION}. Stop any stale daemon (pidfile {}) \
                     and relaunch.",
                    ipc::pidfile_path(&sock_path).display()
                ),
            }
        }
    }
}

/// Outcome of a single connect + handshake attempt.
enum HandshakeOutcome {
    Ok {
        read_half: tokio::net::unix::OwnedReadHalf,
        write_half: tokio::net::unix::OwnedWriteHalf,
        supervisor_pid: u32,
        projects: Vec<ipc::ProjectInfo>,
        worktrees: Vec<ipc::WorktreeView>,
    },
    /// Version mismatch OR an undecodable handshake reply (treat the same):
    /// the daemon must be stopped and respawned.
    Mismatch,
}

/// Connect to the socket (auto-spawning the daemon when nothing is listening),
/// then perform the handshake once.
///
/// A handshake reply that fails to DECODE is reported as
/// [`HandshakeOutcome::Mismatch`] (an even-older daemon predating the
/// `ProtocolMismatch` variant), so the caller can stop + respawn it.
async fn connect_once(sock_path: &std::path::Path) -> Result<HandshakeOutcome> {
    // Probe the socket; auto-spawn the daemon if nothing is listening.
    let stream = match UnixStream::connect(sock_path).await {
        Ok(s) => s,
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
            tracing::info!(
                socket = %sock_path.display(),
                "client: no daemon listening — auto-spawning supervisor"
            );
            daemon::spawn_supervisor()?;
            daemon::wait_for_socket(sock_path, std::time::Duration::from_secs(2)).await?;
            UnixStream::connect(sock_path).await?
        }
        Err(e) => return Err(e.into()),
    };

    let (mut read_half, mut write_half) = stream.into_split();

    // Handshake request (FROZEN 2×u32 — always decodable by any daemon).
    ipc::write_frame_async(
        &mut write_half,
        &HandshakeReq {
            protocol: PROTOCOL_VERSION,
            client_pid: std::process::id(),
        },
    )
    .await?;

    // Read the typed reply.  A decode error means the daemon is old enough that
    // its handshake layout is incompatible — treat it as a mismatch.
    //
    // The read is bounded by HANDSHAKE_TIMEOUT: a wedged/unresponsive daemon must
    // never hang the client forever (that manifests as "karazhan won't open" — a
    // blank alt-screen).  On timeout we treat the daemon as stale and let the
    // caller stop + respawn + retry, which self-heals the hang.
    let reply = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        ipc::read_frame_async::<_, HandshakeResp>(&mut read_half),
    )
    .await
    {
        Ok(r) => r,
        Err(_elapsed) => {
            tracing::warn!(
                timeout_secs = HANDSHAKE_TIMEOUT.as_secs(),
                "client: daemon did not answer handshake in time; assuming wedged daemon, restarting"
            );
            return Ok(HandshakeOutcome::Mismatch);
        }
    };
    match reply {
        Ok(HandshakeResp::Ok {
            supervisor_pid,
            projects,
            worktrees,
        }) => Ok(HandshakeOutcome::Ok {
            read_half,
            write_half,
            supervisor_pid,
            projects,
            worktrees,
        }),
        Ok(HandshakeResp::ProtocolMismatch { supervisor }) => {
            tracing::warn!(
                supervisor,
                client = PROTOCOL_VERSION,
                "client: daemon speaks protocol {supervisor}, client speaks {PROTOCOL_VERSION}; restarting daemon"
            );
            Ok(HandshakeOutcome::Mismatch)
        }
        Err(e) => {
            tracing::warn!(
                "client: handshake reply failed to decode ({e}); assuming stale daemon, restarting"
            );
            Ok(HandshakeOutcome::Mismatch)
        }
    }
}

/// Finish a successful connection: seed the UI snapshot, spawn reader/writer
/// tasks, and return the [`SupervisorClient`] handle.
async fn finish_connect(
    event_tx: mpsc::Sender<AppEvent>,
    mut read_half: tokio::net::unix::OwnedReadHalf,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    supervisor_pid: u32,
    projects: Vec<ipc::ProjectInfo>,
    worktrees: Vec<ipc::WorktreeView>,
) -> Result<SupervisorClient> {
    // Seed the UI with current state immediately.
    let _ = event_tx
        .send(AppEvent::Snapshot {
            projects,
            worktrees,
        })
        .await;

    tracing::info!(supervisor_pid, "client: attached to supervisor");

    // Reader task: daemon → AppEvent.
    let reader_event_tx = event_tx.clone();
    tokio::spawn(async move {
        loop {
            match ipc::read_frame_async::<_, SupervisorMsg>(&mut read_half).await {
                Ok(msg) => {
                    let event = supervisor_msg_to_app_event(msg);
                    if reader_event_tx.send(event).await.is_err() {
                        // UI gone; stop reading.
                        break;
                    }
                }
                Err(e) => {
                    tracing::info!("client: reader stopped (daemon disconnected): {e}");
                    let _ = reader_event_tx.send(AppEvent::DaemonDisconnected).await;
                    break;
                }
            }
        }
    });

    // Writer task: ClientMsg → socket.
    let (tx, mut rx) = mpsc::channel::<ClientMsg>(CLIENT_TX_CAP);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = ipc::write_frame_async(&mut write_half, &msg).await {
                tracing::warn!("client: write failed; writer task exiting: {e}");
                break;
            }
        }
    });

    Ok(SupervisorClient { tx, supervisor_pid })
}

/// `karazhan --agent-event <event>`: run by the hooks of an interactive claude
/// inside a karazhan terminal, which sets `KARAZHAN_WORKTREE`/`KARAZHAN_SLOT`.
/// Reports the event to the daemon.  Never spawns a daemon and never fails,
/// so a missing daemon cannot break the hook.
pub fn send_agent_event(arg: Option<&str>) -> Result<()> {
    let event = arg.and_then(ipc::AgentEvent::from_arg);
    let path = std::env::var_os("KARAZHAN_WORKTREE");
    let slot = std::env::var("KARAZHAN_SLOT")
        .ok()
        .and_then(|s| s.parse().ok());
    let (Some(event), Some(path), Some(slot)) = (event, path, slot) else {
        return Ok(());
    };
    let msg = ClientMsg::AgentEvent {
        worktree_path: path.into(),
        slot,
        event,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let timeout = std::time::Duration::from_secs(3);
    let _ = runtime.block_on(async { tokio::time::timeout(timeout, send_once(msg)).await });
    Ok(())
}

/// Handshake, send `msg`, and wait for the daemon to close the connection
/// (it does once it has handled `msg` and seen our EOF).
async fn send_once(msg: ClientMsg) -> Result<()> {
    let sock = std::env::var_os("KARAZHAN_SOCKET")
        .map_or_else(ipc::resolve_socket_path, std::path::PathBuf::from);
    let stream = UnixStream::connect(sock).await?;
    let (mut read_half, mut write_half) = stream.into_split();
    ipc::write_frame_async(
        &mut write_half,
        &HandshakeReq {
            protocol: PROTOCOL_VERSION,
            client_pid: std::process::id(),
        },
    )
    .await?;
    let HandshakeResp::Ok { .. } = ipc::read_frame_async(&mut read_half).await? else {
        bail!("daemon protocol mismatch");
    };
    ipc::write_frame_async(&mut write_half, &msg).await?;
    // Dropping the write half shuts it down: the daemon sees EOF.
    drop(write_half);
    tokio::io::copy(&mut read_half, &mut tokio::io::sink()).await?;
    Ok(())
}

/// Cleanly stop a running supervisor daemon, if any.
///
/// Reads the pidfile next to the resolved socket; if it names a live PID, sends
/// `SIGTERM` and polls up to ~2s for the process to exit (and/or the socket to
/// disappear).  If still alive after the timeout, escalates to `SIGKILL`.
/// Finally best-effort removes the socket + pidfile so a fresh daemon binds
/// cleanly.  A missing pidfile / no live PID is a no-op (`Ok`).
pub async fn stop_running_daemon() -> Result<()> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let sock_path = ipc::resolve_socket_path();
    let pidfile = ipc::pidfile_path(&sock_path);

    let pid_raw = match ipc::read_pidfile(&pidfile) {
        Some(p) => p,
        None => {
            tracing::info!("client: no pidfile — nothing to stop");
            // Still clear a stale socket so a fresh daemon binds cleanly.
            let _ = std::fs::remove_file(&sock_path);
            return Ok(());
        }
    };
    let pid = Pid::from_raw(pid_raw);

    // Liveness check via signal 0.
    if kill(pid, None).is_err() {
        tracing::info!(pid = pid_raw, "client: pidfile PID not alive — cleaning up");
        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_file(&pidfile);
        return Ok(());
    }

    tracing::info!(pid = pid_raw, "client: sending SIGTERM to stale daemon");
    let _ = kill(pid, Signal::SIGTERM);

    // Poll up to ~2s for the process to exit and/or the socket to disappear.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut exited = false;
    while std::time::Instant::now() < deadline {
        let dead = kill(pid, None).is_err();
        let socket_gone = !sock_path.exists();
        if dead && socket_gone {
            exited = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    if !exited && kill(pid, None).is_ok() {
        tracing::warn!(
            pid = pid_raw,
            "client: daemon did not exit on SIGTERM — escalating to SIGKILL"
        );
        let _ = kill(pid, Signal::SIGKILL);
        // Brief wait for the kernel to reap.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Best-effort cleanup so the fresh daemon binds cleanly.
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&pidfile);
    tracing::info!(pid = pid_raw, "client: stale daemon stopped");
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktree::WorktreeStatus;
    use std::path::PathBuf;

    #[test]
    fn snapshot_maps_to_app_snapshot() {
        let now = chrono::Utc::now();
        let views = vec![ipc::WorktreeView {
            path: PathBuf::from("/wt"),
            project: "proj".into(),
            name: "Unnamed".into(),
            branch: "main".into(),
            prompt_slug: None,
            pr_number: None,
            pr_url: None,
            pr_title: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            pr_status: crate::worktree::model::PrStatus::NoPr,
            unresolved_comments: None,
            last_summary: None,
            activity: None,
            turns: 0,
            tokens: 0,
            run_started_at: None,
            session_id: None,
            created_at: now,
            updated_at: now,
        }];
        let projects = vec![
            ipc::ProjectInfo {
                name: "proj".to_string(),
                path: PathBuf::from("/repo/proj"),
            },
            ipc::ProjectInfo {
                name: "empty".to_string(),
                path: PathBuf::from("/repo/empty"),
            },
        ];
        let event = supervisor_msg_to_app_event(SupervisorMsg::Snapshot {
            projects: projects.clone(),
            worktrees: views.clone(),
        });
        match event {
            AppEvent::Snapshot {
                projects: got_projects,
                worktrees,
            } => {
                assert_eq!(got_projects, projects);
                assert_eq!(worktrees, views);
            }
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }

    #[test]
    fn status_changed_maps_to_worktree_status_changed() {
        let event = supervisor_msg_to_app_event(SupervisorMsg::StatusChanged {
            worktree_path: PathBuf::from("/wt"),
            status: WorktreeStatus::NeedsReview,
            summary: Some("done".into()),
            activity: Some("Editing foo.rs".into()),
            turns: 3,
            tokens: 100,
            run_started_at: None,
        });
        match event {
            AppEvent::WorktreeStatusChanged {
                worktree_path,
                status,
                summary,
                activity,
                turns,
                tokens,
                run_started_at,
            } => {
                assert_eq!(worktree_path, PathBuf::from("/wt"));
                assert_eq!(status, WorktreeStatus::NeedsReview);
                assert_eq!(summary, Some("done".to_string()));
                assert_eq!(activity, Some("Editing foo.rs".to_string()));
                assert_eq!(turns, 3);
                assert_eq!(tokens, 100);
                assert_eq!(run_started_at, None);
            }
            other => panic!("expected WorktreeStatusChanged, got {other:?}"),
        }
    }

    #[test]
    fn error_with_path_maps_to_daemon_error() {
        let event = supervisor_msg_to_app_event(SupervisorMsg::Error {
            worktree_path: Some(PathBuf::from("/wt")),
            message: "gh failed".into(),
        });
        match event {
            AppEvent::DaemonError {
                worktree_path,
                message,
            } => {
                assert_eq!(worktree_path, Some(PathBuf::from("/wt")));
                assert_eq!(message, "gh failed");
            }
            other => panic!("expected DaemonError, got {other:?}"),
        }
    }

    #[test]
    fn error_no_path_maps_to_daemon_error() {
        let event = supervisor_msg_to_app_event(SupervisorMsg::Error {
            worktree_path: None,
            message: "internal".into(),
        });
        match event {
            AppEvent::DaemonError {
                worktree_path,
                message,
            } => {
                assert_eq!(worktree_path, None);
                assert_eq!(message, "internal");
            }
            other => panic!("expected DaemonError, got {other:?}"),
        }
    }

    #[test]
    fn term_screen_maps_to_app_term_screen() {
        let event = supervisor_msg_to_app_event(SupervisorMsg::TermScreen {
            worktree_path: PathBuf::from("/wt"),
            slot: 2,
            rows: 24,
            cols: 80,
            bytes: b"\x1b[H".to_vec(),
        });
        match event {
            AppEvent::TermScreen {
                worktree_path,
                slot,
                rows,
                cols,
                bytes,
            } => {
                assert_eq!(worktree_path, PathBuf::from("/wt"));
                assert_eq!((slot, rows, cols), (2, 24, 80));
                assert_eq!(bytes, b"\x1b[H");
            }
            other => panic!("expected TermScreen, got {other:?}"),
        }
    }

    #[test]
    fn term_diff_maps_to_app_term_diff() {
        let event = supervisor_msg_to_app_event(SupervisorMsg::TermDiff {
            worktree_path: PathBuf::from("/wt"),
            slot: 3,
            bytes: b"x".to_vec(),
        });
        match event {
            AppEvent::TermDiff {
                worktree_path,
                slot,
                bytes,
            } => {
                assert_eq!(worktree_path, PathBuf::from("/wt"));
                assert_eq!(slot, 3);
                assert_eq!(bytes, b"x");
            }
            other => panic!("expected TermDiff, got {other:?}"),
        }
    }

    #[test]
    fn term_sessions_maps_to_app_term_sessions() {
        let event = supervisor_msg_to_app_event(SupervisorMsg::TermSessions {
            worktree_path: PathBuf::from("/wt"),
            slots: vec![1, 3],
            attention: true,
        });
        match event {
            AppEvent::TermSessions {
                worktree_path,
                slots,
                attention,
            } => {
                assert_eq!(worktree_path, PathBuf::from("/wt"));
                assert_eq!(slots, vec![1, 3]);
                assert!(attention);
            }
            other => panic!("expected TermSessions, got {other:?}"),
        }
    }

    #[test]
    fn term_error_maps_to_app_term_error() {
        let event = supervisor_msg_to_app_event(SupervisorMsg::TermError {
            worktree_path: PathBuf::from("/wt"),
            message: "max 9 sessions".into(),
        });
        match event {
            AppEvent::TermError {
                worktree_path,
                message,
            } => {
                assert_eq!(worktree_path, PathBuf::from("/wt"));
                assert_eq!(message, "max 9 sessions");
            }
            other => panic!("expected TermError, got {other:?}"),
        }
    }
}

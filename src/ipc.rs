//! IPC contract for the karazhan supervisor daemon / thin-client split.
//!
//! Transport: Unix domain socket.
//! Framing:   4-byte big-endian length prefix + bincode body (bincode 2.x serde API).
//! Socket path resolution (imbuia-style):
//!   $XDG_RUNTIME_DIR/karazhan/sock
//!   → $XDG_CACHE_HOME/karazhan/sock
//!   → ~/.cache/karazhan/sock
//!
//! Nothing in this module binds, connects, forks, or starts the daemon.
//! That is deferred to Stage 8b (daemon) and 8c (client refactor).

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::worktree::model::{Worktree, WorktreeStatus};

// ── Protocol version ─────────────────────────────────────────────────────────

pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum frame body size accepted by `read_frame_async` (64 MiB).
const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

// ── WorktreeView ─────────────────────────────────────────────────────────────

/// Serialisable snapshot of one worktree, pushed to the client.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WorktreeView {
    pub path: PathBuf,
    pub branch: String,
    pub prompt_slug: Option<String>,
    pub pr_number: Option<u64>,
    pub auto_continue_on_merge: bool,
    pub status: WorktreeStatus,
    /// Last agent summary line received for this worktree.
    pub last_summary: Option<String>,
}

impl WorktreeView {
    /// Build a `WorktreeView` from a live [`Worktree`] plus an optional summary.
    pub fn from_worktree(wt: &Worktree, last_summary: Option<String>) -> Self {
        Self {
            path: wt.path.clone(),
            branch: wt.branch.clone(),
            prompt_slug: wt.prompt_slug.clone(),
            pr_number: wt.pr_number,
            auto_continue_on_merge: wt.auto_continue_on_merge,
            status: wt.status.clone(),
            last_summary,
        }
    }
}

impl From<&Worktree> for WorktreeView {
    fn from(wt: &Worktree) -> Self {
        Self::from_worktree(wt, None)
    }
}

// ── BuiltinKind ──────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum BuiltinKind {
    AddressPrComments,
    CheckCi,
}

// ── Handshake ─────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HandshakeReq {
    pub protocol: u32,
    pub client_pid: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum HandshakeResp {
    Ok {
        supervisor_pid: u32,
        worktrees: Vec<WorktreeView>,
    },
    ProtocolMismatch {
        supervisor: u32,
    },
}

// ── ClientMsg ────────────────────────────────────────────────────────────────

/// Messages the client sends to the supervisor daemon.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ClientMsg {
    /// Re-scan `git worktree list`; daemon will reply with a `SupervisorMsg::Snapshot`.
    Refresh,
    RunPrompt {
        worktree_path: PathBuf,
        prompt: String,
    },
    RunBuiltin {
        worktree_path: PathBuf,
        kind: BuiltinKind,
    },
    SetAutoContinue {
        worktree_path: PathBuf,
        enabled: bool,
    },
    SetPrNumber {
        worktree_path: PathBuf,
        pr: Option<u64>,
    },
    CreateWorktree {
        prompt_slug: Option<String>,
        branch: String,
        path: PathBuf,
    },
    RemoveWorktree {
        path: PathBuf,
        force: bool,
    },
    /// Ask the daemon to stop.
    Shutdown,
}

// ── SupervisorMsg ─────────────────────────────────────────────────────────────

/// Messages the supervisor daemon sends to connected clients.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum SupervisorMsg {
    /// Full state snapshot (after Refresh / create / remove).
    Snapshot { worktrees: Vec<WorktreeView> },
    /// Incremental update for a single worktree.
    StatusChanged {
        worktree_path: PathBuf,
        status: WorktreeStatus,
        summary: Option<String>,
    },
    /// Non-fatal error (gh failures, etc.) surfaced to the client status line.
    Error {
        worktree_path: Option<PathBuf>,
        message: String,
    },
}

// ── Framing helpers ──────────────────────────────────────────────────────────

/// Encode `value` to bincode bytes using the standard configuration.
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    bincode::serde::encode_to_vec(value, bincode::config::standard())
        .map_err(|e| anyhow::anyhow!("bincode encode error: {e}"))
}

/// Decode a value of type `T` from `bytes` using the standard configuration.
fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let (value, _) = bincode::serde::decode_from_slice(bytes, bincode::config::standard())
        .map_err(|e| anyhow::anyhow!("bincode decode error: {e}"))?;
    Ok(value)
}

/// Write a single length-prefixed frame to `w`.
///
/// Format: `[u32 big-endian body_len][body bytes...]`
pub async fn write_frame_async<W, T>(w: &mut W, value: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = encode(value)?;
    let len = body.len() as u32;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Read one length-prefixed frame from `r` and decode it as `T`.
///
/// Returns `Err` if the declared body length exceeds [`MAX_FRAME_LEN`].
pub async fn read_frame_async<R, T>(r: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);

    if len > MAX_FRAME_LEN {
        bail!(
            "frame too large: declared {} bytes, cap is {} bytes",
            len,
            MAX_FRAME_LEN
        );
    }

    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    decode(&body)
}

// ── Path helpers ──────────────────────────────────────────────────────────────

/// Return the directory that should hold the socket and ancillary files,
/// creating it (mkdir -p) as a side effect.
pub fn ensure_sock_dir() -> Result<PathBuf> {
    let dir = sock_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Compute the socket directory without creating it.
fn sock_dir() -> PathBuf {
    if let Some(base) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(base).join("karazhan");
    }
    if let Some(base) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(base).join("karazhan");
    }
    // Fallback: ~/.cache/karazhan
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join(".cache").join("karazhan")
}

/// Resolve the Unix domain socket path, creating the parent directory.
pub fn resolve_socket_path() -> PathBuf {
    sock_dir().join("sock")
}

/// Path to the supervisor PID file (sibling of the socket).
pub fn pidfile_path(sock: &Path) -> PathBuf {
    sock.parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .join("supervisor.pid")
}

/// Path to the supervisor log file (sibling of the socket).
pub fn logfile_path(sock: &Path) -> PathBuf {
    sock.parent()
        .unwrap_or_else(|| Path::new("/tmp"))
        .join("supervisor.log")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    // ── round-trip helpers ────────────────────────────────────────────────────

    fn rt<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T) {
        let bytes = encode(value).expect("encode");
        let decoded: T = decode(&bytes).expect("decode");
        assert_eq!(*value, decoded);
    }

    // ── WorktreeView round-trip ───────────────────────────────────────────────

    #[test]
    fn worktree_view_round_trip() {
        let wv = WorktreeView {
            path: PathBuf::from("/repo/feature"),
            branch: "feature/x".into(),
            prompt_slug: Some("refactor".into()),
            pr_number: Some(42),
            auto_continue_on_merge: true,
            status: WorktreeStatus::Running,
            last_summary: Some("done".into()),
        };
        rt(&wv);
    }

    // ── HandshakeReq / HandshakeResp ──────────────────────────────────────────

    #[test]
    fn handshake_req_round_trip() {
        rt(&HandshakeReq {
            protocol: PROTOCOL_VERSION,
            client_pid: 12345,
        });
    }

    #[test]
    fn handshake_resp_ok_round_trip() {
        rt(&HandshakeResp::Ok {
            supervisor_pid: 999,
            worktrees: vec![WorktreeView {
                path: PathBuf::from("/a"),
                branch: "main".into(),
                prompt_slug: None,
                pr_number: None,
                auto_continue_on_merge: false,
                status: WorktreeStatus::Idle,
                last_summary: None,
            }],
        });
    }

    #[test]
    fn handshake_resp_mismatch_round_trip() {
        rt(&HandshakeResp::ProtocolMismatch { supervisor: 2 });
    }

    // ── ClientMsg variants ────────────────────────────────────────────────────

    #[test]
    fn client_msg_refresh() {
        rt(&ClientMsg::Refresh);
    }

    #[test]
    fn client_msg_run_prompt() {
        rt(&ClientMsg::RunPrompt {
            worktree_path: PathBuf::from("/repo/wt"),
            prompt: "fix the bug".into(),
        });
    }

    #[test]
    fn client_msg_run_builtin_address_pr_comments() {
        rt(&ClientMsg::RunBuiltin {
            worktree_path: PathBuf::from("/repo/wt"),
            kind: BuiltinKind::AddressPrComments,
        });
    }

    #[test]
    fn client_msg_run_builtin_check_ci() {
        rt(&ClientMsg::RunBuiltin {
            worktree_path: PathBuf::from("/repo/wt"),
            kind: BuiltinKind::CheckCi,
        });
    }

    #[test]
    fn client_msg_set_auto_continue() {
        rt(&ClientMsg::SetAutoContinue {
            worktree_path: PathBuf::from("/repo/wt"),
            enabled: true,
        });
    }

    #[test]
    fn client_msg_set_pr_number_some() {
        rt(&ClientMsg::SetPrNumber {
            worktree_path: PathBuf::from("/repo/wt"),
            pr: Some(7),
        });
    }

    #[test]
    fn client_msg_set_pr_number_none() {
        rt(&ClientMsg::SetPrNumber {
            worktree_path: PathBuf::from("/repo/wt"),
            pr: None,
        });
    }

    #[test]
    fn client_msg_create_worktree() {
        rt(&ClientMsg::CreateWorktree {
            prompt_slug: Some("new-feat".into()),
            branch: "feat/new".into(),
            path: PathBuf::from("/repo/feat-new"),
        });
    }

    #[test]
    fn client_msg_remove_worktree() {
        rt(&ClientMsg::RemoveWorktree {
            path: PathBuf::from("/repo/old"),
            force: true,
        });
    }

    #[test]
    fn client_msg_shutdown() {
        rt(&ClientMsg::Shutdown);
    }

    // ── SupervisorMsg variants ────────────────────────────────────────────────

    #[test]
    fn supervisor_msg_snapshot() {
        rt(&SupervisorMsg::Snapshot { worktrees: vec![] });
    }

    #[test]
    fn supervisor_msg_status_changed() {
        rt(&SupervisorMsg::StatusChanged {
            worktree_path: PathBuf::from("/repo/wt"),
            status: WorktreeStatus::NeedsReview,
            summary: Some("agent finished".into()),
        });
    }

    #[test]
    fn supervisor_msg_error_with_path() {
        rt(&SupervisorMsg::Error {
            worktree_path: Some(PathBuf::from("/repo/wt")),
            message: "gh rate limited".into(),
        });
    }

    #[test]
    fn supervisor_msg_error_no_path() {
        rt(&SupervisorMsg::Error {
            worktree_path: None,
            message: "daemon internal error".into(),
        });
    }

    // ── Framing over in-memory duplex ─────────────────────────────────────────

    #[tokio::test]
    async fn framing_duplex_client_msgs() {
        let (mut a, mut b) = duplex(1024);

        let msgs = vec![
            ClientMsg::Refresh,
            ClientMsg::RunPrompt {
                worktree_path: PathBuf::from("/wt"),
                prompt: "hello".into(),
            },
            ClientMsg::Shutdown,
        ];

        // Write all frames on one side.
        for msg in &msgs {
            write_frame_async(&mut a, msg).await.expect("write");
        }

        // Read them back from the other side.
        for expected in &msgs {
            let got: ClientMsg = read_frame_async(&mut b).await.expect("read");
            assert_eq!(*expected, got);
        }
    }

    #[tokio::test]
    async fn framing_duplex_supervisor_msgs() {
        let (mut a, mut b) = duplex(4096);

        let msgs = vec![
            SupervisorMsg::Snapshot { worktrees: vec![] },
            SupervisorMsg::StatusChanged {
                worktree_path: PathBuf::from("/wt"),
                status: WorktreeStatus::Running,
                summary: None,
            },
            SupervisorMsg::Error {
                worktree_path: None,
                message: "oops".into(),
            },
        ];

        for msg in &msgs {
            write_frame_async(&mut a, msg).await.expect("write");
        }
        for expected in &msgs {
            let got: SupervisorMsg = read_frame_async(&mut b).await.expect("read");
            assert_eq!(*expected, got);
        }
    }

    // ── Oversize length guard ─────────────────────────────────────────────────

    #[tokio::test]
    async fn oversized_frame_returns_err() {
        let (mut a, mut b) = duplex(64);

        // Write a length header claiming MAX_FRAME_LEN + 1 bytes.
        let bad_len: u32 = MAX_FRAME_LEN + 1;
        a.write_all(&bad_len.to_be_bytes())
            .await
            .expect("write len");
        // No body bytes follow — the reader must error before trying to read them.
        drop(a);

        let result: Result<ClientMsg> = read_frame_async(&mut b).await;
        assert!(result.is_err(), "expected error for oversized frame");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("too large"),
            "expected 'too large' in error, got: {msg}"
        );
    }

    // ── Path helpers ──────────────────────────────────────────────────────────

    // These tests mutate process-wide env vars that resolve_socket_path() reads,
    // so they must not run concurrently with each other.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn socket_path_uses_xdg_runtime_dir() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Save and restore environment so parallel tests are not disturbed.
        let _guard = EnvGuard::set("XDG_RUNTIME_DIR", "/tmp/test-xdg-runtime");

        let sock = resolve_socket_path();
        assert_eq!(sock, PathBuf::from("/tmp/test-xdg-runtime/karazhan/sock"));
    }

    #[test]
    fn socket_path_falls_back_to_xdg_cache_home() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _r = EnvGuard::remove("XDG_RUNTIME_DIR");
        let _c = EnvGuard::set("XDG_CACHE_HOME", "/tmp/test-cache");

        let sock = resolve_socket_path();
        assert_eq!(sock, PathBuf::from("/tmp/test-cache/karazhan/sock"));
    }

    #[test]
    fn socket_path_falls_back_to_home_cache() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _r = EnvGuard::remove("XDG_RUNTIME_DIR");
        let _c = EnvGuard::remove("XDG_CACHE_HOME");
        let _h = EnvGuard::set("HOME", "/tmp/test-home");

        let sock = resolve_socket_path();
        assert_eq!(sock, PathBuf::from("/tmp/test-home/.cache/karazhan/sock"));
    }

    #[test]
    fn pidfile_and_logfile_are_siblings_of_sock() {
        let sock = PathBuf::from("/run/karazhan/sock");
        assert_eq!(
            pidfile_path(&sock),
            PathBuf::from("/run/karazhan/supervisor.pid")
        );
        assert_eq!(
            logfile_path(&sock),
            PathBuf::from("/run/karazhan/supervisor.log")
        );
    }

    // ── WorktreeView::from_worktree ───────────────────────────────────────────

    #[test]
    fn worktree_view_from_worktree() {
        let wt = Worktree {
            path: PathBuf::from("/repo/feat"),
            branch: "feat/y".into(),
            prompt_slug: Some("tidy".into()),
            pr_number: Some(3),
            auto_continue_on_merge: false,
            status: WorktreeStatus::CIFailing,
        };
        let view = WorktreeView::from_worktree(&wt, Some("summary here".into()));
        assert_eq!(view.path, wt.path);
        assert_eq!(view.branch, wt.branch);
        assert_eq!(view.status, WorktreeStatus::CIFailing);
        assert_eq!(view.last_summary, Some("summary here".into()));

        // From<&Worktree> gives None summary.
        let view2 = WorktreeView::from(&wt);
        assert_eq!(view2.last_summary, None);
    }

    // ── EnvGuard: RAII env-var save/restore ───────────────────────────────────

    struct EnvGuard {
        key: String,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: single-threaded tests only.  Env mutation is inherently
            // racy in multi-threaded test runners, but Rust test threads are
            // separate processes per binary, so each test binary is single-
            // process.  The env-path tests are deliberately sequential (no
            // #[tokio::test] parallelism) and cover distinct env keys where
            // possible.
            #[allow(unsafe_code)]
            unsafe {
                std::env::set_var(key, value);
            }
            Self {
                key: key.to_owned(),
                original,
            }
        }

        fn remove(key: &str) -> Self {
            let original = std::env::var(key).ok();
            #[allow(unsafe_code)]
            unsafe {
                std::env::remove_var(key);
            }
            Self {
                key: key.to_owned(),
                original,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            #[allow(unsafe_code)]
            unsafe {
                match &self.original {
                    Some(v) => std::env::set_var(&self.key, v),
                    None => std::env::remove_var(&self.key),
                }
            }
        }
    }
}

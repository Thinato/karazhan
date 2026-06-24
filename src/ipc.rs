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

use chrono::{DateTime, Utc};

use crate::worktree::model::{PrStatus, Worktree, WorktreeStatus};

// ── Protocol version ─────────────────────────────────────────────────────────

/// Wire-format protocol version.
///
/// RULE: `PROTOCOL_VERSION` MUST be incremented whenever ANY wire-format item
/// changes — adding, removing, reordering, or retyping a field/variant in any
/// of:
///   - [`WorktreeView`]
///   - [`ClientMsg`]
///   - [`SupervisorMsg`]
///   - [`HandshakeReq`]
///   - [`HandshakeResp`]
///   - [`BuiltinKind`]
///   - [`WorktreeStatus`] serialization (defined in `worktree::model`)
///   - the framing (length prefix / bincode config in this module)
///
/// The daemon survives client rebuilds by design, so a version that is NOT
/// bumped after a layout change lets an old daemon and a new client both claim
/// version `N`, agree, then fail to decode each other's bodies (the original
/// `bincode decode error: UnexpectedEnd` bug). Bump this, every time.
///
/// FROZEN HANDSHAKE GUARANTEE: [`HandshakeReq`] (`protocol: u32`,
/// `client_pid: u32`) is a fixed-size 2×u32 record and its layout is FROZEN —
/// keep it exactly two `u32`s so any daemon, of any version, can always decode
/// the request. [`HandshakeResp`]'s variant ORDER is likewise FROZEN/append-only:
/// `Ok` MUST stay variant index 0 and `ProtocolMismatch` MUST stay variant
/// index 1. This guarantees an old daemon and a new client can always exchange
/// the handshake request and a `ProtocolMismatch` reply even when every other
/// wire-format item has changed. Do NOT reorder these two; only append new
/// variants after `ProtocolMismatch`.
pub const PROTOCOL_VERSION: u32 = 13;

/// Maximum frame body size accepted by `read_frame_async` (64 MiB).
const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

// ── WorktreeView ─────────────────────────────────────────────────────────────

/// Serialisable snapshot of one worktree, pushed to the client.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WorktreeView {
    pub path: PathBuf,
    /// Name of the owning project (assigned by the daemon when building views).
    pub project: String,
    /// Human-facing name (supervisor-managed dictionary).
    pub name: String,
    pub branch: String,
    pub prompt_slug: Option<String>,
    pub pr_number: Option<u64>,
    /// Canonical GitHub URL for the PR, if known.
    pub pr_url: Option<String>,
    /// PR title, if known.
    pub pr_title: Option<String>,
    pub auto_continue_on_merge: bool,
    pub status: WorktreeStatus,
    /// PR status (separate axis from the agent-activity `status`).
    pub pr_status: PrStatus,
    /// Count of UNRESOLVED PR review threads (open PRs only); `None` when no
    /// open PR or not yet fetched.
    pub unresolved_comments: Option<u64>,
    /// Last agent summary line received for this worktree.
    pub last_summary: Option<String>,
    /// When the worktree was first created.
    pub created_at: DateTime<Utc>,
    /// When the worktree was last used (any status/name/PR/flag mutation).
    pub updated_at: DateTime<Utc>,
}

impl WorktreeView {
    /// Build a `WorktreeView` from a live [`Worktree`], its owning project's
    /// name, and an optional summary.
    pub fn from_worktree(wt: &Worktree, project: String, last_summary: Option<String>) -> Self {
        Self {
            path: wt.path.clone(),
            project,
            name: wt.name.clone(),
            branch: wt.branch.clone(),
            prompt_slug: wt.prompt_slug.clone(),
            pr_number: wt.pr_number,
            pr_url: wt.pr_url.clone(),
            pr_title: wt.pr_title.clone(),
            auto_continue_on_merge: wt.auto_continue_on_merge,
            status: wt.status.clone(),
            pr_status: wt.pr_status,
            unresolved_comments: wt.unresolved_comments,
            last_summary,
            created_at: wt.created_at,
            updated_at: wt.updated_at,
        }
    }
}

// ── ProjectInfo ────────────────────────────────────────────────────────────────

/// A managed project: its supervisor-assigned name and its root path on disk.
///
/// The client needs the path (not just the name) because per-project prompts
/// live at `<path>/.karazhan/prompts/` and are read client-side.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ProjectInfo {
    pub name: String,
    pub path: PathBuf,
}

// ── BuiltinKind ──────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum BuiltinKind {
    AddressPrComments,
    CheckCi,
}

// ── Handshake ─────────────────────────────────────────────────────────────────

/// FROZEN: fixed-size 2×u32 record. Do NOT add, remove, reorder, or retype
/// these fields — every daemon version must be able to decode this request.
/// See the `PROTOCOL_VERSION` doc comment.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HandshakeReq {
    pub protocol: u32,
    pub client_pid: u32,
}

/// FROZEN variant ORDER (append-only): `Ok` is variant index 0,
/// `ProtocolMismatch` is variant index 1. Do NOT reorder — an old daemon and a
/// new client must always be able to exchange a `ProtocolMismatch` reply.
/// New variants may only be appended after `ProtocolMismatch`.
/// See the `PROTOCOL_VERSION` doc comment.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum HandshakeResp {
    Ok {
        supervisor_pid: u32,
        /// Ordered projects (same order the daemon uses for grid grouping).
        projects: Vec<ProjectInfo>,
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
    /// Create a fresh detached worktree.  The daemon generates the UUID
    /// directory under the configured base and runs `git worktree add
    /// --detach`.  If `prompt_body` is `Some`, the daemon also runs that prompt
    /// body on the new worktree (the client resolves the body from its library
    /// since the daemon has no prompt access).  `prompt_slug` is recorded as
    /// metadata when present.
    NewWorktree {
        /// Name of the project (git repo) to create the detached worktree in.
        project: String,
        prompt_slug: Option<String>,
        prompt_body: Option<String>,
    },
    /// Register a git repository as a new project in the global registry.
    AddProject {
        path: PathBuf,
    },
    /// Rename a worktree (updates the supervisor name dictionary in state.toml).
    SetWorktreeName {
        worktree_path: PathBuf,
        name: String,
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
    Snapshot {
        /// Ordered projects (same order the daemon uses for grid grouping).
        projects: Vec<ProjectInfo>,
        worktrees: Vec<WorktreeView>,
    },
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

/// Read and parse a pidfile into a PID.
///
/// Returns `None` if the file is missing, empty, or does not contain a valid
/// positive integer (garbage tolerated — never panics).
pub fn read_pidfile(pidfile: &Path) -> Option<i32> {
    let contents = std::fs::read_to_string(pidfile).ok()?;
    contents.trim().parse::<i32>().ok().filter(|&p| p > 0)
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
        let now = Utc::now();
        let wv = WorktreeView {
            path: PathBuf::from("/repo/feature"),
            project: "karazhan".into(),
            name: "my-worktree".into(),
            branch: "feature/x".into(),
            prompt_slug: Some("refactor".into()),
            pr_number: Some(42),
            pr_url: Some("https://github.com/owner/repo/pull/42".into()),
            pr_title: Some("Fix the bug".into()),
            auto_continue_on_merge: true,
            status: WorktreeStatus::Running,
            pr_status: PrStatus::ChecksPassing,
            unresolved_comments: Some(2),
            last_summary: Some("done".into()),
            created_at: now,
            updated_at: now,
        };
        rt(&wv);
    }

    #[test]
    fn worktree_view_round_trip_with_pr_url_none() {
        let now = Utc::now();
        let wv = WorktreeView {
            path: PathBuf::from("/repo/feature"),
            project: "karazhan".into(),
            name: "my-worktree".into(),
            branch: "feature/x".into(),
            prompt_slug: None,
            pr_number: None,
            pr_url: None,
            pr_title: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            pr_status: PrStatus::NoPr,
            unresolved_comments: None,
            last_summary: None,
            created_at: now,
            updated_at: now,
        };
        rt(&wv);
    }

    #[test]
    fn worktree_view_round_trip_pr_status_variants() {
        let now = Utc::now();
        for pr_status in [
            PrStatus::Loading,
            PrStatus::NoPr,
            PrStatus::Draft,
            PrStatus::Open,
            PrStatus::ChecksRunning,
            PrStatus::ChecksFailing,
            PrStatus::ChecksPassing,
            PrStatus::Approved,
            PrStatus::Merged,
            PrStatus::Closed,
        ] {
            let wv = WorktreeView {
                path: PathBuf::from("/repo/feature"),
                project: "karazhan".into(),
                name: "wt".into(),
                branch: "feat".into(),
                prompt_slug: None,
                pr_number: Some(7),
                pr_url: None,
                pr_title: None,
                auto_continue_on_merge: false,
                status: WorktreeStatus::Idle,
                pr_status,
                unresolved_comments: None,
                last_summary: None,
                created_at: now,
                updated_at: now,
            };
            rt(&wv);
        }
    }

    #[test]
    fn worktree_view_round_trip_with_pr_title() {
        let now = Utc::now();
        let wv = WorktreeView {
            path: PathBuf::from("/repo/feature"),
            project: "karazhan".into(),
            name: "my-worktree".into(),
            branch: "feature/x".into(),
            prompt_slug: None,
            pr_number: Some(99),
            pr_url: Some("https://github.com/owner/repo/pull/99".into()),
            pr_title: Some("Add copy PR URL + title command".into()),
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            pr_status: PrStatus::Open,
            unresolved_comments: Some(1),
            last_summary: None,
            created_at: now,
            updated_at: now,
        };
        rt(&wv);
        assert_eq!(wv.pr_title, Some("Add copy PR URL + title command".into()));
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
        let now = Utc::now();
        rt(&HandshakeResp::Ok {
            supervisor_pid: 999,
            projects: vec![
                ProjectInfo {
                    name: "proj".into(),
                    path: PathBuf::from("/repo/proj"),
                },
                ProjectInfo {
                    name: "empty-proj".into(),
                    path: PathBuf::from("/repo/empty-proj"),
                },
            ],
            worktrees: vec![WorktreeView {
                path: PathBuf::from("/a"),
                project: "proj".into(),
                name: "Unnamed".into(),
                branch: "main".into(),
                prompt_slug: None,
                pr_number: None,
                pr_url: None,
                pr_title: None,
                auto_continue_on_merge: false,
                status: WorktreeStatus::Idle,
                pr_status: PrStatus::NoPr,
                unresolved_comments: None,
                last_summary: None,
                created_at: now,
                updated_at: now,
            }],
        });
    }

    #[test]
    fn handshake_resp_mismatch_round_trip() {
        rt(&HandshakeResp::ProtocolMismatch { supervisor: 2 });
    }

    // ── Protocol version + frozen wire format ─────────────────────────────────

    #[test]
    fn protocol_version_is_thirteen() {
        assert_eq!(PROTOCOL_VERSION, 13);
    }

    #[test]
    fn project_info_round_trip() {
        rt(&ProjectInfo {
            name: "karazhan".into(),
            path: PathBuf::from("/repo/karazhan"),
        });
    }

    /// Locks the FROZEN variant order of `HandshakeResp`: the first body byte is
    /// the bincode varint enum tag, which MUST be 0 for `Ok` and 1 for
    /// `ProtocolMismatch`. A future reorder breaks this test on purpose.
    #[test]
    fn handshake_resp_variant_tags_are_frozen() {
        let ok = encode(&HandshakeResp::Ok {
            supervisor_pid: 1,
            projects: vec![],
            worktrees: vec![],
        })
        .expect("encode ok");
        assert_eq!(ok[0], 0, "HandshakeResp::Ok must stay variant index 0");

        let mismatch =
            encode(&HandshakeResp::ProtocolMismatch { supervisor: 9 }).expect("encode mismatch");
        assert_eq!(
            mismatch[0], 1,
            "HandshakeResp::ProtocolMismatch must stay variant index 1"
        );
    }

    // ── pidfile parse ─────────────────────────────────────────────────────────

    #[test]
    fn read_pidfile_round_trips_a_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("supervisor.pid");
        std::fs::write(&pidfile, "12345\n").expect("write pidfile");
        assert_eq!(read_pidfile(&pidfile), Some(12345));
    }

    #[test]
    fn read_pidfile_missing_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("does-not-exist.pid");
        assert_eq!(read_pidfile(&pidfile), None);
    }

    #[test]
    fn read_pidfile_garbage_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("supervisor.pid");
        std::fs::write(&pidfile, "not a pid").expect("write pidfile");
        assert_eq!(read_pidfile(&pidfile), None);

        std::fs::write(&pidfile, "").expect("write empty");
        assert_eq!(read_pidfile(&pidfile), None);

        std::fs::write(&pidfile, "-7").expect("write negative");
        assert_eq!(read_pidfile(&pidfile), None);
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
    fn client_msg_new_worktree_blank() {
        rt(&ClientMsg::NewWorktree {
            project: "karazhan".into(),
            prompt_slug: None,
            prompt_body: None,
        });
    }

    #[test]
    fn client_msg_new_worktree_with_prompt() {
        rt(&ClientMsg::NewWorktree {
            project: "karazhan".into(),
            prompt_slug: Some("refactor".into()),
            prompt_body: Some("refactor the parser".into()),
        });
    }

    #[test]
    fn client_msg_add_project() {
        rt(&ClientMsg::AddProject {
            path: PathBuf::from("/repo/another"),
        });
    }

    #[test]
    fn client_msg_set_worktree_name() {
        rt(&ClientMsg::SetWorktreeName {
            worktree_path: PathBuf::from("/repo/wt"),
            name: "shiny".into(),
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
        rt(&SupervisorMsg::Snapshot {
            projects: vec![
                ProjectInfo {
                    name: "alpha".into(),
                    path: PathBuf::from("/repo/alpha"),
                },
                ProjectInfo {
                    name: "beta".into(),
                    path: PathBuf::from("/repo/beta"),
                },
            ],
            worktrees: vec![],
        });
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
            SupervisorMsg::Snapshot {
                projects: vec![ProjectInfo {
                    name: "alpha".into(),
                    path: PathBuf::from("/repo/alpha"),
                }],
                worktrees: vec![],
            },
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
        let now = Utc::now();
        let wt = Worktree {
            path: PathBuf::from("/repo/feat"),
            name: "feat-name".into(),
            branch: "feat/y".into(),
            prompt_slug: Some("tidy".into()),
            pr_number: Some(3),
            pr_url: Some("https://github.com/owner/repo/pull/3".into()),
            pr_title: Some("My PR title".into()),
            auto_continue_on_merge: false,
            status: WorktreeStatus::CIFailing,
            pr_status: PrStatus::Merged,
            unresolved_comments: Some(4),
            created_at: now,
            updated_at: now,
        };
        let view = WorktreeView::from_worktree(&wt, "karazhan".into(), Some("summary here".into()));
        assert_eq!(view.path, wt.path);
        assert_eq!(view.project, "karazhan");
        assert_eq!(view.name, wt.name);
        assert_eq!(view.branch, wt.branch);
        assert_eq!(view.status, WorktreeStatus::CIFailing);
        assert_eq!(view.pr_status, PrStatus::Merged);
        assert_eq!(view.unresolved_comments, Some(4));
        assert_eq!(
            view.pr_url,
            Some("https://github.com/owner/repo/pull/3".into())
        );
        assert_eq!(view.last_summary, Some("summary here".into()));
        assert_eq!(view.created_at, now);
        assert_eq!(view.updated_at, now);

        // No summary → None.
        let view2 = WorktreeView::from_worktree(&wt, "karazhan".into(), None);
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

//! Agent session lifecycle: bridge a spawned process's piped stdout into
//! coarse [`AgentStatus`] updates.
//!
//! The Claude Code headless CLI (`claude -p --output-format stream-json
//! --verbose`) emits one JSON object per line.  Known object `type`s include:
//!
//! - `"system"`  — session init / metadata.
//! - `"assistant"` / `"user"` — message turns.
//! - `"result"`  — final result, with a `subtype` (`"success"` / `"error_*"`)
//!   and a `result` text field.
//!
//! We deliberately surface only coarse progress + an optional short summary
//! (the truncated `result` text), never the raw transcript.  Unknown / non-JSON
//! lines are ignored so version drift in the stream format cannot crash us.

use std::path::PathBuf;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use super::{AgentStatus, SessionHandle};

/// Maximum length of the summary string surfaced to the UI.
const SUMMARY_MAX: usize = 120;

/// Mutable state threaded through the line parser.
///
/// Kept separate from any process so the parser is a pure function testable
/// without spawning anything.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParseState {
    /// Latest coarse status derived from the stream so far.
    pub status: AgentStatus,
    /// Short last-line summary (truncated `result` text), if any.
    pub summary: Option<String>,
}

/// An update emitted by the session runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusUpdate {
    pub worktree_path: PathBuf,
    pub status: AgentStatus,
    pub summary: Option<String>,
}

/// A simulated run plan used by the mock backend (no real process).
///
/// The runner emits `Running`, waits `delay`, then emits `final_status` with
/// `summary` — exercising the exact same channel path a real session uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimPlan {
    pub delay: std::time::Duration,
    pub final_status: AgentStatus,
    pub summary: Option<String>,
}

/// Parse a single stream-json line, mutating `state`.
///
/// Returns `true` if the line changed `state` (caller may emit an update).
/// Tolerates non-JSON and unknown shapes by ignoring them (returns `false`).
///
/// Mapping rules:
/// - any recognized streaming object (`system`/`assistant`/`user`) while not
///   yet finished -> [`AgentStatus::Running`].
/// - `result` with `subtype == "success"` (or absent/other but not an error
///   subtype) -> [`AgentStatus::Done`], capturing the truncated `result` text
///   as the summary.
/// - `result` with an error subtype (starts with `"error"`) -> [`AgentStatus::Error`].
pub fn parse_line(line: &str, state: &mut ParseState) -> bool {
    let line = line.trim();
    if line.is_empty() {
        return false;
    }

    let value: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return false, // non-JSON noise — ignore.
    };

    let ty = match value.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return false, // not a recognized stream object.
    };

    match ty {
        "result" => {
            let subtype = value.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
            let result_text = value
                .get("result")
                .and_then(|r| r.as_str())
                .map(truncate_summary);

            if subtype.starts_with("error") {
                let reason = result_text
                    .clone()
                    .unwrap_or_else(|| format!("agent reported {subtype}"));
                let new = ParseState {
                    status: AgentStatus::Error(reason),
                    summary: result_text,
                };
                changed(state, new)
            } else {
                let new = ParseState {
                    status: AgentStatus::Done,
                    summary: result_text,
                };
                changed(state, new)
            }
        }
        // Any in-flight turn means the session is running. Preserve summary.
        "system" | "assistant" | "user" if state.status != AgentStatus::Running => {
            state.status = AgentStatus::Running;
            true
        }
        _ => false, // unknown type — ignore.
    }
}

fn changed(state: &mut ParseState, new: ParseState) -> bool {
    if *state != new {
        *state = new;
        true
    } else {
        false
    }
}

fn truncate_summary(s: &str) -> String {
    let s = s.trim().replace('\n', " ");
    if s.chars().count() > SUMMARY_MAX {
        let truncated: String = s.chars().take(SUMMARY_MAX).collect();
        format!("{truncated}…")
    } else {
        s
    }
}

/// Drive a spawned process's stdout to completion, sending [`StatusUpdate`]s
/// through `tx`.
///
/// Reads stdout line-by-line, parsing stream-json into coarse status. On
/// process exit: emits [`AgentStatus::Done`] on success or [`AgentStatus::Error`]
/// on non-zero exit (unless the stream already produced a terminal status).
///
/// stdout/stderr are NEVER written to the terminal — only logged via `tracing`
/// and reflected as status, so the TUI is never corrupted.
pub async fn run_session(mut handle: SessionHandle, tx: mpsc::Sender<StatusUpdate>) -> Result<()> {
    let worktree_path = handle.worktree_path.clone();

    let mut child = match handle.child.take() {
        Some(c) => c,
        None => {
            // No real process. If a simulation plan is attached (mock backend),
            // drive it through the same channel; otherwise nothing to do.
            if let Some(plan) = handle.sim.take() {
                return run_simulated(worktree_path, plan, tx).await;
            }
            return Ok(());
        }
    };

    // Emit an immediate Running update.
    let mut state = ParseState {
        status: AgentStatus::Running,
        ..Default::default()
    };
    let _ = tx
        .send(StatusUpdate {
            worktree_path: worktree_path.clone(),
            status: AgentStatus::Running,
            summary: None,
        })
        .await;

    if let Some(stdout) = child.stdout.take() {
        let mut reader = BufReader::new(stdout).lines();
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    tracing::trace!(worktree = %worktree_path.display(), "agent stdout: {line}");
                    if parse_line(&line, &mut state) {
                        let _ = tx
                            .send(StatusUpdate {
                                worktree_path: worktree_path.clone(),
                                status: state.status.clone(),
                                summary: state.summary.clone(),
                            })
                            .await;
                    }
                }
                Ok(None) => break, // EOF
                Err(e) => {
                    tracing::warn!("error reading agent stdout: {e}");
                    break;
                }
            }
        }
    }

    // Await process exit and reconcile final status.
    let exit = child.wait().await;
    let final_status = match exit {
        Ok(status) if status.success() => {
            // If the stream already gave a terminal status, keep it.
            match state.status {
                AgentStatus::Done | AgentStatus::Error(_) => state.status.clone(),
                _ => AgentStatus::Done,
            }
        }
        Ok(status) => {
            // Non-zero exit overrides anything but an already-reported error.
            match state.status {
                AgentStatus::Error(_) => state.status.clone(),
                _ => AgentStatus::Error(format!("agent exited with status {status}")),
            }
        }
        Err(e) => AgentStatus::Error(format!("failed to await agent process: {e}")),
    };

    let _ = tx
        .send(StatusUpdate {
            worktree_path,
            status: final_status,
            summary: state.summary.clone(),
        })
        .await;

    Ok(())
}

/// Drive a [`SimPlan`] (mock backend): emit `Running`, sleep, emit final.
async fn run_simulated(
    worktree_path: PathBuf,
    plan: SimPlan,
    tx: mpsc::Sender<StatusUpdate>,
) -> Result<()> {
    let _ = tx
        .send(StatusUpdate {
            worktree_path: worktree_path.clone(),
            status: AgentStatus::Running,
            summary: None,
        })
        .await;

    tokio::time::sleep(plan.delay).await;

    let _ = tx
        .send(StatusUpdate {
            worktree_path,
            status: plan.final_status,
            summary: plan.summary,
        })
        .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_system_then_assistant_as_running() {
        let mut st = ParseState::default();
        assert_eq!(st.status, AgentStatus::Idle);

        let changed = parse_line(r#"{"type":"system","subtype":"init"}"#, &mut st);
        assert!(changed);
        assert_eq!(st.status, AgentStatus::Running);

        // Second running-type line does not re-trigger a change.
        let changed = parse_line(r#"{"type":"assistant","message":{}}"#, &mut st);
        assert!(!changed);
        assert_eq!(st.status, AgentStatus::Running);
    }

    #[test]
    fn parses_result_success_to_done_with_summary() {
        let mut st = ParseState::default();
        parse_line(r#"{"type":"system"}"#, &mut st);
        let changed = parse_line(
            r#"{"type":"result","subtype":"success","result":"All tests pass."}"#,
            &mut st,
        );
        assert!(changed);
        assert_eq!(st.status, AgentStatus::Done);
        assert_eq!(st.summary.as_deref(), Some("All tests pass."));
    }

    #[test]
    fn parses_result_error_subtype_to_error() {
        let mut st = ParseState::default();
        let changed = parse_line(
            r#"{"type":"result","subtype":"error_max_turns","result":"hit limit"}"#,
            &mut st,
        );
        assert!(changed);
        match &st.status {
            AgentStatus::Error(reason) => assert_eq!(reason, "hit limit"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn garbage_line_is_ignored() {
        let mut st = ParseState::default();
        assert!(!parse_line("this is not json", &mut st));
        assert!(!parse_line("", &mut st));
        assert!(!parse_line(r#"{"no_type":true}"#, &mut st));
        assert!(!parse_line(r#"{"type":"unknown_kind"}"#, &mut st));
        assert_eq!(st.status, AgentStatus::Idle);
    }

    #[tokio::test]
    async fn nonzero_exit_maps_to_error() {
        // Spawn a benign process that exits non-zero (never the real `claude`).
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("exit 3")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = cmd.spawn().expect("spawn sh");

        let handle = SessionHandle {
            worktree_path: PathBuf::from("/tmp/exit-test"),
            child: Some(child),
            sim: None,
        };

        let (tx, mut rx) = mpsc::channel::<StatusUpdate>(8);
        run_session(handle, tx).await.expect("run_session");

        // First: immediate Running.
        let first = rx.recv().await.expect("running update");
        assert_eq!(first.status, AgentStatus::Running);

        // Last update should be an Error from the non-zero exit.
        let mut last = first;
        while let Some(u) = rx.recv().await {
            last = u;
        }
        assert!(
            matches!(last.status, AgentStatus::Error(_)),
            "expected Error, got {:?}",
            last.status
        );
    }

    #[test]
    fn summary_is_truncated() {
        let long = "x".repeat(SUMMARY_MAX + 50);
        let line = format!(r#"{{"type":"result","subtype":"success","result":"{long}"}}"#);
        let mut st = ParseState::default();
        parse_line(&line, &mut st);
        let summary = st.summary.expect("summary");
        // SUMMARY_MAX chars + ellipsis.
        assert_eq!(summary.chars().count(), SUMMARY_MAX + 1);
        assert!(summary.ends_with('…'));
    }
}

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

/// Maximum number of stderr lines to retain for error reporting.
const STDERR_MAX_LINES: usize = 100;
/// Maximum total bytes of stderr to retain (older lines are dropped first).
const STDERR_MAX_BYTES: usize = 8 * 1024;

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
    /// Current human-readable action, e.g. "Editing foo.rs".
    pub activity: Option<String>,
    /// Count of assistant turns with content.
    pub turns: u32,
    /// Cumulative output tokens reported by the stream.
    pub tokens: u64,
    /// Agent `session_id` from the stream-json `init` event, once seen.
    pub session_id: Option<String>,
}

/// An update emitted by the session runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusUpdate {
    pub worktree_path: PathBuf,
    pub status: AgentStatus,
    pub summary: Option<String>,
    pub activity: Option<String>,
    pub turns: u32,
    pub tokens: u64,
    /// Agent `session_id` (constant for the session, captured from `init`).
    pub session_id: Option<String>,
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

    // Capture the session id once.  It appears on the `system`/`init` event (and
    // most subsequent events); record the first non-empty value so the daemon can
    // resume this worktree's session with `--resume <id>`.
    let mut sid_changed = false;
    if state.session_id.is_none() {
        if let Some(sid) = value.get("session_id").and_then(|v| v.as_str()) {
            if !sid.is_empty() {
                state.session_id = Some(sid.to_string());
                sid_changed = true;
            }
        }
    }

    let ty_changed = match ty {
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
                    activity: None,
                    turns: state.turns,
                    tokens: state.tokens,
                    session_id: state.session_id.clone(),
                };
                changed(state, new)
            } else {
                let new = ParseState {
                    status: AgentStatus::Done,
                    summary: result_text,
                    activity: None,
                    turns: state.turns,
                    tokens: state.tokens,
                    session_id: state.session_id.clone(),
                };
                changed(state, new)
            }
        }
        "assistant" => {
            let mut changed_any = false;
            if state.status != AgentStatus::Running {
                state.status = AgentStatus::Running;
                changed_any = true;
            }
            let msg = value.get("message");
            if let Some(content) = msg
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                if !content.is_empty() {
                    state.turns += 1;
                    changed_any = true;
                    if let Some(act) = describe_activity(content) {
                        state.activity = Some(act);
                    }
                }
            }
            if let Some(out) = msg
                .and_then(|m| m.get("usage"))
                .and_then(|u| u.get("output_tokens"))
                .and_then(|v| v.as_u64())
            {
                if out > 0 {
                    state.tokens += out;
                    changed_any = true;
                }
            }
            changed_any
        }
        // Any in-flight turn means the session is running.
        "system" | "user" if state.status != AgentStatus::Running => {
            state.status = AgentStatus::Running;
            true
        }
        _ => false, // unknown type — ignore.
    };
    ty_changed || sid_changed
}

/// Describe the human-facing action represented by a message's content blocks.
///
/// Prefers the LAST `tool_use` block; falls back to "Thinking…" when any text
/// block is present; otherwise `None`.
fn describe_activity(content: &[serde_json::Value]) -> Option<String> {
    let mut last_tool: Option<String> = None;
    let mut has_text = false;
    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("tool_use") => {
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let input = block
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                last_tool = Some(describe_tool(name, &input));
            }
            Some("text") => has_text = true,
            _ => {}
        }
    }
    if let Some(t) = last_tool {
        Some(t)
    } else if has_text {
        Some("Thinking…".to_string())
    } else {
        None
    }
}

/// Map a tool-use (name + input) to a short human action string.
fn describe_tool(name: &str, input: &serde_json::Value) -> String {
    let field = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match name {
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            let file_path = field("file_path");
            if file_path.is_empty() {
                "Editing".to_string()
            } else {
                format!("Editing {}", basename(file_path))
            }
        }
        "Read" => {
            let file_path = field("file_path");
            format!("Reading {}", basename(file_path))
        }
        "Bash" => format!("Bash: {}", truncate_activity(field("command"), 36)),
        "Grep" => format!("Searching: {}", truncate_activity(field("pattern"), 30)),
        "Glob" => format!("Globbing: {}", truncate_activity(field("pattern"), 30)),
        "Task" => format!("Subagent: {}", truncate_activity(field("description"), 30)),
        "WebFetch" | "WebSearch" => "Browsing web…".to_string(),
        "TodoWrite" => "Planning…".to_string(),
        _ => format!("{name}…"),
    }
}

/// Return the substring after the last `/` (the whole string if none); returns
/// the input unchanged when empty.
fn basename(p: &str) -> &str {
    match p.rfind('/') {
        Some(i) => &p[i + 1..],
        None => p,
    }
}

/// Truncate `s` to at most `max` characters, appending `…` if truncated.
fn truncate_activity(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    } else {
        s.to_string()
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
/// Reads stdout line-by-line, parsing stream-json into coarse status. stderr is
/// drained concurrently (via a spawned task) so a full stderr pipe never stalls
/// the child; the last [`STDERR_MAX_LINES`] / [`STDERR_MAX_BYTES`] are retained.
///
/// On process exit: emits [`AgentStatus::Done`] on success or
/// [`AgentStatus::Error`] on non-zero exit (unless the stream already produced a
/// terminal status).  On failure the captured stderr tail is appended to the
/// error message and logged to the daemon log via `tracing::error!`.
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
            activity: None,
            turns: 0,
            tokens: 0,
            session_id: None,
        })
        .await;

    // Drain stderr concurrently so a full pipe never blocks the child.
    // The task accumulates lines into a bounded ring-buffer and joins below.
    let stderr_task = if let Some(stderr) = child.stderr.take() {
        let wt = worktree_path.clone();
        Some(tokio::spawn(async move { drain_stderr(stderr, &wt).await }))
    } else {
        None
    };

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
                                activity: state.activity.clone(),
                                turns: state.turns,
                                tokens: state.tokens,
                                session_id: state.session_id.clone(),
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

    // Collect the captured stderr tail (the task finishes quickly once the
    // child's stderr pipe closes, which happens after wait()).
    let exit = child.wait().await;

    let stderr_tail = if let Some(task) = stderr_task {
        task.await.unwrap_or_default()
    } else {
        String::new()
    };

    // Await process exit and reconcile final status.
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
                _ => {
                    let msg = build_exit_error(
                        format!("agent exited with status {status}"),
                        &stderr_tail,
                    );
                    tracing::error!(
                        worktree = %worktree_path.display(),
                        stderr_tail = %stderr_tail,
                        "agent process failed: {msg}"
                    );
                    AgentStatus::Error(msg)
                }
            }
        }
        Err(e) => {
            let msg = build_exit_error(format!("failed to await agent process: {e}"), &stderr_tail);
            tracing::error!(
                worktree = %worktree_path.display(),
                stderr_tail = %stderr_tail,
                "agent wait failed: {msg}"
            );
            AgentStatus::Error(msg)
        }
    };

    let _ = tx
        .send(StatusUpdate {
            worktree_path,
            status: final_status,
            summary: state.summary.clone(),
            activity: None,
            turns: state.turns,
            tokens: state.tokens,
            session_id: state.session_id.clone(),
        })
        .await;

    Ok(())
}

/// Drain a child's stderr pipe, logging each line at `debug` and accumulating a
/// bounded tail (at most [`STDERR_MAX_LINES`] lines / [`STDERR_MAX_BYTES`] bytes).
///
/// Returns the captured tail as a single string (lines joined by `\n`), ready to
/// be appended to an error message.
async fn drain_stderr(
    stderr: impl tokio::io::AsyncRead + Unpin,
    worktree_path: &std::path::Path,
) -> String {
    let mut reader = BufReader::new(stderr).lines();
    let mut lines: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut total_bytes: usize = 0;

    loop {
        match reader.next_line().await {
            Ok(Some(line)) => {
                tracing::debug!(worktree = %worktree_path.display(), "agent stderr: {line}");
                total_bytes += line.len() + 1; // +1 for newline
                lines.push_back(line);

                // Evict oldest lines to stay within caps.
                while lines.len() > STDERR_MAX_LINES || total_bytes > STDERR_MAX_BYTES {
                    if let Some(evicted) = lines.pop_front() {
                        total_bytes = total_bytes.saturating_sub(evicted.len() + 1);
                    } else {
                        break;
                    }
                }
            }
            Ok(None) => break, // EOF
            Err(e) => {
                tracing::debug!("error reading agent stderr: {e}");
                break;
            }
        }
    }

    lines.into_iter().collect::<Vec<_>>().join("\n")
}

/// Compose an error message from a base reason and an optional stderr tail.
///
/// If `stderr_tail` is non-empty, it is appended (truncated to keep the total
/// under a sane UI length) so the daemon log and the client toast both show the
/// real failure output without extra lookups.
fn build_exit_error(base: String, stderr_tail: &str) -> String {
    let stderr_tail = stderr_tail.trim();
    if stderr_tail.is_empty() {
        return base;
    }
    // Truncate the tail to at most SUMMARY_MAX chars so it stays legible.
    let tail: String = if stderr_tail.chars().count() > SUMMARY_MAX {
        let t: String = stderr_tail.chars().take(SUMMARY_MAX).collect();
        format!("{t}…")
    } else {
        stderr_tail.to_string()
    };
    format!("{base}\nstderr: {tail}")
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
            activity: None,
            turns: 0,
            tokens: 0,
            session_id: None,
        })
        .await;

    tokio::time::sleep(plan.delay).await;

    let _ = tx
        .send(StatusUpdate {
            worktree_path,
            status: plan.final_status,
            summary: plan.summary,
            activity: None,
            turns: 0,
            tokens: 0,
            session_id: None,
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
    fn captures_session_id_from_init_and_keeps_it() {
        let mut st = ParseState::default();
        assert_eq!(st.session_id, None);

        let changed = parse_line(
            r#"{"type":"system","subtype":"init","session_id":"sess-abc"}"#,
            &mut st,
        );
        assert!(changed);
        assert_eq!(st.session_id.as_deref(), Some("sess-abc"));

        // A later event with a different session_id does NOT overwrite the first.
        parse_line(
            r#"{"type":"assistant","session_id":"other","message":{}}"#,
            &mut st,
        );
        assert_eq!(st.session_id.as_deref(), Some("sess-abc"));
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
    #[cfg(unix)]
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

    /// Stderr written by a failing child is captured and surfaced in the Error
    /// message (gap 1 fix).
    #[tokio::test]
    #[cfg(unix)]
    async fn nonzero_exit_includes_stderr_in_error() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("echo boom 1>&2; exit 3")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = cmd.spawn().expect("spawn sh");

        let handle = SessionHandle {
            worktree_path: PathBuf::from("/tmp/stderr-test"),
            child: Some(child),
            sim: None,
        };

        let (tx, mut rx) = mpsc::channel::<StatusUpdate>(8);
        run_session(handle, tx).await.expect("run_session");

        // Collect all updates; the last one must be an Error containing "boom".
        let mut last = rx.recv().await.expect("at least one update");
        while let Some(u) = rx.recv().await {
            last = u;
        }
        match &last.status {
            AgentStatus::Error(msg) => {
                assert!(
                    msg.contains("boom"),
                    "expected 'boom' in error message, got: {msg:?}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// A child that exits cleanly (zero) maps to Done, not Error.
    #[tokio::test]
    #[cfg(unix)]
    async fn zero_exit_maps_to_done() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("exit 0")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = cmd.spawn().expect("spawn sh");

        let handle = SessionHandle {
            worktree_path: PathBuf::from("/tmp/success-test"),
            child: Some(child),
            sim: None,
        };

        let (tx, mut rx) = mpsc::channel::<StatusUpdate>(8);
        run_session(handle, tx).await.expect("run_session");

        let mut last = rx.recv().await.expect("at least one update");
        while let Some(u) = rx.recv().await {
            last = u;
        }
        assert_eq!(
            last.status,
            AgentStatus::Done,
            "zero exit should be Done, got {:?}",
            last.status
        );
    }

    /// Pure helper: `build_exit_error` appends the stderr tail to the base message.
    #[test]
    fn build_exit_error_includes_stderr() {
        let msg = build_exit_error(
            "agent exited with status 1".to_string(),
            "fatal: not a repo",
        );
        assert!(msg.contains("agent exited with status 1"));
        assert!(msg.contains("fatal: not a repo"));
    }

    /// Pure helper: `build_exit_error` returns base unchanged when stderr is empty.
    #[test]
    fn build_exit_error_empty_stderr() {
        let msg = build_exit_error("agent exited with status 1".to_string(), "");
        assert_eq!(msg, "agent exited with status 1");
        assert!(!msg.contains("stderr:"));
    }

    /// Pure helper: `build_exit_error` truncates a very long stderr tail.
    #[test]
    fn build_exit_error_truncates_long_stderr() {
        let long_stderr = "x".repeat(SUMMARY_MAX + 50);
        let msg = build_exit_error("base".to_string(), &long_stderr);
        assert!(msg.contains("…"), "expected ellipsis for truncated tail");
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

    #[test]
    fn assistant_tool_use_edit_sets_activity_turns_tokens() {
        let mut st = ParseState::default();
        let changed = parse_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/a/b/foo.rs"}}],"usage":{"output_tokens":50}}}"#,
            &mut st,
        );
        assert!(changed);
        assert_eq!(st.activity.as_deref(), Some("Editing foo.rs"));
        assert_eq!(st.turns, 1);
        assert_eq!(st.tokens, 50);
    }

    #[test]
    fn assistant_bash_tool_use_sets_activity() {
        let mut st = ParseState::default();
        parse_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#,
            &mut st,
        );
        assert_eq!(st.activity.as_deref(), Some("Bash: cargo test"));
    }

    #[test]
    fn assistant_text_only_is_thinking() {
        let mut st = ParseState::default();
        parse_line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"let me look"}]}}"#,
            &mut st,
        );
        assert_eq!(st.activity.as_deref(), Some("Thinking…"));
    }

    #[test]
    fn result_success_clears_activity_keeps_turns_tokens() {
        let mut st = ParseState::default();
        parse_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/a/b/foo.rs"}}],"usage":{"output_tokens":50}}}"#,
            &mut st,
        );
        assert_eq!(st.turns, 1);
        assert_eq!(st.tokens, 50);
        parse_line(
            r#"{"type":"result","subtype":"success","result":"All done."}"#,
            &mut st,
        );
        assert_eq!(st.status, AgentStatus::Done);
        assert_eq!(st.activity, None);
        assert_eq!(st.turns, 1);
        assert_eq!(st.tokens, 50);
    }
}

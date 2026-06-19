//! Pluggable agent backend.
//!
//! An [`AgentBackend`] launches a coding-agent session inside a worktree for a
//! fully-composed prompt and returns a [`SessionHandle`].  The session's raw
//! transcript is never surfaced to the UI — only a coarse [`AgentStatus`] plus
//! an optional short last-line summary.
//!
//! Concrete backends:
//! - [`claude_code::ClaudeCodeBackend`] — spawns the real `claude` headless CLI.
//! - [`mock::MockBackend`] — simulates a session for tests / offline dev.
//!
//! The [`session`] runner bridges a spawned process's piped stdout into status
//! updates.

pub mod claude_code;
pub mod mock;
pub mod session;

use std::path::{Path, PathBuf};

use anyhow::Result;
use tokio::process::Child;

/// Coarse agent session status surfaced to the UI.
///
/// The raw transcript is intentionally NOT represented here — only progress.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AgentStatus {
    /// No work started yet.
    #[default]
    Idle,
    /// Session is actively running.
    Running,
    /// Session finished successfully.
    Done,
    /// Session failed; carries a short human-readable reason.
    Error(String),
}

/// A handle to a running (or simulated) agent session.
///
/// The backend spawns the underlying work and returns this handle; the
/// [`session`] runner drives it to completion, mapping output to
/// [`AgentStatus`] updates.
///
/// For a real process backend, `child` owns the spawned `claude` process and
/// its piped stdout/stderr.  The mock backend leaves `child` as `None` and sets
/// `sim` so the [`session`] runner drives status purely in code.
pub struct SessionHandle {
    /// The worktree this session is running in.
    pub worktree_path: PathBuf,
    /// The spawned child process, if this is a real-process session.
    pub child: Option<Child>,
    /// Simulated run plan for the mock backend (no real process).
    pub sim: Option<session::SimPlan>,
}

impl SessionHandle {
    /// Process id of the spawned child, if any.
    #[allow(dead_code)] // exposed for future cancel/observe wiring (P5+)
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }
}

/// A pluggable coding-agent backend.
///
/// Implementations must be cheap to clone-share across tasks (`Send + Sync`).
#[async_trait::async_trait]
pub trait AgentBackend: Send + Sync {
    /// Launch a new agent session in `worktree_path` for the fully-composed
    /// `prompt` text.
    async fn start(&self, worktree_path: &Path, prompt: &str) -> Result<SessionHandle>;

    /// Resume the most recent session in `worktree_path` (Claude Code: `-c`).
    // Wired into the watcher's PR-merge auto-continue flow in P6; defined now so
    // every backend implements the full trait surface.
    #[allow(dead_code)]
    async fn continue_session(&self, worktree_path: &Path, prompt: &str) -> Result<SessionHandle>;
}

// ---------------------------------------------------------------------------
// AgentStatus -> WorktreeStatus mapping
// ---------------------------------------------------------------------------

use crate::worktree::WorktreeStatus;

/// Map a coarse [`AgentStatus`] onto a persisted [`WorktreeStatus`].
///
/// Rationale for `Done -> NeedsReview`: when an agent finishes, its output has
/// not yet been reviewed or merged by a human.  The square should signal "ready
/// for you to look at" rather than "Idle" (which reads as "nothing happened").
/// `NeedsReview` is the actionable state that prompts the user to inspect the
/// worktree / open a PR; it is downgraded to `PRMerged` / `Idle` by later phases
/// once review/merge actually happens.
pub fn agent_status_to_worktree_status(status: &AgentStatus) -> WorktreeStatus {
    match status {
        AgentStatus::Idle => WorktreeStatus::Idle,
        AgentStatus::Running => WorktreeStatus::Running,
        AgentStatus::Done => WorktreeStatus::NeedsReview,
        AgentStatus::Error(_) => WorktreeStatus::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping() {
        assert_eq!(
            agent_status_to_worktree_status(&AgentStatus::Idle),
            WorktreeStatus::Idle
        );
        assert_eq!(
            agent_status_to_worktree_status(&AgentStatus::Running),
            WorktreeStatus::Running
        );
        assert_eq!(
            agent_status_to_worktree_status(&AgentStatus::Done),
            WorktreeStatus::NeedsReview
        );
        assert_eq!(
            agent_status_to_worktree_status(&AgentStatus::Error("boom".into())),
            WorktreeStatus::Error
        );
    }
}

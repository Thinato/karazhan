//! Mock agent backend for tests and offline development.
//!
//! [`MockBackend`] never spawns a real process. Instead it returns a
//! [`SessionHandle`] carrying a [`SimPlan`] that the [`session`](super::session)
//! runner drives: `Running` -> (short delay) -> `Done`, emitted through the same
//! channel mechanism a real session uses.
//!
//! Used as the active backend when the `claude` binary is absent from PATH.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use super::session::SimPlan;
use super::{AgentBackend, AgentStatus, SessionHandle};

/// Backend that simulates an agent session without launching a process.
pub struct MockBackend {
    /// Delay before the session transitions from `Running` to its final status.
    pub delay: Duration,
}

impl MockBackend {
    pub fn new() -> Self {
        Self {
            // Short by default so tests / dev feedback are fast.
            delay: Duration::from_millis(50),
        }
    }

    fn handle(&self, worktree_path: &Path, summary: &str) -> SessionHandle {
        SessionHandle {
            worktree_path: worktree_path.to_path_buf(),
            child: None,
            sim: Some(SimPlan {
                delay: self.delay,
                final_status: AgentStatus::Done,
                summary: Some(summary.to_string()),
            }),
        }
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl AgentBackend for MockBackend {
    async fn start(&self, worktree_path: &Path, _prompt: &str) -> Result<SessionHandle> {
        tracing::info!(worktree = %worktree_path.display(), "mock agent session started");
        Ok(self.handle(worktree_path, "mock session complete"))
    }

    async fn continue_session(
        &self,
        worktree_path: &Path,
        _session_id: Option<&str>,
        _prompt: &str,
    ) -> Result<SessionHandle> {
        tracing::info!(worktree = %worktree_path.display(), "mock agent session continued");
        Ok(self.handle(worktree_path, "mock session continued"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session::{run_session, StatusUpdate};
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn mock_start_transitions_running_then_done() {
        let backend = MockBackend::new();
        let wt = PathBuf::from("/tmp/mock-wt");
        let handle = backend.start(&wt, "do the thing").await.expect("start");
        assert!(handle.child.is_none());
        assert!(handle.sim.is_some());

        let (tx, mut rx) = mpsc::channel::<StatusUpdate>(8);
        run_session(handle, tx).await.expect("run_session");

        let first = rx.recv().await.expect("first update");
        assert_eq!(first.status, AgentStatus::Running);
        assert_eq!(first.worktree_path, wt);

        let second = rx.recv().await.expect("second update");
        assert_eq!(second.status, AgentStatus::Done);
        assert_eq!(second.summary.as_deref(), Some("mock session complete"));

        // Channel closes after the runner returns.
        assert!(rx.recv().await.is_none());
    }
}

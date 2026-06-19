//! Claude Code headless backend.
//!
//! Spawns the `claude` CLI in headless / print mode with streaming JSON output:
//!
//! ```text
//! claude -p <prompt> --output-format stream-json --verbose
//! ```
//!
//! `--verbose` is required for `stream-json` when combined with `-p`; without it
//! the CLI rejects the streaming format.  `continue_session` adds `-c` to resume
//! the most recent session in the worktree.
//!
//! stdout/stderr are piped (never inherited) so agent output cannot corrupt the
//! TUI; the [`session`](super::session) runner consumes stdout for status.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

use super::{AgentBackend, SessionHandle};

/// Backend that drives the real `claude` headless CLI.
pub struct ClaudeCodeBackend {
    /// Binary name / path to invoke. Configurable so tests never depend on a
    /// real `claude` on PATH. Defaults to `"claude"`.
    pub bin: String,
}

impl ClaudeCodeBackend {
    pub fn new() -> Self {
        Self {
            bin: "claude".to_string(),
        }
    }

    /// Build the base command shared by `start` / `continue_session`.
    fn base_command(&self, worktree_path: &Path) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.current_dir(worktree_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }
}

impl Default for ClaudeCodeBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl AgentBackend for ClaudeCodeBackend {
    async fn start(&self, worktree_path: &Path, prompt: &str) -> Result<SessionHandle> {
        let mut cmd = self.base_command(worktree_path);
        cmd.arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose");

        tracing::info!(
            worktree = %worktree_path.display(),
            "spawning claude session ({} -p ...)",
            self.bin
        );

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `{}`", self.bin))?;

        Ok(SessionHandle {
            worktree_path: worktree_path.to_path_buf(),
            child: Some(child),
            sim: None,
        })
    }

    async fn continue_session(&self, worktree_path: &Path, prompt: &str) -> Result<SessionHandle> {
        let mut cmd = self.base_command(worktree_path);
        cmd.arg("-c")
            .arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose");

        tracing::info!(
            worktree = %worktree_path.display(),
            "continuing claude session ({} -c -p ...)",
            self.bin
        );

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `{}`", self.bin))?;

        Ok(SessionHandle {
            worktree_path: worktree_path.to_path_buf(),
            child: Some(child),
            sim: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bin_is_claude() {
        let backend = ClaudeCodeBackend::new();
        assert_eq!(backend.bin, "claude");
    }

    // NOTE: we never spawn the real `claude` binary in tests. The spawn path is
    // exercised via MockBackend / the `true`-binary integration test instead.
}

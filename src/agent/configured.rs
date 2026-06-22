//! Project-config-driven agent backend.
//!
//! [`ConfiguredBackend`] reads the invocation recipe from [`AgentConfig`] and
//! assembles the argv via [`AgentConfig::build_args`].  It is a drop-in
//! replacement for the old `ClaudeCodeBackend` and supersedes it exactly when
//! the default [`AgentConfig`] is used.
//!
//! Spawned as:
//! ```text
//! command  args...  [prompt_arg]  prompt
//! ```
//! stdout/stderr are piped (never inherited) so agent output cannot corrupt
//! the TUI; the [`session`](super::session) runner consumes stdout for status.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::project_config::AgentConfig;

use super::{AgentBackend, SessionHandle};

/// Backend driven by a project-scoped [`AgentConfig`].
pub struct ConfiguredBackend {
    pub agent: AgentConfig,
}

impl ConfiguredBackend {
    /// Build the base [`Command`] shared by `start` / `continue_session`.
    fn base_command(&self, worktree_path: &Path) -> Command {
        let mut cmd = Command::new(&self.agent.command);
        cmd.current_dir(worktree_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }
}

#[async_trait::async_trait]
impl AgentBackend for ConfiguredBackend {
    async fn start(&self, worktree_path: &Path, prompt: &str) -> Result<SessionHandle> {
        let mut cmd = self.base_command(worktree_path);
        for arg in self.agent.build_args(false, prompt) {
            cmd.arg(arg);
        }

        tracing::info!(
            worktree = %worktree_path.display(),
            "spawning agent session ({} ...)",
            self.agent.command
        );

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `{}`", self.agent.command))?;

        Ok(SessionHandle {
            worktree_path: worktree_path.to_path_buf(),
            child: Some(child),
            sim: None,
        })
    }

    async fn continue_session(&self, worktree_path: &Path, prompt: &str) -> Result<SessionHandle> {
        let mut cmd = self.base_command(worktree_path);
        for arg in self.agent.build_args(true, prompt) {
            cmd.arg(arg);
        }

        tracing::info!(
            worktree = %worktree_path.display(),
            "continuing agent session ({} ...)",
            self.agent.command
        );

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `{}`", self.agent.command))?;

        Ok(SessionHandle {
            worktree_path: worktree_path.to_path_buf(),
            child: Some(child),
            sim: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::project_config::AgentConfig;

    /// Verify the assembled argv for a fresh session using the default config
    /// (mirrors the old ClaudeCodeBackend invocation exactly).
    #[test]
    fn default_config_fresh_argv() {
        let agent = AgentConfig::default();
        let args = agent.build_args(false, "do the thing");
        assert_eq!(
            args,
            vec![
                "--output-format",
                "stream-json",
                "--verbose",
                "-p",
                "do the thing",
            ]
        );
    }

    /// Verify the assembled argv for a resumed session using the default config.
    #[test]
    fn default_config_continue_argv() {
        let agent = AgentConfig::default();
        let args = agent.build_args(true, "keep going");
        assert_eq!(
            args,
            vec![
                "-c",
                "--output-format",
                "stream-json",
                "--verbose",
                "-p",
                "keep going",
            ]
        );
    }

    /// A custom config with positional prompt (no prompt_arg).
    #[test]
    fn custom_config_positional_prompt() {
        let agent = AgentConfig {
            command: "myagent".to_string(),
            args: vec!["--json".to_string(), "--verbose".to_string()],
            prompt_arg: None,
            continue_args: vec!["--resume".to_string(), "--json".to_string()],
        };
        let fresh = agent.build_args(false, "fresh prompt");
        assert_eq!(fresh, vec!["--json", "--verbose", "fresh prompt"]);

        let resume = agent.build_args(true, "resume prompt");
        assert_eq!(resume, vec!["--resume", "--json", "resume prompt"]);
    }
}

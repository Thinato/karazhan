//! GitHub integration via the `gh` CLI.
//!
//! All `gh` calls are routed through the [`GhRunner`] trait so tests can
//! inject a [`MockGh`] without spawning real processes or hitting the network.

pub mod ci;
pub mod commands;
pub mod pr;

use std::path::Path;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;

// ---------------------------------------------------------------------------
// GhRunner trait
// ---------------------------------------------------------------------------

/// Abstraction over running the `gh` CLI so tests can inject canned output.
///
/// Each call receives `args` (the arguments after `gh`) and `cwd` (the
/// working directory, used by `gh` to auto-detect the repo/branch).
/// Returns the trimmed stdout string on success, or an [`anyhow::Error`]
/// describing the failure (including stderr) on non-zero exit.
#[async_trait]
pub trait GhRunner: Send + Sync {
    async fn run(&self, args: &[&str], cwd: &Path) -> Result<String>;
}

// ---------------------------------------------------------------------------
// RealGh — shells out to the actual `gh` binary
// ---------------------------------------------------------------------------

/// Concrete [`GhRunner`] that delegates to the real `gh` CLI.
pub struct RealGh {
    /// Name (or absolute path) of the `gh` binary. Defaults to `"gh"`.
    pub bin: String,
}

impl RealGh {
    pub fn new() -> Self {
        Self {
            bin: "gh".to_string(),
        }
    }
}

impl Default for RealGh {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl GhRunner for RealGh {
    async fn run(&self, args: &[&str], cwd: &Path) -> Result<String> {
        let output = tokio::process::Command::new(&self.bin)
            .args(args)
            .current_dir(cwd)
            .output()
            .await
            .with_context(|| format!("failed to spawn `{} {}`", self.bin, args.join(" ")))?;

        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok(stdout)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            bail!(
                "`gh {}` failed (exit {}): {}",
                args.join(" "),
                output.status,
                if stderr.is_empty() { stdout } else { stderr }
            )
        }
    }
}

// ---------------------------------------------------------------------------
// gh_available — startup detection helper
// ---------------------------------------------------------------------------

/// Return `true` if `gh` is on PATH and responds to `--version`.
#[allow(dead_code)]
///
/// Logs a warning when absent.  Callers should degrade gracefully rather than
/// crashing when this returns `false`.
pub async fn gh_available() -> bool {
    let result = tokio::process::Command::new("gh")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;

    match result {
        Ok(s) if s.success() => true,
        Ok(s) => {
            tracing::warn!("gh --version exited with {s}; GitHub features degraded");
            false
        }
        Err(e) => {
            tracing::warn!("gh not found on PATH ({e}); GitHub features degraded");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// MockGh — test helper (only compiled in test builds)
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod mock {
    use super::*;

    /// A fake [`GhRunner`] for tests.
    ///
    /// Callers push `(arg_prefix, canned_stdout)` entries. The first entry
    /// whose `arg_prefix` matches the start of the actual `args` slice is
    /// returned. Unmatched calls return an error.
    pub struct MockGh {
        /// (expected arg substring, stdout to return)
        pub responses: Vec<(String, Result<String>)>,
    }

    impl MockGh {
        pub fn new(responses: Vec<(&str, Result<String>)>) -> Self {
            Self {
                responses: responses
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
            }
        }
    }

    #[async_trait]
    impl GhRunner for MockGh {
        async fn run(&self, args: &[&str], _cwd: &Path) -> Result<String> {
            let joined = args.join(" ");
            for (prefix, response) in &self.responses {
                if joined.contains(prefix.as_str()) {
                    return match response {
                        Ok(s) => Ok(s.clone()),
                        Err(e) => Err(anyhow::anyhow!("{e}")),
                    };
                }
            }
            Err(anyhow::anyhow!(
                "MockGh: no response registered for args: {joined}"
            ))
        }
    }
}

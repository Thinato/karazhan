//! Per-repository project configuration.
//!
//! Loaded from `<repo_root>/.karazhan/config.toml`.  A missing file yields
//! defaults silently.  A malformed file logs a warning and also yields
//! defaults — the daemon never crashes on bad project config.
//!
//! The agent is launched as:
//! ```text
//! command  args...  [prompt_arg]  prompt
//! ```
//! where `args` / `continue_args` carry ALL flags (including `--output-format
//! stream-json --verbose`) and `prompt_arg` is the flag placed immediately
//! before the prompt text (e.g. `-p`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// XDG data directory resolution
// ---------------------------------------------------------------------------

/// Return the karazhan XDG data root:
/// `$XDG_DATA_HOME/karazhan` or `$HOME/.local/share/karazhan`.
///
/// No new crate: mirrors the manual XDG pattern used in `config.rs` and
/// `projects.rs`.  Returns `None` only when both env vars are unset/empty.
pub fn karazhan_data_root() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("karazhan"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("karazhan"),
    )
}

// ---------------------------------------------------------------------------
// AgentConfig
// ---------------------------------------------------------------------------

/// Configuration for how the daemon launches the AI agent.
///
/// The full invocation is assembled as:
/// ```text
/// command  args...  [prompt_arg]  prompt
/// ```
///
/// For a fresh session `args` is used; for a resumed session `continue_args`
/// is used instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    /// Binary name or absolute path to invoke (e.g. `"claude"`).
    pub command: String,

    /// Flags for a fresh session.  Must include any streaming flags needed
    /// (e.g. `["--output-format", "stream-json", "--verbose"]`).
    pub args: Vec<String>,

    /// Flag placed immediately before the prompt text.  `None` means the
    /// prompt is passed positionally (no preceding flag).
    pub prompt_arg: Option<String>,

    /// Flags for a resumed session (e.g. `["-c", "--output-format",
    /// "stream-json", "--verbose"]`).
    pub continue_args: Vec<String>,
}

impl Default for AgentConfig {
    /// Reproduces today's exact `claude` invocation so nothing breaks when no
    /// project config file exists.
    ///
    /// Fresh:    `claude --output-format stream-json --verbose -p <prompt>`
    /// Continue: `claude -c --output-format stream-json --verbose -p <prompt>`
    fn default() -> Self {
        Self {
            command: "claude".to_string(),
            args: vec![
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
            ],
            prompt_arg: Some("-p".to_string()),
            continue_args: vec![
                "-c".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
            ],
        }
    }
}

impl AgentConfig {
    /// Build the argument list that follows `command` on the command line.
    ///
    /// Order: `base_flags...  [prompt_arg]  prompt`
    ///
    /// `resume = false` → uses `args`; `resume = true` → uses `continue_args`.
    pub fn build_args(&self, resume: bool, prompt: &str) -> Vec<String> {
        let base = if resume {
            &self.continue_args
        } else {
            &self.args
        };
        let mut out: Vec<String> = base.clone();
        if let Some(ref flag) = self.prompt_arg {
            out.push(flag.clone());
        }
        out.push(prompt.to_string());
        out
    }
}

// ---------------------------------------------------------------------------
// WorktreeSettings
// ---------------------------------------------------------------------------

/// Per-worktree behaviour configured under the `[worktree]` table.
///
/// Both fields are `Option` so a missing key is distinguishable from an
/// explicit value.  This is shared between the global [`crate::config::Config`]
/// and the per-project [`ProjectConfig`]; the daemon resolves the effective
/// values with project-over-global-over-default precedence.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeSettings {
    /// Shell command run once (via `sh -c`) when a new worktree is created.
    /// `None`/absent → no setup step.
    pub setup_command: Option<String>,

    /// Maximum time the setup command may run before it is killed.  `None`
    /// falls back to the built-in default (300 seconds).
    pub setup_timeout_seconds: Option<u64>,
}

// ---------------------------------------------------------------------------
// ProjectConfig
// ---------------------------------------------------------------------------

/// Top-level project-scoped configuration (`<repo_root>/.karazhan/config.toml`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    pub agent: AgentConfig,

    /// Base directory under which new (detached) worktrees are created.
    ///
    /// A relative path is resolved against the repo root.  When unset, the
    /// default is `<repo_root>/.karazhan/worktrees`.
    pub worktrees_dir: Option<PathBuf>,

    /// Per-worktree setup behaviour (`[worktree]` table).
    pub worktree: WorktreeSettings,
}

impl ProjectConfig {
    /// Resolve the base directory for new worktrees.
    ///
    /// - `worktrees_dir` set + absolute → used as-is.
    /// - `worktrees_dir` set + relative → resolved against `repo_root`.
    /// - `worktrees_dir` unset → `$XDG_DATA_HOME/karazhan/worktrees`
    ///   (fallback: `~/.local/share/karazhan/worktrees`).
    ///
    /// The final worktree path is always `<base>/<owner>/<project>/<uuid>`,
    /// where `<owner>` and `<project>` are applied in `new_worktree` after
    /// this function returns the base.
    pub fn worktrees_base(&self, repo_root: &Path) -> PathBuf {
        match &self.worktrees_dir {
            Some(dir) if dir.is_absolute() => dir.clone(),
            Some(dir) => repo_root.join(dir),
            None => {
                // Default: XDG data dir.
                karazhan_data_root()
                    .map(|r| r.join("worktrees"))
                    .unwrap_or_else(|| repo_root.join(".karazhan").join("worktrees"))
            }
        }
    }

    /// Load from `<repo_root>/.karazhan/config.toml`.
    ///
    /// - Missing file → defaults silently.
    /// - Malformed TOML → `tracing::warn` + defaults (never panics).
    pub fn load(repo_root: &Path) -> Self {
        let path = repo_root.join(".karazhan").join("config.toml");

        if !path.exists() {
            return Self::default();
        }

        match std::fs::read_to_string(&path) {
            Err(e) => {
                tracing::warn!("project_config: could not read {}: {e}", path.display());
                Self::default()
            }
            Ok(text) => match toml::from_str::<Self>(&text) {
                Ok(cfg) => {
                    tracing::info!("project_config: loaded from {}", path.display());
                    cfg
                }
                Err(e) => {
                    tracing::warn!(
                        "project_config: malformed TOML at {} ({e}), using defaults",
                        path.display()
                    );
                    Self::default()
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Parse a complete TOML
    // -----------------------------------------------------------------------

    #[test]
    fn parse_full_toml() {
        let toml = r#"
[agent]
command = "myagent"
args = ["--flag-a", "--flag-b"]
prompt_arg = "--prompt"
continue_args = ["--resume", "--flag-b"]
"#;
        let cfg: ProjectConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.agent.command, "myagent");
        assert_eq!(cfg.agent.args, vec!["--flag-a", "--flag-b"]);
        assert_eq!(cfg.agent.prompt_arg, Some("--prompt".to_string()));
        assert_eq!(cfg.agent.continue_args, vec!["--resume", "--flag-b"]);
    }

    // -----------------------------------------------------------------------
    // Missing file → default
    // -----------------------------------------------------------------------

    #[test]
    fn missing_file_returns_default() {
        let dir = TempDir::new().unwrap();
        let cfg = ProjectConfig::load(dir.path());
        assert_eq!(cfg.agent.command, "claude");
        assert_eq!(
            cfg.agent.args,
            vec!["--output-format", "stream-json", "--verbose"]
        );
        assert_eq!(cfg.agent.prompt_arg, Some("-p".to_string()));
        assert_eq!(
            cfg.agent.continue_args,
            vec!["-c", "--output-format", "stream-json", "--verbose"]
        );
    }

    // -----------------------------------------------------------------------
    // Malformed TOML → default (no panic)
    // -----------------------------------------------------------------------

    #[test]
    fn malformed_toml_returns_default_no_panic() {
        let dir = TempDir::new().unwrap();
        let karazhan_dir = dir.path().join(".karazhan");
        std::fs::create_dir_all(&karazhan_dir).unwrap();
        let mut f = std::fs::File::create(karazhan_dir.join("config.toml")).unwrap();
        write!(f, "this is not {{ valid toml ===").unwrap();

        let cfg = ProjectConfig::load(dir.path());
        // Must not panic and must return defaults.
        assert_eq!(cfg.agent.command, "claude");
    }

    // -----------------------------------------------------------------------
    // build_args ordering — fresh session with prompt_arg
    // -----------------------------------------------------------------------

    #[test]
    fn build_args_fresh_with_prompt_arg() {
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

    // -----------------------------------------------------------------------
    // build_args ordering — resume session with prompt_arg
    // -----------------------------------------------------------------------

    #[test]
    fn build_args_resume_with_prompt_arg() {
        let agent = AgentConfig::default();
        let args = agent.build_args(true, "continue the work");
        assert_eq!(
            args,
            vec![
                "-c",
                "--output-format",
                "stream-json",
                "--verbose",
                "-p",
                "continue the work",
            ]
        );
    }

    // -----------------------------------------------------------------------
    // build_args ordering — positional prompt (no prompt_arg)
    // -----------------------------------------------------------------------

    #[test]
    fn build_args_fresh_positional_prompt() {
        let agent = AgentConfig {
            command: "myagent".to_string(),
            args: vec!["--stream".to_string()],
            prompt_arg: None,
            continue_args: vec!["--resume".to_string()],
        };
        let args = agent.build_args(false, "my prompt");
        assert_eq!(args, vec!["--stream", "my prompt"]);
    }

    // -----------------------------------------------------------------------
    // worktrees_base resolution
    // -----------------------------------------------------------------------

    #[test]
    fn worktrees_base_default_uses_xdg_data_home() {
        // When XDG_DATA_HOME is set, the default base must be under it.
        // Use a temp env var manipulation scoped to this test thread.
        // SAFETY: single-threaded test, no other threads touching this var.
        let cfg = ProjectConfig::default();
        let root = Path::new("/repo");

        // Temporarily override env to get a deterministic path.
        std::env::set_var("XDG_DATA_HOME", "/fake/xdg");
        let base = cfg.worktrees_base(root);
        std::env::remove_var("XDG_DATA_HOME");

        assert_eq!(base, Path::new("/fake/xdg/karazhan/worktrees"));
    }

    #[test]
    fn worktrees_base_default_falls_back_to_home_local_share() {
        // Without XDG_DATA_HOME, falls back to $HOME/.local/share/karazhan/worktrees.
        let cfg = ProjectConfig::default();
        let root = Path::new("/repo");

        // Save and temporarily clear XDG_DATA_HOME.
        let saved = std::env::var("XDG_DATA_HOME").ok();
        std::env::remove_var("XDG_DATA_HOME");
        let home = std::env::var("HOME").expect("HOME must be set in test env");
        let base = cfg.worktrees_base(root);
        // Restore.
        if let Some(v) = saved {
            std::env::set_var("XDG_DATA_HOME", v);
        }

        let expected = PathBuf::from(&home)
            .join(".local")
            .join("share")
            .join("karazhan")
            .join("worktrees");
        assert_eq!(base, expected);
    }

    #[test]
    fn worktrees_base_relative_resolved_against_root() {
        let cfg = ProjectConfig {
            worktrees_dir: Some(PathBuf::from("custom/wts")),
            ..ProjectConfig::default()
        };
        let root = Path::new("/repo");
        assert_eq!(cfg.worktrees_base(root), Path::new("/repo/custom/wts"));
    }

    #[test]
    fn worktrees_base_absolute_used_as_is() {
        let cfg = ProjectConfig {
            worktrees_dir: Some(PathBuf::from("/abs/wts")),
            ..ProjectConfig::default()
        };
        let root = Path::new("/repo");
        assert_eq!(cfg.worktrees_base(root), Path::new("/abs/wts"));
    }

    #[test]
    fn worktrees_base_default_path_does_not_include_repo_root() {
        // The default base must NOT be inside the repo root.
        let cfg = ProjectConfig::default();
        let root = Path::new("/my/project/repo");

        std::env::set_var("XDG_DATA_HOME", "/fake/xdg");
        let base = cfg.worktrees_base(root);
        std::env::remove_var("XDG_DATA_HOME");

        assert!(
            !base.starts_with(root),
            "default base {base:?} must not be inside repo root {root:?}"
        );
    }

    // -----------------------------------------------------------------------
    // [worktree] table parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_worktree_table_both_keys() {
        let toml = r#"
[worktree]
setup_command = "npm install"
setup_timeout_seconds = 120
"#;
        let cfg: ProjectConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.worktree.setup_command.as_deref(), Some("npm install"));
        assert_eq!(cfg.worktree.setup_timeout_seconds, Some(120));
    }

    #[test]
    fn parse_missing_worktree_table_is_all_none() {
        let toml = r#"
[agent]
command = "claude"
"#;
        let cfg: ProjectConfig = toml::from_str(toml).expect("parse");
        assert!(cfg.worktree.setup_command.is_none());
        assert!(cfg.worktree.setup_timeout_seconds.is_none());
    }

    #[test]
    fn parse_worktree_table_partial_keys() {
        // Only the command set → timeout stays None.
        let toml = r#"
[worktree]
setup_command = "make setup"
"#;
        let cfg: ProjectConfig = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.worktree.setup_command.as_deref(), Some("make setup"));
        assert!(cfg.worktree.setup_timeout_seconds.is_none());
    }

    #[test]
    fn malformed_worktree_table_falls_back_to_default_no_panic() {
        let dir = TempDir::new().unwrap();
        let karazhan_dir = dir.path().join(".karazhan");
        std::fs::create_dir_all(&karazhan_dir).unwrap();
        let mut f = std::fs::File::create(karazhan_dir.join("config.toml")).unwrap();
        write!(f, "[worktree]\nsetup_timeout_seconds = \"not a number\"").unwrap();

        let cfg = ProjectConfig::load(dir.path());
        // Must not panic; all worktree fields default to None.
        assert!(cfg.worktree.setup_command.is_none());
        assert!(cfg.worktree.setup_timeout_seconds.is_none());
    }

    #[test]
    fn build_args_resume_positional_prompt() {
        let agent = AgentConfig {
            command: "myagent".to_string(),
            args: vec!["--stream".to_string()],
            prompt_arg: None,
            continue_args: vec!["--resume".to_string()],
        };
        let args = agent.build_args(true, "resume prompt");
        assert_eq!(args, vec!["--resume", "resume prompt"]);
    }
}

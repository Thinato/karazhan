//! Configuration loaded from `$XDG_CONFIG_HOME/karazhan/config.toml`
//! (or `~/.config/karazhan/config.toml`).
//!
//! A missing file yields defaults silently.  A malformed file logs a warning
//! and also yields defaults — the app never crashes on bad config.

use std::path::PathBuf;

use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::project_config::WorktreeSettings;
use crate::worktree::WorktreeStatus;

// ---------------------------------------------------------------------------
// ColorsConfig
// ---------------------------------------------------------------------------

/// Colour overrides for each worktree status (stored as CSS-ish colour names).
///
/// Unrecognised names fall back to [`Color::Gray`].  Supported names:
/// `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `gray`,
/// `dark_gray`, `light_red`, `light_green`, `light_yellow`, `light_blue`,
/// `light_magenta`, `light_cyan`, `white`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorsConfig {
    pub idle: String,
    pub running: String,
    pub needs_review: String,
    pub ci_failing: String,
    pub pr_merged: String,
    pub error: String,
}

impl Default for ColorsConfig {
    fn default() -> Self {
        Self {
            idle: "dark_gray".to_string(),
            running: "yellow".to_string(),
            needs_review: "magenta".to_string(),
            ci_failing: "red".to_string(),
            pr_merged: "green".to_string(),
            error: "red".to_string(),
        }
    }
}

impl ColorsConfig {
    /// Resolve a color name string to a ratatui [`Color`].
    #[allow(dead_code)] // used in tests and future grid color wiring
    ///
    /// Unknown names return [`Color::Gray`] so the app degrades visually
    /// without crashing.
    pub fn parse_color(name: &str) -> Color {
        match name.to_lowercase().as_str() {
            "black" => Color::Black,
            "red" => Color::Red,
            "green" => Color::Green,
            "yellow" => Color::Yellow,
            "blue" => Color::Blue,
            "magenta" => Color::Magenta,
            "cyan" => Color::Cyan,
            "gray" | "grey" => Color::Gray,
            "dark_gray" | "dark_grey" | "darkgray" | "darkgrey" => Color::DarkGray,
            "light_red" | "lightred" => Color::LightRed,
            "light_green" | "lightgreen" => Color::LightGreen,
            "light_yellow" | "lightyellow" => Color::LightYellow,
            "light_blue" | "lightblue" => Color::LightBlue,
            "light_magenta" | "lightmagenta" => Color::LightMagenta,
            "light_cyan" | "lightcyan" => Color::LightCyan,
            "white" => Color::White,
            _ => {
                warn!("config: unknown color name '{name}', falling back to gray");
                Color::Gray
            }
        }
    }

    /// Return the configured [`Color`] for a [`WorktreeStatus`].
    #[allow(dead_code)] // used in tests and future grid color wiring
    pub fn color_for(&self, status: &WorktreeStatus) -> Color {
        let name = match status {
            WorktreeStatus::Idle => &self.idle,
            WorktreeStatus::Running => &self.running,
            WorktreeStatus::NeedsReview => &self.needs_review,
            WorktreeStatus::CIFailing => &self.ci_failing,
            WorktreeStatus::PRMerged => &self.pr_merged,
            WorktreeStatus::Error => &self.error,
        };
        Self::parse_color(name)
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Top-level application configuration.
///
/// Loaded from `$XDG_CONFIG_HOME/karazhan/config.toml` (or
/// `~/.config/karazhan/config.toml`).  All fields have `#[serde(default)]`
/// so a partial file is always valid — missing keys take their `Default`
/// values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Background watcher polling interval in seconds (default: 30).
    pub poll_interval_secs: u64,

    /// Directory to load prompt files from.
    ///
    /// `None` → falls back to `<cwd>/prompts` (matching pre-config behaviour).
    pub prompt_dir: Option<PathBuf>,

    /// Name or absolute path of the `claude` binary (default: `"claude"`).
    pub claude_bin: String,

    /// Name or absolute path of the `gh` binary (default: `"gh"`).
    pub gh_bin: String,

    /// Prompt sent to the agent when auto-continue fires after a PR merge.
    pub auto_continue_prompt: String,

    /// Per-status colour overrides.
    pub colors: ColorsConfig,

    /// Global per-worktree setup defaults (`[worktree]` table).  Used as the
    /// fallback when a project does not set its own values.
    pub worktree: WorktreeSettings,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            poll_interval_secs: 30,
            prompt_dir: None,
            claude_bin: "claude".to_string(),
            gh_bin: "gh".to_string(),
            auto_continue_prompt:
                "The PR for this worktree was merged. Continue with the next step of the task."
                    .to_string(),
            colors: ColorsConfig::default(),
            worktree: WorktreeSettings::default(),
        }
    }
}

impl Config {
    /// Load config from the canonical path, falling back to defaults on any
    /// error (missing file, parse error, permission denied, …).
    ///
    /// Resolution order:
    /// 1. `$XDG_CONFIG_HOME/karazhan/config.toml`
    /// 2. `$HOME/.config/karazhan/config.toml`
    /// 3. Defaults (if neither env var is set or the file doesn't exist).
    pub fn load() -> Self {
        let path = Self::config_path();
        let Some(path) = path else {
            // Cannot resolve a home directory — return defaults quietly.
            return Self::default();
        };

        if !path.exists() {
            return Self::default();
        }

        match std::fs::read_to_string(&path) {
            Err(e) => {
                warn!("config: could not read {}: {e}", path.display());
                Self::default()
            }
            Ok(text) => match toml::from_str::<Self>(&text) {
                Ok(cfg) => {
                    tracing::info!("config: loaded from {}", path.display());
                    cfg
                }
                Err(e) => {
                    warn!(
                        "config: malformed TOML at {} ({e}), using defaults",
                        path.display()
                    );
                    Self::default()
                }
            },
        }
    }

    /// Resolve the canonical config file path without touching the filesystem.
    fn config_path() -> Option<PathBuf> {
        // Prefer $XDG_CONFIG_HOME if set.
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return Some(PathBuf::from(xdg).join("karazhan").join("config.toml"));
            }
        }
        // Fall back to $HOME/.config/karazhan/config.toml.
        let home = std::env::var("HOME").ok()?;
        Some(
            PathBuf::from(home)
                .join(".config")
                .join("karazhan")
                .join("config.toml"),
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // -----------------------------------------------------------------------
    // Default sanity
    // -----------------------------------------------------------------------

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::default();
        assert_eq!(cfg.poll_interval_secs, 30);
        assert_eq!(cfg.claude_bin, "claude");
        assert_eq!(cfg.gh_bin, "gh");
        assert!(!cfg.auto_continue_prompt.is_empty());
        assert!(cfg.prompt_dir.is_none());
    }

    // -----------------------------------------------------------------------
    // Parse a valid TOML snippet
    // -----------------------------------------------------------------------

    #[test]
    fn parse_valid_toml() {
        let toml = r#"
poll_interval_secs = 60
claude_bin = "/usr/local/bin/claude"
gh_bin = "/opt/homebrew/bin/gh"
prompt_dir = "/home/user/prompts"
auto_continue_prompt = "keep going"

[colors]
idle = "blue"
running = "cyan"
needs_review = "yellow"
ci_failing = "light_red"
pr_merged = "light_green"
error = "red"
"#;
        let cfg: Config = toml::from_str(toml).expect("should parse");
        assert_eq!(cfg.poll_interval_secs, 60);
        assert_eq!(cfg.claude_bin, "/usr/local/bin/claude");
        assert_eq!(cfg.gh_bin, "/opt/homebrew/bin/gh");
        assert_eq!(cfg.prompt_dir, Some(PathBuf::from("/home/user/prompts")));
        assert_eq!(cfg.auto_continue_prompt, "keep going");
        assert_eq!(cfg.colors.idle, "blue");
        assert_eq!(cfg.colors.running, "cyan");
    }

    // -----------------------------------------------------------------------
    // Partial TOML (missing keys get defaults)
    // -----------------------------------------------------------------------

    #[test]
    fn partial_toml_uses_defaults_for_missing_keys() {
        let toml = r#"poll_interval_secs = 10"#;
        let cfg: Config = toml::from_str(toml).expect("should parse");
        assert_eq!(cfg.poll_interval_secs, 10);
        // Everything else is default.
        assert_eq!(cfg.claude_bin, "claude");
        assert_eq!(cfg.gh_bin, "gh");
    }

    // -----------------------------------------------------------------------
    // Malformed TOML → default (no panic)
    // -----------------------------------------------------------------------

    #[test]
    fn malformed_toml_falls_back_to_default_no_panic() {
        let bad = "this is not { valid toml ===";
        let result = toml::from_str::<Config>(bad);
        // Expect a parse error; the caller (Config::load) converts this to defaults.
        assert!(result.is_err());
        // Confirm we can construct a default without issues.
        let cfg = Config::default();
        assert_eq!(cfg.poll_interval_secs, 30);
    }

    /// Simulate what `Config::load()` does when the file is malformed.
    #[test]
    fn load_returns_default_for_malformed_file() {
        let mut tmp = NamedTempFile::new().unwrap();
        write!(tmp, "this is not {{ valid toml ===").unwrap();

        // Replicate the load logic manually (we can't override env vars portably).
        let text = std::fs::read_to_string(tmp.path()).unwrap();
        let result = toml::from_str::<Config>(&text);
        let cfg = result.unwrap_or_else(|_| Config::default());
        assert_eq!(cfg.poll_interval_secs, 30); // default
    }

    // -----------------------------------------------------------------------
    // color_for maps known + unknown names
    // -----------------------------------------------------------------------

    #[test]
    fn color_for_known_statuses() {
        let colors = ColorsConfig::default();
        assert_eq!(colors.color_for(&WorktreeStatus::Idle), Color::DarkGray);
        assert_eq!(colors.color_for(&WorktreeStatus::Running), Color::Yellow);
        assert_eq!(
            colors.color_for(&WorktreeStatus::NeedsReview),
            Color::Magenta
        );
        assert_eq!(colors.color_for(&WorktreeStatus::CIFailing), Color::Red);
        assert_eq!(colors.color_for(&WorktreeStatus::PRMerged), Color::Green);
        assert_eq!(colors.color_for(&WorktreeStatus::Error), Color::Red);
    }

    #[test]
    fn unknown_color_name_falls_back_to_gray() {
        assert_eq!(
            ColorsConfig::parse_color("totally_unknown_color"),
            Color::Gray
        );
    }

    // -----------------------------------------------------------------------
    // Global [worktree] table parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_global_worktree_table() {
        let toml = r#"
[worktree]
setup_command = "pnpm install"
setup_timeout_seconds = 600
"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert_eq!(cfg.worktree.setup_command.as_deref(), Some("pnpm install"));
        assert_eq!(cfg.worktree.setup_timeout_seconds, Some(600));
    }

    #[test]
    fn missing_global_worktree_table_is_all_none() {
        let toml = r#"poll_interval_secs = 10"#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        assert!(cfg.worktree.setup_command.is_none());
        assert!(cfg.worktree.setup_timeout_seconds.is_none());
    }

    #[test]
    fn parse_color_case_insensitive() {
        assert_eq!(ColorsConfig::parse_color("RED"), Color::Red);
        assert_eq!(ColorsConfig::parse_color("Green"), Color::Green);
        assert_eq!(ColorsConfig::parse_color("DARK_GRAY"), Color::DarkGray);
    }

    // -----------------------------------------------------------------------
    // Config::load() with a real temp file
    // -----------------------------------------------------------------------

    #[test]
    fn load_from_valid_temp_file() {
        // We can't override XDG_CONFIG_HOME / HOME without unsafe env mutation,
        // so we test the parse path directly rather than the full load().
        let toml = r#"
poll_interval_secs = 15
claude_bin = "my-claude"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.poll_interval_secs, 15);
        assert_eq!(cfg.claude_bin, "my-claude");
        assert_eq!(cfg.gh_bin, "gh"); // default
    }
}

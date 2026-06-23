use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Lifecycle status of a worktree / agent session.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeStatus {
    #[default]
    Idle,
    Running,
    NeedsReview,
    CIFailing,
    PRMerged,
    Error,
}

/// PR status of a worktree's pull request, tracked as a SEPARATE axis from the
/// agent-activity [`WorktreeStatus`].  Auto-discovered by the watcher from the
/// worktree's current branch via `gh`.  Detached / no-branch / no-PR → `NoPr`.
#[allow(dead_code)]
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PrStatus {
    #[default]
    NoPr,
    Draft,
    Open,
    /// At least one check is still in progress (IN_PROGRESS / QUEUED / PENDING /
    /// WAITING / REQUESTED) and none have conclusively failed.
    ChecksRunning,
    ChecksFailing,
    ChecksPassing,
    /// PR is OPEN, non-draft, all checks green (or no checks), and the review
    /// decision is explicitly APPROVED.  Takes precedence over `Open` but loses
    /// to `ChecksFailing` and `ChecksRunning`.
    Approved,
    Merged,
    Closed,
}

/// Default human-facing name for a worktree (used when state.toml has no
/// `name` field — e.g. pre-existing worktrees from before names were added).
pub fn default_name() -> String {
    "Unnamed".into()
}

/// Serde default for `created_at` / `updated_at`: returns `Utc::now()`.
///
/// Legacy state.toml entries that pre-date these fields get load-time as a
/// best-effort value.  Once re-saved the persisted timestamp sticks.
pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

/// A git worktree tracked by Karazhan.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// Absolute path to the worktree on disk.
    pub path: PathBuf,
    /// Human-facing name (supervisor-managed dictionary; defaults to "Unnamed").
    #[serde(default = "default_name")]
    pub name: String,
    /// Git branch checked out in this worktree.
    pub branch: String,
    /// Slug of the prompt last used against this worktree, if any.
    pub prompt_slug: Option<String>,
    /// GitHub PR number associated with this worktree, if any.
    pub pr_number: Option<u64>,
    /// When true, the agent will auto-continue as soon as the PR is merged.
    pub auto_continue_on_merge: bool,
    /// Current lifecycle status (defaults to Idle on deserialise if missing).
    #[serde(default)]
    pub status: WorktreeStatus,
    /// PR status, auto-discovered by the watcher (separate axis from `status`).
    /// Legacy state.toml entries that lack this field deserialise as `NoPr`.
    #[serde(default)]
    pub pr_status: PrStatus,
    /// When the worktree was first created.  Serialises as RFC 3339.
    /// Legacy entries that lack this field get the load-time instant as a
    /// best-effort value; `created_at` is NEVER modified after first creation.
    #[serde(default = "now_utc")]
    pub created_at: DateTime<Utc>,
    /// When the worktree was last used (any status/name/PR/flag mutation).
    /// Serialises as RFC 3339.  Defaults to `now_utc()` for legacy entries.
    #[serde(default = "now_utc")]
    pub updated_at: DateTime<Utc>,
}

impl Worktree {
    /// Construct a minimal `Worktree` from live git data (no persisted metadata yet).
    #[allow(dead_code)]
    pub fn from_git(path: PathBuf, branch: String) -> Self {
        let now = Utc::now();
        Self {
            path,
            name: default_name(),
            branch,
            prompt_slug: None,
            pr_number: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            pr_status: PrStatus::NoPr,
            created_at: now,
            updated_at: now,
        }
    }
}

use std::path::PathBuf;

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

/// A git worktree tracked by Karazhan.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// Absolute path to the worktree on disk.
    pub path: PathBuf,
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
}

impl Worktree {
    /// Construct a minimal `Worktree` from live git data (no persisted metadata yet).
    #[allow(dead_code)]
    pub fn from_git(path: PathBuf, branch: String) -> Self {
        Self {
            path,
            branch,
            prompt_slug: None,
            pr_number: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
        }
    }
}

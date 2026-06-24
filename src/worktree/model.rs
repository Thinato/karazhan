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
    /// Worktree is being torn down (git + fs removal in progress).  Transient:
    /// the entry disappears from the next snapshot once removal completes.
    Deleting,
}

/// PR status of a worktree's pull request, tracked as a SEPARATE axis from the
/// agent-activity [`WorktreeStatus`].  Auto-discovered by the watcher from the
/// worktree's current branch via `gh`.  Detached / no-branch / no-PR → `NoPr`.
///
/// `Loading` is the initial/pre-fetch state: newly created worktrees and those
/// discovered from `git worktree list` without prior persisted state start as
/// `Loading` (cyan "loading…" in the UI) until the watcher's first poll
/// completes and transitions them to a real status.  `Loading` is NEVER returned
/// by [`crate::github::pr_status::classify`] — it is purely a pre-fetch marker.
#[allow(dead_code)]
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PrStatus {
    /// Initial/pre-fetch state: the watcher has not yet polled this worktree.
    /// Transitions to a real status on the first successful poll.
    #[default]
    Loading,
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
    /// Canonical GitHub URL for the worktree's PR, if known.
    #[serde(default)]
    pub pr_url: Option<String>,
    /// PR title, as returned by `gh pr view --json title`.
    #[serde(default)]
    pub pr_title: Option<String>,
    /// When true, the agent will auto-continue as soon as the PR is merged.
    pub auto_continue_on_merge: bool,
    /// Current lifecycle status (defaults to Idle on deserialise if missing).
    #[serde(default)]
    pub status: WorktreeStatus,
    /// PR status, auto-discovered by the watcher (separate axis from `status`).
    /// New/legacy state.toml entries that lack this field deserialise as `Loading`
    /// (the `#[default]` on `PrStatus`), which triggers a re-fetch on the next poll.
    #[serde(default)]
    pub pr_status: PrStatus,
    /// Count of UNRESOLVED PR review threads (GitHub "Resolve conversation"
    /// state), auto-discovered by the watcher for OPEN PRs.  `None` when there is
    /// no open PR or the count has not been fetched yet.
    #[serde(default)]
    pub unresolved_comments: Option<u64>,
    /// When the worktree was first created.  Serialises as RFC 3339.
    /// Legacy entries that lack this field get the load-time instant as a
    /// best-effort value; `created_at` is NEVER modified after first creation.
    #[serde(default = "now_utc")]
    pub created_at: DateTime<Utc>,
    /// When the worktree was last used (any status/name/PR/flag mutation).
    /// Serialises as RFC 3339.  Defaults to `now_utc()` for legacy entries.
    #[serde(default = "now_utc")]
    pub updated_at: DateTime<Utc>,
    /// Most recent agent `session_id` (captured from the stream-json `init`
    /// event).  Used to resume this worktree's session deterministically via
    /// `--resume <id>` instead of the ambiguous bare `-c` (which picks whatever
    /// ran last in the directory).  `None` until the first run reports one.
    #[serde(default)]
    pub session_id: Option<String>,
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
            pr_url: None,
            pr_title: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            pr_status: PrStatus::Loading,
            unresolved_comments: None,
            created_at: now,
            updated_at: now,
            session_id: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_status_default_is_loading() {
        assert_eq!(PrStatus::default(), PrStatus::Loading);
    }

    #[test]
    fn from_git_pr_status_is_loading() {
        let wt = Worktree::from_git(PathBuf::from("/tmp/wt"), "feat".to_string());
        assert_eq!(wt.pr_status, PrStatus::Loading);
    }

    #[test]
    fn serde_missing_pr_status_deserializes_to_loading() {
        // A state.toml row without a "pr_status" field (legacy or new) should
        // deserialise to Loading via the #[serde(default)] + #[default] combo.
        let toml_no_pr_status = r#"
path = "/tmp/wt"
branch = "feat"
auto_continue_on_merge = false
"#;
        let wt: Worktree = toml::from_str(toml_no_pr_status).expect("deserialize");
        assert_eq!(wt.pr_status, PrStatus::Loading);
    }

    #[test]
    fn serde_present_pr_status_loads_as_is() {
        // A persisted real status (e.g. "merged") must NOT be overridden by Loading.
        let toml_merged = r#"
path = "/tmp/wt"
branch = "feat"
auto_continue_on_merge = false
pr_status = "merged"
"#;
        let wt: Worktree = toml::from_str(toml_merged).expect("deserialize");
        assert_eq!(wt.pr_status, PrStatus::Merged);
    }

    #[test]
    fn serde_missing_unresolved_comments_defaults_to_none() {
        // A state.toml row without `unresolved_comments` deserialises to None.
        let toml_no_field = r#"
path = "/tmp/wt"
branch = "feat"
auto_continue_on_merge = false
"#;
        let wt: Worktree = toml::from_str(toml_no_field).expect("deserialize");
        assert_eq!(wt.unresolved_comments, None);
    }

    #[test]
    fn serde_present_unresolved_comments_loads() {
        let toml_with_field = r#"
path = "/tmp/wt"
branch = "feat"
auto_continue_on_merge = false
unresolved_comments = 3
"#;
        let wt: Worktree = toml::from_str(toml_with_field).expect("deserialize");
        assert_eq!(wt.unresolved_comments, Some(3));
    }

    #[test]
    fn serde_loading_round_trips() {
        let toml_loading = r#"
path = "/tmp/wt"
branch = "feat"
auto_continue_on_merge = false
pr_status = "loading"
"#;
        let wt: Worktree = toml::from_str(toml_loading).expect("deserialize");
        assert_eq!(wt.pr_status, PrStatus::Loading);
    }
}

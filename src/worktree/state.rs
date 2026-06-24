#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::model::{Worktree, WorktreeStatus};

// ---------------------------------------------------------------------------
// State file format
// ---------------------------------------------------------------------------

/// Persisted state written to `.karazhan/state.toml` under the repo root.
#[allow(dead_code)]
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub worktrees: Vec<Worktree>,
}

// ---------------------------------------------------------------------------
// Path helper
// ---------------------------------------------------------------------------

fn state_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".karazhan").join("state.toml")
}

// ---------------------------------------------------------------------------
// Load / Save
// ---------------------------------------------------------------------------

/// Load state from `<repo_root>/.karazhan/state.toml`.
///
/// A missing file is treated as an empty `State`, not an error.
pub fn load(repo_root: &Path) -> Result<State> {
    let path = state_path(repo_root);

    if !path.exists() {
        return Ok(State::default());
    }

    let content =
        std::fs::read_to_string(&path).with_context(|| format!("cannot read {:?}", path))?;

    let state: State =
        toml::from_str(&content).with_context(|| format!("invalid TOML in {:?}", path))?;

    Ok(state)
}

/// Atomically write `state` to `<repo_root>/.karazhan/state.toml`.
///
/// Creates the `.karazhan/` directory if it does not exist.
/// Uses a temp-file + rename approach to avoid partial writes.
pub fn save(repo_root: &Path, state: &State) -> Result<()> {
    let dir = repo_root.join(".karazhan");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create state dir {:?}", dir))?;

    let final_path = dir.join("state.toml");

    // Write to a sibling temp file first, then rename atomically.
    let tmp_path = dir.join("state.toml.tmp");
    let content = toml::to_string_pretty(state).context("cannot serialise state to TOML")?;
    std::fs::write(&tmp_path, &content)
        .with_context(|| format!("cannot write temp state file {:?}", tmp_path))?;
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("cannot rename {:?} -> {:?}", tmp_path, final_path))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Mutating helpers (operate on an in-memory State)
// ---------------------------------------------------------------------------

impl State {
    /// Insert or replace the entry keyed by `worktree.path`.
    pub fn upsert_worktree(&mut self, worktree: Worktree) {
        if let Some(existing) = self.worktrees.iter_mut().find(|w| w.path == worktree.path) {
            *existing = worktree;
        } else {
            self.worktrees.push(worktree);
        }
    }

    /// Remove the entry whose path matches `path`.  No-op if not found.
    pub fn remove_worktree(&mut self, path: &Path) {
        self.worktrees.retain(|w| w.path != path);
    }

    /// Bump `updated_at` to now for the worktree at `path`.  No-op if absent.
    pub fn touch(&mut self, path: &Path) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.updated_at = Utc::now();
        }
    }

    /// Update the human-facing name for a worktree identified by `path`.
    /// No-op if not found.  Also bumps `updated_at`.
    pub fn set_name(&mut self, path: &Path, name: impl Into<String>) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.name = name.into();
            w.updated_at = Utc::now();
        }
    }

    /// Update the status for a worktree identified by `path`.  No-op if not found.
    /// Also bumps `updated_at`.
    pub fn set_status(&mut self, path: &Path, status: WorktreeStatus) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.status = status;
            w.updated_at = Utc::now();
        }
    }

    /// Update the auto-continue flag for a worktree identified by `path`.
    /// Also bumps `updated_at`.
    pub fn set_auto_continue(&mut self, path: &Path, value: bool) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.auto_continue_on_merge = value;
            w.updated_at = Utc::now();
        }
    }

    /// Update the PR number for a worktree identified by `path`.
    /// Also bumps `updated_at`.
    pub fn set_pr_number(&mut self, path: &Path, pr: Option<u64>) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.pr_number = pr;
            w.updated_at = Utc::now();
        }
    }

    /// Update the PR status for a worktree identified by `path`.  No-op if not
    /// found.  Does NOT bump `updated_at` — polling is not user/agent activity.
    pub fn set_pr_status(&mut self, path: &Path, pr_status: crate::worktree::model::PrStatus) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.pr_status = pr_status;
        }
    }

    /// Set the PR number for a worktree WITHOUT bumping `updated_at` (used by the
    /// poller, which is not user/agent activity).  No-op if not found.
    pub fn set_pr_number_no_touch(&mut self, path: &Path, pr: Option<u64>) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.pr_number = pr;
        }
    }

    /// Set the PR URL for a worktree WITHOUT bumping `updated_at` (used by the
    /// poller).  No-op if not found.
    pub fn set_pr_url_no_touch(&mut self, path: &Path, url: Option<String>) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.pr_url = url;
        }
    }

    /// Set the PR title for a worktree WITHOUT bumping `updated_at` (used by the
    /// poller).  No-op if not found.
    pub fn set_pr_title_no_touch(&mut self, path: &Path, title: Option<String>) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.pr_title = title;
        }
    }

    /// Set the unresolved-review-comment count for a worktree WITHOUT bumping
    /// `updated_at` (used by the poller — polling is not user/agent activity).
    /// No-op if not found.
    pub fn set_unresolved_no_touch(&mut self, path: &Path, unresolved: Option<u64>) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.unresolved_comments = unresolved;
        }
    }

    /// Record the agent `session_id` for a worktree WITHOUT bumping `updated_at`
    /// (the id is captured mid-run from the stream, not a user action).  No-op if
    /// not found.
    pub fn set_session_id_no_touch(&mut self, path: &Path, session_id: Option<String>) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.session_id = session_id;
        }
    }

    /// Prune any state entries whose paths are not in `live_paths`.
    ///
    /// Called after `git worktree list` so orphaned entries are removed.
    pub fn prune_missing(&mut self, live_paths: &[PathBuf]) {
        self.worktrees.retain(|w| live_paths.contains(&w.path));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn make_worktree(path: impl Into<PathBuf>, branch: &str) -> Worktree {
        let now = Utc::now();
        Worktree {
            path: path.into(),
            name: "Unnamed".to_string(),
            branch: branch.to_string(),
            prompt_slug: Some("my-prompt".to_string()),
            pr_number: Some(42),
            pr_url: None,
            pr_title: None,
            auto_continue_on_merge: true,
            status: WorktreeStatus::NeedsReview,
            pr_status: crate::worktree::model::PrStatus::NoPr,
            unresolved_comments: None,
            created_at: now,
            updated_at: now,
            session_id: None,
        }
    }

    #[test]
    fn state_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path();

        let wt = make_worktree("/tmp/wt-a", "feature-a");
        let mut state = State::default();
        state.upsert_worktree(wt.clone());

        save(repo_root, &state).expect("save");
        let loaded = load(repo_root).expect("load");

        assert_eq!(loaded.worktrees.len(), 1);
        let got = &loaded.worktrees[0];
        assert_eq!(got.path, wt.path);
        assert_eq!(got.branch, wt.branch);
        assert_eq!(got.prompt_slug, wt.prompt_slug);
        assert_eq!(got.pr_number, wt.pr_number);
        assert_eq!(got.auto_continue_on_merge, wt.auto_continue_on_merge);
        assert_eq!(got.status, wt.status);
        // Timestamps round-trip through TOML (RFC 3339 with second precision).
        assert_eq!(
            got.created_at.timestamp(),
            wt.created_at.timestamp(),
            "created_at round-trip"
        );
        assert_eq!(
            got.updated_at.timestamp(),
            wt.updated_at.timestamp(),
            "updated_at round-trip"
        );
    }

    #[test]
    fn set_name_persists_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path();

        let path = PathBuf::from("/tmp/wt-name");
        let mut state = State::default();
        state.upsert_worktree(make_worktree(&path, "feature-a"));
        state.set_name(&path, "shiny-name");
        save(repo_root, &state).expect("save");

        let loaded = load(repo_root).expect("load");
        assert_eq!(loaded.worktrees[0].name, "shiny-name");
    }

    #[test]
    fn set_session_id_persists_round_trip_without_touching_updated_at() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path();

        let path = PathBuf::from("/tmp/wt-session");
        let mut state = State::default();
        state.upsert_worktree(make_worktree(&path, "feature-a"));
        let before = state.worktrees[0].updated_at;

        state.set_session_id_no_touch(&path, Some("sess-xyz".to_string()));
        assert_eq!(
            state.worktrees[0].updated_at, before,
            "no_touch must not bump updated_at"
        );
        save(repo_root, &state).expect("save");

        let loaded = load(repo_root).expect("load");
        assert_eq!(loaded.worktrees[0].session_id.as_deref(), Some("sess-xyz"));
    }

    #[test]
    fn missing_name_defaults_to_unnamed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path();
        let karazhan_dir = repo_root.join(".karazhan");
        std::fs::create_dir_all(&karazhan_dir).expect("mkdir");

        // A state.toml entry that predates the `name` field (no `name` key).
        let toml = "[[worktrees]]\n\
                    path = \"/tmp/legacy-wt\"\n\
                    branch = \"legacy\"\n\
                    auto_continue_on_merge = false\n\
                    status = \"idle\"\n";
        std::fs::write(karazhan_dir.join("state.toml"), toml).expect("write");

        let loaded = load(repo_root).expect("load");
        assert_eq!(loaded.worktrees.len(), 1);
        assert_eq!(loaded.worktrees[0].name, "Unnamed");
    }

    #[test]
    fn legacy_toml_without_timestamps_loads_with_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path();
        let karazhan_dir = repo_root.join(".karazhan");
        std::fs::create_dir_all(&karazhan_dir).expect("mkdir");

        // A state.toml entry that predates `created_at` / `updated_at`.
        let toml = "[[worktrees]]\n\
                    path = \"/tmp/legacy-ts-wt\"\n\
                    branch = \"legacy\"\n\
                    name = \"OldName\"\n\
                    auto_continue_on_merge = false\n\
                    status = \"idle\"\n";
        std::fs::write(karazhan_dir.join("state.toml"), toml).expect("write");

        let before = Utc::now();
        let loaded = load(repo_root).expect("load — must not panic on missing timestamps");
        let after = Utc::now();

        assert_eq!(loaded.worktrees.len(), 1);
        let wt = &loaded.worktrees[0];
        // Defaults are generated at load-time so they should be very close to now.
        assert!(
            wt.created_at >= before && wt.created_at <= after,
            "created_at default should be ~now"
        );
        assert!(
            wt.updated_at >= before && wt.updated_at <= after,
            "updated_at default should be ~now"
        );
    }

    #[test]
    fn missing_file_returns_empty_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = load(dir.path()).expect("load");
        assert!(state.worktrees.is_empty());
    }

    #[test]
    fn upsert_replaces_existing() {
        let mut state = State::default();
        let path = PathBuf::from("/tmp/wt-x");
        let now = Utc::now();
        state.upsert_worktree(make_worktree(&path, "branch-1"));
        state.upsert_worktree(Worktree {
            path: path.clone(),
            name: "Unnamed".to_string(),
            branch: "branch-2".to_string(),
            prompt_slug: None,
            pr_number: None,
            pr_url: None,
            pr_title: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            pr_status: crate::worktree::model::PrStatus::NoPr,
            unresolved_comments: None,
            created_at: now,
            updated_at: now,
            session_id: None,
        });
        assert_eq!(state.worktrees.len(), 1);
        assert_eq!(state.worktrees[0].branch, "branch-2");
    }

    #[test]
    fn remove_worktree_by_path() {
        let mut state = State::default();
        let p1 = PathBuf::from("/tmp/wt-1");
        let p2 = PathBuf::from("/tmp/wt-2");
        state.upsert_worktree(make_worktree(&p1, "a"));
        state.upsert_worktree(make_worktree(&p2, "b"));
        state.remove_worktree(&p1);
        assert_eq!(state.worktrees.len(), 1);
        assert_eq!(state.worktrees[0].path, p2);
    }

    #[test]
    fn prune_missing_removes_orphans() {
        let mut state = State::default();
        let p1 = PathBuf::from("/tmp/live");
        let p2 = PathBuf::from("/tmp/dead");
        state.upsert_worktree(make_worktree(&p1, "live"));
        state.upsert_worktree(make_worktree(&p2, "dead"));
        state.prune_missing(std::slice::from_ref(&p1));
        assert_eq!(state.worktrees.len(), 1);
        assert_eq!(state.worktrees[0].path, p1);
    }

    #[test]
    fn touch_bumps_updated_at_not_created_at() {
        let mut state = State::default();
        let path = PathBuf::from("/tmp/touch-wt");
        let past: DateTime<Utc> = "2020-01-01T00:00:00Z".parse().unwrap();
        let wt = Worktree {
            path: path.clone(),
            name: "Unnamed".to_string(),
            branch: "main".to_string(),
            prompt_slug: None,
            pr_number: None,
            pr_url: None,
            pr_title: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            pr_status: crate::worktree::model::PrStatus::NoPr,
            unresolved_comments: None,
            created_at: past,
            updated_at: past,
            session_id: None,
        };
        state.upsert_worktree(wt);

        let before = Utc::now();
        state.touch(&path);
        let after = Utc::now();

        let w = &state.worktrees[0];
        assert_eq!(w.created_at, past, "touch must not modify created_at");
        assert!(
            w.updated_at >= before && w.updated_at <= after,
            "touch must bump updated_at to ~now"
        );
    }

    #[test]
    fn set_status_bumps_updated_at_not_created_at() {
        let mut state = State::default();
        let path = PathBuf::from("/tmp/setstatus-wt");
        let past: DateTime<Utc> = "2020-06-15T12:00:00Z".parse().unwrap();
        state.upsert_worktree(Worktree {
            path: path.clone(),
            name: "Unnamed".to_string(),
            branch: "feat".to_string(),
            prompt_slug: None,
            pr_number: None,
            pr_url: None,
            pr_title: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            pr_status: crate::worktree::model::PrStatus::NoPr,
            unresolved_comments: None,
            created_at: past,
            updated_at: past,
            session_id: None,
        });

        let before = Utc::now();
        state.set_status(&path, WorktreeStatus::Running);
        let after = Utc::now();

        let w = &state.worktrees[0];
        assert_eq!(w.created_at, past, "set_status must not modify created_at");
        assert!(
            w.updated_at >= before && w.updated_at <= after,
            "set_status must bump updated_at"
        );
    }

    #[test]
    fn set_name_bumps_updated_at_not_created_at() {
        let mut state = State::default();
        let path = PathBuf::from("/tmp/setname-wt");
        let past: DateTime<Utc> = "2019-03-10T08:00:00Z".parse().unwrap();
        state.upsert_worktree(Worktree {
            path: path.clone(),
            name: "Old".to_string(),
            branch: "main".to_string(),
            prompt_slug: None,
            pr_number: None,
            pr_url: None,
            pr_title: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            pr_status: crate::worktree::model::PrStatus::NoPr,
            unresolved_comments: None,
            created_at: past,
            updated_at: past,
            session_id: None,
        });

        let before = Utc::now();
        state.set_name(&path, "New");
        let after = Utc::now();

        let w = &state.worktrees[0];
        assert_eq!(w.created_at, past, "set_name must not modify created_at");
        assert!(
            w.updated_at >= before && w.updated_at <= after,
            "set_name must bump updated_at"
        );
    }

    #[test]
    fn set_auto_continue_bumps_updated_at_not_created_at() {
        let mut state = State::default();
        let path = PathBuf::from("/tmp/setac-wt");
        let past: DateTime<Utc> = "2021-11-20T00:00:00Z".parse().unwrap();
        state.upsert_worktree(Worktree {
            path: path.clone(),
            name: "Unnamed".to_string(),
            branch: "main".to_string(),
            prompt_slug: None,
            pr_number: None,
            pr_url: None,
            pr_title: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            pr_status: crate::worktree::model::PrStatus::NoPr,
            unresolved_comments: None,
            created_at: past,
            updated_at: past,
            session_id: None,
        });

        let before = Utc::now();
        state.set_auto_continue(&path, true);
        let after = Utc::now();

        let w = &state.worktrees[0];
        assert_eq!(
            w.created_at, past,
            "set_auto_continue must not modify created_at"
        );
        assert!(
            w.updated_at >= before && w.updated_at <= after,
            "set_auto_continue must bump updated_at"
        );
    }

    #[test]
    fn set_unresolved_no_touch_round_trips_and_does_not_bump() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path();
        let path = PathBuf::from("/tmp/unresolved-state-wt");
        let past: DateTime<Utc> = "2023-05-01T00:00:00Z".parse().unwrap();

        let mut state = State::default();
        state.upsert_worktree(Worktree {
            path: path.clone(),
            name: "Unnamed".to_string(),
            branch: "feat".to_string(),
            prompt_slug: None,
            pr_number: None,
            pr_url: None,
            pr_title: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            pr_status: crate::worktree::model::PrStatus::Open,
            unresolved_comments: None,
            created_at: past,
            updated_at: past,
            session_id: None,
        });

        state.set_unresolved_no_touch(&path, Some(4));

        // updated_at must NOT move (polling is not user activity).
        let w = &state.worktrees[0];
        assert_eq!(w.unresolved_comments, Some(4));
        assert_eq!(
            w.updated_at, past,
            "set_unresolved_no_touch must not bump updated_at"
        );

        // Persists and round-trips through TOML.
        save(repo_root, &state).expect("save");
        let loaded = load(repo_root).expect("load");
        assert_eq!(loaded.worktrees[0].unresolved_comments, Some(4));
    }

    #[test]
    fn set_pr_number_bumps_updated_at_not_created_at() {
        let mut state = State::default();
        let path = PathBuf::from("/tmp/setpr-wt");
        let past: DateTime<Utc> = "2022-07-04T16:00:00Z".parse().unwrap();
        state.upsert_worktree(Worktree {
            path: path.clone(),
            name: "Unnamed".to_string(),
            branch: "feat".to_string(),
            prompt_slug: None,
            pr_number: None,
            pr_url: None,
            pr_title: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
            pr_status: crate::worktree::model::PrStatus::NoPr,
            unresolved_comments: None,
            created_at: past,
            updated_at: past,
            session_id: None,
        });

        let before = Utc::now();
        state.set_pr_number(&path, Some(99));
        let after = Utc::now();

        let w = &state.worktrees[0];
        assert_eq!(
            w.created_at, past,
            "set_pr_number must not modify created_at"
        );
        assert!(
            w.updated_at >= before && w.updated_at <= after,
            "set_pr_number must bump updated_at"
        );
    }
}

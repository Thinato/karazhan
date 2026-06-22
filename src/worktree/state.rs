#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
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

    /// Update the human-facing name for a worktree identified by `path`.
    /// No-op if not found.
    pub fn set_name(&mut self, path: &Path, name: impl Into<String>) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.name = name.into();
        }
    }

    /// Update the status for a worktree identified by `path`.  No-op if not found.
    pub fn set_status(&mut self, path: &Path, status: WorktreeStatus) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.status = status;
        }
    }

    /// Update the auto-continue flag for a worktree identified by `path`.
    pub fn set_auto_continue(&mut self, path: &Path, value: bool) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.auto_continue_on_merge = value;
        }
    }

    /// Update the PR number for a worktree identified by `path`.
    pub fn set_pr_number(&mut self, path: &Path, pr: Option<u64>) {
        if let Some(w) = self.worktrees.iter_mut().find(|w| w.path == path) {
            w.pr_number = pr;
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

    fn make_worktree(path: impl Into<PathBuf>, branch: &str) -> Worktree {
        Worktree {
            path: path.into(),
            name: "Unnamed".to_string(),
            branch: branch.to_string(),
            prompt_slug: Some("my-prompt".to_string()),
            pr_number: Some(42),
            auto_continue_on_merge: true,
            status: WorktreeStatus::NeedsReview,
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
    fn missing_file_returns_empty_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = load(dir.path()).expect("load");
        assert!(state.worktrees.is_empty());
    }

    #[test]
    fn upsert_replaces_existing() {
        let mut state = State::default();
        let path = PathBuf::from("/tmp/wt-x");
        state.upsert_worktree(make_worktree(&path, "branch-1"));
        state.upsert_worktree(Worktree {
            path: path.clone(),
            name: "Unnamed".to_string(),
            branch: "branch-2".to_string(),
            prompt_slug: None,
            pr_number: None,
            auto_continue_on_merge: false,
            status: WorktreeStatus::Idle,
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
}

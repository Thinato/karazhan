#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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
// Per-root serialization
// ---------------------------------------------------------------------------

/// Process-wide lock table keyed by (canonicalized) repo root.
///
/// The daemon runs many concurrent load→mutate→save tasks against the same
/// state file; without serialization two tasks can read the same base state
/// and one clobbers the other's update.  The critical section is pure sync
/// filesystem work (no `.await` while held), so a std mutex is safe from
/// async callers.
static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn lock_for(repo_root: &Path) -> Arc<Mutex<()>> {
    let key = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let table = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = table.lock().unwrap_or_else(|e| e.into_inner());
    Arc::clone(map.entry(key).or_default())
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

/// Load state, degrading gracefully when the file is unreadable or corrupt.
///
/// A worktree's *existence* comes from live git output; this file only holds
/// metadata, so a corrupt state file must never take a whole project down.
/// On failure the bad file is renamed aside to `state.toml.corrupt` (so it can
/// be inspected and so the next save starts clean) and an empty `State` is
/// returned.
pub fn load_or_recover(repo_root: &Path) -> State {
    match load(repo_root) {
        Ok(state) => state,
        Err(e) => {
            let path = state_path(repo_root);
            let quarantine = path.with_extension("toml.corrupt");
            tracing::warn!(
                "state: cannot load {:?} ({e:#}); moving it to {:?} and starting fresh",
                path,
                quarantine
            );
            if let Err(rename_err) = std::fs::rename(&path, &quarantine) {
                tracing::warn!("state: failed to quarantine corrupt state file: {rename_err}");
            }
            State::default()
        }
    }
}

/// Atomically write `state` to `<repo_root>/.karazhan/state.toml`.
///
/// Creates the `.karazhan/` directory if it does not exist.
/// Uses a temp-file + rename approach to avoid partial writes.  The temp file
/// name is unique per writer (pid + sequence number): a fixed shared name lets
/// two concurrent savers interleave their writes into one temp file and
/// publish a spliced, unparseable hybrid.
pub fn save(repo_root: &Path, state: &State) -> Result<()> {
    static SAVE_SEQ: AtomicU64 = AtomicU64::new(0);

    let dir = repo_root.join(".karazhan");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create state dir {:?}", dir))?;

    let final_path = dir.join("state.toml");

    // Write to a sibling temp file first, then rename atomically.
    let tmp_path = dir.join(format!(
        "state.toml.{}.{}.tmp",
        std::process::id(),
        SAVE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let content = toml::to_string_pretty(state).context("cannot serialise state to TOML")?;
    std::fs::write(&tmp_path, &content)
        .with_context(|| format!("cannot write temp state file {:?}", tmp_path))?;
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("cannot rename {:?} -> {:?}", tmp_path, final_path))?;

    Ok(())
}

/// Serialized read-modify-write of a project's state file.
///
/// Takes the per-root lock, loads (recovering from corruption if needed),
/// applies `mutate`, saves, and returns the resulting state.  All daemon-side
/// state mutations must go through here so concurrent tasks cannot lose each
/// other's updates.
pub fn update<F: FnOnce(&mut State)>(repo_root: &Path, mutate: F) -> Result<State> {
    let lock = lock_for(repo_root);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());

    let mut state = load_or_recover(repo_root);
    mutate(&mut state);
    save(repo_root, &state)?;
    Ok(state)
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
    fn load_or_recover_quarantines_corrupt_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path();
        let karazhan_dir = repo_root.join(".karazhan");
        std::fs::create_dir_all(&karazhan_dir).expect("mkdir");

        // A spliced file like the one produced by the temp-path race:
        // valid prefix + orphaned tail of a longer previous version
        // (duplicate key → parse error).
        let corrupt = "[[worktrees]]\n\
                       path = \"/tmp/wt-a\"\n\
                       branch = \"feat\"\n\
                       status = \"idle\"\n\
                       atus = \"running\"\n\
                       status = \"running\"\n";
        std::fs::write(karazhan_dir.join("state.toml"), corrupt).expect("write");

        let state = load_or_recover(repo_root);
        assert!(state.worktrees.is_empty(), "corrupt file → default state");
        assert!(
            !karazhan_dir.join("state.toml").exists(),
            "corrupt file must be moved aside"
        );
        assert!(
            karazhan_dir.join("state.toml.corrupt").exists(),
            "corrupt file must be preserved for inspection"
        );

        // A subsequent save + load round-trips normally.
        save(repo_root, &state).expect("save after recovery");
        assert!(load(repo_root).expect("load").worktrees.is_empty());
    }

    #[test]
    fn concurrent_updates_never_corrupt_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo_root = dir.path().to_path_buf();

        // Seed with one entry so every update rewrites real content.
        let mut seed = State::default();
        seed.upsert_worktree(make_worktree("/tmp/seed", "seed"));
        save(&repo_root, &seed).expect("seed save");

        let threads: Vec<_> = (0..8)
            .map(|t| {
                let root = repo_root.clone();
                std::thread::spawn(move || {
                    for i in 0..50 {
                        update(&root, |st| {
                            st.upsert_worktree(make_worktree(format!("/tmp/wt-{t}-{i}"), "stress"));
                        })
                        .expect("update");
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("thread");
        }

        // The file must parse, contain every update (no lost writes), and no
        // temp litter may remain.
        let final_state = load(&repo_root).expect("final file must parse");
        assert_eq!(
            final_state.worktrees.len(),
            1 + 8 * 50,
            "every concurrent update must be retained"
        );
        let leftovers: Vec<_> = std::fs::read_dir(repo_root.join(".karazhan"))
            .expect("read dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
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

//! Persisted PR/CI status written by the standalone watcher process and read by
//! the session daemon.
//!
//! This file is the hand-off channel that lets the GitHub-polling watcher live
//! in its OWN process, separate from the session daemon.  The watcher owns
//! `<project_root>/.karazhan/pr_status.toml`; the daemon only READS it when
//! building snapshots.  Keeping it a distinct file from `state.toml` (owned by
//! the daemon) means the two processes never contend on the same file — no lock
//! is needed.
//!
//! # Tolerance
//! [`load`] NEVER errors: a missing or malformed file yields a default (empty)
//! [`PrStatusFile`], mirroring [`crate::config::Config::load`].  The daemon reads
//! this on a hot path and must never panic or fail on a torn/garbage file — the
//! atomic temp-file + rename in [`save`] makes torn reads impossible in practice,
//! but a corrupt file (e.g. disk-full mid-write on an older build) still degrades
//! gracefully to "no PR info yet".

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::worktree::model::PrStatus;

// ---------------------------------------------------------------------------
// File format
// ---------------------------------------------------------------------------

/// One worktree's PR/CI status, as observed by the watcher's most recent poll.
///
/// Mirrors the PR-axis fields of [`crate::ipc::WorktreeView`] so the daemon can
/// fold these straight into the view it broadcasts, with no wire-format change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStatusEntry {
    /// Absolute path of the worktree this status belongs to (the join key).
    pub path: PathBuf,
    /// PR status observed by the watcher's last poll.
    #[serde(default)]
    pub pr_status: PrStatus,
    /// GitHub PR number, if the branch has an open/known PR.
    #[serde(default)]
    pub pr_number: Option<u64>,
    /// Canonical PR URL, if known.
    #[serde(default)]
    pub pr_url: Option<String>,
    /// PR title, if known.
    #[serde(default)]
    pub pr_title: Option<String>,
    /// Count of UNRESOLVED review threads (open PRs only); `None` otherwise.
    #[serde(default)]
    pub unresolved_comments: Option<u64>,
    /// When the watcher last wrote this entry.  Serialises as RFC 3339.
    #[serde(default = "crate::worktree::model::now_utc")]
    pub updated_at: DateTime<Utc>,
}

/// The whole `pr_status.toml` document: one entry per worktree in the project.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrStatusFile {
    #[serde(default)]
    pub worktree: Vec<PrStatusEntry>,
}

impl PrStatusFile {
    /// Insert or replace the entry keyed by `entry.path`.
    pub fn upsert(&mut self, entry: PrStatusEntry) {
        if let Some(existing) = self.worktree.iter_mut().find(|e| e.path == entry.path) {
            *existing = entry;
        } else {
            self.worktree.push(entry);
        }
    }

    /// Look up the entry for `path`, if present.
    pub fn get(&self, path: &Path) -> Option<&PrStatusEntry> {
        self.worktree.iter().find(|e| e.path == path)
    }

    /// Drop any entries whose path is not in `live_paths` (called after a
    /// worktree-set rebuild so orphaned rows are pruned).
    pub fn prune_missing(&mut self, live_paths: &[PathBuf]) {
        self.worktree.retain(|e| live_paths.contains(&e.path));
    }
}

// ---------------------------------------------------------------------------
// Path helper
// ---------------------------------------------------------------------------

/// Path to a project's PR-status file: `<project_root>/.karazhan/pr_status.toml`.
pub fn pr_status_path(project_root: &Path) -> PathBuf {
    project_root.join(".karazhan").join("pr_status.toml")
}

// ---------------------------------------------------------------------------
// Load / Save
// ---------------------------------------------------------------------------

/// Load the PR-status file for `project_root`.
///
/// NEVER errors: a missing file or any read/parse failure yields the default
/// (empty) [`PrStatusFile`], logging a `warn` on genuine corruption so the daemon
/// stays alive.  A missing file is the normal cold-start case (no `warn`).
pub fn load(project_root: &Path) -> PrStatusFile {
    let path = pr_status_path(project_root);

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PrStatusFile::default(),
        Err(e) => {
            tracing::warn!("pr_status: cannot read {}: {e}", path.display());
            return PrStatusFile::default();
        }
    };

    match toml::from_str(&content) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!(
                "pr_status: invalid TOML in {} (ignoring): {e}",
                path.display()
            );
            PrStatusFile::default()
        }
    }
}

/// Atomically write `file` to `<project_root>/.karazhan/pr_status.toml`.
///
/// Creates `.karazhan/` if needed and uses a temp-file + rename so the daemon
/// (the reader) never observes a partially-written file.
pub fn save(project_root: &Path, file: &PrStatusFile) -> Result<()> {
    let dir = project_root.join(".karazhan");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create pr_status dir {:?}", dir))?;

    let final_path = dir.join("pr_status.toml");
    let tmp_path = dir.join("pr_status.toml.tmp");

    let content = toml::to_string_pretty(file).context("cannot serialise pr_status to TOML")?;
    std::fs::write(&tmp_path, &content)
        .with_context(|| format!("cannot write temp pr_status file {:?}", tmp_path))?;
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("cannot rename {:?} -> {:?}", tmp_path, final_path))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, status: PrStatus, pr: Option<u64>) -> PrStatusEntry {
        PrStatusEntry {
            path: PathBuf::from(path),
            pr_status: status,
            pr_number: pr,
            pr_url: pr.map(|n| format!("https://github.com/o/r/pull/{n}")),
            pr_title: pr.map(|_| "A PR".to_string()),
            unresolved_comments: None,
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn round_trip_save_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        let mut file = PrStatusFile::default();
        file.upsert(entry("/tmp/wt-a", PrStatus::Open, Some(42)));
        file.upsert(PrStatusEntry {
            unresolved_comments: Some(3),
            ..entry("/tmp/wt-b", PrStatus::ChecksFailing, Some(7))
        });

        save(root, &file).expect("save");
        let loaded = load(root);

        assert_eq!(loaded, file);
        assert_eq!(loaded.worktree.len(), 2);
        let a = loaded.get(Path::new("/tmp/wt-a")).expect("entry a");
        assert_eq!(a.pr_status, PrStatus::Open);
        assert_eq!(a.pr_number, Some(42));
        let b = loaded.get(Path::new("/tmp/wt-b")).expect("entry b");
        assert_eq!(b.unresolved_comments, Some(3));
    }

    #[test]
    fn missing_file_returns_default_no_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let loaded = load(dir.path());
        assert_eq!(loaded, PrStatusFile::default());
        assert!(loaded.worktree.is_empty());
    }

    #[test]
    fn malformed_file_returns_default_no_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let karazhan = dir.path().join(".karazhan");
        std::fs::create_dir_all(&karazhan).expect("mkdir");
        std::fs::write(karazhan.join("pr_status.toml"), "this = is not [valid toml")
            .expect("write garbage");

        // Must not panic and must degrade to empty.
        let loaded = load(dir.path());
        assert_eq!(loaded, PrStatusFile::default());
    }

    #[test]
    fn save_is_atomic_no_tmp_left_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let mut file = PrStatusFile::default();
        file.upsert(entry("/tmp/wt", PrStatus::Merged, Some(1)));
        save(root, &file).expect("save");

        let tmp = root.join(".karazhan").join("pr_status.toml.tmp");
        assert!(
            !tmp.exists(),
            "temp file must be renamed away, not left behind"
        );
        assert!(root.join(".karazhan").join("pr_status.toml").exists());
    }

    #[test]
    fn upsert_replaces_and_get_finds() {
        let mut file = PrStatusFile::default();
        file.upsert(entry("/tmp/wt", PrStatus::Open, Some(1)));
        file.upsert(entry("/tmp/wt", PrStatus::Merged, Some(1)));
        assert_eq!(file.worktree.len(), 1);
        assert_eq!(
            file.get(Path::new("/tmp/wt")).unwrap().pr_status,
            PrStatus::Merged
        );
    }

    #[test]
    fn prune_missing_drops_orphans() {
        let mut file = PrStatusFile::default();
        file.upsert(entry("/tmp/live", PrStatus::Open, None));
        file.upsert(entry("/tmp/dead", PrStatus::Open, None));
        file.prune_missing(&[PathBuf::from("/tmp/live")]);
        assert_eq!(file.worktree.len(), 1);
        assert_eq!(file.worktree[0].path, PathBuf::from("/tmp/live"));
    }

    #[test]
    fn missing_optional_fields_deserialize_to_defaults() {
        // A minimal row (only path + branch-free) must fill defaults.
        let toml = r#"
[[worktree]]
path = "/tmp/wt"
"#;
        let file: PrStatusFile = toml::from_str(toml).expect("deserialize");
        assert_eq!(file.worktree.len(), 1);
        let e = &file.worktree[0];
        assert_eq!(e.pr_status, PrStatus::Loading);
        assert_eq!(e.pr_number, None);
        assert_eq!(e.unresolved_comments, None);
    }
}

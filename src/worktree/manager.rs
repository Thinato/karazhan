#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::model::Worktree;
use super::state;

// ---------------------------------------------------------------------------
// WorktreeManager
// ---------------------------------------------------------------------------

/// Wraps `git worktree` operations for a single git repository.
///
/// All git commands are shelled out via `std::process::Command`.
/// State metadata (prompt slug, PR number, flags, status) is persisted to
/// `.karazhan/state.toml` under `repo_root`.
pub struct WorktreeManager {
    /// Absolute path to the root of the git repository.
    pub repo_root: PathBuf,
}

impl WorktreeManager {
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
    }

    // -----------------------------------------------------------------------
    // create
    // -----------------------------------------------------------------------

    /// Create a new git worktree at `path` on `branch`, optionally linking it
    /// to a prompt slug.
    ///
    /// - If `branch` does not yet exist, runs `git worktree add -b <branch> <path>`.
    /// - If `branch` already exists, runs `git worktree add <path> <branch>` (no `-b`).
    ///
    /// On success the worktree is registered in `.karazhan/state.toml`.
    pub fn create(
        &self,
        prompt_slug: Option<String>,
        branch: &str,
        path: &Path,
    ) -> Result<Worktree> {
        // Determine whether the branch already exists in the repo.
        let branch_exists = self.branch_exists(branch)?;

        let output = if branch_exists {
            // Branch exists — check it out without -b.
            Command::new("git")
                .args(["worktree", "add"])
                .arg(path)
                .arg(branch)
                .current_dir(&self.repo_root)
                .output()
                .context("failed to run git worktree add")?
        } else {
            // New branch — create and check out.
            Command::new("git")
                .args(["worktree", "add", "-b"])
                .arg(branch)
                .arg(path)
                .current_dir(&self.repo_root)
                .output()
                .context("failed to run git worktree add -b")?
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git worktree add failed: {stderr}");
        }

        // Canonicalize the path now that the directory exists on disk.
        // This resolves macOS /var → /private/var symlinks so the key stored
        // in state always matches what `git worktree list --porcelain` reports.
        let canonical_path = path
            .canonicalize()
            .with_context(|| format!("cannot canonicalize worktree path {:?}", path))?;

        let worktree = Worktree {
            path: canonical_path,
            branch: branch.to_string(),
            prompt_slug,
            pr_number: None,
            auto_continue_on_merge: false,
            status: super::model::WorktreeStatus::Idle,
        };

        // Persist to state.
        let mut st = state::load(&self.repo_root)?;
        st.upsert_worktree(worktree.clone());
        state::save(&self.repo_root, &st)?;

        Ok(worktree)
    }

    // -----------------------------------------------------------------------
    // list
    // -----------------------------------------------------------------------

    /// Return all git worktrees known to `git worktree list --porcelain`,
    /// overlaid with persisted metadata from `.karazhan/state.toml`.
    ///
    /// Live git output is the source of truth for *existence*; persisted state
    /// is the source of truth for *metadata*.  Entries in state whose path no
    /// longer appears in git output are pruned automatically.
    pub fn list(&self) -> Result<Vec<Worktree>> {
        let output = Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(&self.repo_root)
            .output()
            .context("failed to run git worktree list")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git worktree list failed: {stderr}");
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut live = parse_porcelain(&stdout);

        // Load persisted state and prune orphaned entries.
        let live_paths: Vec<PathBuf> = live.iter().map(|w| w.path.clone()).collect();
        let mut st = state::load(&self.repo_root)?;
        st.prune_missing(&live_paths);

        // Overlay persisted metadata onto live worktrees.
        for wt in &mut live {
            if let Some(persisted) = st.worktrees.iter().find(|p| p.path == wt.path) {
                wt.prompt_slug = persisted.prompt_slug.clone();
                wt.pr_number = persisted.pr_number;
                wt.auto_continue_on_merge = persisted.auto_continue_on_merge;
                wt.status = persisted.status.clone();
            }
        }

        // Persist pruned state back so orphans are removed on disk.
        state::save(&self.repo_root, &st)?;

        Ok(live)
    }

    // -----------------------------------------------------------------------
    // remove
    // -----------------------------------------------------------------------

    /// Remove the worktree at `path`.
    ///
    /// Runs `git worktree remove [--force] <path>` and drops the state entry.
    /// Force is only applied when `force == true`.
    pub fn remove(&self, path: &Path, force: bool) -> Result<()> {
        // Canonicalize before running git — the directory still exists at this
        // point, so canonicalize() will succeed and the key will match what
        // create() stored in state.
        let canonical = path
            .canonicalize()
            .with_context(|| format!("cannot canonicalize worktree path {:?}", path))?;

        let mut cmd = Command::new("git");
        cmd.arg("worktree").arg("remove");
        if force {
            cmd.arg("--force");
        }
        cmd.arg(&canonical).current_dir(&self.repo_root);

        let output = cmd.output().context("failed to run git worktree remove")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git worktree remove failed: {stderr}");
        }

        // Drop from persisted state.
        let mut st = state::load(&self.repo_root)?;
        st.remove_worktree(&canonical);
        state::save(&self.repo_root, &st)?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Return true if `branch` already exists as a local branch in the repo.
    fn branch_exists(&self, branch: &str) -> Result<bool> {
        let output = Command::new("git")
            .args(["branch", "--list", branch])
            .current_dir(&self.repo_root)
            .output()
            .context("failed to run git branch --list")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git branch --list failed: {stderr}");
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        // Output is non-empty when the branch exists.
        Ok(!stdout.trim().is_empty())
    }
}

// ---------------------------------------------------------------------------
// Porcelain parser
// ---------------------------------------------------------------------------

/// Parse the output of `git worktree list --porcelain` into a vec of `Worktree`s
/// with default metadata (no state overlay yet).
///
/// The porcelain format is one stanza per worktree separated by blank lines:
///
/// ```text
/// worktree /abs/path/to/main
/// HEAD abc123
/// branch refs/heads/main
///
/// worktree /abs/path/to/feature
/// HEAD def456
/// branch refs/heads/feature
///
/// worktree /abs/path/to/detached
/// HEAD ghi789
/// detached
/// ```
fn parse_porcelain(output: &str) -> Vec<Worktree> {
    let mut result = Vec::new();

    // Split into stanzas on blank lines.
    for stanza in output.split("\n\n") {
        let stanza = stanza.trim();
        if stanza.is_empty() {
            continue;
        }

        let mut path: Option<PathBuf> = None;
        let mut branch: Option<String> = None;
        let mut is_bare = false;

        for line in stanza.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p.trim()));
            } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branch = Some(b.trim().to_string());
            } else if line.trim() == "bare" {
                is_bare = true;
            }
            // "detached" and "HEAD <sha>" lines are intentionally skipped —
            // a detached worktree will have branch = None which we handle below.
        }

        // Skip bare worktrees (the main worktree of a bare clone).
        if is_bare {
            continue;
        }

        if let Some(p) = path {
            let b = branch.unwrap_or_else(|| "HEAD".to_string());
            // Canonicalize so symlink-based paths (e.g. macOS /var → /private/var)
            // match the canonical paths stored in state.
            let canonical = p.canonicalize().unwrap_or(p);
            result.push(Worktree::from_git(canonical, b));
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // Test-repo fixture
    // -----------------------------------------------------------------------

    /// Create a real temporary git repository suitable for `git worktree` ops.
    ///
    /// Requirements:
    /// - `git init`
    /// - local user.email + user.name so commits work without global config
    /// - at least one commit (worktrees cannot be added without a commit)
    fn make_temp_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();

        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap_or_else(|e| panic!("git {args:?} failed to launch: {e}"));
            assert!(status.success(), "git {args:?} exited with non-zero status");
        };

        run(&["init"]);
        run(&["config", "user.email", "test@karazhan.test"]);
        run(&["config", "user.name", "Karazhan Test"]);

        // Create an initial commit so worktrees can be attached.
        let readme = root.join("README.md");
        std::fs::write(&readme, "karazhan test repo\n").expect("write README");
        run(&["add", "README.md"]);
        run(&["commit", "-m", "initial commit"]);

        (dir, root)
    }

    // -----------------------------------------------------------------------
    // create + list
    // -----------------------------------------------------------------------

    #[test]
    fn create_worktree_appears_in_list() {
        let (_dir, root) = make_temp_repo();
        // Also create a separate temp dir for the worktree path so it's outside
        // the repo dir (avoids git's "already a worktree" check).
        let wt_dir = tempfile::tempdir().expect("wt tempdir");
        let wt_path = wt_dir.path().to_path_buf();

        let mgr = WorktreeManager::new(&root);
        let wt = mgr
            .create(Some("my-prompt".to_string()), "feature-a", &wt_path)
            .expect("create");

        assert_eq!(wt.branch, "feature-a");
        assert_eq!(wt.prompt_slug, Some("my-prompt".to_string()));
        assert!(wt_path.exists(), "worktree directory should exist on disk");

        // Canonicalize for comparison — on macOS /var is a symlink to /private/var,
        // so git and Rust's canonicalize() both resolve to the /private/var path.
        let canonical_wt = wt_path.canonicalize().expect("canonicalize wt_path");

        let list = mgr.list().expect("list");
        // Should contain at least the main worktree + our new one.
        assert!(
            list.iter().any(|w| w.path == canonical_wt),
            "new worktree should appear in list; got: {:?}",
            list.iter().map(|w| &w.path).collect::<Vec<_>>()
        );
    }

    // -----------------------------------------------------------------------
    // Multiple worktrees for the same prompt slug
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_worktrees_same_prompt_slug() {
        let (_dir, root) = make_temp_repo();
        let wt_dir1 = tempfile::tempdir().expect("wt tempdir 1");
        let wt_dir2 = tempfile::tempdir().expect("wt tempdir 2");

        let mgr = WorktreeManager::new(&root);
        mgr.create(
            Some("shared-prompt".to_string()),
            "branch-one",
            wt_dir1.path(),
        )
        .expect("create 1");
        mgr.create(
            Some("shared-prompt".to_string()),
            "branch-two",
            wt_dir2.path(),
        )
        .expect("create 2");

        let list = mgr.list().expect("list");

        let matching: Vec<_> = list
            .iter()
            .filter(|w| w.prompt_slug.as_deref() == Some("shared-prompt"))
            .collect();

        assert_eq!(
            matching.len(),
            2,
            "both worktrees should appear for the same prompt slug"
        );
    }

    // -----------------------------------------------------------------------
    // State round-trip + list overlay
    // -----------------------------------------------------------------------

    #[test]
    fn list_overlays_persisted_metadata() {
        let (_dir, root) = make_temp_repo();
        let wt_dir = tempfile::tempdir().expect("wt tempdir");
        let wt_path = wt_dir.path().to_path_buf();

        let mgr = WorktreeManager::new(&root);
        mgr.create(
            Some("overlay-prompt".to_string()),
            "overlay-branch",
            &wt_path,
        )
        .expect("create");

        // create() stores the canonical path; use it for state key lookups.
        let canonical_wt = wt_path.canonicalize().expect("canonicalize wt_path");

        // Manually update state with extra metadata.
        let mut st = state::load(&root).expect("load state");
        st.set_pr_number(&canonical_wt, Some(99));
        st.set_auto_continue(&canonical_wt, true);
        st.set_status(
            &canonical_wt,
            super::super::model::WorktreeStatus::NeedsReview,
        );
        state::save(&root, &st).expect("save state");

        // list() should overlay that metadata back.
        let list = mgr.list().expect("list");
        let wt = list
            .iter()
            .find(|w| w.path == canonical_wt)
            .expect("wt not found in list");
        assert_eq!(wt.pr_number, Some(99));
        assert!(wt.auto_continue_on_merge);
        assert_eq!(wt.status, super::super::model::WorktreeStatus::NeedsReview);
        assert_eq!(wt.prompt_slug.as_deref(), Some("overlay-prompt"));
    }

    // -----------------------------------------------------------------------
    // remove
    // -----------------------------------------------------------------------

    #[test]
    fn remove_deletes_worktree_and_prunes_state() {
        let (_dir, root) = make_temp_repo();
        let wt_dir = tempfile::tempdir().expect("wt tempdir");
        let wt_path = wt_dir.path().to_path_buf();

        let mgr = WorktreeManager::new(&root);
        mgr.create(None, "rm-branch", &wt_path).expect("create");

        // Canonicalize before removal — the directory still exists at this point.
        let canonical_wt = wt_path.canonicalize().expect("canonicalize wt_path");

        mgr.remove(&wt_path, false).expect("remove");

        let list = mgr.list().expect("list");
        assert!(
            !list.iter().any(|w| w.path == canonical_wt),
            "removed worktree should not appear in list"
        );

        let st = state::load(&root).expect("load state");
        assert!(
            !st.worktrees.iter().any(|w| w.path == canonical_wt),
            "removed worktree should be pruned from state"
        );
    }

    // -----------------------------------------------------------------------
    // Porcelain parser unit tests (no git subprocess needed)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_porcelain_basic() {
        let input = "\
worktree /home/user/repo
HEAD abc123def456
branch refs/heads/main

worktree /home/user/repo-feature
HEAD 789abc012def
branch refs/heads/feature-x

";
        let wts = parse_porcelain(input);
        assert_eq!(wts.len(), 2);
        assert_eq!(wts[0].path, PathBuf::from("/home/user/repo"));
        assert_eq!(wts[0].branch, "main");
        assert_eq!(wts[1].path, PathBuf::from("/home/user/repo-feature"));
        assert_eq!(wts[1].branch, "feature-x");
    }

    #[test]
    fn parse_porcelain_detached() {
        let input = "\
worktree /tmp/detached
HEAD deadbeef1234
detached

";
        let wts = parse_porcelain(input);
        assert_eq!(wts.len(), 1);
        assert_eq!(wts[0].branch, "HEAD");
    }

    #[test]
    fn parse_porcelain_skips_bare() {
        let input = "\
worktree /srv/bare.git
HEAD aabbccdd1122
bare

worktree /srv/wt
HEAD 11223344aabb
branch refs/heads/work

";
        let wts = parse_porcelain(input);
        assert_eq!(wts.len(), 1);
        assert_eq!(wts[0].branch, "work");
    }
}

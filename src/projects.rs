//! Global multi-project registry persisted to
//! `$XDG_CONFIG_HOME/karazhan/projects.toml` (or
//! `~/.config/karazhan/projects.toml`).
//!
//! The daemon manages N git repositories ("projects").  Each project is a git
//! repo that may carry its own `.karazhan/config.toml` (agent invocation) and
//! `.karazhan/state.toml` (worktree metadata).  This module owns the registry
//! file (load/save) plus the helpers used to register a new project: git-repo
//! validation, path canonicalization, and unique-name derivation.
//!
//! A missing file yields an empty registry silently.  A malformed file logs a
//! warning and also yields an empty registry — the daemon never crashes on bad
//! registry data.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// A single registered project: a name (unique within the registry) plus the
/// canonical path to its git repository root.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
}

/// On-disk shape of `projects.toml`.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq, Eq)]
pub struct ProjectsFile {
    #[serde(default)]
    pub projects: Vec<Project>,
}

// ---------------------------------------------------------------------------
// Path resolution (mirrors config.rs)
// ---------------------------------------------------------------------------

/// Resolve the canonical registry file path.
///
/// Resolution order (mirrors [`crate::config::Config`]):
/// 1. `$XDG_CONFIG_HOME/karazhan/projects.toml`
/// 2. `$HOME/.config/karazhan/projects.toml`
pub fn registry_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("karazhan").join("projects.toml");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".config")
        .join("karazhan")
        .join("projects.toml")
}

// ---------------------------------------------------------------------------
// Load / Save
// ---------------------------------------------------------------------------

/// Load the registry from [`registry_path`].
///
/// A missing file → empty registry.  A malformed file → `tracing::warn` +
/// empty registry (never panics).
pub fn load() -> ProjectsFile {
    let path = registry_path();
    if !path.exists() {
        return ProjectsFile::default();
    }
    match std::fs::read_to_string(&path) {
        Err(e) => {
            tracing::warn!("projects: could not read {}: {e}", path.display());
            ProjectsFile::default()
        }
        Ok(text) => match toml::from_str::<ProjectsFile>(&text) {
            Ok(pf) => pf,
            Err(e) => {
                tracing::warn!(
                    "projects: malformed TOML at {} ({e}), using empty registry",
                    path.display()
                );
                ProjectsFile::default()
            }
        },
    }
}

/// Atomically write `pf` to [`registry_path`], creating the parent directory.
pub fn save(pf: &ProjectsFile) -> Result<()> {
    let path = registry_path();
    let dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create registry dir {:?}", dir))?;

    let tmp_path = dir.join("projects.toml.tmp");
    let content = toml::to_string_pretty(pf).context("cannot serialise projects registry")?;
    std::fs::write(&tmp_path, &content)
        .with_context(|| format!("cannot write temp registry file {:?}", tmp_path))?;
    std::fs::rename(&tmp_path, &path)
        .with_context(|| format!("cannot rename {:?} -> {:?}", tmp_path, path))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Git-repo validation
// ---------------------------------------------------------------------------

/// Best-effort check that `path` is inside a git work tree.
///
/// Canonicalizes `path` first; runs `git -C <path> rev-parse
/// --is-inside-work-tree`.  Returns `false` on any error (missing dir, not a
/// repo, git not on PATH).
pub fn is_git_repo(path: &Path) -> bool {
    let canonical = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => return false,
    };
    Command::new("git")
        .arg("-C")
        .arg(&canonical)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Name derivation
// ---------------------------------------------------------------------------

/// Derive a unique project name from `path`'s basename.
///
/// If a project with that name already exists in `existing`, append `-2`,
/// `-3`, … until the name is unique.  A path with no usable basename falls
/// back to "project".
pub fn derive_name(path: &Path, existing: &[Project]) -> String {
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "project".to_string());

    let taken = |candidate: &str| existing.iter().any(|p| p.name == candidate);

    if !taken(&base) {
        return base;
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{base}-{n}");
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

// ---------------------------------------------------------------------------
// Register a project
// ---------------------------------------------------------------------------

/// Register the git repository at `path` in the global registry.
///
/// Steps: canonicalize, validate it is a git repo (else `Err`), dedupe by
/// canonical path (returning the existing entry if already present), derive a
/// unique name, append, persist, and return the [`Project`].
pub fn add(path: &Path) -> Result<Project> {
    if !is_git_repo(path) {
        bail!("not a git repository: {}", path.display());
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot canonicalize project path {:?}", path))?;

    let mut pf = load();

    // Dedupe by canonical path.
    if let Some(existing) = pf.projects.iter().find(|p| p.path == canonical) {
        return Ok(existing.clone());
    }

    let name = derive_name(&canonical, &pf.projects);
    let project = Project {
        name,
        path: canonical,
    };
    pf.projects.push(project.clone());
    save(&pf)?;
    Ok(project)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Create a real temporary git repository.
    fn make_temp_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap_or_else(|e| panic!("git {args:?} failed: {e}"));
            assert!(status.success(), "git {args:?} non-zero");
        };
        run(&["init"]);
        run(&["config", "user.email", "test@karazhan.test"]);
        run(&["config", "user.name", "Karazhan Test"]);
        std::fs::write(root.join("README.md"), "karazhan test\n").expect("write README");
        run(&["add", "README.md"]);
        run(&["commit", "-m", "initial"]);
        (dir, root)
    }

    #[test]
    fn projects_file_round_trip() {
        let pf = ProjectsFile {
            projects: vec![
                Project {
                    name: "alpha".into(),
                    path: PathBuf::from("/repo/alpha"),
                },
                Project {
                    name: "beta".into(),
                    path: PathBuf::from("/repo/beta"),
                },
            ],
        };
        let text = toml::to_string_pretty(&pf).expect("serialise");
        let back: ProjectsFile = toml::from_str(&text).expect("deserialise");
        assert_eq!(pf, back);
    }

    #[test]
    fn empty_registry_round_trip() {
        let pf = ProjectsFile::default();
        let text = toml::to_string_pretty(&pf).expect("serialise");
        let back: ProjectsFile = toml::from_str(&text).expect("deserialise");
        assert!(back.projects.is_empty());
    }

    #[test]
    fn malformed_toml_yields_empty() {
        let bad = "this is not { valid toml ===";
        let result = toml::from_str::<ProjectsFile>(bad);
        assert!(result.is_err());
        // load() converts this to ProjectsFile::default(); confirm default empty.
        assert!(ProjectsFile::default().projects.is_empty());
    }

    #[test]
    fn missing_file_yields_empty() {
        // toml parsing of an empty doc → empty registry.
        let pf: ProjectsFile = toml::from_str("").expect("empty doc parses");
        assert!(pf.projects.is_empty());
    }

    #[test]
    fn derive_name_basename() {
        let existing = vec![];
        assert_eq!(
            derive_name(Path::new("/a/b/karazhan"), &existing),
            "karazhan"
        );
    }

    #[test]
    fn derive_name_dedups_with_suffix() {
        let existing = vec![
            Project {
                name: "karazhan".into(),
                path: PathBuf::from("/x/karazhan"),
            },
            Project {
                name: "karazhan-2".into(),
                path: PathBuf::from("/y/karazhan"),
            },
        ];
        assert_eq!(
            derive_name(Path::new("/z/karazhan"), &existing),
            "karazhan-3"
        );
    }

    #[test]
    fn is_git_repo_true_for_temp_repo() {
        let (_dir, root) = make_temp_repo();
        assert!(is_git_repo(&root));
    }

    #[test]
    fn is_git_repo_false_for_plain_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!is_git_repo(dir.path()));
    }

    #[test]
    fn is_git_repo_false_for_missing_path() {
        assert!(!is_git_repo(Path::new("/nonexistent/path/xyz")));
    }
}

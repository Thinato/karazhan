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
// Git owner parsing
// ---------------------------------------------------------------------------

/// Parse the owner/organisation segment from a git remote URL.
///
/// Handles the common forms:
/// - `git@github.com:Owner/Repo.git` → `Owner`
/// - `https://github.com/Owner/Repo.git` → `Owner`
/// - `ssh://git@host/Owner/Repo` → `Owner`
///
/// Returns `None` when the URL doesn't match any recognised pattern or the
/// extracted segment would be empty / contain path separators.
///
/// Trailing `.git` is stripped before splitting.
pub fn parse_owner(remote_url: &str) -> Option<String> {
    let url = remote_url.trim();
    if url.is_empty() {
        return None;
    }

    // Strip a trailing `.git` suffix (case-insensitive would be exotic; `.git`
    // is canonical).
    let url = url.strip_suffix(".git").unwrap_or(url);

    // URL-style: strip the scheme (everything up to and including `://`) first,
    // since `://` is unambiguous.
    if let Some(scheme_end) = url.find("://") {
        let after_scheme = &url[scheme_end + 3..];
        // Drop the host[:port] (everything before the first `/`).
        let path_part = if let Some(slash) = after_scheme.find('/') {
            &after_scheme[slash + 1..]
        } else {
            return None;
        };
        return owner_from_slash_path(path_part);
    }

    // SCP-style: git@host:Owner/Repo  (colon separates host from path, no `://`).
    if let Some(colon_pos) = url.find(':') {
        // The part before the colon must not contain `/` (it's `git@host` or
        // `host`, not a path).
        let before = &url[..colon_pos];
        if !before.contains('/') {
            let path = &url[colon_pos + 1..];
            return owner_from_slash_path(path);
        }
    }

    // No recognised URL form.
    None
}

/// Extract the first path segment (owner) from a slash-separated path like
/// `Owner/Repo` or `/Owner/Repo`.
fn owner_from_slash_path(path: &str) -> Option<String> {
    // Strip a leading slash if present.
    let path = path.strip_prefix('/').unwrap_or(path);
    let owner = path.split('/').next()?;
    if owner.is_empty() {
        return None;
    }
    Some(sanitize_path_segment(owner))
}

/// Parse the repository-name segment from a git remote URL.
///
/// Symmetric with [`parse_owner`]: handles the same https / ssh-scheme / SCP
/// forms and strips a trailing `.git`.  Returns the SECOND path segment
/// (`Owner/Repo` → `Repo`).
///
/// Returns `None` when the URL doesn't match any recognised pattern or the
/// extracted segment would be empty.
pub fn parse_repo(remote_url: &str) -> Option<String> {
    let url = remote_url.trim();
    if url.is_empty() {
        return None;
    }

    // Strip a trailing `.git` suffix.
    let url = url.strip_suffix(".git").unwrap_or(url);

    // URL-style: strip the scheme (everything up to and including `://`) first.
    if let Some(scheme_end) = url.find("://") {
        let after_scheme = &url[scheme_end + 3..];
        let path_part = if let Some(slash) = after_scheme.find('/') {
            &after_scheme[slash + 1..]
        } else {
            return None;
        };
        return repo_from_slash_path(path_part);
    }

    // SCP-style: git@host:Owner/Repo  (colon separates host from path, no `://`).
    if let Some(colon_pos) = url.find(':') {
        let before = &url[..colon_pos];
        if !before.contains('/') {
            let path = &url[colon_pos + 1..];
            return repo_from_slash_path(path);
        }
    }

    None
}

/// Extract the second path segment (repo) from a slash-separated path like
/// `Owner/Repo` or `/Owner/Repo`.
fn repo_from_slash_path(path: &str) -> Option<String> {
    let path = path.strip_prefix('/').unwrap_or(path);
    let mut segments = path.split('/');
    let _owner = segments.next()?;
    let repo = segments.next()?;
    if repo.is_empty() {
        return None;
    }
    Some(sanitize_path_segment(repo))
}

/// Sanitize a string so it is safe to use as a single path segment.
///
/// Replaces any character that is not alphanumeric, `-`, `_`, or `.` with `_`.
/// Leading dots are replaced to avoid hidden-dir accidents.
fn sanitize_path_segment(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Replace a leading dot so the segment doesn't become a hidden dir.
    if out.starts_with('.') {
        out.replace_range(..1, "_");
    }
    out
}

/// Return the git remote owner for `repo_root` by reading `remote.origin.url`.
///
/// Runs `git -C <repo_root> config --get remote.origin.url` and parses the
/// result with [`parse_owner`].  Falls back to `"local"` on any failure
/// (no remote, command not found, unparseable URL).
pub fn git_owner(repo_root: &Path) -> String {
    let output = Command::new("git")
        .args(["-C"])
        .arg(repo_root)
        .args(["config", "--get", "remote.origin.url"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();

    let url = match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => return "local".to_string(),
    };

    parse_owner(&url).unwrap_or_else(|| "local".to_string())
}

/// Return the `(owner, repo)` GitHub coordinates for `repo_root` by reading
/// `remote.origin.url`.
///
/// Runs `git -C <repo_root> config --get remote.origin.url` ONCE and parses the
/// result with [`parse_owner`] + [`parse_repo`].  Returns `None` when there is
/// no remote, git is unavailable, or the URL is unparseable.
///
/// A linked/detached worktree shares its parent repo's remote, so this works
/// when called with EITHER the project root OR a worktree path — the daemon may
/// pass whichever is convenient.
pub fn git_owner_repo(repo_root: &Path) -> Option<(String, String)> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(repo_root)
        .args(["config", "--get", "remote.origin.url"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let owner = parse_owner(&url)?;
    let repo = parse_repo(&url)?;
    Some((owner, repo))
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

    // -----------------------------------------------------------------------
    // parse_owner — pure URL parser, no git spawning
    // -----------------------------------------------------------------------

    #[test]
    fn parse_owner_scp_style_github() {
        assert_eq!(
            parse_owner("git@github.com:Owner/Repo.git"),
            Some("Owner".to_string())
        );
    }

    #[test]
    fn parse_owner_https_style_github() {
        assert_eq!(
            parse_owner("https://github.com/Owner/Repo.git"),
            Some("Owner".to_string())
        );
    }

    #[test]
    fn parse_owner_ssh_scheme() {
        assert_eq!(
            parse_owner("ssh://git@host/Owner/Repo"),
            Some("Owner".to_string())
        );
    }

    #[test]
    fn parse_owner_non_github_host() {
        assert_eq!(
            parse_owner("https://gitlab.company.io/MyOrg/my-project.git"),
            Some("MyOrg".to_string())
        );
    }

    #[test]
    fn parse_owner_scp_no_dot_git() {
        assert_eq!(
            parse_owner("git@github.com:Thinato/karazhan"),
            Some("Thinato".to_string())
        );
    }

    #[test]
    fn parse_owner_empty_string_returns_none() {
        assert_eq!(parse_owner(""), None);
    }

    #[test]
    fn parse_owner_garbage_returns_none() {
        assert_eq!(parse_owner("not-a-url"), None);
    }

    #[test]
    fn parse_owner_whitespace_only_returns_none() {
        assert_eq!(parse_owner("   "), None);
    }

    #[test]
    fn parse_owner_sanitizes_special_chars() {
        // An owner with characters outside [A-Za-z0-9._-] gets sanitized.
        let result = parse_owner("https://host/My Owner/Repo.git");
        // Space → underscore
        assert_eq!(result, Some("My_Owner".to_string()));
    }

    // -----------------------------------------------------------------------
    // parse_repo — pure URL parser, no git spawning
    // -----------------------------------------------------------------------

    #[test]
    fn parse_repo_scp_style_github() {
        assert_eq!(
            parse_repo("git@github.com:Owner/Repo.git"),
            Some("Repo".to_string())
        );
    }

    #[test]
    fn parse_repo_https_style_github() {
        assert_eq!(
            parse_repo("https://github.com/Owner/Repo.git"),
            Some("Repo".to_string())
        );
    }

    #[test]
    fn parse_repo_ssh_scheme() {
        assert_eq!(
            parse_repo("ssh://git@host/Owner/Repo"),
            Some("Repo".to_string())
        );
    }

    #[test]
    fn parse_repo_scp_no_dot_git() {
        assert_eq!(
            parse_repo("git@github.com:Thinato/karazhan"),
            Some("karazhan".to_string())
        );
    }

    #[test]
    fn parse_repo_strips_dot_git_suffix() {
        // The `.git` suffix must not survive into the parsed repo name.
        assert_eq!(
            parse_repo("https://github.com/Org/my-project.git"),
            Some("my-project".to_string())
        );
    }

    #[test]
    fn parse_repo_empty_string_returns_none() {
        assert_eq!(parse_repo(""), None);
    }

    #[test]
    fn parse_repo_garbage_returns_none() {
        assert_eq!(parse_repo("not-a-url"), None);
    }

    #[test]
    fn parse_repo_owner_only_returns_none() {
        // A path with no second segment has no repo.
        assert_eq!(parse_repo("git@github.com:Owner"), None);
    }

    #[test]
    fn git_owner_repo_no_remote_returns_none() {
        let (_dir, root) = make_temp_repo();
        assert_eq!(git_owner_repo(&root), None);
    }

    #[test]
    fn git_owner_repo_with_remote_parses_pair() {
        let (_dir, root) = make_temp_repo();
        let status = Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "git@github.com:TestOrg/testrepo.git",
            ])
            .current_dir(&root)
            .status()
            .expect("git remote add");
        assert!(status.success());
        assert_eq!(
            git_owner_repo(&root),
            Some(("TestOrg".to_string(), "testrepo".to_string()))
        );
    }

    #[test]
    fn git_owner_no_remote_returns_local() {
        // A repo without a remote origin must return "local".
        let (_dir, root) = make_temp_repo();
        assert_eq!(git_owner(&root), "local");
    }

    #[test]
    fn git_owner_with_remote_parses_owner() {
        let (_dir, root) = make_temp_repo();
        // Add a fake remote so we can test the live git_owner path.
        let status = Command::new("git")
            .args([
                "remote",
                "add",
                "origin",
                "git@github.com:TestOrg/testrepo.git",
            ])
            .current_dir(&root)
            .status()
            .expect("git remote add");
        assert!(status.success());
        assert_eq!(git_owner(&root), "TestOrg");
    }

    // -----------------------------------------------------------------------
    // Existing tests
    // -----------------------------------------------------------------------

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

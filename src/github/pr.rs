//! PR state, review comments, and PR-for-branch discovery via `gh`.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::GhRunner;

// ---------------------------------------------------------------------------
// PrState
// ---------------------------------------------------------------------------

/// Coarse state of a GitHub PR as returned by `gh pr view --json`.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrState {
    /// `"OPEN"`, `"MERGED"`, or `"CLOSED"`.
    pub state: String,
    /// GitHub's `mergeStateStatus` field (e.g. `"CLEAN"`, `"BLOCKED"`, …).
    pub merge_state_status: Option<String>,
    /// True when the PR has been merged.
    pub merged: bool,
    pub title: String,
}

// Internal serde shape that mirrors `gh pr view --json state,mergeStateStatus,mergedAt,title`.
#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPrView {
    state: String,
    merge_state_status: Option<String>,
    merged_at: Option<String>,
    title: String,
}

/// Fetch the state of PR `pr` in the repository rooted at `cwd`.
///
/// Uses `gh pr view <pr> --json state,mergeStateStatus,mergedAt,title`.
#[allow(dead_code)]
pub async fn pr_state(runner: &dyn GhRunner, cwd: &Path, pr: u64) -> Result<PrState> {
    let pr_str = pr.to_string();
    let stdout = runner
        .run(
            &[
                "pr",
                "view",
                &pr_str,
                "--json",
                "state,mergeStateStatus,mergedAt,title",
            ],
            cwd,
        )
        .await
        .with_context(|| format!("gh pr view {pr} failed"))?;

    let raw: RawPrView =
        serde_json::from_str(&stdout).with_context(|| "failed to parse gh pr view JSON")?;

    Ok(PrState {
        state: raw.state,
        merge_state_status: raw.merge_state_status,
        merged: raw.merged_at.is_some(),
        title: raw.title,
    })
}

// ---------------------------------------------------------------------------
// pr_for_current_branch
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawPrNumber {
    number: u64,
}

/// Return the PR number for the branch currently checked out in `cwd`, or
/// `None` when no PR is open (not an error).
///
/// Uses `gh pr view --json number` (no PR number argument — defaults to the
/// current branch).  `gh` exits with non-zero and prints "no pull requests
/// found" when there is no PR; that specific case is mapped to `Ok(None)`.
pub async fn pr_for_current_branch(runner: &dyn GhRunner, cwd: &Path) -> Result<Option<u64>> {
    let result = runner.run(&["pr", "view", "--json", "number"], cwd).await;

    match result {
        Ok(stdout) => {
            let raw: RawPrNumber = serde_json::from_str(&stdout)
                .with_context(|| "failed to parse gh pr view --json number")?;
            Ok(Some(raw.number))
        }
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            // gh prints "no pull requests found" / "could not find" when
            // there is simply no open PR — treat that as Ok(None).
            if msg.contains("no pull request") || msg.contains("could not find") {
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ReviewComment
// ---------------------------------------------------------------------------

/// A single review comment on a pull request.
#[derive(Debug, Clone)]
pub struct ReviewComment {
    pub author: String,
    /// File path the comment is attached to, if any.
    pub path: Option<String>,
    /// Line number within the file, if any.
    pub line: Option<u64>,
    pub body: String,
}

// Serde shapes from `gh api repos/{owner}/{repo}/pulls/<pr>/comments`.
#[derive(Deserialize)]
struct RawApiComment {
    #[serde(default)]
    user: Option<RawUser>,
    #[serde(default)]
    path: Option<String>,
    /// `gh api` returns the field as `line` (can be null).
    #[serde(default)]
    line: Option<u64>,
    body: String,
}

#[derive(Deserialize)]
struct RawUser {
    login: String,
}

// Shape from `gh pr view <pr> --json comments`.
#[derive(Deserialize)]
struct RawPrComments {
    comments: Vec<RawPrComment>,
}

#[derive(Deserialize)]
struct RawPrComment {
    #[serde(default)]
    author: Option<RawCommentAuthor>,
    body: String,
}

#[derive(Deserialize)]
struct RawCommentAuthor {
    login: String,
}

/// Fetch all review (inline) comments for PR `pr` in the repo at `cwd`.
///
/// Uses `gh pr view <pr> --json comments` which returns the general PR
/// comments.  Fields `path` and `line` are only present for review (diff)
/// comments; they are `None` for issue-style comments on the PR.
pub async fn review_comments(
    runner: &dyn GhRunner,
    cwd: &Path,
    pr: u64,
) -> Result<Vec<ReviewComment>> {
    let pr_str = pr.to_string();
    let stdout = runner
        .run(&["pr", "view", &pr_str, "--json", "comments"], cwd)
        .await
        .with_context(|| format!("gh pr view {pr} --json comments failed"))?;

    // Try parsing as the `gh pr view --json comments` shape first.
    if let Ok(raw) = serde_json::from_str::<RawPrComments>(&stdout) {
        let comments = raw
            .comments
            .into_iter()
            .map(|c| ReviewComment {
                author: c
                    .author
                    .map(|a| a.login)
                    .unwrap_or_else(|| "unknown".into()),
                path: None,
                line: None,
                body: c.body,
            })
            .collect();
        return Ok(comments);
    }

    // Fall back to the `gh api` array shape (inline review comments).
    let raw_api: Vec<RawApiComment> =
        serde_json::from_str(&stdout).with_context(|| "failed to parse review comments JSON")?;

    let comments = raw_api
        .into_iter()
        .map(|c| ReviewComment {
            author: c.user.map(|u| u.login).unwrap_or_else(|| "unknown".into()),
            path: c.path,
            line: c.line,
            body: c.body,
        })
        .collect();

    Ok(comments)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::mock::MockGh;
    use std::path::Path;

    // -----------------------------------------------------------------------
    // pr_state tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn pr_state_open() {
        let json =
            r#"{"state":"OPEN","mergeStateStatus":"CLEAN","mergedAt":null,"title":"Add feature"}"#;
        let mock = MockGh::new(vec![("pr view 42", Ok(json.to_string()))]);
        let result = pr_state(&mock, Path::new("/tmp"), 42).await.unwrap();
        assert_eq!(result.state, "OPEN");
        assert_eq!(result.merge_state_status, Some("CLEAN".to_string()));
        assert!(!result.merged);
        assert_eq!(result.title, "Add feature");
    }

    #[tokio::test]
    async fn pr_state_merged() {
        let json = r#"{"state":"MERGED","mergeStateStatus":null,"mergedAt":"2024-01-15T10:00:00Z","title":"Fix bug"}"#;
        let mock = MockGh::new(vec![("pr view 7", Ok(json.to_string()))]);
        let result = pr_state(&mock, Path::new("/tmp"), 7).await.unwrap();
        assert_eq!(result.state, "MERGED");
        assert!(result.merged);
        assert_eq!(result.title, "Fix bug");
    }

    // -----------------------------------------------------------------------
    // pr_for_current_branch tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn pr_for_current_branch_returns_none_when_no_pr() {
        let mock = MockGh::new(vec![(
            "pr view --json number",
            Err(anyhow::anyhow!("no pull requests found for branch")),
        )]);
        let result = pr_for_current_branch(&mock, Path::new("/tmp"))
            .await
            .unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn pr_for_current_branch_returns_number_when_pr_exists() {
        let json = r#"{"number":123}"#;
        let mock = MockGh::new(vec![("pr view --json number", Ok(json.to_string()))]);
        let result = pr_for_current_branch(&mock, Path::new("/tmp"))
            .await
            .unwrap();
        assert_eq!(result, Some(123));
    }

    // -----------------------------------------------------------------------
    // review_comments tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn review_comments_parses_correctly() {
        let json = r#"{
            "comments": [
                {"author": {"login": "alice"}, "body": "Please fix this."},
                {"author": {"login": "bob"}, "body": "Needs more tests."}
            ]
        }"#;
        let mock = MockGh::new(vec![("pr view 10 --json comments", Ok(json.to_string()))]);
        let comments = review_comments(&mock, Path::new("/tmp"), 10).await.unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].author, "alice");
        assert_eq!(comments[0].body, "Please fix this.");
        assert_eq!(comments[1].author, "bob");
        assert_eq!(comments[1].body, "Needs more tests.");
    }

    #[tokio::test]
    async fn review_comments_handles_missing_author() {
        let json = r#"{"comments": [{"body": "Anonymous comment"}]}"#;
        let mock = MockGh::new(vec![("pr view 5 --json comments", Ok(json.to_string()))]);
        let comments = review_comments(&mock, Path::new("/tmp"), 5).await.unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "unknown");
    }

    #[tokio::test]
    async fn review_comments_api_shape_with_path_line() {
        let json = r#"[
            {"user": {"login": "reviewer"}, "path": "src/main.rs", "line": 42, "body": "Fix this line."}
        ]"#;
        let mock = MockGh::new(vec![("pr view 99 --json comments", Ok(json.to_string()))]);
        let comments = review_comments(&mock, Path::new("/tmp"), 99).await.unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "reviewer");
        assert_eq!(comments[0].path, Some("src/main.rs".to_string()));
        assert_eq!(comments[0].line, Some(42));
        assert_eq!(comments[0].body, "Fix this line.");
    }
}

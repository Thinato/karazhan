//! Built-in command prompt builders — compose `gh`-sourced context into agent
//! prompt strings that are then sent to the active [`AgentBackend`].
//!
//! All functions return a fully-formed `String` ready to pass to
//! `run_agent(worktree, prompt)`.  They are async because they shell out to
//! `gh`; callers should spawn them on a tokio task so the UI never blocks.

use std::path::Path;

use anyhow::{bail, Result};

use super::ci::{ci_status, failing_logs};
use super::pr::review_comments;
use super::GhRunner;

// ---------------------------------------------------------------------------
// build_address_pr_comments_prompt
// ---------------------------------------------------------------------------

/// Build a prompt that asks the agent to address all open review comments on
/// PR `pr`.
///
/// Returns `Err` when there are no comments (the UI can surface this as a
/// friendly status message rather than starting a no-op agent run).
pub async fn build_address_pr_comments_prompt(
    runner: &dyn GhRunner,
    cwd: &Path,
    pr: u64,
) -> Result<String> {
    let comments = review_comments(runner, cwd, pr).await?;

    if comments.is_empty() {
        bail!("no review comments found on PR #{pr}");
    }

    let mut lines = Vec::with_capacity(comments.len());
    for c in &comments {
        let location = match (&c.path, c.line) {
            (Some(p), Some(l)) => format!("{p}:{l}"),
            (Some(p), None) => p.clone(),
            _ => "general".to_string(),
        };
        lines.push(format!("- [{}] {}: {}", location, c.author, c.body));
    }

    let prompt = format!(
        "Address all of the following review comments on PR #{}:\n\n{}\n\nMake the changes and explain what you did.",
        pr,
        lines.join("\n")
    );

    Ok(prompt)
}

// ---------------------------------------------------------------------------
// build_check_ci_prompt
// ---------------------------------------------------------------------------

/// Build a prompt for the agent based on the CI status of PR `pr_number`.
///
/// - If all checks pass: returns a short message noting CI is green (no agent
///   work needed; the UI can show this as a status toast).
/// - If checks are failing: fetches logs for each failed run and builds a
///   diagnostic prompt.
///
/// `run_id_for_check` is an optional helper closure used in tests to map a
/// failing check name to its run ID.  In production, `gh pr checks` does not
/// directly return run IDs; we use `0` as a sentinel and let `gh` infer from
/// context (or the caller can extract run IDs from a richer API call).  For
/// the current implementation we attempt a single `gh run view --log-failed`
/// using run_id `0` which `gh` resolves to the latest run for the repo.
pub async fn build_check_ci_prompt(
    runner: &dyn GhRunner,
    cwd: &Path,
    pr_number: u64,
) -> Result<String> {
    let status = ci_status(runner, cwd, pr_number).await?;

    if status.all_passing {
        return Ok(format!(
            "CI is passing for PR #{pr_number} — all checks are green. Nothing to fix."
        ));
    }

    let failing: Vec<_> = status.checks.iter().filter(|c| c.is_failing()).collect();

    if failing.is_empty() {
        // Checks exist but none have a failing conclusion (still running, etc.).
        return Ok(format!(
            "CI checks for PR #{pr_number} are not yet complete. No failures to diagnose."
        ));
    }

    let failing_names: Vec<&str> = failing.iter().map(|c| c.name.as_str()).collect();

    // Attempt to fetch failing logs. We use run_id=0 as a sentinel here
    // (real wiring from watcher can supply an actual run_id; for the built-in
    // command we fall back to a descriptive message if logs aren't available).
    let log_section = match failing_logs(runner, cwd, 0).await {
        Ok(logs) if !logs.is_empty() => format!("Failing logs:\n{logs}"),
        _ => "Failing logs could not be retrieved.".to_string(),
    };

    let prompt = format!(
        "The following CI checks are failing for PR #{}:\n{}\n\n{}\n\nDiagnose and fix the failures.",
        pr_number,
        failing_names.join(", "),
        log_section,
    );

    Ok(prompt)
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
    // build_address_pr_comments_prompt
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn address_pr_comments_includes_each_comment() {
        let comments_json = r#"{
            "comments": [
                {"author": {"login": "alice"}, "body": "Fix the null check."},
                {"author": {"login": "bob"},   "body": "Add more tests please."}
            ]
        }"#;
        let mock = MockGh::new(vec![(
            "pr view 5 --json comments",
            Ok(comments_json.to_string()),
        )]);
        let prompt = build_address_pr_comments_prompt(&mock, Path::new("/tmp"), 5)
            .await
            .unwrap();

        assert!(prompt.contains("PR #5"));
        assert!(prompt.contains("Fix the null check."));
        assert!(prompt.contains("Add more tests please."));
        assert!(prompt.contains("alice"));
        assert!(prompt.contains("bob"));
    }

    #[tokio::test]
    async fn address_pr_comments_errors_when_no_comments() {
        let comments_json = r#"{"comments": []}"#;
        let mock = MockGh::new(vec![(
            "pr view 7 --json comments",
            Ok(comments_json.to_string()),
        )]);
        let result = build_address_pr_comments_prompt(&mock, Path::new("/tmp"), 7).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no review comments"));
    }

    #[tokio::test]
    async fn address_pr_comments_includes_path_and_line() {
        // API shape with path/line.
        let api_json = r#"[
            {"user": {"login": "rev"}, "path": "src/foo.rs", "line": 10, "body": "Wrong type."}
        ]"#;
        let mock = MockGh::new(vec![(
            "pr view 3 --json comments",
            Ok(api_json.to_string()),
        )]);
        let prompt = build_address_pr_comments_prompt(&mock, Path::new("/tmp"), 3)
            .await
            .unwrap();
        assert!(prompt.contains("src/foo.rs:10"));
        assert!(prompt.contains("Wrong type."));
    }

    // -----------------------------------------------------------------------
    // build_check_ci_prompt
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn check_ci_prompt_passing() {
        let checks_json = r#"[
            {"name": "build", "status": "completed", "conclusion": "success"}
        ]"#;
        let mock = MockGh::new(vec![("pr checks 8", Ok(checks_json.to_string()))]);
        let prompt = build_check_ci_prompt(&mock, Path::new("/tmp"), 8)
            .await
            .unwrap();
        assert!(
            prompt.contains("passing") || prompt.contains("green"),
            "expected passing message, got: {prompt}"
        );
    }

    #[tokio::test]
    async fn check_ci_prompt_failing_includes_names_and_logs() {
        let checks_json = r#"[
            {"name": "lint",  "status": "completed", "conclusion": "failure"},
            {"name": "build", "status": "completed", "conclusion": "success"}
        ]"#;
        let logs = "ERROR: lint failed on line 5\nExpected foo, got bar\n".to_string();
        let mock = MockGh::new(vec![
            ("pr checks 9", Ok(checks_json.to_string())),
            ("run view 0 --log-failed", Ok(logs.clone())),
        ]);
        let prompt = build_check_ci_prompt(&mock, Path::new("/tmp"), 9)
            .await
            .unwrap();
        assert!(prompt.contains("lint"), "missing failing check name");
        assert!(prompt.contains("ERROR: lint failed"), "missing log content");
        assert!(
            prompt.contains("Diagnose and fix"),
            "missing action request"
        );
    }
}

//! CI run status and failing log retrieval via `gh`.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::GhRunner;

// ---------------------------------------------------------------------------
// CiStatus / Check
// ---------------------------------------------------------------------------

/// A single CI check on a PR or branch.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    /// Final conclusion: `"success"`, `"failure"`, `"cancelled"`, `"timed_out"`, etc.
    /// `None` while the check is still in progress.
    pub conclusion: Option<String>,
    /// Current status: `"queued"`, `"in_progress"`, `"completed"`, etc.
    #[allow(dead_code)]
    pub status: String,
}

impl Check {
    /// Return `true` when this check has conclusively failed.
    pub fn is_failing(&self) -> bool {
        matches!(
            self.conclusion.as_deref(),
            Some("failure") | Some("cancelled") | Some("timed_out") | Some("action_required")
        )
    }
}

/// Aggregate CI status for a PR or branch.
#[derive(Debug, Clone)]
pub struct CiStatus {
    /// `true` only when every check has a `"success"` conclusion.
    pub all_passing: bool,
    pub checks: Vec<Check>,
}

// ---------------------------------------------------------------------------
// Raw serde shapes for `gh pr checks <pr> --json name,status,conclusion`
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCheck {
    name: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    status: Option<String>,
    // gh run list uses "workflowName" + "conclusion" + "status"
    #[serde(default, rename = "workflowName")]
    workflow_name: Option<String>,
}

// ---------------------------------------------------------------------------
// ci_status
// ---------------------------------------------------------------------------

/// Fetch the CI check status for PR `pr_number` in the repository at `cwd`.
///
/// Uses `gh pr checks <pr> --json name,status,conclusion`.
/// A check is considered failing when its `conclusion` is one of:
/// `failure`, `cancelled`, `timed_out`, `action_required`.
pub async fn ci_status(runner: &dyn GhRunner, cwd: &Path, pr_number: u64) -> Result<CiStatus> {
    let pr_str = pr_number.to_string();
    // `run_lenient`: `gh pr checks` exits non-zero when checks are pending or
    // failing (exactly the cases `i` exists to handle) but still emits the JSON.
    let stdout = runner
        .run_lenient(
            &["pr", "checks", &pr_str, "--json", "name,status,conclusion"],
            cwd,
        )
        .await
        .with_context(|| format!("gh pr checks {pr_number} failed"))?;

    parse_ci_status_json(&stdout)
}

fn parse_ci_status_json(stdout: &str) -> Result<CiStatus> {
    let raw: Vec<RawCheck> =
        serde_json::from_str(stdout).with_context(|| "failed to parse gh pr checks JSON")?;

    let checks: Vec<Check> = raw
        .into_iter()
        .map(|r| {
            let name = if r.name.is_empty() {
                r.workflow_name.unwrap_or_else(|| "unknown".to_string())
            } else {
                r.name
            };
            Check {
                name,
                conclusion: r.conclusion,
                status: r.status.unwrap_or_else(|| "unknown".to_string()),
            }
        })
        .collect();

    let all_passing = !checks.is_empty()
        && checks
            .iter()
            .all(|c| c.conclusion.as_deref() == Some("success"));

    Ok(CiStatus {
        all_passing,
        checks,
    })
}

// ---------------------------------------------------------------------------
// failing_logs
// ---------------------------------------------------------------------------

/// Maximum number of trailing bytes retained from a failing-run log.
///
/// Keeps prompt sizes bounded — `gh run view --log-failed` can produce
/// multi-megabyte output for large test suites.
const MAX_LOG_BYTES: usize = 4000;

/// Fetch the failing step logs for workflow run `run_id` in `cwd`.
///
/// Uses `gh run view <run_id> --log-failed`.
/// Output is truncated to the last [`MAX_LOG_BYTES`] bytes to keep prompts bounded.
pub async fn failing_logs(runner: &dyn GhRunner, cwd: &Path, run_id: u64) -> Result<String> {
    let run_id_str = run_id.to_string();
    let stdout = runner
        .run(&["run", "view", &run_id_str, "--log-failed"], cwd)
        .await
        .with_context(|| format!("gh run view {run_id} --log-failed failed"))?;

    if stdout.len() <= MAX_LOG_BYTES {
        Ok(stdout)
    } else {
        let truncated = &stdout[stdout.len() - MAX_LOG_BYTES..];
        // Find first newline so we don't start mid-line.
        let start = truncated.find('\n').map(|i| i + 1).unwrap_or(0);
        Ok(format!("... (truncated) ...\n{}", &truncated[start..]))
    }
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
    // ci_status tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ci_status_all_passing() {
        let json = r#"[
            {"name": "build", "status": "completed", "conclusion": "success"},
            {"name": "test",  "status": "completed", "conclusion": "success"}
        ]"#;
        let mock = MockGh::new(vec![("pr checks 1", Ok(json.to_string()))]);
        let status = ci_status(&mock, Path::new("/tmp"), 1).await.unwrap();
        assert!(status.all_passing);
        assert_eq!(status.checks.len(), 2);
    }

    #[tokio::test]
    async fn ci_status_with_failing_check() {
        let json = r#"[
            {"name": "build",  "status": "completed", "conclusion": "success"},
            {"name": "lint",   "status": "completed", "conclusion": "failure"},
            {"name": "deploy", "status": "in_progress", "conclusion": null}
        ]"#;
        let mock = MockGh::new(vec![("pr checks 2", Ok(json.to_string()))]);
        let status = ci_status(&mock, Path::new("/tmp"), 2).await.unwrap();
        assert!(!status.all_passing);
        let failing: Vec<_> = status.checks.iter().filter(|c| c.is_failing()).collect();
        assert_eq!(failing.len(), 1);
        assert_eq!(failing[0].name, "lint");
    }

    #[tokio::test]
    async fn ci_status_cancelled_check_is_failing() {
        let json = r#"[
            {"name": "integration", "status": "completed", "conclusion": "cancelled"}
        ]"#;
        let mock = MockGh::new(vec![("pr checks 3", Ok(json.to_string()))]);
        let status = ci_status(&mock, Path::new("/tmp"), 3).await.unwrap();
        assert!(!status.all_passing);
        assert!(status.checks[0].is_failing());
    }

    // -----------------------------------------------------------------------
    // failing_logs truncation test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn failing_logs_truncates_large_output() {
        // Generate output larger than MAX_LOG_BYTES.
        let big: String = "a\n".repeat(3000); // 6000 bytes
        let mock = MockGh::new(vec![("run view 99 --log-failed", Ok(big.clone()))]);
        let logs = failing_logs(&mock, Path::new("/tmp"), 99).await.unwrap();
        assert!(logs.len() <= MAX_LOG_BYTES + 30); // +30 for the "... (truncated) ..." prefix
        assert!(logs.contains("(truncated)"));
    }

    #[tokio::test]
    async fn failing_logs_short_output_unchanged() {
        let short = "some error\non line 42\n".to_string();
        let mock = MockGh::new(vec![("run view 5 --log-failed", Ok(short.clone()))]);
        let logs = failing_logs(&mock, Path::new("/tmp"), 5).await.unwrap();
        assert_eq!(logs, short);
    }
}

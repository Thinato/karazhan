//! Auto-discovery of a worktree's PR status via `gh`.
//!
//! A single `gh pr view --json …` call (no PR number) resolves the PR for the
//! branch currently checked out in the worktree's cwd, then [`classify`] maps
//! the raw fields onto a [`PrStatus`].  Detached HEAD / no PR for the branch →
//! `Ok(None)` (NOT an error).

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::GhRunner;
use crate::worktree::model::PrStatus;

// ---------------------------------------------------------------------------
// PrInfo
// ---------------------------------------------------------------------------

/// Resolved PR info for a worktree's current branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrInfo {
    pub number: u64,
    pub status: PrStatus,
}

// ---------------------------------------------------------------------------
// Raw serde shapes for `gh pr view --json number,state,isDraft,mergedAt,statusCheckRollup`
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPrView {
    number: u64,
    state: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    status_check_rollup: Vec<RawRollupCheck>,
}

/// A single entry in `statusCheckRollup`.  `gh pr view` reports UPPERCASE
/// values (e.g. `status: "COMPLETED"`, `conclusion: "SUCCESS"`); we compare
/// case-insensitively so the same mapping survives lowercase variants.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRollupCheck {
    #[serde(default)]
    conclusion: Option<String>,
}

// ---------------------------------------------------------------------------
// classify — pure mapping
// ---------------------------------------------------------------------------

/// A single check's conclusion (case-insensitive string), used by [`classify`].
///
/// We classify purely on `conclusion`: a check with no conclusion yet is still
/// in progress, which keeps an OPEN PR at `Open` (not `ChecksPassing`).
#[derive(Debug, Clone)]
pub struct CheckSummary {
    pub conclusion: Option<String>,
}

impl CheckSummary {
    /// True when the check has conclusively failed.
    fn is_failing(&self) -> bool {
        matches!(
            self.conclusion
                .as_deref()
                .map(str::to_ascii_uppercase)
                .as_deref(),
            Some("FAILURE") | Some("CANCELLED") | Some("TIMED_OUT") | Some("ACTION_REQUIRED")
        )
    }

    /// True when the check has concluded successfully.
    fn is_success(&self) -> bool {
        self.conclusion
            .as_deref()
            .map(str::to_ascii_uppercase)
            .as_deref()
            == Some("SUCCESS")
    }
}

/// Pure mapping from the raw PR fields to a [`PrStatus`].
///
/// - MERGED → `Merged`
/// - CLOSED (not merged) → `Closed`
/// - OPEN + draft → `Draft`
/// - OPEN + any failing check → `ChecksFailing`
/// - OPEN + non-empty rollup, every check concluded SUCCESS → `ChecksPassing`
/// - OPEN otherwise (no checks, or some pending/in-progress) → `Open`
pub fn classify(state: &str, merged: bool, is_draft: bool, checks: &[CheckSummary]) -> PrStatus {
    if merged {
        return PrStatus::Merged;
    }
    match state.to_ascii_uppercase().as_str() {
        "MERGED" => PrStatus::Merged,
        "CLOSED" => PrStatus::Closed,
        "OPEN" => {
            if is_draft {
                return PrStatus::Draft;
            }
            if checks.iter().any(CheckSummary::is_failing) {
                return PrStatus::ChecksFailing;
            }
            if !checks.is_empty() && checks.iter().all(CheckSummary::is_success) {
                return PrStatus::ChecksPassing;
            }
            PrStatus::Open
        }
        // Unknown state → treat as a plain open PR (conservative).
        _ => PrStatus::Open,
    }
}

// ---------------------------------------------------------------------------
// fetch_pr_status
// ---------------------------------------------------------------------------

/// Fetch the PR status for the branch currently checked out in `cwd`.
///
/// Runs ONE `gh pr view --json number,state,isDraft,mergedAt,statusCheckRollup`
/// call (no PR number — gh resolves the current branch's PR).  Returns:
/// - `Ok(Some(PrInfo))` when a PR exists,
/// - `Ok(None)` when there is no PR for the branch / detached HEAD (gh exits
///   non-zero with a "no pull requests found" / "could not find" message),
/// - `Err(_)` only on unexpected failures (gh missing, auth, parse errors).
pub async fn fetch_pr_status(runner: &dyn GhRunner, cwd: &Path) -> Result<Option<PrInfo>> {
    let result = runner
        .run(
            &[
                "pr",
                "view",
                "--json",
                "number,state,isDraft,mergedAt,statusCheckRollup",
            ],
            cwd,
        )
        .await;

    let stdout = match result {
        Ok(s) => s,
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            // No PR for the branch (incl. detached HEAD) → Ok(None), not an error.
            if msg.contains("no pull request")
                || msg.contains("could not find")
                || msg.contains("no open pull")
            {
                return Ok(None);
            }
            return Err(e);
        }
    };

    let raw: RawPrView = serde_json::from_str(&stdout)
        .with_context(|| "failed to parse gh pr view --json (pr_status) JSON")?;

    let checks: Vec<CheckSummary> = raw
        .status_check_rollup
        .into_iter()
        .map(|c| CheckSummary {
            conclusion: c.conclusion,
        })
        .collect();

    let status = classify(&raw.state, raw.merged_at.is_some(), raw.is_draft, &checks);

    Ok(Some(PrInfo {
        number: raw.number,
        status,
    }))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::mock::MockGh;
    use std::path::Path;

    fn checks(pairs: &[(&str, Option<&str>)]) -> Vec<CheckSummary> {
        pairs
            .iter()
            .map(|(_status, conclusion)| CheckSummary {
                conclusion: conclusion.map(|c| c.to_string()),
            })
            .collect()
    }

    // -- classify -------------------------------------------------------------

    #[test]
    fn classify_merged() {
        assert_eq!(classify("MERGED", true, false, &[]), PrStatus::Merged);
        // merged flag wins even if state somehow disagrees.
        assert_eq!(classify("OPEN", true, false, &[]), PrStatus::Merged);
    }

    #[test]
    fn classify_closed_unmerged() {
        assert_eq!(classify("CLOSED", false, false, &[]), PrStatus::Closed);
    }

    #[test]
    fn classify_draft() {
        assert_eq!(classify("OPEN", false, true, &[]), PrStatus::Draft);
        // Draft takes precedence over checks.
        let c = checks(&[("COMPLETED", Some("FAILURE"))]);
        assert_eq!(classify("OPEN", false, true, &c), PrStatus::Draft);
    }

    #[test]
    fn classify_failing() {
        let c = checks(&[
            ("COMPLETED", Some("SUCCESS")),
            ("COMPLETED", Some("FAILURE")),
        ]);
        assert_eq!(classify("OPEN", false, false, &c), PrStatus::ChecksFailing);

        for conclusion in ["CANCELLED", "TIMED_OUT", "ACTION_REQUIRED"] {
            let c = checks(&[("COMPLETED", Some(conclusion))]);
            assert_eq!(classify("OPEN", false, false, &c), PrStatus::ChecksFailing);
        }
    }

    #[test]
    fn classify_passing() {
        let c = checks(&[
            ("COMPLETED", Some("SUCCESS")),
            ("COMPLETED", Some("SUCCESS")),
        ]);
        assert_eq!(classify("OPEN", false, false, &c), PrStatus::ChecksPassing);
        // Case-insensitive.
        let c = checks(&[("completed", Some("success"))]);
        assert_eq!(classify("open", false, false, &c), PrStatus::ChecksPassing);
    }

    #[test]
    fn classify_open_pending() {
        // Some checks still in progress → Open (not passing yet).
        let c = checks(&[("COMPLETED", Some("SUCCESS")), ("IN_PROGRESS", None)]);
        assert_eq!(classify("OPEN", false, false, &c), PrStatus::Open);
    }

    #[test]
    fn classify_open_no_checks() {
        assert_eq!(classify("OPEN", false, false, &[]), PrStatus::Open);
    }

    // -- fetch_pr_status ------------------------------------------------------

    #[tokio::test]
    async fn fetch_open_with_passing() {
        let json = r#"{
            "number": 42,
            "state": "OPEN",
            "isDraft": false,
            "mergedAt": null,
            "statusCheckRollup": [
                {"status": "COMPLETED", "conclusion": "SUCCESS"},
                {"status": "COMPLETED", "conclusion": "SUCCESS"}
            ]
        }"#;
        let mock = MockGh::new(vec![("pr view --json", Ok(json.to_string()))]);
        let info = fetch_pr_status(&mock, Path::new("/tmp"))
            .await
            .unwrap()
            .expect("Some");
        assert_eq!(info.number, 42);
        assert_eq!(info.status, PrStatus::ChecksPassing);
    }

    #[tokio::test]
    async fn fetch_no_pr_is_none() {
        let mock = MockGh::new(vec![(
            "pr view --json",
            Err(anyhow::anyhow!(
                "no pull requests found for branch \"feat\""
            )),
        )]);
        let info = fetch_pr_status(&mock, Path::new("/tmp")).await.unwrap();
        assert_eq!(info, None);
    }

    #[tokio::test]
    async fn fetch_merged_is_merged() {
        let json = r#"{
            "number": 7,
            "state": "MERGED",
            "isDraft": false,
            "mergedAt": "2024-01-15T10:00:00Z",
            "statusCheckRollup": []
        }"#;
        let mock = MockGh::new(vec![("pr view --json", Ok(json.to_string()))]);
        let info = fetch_pr_status(&mock, Path::new("/tmp"))
            .await
            .unwrap()
            .expect("Some");
        assert_eq!(info.number, 7);
        assert_eq!(info.status, PrStatus::Merged);
    }
}

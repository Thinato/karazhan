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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrInfo {
    pub number: u64,
    pub status: PrStatus,
    /// Canonical GitHub URL for the PR (e.g. `https://github.com/owner/repo/pull/42`).
    pub url: Option<String>,
    /// PR title, as returned by `gh pr view --json title`.
    pub title: Option<String>,
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
    /// GitHub review decision: "APPROVED" | "CHANGES_REQUESTED" |
    /// "REVIEW_REQUIRED" | null.  Present when `--json reviewDecision` is
    /// included in the gh call.
    #[serde(default)]
    review_decision: Option<String>,
    /// Canonical HTML URL for the PR.
    #[serde(default)]
    url: Option<String>,
    /// PR title.
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status_check_rollup: Vec<RawRollupCheck>,
}

/// A single entry in `statusCheckRollup`.  `gh pr view` reports UPPERCASE
/// values (e.g. `status: "COMPLETED"`, `conclusion: "SUCCESS"`); we compare
/// case-insensitively so the same mapping survives lowercase variants.
///
/// Two heterogeneous types appear in the rollup:
/// - **CheckRun**: has `status` (QUEUED / IN_PROGRESS / COMPLETED) and
///   `conclusion` (SUCCESS / FAILURE / CANCELLED / TIMED_OUT / …).
/// - **StatusContext**: has `state` (PENDING / SUCCESS / FAILURE / ERROR)
///   instead of `status` / `conclusion`.
///
/// All three fields are optional so both types deserialise into this struct.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRollupCheck {
    /// CheckRun: QUEUED | IN_PROGRESS | COMPLETED (absent for StatusContext).
    #[serde(default)]
    status: Option<String>,
    /// CheckRun: SUCCESS | FAILURE | CANCELLED | TIMED_OUT | ACTION_REQUIRED | …
    /// (absent when still running, and absent for StatusContext).
    #[serde(default)]
    conclusion: Option<String>,
    /// StatusContext: PENDING | SUCCESS | FAILURE | ERROR
    /// (absent for CheckRun entries).
    #[serde(default)]
    state: Option<String>,
}

// ---------------------------------------------------------------------------
// classify — pure mapping
// ---------------------------------------------------------------------------

/// Normalised kind for a single rollup entry, derived from the raw fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckKind {
    Failing,
    Running,
    Success,
}

/// A single check entry from `statusCheckRollup`, normalised for [`classify`].
///
/// Constructed from [`RawRollupCheck`] by [`normalize_check`].
#[derive(Debug, Clone)]
pub struct CheckSummary {
    kind: CheckKind,
}

impl CheckSummary {
    fn is_failing(&self) -> bool {
        self.kind == CheckKind::Failing
    }
    fn is_running(&self) -> bool {
        self.kind == CheckKind::Running
    }
    fn is_success(&self) -> bool {
        self.kind == CheckKind::Success
    }
}

/// Normalise a [`RawRollupCheck`] into a [`CheckSummary`].
///
/// Precedence:
/// 1. If `state` is present → StatusContext path:
///    - PENDING → Running; SUCCESS → Success; FAILURE / ERROR → Failing.
/// 2. Otherwise → CheckRun path using `status` and `conclusion`:
///    - status != COMPLETED (or absent) → Running.
///    - status == COMPLETED, conclusion FAILURE / CANCELLED / TIMED_OUT /
///      ACTION_REQUIRED → Failing.
///    - status == COMPLETED, conclusion SUCCESS / NEUTRAL / SKIPPED → Success.
///    - status == COMPLETED, anything else → Running (conservative).
fn normalize_check(raw: &RawRollupCheck) -> CheckSummary {
    // StatusContext branch: `state` present.
    if let Some(ref s) = raw.state {
        let kind = match s.to_ascii_uppercase().as_str() {
            "SUCCESS" => CheckKind::Success,
            "FAILURE" | "ERROR" => CheckKind::Failing,
            // PENDING and anything unknown → Running.
            _ => CheckKind::Running,
        };
        return CheckSummary { kind };
    }

    // CheckRun branch.
    let status_upper = raw
        .status
        .as_deref()
        .map(str::to_ascii_uppercase)
        .unwrap_or_default();

    if status_upper != "COMPLETED" {
        // IN_PROGRESS, QUEUED, PENDING, WAITING, REQUESTED, or absent → Running.
        return CheckSummary {
            kind: CheckKind::Running,
        };
    }

    // COMPLETED — classify by conclusion.
    let conclusion_upper = raw
        .conclusion
        .as_deref()
        .map(str::to_ascii_uppercase)
        .unwrap_or_default();

    let kind = match conclusion_upper.as_str() {
        "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" => CheckKind::Failing,
        "SUCCESS" | "NEUTRAL" | "SKIPPED" => CheckKind::Success,
        // COMPLETED but unknown / missing conclusion → conservative Running.
        _ => CheckKind::Running,
    };
    CheckSummary { kind }
}

/// Pure mapping from raw PR fields to a [`PrStatus`].
///
/// Precedence for an OPEN non-merged non-draft PR:
/// 1. any check FAILING → `ChecksFailing`
/// 2. any check RUNNING (and none failing) → `ChecksRunning`
/// 3. `review_decision == "APPROVED"` (case-insensitive) → `Approved`
/// 4. rollup non-empty AND all SUCCESS → `ChecksPassing`
/// 5. otherwise (empty rollup / no checks) → `Open`
///
/// MERGED / CLOSED / draft short-circuit before checks or review are examined.
pub fn classify(
    state: &str,
    merged: bool,
    is_draft: bool,
    review_decision: Option<&str>,
    checks: &[CheckSummary],
) -> PrStatus {
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
            // Failing beats everything else.
            if checks.iter().any(CheckSummary::is_failing) {
                return PrStatus::ChecksFailing;
            }
            // Running beats approval (CI still in flight).
            if checks.iter().any(CheckSummary::is_running) {
                return PrStatus::ChecksRunning;
            }
            // Explicit approval (all checks passed or no checks).
            let is_approved = review_decision
                .map(|d| d.eq_ignore_ascii_case("APPROVED"))
                .unwrap_or(false);
            if is_approved {
                return PrStatus::Approved;
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
                "number,state,isDraft,mergedAt,reviewDecision,url,title,statusCheckRollup",
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
        .iter()
        .map(normalize_check)
        .collect();

    let status = classify(
        &raw.state,
        raw.merged_at.is_some(),
        raw.is_draft,
        raw.review_decision.as_deref(),
        &checks,
    );

    Ok(Some(PrInfo {
        number: raw.number,
        status,
        url: raw.url,
        title: raw.title,
    }))
}

// ---------------------------------------------------------------------------
// fetch_unresolved_count — GraphQL unresolved review-thread count
// ---------------------------------------------------------------------------

/// GraphQL query fetching the first 100 review threads of a PR plus the total
/// count.  Single page — see [`count_unresolved`] for the >100 cap behaviour.
const UNRESOLVED_QUERY: &str = "query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){pullRequest(number:$number){reviewThreads(first:100){totalCount nodes{isResolved}}}}}";

/// Raw serde shape for the `gh api graphql` reviewThreads response.
#[derive(Deserialize)]
struct RawGraphQl {
    data: RawGraphQlData,
}

#[derive(Deserialize)]
struct RawGraphQlData {
    repository: RawRepository,
}

#[derive(Deserialize)]
struct RawRepository {
    #[serde(rename = "pullRequest")]
    pull_request: RawPullRequest,
}

#[derive(Deserialize)]
struct RawPullRequest {
    #[serde(rename = "reviewThreads")]
    review_threads: RawReviewThreads,
}

#[derive(Deserialize)]
struct RawReviewThreads {
    #[serde(rename = "totalCount")]
    total_count: u64,
    nodes: Vec<RawReviewThread>,
}

#[derive(Deserialize)]
struct RawReviewThread {
    #[serde(rename = "isResolved")]
    is_resolved: bool,
}

/// Pure parse helper: given the JSON returned by `gh api graphql`, count the
/// review threads whose `isResolved == false`.
///
/// When `totalCount` exceeds the number of returned `nodes` (i.e. the PR has
/// more than the first 100 review threads), logs a `debug` note that the count
/// is capped at the first 100 — we deliberately do NOT paginate for now.
pub fn count_unresolved(json: &str) -> Result<u64> {
    let parsed: RawGraphQl = serde_json::from_str(json)
        .with_context(|| "failed to parse gh api graphql reviewThreads JSON")?;
    let threads = parsed.data.repository.pull_request.review_threads;

    if threads.total_count > threads.nodes.len() as u64 {
        tracing::debug!(
            total = threads.total_count,
            fetched = threads.nodes.len(),
            "unresolved review-thread count capped at first 100 threads (not paginating)"
        );
    }

    let unresolved = threads.nodes.iter().filter(|t| !t.is_resolved).count() as u64;
    Ok(unresolved)
}

/// Fetch the number of UNRESOLVED review threads (GitHub's "Resolve
/// conversation" state) for PR `number` in `owner/repo`, via a single
/// `gh api graphql` call run in `cwd`.
///
/// Only one page (`reviewThreads(first:100)`) is fetched; counts beyond 100
/// threads are capped (logged in [`count_unresolved`]).  Returns `Err` on any
/// gh failure or parse error so the caller can log + skip and leave the
/// previously-known count unchanged.
pub async fn fetch_unresolved_count(
    runner: &dyn GhRunner,
    cwd: &Path,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<u64> {
    let query_arg = format!("query={UNRESOLVED_QUERY}");
    let owner_arg = format!("owner={owner}");
    let repo_arg = format!("repo={repo}");
    let number_arg = format!("number={number}");

    let stdout = runner
        .run(
            &[
                "api",
                "graphql",
                "-f",
                &query_arg,
                "-f",
                &owner_arg,
                "-f",
                &repo_arg,
                "-F",
                &number_arg,
            ],
            cwd,
        )
        .await
        .with_context(|| "gh api graphql (reviewThreads) failed")?;

    count_unresolved(&stdout)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::mock::MockGh;
    use std::path::Path;

    /// Build `CheckSummary` entries from (status, conclusion, state) tuples,
    /// mirroring what `normalize_check` produces from the raw JSON fields.
    fn raw_checks(entries: &[(&str, Option<&str>, Option<&str>)]) -> Vec<CheckSummary> {
        entries
            .iter()
            .map(|(status, conclusion, state)| {
                let raw = RawRollupCheck {
                    status: Some(status.to_string()),
                    conclusion: conclusion.map(|s| s.to_string()),
                    state: state.map(|s| s.to_string()),
                };
                normalize_check(&raw)
            })
            .collect()
    }

    // Convenience: CheckRun-style entries (status + conclusion, no state).
    fn checkrun(status: &str, conclusion: Option<&str>) -> CheckSummary {
        normalize_check(&RawRollupCheck {
            status: Some(status.to_string()),
            conclusion: conclusion.map(str::to_string),
            state: None,
        })
    }

    // Convenience: StatusContext-style entries (state only).
    fn statusctx(state: &str) -> CheckSummary {
        normalize_check(&RawRollupCheck {
            status: None,
            conclusion: None,
            state: Some(state.to_string()),
        })
    }

    // -- classify -------------------------------------------------------------

    #[test]
    fn classify_merged() {
        assert_eq!(classify("MERGED", true, false, None, &[]), PrStatus::Merged);
        // merged flag wins even if state somehow disagrees.
        assert_eq!(classify("OPEN", true, false, None, &[]), PrStatus::Merged);
    }

    #[test]
    fn classify_closed_unmerged() {
        assert_eq!(
            classify("CLOSED", false, false, None, &[]),
            PrStatus::Closed
        );
    }

    #[test]
    fn classify_draft() {
        assert_eq!(classify("OPEN", false, true, None, &[]), PrStatus::Draft);
        // Draft takes precedence over checks.
        let c = vec![checkrun("COMPLETED", Some("FAILURE"))];
        assert_eq!(classify("OPEN", false, true, None, &c), PrStatus::Draft);
    }

    #[test]
    fn classify_failing() {
        let c = vec![
            checkrun("COMPLETED", Some("SUCCESS")),
            checkrun("COMPLETED", Some("FAILURE")),
        ];
        assert_eq!(
            classify("OPEN", false, false, None, &c),
            PrStatus::ChecksFailing
        );

        for conclusion in ["CANCELLED", "TIMED_OUT", "ACTION_REQUIRED"] {
            let c = vec![checkrun("COMPLETED", Some(conclusion))];
            assert_eq!(
                classify("OPEN", false, false, None, &c),
                PrStatus::ChecksFailing
            );
        }
    }

    #[test]
    fn classify_failing_wins_over_running() {
        // A failing check + a running check → ChecksFailing (failing wins).
        let c = vec![
            checkrun("COMPLETED", Some("FAILURE")),
            checkrun("IN_PROGRESS", None),
        ];
        assert_eq!(
            classify("OPEN", false, false, None, &c),
            PrStatus::ChecksFailing
        );
    }

    #[test]
    fn classify_passing() {
        let c = vec![
            checkrun("COMPLETED", Some("SUCCESS")),
            checkrun("COMPLETED", Some("SUCCESS")),
        ];
        assert_eq!(
            classify("OPEN", false, false, None, &c),
            PrStatus::ChecksPassing
        );
        // Case-insensitive via normalize_check.
        let c = raw_checks(&[("COMPLETED", Some("success"), None)]);
        assert_eq!(
            classify("open", false, false, None, &c),
            PrStatus::ChecksPassing
        );
    }

    #[test]
    fn classify_neutral_and_skipped_count_as_success() {
        let c = vec![
            checkrun("COMPLETED", Some("NEUTRAL")),
            checkrun("COMPLETED", Some("SKIPPED")),
            checkrun("COMPLETED", Some("SUCCESS")),
        ];
        assert_eq!(
            classify("OPEN", false, false, None, &c),
            PrStatus::ChecksPassing
        );
    }

    #[test]
    fn classify_checks_running_in_progress() {
        // One IN_PROGRESS + one SUCCESS → ChecksRunning.
        let c = vec![
            checkrun("COMPLETED", Some("SUCCESS")),
            checkrun("IN_PROGRESS", None),
        ];
        assert_eq!(
            classify("OPEN", false, false, None, &c),
            PrStatus::ChecksRunning
        );
    }

    #[test]
    fn classify_checks_running_queued() {
        let c = vec![checkrun("QUEUED", None)];
        assert_eq!(
            classify("OPEN", false, false, None, &c),
            PrStatus::ChecksRunning
        );
    }

    #[test]
    fn classify_checks_running_statuscontext_pending() {
        // StatusContext with state=PENDING → running.
        let c = vec![statusctx("PENDING")];
        assert_eq!(
            classify("OPEN", false, false, None, &c),
            PrStatus::ChecksRunning
        );
    }

    #[test]
    fn classify_statuscontext_success() {
        let c = vec![statusctx("SUCCESS")];
        assert_eq!(
            classify("OPEN", false, false, None, &c),
            PrStatus::ChecksPassing
        );
    }

    #[test]
    fn classify_statuscontext_failure() {
        let c = vec![statusctx("FAILURE")];
        assert_eq!(
            classify("OPEN", false, false, None, &c),
            PrStatus::ChecksFailing
        );
    }

    #[test]
    fn classify_statuscontext_error() {
        let c = vec![statusctx("ERROR")];
        assert_eq!(
            classify("OPEN", false, false, None, &c),
            PrStatus::ChecksFailing
        );
    }

    #[test]
    fn classify_open_no_checks() {
        assert_eq!(classify("OPEN", false, false, None, &[]), PrStatus::Open);
    }

    // -- Approved / reviewDecision tests -------------------------------------

    #[test]
    fn classify_approved_all_success_gives_approved() {
        let c = vec![
            checkrun("COMPLETED", Some("SUCCESS")),
            checkrun("COMPLETED", Some("SUCCESS")),
        ];
        assert_eq!(
            classify("OPEN", false, false, Some("APPROVED"), &c),
            PrStatus::Approved
        );
    }

    #[test]
    fn classify_approved_running_check_gives_checks_running() {
        // Running beats approval.
        let c = vec![
            checkrun("COMPLETED", Some("SUCCESS")),
            checkrun("IN_PROGRESS", None),
        ];
        assert_eq!(
            classify("OPEN", false, false, Some("APPROVED"), &c),
            PrStatus::ChecksRunning
        );
    }

    #[test]
    fn classify_approved_failing_check_gives_checks_failing() {
        // Failing beats approval.
        let c = vec![
            checkrun("COMPLETED", Some("FAILURE")),
            checkrun("COMPLETED", Some("SUCCESS")),
        ];
        assert_eq!(
            classify("OPEN", false, false, Some("APPROVED"), &c),
            PrStatus::ChecksFailing
        );
    }

    #[test]
    fn classify_null_review_decision_all_success_gives_checks_passing() {
        // No reviewDecision → still ChecksPassing when all checks pass.
        let c = vec![checkrun("COMPLETED", Some("SUCCESS"))];
        assert_eq!(
            classify("OPEN", false, false, None, &c),
            PrStatus::ChecksPassing
        );
    }

    #[test]
    fn classify_changes_requested_all_success_gives_checks_passing() {
        // Only APPROVED triggers Approved; CHANGES_REQUESTED still gives ChecksPassing.
        let c = vec![checkrun("COMPLETED", Some("SUCCESS"))];
        assert_eq!(
            classify("OPEN", false, false, Some("CHANGES_REQUESTED"), &c),
            PrStatus::ChecksPassing
        );
    }

    #[test]
    fn classify_approved_empty_rollup_gives_approved() {
        // Approval beats no-checks (Open).
        assert_eq!(
            classify("OPEN", false, false, Some("APPROVED"), &[]),
            PrStatus::Approved
        );
    }

    #[test]
    fn classify_approved_case_insensitive() {
        // "approved" (lowercase) should still match.
        let c = vec![checkrun("COMPLETED", Some("SUCCESS"))];
        assert_eq!(
            classify("OPEN", false, false, Some("approved"), &c),
            PrStatus::Approved
        );
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
        assert_eq!(info.url, None);
    }

    #[tokio::test]
    async fn fetch_open_with_in_progress_check_is_checks_running() {
        let json = r#"{
            "number": 55,
            "state": "OPEN",
            "isDraft": false,
            "mergedAt": null,
            "statusCheckRollup": [
                {"status": "COMPLETED", "conclusion": "SUCCESS"},
                {"status": "IN_PROGRESS", "conclusion": null}
            ]
        }"#;
        let mock = MockGh::new(vec![("pr view --json", Ok(json.to_string()))]);
        let info = fetch_pr_status(&mock, Path::new("/tmp"))
            .await
            .unwrap()
            .expect("Some");
        assert_eq!(info.number, 55);
        assert_eq!(info.status, PrStatus::ChecksRunning);
        assert_eq!(info.url, None);
    }

    #[tokio::test]
    async fn fetch_approved_with_passing_checks_gives_approved() {
        let json = r#"{
            "number": 99,
            "state": "OPEN",
            "isDraft": false,
            "mergedAt": null,
            "reviewDecision": "APPROVED",
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
        assert_eq!(info.number, 99);
        assert_eq!(info.status, PrStatus::Approved);
        assert_eq!(info.url, None);
    }

    #[tokio::test]
    async fn fetch_pr_url_is_captured() {
        let json = r#"{
            "number": 7,
            "state": "OPEN",
            "isDraft": false,
            "mergedAt": null,
            "url": "https://github.com/owner/repo/pull/7",
            "statusCheckRollup": []
        }"#;
        let mock = MockGh::new(vec![("pr view --json", Ok(json.to_string()))]);
        let info = fetch_pr_status(&mock, Path::new("/tmp"))
            .await
            .unwrap()
            .expect("Some");
        assert_eq!(info.number, 7);
        assert_eq!(
            info.url,
            Some("https://github.com/owner/repo/pull/7".to_string())
        );
    }

    #[tokio::test]
    async fn fetch_pr_title_is_captured() {
        let json = r#"{
            "number": 11,
            "state": "OPEN",
            "isDraft": false,
            "mergedAt": null,
            "url": "https://github.com/owner/repo/pull/11",
            "title": "Fix the widget renderer",
            "statusCheckRollup": []
        }"#;
        let mock = MockGh::new(vec![("pr view --json", Ok(json.to_string()))]);
        let info = fetch_pr_status(&mock, Path::new("/tmp"))
            .await
            .unwrap()
            .expect("Some");
        assert_eq!(info.number, 11);
        assert_eq!(info.title, Some("Fix the widget renderer".to_string()));
    }

    #[tokio::test]
    async fn fetch_pr_title_absent_is_none() {
        // JSON without a "title" field — the serde default kicks in.
        let json = r#"{
            "number": 3,
            "state": "OPEN",
            "isDraft": false,
            "mergedAt": null,
            "statusCheckRollup": []
        }"#;
        let mock = MockGh::new(vec![("pr view --json", Ok(json.to_string()))]);
        let info = fetch_pr_status(&mock, Path::new("/tmp"))
            .await
            .unwrap()
            .expect("Some");
        assert_eq!(info.title, None);
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
        assert_eq!(info.url, None);
    }

    // -- count_unresolved (pure parse) ---------------------------------------

    fn graphql_threads(total: u64, resolved_flags: &[bool]) -> String {
        let nodes: Vec<String> = resolved_flags
            .iter()
            .map(|r| format!(r#"{{"isResolved":{r}}}"#))
            .collect();
        format!(
            r#"{{"data":{{"repository":{{"pullRequest":{{"reviewThreads":{{"totalCount":{total},"nodes":[{}]}}}}}}}}}}"#,
            nodes.join(",")
        )
    }

    #[test]
    fn count_unresolved_two_of_three() {
        // 3 threads: 2 unresolved (false), 1 resolved (true) → 2.
        let json = graphql_threads(3, &[false, false, true]);
        assert_eq!(count_unresolved(&json).unwrap(), 2);
    }

    #[test]
    fn count_unresolved_all_resolved_is_zero() {
        let json = graphql_threads(2, &[true, true]);
        assert_eq!(count_unresolved(&json).unwrap(), 0);
    }

    #[test]
    fn count_unresolved_empty_is_zero() {
        let json = graphql_threads(0, &[]);
        assert_eq!(count_unresolved(&json).unwrap(), 0);
    }

    #[test]
    fn count_unresolved_over_100_caps_at_fetched_nodes() {
        // totalCount > nodes.len() → debug log + count only fetched nodes.
        // 2 fetched nodes, both unresolved, but totalCount claims 150.
        let json = graphql_threads(150, &[false, false]);
        assert_eq!(count_unresolved(&json).unwrap(), 2);
    }

    #[test]
    fn count_unresolved_malformed_is_err() {
        assert!(count_unresolved("not json").is_err());
    }

    // -- fetch_unresolved_count via MockGh -----------------------------------

    #[tokio::test]
    async fn fetch_unresolved_count_two_of_three() {
        let json = graphql_threads(3, &[false, true, false]);
        let mock = MockGh::new(vec![("api graphql", Ok(json))]);
        let n = fetch_unresolved_count(&mock, Path::new("/tmp"), "owner", "repo", 42)
            .await
            .unwrap();
        assert_eq!(n, 2);
    }

    #[tokio::test]
    async fn fetch_unresolved_count_gh_error_is_err() {
        let mock = MockGh::new(vec![(
            "api graphql",
            Err(anyhow::anyhow!("gh api graphql failed (exit 1): bad creds")),
        )]);
        let result = fetch_unresolved_count(&mock, Path::new("/tmp"), "owner", "repo", 42).await;
        assert!(result.is_err());
    }
}

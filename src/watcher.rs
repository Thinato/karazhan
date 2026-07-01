//! Background poller: auto-discovers each worktree's PR status by branch.
//!
//! The watcher runs as a detached tokio task that ticks every [`WatcherConfig::interval`].
//! For EVERY worktree in the shared watch-set it calls [`fetch_pr_status`] (one `gh pr
//! view` per worktree, resolving the PR from the worktree's current branch) and emits a
//! [`AppEvent::PrStatusChanged`] whenever the observed [`PrStatus`] changes.
//!
//! # Watch-set sharing
//! The daemon owns an `Arc<Mutex<Vec<WatchItem>>>` that it updates after every registry
//! rebuild.  The watcher reads this shared list on each tick so it always polls the
//! current set of worktrees without a separate channel handshake.
//!
//! # Error handling
//! Errors for individual worktrees are logged as warnings and skipped — one bad worktree
//! never stops the poll of others, and the watcher never crashes.  A worktree with no PR
//! (detached / unopened) is `NoPr` and is not an error.
//!
//! # Shutdown
//! The caller passes a `tokio::sync::watch::Receiver<bool>` (shutdown signal).  The
//! watcher `select!`s between the interval tick and a change on the receiver; when the
//! receiver sees `true` the loop exits cleanly.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time;

use crate::app::AppEvent;
use crate::github::pr_status::{fetch_pr_status, fetch_unresolved_count};
use crate::github::GhRunner;
use crate::pr_status_store::{self, PrStatusEntry};
use crate::worktree::model::PrStatus;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Configuration for the background watcher task.
pub struct WatcherConfig {
    /// How often to poll `gh` for each active worktree.
    pub interval: Duration,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30),
        }
    }
}

/// A single entry in the shared watch-set: a worktree path plus the GitHub
/// coordinates (owner/repo) of its repository.
///
/// PR discovery is by branch (gh resolves the PR from the worktree's current
/// branch), so no PR number is stored here.  `owner`/`repo` are computed ONCE
/// by the daemon when building the watch-set (no per-tick `git` call) and are
/// `None` when the repo has no parseable GitHub remote — in which case the
/// unresolved-comment GraphQL call is skipped.
#[derive(Debug, Clone)]
pub struct WatchItem {
    pub worktree_path: PathBuf,
    /// Owning project's repo root.  The watcher persists this worktree's PR
    /// status under `<project_root>/.karazhan/pr_status.toml` so a separate
    /// session daemon can read it without calling `gh` itself.
    pub project_root: PathBuf,
    pub owner: Option<String>,
    pub repo: Option<String>,
}

// ---------------------------------------------------------------------------
// Change-detection (pure, easily unit-tested)
// ---------------------------------------------------------------------------

/// An event produced when a worktree's observed [`PrStatus`] OR its unresolved
/// review-comment count changes.
#[derive(Debug, PartialEq, Eq)]
pub enum WatchEvent {
    PrStatusChanged {
        worktree_path: PathBuf,
        pr_status: PrStatus,
        pr_number: Option<u64>,
        pr_url: Option<String>,
        pr_title: Option<String>,
        unresolved_comments: Option<u64>,
    },
}

/// Pure change-detection: should we emit an event given the previous observed
/// `(status, unresolved)` (`None` on the first tick) and the current pair?
///
/// We emit on any real change — EITHER the PR status OR the unresolved count
/// changing (so a brand-new comment alone repaints).  The first observation
/// (`prev == None`) always emits — the registry default is `Loading` (not
/// `NoPr`), so even a `NoPr` result from the first poll must replace the
/// `Loading` placeholder.  Extracted so tests can cover transitions without I/O.
pub fn diff_pr(prev: Option<(PrStatus, Option<u64>)>, curr: (PrStatus, Option<u64>)) -> bool {
    match prev {
        None => true,
        Some(p) => p != curr,
    }
}

// ---------------------------------------------------------------------------
// Auto-continue gate (pure helper)
// ---------------------------------------------------------------------------

/// Return `true` when a worktree should trigger the auto-continue flow after
/// its PR is merged.  This is intentionally a plain function so tests can
/// exercise it without constructing a full `Worktree`.
#[allow(dead_code)]
pub fn should_auto_continue(auto_continue_on_merge: bool) -> bool {
    auto_continue_on_merge
}

// ---------------------------------------------------------------------------
// Watcher task
// ---------------------------------------------------------------------------

/// Spawn the background watcher task.
///
/// # Parameters
/// - `runner`: shared `gh` implementation (real or mock).
/// - `event_tx`: sender half of the App's event channel.
/// - `watch_set`: shared, mutable list of (path, pr_number) pairs to poll;
///   the App updates this after every `refresh_worktrees()`.
/// - `config`: polling interval and other tuning.
/// - `shutdown_rx`: watch channel; when the value becomes `true` the watcher
///   exits the loop and the `JoinHandle` resolves.
///
/// # gh-unavailable
/// The caller is responsible for checking `gh_available()` before spawning.
/// If gh is absent, simply do not call this function.
///
/// # Returns
/// A [`JoinHandle`] that resolves once the watcher exits.  Best-effort abort or
/// await it on app shutdown.
pub fn spawn_watcher(
    runner: Arc<dyn GhRunner>,
    event_tx: tokio::sync::mpsc::Sender<AppEvent>,
    watch_set: Arc<Mutex<Vec<WatchItem>>>,
    config: WatcherConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Per-path last-observed (PR status, unresolved-count); initialised
        // lazily on first tick.
        let mut known: HashMap<PathBuf, (PrStatus, Option<u64>)> = HashMap::new();

        let mut interval = time::interval(config.interval);
        // Don't fire the first tick immediately — wait one full interval so the
        // daemon has time to populate the watch-set after a registry rebuild.
        interval.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Snapshot the watch-set so we don't hold the lock across
                    // async gh calls.
                    let items: Vec<WatchItem> = {
                        let guard = watch_set.lock().await;
                        guard.clone()
                    };

                    for item in &items {
                        // One gh call per worktree, by branch (no PR number).
                        let info = match fetch_pr_status(runner.as_ref(), &item.worktree_path).await
                        {
                            Ok(info) => info,
                            Err(e) => {
                                tracing::warn!(
                                    worktree = %item.worktree_path.display(),
                                    "watcher: gh fetch_pr_status error (skipping): {e}"
                                );
                                continue;
                            }
                        };

                        let (curr, pr_number, pr_url, pr_title) = match info {
                            Some(pr) => (pr.status, Some(pr.number), pr.url, pr.title),
                            None => (PrStatus::NoPr, None, None, None),
                        };

                        // Carry forward the previously-known unresolved count so a
                        // transient GraphQL failure doesn't flap the badge.
                        let prev = known.get(&item.worktree_path).copied();
                        let prev_unresolved = prev.and_then(|(_, u)| u);

                        // SECOND gh call (GraphQL) ONLY for OPEN-ish PRs (Some PR,
                        // not Merged/Closed) that have parseable owner/repo.
                        let is_open_ish = pr_number.is_some()
                            && !matches!(curr, PrStatus::Merged | PrStatus::Closed);
                        let unresolved = match (
                            is_open_ish,
                            item.owner.as_deref(),
                            item.repo.as_deref(),
                            pr_number,
                        ) {
                            (true, Some(owner), Some(repo), Some(number)) => {
                                match fetch_unresolved_count(
                                    runner.as_ref(),
                                    &item.worktree_path,
                                    owner,
                                    repo,
                                    number,
                                )
                                .await
                                {
                                    Ok(n) => Some(n),
                                    Err(e) => {
                                        tracing::warn!(
                                            worktree = %item.worktree_path.display(),
                                            "watcher: fetch_unresolved_count error (carrying forward): {e}"
                                        );
                                        prev_unresolved
                                    }
                                }
                            }
                            // NoPr / Merged / Closed / missing owner-repo → no count.
                            _ => None,
                        };

                        let curr_pair = (curr, unresolved);
                        if diff_pr(prev, curr_pair) {
                            // Persist to the owning project's pr_status.toml so a
                            // separate session daemon can read PR state without
                            // calling `gh`.  Best-effort: a write failure is logged
                            // and never stops the poll.
                            persist_pr_status(
                                &item.project_root,
                                PrStatusEntry {
                                    path: item.worktree_path.clone(),
                                    pr_status: curr,
                                    pr_number,
                                    pr_url: pr_url.clone(),
                                    pr_title: pr_title.clone(),
                                    unresolved_comments: unresolved,
                                    updated_at: chrono::Utc::now(),
                                },
                            );

                            emit_event(
                                &event_tx,
                                WatchEvent::PrStatusChanged {
                                    worktree_path: item.worktree_path.clone(),
                                    pr_status: curr,
                                    pr_number,
                                    pr_url,
                                    pr_title,
                                    unresolved_comments: unresolved,
                                },
                            )
                            .await;
                        }
                        known.insert(item.worktree_path.clone(), curr_pair);
                    }
                }

                // Shutdown signal: exit cleanly when the value becomes true.
                result = shutdown_rx.changed() => {
                    if result.is_err() || *shutdown_rx.borrow() {
                        tracing::info!("watcher: shutdown signal received, exiting");
                        break;
                    }
                }
            }
        }
    })
}

/// Send a [`WatchEvent`] into the app event channel, converting it to the
/// appropriate [`AppEvent`].  Logs and continues on send failure (receiver
/// dropped means the app is shutting down — that's fine).
async fn emit_event(tx: &tokio::sync::mpsc::Sender<AppEvent>, event: WatchEvent) {
    let app_event = match event {
        WatchEvent::PrStatusChanged {
            worktree_path,
            pr_status,
            pr_number,
            pr_url,
            pr_title,
            unresolved_comments,
        } => AppEvent::PrStatusChanged {
            worktree_path,
            pr_status,
            pr_number,
            pr_url,
            pr_title,
            unresolved_comments,
        },
    };
    if tx.send(app_event).await.is_err() {
        tracing::debug!("watcher: event_tx closed, app shutting down");
    }
}

/// Persist a single worktree's PR status into its project's `pr_status.toml`.
///
/// Loads the current file (missing/malformed → empty), upserts this worktree's
/// entry, and atomically writes it back.  Best-effort: any error is logged and
/// swallowed so a failed write never crashes the watcher or stops the poll.
fn persist_pr_status(project_root: &std::path::Path, entry: PrStatusEntry) {
    let mut file = pr_status_store::load(project_root);
    file.upsert(entry);
    if let Err(e) = pr_status_store::save(project_root, &file) {
        tracing::warn!(
            project = %project_root.display(),
            "watcher: failed to persist pr_status.toml: {e}"
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // diff_pr: change-detection pure logic
    // -----------------------------------------------------------------------

    #[test]
    fn diff_first_observation_nopr_emits() {
        // First observation always emits: the registry default is now Loading (not
        // NoPr), so even NoPr must be sent to replace the Loading placeholder.
        assert!(diff_pr(None, (PrStatus::NoPr, None)));
    }

    #[test]
    fn diff_first_observation_open_emits() {
        assert!(diff_pr(None, (PrStatus::Open, None)));
    }

    #[test]
    fn diff_first_observation_loading_emits() {
        // Loading differs from every real status; prev=None always emits.
        assert!(diff_pr(None, (PrStatus::Loading, None)));
    }

    #[test]
    fn diff_loading_to_nopr_emits() {
        // The daemon starts worktrees at Loading; first poll returning NoPr must emit.
        assert!(diff_pr(
            Some((PrStatus::Loading, None)),
            (PrStatus::NoPr, None)
        ));
    }

    #[test]
    fn diff_loading_to_open_emits() {
        assert!(diff_pr(
            Some((PrStatus::Loading, None)),
            (PrStatus::Open, None)
        ));
    }

    #[test]
    fn diff_loading_to_merged_emits() {
        assert!(diff_pr(
            Some((PrStatus::Loading, None)),
            (PrStatus::Merged, None)
        ));
    }

    #[test]
    fn diff_nopr_to_open_emits() {
        assert!(diff_pr(
            Some((PrStatus::NoPr, None)),
            (PrStatus::Open, None)
        ));
    }

    #[test]
    fn diff_open_to_open_is_noop() {
        assert!(!diff_pr(
            Some((PrStatus::Open, None)),
            (PrStatus::Open, None)
        ));
    }

    #[test]
    fn diff_open_to_merged_emits() {
        assert!(diff_pr(
            Some((PrStatus::Open, None)),
            (PrStatus::Merged, None)
        ));
    }

    #[test]
    fn diff_merged_to_merged_is_noop() {
        // Once merged, staying merged must not re-emit (gate auto-continue edge).
        assert!(!diff_pr(
            Some((PrStatus::Merged, None)),
            (PrStatus::Merged, None)
        ));
    }

    #[test]
    fn diff_unresolved_count_change_emits_even_when_status_unchanged() {
        // Same PR status (Open) but unresolved count changed → must emit.
        assert!(diff_pr(
            Some((PrStatus::Open, Some(0))),
            (PrStatus::Open, Some(2))
        ));
        // Identical status + identical count → no-op.
        assert!(!diff_pr(
            Some((PrStatus::Open, Some(2))),
            (PrStatus::Open, Some(2))
        ));
        // None → Some(n) at same status emits (first comment appears).
        assert!(diff_pr(
            Some((PrStatus::Open, None)),
            (PrStatus::Open, Some(1))
        ));
    }

    // -----------------------------------------------------------------------
    // should_auto_continue
    // -----------------------------------------------------------------------

    #[test]
    fn auto_continue_only_when_flag_set() {
        assert!(should_auto_continue(true));
        assert!(!should_auto_continue(false));
    }

    // -----------------------------------------------------------------------
    // Watcher tick integration test (fast interval, MockGh, no real network)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn watcher_tick_emits_pr_status_changed_via_mock() {
        use crate::github::mock::MockGh;
        use tokio::sync::mpsc;

        // MockGh returns a merged PR for the branch.
        let pr_json = r#"{"number":99,"state":"MERGED","isDraft":false,"mergedAt":"2024-01-15T10:00:00Z","statusCheckRollup":[]}"#;
        let mock = Arc::new(MockGh::new(vec![(
            "pr view --json",
            Ok(pr_json.to_string()),
        )]));

        let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(16);
        let watch_set = Arc::new(Mutex::new(vec![WatchItem {
            worktree_path: PathBuf::from("/tmp/wt-test"),
            project_root: PathBuf::from("/tmp/wt-test"),
            owner: None,
            repo: None,
        }]));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let handle = spawn_watcher(
            mock,
            event_tx,
            watch_set,
            WatcherConfig {
                interval: Duration::from_millis(10),
            },
            shutdown_rx,
        );

        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout waiting for watcher event")
            .expect("channel closed");

        match event {
            AppEvent::PrStatusChanged {
                pr_status,
                pr_number,
                pr_url,
                ..
            } => {
                assert_eq!(pr_status, PrStatus::Merged);
                assert_eq!(pr_number, Some(99));
                assert_eq!(pr_url, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    /// Verify that a worktree starting at Loading transitions to NoPr on the
    /// first poll (MockGh returns "no pull requests found" → NoPr).
    #[tokio::test]
    async fn watcher_loading_to_nopr_on_first_poll() {
        use crate::github::mock::MockGh;
        use tokio::sync::mpsc;

        // MockGh returns "no PR" error — simulates a worktree with no open PR.
        let mock = Arc::new(MockGh::new(vec![(
            "pr view --json",
            Err(anyhow::anyhow!(
                "no pull requests found for branch \"feat\""
            )),
        )]));

        let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(16);
        let watch_set = Arc::new(Mutex::new(vec![WatchItem {
            worktree_path: PathBuf::from("/tmp/wt-loading-nopr"),
            project_root: PathBuf::from("/tmp/wt-loading-nopr"),
            owner: None,
            repo: None,
        }]));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let handle = spawn_watcher(
            mock,
            event_tx,
            watch_set,
            WatcherConfig {
                interval: Duration::from_millis(10),
            },
            shutdown_rx,
        );

        // First tick must emit PrStatusChanged{NoPr} to replace the Loading state.
        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout: watcher did not emit Loading→NoPr transition")
            .expect("channel closed");

        match event {
            AppEvent::PrStatusChanged {
                pr_status,
                pr_number,
                ..
            } => {
                assert_eq!(pr_status, PrStatus::NoPr, "Loading→NoPr must emit NoPr");
                assert_eq!(pr_number, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    /// Verify that a worktree starting at Loading transitions to Merged on the
    /// first poll (MockGh returns a merged PR).
    #[tokio::test]
    async fn watcher_loading_to_merged_on_first_poll() {
        use crate::github::mock::MockGh;
        use tokio::sync::mpsc;

        let pr_json = r#"{"number":7,"state":"MERGED","isDraft":false,"mergedAt":"2024-01-15T10:00:00Z","statusCheckRollup":[]}"#;
        let mock = Arc::new(MockGh::new(vec![(
            "pr view --json",
            Ok(pr_json.to_string()),
        )]));

        let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(16);
        let watch_set = Arc::new(Mutex::new(vec![WatchItem {
            worktree_path: PathBuf::from("/tmp/wt-loading-merged"),
            project_root: PathBuf::from("/tmp/wt-loading-merged"),
            owner: None,
            repo: None,
        }]));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let handle = spawn_watcher(
            mock,
            event_tx,
            watch_set,
            WatcherConfig {
                interval: Duration::from_millis(10),
            },
            shutdown_rx,
        );

        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout: watcher did not emit Loading→Merged transition")
            .expect("channel closed");

        match event {
            AppEvent::PrStatusChanged {
                pr_status,
                pr_number,
                ..
            } => {
                assert_eq!(
                    pr_status,
                    PrStatus::Merged,
                    "Loading→Merged must emit Merged"
                );
                assert_eq!(pr_number, Some(7));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    /// An OPEN PR with owner/repo set triggers the SECOND (GraphQL) call and the
    /// emitted event carries the unresolved count.
    #[tokio::test]
    async fn watcher_open_pr_triggers_unresolved_count() {
        use crate::github::mock::MockGh;
        use tokio::sync::mpsc;

        let pr_json = r#"{"number":42,"state":"OPEN","isDraft":false,"mergedAt":null,"statusCheckRollup":[]}"#;
        let graphql_json = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"totalCount":3,"nodes":[{"isResolved":false},{"isResolved":false},{"isResolved":true}]}}}}}"#;
        let mock = Arc::new(MockGh::new(vec![
            ("api graphql", Ok(graphql_json.to_string())),
            ("pr view --json", Ok(pr_json.to_string())),
        ]));

        let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(16);
        let watch_set = Arc::new(Mutex::new(vec![WatchItem {
            worktree_path: PathBuf::from("/tmp/wt-open-unresolved"),
            project_root: PathBuf::from("/tmp/wt-open-unresolved"),
            owner: Some("Owner".to_string()),
            repo: Some("Repo".to_string()),
        }]));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let handle = spawn_watcher(
            mock,
            event_tx,
            watch_set,
            WatcherConfig {
                interval: Duration::from_millis(10),
            },
            shutdown_rx,
        );

        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout waiting for watcher event")
            .expect("channel closed");

        match event {
            AppEvent::PrStatusChanged {
                pr_status,
                unresolved_comments,
                ..
            } => {
                assert_eq!(pr_status, PrStatus::Open);
                assert_eq!(unresolved_comments, Some(2));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    /// A MERGED PR (even with owner/repo set) does NOT make the GraphQL call:
    /// the emitted event's unresolved count is `None`.  If the watcher had
    /// erroneously called graphql, the MockGh (which DOES have a graphql entry
    /// returning a non-zero count) would surface Some(n) instead of None.
    #[tokio::test]
    async fn watcher_merged_pr_skips_unresolved_count() {
        use crate::github::mock::MockGh;
        use tokio::sync::mpsc;

        let pr_json = r#"{"number":7,"state":"MERGED","isDraft":false,"mergedAt":"2024-01-15T10:00:00Z","statusCheckRollup":[]}"#;
        let graphql_json = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"totalCount":1,"nodes":[{"isResolved":false}]}}}}}"#;
        let mock = Arc::new(MockGh::new(vec![
            ("api graphql", Ok(graphql_json.to_string())),
            ("pr view --json", Ok(pr_json.to_string())),
        ]));

        let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(16);
        let watch_set = Arc::new(Mutex::new(vec![WatchItem {
            worktree_path: PathBuf::from("/tmp/wt-merged-skip"),
            project_root: PathBuf::from("/tmp/wt-merged-skip"),
            owner: Some("Owner".to_string()),
            repo: Some("Repo".to_string()),
        }]));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let handle = spawn_watcher(
            mock,
            event_tx,
            watch_set,
            WatcherConfig {
                interval: Duration::from_millis(10),
            },
            shutdown_rx,
        );

        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout waiting for watcher event")
            .expect("channel closed");

        match event {
            AppEvent::PrStatusChanged {
                pr_status,
                unresolved_comments,
                ..
            } => {
                assert_eq!(pr_status, PrStatus::Merged);
                assert_eq!(
                    unresolved_comments, None,
                    "merged PR must not fetch/report an unresolved count"
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    /// The watcher persists each observed PR status to the owning project's
    /// `pr_status.toml` (the hand-off file the future standalone daemon reads).
    #[tokio::test]
    async fn watcher_persists_pr_status_to_toml() {
        use crate::github::mock::MockGh;
        use crate::pr_status_store;
        use tokio::sync::mpsc;

        // Project root is a real tempdir so the watcher's write lands somewhere
        // inspectable and is cleaned up with the test.
        let project = tempfile::tempdir().expect("tempdir");
        let wt_path = project.path().join("wt-persist");

        let pr_json = r#"{"number":123,"state":"OPEN","isDraft":false,"mergedAt":null,"statusCheckRollup":[]}"#;
        let graphql_json = r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"totalCount":2,"nodes":[{"isResolved":false},{"isResolved":true}]}}}}}"#;
        let mock = Arc::new(MockGh::new(vec![
            ("api graphql", Ok(graphql_json.to_string())),
            ("pr view --json", Ok(pr_json.to_string())),
        ]));

        let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(16);
        let watch_set = Arc::new(Mutex::new(vec![WatchItem {
            worktree_path: wt_path.clone(),
            project_root: project.path().to_path_buf(),
            owner: Some("Owner".to_string()),
            repo: Some("Repo".to_string()),
        }]));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let handle = spawn_watcher(
            mock,
            event_tx,
            watch_set,
            WatcherConfig {
                interval: Duration::from_millis(10),
            },
            shutdown_rx,
        );

        // Wait for the emit so we know a full tick (incl. the persist) ran.
        let _ = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout waiting for watcher event")
            .expect("channel closed");

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;

        // The file must now hold this worktree's entry with the observed values.
        let file = pr_status_store::load(project.path());
        let entry = file
            .get(&wt_path)
            .expect("pr_status.toml must contain the worktree entry");
        assert_eq!(entry.pr_status, PrStatus::Open);
        assert_eq!(entry.pr_number, Some(123));
        assert_eq!(entry.unresolved_comments, Some(1));
    }
}

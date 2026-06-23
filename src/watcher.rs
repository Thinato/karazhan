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
use crate::github::pr_status::fetch_pr_status;
use crate::github::GhRunner;
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

/// A single entry in the shared watch-set: a worktree path.
///
/// PR discovery is by branch now (gh resolves the PR from the worktree's
/// current branch), so no PR number is stored here.
#[derive(Debug, Clone)]
pub struct WatchItem {
    pub worktree_path: PathBuf,
}

// ---------------------------------------------------------------------------
// Change-detection (pure, easily unit-tested)
// ---------------------------------------------------------------------------

/// An event produced when a worktree's observed [`PrStatus`] changes.
#[derive(Debug, PartialEq, Eq)]
pub enum WatchEvent {
    PrStatusChanged {
        worktree_path: PathBuf,
        pr_status: PrStatus,
        pr_number: Option<u64>,
    },
}

/// Pure change-detection: should we emit an event given the previous observed
/// status (`None` on the first tick) and the current one?
///
/// We emit on any real change.  The first observation emits only when it is not
/// `NoPr` (a fresh worktree with no PR starts at the registry default `NoPr`, so
/// re-announcing `NoPr` is noise).  Extracted so tests can cover transitions
/// without any I/O.
pub fn diff_pr(prev: Option<PrStatus>, curr: PrStatus) -> bool {
    match prev {
        None => curr != PrStatus::NoPr,
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
        // Per-path last-observed PR status; initialised lazily on first tick.
        let mut known: HashMap<PathBuf, PrStatus> = HashMap::new();

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

                        let (curr, pr_number) = match info {
                            Some(pr) => (pr.status, Some(pr.number)),
                            None => (PrStatus::NoPr, None),
                        };

                        let prev = known.get(&item.worktree_path).copied();
                        if diff_pr(prev, curr) {
                            emit_event(
                                &event_tx,
                                WatchEvent::PrStatusChanged {
                                    worktree_path: item.worktree_path.clone(),
                                    pr_status: curr,
                                    pr_number,
                                },
                            )
                            .await;
                        }
                        known.insert(item.worktree_path.clone(), curr);
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
        } => AppEvent::PrStatusChanged {
            worktree_path,
            pr_status,
            pr_number,
        },
    };
    if tx.send(app_event).await.is_err() {
        tracing::debug!("watcher: event_tx closed, app shutting down");
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
    fn diff_first_observation_nopr_is_noop() {
        // Fresh worktree, still NoPr → no event (it already defaults to NoPr).
        assert!(!diff_pr(None, PrStatus::NoPr));
    }

    #[test]
    fn diff_first_observation_open_emits() {
        assert!(diff_pr(None, PrStatus::Open));
    }

    #[test]
    fn diff_nopr_to_open_emits() {
        assert!(diff_pr(Some(PrStatus::NoPr), PrStatus::Open));
    }

    #[test]
    fn diff_open_to_open_is_noop() {
        assert!(!diff_pr(Some(PrStatus::Open), PrStatus::Open));
    }

    #[test]
    fn diff_open_to_merged_emits() {
        assert!(diff_pr(Some(PrStatus::Open), PrStatus::Merged));
    }

    #[test]
    fn diff_merged_to_merged_is_noop() {
        // Once merged, staying merged must not re-emit (gate auto-continue edge).
        assert!(!diff_pr(Some(PrStatus::Merged), PrStatus::Merged));
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
                ..
            } => {
                assert_eq!(pr_status, PrStatus::Merged);
                assert_eq!(pr_number, Some(99));
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }
}

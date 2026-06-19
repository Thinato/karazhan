//! Background poller: monitors PR state and CI status for active worktrees.
//!
//! The watcher runs as a detached tokio task that ticks every [`WatcherConfig::interval`],
//! queries `gh` for each worktree that has a known PR number, and emits [`AppEvent`]s
//! when state changes (PR merged, CI passing/failing).
//!
//! # Watch-set sharing
//! The App owns an `Arc<Mutex<Vec<WatchItem>>>` that it updates after every
//! `refresh_worktrees()` call.  The watcher reads this shared list on each tick so
//! it always polls the current set of active worktrees without requiring a separate
//! channel handshake.
//!
//! # Error handling
//! Errors for individual worktrees are logged as warnings and skipped — one bad worktree
//! never stops the poll of others, and the watcher never crashes.
//!
//! # Shutdown
//! The caller passes a `tokio::sync::watch::Receiver<bool>` (shutdown signal).  The
//! watcher `select!`s between the interval tick and a change on the receiver; when the
//! receiver sees `true` the loop exits cleanly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time;

use crate::app::AppEvent;
use crate::github::ci::ci_status;
use crate::github::pr::pr_state;
use crate::github::GhRunner;

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

/// A single entry in the shared watch-set: a worktree path + its PR number.
#[derive(Debug, Clone)]
pub struct WatchItem {
    pub worktree_path: PathBuf,
    pub pr_number: u64,
}

// ---------------------------------------------------------------------------
// Change-detection (pure, easily unit-tested)
// ---------------------------------------------------------------------------

/// Last-known state for a single (worktree, PR) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchState {
    /// Whether the PR was merged on the previous tick.
    pub merged: bool,
    /// Whether CI was fully passing on the previous tick.
    pub ci_all_passing: bool,
}

/// An event produced by the diff between previous and current state.
#[derive(Debug, PartialEq, Eq)]
pub enum WatchEvent {
    PrMerged {
        worktree_path: PathBuf,
        pr: u64,
    },
    CiStatusChanged {
        worktree_path: PathBuf,
        all_passing: bool,
    },
}

/// Pure function: compare previous state with freshly-fetched values and
/// produce the minimal set of [`WatchEvent`]s needed to update the UI.
///
/// This is extracted so unit tests can exercise it without any I/O.
pub fn diff_state(
    worktree_path: &Path,
    pr: u64,
    prev: &WatchState,
    merged: bool,
    ci_all_passing: bool,
) -> Vec<WatchEvent> {
    let mut events = Vec::new();

    // PR transition: open → merged (only fire once; stay quiet if already merged).
    if merged && !prev.merged {
        events.push(WatchEvent::PrMerged {
            worktree_path: worktree_path.to_path_buf(),
            pr,
        });
    }

    // CI status changed in either direction.
    if ci_all_passing != prev.ci_all_passing {
        events.push(WatchEvent::CiStatusChanged {
            worktree_path: worktree_path.to_path_buf(),
            all_passing: ci_all_passing,
        });
    }

    events
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
/// - `cwd`: repository root passed to each `gh` call.
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
    cwd: PathBuf,
    event_tx: tokio::sync::mpsc::Sender<AppEvent>,
    watch_set: Arc<Mutex<Vec<WatchItem>>>,
    config: WatcherConfig,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Per-(path, pr) last-known state; initialised lazily on first tick.
        let mut known: HashMap<(PathBuf, u64), WatchState> = HashMap::new();

        let mut interval = time::interval(config.interval);
        // Don't fire the first tick immediately — wait one full interval so the
        // App has time to populate the watch-set after refresh_worktrees().
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
                        let key = (item.worktree_path.clone(), item.pr_number);

                        // Fetch fresh PR + CI state.
                        let pr_result = pr_state(runner.as_ref(), &cwd, item.pr_number).await;
                        let ci_result = ci_status(runner.as_ref(), &cwd, item.pr_number).await;

                        let (merged, ci_all_passing) = match (pr_result, ci_result) {
                            (Ok(pr), Ok(ci)) => (pr.merged, ci.all_passing),
                            (Err(e), _) => {
                                tracing::warn!(
                                    worktree = %item.worktree_path.display(),
                                    pr = item.pr_number,
                                    "watcher: gh pr_state error (skipping): {e}"
                                );
                                continue;
                            }
                            (Ok(pr), Err(e)) => {
                                tracing::warn!(
                                    worktree = %item.worktree_path.display(),
                                    pr = item.pr_number,
                                    "watcher: gh ci_status error (using pr only): {e}"
                                );
                                // Still process PR state change even without CI.
                                let prev = known.entry(key.clone()).or_insert(WatchState {
                                    merged: false,
                                    ci_all_passing: false,
                                });
                                let events = diff_state(
                                    &item.worktree_path,
                                    item.pr_number,
                                    prev,
                                    pr.merged,
                                    prev.ci_all_passing, // no change
                                );
                                prev.merged = pr.merged;
                                emit_events(&event_tx, events).await;
                                continue;
                            }
                        };

                        let prev = known.entry(key).or_insert(WatchState {
                            merged: false,
                            ci_all_passing: false,
                        });

                        let events = diff_state(
                            &item.worktree_path,
                            item.pr_number,
                            prev,
                            merged,
                            ci_all_passing,
                        );

                        // Update last-known state.
                        prev.merged = merged;
                        prev.ci_all_passing = ci_all_passing;

                        emit_events(&event_tx, events).await;
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

/// Send a slice of [`WatchEvent`]s into the app event channel, converting each
/// to the appropriate [`AppEvent`].  Logs and continues on send failure (receiver
/// dropped means the app is shutting down — that's fine).
async fn emit_events(tx: &tokio::sync::mpsc::Sender<AppEvent>, events: Vec<WatchEvent>) {
    for ev in events {
        let app_event = match ev {
            WatchEvent::PrMerged { worktree_path, pr } => AppEvent::PrMerged { worktree_path, pr },
            WatchEvent::CiStatusChanged {
                worktree_path,
                all_passing,
            } => AppEvent::CiStatusChanged {
                worktree_path,
                all_passing,
            },
        };
        if tx.send(app_event).await.is_err() {
            tracing::debug!("watcher: event_tx closed, app shutting down");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn path(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    // -----------------------------------------------------------------------
    // diff_state: change-detection pure logic
    // -----------------------------------------------------------------------

    #[test]
    fn diff_open_to_merged_yields_pr_merged() {
        let prev = WatchState {
            merged: false,
            ci_all_passing: false,
        };
        let events = diff_state(&path("/wt/a"), 42, &prev, true, false);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], WatchEvent::PrMerged { pr: 42, .. }));
    }

    #[test]
    fn diff_already_merged_no_duplicate_event() {
        // PR was already merged on the previous tick — no new event.
        let prev = WatchState {
            merged: true,
            ci_all_passing: false,
        };
        let events = diff_state(&path("/wt/a"), 42, &prev, true, false);
        assert!(events.is_empty());
    }

    #[test]
    fn diff_ci_passing_to_failing_yields_ci_status_changed_false() {
        let prev = WatchState {
            merged: false,
            ci_all_passing: true,
        };
        let events = diff_state(&path("/wt/b"), 7, &prev, false, false);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            WatchEvent::CiStatusChanged {
                all_passing: false,
                ..
            }
        ));
    }

    #[test]
    fn diff_ci_failing_to_passing_yields_ci_status_changed_true() {
        let prev = WatchState {
            merged: false,
            ci_all_passing: false,
        };
        let events = diff_state(&path("/wt/c"), 3, &prev, false, true);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            WatchEvent::CiStatusChanged {
                all_passing: true,
                ..
            }
        ));
    }

    #[test]
    fn diff_no_change_yields_nothing() {
        let prev = WatchState {
            merged: false,
            ci_all_passing: true,
        };
        let events = diff_state(&path("/wt/d"), 1, &prev, false, true);
        assert!(events.is_empty());
    }

    #[test]
    fn diff_both_pr_and_ci_change_yields_two_events() {
        let prev = WatchState {
            merged: false,
            ci_all_passing: true,
        };
        // PR merged + CI now failing (edge-case: merge can flip CI to failing state)
        let events = diff_state(&path("/wt/e"), 5, &prev, true, false);
        assert_eq!(events.len(), 2);
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
    async fn watcher_tick_emits_pr_merged_via_mock() {
        use crate::github::mock::MockGh;
        use tokio::sync::mpsc;

        // MockGh returns a merged PR and passing CI.
        let pr_json = r#"{"state":"MERGED","mergeStateStatus":null,"mergedAt":"2024-01-15T10:00:00Z","title":"feat"}"#;
        let ci_json = r#"[{"name":"build","status":"completed","conclusion":"success"}]"#;
        let mock = Arc::new(MockGh::new(vec![
            ("pr view", Ok(pr_json.to_string())),
            ("pr checks", Ok(ci_json.to_string())),
        ]));

        let (event_tx, mut event_rx) = mpsc::channel::<AppEvent>(16);
        let watch_set = Arc::new(Mutex::new(vec![WatchItem {
            worktree_path: PathBuf::from("/tmp/wt-test"),
            pr_number: 99,
        }]));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let handle = spawn_watcher(
            mock,
            PathBuf::from("/tmp"),
            event_tx,
            watch_set,
            WatcherConfig {
                interval: Duration::from_millis(10),
            },
            shutdown_rx,
        );

        // Wait for an event with a generous timeout.
        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout waiting for watcher event")
            .expect("channel closed");

        // Expect either PrMerged or CiStatusChanged (both are valid on first tick).
        match event {
            AppEvent::PrMerged { pr, .. } => assert_eq!(pr, 99),
            AppEvent::CiStatusChanged { all_passing, .. } => assert!(all_passing),
            other => panic!("unexpected event: {other:?}"),
        }

        // Shut down.
        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }
}

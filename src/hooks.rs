//! State-predicate hooks: run a shell command when a worktree's state
//! transitions INTO a configured condition.
//!
//! A worktree carries TWO independent status axes — the agent-activity
//! [`WorktreeStatus`] (written by `Shared::set_status`) and the
//! [`PrStatus`] (written by `Shared::set_pr_status` from the PR poller).  They
//! are updated by different code paths, usually minutes apart, so "the agent
//! finished AND checks are green" can never be a single event.  A hook is
//! therefore a PREDICATE over the current pair, evaluated on every transition:
//!
//! ```text
//! fire when  matches(after) && !matches(before)
//! ```
//!
//! Edge-triggering is mandatory, not a nicety: the PR poller re-reads status on
//! every `poll_interval_secs` tick, so a level-triggered hook would re-run its
//! command forever while the state held.  This mirrors the `merged_edge` check
//! that already guards auto-continue-on-merge.
//!
//! Config surface (`[[hooks]]`, accepted in BOTH the global config and a
//! project's `.karazhan/config.toml`; the two lists are concatenated):
//!
//! ```toml
//! [[hooks]]
//! name            = "ship-it"
//! status          = "needs_review"
//! pr_status       = "checks_passing"
//! run             = "~/scripts/x.sh"
//! timeout_seconds = 120
//! ```
//!
//! Every SPECIFIED field must match (AND); an omitted field is a wildcard.
//! Several `[[hooks]]` entries give OR.

use std::time::Duration;

use serde::de::IntoDeserializer;
use serde::{Deserialize, Serialize};

use crate::worktree::model::PrStatus;
use crate::worktree::WorktreeStatus;

/// Built-in default timeout (seconds) for a hook command.
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// HookRule — the raw serde surface
// ---------------------------------------------------------------------------

/// One `[[hooks]]` entry exactly as written in config TOML.
///
/// Conditions are kept as `String` rather than the enums so that a typo'd
/// status name costs only that ONE hook (warn + skip in [`compile`]) instead of
/// failing the whole config file back to defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HookRule {
    /// Optional label, used in logs and exported as `KARAZHAN_HOOK`.
    pub name: Option<String>,
    /// Required [`WorktreeStatus`] (snake_case), or absent for "any".
    pub status: Option<String>,
    /// Required [`PrStatus`] (snake_case), or absent for "any".
    pub pr_status: Option<String>,
    /// Shell command, run via `sh -c` with the worktree as cwd.
    pub run: String,
    /// Max runtime before the command is killed.  Absent → 300s.
    pub timeout_seconds: Option<u64>,
}

// ---------------------------------------------------------------------------
// Hook — the validated form
// ---------------------------------------------------------------------------

/// A hook that passed validation and is ready to be matched against state.
#[derive(Debug, Clone, PartialEq)]
pub struct Hook {
    pub name: String,
    pub status: Option<WorktreeStatus>,
    pub pr_status: Option<PrStatus>,
    pub run: String,
    pub timeout: Duration,
}

impl Hook {
    /// Does this hook's predicate hold for the given state pair?
    fn matches(&self, status: &WorktreeStatus, pr_status: PrStatus) -> bool {
        self.status.as_ref().is_none_or(|s| s == status)
            && self.pr_status.is_none_or(|p| p == pr_status)
    }

    /// Should this hook run for the transition `before` → `after`?
    ///
    /// True only on the `false → true` EDGE of the predicate, so a state that
    /// merely persists across poll ticks never re-fires.  A `before` of `None`
    /// (worktree not yet in the registry) counts as "did not match".
    pub fn fires(
        &self,
        before: Option<&(WorktreeStatus, PrStatus)>,
        after: &(WorktreeStatus, PrStatus),
    ) -> bool {
        self.matches(&after.0, after.1) && !before.is_some_and(|b| self.matches(&b.0, b.1))
    }
}

// ---------------------------------------------------------------------------
// compile — validate raw rules, warning + skipping the bad ones
// ---------------------------------------------------------------------------

/// Deserialize a unit-variant enum from its snake_case config spelling.
fn parse_enum<'a, T: Deserialize<'a>>(s: &'a str) -> Option<T> {
    let de: serde::de::value::StrDeserializer<'a, serde::de::value::Error> = s.into_deserializer();
    T::deserialize(de).ok()
}

/// The snake_case config/state spelling of a unit-variant enum value.
pub fn name_of<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Validate raw `[[hooks]]` entries, dropping (with a warning) any that could
/// never behave sensibly.  `source` names the config file for the log line.
///
/// Rejected: an empty `run`, an unparseable `status` / `pr_status`, and a hook
/// with NO condition at all (which would fire on every single transition).
pub fn compile(rules: &[HookRule], source: &str) -> Vec<Hook> {
    let mut out = Vec::new();
    for (i, rule) in rules.iter().enumerate() {
        let name = rule.name.clone().unwrap_or_else(|| format!("hooks[{i}]"));

        if rule.run.trim().is_empty() {
            tracing::warn!("hooks: {source}: `{name}` has an empty `run` — skipped");
            continue;
        }

        let status = match &rule.status {
            None => None,
            Some(s) => match parse_enum::<WorktreeStatus>(s) {
                Some(v) => Some(v),
                None => {
                    tracing::warn!("hooks: {source}: `{name}` has unknown status `{s}` — skipped");
                    continue;
                }
            },
        };

        let pr_status = match &rule.pr_status {
            None => None,
            Some(s) => match parse_enum::<PrStatus>(s) {
                Some(v) => Some(v),
                None => {
                    tracing::warn!(
                        "hooks: {source}: `{name}` has unknown pr_status `{s}` — skipped"
                    );
                    continue;
                }
            },
        };

        if status.is_none() && pr_status.is_none() {
            tracing::warn!(
                "hooks: {source}: `{name}` has no condition (needs `status` and/or \
                 `pr_status`) — skipped"
            );
            continue;
        }

        out.push(Hook {
            name,
            status,
            pr_status,
            run: rule.run.clone(),
            timeout: Duration::from_secs(rule.timeout_seconds.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS)),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(status: Option<&str>, pr_status: Option<&str>) -> HookRule {
        HookRule {
            name: Some("t".to_string()),
            status: status.map(str::to_string),
            pr_status: pr_status.map(str::to_string),
            run: "true".to_string(),
            timeout_seconds: None,
        }
    }

    fn one(status: Option<&str>, pr_status: Option<&str>) -> Option<Hook> {
        compile(&[rule(status, pr_status)], "test")
            .into_iter()
            .next()
    }

    #[test]
    fn compiles_both_conditions() {
        let h = one(Some("needs_review"), Some("checks_passing")).expect("valid hook");
        assert_eq!(h.status, Some(WorktreeStatus::NeedsReview));
        assert_eq!(h.pr_status, Some(PrStatus::ChecksPassing));
        assert_eq!(h.timeout, Duration::from_secs(DEFAULT_HOOK_TIMEOUT_SECS));
    }

    #[test]
    fn fires_once_on_the_rising_edge() {
        let h = one(Some("needs_review"), Some("checks_passing")).unwrap();
        let before = (WorktreeStatus::NeedsReview, PrStatus::ChecksRunning);
        let after = (WorktreeStatus::NeedsReview, PrStatus::ChecksPassing);
        assert!(h.fires(Some(&before), &after), "must fire on the edge");
        // Same state re-observed on the next poll tick: must NOT fire again.
        assert!(
            !h.fires(Some(&after), &after),
            "level-triggering would re-run the command every poll tick"
        );
    }

    #[test]
    fn does_not_fire_when_only_one_condition_holds() {
        let h = one(Some("needs_review"), Some("checks_passing")).unwrap();
        let before = (WorktreeStatus::Running, PrStatus::ChecksRunning);
        let after = (WorktreeStatus::Running, PrStatus::ChecksPassing);
        assert!(!h.fires(Some(&before), &after));
    }

    #[test]
    fn absent_condition_is_a_wildcard() {
        let h = one(Some("error"), None).unwrap();
        assert!(h.fires(
            Some(&(WorktreeStatus::Running, PrStatus::NoPr)),
            &(WorktreeStatus::Error, PrStatus::NoPr)
        ));
        assert!(h.fires(
            Some(&(WorktreeStatus::Running, PrStatus::Open)),
            &(WorktreeStatus::Error, PrStatus::Open)
        ));
    }

    #[test]
    fn unknown_worktree_fires_once() {
        let h = one(Some("needs_review"), None).unwrap();
        assert!(h.fires(None, &(WorktreeStatus::NeedsReview, PrStatus::NoPr)));
    }

    #[test]
    fn invalid_rules_are_dropped() {
        // Unknown status name.
        assert!(one(Some("nope"), None).is_none());
        // Unknown pr_status name.
        assert!(one(None, Some("nope")).is_none());
        // No condition at all — would fire on every transition.
        assert!(one(None, None).is_none());
        // Empty command.
        let mut r = rule(Some("error"), None);
        r.run = "  ".to_string();
        assert!(compile(&[r], "test").is_empty());
    }

    #[test]
    fn valid_rules_survive_alongside_invalid_ones() {
        let rules = vec![
            rule(Some("bogus"), None),
            rule(Some("needs_review"), None),
            rule(None, None),
        ];
        let compiled = compile(&rules, "test");
        assert_eq!(compiled.len(), 1);
        assert_eq!(compiled[0].status, Some(WorktreeStatus::NeedsReview));
    }

    #[test]
    fn unnamed_rules_get_a_positional_name() {
        let mut r = rule(Some("error"), None);
        r.name = None;
        assert_eq!(compile(&[r], "test")[0].name, "hooks[0]");
    }

    #[test]
    fn name_of_matches_config_spelling() {
        assert_eq!(name_of(&WorktreeStatus::NeedsReview), "needs_review");
        assert_eq!(name_of(&PrStatus::ChecksPassing), "checks_passing");
    }
}

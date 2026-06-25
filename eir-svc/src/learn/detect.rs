//! Pure pattern detectors over already-recorded audit rows. No I/O, no AI — fully
//! unit-testable with row case tables. Phase 1: self-updater detection.

use crate::updater::check::base_id;
use crate::updater::proc::TIMED_OUT;
use std::collections::{HashMap, HashSet};

/// One update attempt, as read from `update_attempts` (the columns the detectors need).
#[derive(Debug, Clone)]
pub struct AttemptRow {
    pub app_id: String,
    pub cycle_id: i64,
    pub success: bool,
    pub exit_code: Option<i32>,
    /// Failure category as the stored snake_case token (NULL/None on success).
    pub category: Option<String>,
    pub detail: String,
}

/// A learned self-updater: an app whose package-manager update keeps timing out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfUpdaterCandidate {
    /// `base_id` of the app (choco `.install`/etc. variants collapsed to one identity).
    pub subject: String,
    /// Distinct cycles in which the update timed out — the strength of the evidence.
    pub timeout_cycles: i64,
}

/// True when an attempt represents a package-manager TIMEOUT — the self-updater signal.
/// A timeout is recorded as `category == network_transient` (classify_error maps
/// "timed out" → NetworkTransient, NOT InstallerFailed) together with either the
/// `proc::TIMED_OUT` exit code or a "timed out" detail. Requiring the network_transient
/// category is essential: the exit code `-4` is ALSO used by the native method to abort
/// when a staged installer's hash changed (a tamper guard, category `hash_mismatch`) —
/// keying on the bare code would mislabel a tampering signal as "this app self-updates".
fn is_timeout(r: &AttemptRow) -> bool {
    !r.success
        && r.category.as_deref() == Some("network_transient")
        && (r.exit_code == Some(TIMED_OUT) || r.detail.to_lowercase().contains("timed out"))
}

/// Apps that timed out in at least `quorum` distinct cycles AND never succeeded in the
/// window — the deterministic "this is a self-updater, stop fighting it" signal. The
/// zero-successes guard stops a single bad-release week from sidelining a healthy app.
pub fn detect_self_updaters(rows: &[AttemptRow], quorum: i64) -> Vec<SelfUpdaterCandidate> {
    struct Agg {
        timeout_cycles: HashSet<i64>,
        successes: i64,
    }
    let mut by_app: HashMap<String, Agg> = HashMap::new();
    for r in rows {
        let agg = by_app.entry(base_id(&r.app_id).to_string()).or_insert(Agg {
            timeout_cycles: HashSet::new(),
            successes: 0,
        });
        if r.success {
            agg.successes += 1;
        } else if is_timeout(r) {
            agg.timeout_cycles.insert(r.cycle_id);
        }
    }
    let mut out: Vec<SelfUpdaterCandidate> = by_app
        .into_iter()
        .filter(|(_, a)| a.successes == 0 && (a.timeout_cycles.len() as i64) >= quorum)
        .map(|(subject, a)| SelfUpdaterCandidate {
            subject,
            timeout_cycles: a.timeout_cycles.len() as i64,
        })
        .collect();
    // Deterministic order (HashMap iteration is not), so callers/tests are stable.
    out.sort_by(|a, b| a.subject.cmp(&b.subject));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        app: &str,
        cycle: i64,
        success: bool,
        exit: Option<i32>,
        cat: &str,
        detail: &str,
    ) -> AttemptRow {
        AttemptRow {
            app_id: app.to_string(),
            cycle_id: cycle,
            success,
            exit_code: exit,
            category: if cat.is_empty() {
                None
            } else {
                Some(cat.to_string())
            },
            detail: detail.to_string(),
        }
    }

    fn timeout_row(app: &str, cycle: i64) -> AttemptRow {
        row(
            app,
            cycle,
            false,
            Some(TIMED_OUT),
            "network_transient",
            "command timed out after 600s",
        )
    }

    #[test]
    fn three_timeout_cycles_zero_successes_is_a_self_updater() {
        let rows = vec![
            timeout_row("discord.install", 1),
            timeout_row("discord.install", 2),
            timeout_row("discord", 3), // collapses to the same base id
        ];
        let got = detect_self_updaters(&rows, 3);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].subject, "discord");
        assert_eq!(got[0].timeout_cycles, 3);
    }

    #[test]
    fn category_fallback_detects_timeout_without_the_exit_code() {
        // A timeout recorded with the network_transient category + detail but no exit code.
        let rows = vec![
            row(
                "foo",
                1,
                false,
                None,
                "network_transient",
                "the request timed out",
            ),
            row(
                "foo",
                2,
                false,
                None,
                "network_transient",
                "command timed out",
            ),
            row(
                "foo",
                3,
                false,
                None,
                "network_transient",
                "timed out and was terminated",
            ),
        ];
        assert_eq!(detect_self_updaters(&rows, 3).len(), 1);
    }

    #[test]
    fn any_success_in_window_disqualifies() {
        let rows = vec![
            timeout_row("vlc", 1),
            timeout_row("vlc", 2),
            timeout_row("vlc", 3),
            row("vlc", 4, true, Some(0), "", "updated"),
        ];
        assert!(detect_self_updaters(&rows, 3).is_empty());
    }

    #[test]
    fn below_quorum_is_not_a_fact() {
        let rows = vec![timeout_row("obs", 1), timeout_row("obs", 2)];
        assert!(detect_self_updaters(&rows, 3).is_empty());
    }

    #[test]
    fn repeated_timeouts_in_one_cycle_count_once() {
        // Same cycle id three times is one cycle of evidence, not three.
        let rows = vec![
            timeout_row("krita", 7),
            timeout_row("krita", 7),
            timeout_row("krita", 7),
        ];
        assert!(detect_self_updaters(&rows, 3).is_empty());
    }

    #[test]
    fn non_timeout_failures_do_not_count() {
        // InstallerFailed (a real install error) is not the self-updater signal.
        let rows = vec![
            row("app", 1, false, Some(1), "installer_failed", "exit code 1"),
            row("app", 2, false, Some(1), "installer_failed", "exit code 1"),
            row("app", 3, false, Some(1), "installer_failed", "exit code 1"),
        ];
        assert!(detect_self_updaters(&rows, 3).is_empty());
    }

    #[test]
    fn native_tamper_abort_is_not_a_timeout() {
        // The native method aborts with exit_code -4 (== TIMED_OUT) AND category
        // hash_mismatch when a staged installer changed before launch (tamper guard).
        // That integrity signal must NEVER be read as a self-updater timeout.
        let rows = vec![
            row(
                "app",
                1,
                false,
                Some(TIMED_OUT),
                "hash_mismatch",
                "staged installer changed before launch — aborted (possible tampering)",
            ),
            row(
                "app",
                2,
                false,
                Some(TIMED_OUT),
                "hash_mismatch",
                "staged installer changed before launch — aborted (possible tampering)",
            ),
            row(
                "app",
                3,
                false,
                Some(TIMED_OUT),
                "hash_mismatch",
                "staged installer changed before launch — aborted (possible tampering)",
            ),
        ];
        assert!(detect_self_updaters(&rows, 3).is_empty());
    }
}

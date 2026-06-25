//! Machine-pattern self-improvement.
//!
//! Eir learns patterns about the specific machine from its own audit history and adapts
//! — deterministically, with no AI in the write-path, and only ever toward *more*
//! conservative behaviour. See ARCHITECTURE.md "Self-improvement" for the full design.
//!
//! Phase 1 (here): learn which apps are self-updaters whose package-manager update keeps
//! timing out (e.g. Discord under choco), so the updater stops fighting them — replacing
//! the hardcoded `SELF_UPDATING` seed in `updater::check` with a fact derived from real
//! `update_attempts` history.

mod detect;
mod store;

pub use store::{active_self_updater_subjects, clear_detector_facts};

use sqlx::SqlitePool;
use std::collections::HashSet;
use tracing::{info, warn};

/// Rolling window (days) over which update-attempt evidence is counted.
const WINDOW_DAYS: i64 = 30;
/// Distinct timed-out cycles (with zero successes) before an app is judged a self-updater.
const SELF_UPDATER_QUORUM: i64 = 3;

/// Detect machine patterns from already-recorded history and persist the learned facts.
/// Runs at the end of an update cycle on data that cycle already collected — no new
/// external I/O, no AI. Best-effort: a failure is logged and never disturbs the cycle.
///
/// Phase 1 derives only `SelfUpdaterSuspected`; later phases add more detectors here.
pub async fn analyse(pool: &SqlitePool) {
    let rows = match store::self_updater_evidence(pool, WINDOW_DAYS).await {
        Ok(rows) => rows,
        Err(e) => {
            warn!("self-improvement: reading update history failed: {e}");
            return;
        }
    };
    let candidates = detect::detect_self_updaters(&rows, SELF_UPDATER_QUORUM);
    let current: HashSet<String> = candidates.iter().map(|c| c.subject.clone()).collect();
    for cand in &candidates {
        match store::upsert_self_updater(pool, cand, WINDOW_DAYS).await {
            Ok(()) => info!(
                app = %cand.subject,
                timeout_cycles = cand.timeout_cycles,
                "self-improvement: will stop managing this app — its update keeps timing out"
            ),
            Err(e) => warn!("self-improvement: persisting {} failed: {e}", cand.subject),
        }
    }
    // Re-probe: expire facts whose evidence has aged out of the window, so a skip
    // self-corrects (a true self-updater simply re-forms its fact on the next timeout).
    if let Err(e) = store::expire_unsupported_self_updaters(pool, &current).await {
        warn!("self-improvement: expiring stale learned facts failed: {e}");
    }
}

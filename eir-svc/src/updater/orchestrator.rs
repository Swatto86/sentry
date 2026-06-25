//! The self-healing loop. For each candidate it tries methods in order; on a
//! failure it picks the next method to try. This phase is purely deterministic
//! (winget -> native ladder); Phase 6 inserts the AI diagnostician between the
//! attempt and the next-method choice. Either way Rust classifies the error and
//! decides — the AI only ever proposes within the bounds Rust allows.

use crate::ai::client::AiClient;
use crate::updater::check;
use crate::updater::config::UpdaterConfig;
use crate::updater::domain::{AttemptOutcome, ErrorCategory, Method, UpdateCandidate};
use crate::updater::history;
use crate::updater::methods::{native, winget};
use sqlx::SqlitePool;
use tracing::warn;

/// Everything an attempt needs that isn't the candidate itself.
pub struct EngineCtx<'a> {
    pub ai: Option<&'a AiClient>,
    pub config: &'a UpdaterConfig,
    /// Model for the web-search calls (the configured `update_check_model`).
    pub model_override: &'a str,
}

/// Run one method against one candidate.
async fn dispatch(
    method: Method,
    candidate: &UpdateCandidate,
    ctx: &EngineCtx<'_>,
) -> AttemptOutcome {
    match method {
        Method::Winget => winget::attempt(candidate).await,
        Method::Native => match ctx.ai {
            Some(ai) => {
                let note = ctx.config.notes.get(&candidate.id).map(String::as_str);
                let max_bytes = ctx.config.max_installer_mb.saturating_mul(1024 * 1024);
                native::update_native(
                    ai,
                    &candidate.name,
                    &candidate.current,
                    note,
                    ctx.config.native_signature_policy,
                    max_bytes,
                    ctx.model_override,
                )
                .await
            }
            None => AttemptOutcome::failed(
                Method::Native,
                ErrorCategory::NotFound,
                "no AI provider configured for native installs",
            ),
        },
        // Implemented in Phase 7; never reached today (check never proposes them).
        Method::Choco | Method::Scoop | Method::MsStore => {
            AttemptOutcome::failed(method, ErrorCategory::NotFound, "method not available yet")
        }
    }
}

/// The deterministic next-method choice: stop on success or a terminal integrity
/// failure, otherwise the first available method not yet tried. Pure — this is the
/// loop core, and Phase 6's AI diagnostician is layered on top of it as a fallback.
fn next_method(order: &[Method], tried: &[Method], last: &AttemptOutcome) -> Option<Method> {
    if last.success {
        return None;
    }
    if last
        .category
        .map(ErrorCategory::is_terminal)
        .unwrap_or(false)
    {
        return None;
    }
    order.iter().copied().find(|m| !tried.contains(m))
}

/// Heal one candidate: try methods until one verifies, an integrity failure is hit,
/// the methods are exhausted, or the attempt cap is reached.
pub async fn heal(
    candidate: &UpdateCandidate,
    ctx: &EngineCtx<'_>,
    available: &[Method],
) -> Vec<AttemptOutcome> {
    let order: Vec<Method> = candidate
        .methods
        .iter()
        .copied()
        .filter(|m| available.contains(m))
        .collect();
    let max = (ctx.config.max_attempts_per_app as usize).max(1);
    let mut attempts: Vec<AttemptOutcome> = Vec::new();
    let mut tried: Vec<Method> = Vec::new();
    let mut next = order.first().copied();

    while let Some(method) = next {
        if attempts.len() >= max {
            break;
        }
        let outcome = dispatch(method, candidate, ctx).await;
        tried.push(method);
        next = next_method(&order, &tried, &outcome);
        attempts.push(outcome);
    }
    attempts
}

/// Methods usable right now: enabled in config and implemented. Package-manager
/// detection (choco/scoop presence) arrives in Phase 7; until then only winget and
/// native are offered.
pub fn available_methods(cfg: &UpdaterConfig, ai: Option<&AiClient>) -> Vec<Method> {
    let enabled: Vec<Method> = cfg
        .methods
        .iter()
        .filter_map(|m| Method::from_token(m))
        .collect();
    let mut v = Vec::new();
    if enabled.contains(&Method::Winget) {
        v.push(Method::Winget);
    }
    if cfg.native_enabled && ai.is_some() {
        v.push(Method::Native);
    }
    v
}

/// The outcome of one full update cycle.
pub struct CycleSummary {
    pub cycle_id: i64,
    pub results: Vec<(UpdateCandidate, Vec<AttemptOutcome>)>,
    pub notes: Vec<String>,
    pub cost_usd: f64,
}

/// Run one full cycle: check for candidates, heal each (bounded by the per-run app
/// and budget caps), and persist every attempt. The cycle id groups this run's
/// attempts in the history table.
pub async fn run_cycle(pool: &SqlitePool, ctx: &EngineCtx<'_>, cycle_id: i64) -> CycleSummary {
    let check = check::collect(ctx.ai, ctx.config, ctx.model_override).await;
    let available = available_methods(ctx.config, ctx.ai);
    let budget = ctx.config.budget_usd_per_run;
    let mut spent = check.cost_usd;
    let mut notes = check.notes;
    let mut results = Vec::new();

    for cand in check
        .candidates
        .into_iter()
        .take(ctx.config.max_apps_per_run as usize)
    {
        if budget > 0.0 && spent >= budget {
            notes.push(format!(
                "Stopped at the £/$ budget after {} apps.",
                results.len()
            ));
            break;
        }
        let outcomes = heal(&cand, ctx, &available).await;
        spent += outcomes.iter().map(|o| o.cost_usd).sum::<f64>();
        if let Err(e) = history::record_attempts(pool, cycle_id, &cand, &outcomes).await {
            warn!("failed to record update history for {}: {e}", cand.name);
        }
        results.push((cand, outcomes));
    }

    CycleSummary {
        cycle_id,
        results,
        notes,
        cost_usd: spent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::updater::domain::Verification;

    fn outcome(method: Method, success: bool, category: Option<ErrorCategory>) -> AttemptOutcome {
        AttemptOutcome {
            method,
            success,
            verification: if success {
                Verification::Verified
            } else {
                Verification::NotChecked
            },
            category,
            exit_code: None,
            installed_version: None,
            detail: String::new(),
            signature: None,
            sha256: None,
            cost_usd: 0.0,
        }
    }

    #[test]
    fn next_method_stops_on_success() {
        let order = [Method::Winget, Method::Native];
        let last = outcome(Method::Winget, true, None);
        assert_eq!(next_method(&order, &[Method::Winget], &last), None);
    }

    #[test]
    fn next_method_stops_on_terminal_integrity_failure() {
        let order = [Method::Winget, Method::Native];
        let last = outcome(
            Method::Native,
            false,
            Some(ErrorCategory::SignatureRejected),
        );
        // Even though winget is untried, a terminal integrity failure ends it.
        assert_eq!(next_method(&order, &[Method::Native], &last), None);
    }

    #[test]
    fn next_method_advances_to_the_next_untried_method() {
        let order = [Method::Winget, Method::Native];
        let last = outcome(Method::Winget, false, Some(ErrorCategory::InstallerFailed));
        assert_eq!(
            next_method(&order, &[Method::Winget], &last),
            Some(Method::Native)
        );
        // Once both are tried, there's nowhere left to go.
        let last2 = outcome(Method::Native, false, Some(ErrorCategory::InstallerFailed));
        assert_eq!(
            next_method(&order, &[Method::Winget, Method::Native], &last2),
            None
        );
    }
}

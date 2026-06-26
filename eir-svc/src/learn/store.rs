//! Persistence for learned facts: read the audit evidence the detectors need, record
//! approval rejections, and upsert/reconcile/query the `learned_facts` table (migrations
//! 0008/0009). Same bare-query/bind style as `audit.rs` and `updater::history`.

use super::detect::{AttemptRow, FeedbackRow, RejectionRow};
use anyhow::Result;
use sqlx::{Row, SqlitePool};
use std::collections::HashSet;

const KIND_SELF_UPDATER: &str = "self_updater_suspected";

fn cutoff(window_days: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::days(window_days)).to_rfc3339()
}

// ── Evidence reads ──────────────────────────────────────────────────────────────

/// Update attempts in the rolling window — evidence for self-updater and method detection.
pub async fn update_attempt_rows(pool: &SqlitePool, window_days: i64) -> Result<Vec<AttemptRow>> {
    let rows = sqlx::query(
        "SELECT app_id, cycle_id, success, exit_code, category, method, detail \
         FROM update_attempts WHERE created_at > ?",
    )
    .bind(cutoff(window_days))
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(AttemptRow {
            app_id: r.try_get("app_id")?,
            cycle_id: r.try_get("cycle_id")?,
            success: r.try_get::<i64, _>("success")? != 0,
            exit_code: r.try_get::<Option<i64>, _>("exit_code")?.map(|v| v as i32),
            category: r.try_get::<Option<String>, _>("category")?,
            method: r.try_get("method").unwrap_or_default(),
            detail: r.try_get("detail").unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Execution-feedback rows in the window — evidence for ineffective-fix detection.
pub async fn fix_feedback_rows(pool: &SqlitePool, window_days: i64) -> Result<Vec<FeedbackRow>> {
    let rows = sqlx::query(
        "SELECT action, succeeded, failed_services_before, failed_services_after \
         FROM execution_feedback WHERE recorded_at > ?",
    )
    .bind(cutoff(window_days))
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(FeedbackRow {
            action: r.try_get("action")?,
            succeeded: r.try_get::<i64, _>("succeeded")? != 0,
            failed_services_before: r.try_get("failed_services_before")?,
            failed_services_after: r.try_get("failed_services_after")?,
        });
    }
    Ok(out)
}

/// Approval-rejection rows in the window — evidence for user-rejected-action detection.
pub async fn rejection_rows(pool: &SqlitePool, window_days: i64) -> Result<Vec<RejectionRow>> {
    let rows = sqlx::query("SELECT action_label FROM approval_rejections WHERE rejected_at > ?")
        .bind(cutoff(window_days))
        .fetch_all(pool)
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(RejectionRow {
            action_label: r.try_get("action_label")?,
        });
    }
    Ok(out)
}

/// Record that the user rejected a queued action (drives RejectedSignal learning).
pub async fn record_rejection(
    pool: &SqlitePool,
    decision_id: i64,
    action_label: &str,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO approval_rejections (decision_id, action_label, rejected_at) VALUES (?, ?, ?)",
    )
    .bind(decision_id)
    .bind(action_label)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

// ── Learned-fact writes ─────────────────────────────────────────────────────────

/// Insert or reinforce a learned fact. Idempotent via UNIQUE(kind,subject); a user
/// `pinned`/`disabled` override is preserved across reinforcement.
pub async fn upsert_fact(
    pool: &SqlitePool,
    kind: &str,
    subject: &str,
    effect_json: &str,
    evidence: &str,
    window_days: i64,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO learned_facts \
            (kind, subject, effect_json, evidence_count, evidence_json, window_days, \
             first_seen_at, last_reinforced_at, status, source) \
         VALUES (?, ?, ?, 0, ?, ?, ?, ?, 'active', 'detector') \
         ON CONFLICT(kind, subject) DO UPDATE SET \
            effect_json = excluded.effect_json, \
            evidence_json = excluded.evidence_json, \
            last_reinforced_at = excluded.last_reinforced_at, \
            status = CASE WHEN learned_facts.status IN ('user_disabled', 'user_pinned') \
                          THEN learned_facts.status ELSE 'active' END",
    )
    .bind(kind)
    .bind(subject)
    .bind(effect_json)
    .bind(evidence)
    .bind(window_days)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark as `expired` any `active` detector fact of this kind whose subject is no longer
/// supported by current evidence (`current`). This is the re-probe/decay: once a skip or
/// deprioritisation stops being re-confirmed (its evidence ages out of the window) it
/// lapses, so the behaviour self-corrects. User overrides are never touched.
pub async fn expire_unsupported(
    pool: &SqlitePool,
    kind: &str,
    current: &HashSet<String>,
) -> Result<()> {
    let rows = sqlx::query(
        "SELECT subject FROM learned_facts \
         WHERE kind = ? AND source = 'detector' AND status = 'active'",
    )
    .bind(kind)
    .fetch_all(pool)
    .await?;
    for r in rows {
        let subject: String = r.try_get("subject")?;
        if !current.contains(&subject) {
            sqlx::query(
                "UPDATE learned_facts SET status = 'expired' \
                 WHERE kind = ? AND subject = ? AND source = 'detector' AND status = 'active'",
            )
            .bind(kind)
            .bind(&subject)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Remove detector-derived learned facts (the user's "Clear" reset). Only `active` and
/// `expired` rows go; `pinned`/`disabled` overrides are preserved. Returns rows removed.
pub async fn clear_detector_facts(pool: &SqlitePool) -> Result<u64> {
    let res = sqlx::query(
        "DELETE FROM learned_facts WHERE source = 'detector' AND status IN ('active', 'expired')",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

// ── Learned-fact reads ──────────────────────────────────────────────────────────

/// A learned fact in force, for application at the decision seams.
pub struct FactRow {
    pub kind: String,
    pub subject: String,
    pub effect_json: String,
    pub evidence: String,
}

/// All facts currently IN FORCE (`active` or `user_pinned`). Consulted by `apply`.
pub async fn active_facts(pool: &SqlitePool) -> Result<Vec<FactRow>> {
    let rows = sqlx::query(
        "SELECT kind, subject, effect_json, evidence_json FROM learned_facts \
         WHERE status IN ('active', 'user_pinned')",
    )
    .fetch_all(pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(FactRow {
            kind: r.try_get("kind")?,
            subject: r.try_get("subject")?,
            effect_json: r.try_get("effect_json")?,
            evidence: r.try_get("evidence_json").unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Just the self-updater subjects in force — the updater's candidate filter consults this.
pub async fn active_self_updater_subjects(pool: &SqlitePool) -> Result<HashSet<String>> {
    let rows = sqlx::query(
        "SELECT subject FROM learned_facts \
         WHERE kind = ? AND status IN ('active', 'user_pinned')",
    )
    .bind(KIND_SELF_UPDATER)
    .fetch_all(pool)
    .await?;
    let mut set = HashSet::with_capacity(rows.len());
    for r in rows {
        set.insert(r.try_get::<String, _>("subject")?);
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn::{Effect, LearnedFactKind};

    async fn test_pool(tag: &str) -> SqlitePool {
        let path = std::env::temp_dir().join(format!("eir-learn-{tag}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let url = format!(
            "sqlite:{}?mode=rwc",
            path.to_string_lossy().replace('\\', "/")
        );
        let pool = SqlitePool::connect(&url).await.expect("open db");
        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrate");
        pool
    }

    #[tokio::test]
    async fn fact_round_trips_reinforces_and_reconciles() {
        let pool = test_pool("round").await;
        assert!(active_self_updater_subjects(&pool)
            .await
            .unwrap()
            .is_empty());

        let su = LearnedFactKind::SelfUpdaterSuspected.as_token();
        upsert_fact(
            &pool,
            su,
            "discord",
            &Effect::Skip.to_json(),
            "3 timeouts",
            30,
        )
        .await
        .unwrap();
        assert!(active_self_updater_subjects(&pool)
            .await
            .unwrap()
            .contains("discord"));

        // Reinforce: no duplicate row.
        upsert_fact(
            &pool,
            su,
            "discord",
            &Effect::Skip.to_json(),
            "5 timeouts",
            30,
        )
        .await
        .unwrap();
        let n: i64 = sqlx::query("SELECT COUNT(*) AS n FROM learned_facts")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("n");
        assert_eq!(n, 1);

        // Reconcile with empty current → expires; user override survives.
        sqlx::query(
            "INSERT INTO learned_facts (kind, subject, effect_json, first_seen_at, last_reinforced_at, status, source) \
             VALUES (?, 'pinned', '{\"type\":\"skip\"}', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 'user_pinned', 'detector')",
        )
        .bind(su)
        .execute(&pool)
        .await
        .unwrap();
        expire_unsupported(&pool, su, &HashSet::new())
            .await
            .unwrap();
        let active = active_self_updater_subjects(&pool).await.unwrap();
        assert!(
            !active.contains("discord"),
            "expired detector fact must lapse"
        );
        assert!(active.contains("pinned"), "user_pinned survives reconcile");

        // Clear removes detector facts but preserves the user override.
        assert_eq!(clear_detector_facts(&pool).await.unwrap(), 1);
        let after = active_facts(&pool).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].subject, "pinned");
        drop(pool);
    }

    #[tokio::test]
    async fn rejection_round_trips() {
        let pool = test_pool("reject").await;
        record_rejection(&pool, 1, "ProcessKill { process_name: \"x\" }")
            .await
            .unwrap();
        let rows = rejection_rows(&pool, 30).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].action_label, "ProcessKill { process_name: \"x\" }");
        drop(pool);
    }
}

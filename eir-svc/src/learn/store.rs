//! Persistence for learned facts: read the audit evidence the detectors need, and
//! upsert/query the `learned_facts` table (migration 0008). Same bare-query/bind style
//! as `audit.rs` and `updater::history`.

use super::detect::{AttemptRow, SelfUpdaterCandidate};
use anyhow::Result;
use sqlx::{Row, SqlitePool};
use std::collections::HashSet;

/// Token stored in `learned_facts.kind` for a suspected self-updater.
const KIND_SELF_UPDATER: &str = "self_updater_suspected";

/// Read the update attempts in the rolling window (the raw evidence for self-updater
/// detection). `success` is read as an integer; `category` may be NULL on success.
pub async fn self_updater_evidence(pool: &SqlitePool, window_days: i64) -> Result<Vec<AttemptRow>> {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(window_days)).to_rfc3339();
    let rows = sqlx::query(
        "SELECT app_id, cycle_id, success, exit_code, category, detail \
         FROM update_attempts WHERE created_at > ?",
    )
    .bind(cutoff)
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
            detail: r.try_get("detail").unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Insert or reinforce a learned self-updater fact. Idempotent via UNIQUE(kind,subject):
/// a re-confirmation bumps the evidence and timestamp but never resurrects a fact the
/// user has disabled.
pub async fn upsert_self_updater(
    pool: &SqlitePool,
    cand: &SelfUpdaterCandidate,
    window_days: i64,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let evidence = format!(
        "{} timed-out update cycles, 0 successes in {}d",
        cand.timeout_cycles, window_days
    );
    sqlx::query(
        "INSERT INTO learned_facts \
            (kind, subject, effect_json, evidence_count, evidence_json, window_days, \
             first_seen_at, last_reinforced_at, status, source) \
         VALUES (?, ?, '\"skip\"', ?, ?, ?, ?, ?, 'active', 'detector') \
         ON CONFLICT(kind, subject) DO UPDATE SET \
            evidence_count = excluded.evidence_count, \
            evidence_json  = excluded.evidence_json, \
            last_reinforced_at = excluded.last_reinforced_at, \
            status = CASE WHEN learned_facts.status = 'user_disabled' \
                          THEN 'user_disabled' ELSE 'active' END",
    )
    .bind(KIND_SELF_UPDATER)
    .bind(&cand.subject)
    .bind(cand.timeout_cycles)
    .bind(&evidence)
    .bind(window_days)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark as `expired` any detector self-updater fact whose evidence no longer meets the
/// quorum this cycle (its subject isn't in `current`). This is the Phase-1 re-probe:
/// once an app is skipped it stops producing attempts, so its timeouts age out of the
/// window, the fact expires, and the app is retried next cycle — self-correcting a false
/// positive (e.g. a large/slow install that merely exceeded the time cap, not a true
/// self-updater). User `pinned`/`disabled` facts are never touched.
pub async fn expire_unsupported_self_updaters(
    pool: &SqlitePool,
    current: &HashSet<String>,
) -> Result<()> {
    let rows = sqlx::query(
        "SELECT subject FROM learned_facts \
         WHERE kind = ? AND source = 'detector' AND status = 'active'",
    )
    .bind(KIND_SELF_UPDATER)
    .fetch_all(pool)
    .await?;
    for r in rows {
        let subject: String = r.try_get("subject")?;
        if !current.contains(&subject) {
            sqlx::query(
                "UPDATE learned_facts SET status = 'expired' \
                 WHERE kind = ? AND subject = ? AND source = 'detector' AND status = 'active'",
            )
            .bind(KIND_SELF_UPDATER)
            .bind(&subject)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Remove detector-derived learned facts — the user's "Clear" reset. Only `active` and
/// `expired` rows go; a fact the user `pinned`/`disabled` is a deliberate override and is
/// preserved even though it too has `source = 'detector'`. Returns the rows removed.
pub async fn clear_detector_facts(pool: &SqlitePool) -> Result<u64> {
    let res = sqlx::query(
        "DELETE FROM learned_facts WHERE source = 'detector' AND status IN ('active', 'expired')",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// The set of app subjects currently learned to be self-updaters and in force
/// (`active` or `user_pinned`; a `user_disabled` fact is intentionally excluded so the
/// user override wins). Consulted by the updater's candidate filter.
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

    #[tokio::test]
    async fn learned_fact_round_trips_and_reinforces() {
        let path = std::env::temp_dir().join(format!("eir-learn-{}.db", std::process::id()));
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

        // Nothing learned yet.
        assert!(active_self_updater_subjects(&pool)
            .await
            .unwrap()
            .is_empty());

        let cand = SelfUpdaterCandidate {
            subject: "discord".into(),
            timeout_cycles: 3,
        };
        upsert_self_updater(&pool, &cand, 30).await.expect("insert");
        let subjects = active_self_updater_subjects(&pool).await.unwrap();
        assert!(subjects.contains("discord"));
        assert_eq!(subjects.len(), 1);

        // Reinforcing the same fact must not duplicate it (UNIQUE(kind,subject)).
        let stronger = SelfUpdaterCandidate {
            subject: "discord".into(),
            timeout_cycles: 5,
        };
        upsert_self_updater(&pool, &stronger, 30)
            .await
            .expect("reinforce");
        let count: i64 = sqlx::query("SELECT COUNT(*) AS n FROM learned_facts")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("n");
        assert_eq!(count, 1);
        let evidence: i64 = sqlx::query("SELECT evidence_count AS n FROM learned_facts")
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("n");
        assert_eq!(evidence, 5);

        drop(pool);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reconcile_expires_unsupported_but_spares_user_facts() {
        let path = std::env::temp_dir().join(format!("eir-learn2-{}.db", std::process::id()));
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

        // A detector fact (active) and a user-pinned fact (a deliberate override).
        upsert_self_updater(
            &pool,
            &SelfUpdaterCandidate {
                subject: "discord".into(),
                timeout_cycles: 3,
            },
            30,
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO learned_facts \
                (kind, subject, effect_json, first_seen_at, last_reinforced_at, status, source) \
             VALUES (?, 'pinnedapp', '\"skip\"', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z', 'user_pinned', 'detector')",
        )
        .bind(KIND_SELF_UPDATER)
        .execute(&pool)
        .await
        .unwrap();

        // Reconcile with NO current candidates: the detector fact expires; the user fact stays.
        expire_unsupported_self_updaters(&pool, &HashSet::new())
            .await
            .unwrap();
        let active = active_self_updater_subjects(&pool).await.unwrap();
        assert!(
            !active.contains("discord"),
            "expired detector fact must not apply"
        );
        assert!(
            active.contains("pinnedapp"),
            "user_pinned fact must still apply"
        );

        // Clear removes the (now-expired) detector fact but preserves the user override.
        let removed = clear_detector_facts(&pool).await.unwrap();
        assert_eq!(removed, 1);
        let after = active_self_updater_subjects(&pool).await.unwrap();
        assert_eq!(after.len(), 1);
        assert!(after.contains("pinnedapp"));

        drop(pool);
        let _ = std::fs::remove_file(&path);
    }
}

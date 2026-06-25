-- Self-improvement: machine-pattern learned facts (see ARCHITECTURE.md
-- "Self-improvement"). Phase 1 writes only SelfUpdaterSuspected{app} -> skip; the
-- decay/AI columns are present (with defaults) so later phases need no new migration.
CREATE TABLE IF NOT EXISTS learned_facts (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    kind               TEXT    NOT NULL,            -- closed token mirrored by LearnedFactKind
    subject            TEXT    NOT NULL,            -- app_id | action_type | fingerprint | "app_id\u{1f}method"
    effect_json        TEXT    NOT NULL,            -- conservative-only Effect, serialised
    evidence_count     INTEGER NOT NULL DEFAULT 0,  -- supporting observations at last detect
    evidence_json      TEXT    NOT NULL DEFAULT '', -- compact provenance for the UI
    window_days        INTEGER NOT NULL DEFAULT 30, -- rolling window the quorum was measured over
    half_life_days     REAL    NOT NULL DEFAULT 30.0, -- decay constant (used from Phase 2)
    first_seen_at      TEXT    NOT NULL,            -- RFC3339 UTC
    last_reinforced_at TEXT    NOT NULL,            -- bumped each time the detector re-confirms
    status             TEXT    NOT NULL DEFAULT 'active', -- active | expired | user_pinned | user_disabled
    source             TEXT    NOT NULL DEFAULT 'detector', -- detector | ai_labelled
    ai_explanation     TEXT,                        -- NULL until Phase 5
    UNIQUE(kind, subject)
);

CREATE INDEX IF NOT EXISTS idx_learned_facts_kind ON learned_facts(kind, subject);

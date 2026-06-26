-- Records each fix the user rejected from the approval queue, so the self-improvement
-- layer can learn to propose a repeatedly-rejected action less readily (RejectedSignal).
-- See ARCHITECTURE.md "Self-improvement".
CREATE TABLE IF NOT EXISTS approval_rejections (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    decision_id  INTEGER NOT NULL,
    action_label TEXT    NOT NULL,   -- format!("{action:?}"), the same key the loop uses
    rejected_at  TEXT    NOT NULL    -- RFC3339 UTC
);

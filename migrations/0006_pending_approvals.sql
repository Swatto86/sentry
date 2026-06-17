-- Actions awaiting the user's decision. Persisted so an approval survives both
-- idle decision cycles and a service restart (e.g. after a settings change),
-- rather than expiring on a short timer the way the old blocking flow did.
-- The row id is the approval id surfaced to the UI.
CREATE TABLE IF NOT EXISTS pending_approvals (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at    TEXT    NOT NULL,
    decision_id   INTEGER NOT NULL,
    action_json   TEXT    NOT NULL,  -- serialized FixAction, executed verbatim on approval
    info_json     TEXT    NOT NULL,  -- serialized ApprovalInfo for the UI (id filled from this row)
    baseline_json TEXT    NOT NULL,  -- SystemState at proposal time, for post-execution feedback
    FOREIGN KEY(decision_id) REFERENCES decisions(id)
);

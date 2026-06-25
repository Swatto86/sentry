-- Append-only log of every update attempt the autonomous updater makes, grouped by
-- cycle. Lets the UI show what was tried for each app, by which method, and why it
-- failed — and gives an audit trail for unattended installs.
CREATE TABLE IF NOT EXISTS update_attempts (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    cycle_id     INTEGER NOT NULL,        -- groups all attempts from one run
    app_id       TEXT    NOT NULL,        -- stable, version-stripped app identity
    name         TEXT    NOT NULL,        -- display name
    from_version TEXT,                    -- version installed before the attempt
    to_version   TEXT,                    -- version observed after (if read)
    method       TEXT    NOT NULL,        -- winget | choco | scoop | msstore | native
    success      INTEGER NOT NULL,        -- 1 = updated + verified
    category     TEXT,                    -- failure ErrorCategory, NULL on success
    exit_code    INTEGER,                 -- the method/installer exit code
    signature    TEXT,                    -- Authenticode result for native installs
    sha256       TEXT,                    -- installer hash for native installs
    detail       TEXT,                    -- cleaned message / reason
    cost_usd     REAL,                    -- AI spend attributable to this attempt
    created_at   TEXT    NOT NULL         -- RFC3339 UTC
);

CREATE INDEX IF NOT EXISTS idx_update_attempts_app ON update_attempts(app_id, created_at);
CREATE INDEX IF NOT EXISTS idx_update_attempts_cycle ON update_attempts(cycle_id);

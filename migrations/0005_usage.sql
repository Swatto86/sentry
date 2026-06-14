-- Per-call Claude usage, recorded when the claude_cli provider returns usage data.
CREATE TABLE IF NOT EXISTS usage_log (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp         TEXT    NOT NULL,
    input_tokens      INTEGER NOT NULL DEFAULT 0,
    output_tokens     INTEGER NOT NULL DEFAULT 0,
    cache_creation    INTEGER NOT NULL DEFAULT 0,
    cache_read        INTEGER NOT NULL DEFAULT 0,
    cost_usd          REAL    NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_usage_log_timestamp ON usage_log (timestamp);

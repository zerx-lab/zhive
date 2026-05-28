-- Logs database initial schema (D-011 revised).
--
-- Append-only structured log sink. Independent of the state DB so heavy
-- log volume does not bloat the main session index (codex PR #24591
-- pattern).

CREATE TABLE logs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp   INTEGER NOT NULL,
    level       TEXT NOT NULL,
    target      TEXT NOT NULL,
    message     TEXT NOT NULL,
    thread_id   TEXT,
    fields      TEXT
);

CREATE INDEX idx_logs_timestamp ON logs (timestamp DESC);
CREATE INDEX idx_logs_thread ON logs (thread_id, timestamp);
CREATE INDEX idx_logs_level ON logs (level, timestamp DESC);

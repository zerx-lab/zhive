-- Goals database initial schema (D-011 revised).
--
-- Lightweight task / goal tracking; intentionally a separate database
-- so the much larger state DB does not have to migrate every time a
-- goal-related column is added.

CREATE TABLE goals (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id       TEXT,
    description     TEXT NOT NULL,
    status          TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    completed_at    INTEGER
);

CREATE INDEX idx_goals_status ON goals (status, created_at DESC);
CREATE INDEX idx_goals_thread ON goals (thread_id, created_at);

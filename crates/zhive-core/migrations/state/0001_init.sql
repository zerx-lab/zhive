-- State database initial schema (D-011 revised).
--
-- Holds the canonical Thread / Turn / Item index. The JSONL rollout
-- remains the source of truth (see `persistence::rollout`); this
-- database is a queryable projection that can be rebuilt from JSONL
-- when needed.

CREATE TABLE threads (
    id              TEXT PRIMARY KEY NOT NULL,
    session_id      TEXT,
    forked_from     TEXT REFERENCES threads(id),
    preview         TEXT NOT NULL,
    ephemeral       INTEGER NOT NULL DEFAULT 0,
    model_provider  TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    status          TEXT NOT NULL,
    cwd             TEXT NOT NULL,
    source          TEXT NOT NULL,
    name            TEXT
);

CREATE INDEX idx_threads_updated_at ON threads (updated_at DESC);
CREATE INDEX idx_threads_forked_from ON threads (forked_from);

CREATE TABLE turns (
    id              TEXT PRIMARY KEY NOT NULL,
    thread_id       TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    status          TEXT NOT NULL,
    error_message   TEXT,
    error_details   TEXT,
    started_at      INTEGER,
    completed_at    INTEGER,
    duration_ms     INTEGER
);

CREATE INDEX idx_turns_thread ON turns (thread_id, started_at);

CREATE TABLE items (
    id              TEXT PRIMARY KEY NOT NULL,
    turn_id         TEXT NOT NULL REFERENCES turns(id) ON DELETE CASCADE,
    seq             INTEGER NOT NULL,
    item_kind       TEXT NOT NULL,
    payload         TEXT NOT NULL
);

CREATE INDEX idx_items_turn ON items (turn_id, seq);

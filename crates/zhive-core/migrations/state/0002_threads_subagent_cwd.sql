-- 0002: three threads-table changes, applied by rebuilding the table (SQLite
-- cannot drop a column constraint in place).
--
-- 1. Drop the `forked_from` FOREIGN KEY. A fork is a cross-file pointer (the
--    source may live in another rollout file or have been deleted), so an
--    isolated forked rollout must still rebuild without the parent row present.
--    The column is kept as a soft, indexed link — just not a foreign key.
-- 2. Add `subagent_parent`: the thread that spawned this one as a subagent.
--    Previously the parent↔child link was only implied by the child thread id
--    naming; this records it so resume/rebuild can recover the relationship.
-- 3. Add an index on `cwd` so sessions can be listed per project (codex-style).
--
-- Existing rows are preserved verbatim (subagent_parent defaults to NULL).

CREATE TABLE threads_new (
    id              TEXT PRIMARY KEY NOT NULL,
    session_id      TEXT,
    forked_from     TEXT,
    subagent_parent TEXT,
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

INSERT INTO threads_new
    (id, session_id, forked_from, preview, ephemeral, model_provider,
     created_at, updated_at, status, cwd, source, name)
SELECT
    id, session_id, forked_from, preview, ephemeral, model_provider,
    created_at, updated_at, status, cwd, source, name
FROM threads;

DROP TABLE threads;
ALTER TABLE threads_new RENAME TO threads;

CREATE INDEX idx_threads_updated_at ON threads (updated_at DESC);
CREATE INDEX idx_threads_forked_from ON threads (forked_from);
CREATE INDEX idx_threads_cwd ON threads (cwd);

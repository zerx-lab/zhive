-- 0003: per-turn workspace file snapshots backing the file-revert ("undo")
-- feature.
--
-- Each row records the shadow-git tree id captured at the start of a top-level
-- user turn, plus a short preview of that turn's user message so the rewind
-- picker can label checkpoints without parsing item payloads. The JSONL rollout
-- (`RolloutEntry::Snapshot`) remains the source of truth; this table is a
-- queryable projection rebuilt from it.
--
-- No foreign key to `turns`: a snapshot is enqueued at turn start and is
-- intentionally decoupled from the turn row's lifecycle (mirroring the design's
-- standalone checkpoint index). `thread_id` is carried explicitly so the table
-- can be queried and pruned per thread without a join.

CREATE TABLE turn_snapshots (
    thread_id  TEXT NOT NULL,
    turn_id    TEXT NOT NULL,
    tree       TEXT NOT NULL,
    preview    TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    PRIMARY KEY (thread_id, turn_id)
);

CREATE INDEX idx_turn_snapshots_thread ON turn_snapshots (thread_id, created_at);

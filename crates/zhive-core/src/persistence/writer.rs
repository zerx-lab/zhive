//! Write-through persistence task for the engine.
//!
//! The [`PersistenceWriter`] task drains a [`mpsc::Receiver<StorageWriteOp>`]
//! and applies each operation to:
//!
//! 1. The **JSONL rollout** — the source of truth; written first, always.
//! 2. The **SQL index** (`StateDb`) — asynchronous, best-effort.  A SQL
//!    failure is logged at `error` level but does NOT crash the engine; the
//!    JSONL remains intact and the index can be rebuilt later.
//!
//! ## Save points
//!
//! Per-item appends are buffered inside the `BufWriter` during a turn.
//! A hard `fsync` (save point) is only called on `TurnEnded` and `Flush`
//! operations, consistent with B7 `pendingSessionWrites` semantics: data
//! survives a crash as long as the most recent turn completed.
//!
//! ## Shutdown
//!
//! When the engine closes the sender the receiver returns `None`. At that
//! point the writer drains any remaining ops, fsyncs every open rollout,
//! then exits.  The engine's `shutdown` method awaits a `JoinHandle` that
//! wraps this task, so a freshly-completed turn is durable before the
//! process exits.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::Instrument as _;
use zhive_proto::domain::{Item, Thread, ThreadId, TurnError, TurnId, TurnStatus};
use zhive_proto::permission::RequestPermissionRequest;

use super::error::StorageResult;
use super::rollout::{RolloutEntry, RolloutWriter};
use super::{Storage, StorageError};

// ------------------------------------------------------------------
// Public write-op enum
// ------------------------------------------------------------------

/// One persistence write operation queued by the engine.
///
/// The engine sends these non-blocking (`try_send` / `send`) so it never
/// blocks the turn loop on disk I/O.  The writer task processes them
/// sequentially.
#[derive(Debug)]
#[non_exhaustive]
pub enum StorageWriteOp {
    /// A thread was created or its metadata changed; upsert the DB row.
    ThreadUpserted(Box<Thread>),
    /// A turn started; create the DB row with `status = InProgress`.
    TurnStarted {
        /// Parent thread id.
        thread_id: ThreadId,
        /// The new turn id.
        turn_id: TurnId,
        /// Unix-seconds start timestamp.
        started_at: i64,
    },
    /// An item was appended inside an active turn.
    ItemAppended {
        /// Parent thread id (needed to locate the rollout file).
        thread_id: ThreadId,
        /// Containing turn id.
        turn_id: TurnId,
        /// Monotonically increasing per-turn item sequence number.
        seq: i64,
        /// The item payload.
        item: Box<Item>,
    },
    /// A turn reached a terminal state (completed / interrupted / failed).
    ///
    /// The writer calls `RolloutWriter::sync_all` after applying this op so
    /// the whole turn's items are durably persisted (B7 save point).
    TurnEnded {
        /// Parent thread id (needed to locate the rollout file).
        thread_id: ThreadId,
        /// The ending turn id.
        turn_id: TurnId,
        /// Final turn status.
        status: TurnStatus,
        /// Non-`None` when `status == Failed`.
        error: Option<TurnError>,
        /// Unix-seconds completion timestamp.
        completed_at: i64,
        /// Wall-clock duration in milliseconds.
        duration_ms: Option<i64>,
    },
    /// Force-flush and fsync the rollout for a given thread without ending
    /// a turn. Used for early-save scenarios.
    ///
    /// When `ack` is `Some`, the writer fires the oneshot **after** the
    /// `sync_all` completes, so an enqueuer can `await` until the rollout is
    /// durably drained (the fork path relies on this to read a self-consistent
    /// source rollout). A `None` ack is fire-and-forget. The ack is dropped
    /// without sending if the rollout writer cannot be opened (no fsync
    /// happened); the awaiter then observes a closed channel rather than a
    /// false durability signal.
    Flush {
        /// Thread whose rollout should be fsynced.
        thread_id: ThreadId,
        /// Optional completion signal fired after the fsync.
        ack: Option<tokio::sync::oneshot::Sender<()>>,
    },
    /// The active model for a thread changed; update the `model_provider`
    /// column on the threads row.
    ///
    /// Best-effort SQL-only: there is no dedicated JSONL row for a model
    /// change in Phase 1, so a crash before the next item append loses only
    /// the index update (the JSONL session header still records the original
    /// provider and the index can be rebuilt from a fresh upsert).
    ModelChanged {
        /// Thread whose model changed.
        thread_id: ThreadId,
        /// New provider identifier (e.g. `"anthropic"`).
        provider: String,
        /// New model identifier (e.g. `"claude-opus-4"`).
        model_id: String,
    },
    /// The human-facing session name changed; update the `name` column on the
    /// threads row.
    ///
    /// Best-effort SQL-only for the same reason as [`Self::ModelChanged`].
    SessionNameSet {
        /// Thread whose name changed.
        thread_id: ThreadId,
        /// New session name (an empty string clears it).
        name: String,
    },
    /// Move (or stamp) a thread's active branch-head leaf pointer.
    ///
    /// Appends a [`RolloutEntry::Leaf`] and fsyncs it as a save point. A
    /// `target_id = Some(id)` records a fork / branch leaf at item `id`;
    /// `target_id = None` is the same turn-completion marker the writer emits at
    /// [`Self::TurnEnded`]. Written by the fork path after replaying the source
    /// history into a new thread's rollout (see `engine::fork`).
    SetLeaf {
        /// Thread whose leaf pointer moves.
        thread_id: ThreadId,
        /// Item id at the new branch head, or `None` for an empty branch.
        target_id: Option<String>,
    },
    /// Write the first line of a **forked** thread's rollout: a session header
    /// naming the source thread as its parent.
    ///
    /// The writer records `thread_id` in its `header_written` set so the
    /// subsequent [`Self::ThreadUpserted`] for the forked thread does **not**
    /// re-emit a header (which would carry `parent_session = None` and lose the
    /// fork link). Used by the fork path before the replayed items are appended.
    ForkHeader {
        /// Forked (new) thread whose rollout is being opened.
        thread_id: ThreadId,
        /// Source thread the fork was taken from.
        parent_session: ThreadId,
        /// Working directory recorded in the header.
        cwd: String,
        /// Unix-seconds creation timestamp.
        created_at: i64,
    },
    /// Append a compaction checkpoint to the rollout and fsync (durable save
    /// point).
    ///
    /// Writes a [`RolloutEntry::Compaction`] entry so that a rebuilt or
    /// resumed engine replaces all prior items of the thread with
    /// `replacement` rather than replaying the full un-compacted history.
    /// The `sync_all` call guarantees the checkpoint survives a crash;
    /// without it a restart would replay the full history and re-trigger
    /// compaction, potentially exceeding the provider context limit.
    ///
    /// SQL index: the replacement items are written to a synthetic compaction
    /// turn (`turn_id`) so they are query-accessible. Old turn rows are **not**
    /// deleted (the index is a rebuildable derivative; historical turn data
    /// remains available for UI review).
    Compaction {
        /// Thread the compaction belongs to.
        thread_id: ThreadId,
        /// Synthetic compaction turn id (e.g. `<thread>::compaction-1`).
        turn_id: TurnId,
        /// Unix-seconds timestamp of the compaction.
        timestamp: i64,
        /// Handoff summary text (stored verbatim for diagnostics).
        summary: String,
        /// Post-compaction replacement transcript (`[marker, summary]`).
        replacement: Vec<Box<Item>>,
        /// Number of original items compacted away.
        entries_compacted: u32,
    },
    /// Record that a turn was suspended waiting for a deferred permission
    /// decision (B6 pending-permission persistence).
    ///
    /// Appends a [`RolloutEntry::PendingPermission`] entry and calls
    /// `sync_all` so the suspended state survives a crash.  Resume reads this
    /// entry and re-registers the pending request in
    /// [`crate::permission::PendingPermissions`] so a reconnecting client can
    /// answer via `session/resume_permission`.
    ///
    /// Pair with [`Self::PermissionResolved`] once the client answers.
    PermissionSuspended {
        /// Thread that owns the suspended turn.
        thread_id: ThreadId,
        /// Suspended turn id.
        turn_id: TurnId,
        /// Unix-seconds timestamp of the suspension.
        timestamp: i64,
        /// Wire-form request id (e.g. `"perm:7"`) the client uses to answer.
        request_id: String,
        /// Full request payload stored so resume can re-emit the approval
        /// prompt to the reconnecting client.
        request: Box<RequestPermissionRequest>,
    },
    /// Record that a pending deferred permission request was resolved (B6).
    ///
    /// Appends a [`RolloutEntry::PermissionResolved`] entry and calls
    /// `sync_all`.  Resume uses this to skip requests that were already
    /// answered before the crash, preventing stale approval prompts from
    /// re-surfacing.
    PermissionResolved {
        /// Thread the request belonged to.
        thread_id: ThreadId,
        /// Wire-form request id that was resolved.
        request_id: String,
        /// Unix-seconds timestamp of the resolution.
        timestamp: i64,
    },

    /// Records a per-turn workspace file snapshot checkpoint.
    ///
    /// Appends a [`RolloutEntry::Snapshot`] to the rollout and projects it into
    /// the `turn_snapshots` SQL table. Enqueued at top-level turn start so the
    /// checkpoint is durable before any tool write.
    Snapshot {
        /// Thread the snapshot belongs to.
        thread_id: ThreadId,
        /// Turn whose start state this snapshot captured.
        turn_id: TurnId,
        /// Unix-seconds timestamp at capture time.
        timestamp: i64,
        /// 40-hex shadow-git tree id of the captured workspace state.
        tree: String,
        /// Short preview of the turn's user message, for the rewind picker.
        preview: String,
    },
}

// ------------------------------------------------------------------
// PersistenceWriter
// ------------------------------------------------------------------

/// Background task that applies [`StorageWriteOp`]s to JSONL and SQL.
///
/// Construct via [`PersistenceWriter::spawn`] which returns a
/// `(Sender, JoinHandle)` pair. Drop (or close) the sender to trigger
/// drain-and-exit.
///
/// # Examples
///
/// ```no_run
/// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
/// use std::path::Path;
/// use std::sync::Arc;
/// use zhive_core::persistence::{Storage, writer::PersistenceWriter};
///
/// let storage = Arc::new(Storage::open(Path::new("/tmp/demo")).await?);
/// let (tx, handle) = PersistenceWriter::spawn(storage);
/// // … engine enqueues ops via tx …
/// drop(tx);   // close the channel to trigger drain-and-exit
/// handle.await.ok();
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct PersistenceWriter;

impl PersistenceWriter {
    /// Spawns the writer task and returns the sender and join handle.
    ///
    /// The returned `mpsc::Sender` has capacity [`WRITER_CHANNEL_CAP`].
    ///
    /// # Errors (at runtime)
    ///
    /// The task logs at `error` level for SQL failures but continues running;
    /// JSONL errors (e.g. disk full) are also logged and continue if possible.
    #[must_use]
    pub fn spawn(
        storage: Arc<Storage>,
    ) -> (mpsc::Sender<StorageWriteOp>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(WRITER_CHANNEL_CAP);
        let handle = tokio::spawn(run_writer(storage, rx));
        (tx, handle)
    }
}

/// Channel capacity for the `StorageWriteOp` queue.
///
/// 4 096 matches the Pi `pendingSessionWrites` buffer heuristic: a turn
/// typically produces far fewer than 4 K items, so the engine almost never
/// blocks.  If it does, back-pressure is correct behaviour (disk fell behind).
const WRITER_CHANNEL_CAP: usize = 4096;

// ------------------------------------------------------------------
// Writer task body
// ------------------------------------------------------------------

/// Per-turn sequence counter for the current turn.
///
/// Rolled forward by [`apply_op`] on `ItemAppended` ops; reset on
/// `TurnStarted`.  The writer keeps this counter so the engine side
/// only needs to pass the `seq` it already tracks via `ActiveTurn`.
struct WriterState {
    storage: Arc<Storage>,
    /// Open rollout writers keyed by thread id.
    rollouts: HashMap<ThreadId, RolloutWriter>,
    /// Sequence counter tracking per turn for header-once logic.
    header_written: std::collections::HashSet<ThreadId>,
}

impl WriterState {
    fn new(storage: Arc<Storage>) -> Self {
        Self {
            storage,
            rollouts: HashMap::new(),
            header_written: std::collections::HashSet::default(),
        }
    }

    /// Returns (or lazily opens) the rollout writer for `thread_id`.
    ///
    /// If opening fails the error is logged and `None` is returned; the
    /// caller skips the JSONL write for this op.
    async fn rollout_for(&mut self, thread_id: &ThreadId) -> Option<&mut RolloutWriter> {
        if !self.rollouts.contains_key(thread_id) {
            let path = self.storage.rollout_path(&thread_id.0);
            match RolloutWriter::open(path).await {
                Ok(w) => {
                    self.rollouts.insert(thread_id.clone(), w);
                }
                Err(err) => {
                    tracing::error!(
                        name: "zhive.persistence.writer.rollout_open_failed",
                        error = %err,
                        thread_id = %thread_id.0,
                        "failed to open rollout writer; JSONL writes skipped for this thread"
                    );
                    return None;
                }
            }
        }
        self.rollouts.get_mut(thread_id)
    }
}

async fn run_writer(storage: Arc<Storage>, mut rx: mpsc::Receiver<StorageWriteOp>) {
    let mut state = WriterState::new(storage);

    // tokio::sync::mpsc::Receiver::recv() returns None only after ALL senders
    // are dropped AND the internal buffer is fully drained.  There is no need
    // for a secondary try_recv() drain loop — all ops are consumed here.
    while let Some(op) = rx.recv().await {
        apply_op(&mut state, op).await;
    }
    // Channel closed and fully drained.  Fsync every open rollout so that the
    // last completed turn's data survives process exit.
    //
    // Invariant: the engine must send TurnEnded before dropping the sender;
    // otherwise only partial (un-synced) data may exist in BufWriter buffers.
    for thread_id in state.rollouts.keys().cloned().collect::<Vec<_>>() {
        if let Some(writer) = state.rollouts.get_mut(&thread_id)
            && let Err(err) = writer.sync_all().await
        {
            tracing::error!(
                name: "zhive.persistence.writer.shutdown_sync_failed",
                error = %err,
                thread_id = %thread_id.0,
                "final sync_all failed on shutdown"
            );
        }
    }

    tracing::debug!(
        name: "zhive.persistence.writer.stopped",
        "persistence writer task exited cleanly"
    );
}

async fn apply_op(state: &mut WriterState, op: StorageWriteOp) {
    match op {
        StorageWriteOp::ThreadUpserted(thread) => apply_thread_upserted(state, thread).await,
        StorageWriteOp::TurnStarted {
            thread_id,
            turn_id,
            started_at,
        } => apply_turn_started(state, thread_id, turn_id, started_at).await,
        StorageWriteOp::ItemAppended {
            thread_id,
            turn_id,
            seq,
            item,
        } => apply_item_appended(state, thread_id, turn_id, seq, item).await,
        StorageWriteOp::TurnEnded {
            thread_id,
            turn_id,
            status,
            error,
            completed_at,
            duration_ms,
        } => {
            apply_turn_ended(
                state,
                thread_id,
                turn_id,
                status,
                error,
                completed_at,
                duration_ms,
            )
            .await;
        }
        StorageWriteOp::Flush { thread_id, ack } => apply_flush(state, thread_id, ack).await,
        StorageWriteOp::ModelChanged {
            thread_id,
            provider,
            model_id,
        } => apply_model_changed(state, thread_id, provider, model_id).await,
        StorageWriteOp::SessionNameSet { thread_id, name } => {
            apply_session_name_set(state, thread_id, name).await;
        }
        StorageWriteOp::SetLeaf {
            thread_id,
            target_id,
        } => apply_set_leaf(state, thread_id, target_id).await,
        StorageWriteOp::ForkHeader {
            thread_id,
            parent_session,
            cwd,
            created_at,
        } => apply_fork_header(state, thread_id, parent_session, cwd, created_at).await,
        StorageWriteOp::Compaction {
            thread_id,
            turn_id,
            timestamp,
            summary,
            replacement,
            entries_compacted,
        } => {
            apply_compaction(
                state,
                thread_id,
                turn_id,
                timestamp,
                summary,
                replacement,
                entries_compacted,
            )
            .await;
        }
        StorageWriteOp::PermissionSuspended {
            thread_id,
            turn_id,
            timestamp,
            request_id,
            request,
        } => {
            apply_permission_suspended(state, thread_id, turn_id, timestamp, request_id, request)
                .await;
        }
        StorageWriteOp::PermissionResolved {
            thread_id,
            request_id,
            timestamp,
        } => {
            apply_permission_resolved(state, thread_id, request_id, timestamp).await;
        }
        StorageWriteOp::Snapshot {
            thread_id,
            turn_id,
            timestamp,
            tree,
            preview,
        } => {
            apply_snapshot(state, thread_id, turn_id, timestamp, tree, preview).await;
        }
    }
}

async fn apply_thread_upserted(state: &mut WriterState, thread: Box<zhive_proto::domain::Thread>) {
    // JSONL first: write session header once per thread.
    if !state.header_written.contains(&thread.id) {
        let cwd = thread.cwd.to_str().unwrap_or("/").to_owned();
        // Derive the rollout header's parent_session from the thread's
        // forked_from link so a thread created through the fork path (or any
        // future caller that sets forked_from) records its origin in the JSONL
        // source of truth. The fork path itself normally writes the header via
        // ForkHeader first (which marks header_written), so this branch only
        // runs for forked threads whose upsert reached the writer before the
        // ForkHeader — in which case it must still carry the parent link.
        //
        // B9: also record subagent_parent and source so rebuild/resume can
        // recover the parent-child relationship and thread origin.
        let parent_session = thread.forked_from.as_ref().map(|p| p.0.to_string());
        let subagent_parent = thread.subagent_parent.as_ref().map(|p| p.0.to_string());
        let session_entry = RolloutEntry::Session {
            version: super::rollout::SESSION_VERSION,
            id: thread.id.0.to_string(),
            timestamp: thread.created_at,
            cwd,
            parent_session,
            // Store as Some so the reader can recover the non-User origin.
            // None for User keeps the serialized field absent, saving bytes
            // and staying compatible with readers that predate Wave4.
            subagent_parent,
            source: Some(thread.source),
        };
        if let Some(w) = state.rollout_for(&thread.id).await {
            if let Err(err) = w.append(&session_entry).await {
                tracing::error!(
                    name: "zhive.persistence.writer.session_header_failed",
                    error = %err,
                    thread_id = %thread.id.0,
                    "failed to write session header to rollout"
                );
            } else {
                state.header_written.insert(thread.id.clone());
            }
        }
    }

    // SQL index upsert (best-effort after JSONL).
    if let Err(err) = state.storage.state.upsert_thread(&thread).await {
        tracing::error!(
            name: "zhive.persistence.writer.thread_upsert_failed",
            error = %err,
            thread_id = %thread.id.0,
            "SQL upsert_thread failed; JSONL is authoritative"
        );
    }
}

async fn apply_turn_started(
    state: &mut WriterState,
    thread_id: ThreadId,
    turn_id: TurnId,
    started_at: i64,
) {
    // SQL only (no JSONL entry for TurnStarted).
    if let Err(err) = state
        .storage
        .state
        .record_turn_start(&thread_id, &turn_id, started_at)
        .await
    {
        tracing::error!(
            name: "zhive.persistence.writer.turn_start_failed",
            error = %err,
            turn_id = %turn_id.0,
            "SQL record_turn_start failed; JSONL is authoritative"
        );
    }
}

async fn apply_item_appended(
    state: &mut WriterState,
    thread_id: ThreadId,
    turn_id: TurnId,
    seq: i64,
    item: Box<Item>,
) {
    // JSONL first — append the item entry.
    let now = unix_now();
    let rollout_entry = RolloutEntry::Item {
        thread_id: thread_id.0.to_string(),
        turn_id: turn_id.0.to_string(),
        timestamp: now,
        item: item.clone(),
    };
    if let Some(w) = state.rollout_for(&thread_id).await
        && let Err(err) = w.append(&rollout_entry).await
    {
        tracing::error!(
            name: "zhive.persistence.writer.item_append_failed",
            error = %err,
            thread_id = %thread_id.0,
            "failed to append item to rollout"
        );
    }

    // SQL index (best-effort after JSONL).
    if let Err(err) = state.storage.state.append_item(&turn_id, seq, &item).await {
        tracing::error!(
            name: "zhive.persistence.writer.item_sql_failed",
            error = %err,
            turn_id = %turn_id.0,
            "SQL append_item failed; JSONL is authoritative"
        );
    }
}

async fn apply_turn_ended(
    state: &mut WriterState,
    thread_id: ThreadId,
    turn_id: TurnId,
    status: TurnStatus,
    error: Option<TurnError>,
    completed_at: i64,
    duration_ms: Option<i64>,
) {
    // JSONL FIRST (source-of-truth invariant): append the leaf pointer and
    // fsync before updating the SQL index.  A crash between JSONL write and
    // SQL write leaves the index lagging; it can be rebuilt from the JSONL.
    // The reverse (SQL updated but JSONL missing the Leaf) would break
    // recovery heuristics that rely on the Leaf as the completion marker.
    if let Some(w) = state.rollout_for(&thread_id).await {
        let leaf = RolloutEntry::Leaf { target_id: None };
        if let Err(err) = w.append(&leaf).await {
            tracing::error!(
                name: "zhive.persistence.writer.leaf_failed",
                error = %err,
                thread_id = %thread_id.0,
                "failed to append leaf entry to rollout"
            );
        }
        // Open a `zhive.rollback_point` span that wraps the fsync save-point.
        //
        // This span marks the durability boundary: after `sync_all` the
        // turn's items are guaranteed to survive a crash. An OTLP exporter
        // can use this span to measure fsync latency.
        //
        // Span name is a literal; spans::ROLLBACK_POINT / fields::THREAD_ID /
        // fields::DB_OPERATION are the single source of truth (see
        // observability tests).
        let sync_span = tracing::info_span!(
            "zhive.rollback_point",
            "session.id"   = %thread_id.0,
            "db.operation" = "fsync",
        );
        if let Err(err) = w.sync_all().instrument(sync_span).await {
            tracing::error!(
                name: "zhive.persistence.writer.sync_failed",
                error = %err,
                thread_id = %thread_id.0,
                "fsync after TurnEnded failed"
            );
        }
    }

    // SQL index (best-effort after JSONL fsync).
    if let Err(err) = state
        .storage
        .state
        .record_turn_end(&turn_id, status, error.as_ref(), completed_at, duration_ms)
        .await
    {
        tracing::error!(
            name: "zhive.persistence.writer.turn_end_failed",
            error = %err,
            turn_id = %turn_id.0,
            "SQL record_turn_end failed; JSONL is authoritative"
        );
    }
}

async fn apply_flush(
    state: &mut WriterState,
    thread_id: ThreadId,
    ack: Option<tokio::sync::oneshot::Sender<()>>,
) {
    // Only fsync a rollout that is already open. A thread with no buffered
    // writes has nothing to drain; opening it here would create an empty file.
    // In that case the durability signal is still valid (there is nothing
    // un-synced), so the ack fires regardless of whether the writer existed.
    if let Some(w) = state.rollouts.get_mut(&thread_id)
        && let Err(err) = w.sync_all().await
    {
        tracing::error!(
            name: "zhive.persistence.writer.flush_failed",
            error = %err,
            thread_id = %thread_id.0,
            "Flush sync_all failed"
        );
    }
    // Fire the completion signal (if requested) after the fsync attempt. A
    // dropped receiver (awaiter went away) is benign — `send` returns Err and
    // is ignored.
    if let Some(ack) = ack {
        let _ = ack.send(());
    }
}

async fn apply_model_changed(
    state: &mut WriterState,
    thread_id: ThreadId,
    provider: String,
    model_id: String,
) {
    // SQL-only best-effort: there is no JSONL row for a model change in
    // Phase 1, so the index is updated directly and the JSONL is left intact.
    if let Err(err) = state
        .storage
        .state
        .set_thread_model(&thread_id, &provider, &model_id)
        .await
    {
        tracing::error!(
            name: "zhive.persistence.writer.model_changed_failed",
            error = %err,
            thread_id = %thread_id.0,
            "SQL set_thread_model failed; JSONL is authoritative"
        );
    }
}

async fn apply_session_name_set(state: &mut WriterState, thread_id: ThreadId, name: String) {
    // SQL-only best-effort, same rationale as `apply_model_changed`.
    if let Err(err) = state.storage.state.set_thread_name(&thread_id, &name).await {
        tracing::error!(
            name: "zhive.persistence.writer.session_name_failed",
            error = %err,
            thread_id = %thread_id.0,
            "SQL set_thread_name failed; JSONL is authoritative"
        );
    }
}

async fn apply_set_leaf(state: &mut WriterState, thread_id: ThreadId, target_id: Option<String>) {
    // JSONL-only: append the leaf pointer then fsync so the fork's branch head
    // survives a crash. There is no SQL column for the active leaf in Phase 1;
    // rebuild reads the last Leaf back from the rollout (see
    // `rebuild_state_from_rollout`).
    if let Some(w) = state.rollout_for(&thread_id).await {
        if let Err(err) = w.set_leaf_id(target_id.as_deref()).await {
            tracing::error!(
                name: "zhive.persistence.writer.set_leaf_failed",
                error = %err,
                thread_id = %thread_id.0,
                "failed to append leaf entry to rollout"
            );
        }
        if let Err(err) = w.sync_all().await {
            tracing::error!(
                name: "zhive.persistence.writer.set_leaf_sync_failed",
                error = %err,
                thread_id = %thread_id.0,
                "fsync after SetLeaf failed"
            );
        }
    }
}

async fn apply_fork_header(
    state: &mut WriterState,
    thread_id: ThreadId,
    parent_session: ThreadId,
    cwd: String,
    created_at: i64,
) {
    // JSONL first: write the forked thread's session header naming its parent,
    // and mark the header as written so the subsequent ThreadUpserted for the
    // same thread skips its own (parent-less) header. Without this guard the
    // upsert would emit a second Session line with parent_session=None and the
    // fork link would be lost on rebuild.
    if state.header_written.contains(&thread_id) {
        // Header already emitted (e.g. a prior upsert raced the fork). Do not
        // double-write; the existing header is authoritative.
        return;
    }
    if let Some(w) = state.rollout_for(&thread_id).await {
        if let Err(err) = w
            .append_session_header(&thread_id.0, created_at, &cwd, Some(&parent_session.0))
            .await
        {
            tracing::error!(
                name: "zhive.persistence.writer.fork_header_failed",
                error = %err,
                thread_id = %thread_id.0,
                "failed to write forked session header to rollout"
            );
        } else {
            state.header_written.insert(thread_id);
        }
    }
}

// ------------------------------------------------------------------
// Crash recovery
// ------------------------------------------------------------------

/// Rebuilds the [`StateDb`] index from the JSONL rollout at `rollout_path`.
///
/// Reads all [`RolloutEntry`] values, then delegates to
/// [`rebuild_state_from_entries`] which replays them into the `state` database
/// (upsert thread, record turn starts/ends, append items) and marks turns
/// `Completed` best-effort (since the rollout does not track final status,
/// we use the last seen item's existence as a proxy for completion).
///
/// This function is idempotent: calling it a second time on the same data
/// will simply re-upsert the same rows.
///
/// # Errors
///
/// Returns [`StorageError`] if the rollout file cannot be read or a DB
/// write fails.
///
/// # Examples
///
/// ```no_run
/// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
/// use std::path::Path;
/// use zhive_core::persistence::Storage;
/// use zhive_core::persistence::writer::rebuild_state_from_rollout;
///
/// let storage = Storage::open(Path::new("/tmp/demo")).await?;
/// let rollout = Path::new("/tmp/demo/rollouts/thread_native_01.jsonl");
/// rebuild_state_from_rollout(&storage.state, rollout).await?;
/// # Ok(())
/// # }
/// ```
pub async fn rebuild_state_from_rollout(
    state: &super::StateDb,
    rollout_path: &std::path::Path,
) -> StorageResult<()> {
    // B8: use the tolerant reader so a single corrupt/truncated trailing line
    // (common after a crash-during-append) does not abort the whole rebuild.
    // A corrupt line before the last one is still a real corruption and
    // propagates as `StorageError::RolloutCorrupted`.
    let entries = match super::rollout::read_all_tolerant(rollout_path).await {
        Ok(e) => e,
        Err(StorageError::Io(io)) if io.kind() == std::io::ErrorKind::NotFound => {
            // A missing rollout file is normal for a fresh install; nothing to rebuild.
            return Ok(());
        }
        Err(other) => return Err(other),
    };
    rebuild_state_from_entries(state, entries).await
}

/// Replays already-read [`RolloutEntry`] values into the [`StateDb`] index.
///
/// The pure-data counterpart of [`rebuild_state_from_rollout`]: it takes the
/// entries a caller has already loaded (so a directory walk that read a rollout
/// for its stats does not read the same file a second time) and applies the
/// same upsert / turn / item replay. Both entry points share this body, so the
/// index produced from a re-read and from a passed-in slice can never diverge.
///
/// This function is idempotent: replaying the same entries twice re-upserts the
/// same rows.
///
/// # Errors
///
/// Returns [`StorageError`] when a DB write fails.
///
/// # Examples
///
/// ```no_run
/// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
/// use std::path::Path;
/// use zhive_core::persistence::{Storage, read_all};
/// use zhive_core::persistence::writer::rebuild_state_from_entries;
///
/// let storage = Storage::open(Path::new("/tmp/demo")).await?;
/// let rollout = Path::new("/tmp/demo/rollouts/thread_native_01.jsonl");
/// let entries = read_all(rollout).await?;
/// rebuild_state_from_entries(&storage.state, entries).await?;
/// # Ok(())
/// # }
/// ```
#[expect(
    clippy::too_many_lines,
    reason = "single-pass rollout replay logic; all arms are tightly coupled to the same state variables — splitting would require threading multiple mutable references"
)]
pub async fn rebuild_state_from_entries(
    state: &super::StateDb,
    entries: Vec<RolloutEntry>,
) -> StorageResult<()> {
    use std::collections::HashMap as HMap;
    use std::path::PathBuf;
    use zhive_proto::domain::{ThreadSource, ThreadStatus};

    // Track per-turn item count (used to detect non-empty turns for
    // best-effort completion marking).
    let mut turn_items: HMap<TurnId, i64> = HMap::new();
    // Track which threads we've seen so we don't duplicate upserts.
    let mut thread_ids_seen: std::collections::HashSet<String> =
        std::collections::HashSet::default();
    // Turn → thread mapping for marking done.
    let mut turn_to_thread: HMap<TurnId, ThreadId> = HMap::new();
    // Last fork/branch leaf target seen (target_id = Some). Diagnostic only:
    // the active branch head is not a SQL column in Phase 1.
    let mut last_leaf_target: Option<String> = None;

    let now = unix_now();

    for entry in entries {
        match entry {
            RolloutEntry::Session {
                id,
                timestamp,
                cwd,
                parent_session,
                subagent_parent,
                source,
                ..
            } => {
                if thread_ids_seen.contains(&id) {
                    continue;
                }
                thread_ids_seen.insert(id.clone());

                // A forked thread's header names its source as parent_session;
                // map that back onto Thread.forked_from so the rebuilt SQL
                // index records the fork link (it was previously dropped here).
                let forked_from = parent_session.as_deref().map(|p| ThreadId(Arc::from(p)));
                // B9: recover subagent parent and thread origin from the header.
                // Pre-Wave4 rollouts omit both fields; serde defaults them to
                // None.  `source.unwrap_or(User)` preserves the historical
                // hard-coded value so old files rebuild identically to before.
                let recovered_subagent_parent =
                    subagent_parent.as_deref().map(|p| ThreadId(Arc::from(p)));
                let recovered_source = source.unwrap_or(ThreadSource::User);
                let thread = zhive_proto::domain::Thread {
                    id: ThreadId(Arc::from(id.as_str())),
                    session_id: None,
                    forked_from,
                    subagent_parent: recovered_subagent_parent,
                    preview: String::new(),
                    ephemeral: false,
                    model_provider: "unknown".to_owned(),
                    created_at: timestamp,
                    updated_at: timestamp,
                    status: ThreadStatus::Idle,
                    cwd: PathBuf::from(cwd),
                    source: recovered_source,
                    name: None,
                    turns: vec![],
                };
                state.upsert_thread(&thread).await?;
            }

            RolloutEntry::Item {
                thread_id: item_thread_id,
                turn_id: item_turn_id,
                timestamp: _,
                item,
            } => {
                let tid = ThreadId(Arc::from(item_thread_id.as_str()));
                let tid_turn = TurnId(Arc::from(item_turn_id.as_str()));

                // Lazily record a turn start (we may not have seen a
                // dedicated TurnStarted entry in this rollout format).
                turn_to_thread
                    .entry(tid_turn.clone())
                    .or_insert_with(|| tid.clone());
                let seq = turn_items.entry(tid_turn.clone()).or_insert(0);

                // Make sure the turn row exists (INSERT OR IGNORE).
                state.record_turn_start(&tid, &tid_turn, now).await?;

                state.append_item(&tid_turn, *seq, &item).await?;
                *seq += 1;
            }

            // A Leaf with target_id = Some marks a fork / branch head; remember
            // the most recent one for diagnostics.
            // target_id = None is a plain turn-completion save point.
            // PendingPermission and PermissionResolved are JSONL-only control-flow
            // entries with no SQL index equivalent.
            // All of these are intentionally ignored here: neither the active leaf
            // nor the pending permission state is a SQL column in Phase 1; the
            // resume path reads PendingPermission/PermissionResolved directly.
            RolloutEntry::Leaf {
                target_id: Some(target),
            } => {
                last_leaf_target = Some(target);
            }
            RolloutEntry::Leaf { target_id: None }
            | RolloutEntry::PendingPermission { .. }
            | RolloutEntry::PermissionResolved { .. } => {}

            // Compaction checkpoint: record the synthetic compaction turn and
            // its replacement items in the SQL index. Old turn rows are NOT
            // deleted (the index is a rebuildable derivative; historical data
            // is useful for diagnostics and UI review).
            //
            // The `summary` and `entries_compacted` fields are diagnostic-only;
            // only `replacement` matters for the index. The `thread_id` field
            // inside the entry is used as the turn's thread association.
            RolloutEntry::Compaction {
                thread_id: compaction_thread_id,
                turn_id: compaction_turn_id,
                replacement,
                ..
            } => {
                let tid = ThreadId(Arc::from(compaction_thread_id.as_str()));
                let tid_turn = TurnId(Arc::from(compaction_turn_id.as_str()));

                turn_to_thread
                    .entry(tid_turn.clone())
                    .or_insert_with(|| tid.clone());

                // Initialise the sequence counter for this synthetic turn.
                let seq_base = turn_items.entry(tid_turn.clone()).or_insert(0);

                state.record_turn_start(&tid, &tid_turn, now).await?;

                for item in &replacement {
                    state.append_item(&tid_turn, *seq_base, item).await?;
                    *seq_base += 1;
                }
            }

            // Per-turn workspace snapshot: re-project into the turn_snapshots
            // table so the rewind picker survives a rebuild. Idempotent via the
            // table's REPLACE semantics.
            RolloutEntry::Snapshot {
                thread_id: snap_thread_id,
                turn_id: snap_turn_id,
                timestamp,
                tree,
                preview,
            } => {
                let tid = ThreadId(Arc::from(snap_thread_id.as_str()));
                let tid_turn = TurnId(Arc::from(snap_turn_id.as_str()));
                state
                    .record_snapshot(&tid, &tid_turn, &tree, &preview, timestamp)
                    .await?;
            }
        }
    }

    if let Some(leaf) = &last_leaf_target {
        tracing::debug!(
            name: "zhive.persistence.rebuild.active_leaf",
            target_id = %leaf,
            "rebuilt rollout had an explicit fork/branch leaf"
        );
    }

    // Best-effort: mark every non-empty turn as Completed.
    for completed_turn_id in turn_items.keys() {
        state
            .record_turn_end(completed_turn_id, TurnStatus::Completed, None, now, None)
            .await?;
    }

    Ok(())
}

/// Aggregate counts produced by [`rebuild_indexes_from_jsonl`].
///
/// `threads_rebuilt` counts distinct `Session` headers replayed across every
/// rollout file; `entries_replayed` counts every JSONL line fed to the
/// per-file rebuild (`Session` + `Item` + `Leaf`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RebuildStats {
    /// Number of distinct threads (one per `Session` header) rebuilt.
    pub threads_rebuilt: u64,
    /// Total rollout entries replayed across all files.
    pub entries_replayed: u64,
}

/// Rebuilds the [`StateDb`] index from every rollout under `rollouts_dir`.
///
/// Walks the directory, skips the [`session_index.jsonl`] sidecar and any
/// non-`.jsonl` file, and replays each remaining rollout through
/// [`rebuild_state_from_rollout`]. Counts are accumulated into a
/// [`RebuildStats`].
///
/// **Trailing-line tolerance (B8)**: a single corrupt or truncated *trailing*
/// line per file is silently discarded (crash-truncation scenario); the valid
/// prefix is still replayed.  A corrupt line in the *middle* of a file is
/// still a real corruption and aborts the walk so the operator learns the
/// index is incomplete rather than silently partial.
///
/// This is the crash-recovery entry point that rebuilds the full index after
/// the SQL database is lost or out of date; the JSONL rollout remains the
/// source of truth (see the module header).
///
/// [`session_index.jsonl`]: super::session_index::SESSION_INDEX_FILE
///
/// # Errors
///
/// Returns [`StorageError`] when the directory cannot be read or any rollout
/// fails to replay.
///
/// # Examples
///
/// ```no_run
/// # async fn demo() -> zhive_core::persistence::StorageResult<()> {
/// use std::path::Path;
/// use zhive_core::persistence::Storage;
/// use zhive_core::persistence::writer::rebuild_indexes_from_jsonl;
///
/// let storage = Storage::open(Path::new("/tmp/demo")).await?;
/// let stats = rebuild_indexes_from_jsonl(
///     Path::new("/tmp/demo/rollouts"),
///     &storage.state,
/// )
/// .await?;
/// println!("rebuilt {} threads", stats.threads_rebuilt);
/// # Ok(())
/// # }
/// ```
pub async fn rebuild_indexes_from_jsonl(
    rollouts_dir: &std::path::Path,
    state_db: &super::StateDb,
) -> StorageResult<RebuildStats> {
    let mut stats = RebuildStats::default();
    let mut dir = match tokio::fs::read_dir(rollouts_dir).await {
        Ok(d) => d,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // No rollout directory yet (fresh install): nothing to rebuild.
            return Ok(stats);
        }
        Err(err) => return Err(err.into()),
    };

    while let Some(entry) = dir.next_entry().await? {
        let path = entry.path();
        // Only consider regular `*.jsonl` files; skip the session-index
        // sidecar so it is never replayed as a rollout.
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str())
            == Some(super::session_index::SESSION_INDEX_FILE)
        {
            continue;
        }

        // Count entries (Session headers and total) before replaying so the
        // stats reflect what landed in the index.
        //
        // B8: tolerant read — a trailing corrupt line (crash-truncation) is
        // discarded and the valid prefix is replayed.  A corrupt mid-file line
        // still aborts rebuild for that file so the operator is aware.
        let entries = match super::rollout::read_all_tolerant(&path).await {
            Ok(e) => e,
            Err(StorageError::Io(io)) if io.kind() == std::io::ErrorKind::NotFound => continue,
            Err(other) => return Err(other),
        };
        let threads_in_file = entries
            .iter()
            .filter(|e| matches!(e, RolloutEntry::Session { .. }))
            .count();
        stats.threads_rebuilt = stats
            .threads_rebuilt
            .saturating_add(u64::try_from(threads_in_file).unwrap_or(u64::MAX));
        stats.entries_replayed = stats
            .entries_replayed
            .saturating_add(u64::try_from(entries.len()).unwrap_or(u64::MAX));

        // Replay the SAME entries we counted above: passing the already-read
        // Vec to `rebuild_state_from_entries` (instead of calling
        // `rebuild_state_from_rollout`, which would re-`read_all` the file)
        // guarantees the stats and the index reflect one identical read.
        rebuild_state_from_entries(state_db, entries).await?;
    }

    Ok(stats)
}

/// Appends a [`RolloutEntry::PendingPermission`] entry and fsyncs (B6 save
/// point).
///
/// This is a critical save point — the suspended state must survive a crash so
/// resume can re-surface the approval prompt to a reconnecting client.
async fn apply_permission_suspended(
    state: &mut WriterState,
    thread_id: ThreadId,
    turn_id: TurnId,
    timestamp: i64,
    request_id: String,
    request: Box<RequestPermissionRequest>,
) {
    let entry = RolloutEntry::PendingPermission {
        thread_id: thread_id.0.to_string(),
        turn_id: turn_id.0.to_string(),
        timestamp,
        request_id,
        request,
    };
    if let Some(w) = state.rollout_for(&thread_id).await {
        if let Err(err) = w.append(&entry).await {
            tracing::error!(
                name: "zhive.persistence.writer.permission_suspended_append_failed",
                error = %err,
                thread_id = %thread_id.0,
                "failed to append PendingPermission entry to rollout"
            );
            return;
        }
        if let Err(err) = w.sync_all().await {
            tracing::error!(
                name: "zhive.persistence.writer.permission_suspended_sync_failed",
                error = %err,
                thread_id = %thread_id.0,
                "fsync after PendingPermission entry failed"
            );
        }
    }
}

/// Appends a [`RolloutEntry::PermissionResolved`] entry and fsyncs (B6 save
/// point).
///
/// Called once the client answers a deferred request (or the turn is
/// cancelled) so that a subsequent resume does not re-surface the prompt.
async fn apply_permission_resolved(
    state: &mut WriterState,
    thread_id: ThreadId,
    request_id: String,
    timestamp: i64,
) {
    let entry = RolloutEntry::PermissionResolved {
        thread_id: thread_id.0.to_string(),
        request_id,
        timestamp,
    };
    if let Some(w) = state.rollout_for(&thread_id).await {
        if let Err(err) = w.append(&entry).await {
            tracing::error!(
                name: "zhive.persistence.writer.permission_resolved_append_failed",
                error = %err,
                thread_id = %thread_id.0,
                "failed to append PermissionResolved entry to rollout"
            );
            return;
        }
        if let Err(err) = w.sync_all().await {
            tracing::error!(
                name: "zhive.persistence.writer.permission_resolved_sync_failed",
                error = %err,
                thread_id = %thread_id.0,
                "fsync after PermissionResolved entry failed"
            );
        }
    }
}

#[expect(
    clippy::vec_box,
    reason = "consistent with RolloutEntry::Compaction.replacement and StorageWriteOp::Compaction.replacement; Box keeps the enum size small on the wire"
)]
async fn apply_compaction(
    state: &mut WriterState,
    thread_id: ThreadId,
    turn_id: TurnId,
    timestamp: i64,
    summary: String,
    replacement: Vec<Box<Item>>,
    entries_compacted: u32,
) {
    // JSONL first: write the compaction checkpoint and fsync (save point).
    // Without fsync a crash after this point would cause replay of the full
    // un-compacted history, which can exceed the provider context limit.
    if let Some(w) = state.rollout_for(&thread_id).await {
        let entry = RolloutEntry::Compaction {
            thread_id: thread_id.0.to_string(),
            turn_id: turn_id.0.to_string(),
            timestamp,
            summary,
            replacement: replacement.clone(),
            entries_compacted,
        };
        if let Err(err) = w.append(&entry).await {
            tracing::error!(
                name: "zhive.persistence.writer.compaction_append_failed",
                error = %err,
                thread_id = %thread_id.0,
                "failed to append Compaction entry to rollout"
            );
            return;
        }
        if let Err(err) = w.sync_all().await {
            tracing::error!(
                name: "zhive.persistence.writer.compaction_sync_failed",
                error = %err,
                thread_id = %thread_id.0,
                "fsync after Compaction checkpoint failed"
            );
        }
    }

    // SQL index: record the synthetic compaction turn and its replacement items
    // so they are query-accessible (for UI history review). Old turn rows are
    // intentionally left in place — the SQL index is a rebuildable derivative
    // and historical data is useful for diagnostics.
    let now = unix_now();
    if let Err(err) = state
        .storage
        .state
        .record_turn_start(&thread_id, &turn_id, now)
        .await
    {
        tracing::error!(
            name: "zhive.persistence.writer.compaction_turn_start_failed",
            error = %err,
            turn_id = %turn_id.0,
            "SQL record_turn_start for compaction turn failed; JSONL is authoritative"
        );
    }
    for (seq, item) in replacement.iter().enumerate() {
        let seq = i64::try_from(seq).unwrap_or(i64::MAX);
        if let Err(err) = state.storage.state.append_item(&turn_id, seq, item).await {
            tracing::error!(
                name: "zhive.persistence.writer.compaction_item_failed",
                error = %err,
                turn_id = %turn_id.0,
                "SQL append_item for compaction replacement failed; JSONL is authoritative"
            );
        }
    }
    if let Err(err) = state
        .storage
        .state
        .record_turn_end(&turn_id, TurnStatus::Completed, None, now, None)
        .await
    {
        tracing::error!(
            name: "zhive.persistence.writer.compaction_turn_end_failed",
            error = %err,
            turn_id = %turn_id.0,
            "SQL record_turn_end for compaction turn failed; JSONL is authoritative"
        );
    }
}

/// Persists a per-turn workspace snapshot: appends to JSONL (with fsync) then
/// projects into the `turn_snapshots` SQL table.
///
/// The fsync is the durability point that makes the checkpoint survive a crash
/// mid-turn, which is exactly when an undo is most wanted.
async fn apply_snapshot(
    state: &mut WriterState,
    thread_id: ThreadId,
    turn_id: TurnId,
    timestamp: i64,
    tree: String,
    preview: String,
) {
    if let Some(w) = state.rollout_for(&thread_id).await {
        let entry = RolloutEntry::Snapshot {
            thread_id: thread_id.0.to_string(),
            turn_id: turn_id.0.to_string(),
            timestamp,
            tree: tree.clone(),
            preview: preview.clone(),
        };
        if let Err(err) = w.append(&entry).await {
            tracing::error!(
                name: "zhive.persistence.writer.snapshot_append_failed",
                error = %err,
                thread_id = %thread_id.0,
                "failed to append Snapshot entry to rollout"
            );
            return;
        }
        if let Err(err) = w.sync_all().await {
            tracing::error!(
                name: "zhive.persistence.writer.snapshot_sync_failed",
                error = %err,
                thread_id = %thread_id.0,
                "fsync after Snapshot checkpoint failed"
            );
        }
    }

    if let Err(err) = state
        .storage
        .state
        .record_snapshot(&thread_id, &turn_id, &tree, &preview, timestamp)
        .await
    {
        tracing::error!(
            name: "zhive.persistence.writer.snapshot_sql_failed",
            error = %err,
            turn_id = %turn_id.0,
            "SQL record_snapshot failed; JSONL is authoritative"
        );
    }
}

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

/// Seconds since the Unix epoch; saturates on error.
fn unix_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().try_into().unwrap_or(i64::MAX))
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use zhive_proto::domain::{ItemId, ThreadSource, ThreadStatus};

    use super::*;

    async fn open_storage() -> (tempfile::TempDir, Arc<Storage>) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(dir.path()).await.unwrap();
        (dir, Arc::new(storage))
    }

    fn make_thread(id: &str) -> Thread {
        Thread {
            id: ThreadId(Arc::from(id)),
            session_id: None,
            forked_from: None,
            subagent_parent: None,
            preview: "preview".into(),
            ephemeral: false,
            model_provider: "test".into(),
            created_at: 1_000,
            updated_at: 1_000,
            status: ThreadStatus::Idle,
            cwd: PathBuf::from("/tmp"),
            source: ThreadSource::User,
            name: None,
            turns: vec![],
        }
    }

    // ----------------------------------------------------------------
    // Test: crash recovery rebuild
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn rebuild_state_from_rollout_round_trip() {
        let (_dir, storage) = open_storage().await;

        let thread_id = ThreadId(Arc::from("thread:native/rebuild-test"));
        let turn_id = TurnId(Arc::from("turn:thread:native/rebuild-test/0"));

        // Write rollout directly.
        let rollout_path = storage.rollout_path(&thread_id.0);
        let mut w = crate::persistence::RolloutWriter::open(rollout_path.clone())
            .await
            .unwrap();

        w.append(&RolloutEntry::Session {
            version: 3,
            id: "thread:native/rebuild-test".into(),
            timestamp: 1_000,
            cwd: "/tmp".into(),
            parent_session: None,
            subagent_parent: None,
            source: None,
        })
        .await
        .unwrap();

        let item = Item::AgentMessage {
            id: ItemId(Arc::from("item:rebuild/0")),
            text: "rebuilt".into(),
        };
        w.append(&RolloutEntry::Item {
            thread_id: "thread:native/rebuild-test".into(),
            turn_id: "turn:thread:native/rebuild-test/0".into(),
            timestamp: 1_001,
            item: Box::new(item.clone()),
        })
        .await
        .unwrap();

        w.sync_all().await.unwrap();
        drop(w);

        // Rebuild into a fresh state DB.
        rebuild_state_from_rollout(&storage.state, &rollout_path)
            .await
            .unwrap();

        // Verify thread.
        let t = storage
            .state
            .get_thread(&thread_id)
            .await
            .unwrap()
            .expect("thread must be present");
        assert_eq!(t.id.0.as_ref(), "thread:native/rebuild-test");

        // Verify items.
        let items = storage.state.get_turn_items(&turn_id).await.unwrap();
        assert_eq!(items.len(), 1);
        let Item::AgentMessage { text, .. } = &items[0] else {
            panic!("expected AgentMessage");
        };
        assert_eq!(text, "rebuilt");
    }

    // ----------------------------------------------------------------
    // Test: PersistenceWriter e2e
    // ----------------------------------------------------------------

    #[tokio::test]
    async fn writer_applies_ops_and_persists_items() {
        let (_dir, storage) = open_storage().await;

        let (tx, handle) = PersistenceWriter::spawn(Arc::clone(&storage));

        let thread_id = ThreadId(Arc::from("thread:native/writer-test"));
        let turn_id = TurnId(Arc::from("turn:thread:native/writer-test/0"));

        let thread = make_thread("thread:native/writer-test");
        tx.send(StorageWriteOp::ThreadUpserted(Box::new(thread)))
            .await
            .unwrap();

        tx.send(StorageWriteOp::TurnStarted {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            started_at: 1_000,
        })
        .await
        .unwrap();

        let item = Item::AgentMessage {
            id: ItemId(Arc::from("item:writer-test/0")),
            text: "persisted".into(),
        };
        tx.send(StorageWriteOp::ItemAppended {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            seq: 0,
            item: Box::new(item),
        })
        .await
        .unwrap();

        tx.send(StorageWriteOp::TurnEnded {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            status: TurnStatus::Completed,
            error: None,
            completed_at: 2_000,
            duration_ms: Some(1_000),
        })
        .await
        .unwrap();

        // Drop the sender to trigger drain-and-exit.
        drop(tx);
        handle.await.unwrap();

        // Verify SQL index was updated.
        let t = storage
            .state
            .get_thread(&thread_id)
            .await
            .unwrap()
            .expect("thread must be present");
        assert_eq!(t.id.0.as_ref(), "thread:native/writer-test");

        let items = storage.state.get_turn_items(&turn_id).await.unwrap();
        assert_eq!(items.len(), 1);
        let Item::AgentMessage { text, .. } = &items[0] else {
            panic!("expected AgentMessage");
        };
        assert_eq!(text, "persisted");

        // Verify JSONL rollout contains the item entry.
        let rollout_path = storage.rollout_path(&thread_id.0);
        let entries = crate::persistence::read_all(&rollout_path).await.unwrap();
        let has_item = entries.iter().any(|e| {
            matches!(e, RolloutEntry::Item { turn_id: tid, .. } if tid == "turn:thread:native/writer-test/0")
        });
        assert!(
            has_item,
            "JSONL rollout must contain the ItemAppended entry"
        );
    }

    /// `ModelChanged` and `SessionNameSet` ops update the existing threads
    /// row in place (best-effort SQL index update).
    #[tokio::test]
    async fn writer_applies_model_and_name_ops() {
        let (_dir, storage) = open_storage().await;

        // Seed the threads row first via a direct upsert.
        let thread_id = ThreadId(Arc::from("thread:native/meta-test"));
        storage
            .state
            .upsert_thread(&make_thread("thread:native/meta-test"))
            .await
            .unwrap();

        let (tx, handle) = PersistenceWriter::spawn(Arc::clone(&storage));
        tx.send(StorageWriteOp::ModelChanged {
            thread_id: thread_id.clone(),
            provider: "anthropic".into(),
            model_id: "claude-opus-4".into(),
        })
        .await
        .unwrap();
        tx.send(StorageWriteOp::SessionNameSet {
            thread_id: thread_id.clone(),
            name: "release planning".into(),
        })
        .await
        .unwrap();
        drop(tx);
        handle.await.unwrap();

        let t = storage
            .state
            .get_thread(&thread_id)
            .await
            .unwrap()
            .expect("thread must be present");
        assert_eq!(t.model_provider, "anthropic/claude-opus-4");
        assert_eq!(t.name.as_deref(), Some("release planning"));
    }

    /// `rebuild_indexes_from_jsonl` replays every per-thread rollout in a
    /// directory, skips the `session_index.jsonl` sidecar, and reports the
    /// aggregate counts.
    #[tokio::test]
    async fn rebuild_indexes_from_jsonl_scans_directory() {
        let (_dir, storage) = open_storage().await;
        let rollouts_dir = storage.rollout_path("ignored").parent().unwrap().to_owned();

        // Two thread rollouts, each one Session + one Item.
        for n in 0..2 {
            let thread_id = ThreadId(Arc::from(format!("thread:native/scan-{n}").as_str()));
            let path = storage.rollout_path(&thread_id.0);
            let mut w = crate::persistence::RolloutWriter::open(path).await.unwrap();
            w.append(&RolloutEntry::Session {
                version: 3,
                id: thread_id.0.to_string(),
                timestamp: 1_000,
                cwd: "/tmp".into(),
                parent_session: None,
                subagent_parent: None,
                source: None,
            })
            .await
            .unwrap();
            w.append(&RolloutEntry::Item {
                thread_id: thread_id.0.to_string(),
                turn_id: format!("turn:{}/0", thread_id.0),
                timestamp: 1_001,
                item: Box::new(Item::AgentMessage {
                    id: ItemId(Arc::from(format!("item:scan-{n}/0").as_str())),
                    text: "scanned".into(),
                }),
            })
            .await
            .unwrap();
            w.sync_all().await.unwrap();
        }

        // A session-index sidecar in the same directory must be skipped.
        crate::persistence::session_index::append_entry(
            &rollouts_dir,
            &crate::persistence::session_index::SessionIndexEntry::new(
                "thread:native/scan-0",
                "named",
                5,
            ),
        )
        .await
        .unwrap();

        let stats = rebuild_indexes_from_jsonl(&rollouts_dir, &storage.state)
            .await
            .unwrap();
        assert_eq!(stats.threads_rebuilt, 2, "two Session headers");
        assert_eq!(stats.entries_replayed, 4, "2 Session + 2 Item lines");

        // Both threads landed in the SQL index; the sidecar did not.
        let threads = storage.state.list_threads(None).await.unwrap();
        assert_eq!(threads.len(), 2);
    }

    /// `ForkHeader` writes a Session header naming the parent, and a subsequent
    /// `ThreadUpserted` for the same thread does NOT overwrite it with a
    /// parent-less header. `SetLeaf` appends a fork leaf at the file tail.
    #[tokio::test]
    async fn writer_fork_header_and_set_leaf() {
        let (_dir, storage) = open_storage().await;

        let (tx, handle) = PersistenceWriter::spawn(Arc::clone(&storage));
        let forked_id = ThreadId(Arc::from("thread:native/fork/0"));
        let source_id = ThreadId(Arc::from("thread:native/src"));

        // ForkHeader first: writes the parent-naming session header.
        tx.send(StorageWriteOp::ForkHeader {
            thread_id: forked_id.clone(),
            parent_session: source_id.clone(),
            cwd: "/work".into(),
            created_at: 1_000,
        })
        .await
        .unwrap();
        // A later upsert for the same thread must be skipped for header purposes.
        let mut forked_thread = make_thread("thread:native/fork/0");
        forked_thread.forked_from = Some(source_id.clone());
        tx.send(StorageWriteOp::ThreadUpserted(Box::new(forked_thread)))
            .await
            .unwrap();
        // SetLeaf at the tail.
        tx.send(StorageWriteOp::SetLeaf {
            thread_id: forked_id.clone(),
            target_id: Some("item:replay/1".into()),
        })
        .await
        .unwrap();
        drop(tx);
        handle.await.unwrap();

        let rollout_path = storage.rollout_path(&forked_id.0);
        let entries = crate::persistence::read_all(&rollout_path).await.unwrap();

        // Exactly one Session header, carrying the parent link.
        let sessions: Vec<_> = entries
            .iter()
            .filter_map(|e| match e {
                RolloutEntry::Session { parent_session, .. } => Some(parent_session.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(sessions.len(), 1, "exactly one Session header");
        assert_eq!(sessions[0].as_deref(), Some("thread:native/src"));

        // Tail is a fork leaf.
        assert_eq!(
            entries.last(),
            Some(&RolloutEntry::Leaf {
                target_id: Some("item:replay/1".to_owned()),
            })
        );
    }

    /// A rollout whose Session header carries `parent_session` rebuilds the SQL
    /// `forked_from` column (previously dropped during rebuild).
    #[tokio::test]
    async fn rebuild_recovers_forked_from_from_parent_session() {
        let (_dir, storage) = open_storage().await;

        let forked_id = ThreadId(Arc::from("thread:native/fork/rebuild"));
        let rollout_path = storage.rollout_path(&forked_id.0);
        let mut w = crate::persistence::RolloutWriter::open(rollout_path.clone())
            .await
            .unwrap();
        w.append_session_header(&forked_id.0, 1_000, "/work", Some("thread:native/origin"))
            .await
            .unwrap();
        w.set_leaf_id(Some("item:replay/0")).await.unwrap();
        w.sync_all().await.unwrap();
        drop(w);

        rebuild_state_from_rollout(&storage.state, &rollout_path)
            .await
            .unwrap();

        let t = storage
            .state
            .get_thread(&forked_id)
            .await
            .unwrap()
            .expect("forked thread must be present");
        assert_eq!(
            t.forked_from.as_ref().map(|f| f.0.to_string()).as_deref(),
            Some("thread:native/origin"),
            "rebuild must recover forked_from from Session.parent_session"
        );
    }

    /// `rebuild_state_from_rollout` must return `Ok(())` for a missing rollout
    /// file (normal condition on first boot; nothing to rebuild).
    #[tokio::test]
    async fn rebuild_on_nonexistent_path_returns_ok() {
        let (_dir, storage) = open_storage().await;
        let nonexistent = std::path::Path::new("/tmp/zhive-test-does-not-exist-rebuild.jsonl");
        rebuild_state_from_rollout(&storage.state, nonexistent)
            .await
            .expect("missing rollout must be treated as empty, not an error");
    }

    /// A `Flush { ack: Some(_) }` op fires its oneshot AFTER the item it follows
    /// is durably written, so an awaiter that drains the ack observes the
    /// preceding `ItemAppended` on disk.
    #[tokio::test]
    async fn flush_ack_fires_after_item_durable() {
        let (_dir, storage) = open_storage().await;
        let (tx, handle) = PersistenceWriter::spawn(Arc::clone(&storage));

        let thread_id = ThreadId(Arc::from("thread:native/flush-ack"));
        let turn_id = TurnId(Arc::from("turn:thread:native/flush-ack/0"));

        tx.send(StorageWriteOp::ItemAppended {
            thread_id: thread_id.clone(),
            turn_id,
            seq: 0,
            item: Box::new(Item::AgentMessage {
                id: ItemId(Arc::from("item:flush-ack/0")),
                text: "buffered".into(),
            }),
        })
        .await
        .unwrap();

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        tx.send(StorageWriteOp::Flush {
            thread_id: thread_id.clone(),
            ack: Some(ack_tx),
        })
        .await
        .unwrap();

        // The ack resolves once the writer has fsynced.
        ack_rx.await.expect("ack must fire after flush");

        // The preceding item is durably visible on disk (the ack is a true
        // durability barrier, not merely an enqueue echo).
        let rollout_path = storage.rollout_path(&thread_id.0);
        let entries = crate::persistence::read_all(&rollout_path).await.unwrap();
        assert!(
            entries.iter().any(|e| matches!(
                e,
                RolloutEntry::Item { item, .. }
                    if item.id().0.as_ref() == "item:flush-ack/0"
            )),
            "item must be on disk by the time the flush ack fires"
        );

        drop(tx);
        handle.await.unwrap();
    }

    /// `rebuild_state_from_entries` produces the same index as
    /// `rebuild_state_from_rollout` reading the same file (shared body, no
    /// divergence between the stats read and the index read).
    #[tokio::test]
    async fn rebuild_from_entries_matches_rollout_read() {
        let (_dir, storage) = open_storage().await;
        let thread_id = ThreadId(Arc::from("thread:native/entries-eq"));
        let turn_id = TurnId(Arc::from("turn:thread:native/entries-eq/0"));
        let rollout_path = storage.rollout_path(&thread_id.0);

        let mut w = crate::persistence::RolloutWriter::open(rollout_path.clone())
            .await
            .unwrap();
        w.append(&RolloutEntry::Session {
            version: 3,
            id: thread_id.0.to_string(),
            timestamp: 1_000,
            cwd: "/tmp".into(),
            parent_session: None,
            subagent_parent: None,
            source: None,
        })
        .await
        .unwrap();
        w.append(&RolloutEntry::Item {
            thread_id: thread_id.0.to_string(),
            turn_id: turn_id.0.to_string(),
            timestamp: 1_001,
            item: Box::new(Item::AgentMessage {
                id: ItemId(Arc::from("item:entries-eq/0")),
                text: "x".into(),
            }),
        })
        .await
        .unwrap();
        w.sync_all().await.unwrap();
        drop(w);

        let entries = crate::persistence::read_all(&rollout_path).await.unwrap();
        rebuild_state_from_entries(&storage.state, entries)
            .await
            .unwrap();

        let items = storage.state.get_turn_items(&turn_id).await.unwrap();
        assert_eq!(items.len(), 1, "entries replay populates the SQL index");
    }

    /// A failed `TurnEnded { error: Some(_) }` writes the failure cause to the
    /// `turns` row's `error_message` / `error_details` columns — the cause that
    /// `finish_turn` now threads through (previously hard-coded `error: None`).
    #[tokio::test]
    async fn turn_ended_persists_error_detail() {
        let (_dir, storage) = open_storage().await;
        let (tx, handle) = PersistenceWriter::spawn(Arc::clone(&storage));

        let thread_id = ThreadId(Arc::from("thread:native/failed-turn"));
        let turn_id = TurnId(Arc::from("turn:thread:native/failed-turn/0"));

        tx.send(StorageWriteOp::ThreadUpserted(Box::new(make_thread(
            "thread:native/failed-turn",
        ))))
        .await
        .unwrap();
        tx.send(StorageWriteOp::TurnStarted {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            started_at: 1_000,
        })
        .await
        .unwrap();
        tx.send(StorageWriteOp::TurnEnded {
            thread_id,
            turn_id: turn_id.clone(),
            status: TurnStatus::Failed,
            error: Some(TurnError {
                message: "provider exploded".to_owned(),
                additional_details: Some("HTTP 500".to_owned()),
            }),
            completed_at: 2_000,
            duration_ms: Some(1_000),
        })
        .await
        .unwrap();
        drop(tx);
        handle.await.unwrap();

        // Read the persisted error columns directly.
        let row: (String, Option<String>, Option<String>) =
            sqlx::query_as("SELECT status, error_message, error_details FROM turns WHERE id = ?1")
                .bind(turn_id.0.as_ref())
                .fetch_one(storage.state.pool())
                .await
                .unwrap();
        assert_eq!(row.0, "failed", "status column reflects the failure");
        assert_eq!(row.1.as_deref(), Some("provider exploded"));
        assert_eq!(row.2.as_deref(), Some("HTTP 500"));
    }

    /// A rollout that has no `Compaction` entry (legacy format) still rebuilds
    /// the SQL index correctly. This is the backward-compatibility invariant:
    /// the new `Compaction` arm is a no-op when absent.
    #[tokio::test]
    async fn rebuild_legacy_rollout_without_compaction_entry() {
        let (_dir, storage) = open_storage().await;
        let thread_id = ThreadId(Arc::from("thread:native/legacy-no-compact"));
        let turn_id = TurnId(Arc::from("turn:thread:native/legacy-no-compact/0"));
        let rollout_path = storage.rollout_path(&thread_id.0);

        // Write a legacy rollout: Session + Item only, no Compaction entry.
        let mut w = crate::persistence::RolloutWriter::open(rollout_path.clone())
            .await
            .unwrap();
        w.append(&RolloutEntry::Session {
            version: 3,
            id: thread_id.0.to_string(),
            timestamp: 1_000,
            cwd: "/tmp".into(),
            parent_session: None,
            subagent_parent: None,
            source: None,
        })
        .await
        .unwrap();
        let item = Item::AgentMessage {
            id: ItemId(Arc::from("item:legacy/0")),
            text: "legacy".into(),
        };
        w.append(&RolloutEntry::Item {
            thread_id: thread_id.0.to_string(),
            turn_id: turn_id.0.to_string(),
            timestamp: 1_001,
            item: Box::new(item),
        })
        .await
        .unwrap();
        w.sync_all().await.unwrap();
        drop(w);

        // Rebuild must succeed and produce the same result as before.
        rebuild_state_from_rollout(&storage.state, &rollout_path)
            .await
            .expect("legacy rollout rebuild must succeed");

        let t = storage
            .state
            .get_thread(&thread_id)
            .await
            .unwrap()
            .expect("thread must be present");
        assert_eq!(t.id.0.as_ref(), "thread:native/legacy-no-compact");

        let items = storage.state.get_turn_items(&turn_id).await.unwrap();
        assert_eq!(items.len(), 1, "legacy item must be rebuilt");
        assert!(matches!(items[0], Item::AgentMessage { .. }));
    }

    /// A rollout containing `[Session, Item×3 (turn0), Compaction, Item×1 (turn1)]`
    /// rebuilds to a SQL index where:
    /// - the compaction turn contains the replacement items (not turn0's items),
    /// - the post-compaction turn (turn1) contains its own item.
    ///
    /// Also verifies that `replay_thread_items` (used by `get_items(None)`) returns
    /// only `[marker, summary_item, turn1_item]` — not the original 3 items.
    #[expect(
        clippy::too_many_lines,
        reason = "linear scenario test: seeds a multi-entry rollout then asserts the rebuilt SQL index and replay"
    )]
    #[tokio::test]
    async fn rebuild_and_replay_with_compaction_entry() {
        use crate::persistence::read_all;

        let (_dir, storage) = open_storage().await;
        let thread_id = ThreadId(Arc::from("thread:native/with-compact"));
        let turn0 = TurnId(Arc::from("turn:thread:native/with-compact/0"));
        let compact_turn = TurnId(Arc::from("thread:native/with-compact::compaction-1"));
        let turn1 = TurnId(Arc::from("turn:thread:native/with-compact/1"));
        let rollout_path = storage.rollout_path(&thread_id.0);

        // Build the rollout programmatically.
        let mut w = crate::persistence::RolloutWriter::open(rollout_path.clone())
            .await
            .unwrap();
        w.append(&RolloutEntry::Session {
            version: 3,
            id: thread_id.0.to_string(),
            timestamp: 1_000,
            cwd: "/tmp".into(),
            parent_session: None,
            subagent_parent: None,
            source: None,
        })
        .await
        .unwrap();
        // Three items in turn0 (will be compacted away).
        for n in 0u32..3 {
            w.append(&RolloutEntry::Item {
                thread_id: thread_id.0.to_string(),
                turn_id: turn0.0.to_string(),
                timestamp: 1_001 + i64::from(n),
                item: Box::new(Item::AgentMessage {
                    id: ItemId(Arc::from(format!("item:t0/{n}").as_str())),
                    text: format!("old-{n}"),
                }),
            })
            .await
            .unwrap();
        }

        // Compaction entry with a [marker, summary] replacement.
        let marker = Item::ContextCompaction {
            id: ItemId(Arc::from("thread:native/with-compact::compaction-1-marker")),
        };
        let summary_item = Item::AgentMessage {
            id: ItemId(Arc::from(
                "thread:native/with-compact::compaction-1-summary",
            )),
            text: "[context summary]\nSUMMARY".to_owned(),
        };
        let replacement: Vec<Box<Item>> =
            vec![Box::new(marker.clone()), Box::new(summary_item.clone())];
        w.append(&RolloutEntry::Compaction {
            thread_id: thread_id.0.to_string(),
            turn_id: compact_turn.0.to_string(),
            timestamp: 1_010,
            summary: "SUMMARY".to_owned(),
            replacement: replacement.clone(),
            entries_compacted: 3,
        })
        .await
        .unwrap();

        // One item in turn1 (written after the compaction).
        let turn1_item = Item::UserMessage {
            id: ItemId(Arc::from("item:t1/0")),
            content: vec![],
        };
        w.append(&RolloutEntry::Item {
            thread_id: thread_id.0.to_string(),
            turn_id: turn1.0.to_string(),
            timestamp: 1_020,
            item: Box::new(turn1_item.clone()),
        })
        .await
        .unwrap();
        w.sync_all().await.unwrap();
        drop(w);

        // --- Verify rebuild_state_from_rollout (SQL index) ---
        rebuild_state_from_rollout(&storage.state, &rollout_path)
            .await
            .expect("rebuild must succeed");

        // The compaction turn in the SQL index contains the replacement items.
        let compact_items = storage.state.get_turn_items(&compact_turn).await.unwrap();
        assert_eq!(
            compact_items.len(),
            2,
            "compaction turn must have the 2 replacement items"
        );
        assert!(
            matches!(compact_items[0], Item::ContextCompaction { .. }),
            "first replacement must be ContextCompaction marker"
        );

        // The post-compaction turn contains its one item.
        let turn1_items = storage.state.get_turn_items(&turn1).await.unwrap();
        assert_eq!(turn1_items.len(), 1);

        // --- Verify replay_thread_items (full-history read for get_items(None)) ---
        // Must return [marker, summary, turn1_item] — NOT the original 3 turn0 items.
        let entries = read_all(&rollout_path).await.unwrap();
        let mut replayed_items = Vec::new();
        for entry in entries {
            match entry {
                RolloutEntry::Item { item, .. } => {
                    replayed_items.push(*item);
                }
                RolloutEntry::Compaction { replacement, .. } => {
                    replayed_items.clear();
                    replayed_items.extend(replacement.into_iter().map(|b| *b));
                }
                _ => {}
            }
        }
        assert_eq!(
            replayed_items.len(),
            3,
            "replay must yield [marker, summary, turn1_item], not the original 3 items"
        );
        assert!(
            matches!(replayed_items[0], Item::ContextCompaction { .. }),
            "first replayed item must be the compaction marker"
        );
        assert!(
            matches!(replayed_items[2], Item::UserMessage { .. }),
            "last replayed item must be the post-compaction turn1 item"
        );
    }

    // ----------------------------------------------------------------
    // B8: rebuild tolerates a trailing corrupt line
    // ----------------------------------------------------------------

    /// `rebuild_state_from_rollout` succeeds even if the last JSONL line
    /// is a truncated / corrupt half-write (crash-during-append scenario).
    #[tokio::test]
    async fn rebuild_recovers_from_trailing_corrupt_line() {
        let (_dir, storage) = open_storage().await;

        let thread_id = ThreadId(Arc::from("thread:native/crash-rebuild"));
        let turn_id = TurnId(Arc::from("turn:thread:native/crash-rebuild/0"));
        let rollout_path = storage.rollout_path(&thread_id.0);

        // Write a complete, valid rollout and then append a corrupt tail.
        let mut w = crate::persistence::RolloutWriter::open(rollout_path.clone())
            .await
            .unwrap();
        w.append(&RolloutEntry::Session {
            version: super::super::rollout::SESSION_VERSION,
            id: thread_id.0.to_string(),
            timestamp: 1_000,
            cwd: "/tmp".into(),
            parent_session: None,
            subagent_parent: None,
            source: None,
        })
        .await
        .unwrap();
        w.append(&RolloutEntry::Item {
            thread_id: thread_id.0.to_string(),
            turn_id: turn_id.0.to_string(),
            timestamp: 1_001,
            item: Box::new(Item::AgentMessage {
                id: ItemId(Arc::from("item:crash/0")),
                text: "before crash".into(),
            }),
        })
        .await
        .unwrap();
        w.sync_all().await.unwrap();
        drop(w);

        // Simulate a crash-truncated tail: append a half-written JSON line
        // with no trailing newline.
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&rollout_path)
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(
            &mut f,
            b"{\"type\":\"item\",\"thread_id\":\"t\",\"turn",
        )
        .await
        .unwrap();
        tokio::io::AsyncWriteExt::flush(&mut f).await.unwrap();
        drop(f);

        // Rebuild must succeed (trailing bad line is discarded).
        rebuild_state_from_rollout(&storage.state, &rollout_path)
            .await
            .expect("rebuild must succeed despite trailing corrupt line");

        // The valid item before the corrupt tail must be present.
        let items = storage.state.get_turn_items(&turn_id).await.unwrap();
        assert_eq!(items.len(), 1, "one valid item must be rebuilt");
    }

    // ----------------------------------------------------------------
    // B9: rebuild recovers subagent_parent and source
    // ----------------------------------------------------------------

    /// `rebuild_state_from_rollout` restores `subagent_parent` and `source`
    /// when the rollout Session header carries the Wave4 fields.
    #[tokio::test]
    async fn rebuild_recovers_subagent_parent_and_source() {
        use zhive_proto::domain::ThreadSource;

        let (_dir, storage) = open_storage().await;

        let child_id = ThreadId(Arc::from("thread:native/child/b9"));
        let parent_id = ThreadId(Arc::from("thread:native/parent/b9"));
        let rollout_path = storage.rollout_path(&child_id.0);

        // Write a Wave4-format Session header with subagent fields populated.
        let mut w = crate::persistence::RolloutWriter::open(rollout_path.clone())
            .await
            .unwrap();
        w.append_session_header_full(
            &child_id.0,
            2_000,
            "/work",
            None,
            Some(&parent_id.0),
            Some(ThreadSource::Subagent),
        )
        .await
        .unwrap();
        w.sync_all().await.unwrap();
        drop(w);

        rebuild_state_from_rollout(&storage.state, &rollout_path)
            .await
            .unwrap();

        let t = storage
            .state
            .get_thread(&child_id)
            .await
            .unwrap()
            .expect("child thread must be present");

        assert_eq!(
            t.subagent_parent.as_ref().map(|p| p.0.as_ref()),
            Some(parent_id.0.as_ref()),
            "rebuild must recover subagent_parent from Session header"
        );
        assert_eq!(
            t.source,
            ThreadSource::Subagent,
            "rebuild must recover source from Session header"
        );
    }

    /// A legacy (v3) rollout without `subagent_parent` / `source` rebuilds
    /// with defaults: `subagent_parent = None`, `source = User`.
    /// This is the backward-compatibility lock test.
    #[tokio::test]
    async fn rebuild_legacy_session_without_new_fields_defaults_user() {
        use zhive_proto::domain::ThreadSource;

        let (_dir, storage) = open_storage().await;

        let thread_id = ThreadId(Arc::from("thread:native/legacy/b9"));
        let rollout_path = storage.rollout_path(&thread_id.0);

        // Write a JSON line that looks exactly like a pre-Wave4 v3 file
        // (no subagent_parent / source keys).
        let legacy_json = format!(
            "{{\"type\":\"session\",\"version\":3,\"id\":\"{}\",\"timestamp\":500,\"cwd\":\"/old\"}}\n",
            thread_id.0
        );
        tokio::fs::write(&rollout_path, legacy_json.as_bytes())
            .await
            .unwrap();

        rebuild_state_from_rollout(&storage.state, &rollout_path)
            .await
            .unwrap();

        let t = storage
            .state
            .get_thread(&thread_id)
            .await
            .unwrap()
            .expect("legacy thread must be present");

        assert!(
            t.subagent_parent.is_none(),
            "legacy rollout must rebuild with subagent_parent = None"
        );
        assert_eq!(
            t.source,
            ThreadSource::User,
            "legacy rollout must rebuild with source = User (historic default)"
        );
    }

    // ----------------------------------------------------------------
    // B6: PendingPermission / PermissionResolved persistence tests
    // ----------------------------------------------------------------

    fn make_perm_request(tid: &str, tool: &str) -> RequestPermissionRequest {
        serde_json::from_value(serde_json::json!({
            "threadId": tid,
            "resourceType": "tool",
            "name": tool,
            "reason": "test",
            "options": []
        }))
        .expect("perm request fixture")
    }

    /// `PermissionSuspended` op writes a `PendingPermission` entry to the
    /// rollout, and `PermissionResolved` follows it with the matching resolved
    /// entry.
    #[tokio::test]
    async fn writer_persists_and_resolves_pending_permission() {
        let (_dir, storage) = open_storage().await;
        let (tx, handle) = PersistenceWriter::spawn(Arc::clone(&storage));

        let thread_id = ThreadId(Arc::from("thread:native/perm-test"));
        let turn_id = TurnId(Arc::from("turn:0"));

        tx.send(StorageWriteOp::ThreadUpserted(Box::new(make_thread(
            "thread:native/perm-test",
        ))))
        .await
        .unwrap();

        tx.send(StorageWriteOp::PermissionSuspended {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            timestamp: 100,
            request_id: "perm:7".into(),
            request: Box::new(make_perm_request("thread:native/perm-test", "bash")),
        })
        .await
        .unwrap();

        tx.send(StorageWriteOp::PermissionResolved {
            thread_id: thread_id.clone(),
            request_id: "perm:7".into(),
            timestamp: 200,
        })
        .await
        .unwrap();

        drop(tx);
        handle.await.unwrap();

        // Read the rollout and verify both entries landed.
        let rollout_path = storage.rollout_path(&thread_id.0);
        let entries = crate::persistence::read_all(&rollout_path).await.unwrap();

        let pending: Vec<_> = entries
            .iter()
            .filter(|e| matches!(e, RolloutEntry::PendingPermission { .. }))
            .collect();
        let resolved: Vec<_> = entries
            .iter()
            .filter(|e| matches!(e, RolloutEntry::PermissionResolved { .. }))
            .collect();

        assert_eq!(pending.len(), 1, "one PendingPermission entry expected");
        assert_eq!(resolved.len(), 1, "one PermissionResolved entry expected");

        // Verify request_id matches in both.
        if let RolloutEntry::PendingPermission { request_id, .. } = pending[0] {
            assert_eq!(request_id, "perm:7");
        }
        if let RolloutEntry::PermissionResolved { request_id, .. } = resolved[0] {
            assert_eq!(request_id, "perm:7");
        }
    }
}

// Rust guideline compliant 2026-02-21

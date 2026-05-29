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
    Flush {
        /// Thread whose rollout should be fsynced.
        thread_id: ThreadId,
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
        StorageWriteOp::Flush { thread_id } => apply_flush(state, thread_id).await,
    }
}

async fn apply_thread_upserted(state: &mut WriterState, thread: Box<zhive_proto::domain::Thread>) {
    // JSONL first: write session header once per thread.
    if !state.header_written.contains(&thread.id) {
        let cwd = thread.cwd.to_str().unwrap_or("/").to_owned();
        let session_entry = RolloutEntry::Session {
            version: 3,
            id: thread.id.0.to_string(),
            timestamp: thread.created_at,
            cwd,
            parent_session: None,
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

async fn apply_flush(state: &mut WriterState, thread_id: ThreadId) {
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
}

// ------------------------------------------------------------------
// Crash recovery
// ------------------------------------------------------------------

/// Rebuilds the [`StateDb`] index from the JSONL rollout at `rollout_path`.
///
/// Reads all [`RolloutEntry`] values, replays them into the `state` database
/// (upsert thread, record turn starts/ends, append items), and marks turns
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
    use std::collections::HashMap as HMap;
    use std::path::PathBuf;
    use zhive_proto::domain::{ThreadSource, ThreadStatus};

    let entries = match super::rollout::read_all(rollout_path).await {
        Ok(e) => e,
        Err(StorageError::Io(io)) if io.kind() == std::io::ErrorKind::NotFound => {
            // A missing rollout file is normal for a fresh install; nothing to rebuild.
            return Ok(());
        }
        Err(other) => return Err(other),
    };

    // Track per-turn item count (used to detect non-empty turns for
    // best-effort completion marking).
    let mut turn_items: HMap<TurnId, i64> = HMap::new();
    // Track which threads we've seen so we don't duplicate upserts.
    let mut thread_ids_seen: std::collections::HashSet<String> =
        std::collections::HashSet::default();
    // Turn → thread mapping for marking done.
    let mut turn_to_thread: HMap<TurnId, ThreadId> = HMap::new();

    let now = unix_now();

    for entry in entries {
        match entry {
            RolloutEntry::Session {
                id, timestamp, cwd, ..
            } => {
                if thread_ids_seen.contains(&id) {
                    continue;
                }
                thread_ids_seen.insert(id.clone());

                let thread = zhive_proto::domain::Thread {
                    id: ThreadId(Arc::from(id.as_str())),
                    session_id: None,
                    forked_from: None,
                    preview: String::new(),
                    ephemeral: false,
                    model_provider: "unknown".to_owned(),
                    created_at: timestamp,
                    updated_at: timestamp,
                    status: ThreadStatus::Idle,
                    cwd: PathBuf::from(cwd),
                    source: ThreadSource::User,
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

            // Leaf and any future variants: no item to replay.
            _ => {}
        }
    }

    // Best-effort: mark every non-empty turn as Completed.
    for completed_turn_id in turn_items.keys() {
        state
            .record_turn_end(completed_turn_id, TurnStatus::Completed, None, now, None)
            .await?;
    }

    Ok(())
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
}

// Rust guideline compliant 2026-02-21

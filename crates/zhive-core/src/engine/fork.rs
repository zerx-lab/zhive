//! Cross-thread fork: branch a new thread off another thread's history.
//!
//! Forking reads a source thread's JSONL rollout (the source of truth, which
//! includes history beyond the in-memory window), allocates a brand-new thread
//! id, and replays the source items into the new thread up to an optional
//! boundary item. The new thread records its origin
//! ([`zhive_proto::domain::Thread::forked_from`] + a `parent_session` rollout
//! header) so it can be resumed and rebuilt on its own. This mirrors codex
//! `core/src/thread_manager.rs::fork_thread` (the "new thread" model), not Pi's
//! same-thread leaf-pointer move.
//!
//! Fork runs in the [`EnginePhase::BranchSummary`] phase: it claims the phase
//! with the same `Idle → BranchSummary → Idle` compare-and-set pattern as
//! [`super::compaction`], so it can never race a live turn or a compaction.
//! The work is bracketed by a `zhive.branch_summary` span (the first real
//! producer of that phase / span).
//!
//! ## Durability
//!
//! The new thread's rollout is written through the persistence writer in this
//! strict order: `ForkHeader` (the `Session{parent_session}` first line) →
//! `ThreadUpserted` → `TurnStarted` (opens the synthetic fork turn) →
//! N × `ItemAppended` (the replayed transcript) → `TurnEnded{Completed}`
//! (closes that turn) → `SetLeaf` (the branch head) → `Flush`. The header is
//! written before the items so the writer can open the file; the `TurnStarted`
//! precedes the items so the SQL `items` FK to `turns(id)` is satisfied on the
//! LIVE path; the leaf marks the branch head for crash recovery.
//!
//! Cross-thread fork requires persistent storage. An in-memory engine
//! (`storage = None`) cannot read a source rollout and returns
//! [`ForkError::SourceNotFound`].
//
// TODO(phase2): unify subagent spawn onto this fork path — a forked subagent is
// a child thread seeded with (a slice of) the parent's history, which is
// exactly what this module produces. Today `subagent_spawn` starts children
// with an empty transcript via `ThreadHandle::new_child`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use tracing::Instrument as _;
use zhive_proto::domain::{
    Item, ItemId, Thread, ThreadId, ThreadSource, ThreadStatus, TurnId, TurnStatus,
};
use zhive_proto::hook::EnginePhase;

use crate::persistence::writer::StorageWriteOp;
use crate::state::ThreadHandle;

use super::event::EngineEvent;
use super::inner::EngineInner;
use super::submission::{ForkError, ForkReply};

/// Prefix stamped on a generated branch-summary item so UI / event consumers
/// can tell a fork handoff apart from a normal agent message.
const BRANCH_SUMMARY_PREFIX: &str = "[branch summary]\n";

/// Synthetic turn id suffix grouping every replayed item under one turn on the
/// forked thread.
///
/// Replayed items keep their original [`ItemId`]s (so internal references such
/// as a `tool_call_id ↔ tool_result` pairing survive), but they are re-homed
/// into a single fork turn on the new thread so the forked rollout's turn index
/// is namespaced to the new thread rather than echoing the source's turn ids.
/// DELIBERATE: item ids are preserved; only the containing turn is reminted.
const FORK_TURN_SUFFIX: &str = "::forked";

/// How long the fork waits for the source rollout's flush `ack` before reading.
///
/// Large enough that a healthy writer always drains its buffer and fires the
/// ack first (a flush is a single `sync_all`), small enough that a wedged or
/// shutting-down writer — whose ack sender was dropped without sending — cannot
/// stall the fork. On timeout the fork falls back to reading the current
/// on-disk state, preserving the prior best-effort semantics.
const FORK_FLUSH_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl EngineInner {
    /// Forks a new thread from `source_thread_id`'s history.
    ///
    /// See the [module docs](self) for the full flow. Returns the new thread id
    /// plus the number of items replayed and whether a branch summary was
    /// generated.
    ///
    /// # Errors
    ///
    /// * [`ForkError::SourceNotFound`] — the source has no readable history
    ///   (unknown thread + no rollout, or storage is not configured).
    /// * [`ForkError::EngineBusy`] — engine phase was not `Idle`.
    /// * [`ForkError::ReplayFailed`] — reading the source rollout failed.
    /// * [`ForkError::SummarizationFailed`] — the optional summary call failed.
    pub(in crate::engine) async fn fork_thread(
        self: &Arc<Self>,
        source_thread_id: ThreadId,
        up_to_item: Option<ItemId>,
        summarize: bool,
    ) -> Result<ForkReply, ForkError> {
        // 1. Cross-thread fork reads the source rollout; without storage there
        //    is no source of truth to branch from.
        let storage = Arc::clone(self.storage().ok_or(ForkError::SourceNotFound)?);

        // 2. Claim the engine phase. Fork requires Idle and holds BranchSummary
        //    for its duration (mirrors compaction's Idle → Compaction CAS).
        if let Err(err) = self.try_set_phase_atomic(EnginePhase::Idle, EnginePhase::BranchSummary) {
            return Err(ForkError::EngineBusy {
                current: err.actual(),
            });
        }
        let _ = self.events_tx().send(EngineEvent::PhaseChanged {
            thread_id: Some(source_thread_id.clone()),
            from: EnginePhase::Idle,
            to: EnginePhase::BranchSummary,
        });

        // 3. Allocate the new thread id up front so the span names it.
        let new_thread_id = self.allocate_fork_thread_id(&source_thread_id);

        // 4. Arm a Drop guard that rolls the phase back to Idle. Holding the
        //    rollback in a guard (rather than a plain call after the await)
        //    means a panic inside `fork_inner` still unwinds the phase, so a
        //    panicking fork can never wedge the engine in `BranchSummary`
        //    forever. The guard fires on the normal return path too.
        let phase_guard = BranchSummaryGuard {
            inner: self,
            thread_id: &source_thread_id,
        };

        // 5. Run the fork body inside the `zhive.branch_summary` span. Use
        //    `.instrument()` (not `.enter()`) so the span attaches correctly
        //    across awaits on a multi-thread runtime.
        let span = tracing::info_span!(
            "zhive.branch_summary",
            "session.id"              = %new_thread_id.0,
            "zhive.parent.session.id" = %source_thread_id.0,
        );
        let result = self
            .fork_inner(
                &storage,
                source_thread_id.clone(),
                new_thread_id.clone(),
                up_to_item.clone(),
                summarize,
            )
            .instrument(span)
            .await;

        // 6. Roll the phase back to Idle now, on the success and error paths
        //    alike. Dropping the guard explicitly here (instead of letting it
        //    fall out of scope at function end) keeps the phase rollback before
        //    the `ThreadForked` broadcast below, preserving the original event
        //    ordering. On a panic the same guard fires during unwind.
        drop(phase_guard);

        let reply = result?;

        // 6. Broadcast the fork outcome for observers (UI / server bridge).
        let _ = self.events_tx().send(EngineEvent::ThreadForked {
            source_thread_id,
            new_thread_id,
            forked_from_item: up_to_item,
        });

        Ok(reply)
    }

    /// Body of [`Self::fork_thread`], run inside the `zhive.branch_summary`
    /// span. The caller owns the phase claim / rollback so every early return
    /// here still unwinds the phase.
    async fn fork_inner(
        self: &Arc<Self>,
        storage: &Arc<crate::persistence::Storage>,
        source_thread_id: ThreadId,
        new_thread_id: ThreadId,
        up_to_item: Option<ItemId>,
        summarize: bool,
    ) -> Result<ForkReply, ForkError> {
        // Flush the source thread's buffered rollout writes before reading its
        // history, so a fork taken right after a turn does not miss the most
        // recent items still sitting in the writer's BufWriter (the invariant
        // codex's `fork_flushes_parent_rollout_before_loading_history` verifies).
        //
        // The flush carries an `ack` oneshot the writer fires AFTER its
        // `sync_all`, so we can block until the source rollout is durably
        // drained before reading it — closing the race where the writer had not
        // yet flushed the most recent items when `replay_thread_items` ran. A
        // timeout bounds the wait so a closed / wedged writer (e.g. mid-shutdown,
        // where the ack sender is dropped) cannot hang the fork forever; on
        // timeout or a dropped ack we fall through and read whatever is on disk,
        // matching the previous best-effort behaviour.
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.enqueue_storage_op(StorageWriteOp::Flush {
            thread_id: source_thread_id.clone(),
            ack: Some(ack_tx),
        });
        if tokio::time::timeout(FORK_FLUSH_ACK_TIMEOUT, ack_rx)
            .await
            .is_err()
        {
            tracing::warn!(
                name: "zhive.engine.fork.flush_ack_timeout",
                thread_id = %source_thread_id.0,
                "fork source flush ack did not arrive within the timeout; reading current on-disk state"
            );
        }

        // Read the source history from the rollout (source of truth). This
        // returns the FULL history, not just the in-memory window — the ability
        // that makes cross-thread fork possible (compaction only sees the tail).
        let replayed = storage
            .replay_thread_items(&source_thread_id, up_to_item.as_ref())
            .await
            .map_err(|e| ForkError::ReplayFailed {
                message: e.to_string(),
            })?;

        // No rollout history AND no resident thread → nothing to fork from.
        if replayed.is_empty() && self.threads().get(&source_thread_id).await.is_none() {
            return Err(ForkError::SourceNotFound);
        }

        let items_replayed = u32::try_from(replayed.len()).unwrap_or(u32::MAX);

        // Optional branch summary: reuse the compaction provider path so we do
        // not build a parallel summariser.
        let summary_item = if summarize {
            match super::compaction::summarize(self.provider(), &replayed).await {
                Ok(text) => Some(Item::AgentMessage {
                    id: ItemId(Arc::from(format!(
                        "item:{}{FORK_TURN_SUFFIX}/summary",
                        new_thread_id.0
                    ))),
                    text: format!("{BRANCH_SUMMARY_PREFIX}{text}"),
                }),
                Err(e) => {
                    return Err(ForkError::SummarizationFailed {
                        message: e.to_string(),
                    });
                }
            }
        } else {
            None
        };

        // Register the new thread handle and seed its in-memory transcript.
        let fork_turn = TurnId(Arc::from(format!(
            "turn:{}{FORK_TURN_SUFFIX}",
            new_thread_id.0
        )));
        let handle = Arc::new(ThreadHandle::new_idle(new_thread_id.clone()));
        self.threads()
            .write_guard()
            .await
            .insert(new_thread_id.clone(), Arc::clone(&handle));
        // Seed an active turn buffer so `push_item` lands the replayed items.
        let now = super::lifecycle::unix_now_pub();
        handle.start_turn_buffer(fork_turn.clone(), now).await;
        // The summary item (if any) opens the new thread's context, followed by
        // the replayed history; both feed the in-memory tail window (which keeps
        // at most the most recent items per its own cap — older history stays in
        // the rollout and is lazily reloadable).
        if let Some(item) = &summary_item {
            handle.push_item(item.clone()).await;
        }
        for item in &replayed {
            handle.push_item(item.clone()).await;
        }
        // The fork turn is a synthetic seed, not a live turn: finalise the
        // buffer so the handle returns to Idle and is not mistaken for in-flight.
        handle
            .finish_turn_buffer(zhive_proto::domain::TurnStatus::Completed, now, Some(0))
            .await;

        // Persist the new thread durably (codex `spawn_thread` equivalent).
        self.persist_fork(
            &source_thread_id,
            &new_thread_id,
            &fork_turn,
            now,
            summary_item,
            replayed,
        );

        Ok(ForkReply::Forked {
            new_thread_id: handle.id.clone(),
            items_replayed,
            summarized: summarize,
        })
    }

    /// Enqueues the durable-write sequence for a freshly forked thread.
    ///
    /// Strict order so the rollout is self-contained and crash-safe:
    /// `ForkHeader` (parent-naming `Session` line) → `ThreadUpserted` (SQL row
    /// carrying `forked_from`) → `TurnStarted` (creates the SQL `turns` row the
    /// items reference) → N × `ItemAppended` (summary first, if any, then the
    /// replayed history) → `TurnEnded { Completed }` (closes the synthetic fork
    /// turn) → `SetLeaf` (branch head) → `Flush` (fsync save point).
    ///
    /// The `TurnStarted` / `TurnEnded` bracket is essential, not cosmetic: the
    /// SQL `items` table has a `turn_id REFERENCES turns(id)` foreign key, so
    /// appending items before the turn row exists fails the FK on the LIVE path
    /// and leaves the forked thread's SQL item index empty. Bracketing mirrors
    /// the `record_turn_start` / `record_turn_end` the rebuild path performs for
    /// every replayed turn.
    ///
    /// Every op is best-effort (`enqueue_storage_op` only logs on a full/closed
    /// channel) and never blocks the actor.
    fn persist_fork(
        &self,
        source_thread_id: &ThreadId,
        new_thread_id: &ThreadId,
        fork_turn: &TurnId,
        now: i64,
        summary_item: Option<Item>,
        replayed: Vec<Item>,
    ) {
        // A fork inherits the engine's working directory: the branch is a
        // continuation of the same project, so it must be listable under the
        // same `cwd` as its source thread.
        let cwd = self.cwd().to_string_lossy().into_owned();
        self.enqueue_storage_op(StorageWriteOp::ForkHeader {
            thread_id: new_thread_id.clone(),
            parent_session: source_thread_id.clone(),
            cwd,
            created_at: now,
        });
        let thread_snapshot = Thread {
            id: new_thread_id.clone(),
            session_id: None,
            forked_from: Some(source_thread_id.clone()),
            // A fork is a branch, not a subagent: no parent-child link.
            subagent_parent: None,
            preview: String::new(),
            ephemeral: false,
            model_provider: "unknown".to_owned(),
            created_at: now,
            updated_at: now,
            status: ThreadStatus::Idle,
            cwd: self.cwd().to_path_buf(),
            source: ThreadSource::User,
            name: None,
            turns: vec![],
        };
        self.enqueue_storage_op(StorageWriteOp::ThreadUpserted(Box::new(thread_snapshot)));

        // Open the synthetic fork turn BEFORE any item so the SQL `turns` row
        // exists when the items' FK is checked (see the FK note in the doc).
        self.enqueue_storage_op(StorageWriteOp::TurnStarted {
            thread_id: new_thread_id.clone(),
            turn_id: fork_turn.clone(),
            started_at: now,
        });

        // Append the summary (if any) then the replayed items, in order. The
        // enumeration index is the per-turn sequence number the writer records
        // in the SQL index.
        let mut last_item_id: Option<String> = None;
        for (seq, item) in summary_item.into_iter().chain(replayed).enumerate() {
            last_item_id = Some(item.id().0.to_string());
            self.enqueue_storage_op(StorageWriteOp::ItemAppended {
                thread_id: new_thread_id.clone(),
                turn_id: fork_turn.clone(),
                seq: i64::try_from(seq).unwrap_or(i64::MAX),
                item: Box::new(item),
            });
        }

        // Close the fork turn as Completed (it is a finished seed, never live),
        // mirroring the rebuild path's `record_turn_end`. `duration_ms = Some(0)`
        // because the seed turn has no wall-clock span. This also performs the
        // writer's TurnEnded fsync save point.
        self.enqueue_storage_op(StorageWriteOp::TurnEnded {
            thread_id: new_thread_id.clone(),
            turn_id: fork_turn.clone(),
            status: TurnStatus::Completed,
            error: None,
            completed_at: now,
            duration_ms: Some(0),
        });

        // Branch head leaf, then a flush save point so the whole fork is durable.
        self.enqueue_storage_op(StorageWriteOp::SetLeaf {
            thread_id: new_thread_id.clone(),
            target_id: last_item_id,
        });
        self.enqueue_storage_op(StorageWriteOp::Flush {
            thread_id: new_thread_id.clone(),
            ack: None,
        });
    }

    /// Restores `Idle` from `BranchSummary` and broadcasts the `PhaseChanged`.
    ///
    /// Mirrors [`super::compaction`]'s `leave_compaction`; a CAS failure here is
    /// a state-machine drift bug and is logged at `error`.
    fn leave_branch_summary(&self, thread_id: &ThreadId) {
        if let Err(err) = self.try_set_phase_atomic(EnginePhase::BranchSummary, EnginePhase::Idle) {
            tracing::error!(
                name: "zhive.engine.phase.branch_summary_rollback_failed",
                actual = ?err.actual(),
                "engine phase was not BranchSummary when finishing fork; state machine drift"
            );
        } else {
            let _ = self.events_tx().send(EngineEvent::PhaseChanged {
                thread_id: Some(thread_id.clone()),
                from: EnginePhase::BranchSummary,
                to: EnginePhase::Idle,
            });
        }
    }

    /// Allocates a fresh forked thread id: `thread:native/fork/<source-stem>/<n>`.
    ///
    /// The engine-wide turn counter supplies a monotonic `<n>` so repeated forks
    /// of the same source never collide, without pulling in a uuid dependency
    /// (matching the counter scheme used by `subagent_spawn`).
    fn allocate_fork_thread_id(&self, source: &ThreadId) -> ThreadId {
        let n = self.turn_counter().fetch_add(1, Ordering::Relaxed);
        let stem = source.0.strip_prefix("thread:").unwrap_or(&source.0);
        ThreadId(Arc::from(format!("thread:native/fork/{stem}/{n}")))
    }
}

/// Drop guard that rolls the engine phase back from `BranchSummary` to `Idle`.
///
/// `fork_thread` arms one of these right after claiming the phase, so the
/// rollback runs on the normal return path **and** on a panic unwind out of
/// `fork_inner`. Without it, a panicking fork would leave the engine wedged in
/// `BranchSummary` forever, rejecting every subsequent turn / compaction / fork.
/// The guard borrows the engine and thread id; both outlive the fork call.
struct BranchSummaryGuard<'a> {
    /// Engine whose phase is rolled back on drop.
    inner: &'a EngineInner,
    /// Thread id attributed to the emitted `PhaseChanged` event.
    thread_id: &'a ThreadId,
}

impl Drop for BranchSummaryGuard<'_> {
    fn drop(&mut self) {
        self.inner.leave_branch_summary(self.thread_id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::broadcast;
    use zhive_proto::domain::{Item, ItemId, ThreadId, TurnId};
    use zhive_proto::hook::EnginePhase;

    use crate::engine::event::EngineEvent;
    use crate::engine::inner::EngineInner;
    use crate::engine::submission::{ForkError, ForkReply};
    use crate::persistence::Storage;
    use crate::persistence::writer::PersistenceWriter;
    use crate::provider::{DynLanguageModel, ScriptedModel};

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    fn noop_provider() -> DynLanguageModel {
        ScriptedModel::new("noop", "noop", vec![]).into_dyn()
    }

    /// Builds an engine inner backed by a real on-disk `Storage`, returning the
    /// inner plus the temp dir (kept alive for the duration of the test) and an
    /// `Arc<Storage>` read handle for direct rollout inspection.
    async fn inner_with_storage() -> (Arc<EngineInner>, tempfile::TempDir, Arc<Storage>) {
        inner_with_storage_provider(noop_provider()).await
    }

    async fn inner_with_storage_provider(
        provider: DynLanguageModel,
    ) -> (Arc<EngineInner>, tempfile::TempDir, Arc<Storage>) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(dir.path()).await.unwrap());
        let (tx, handle) = PersistenceWriter::spawn(Arc::clone(&storage));
        let (events_tx, _) = broadcast::channel::<EngineEvent>(64);
        let inner = Arc::new(EngineInner::new_with_hooks_tools_storage(
            events_tx,
            provider,
            Arc::new(crate::hooks::HookHost::new()),
            Arc::new(crate::tools::ToolRegistry::new()),
            crate::engine::TurnLimits::default(),
            None,
            Some(tx),
            Some(handle),
            None,
            Some(Arc::clone(&storage)),
            std::path::PathBuf::from("/fork/cwd"),
        ));
        (inner, dir, storage)
    }

    /// Seeds a source thread rollout with a `Session` header + `n`
    /// `AgentMessage` items under one turn, returning the item ids in order.
    async fn seed_source(storage: &Storage, source: &ThreadId, n: usize) -> Vec<ItemId> {
        use crate::persistence::{RolloutEntry, RolloutWriter};
        let mut w = RolloutWriter::open(storage.rollout_path(&source.0))
            .await
            .unwrap();
        w.append(&RolloutEntry::Session {
            version: 3,
            id: source.0.to_string(),
            timestamp: 0,
            cwd: "/".into(),
            parent_session: None,
            subagent_parent: None,
            source: None,
        })
        .await
        .unwrap();
        let mut ids = Vec::new();
        for i in 0..n {
            let id = ItemId(Arc::from(format!("item:src/{i}").as_str()));
            ids.push(id.clone());
            w.append(&RolloutEntry::Item {
                thread_id: source.0.to_string(),
                turn_id: format!("turn:{}/0", source.0),
                timestamp: 0,
                item: Box::new(Item::AgentMessage {
                    id,
                    text: format!("m{i}"),
                }),
            })
            .await
            .unwrap();
        }
        w.sync_all().await.unwrap();
        ids
    }

    /// Fork (`up_to=None`) of a seeded source: new id differs, all items
    /// replayed, the new handle is registered, and the phase returns to `Idle`.
    #[tokio::test]
    async fn fork_replays_full_history_and_registers_thread() {
        let (inner, _dir, storage) = inner_with_storage().await;
        let source = tid("thread:native/fork-src");
        seed_source(&storage, &source, 3).await;

        let reply = inner
            .fork_thread(source.clone(), None, false)
            .await
            .expect("fork must succeed");
        let ForkReply::Forked {
            new_thread_id,
            items_replayed,
            summarized,
        } = reply;
        assert_ne!(new_thread_id, source, "fork must allocate a new id");
        assert_eq!(items_replayed, 3);
        assert!(!summarized);

        // New thread is registered with the replayed tail resident.
        let handle = inner
            .threads()
            .get(&new_thread_id)
            .await
            .expect("forked thread must be registered");
        assert_eq!(handle.item_count().await, 3);

        // Phase rolled back to Idle.
        assert_eq!(*inner.phase_lock(), EnginePhase::Idle);
    }

    /// Regression: after a fork the replayed items are present in the LIVE SQL
    /// index under the synthetic fork turn — proving `persist_fork` opens the
    /// `turns` row (via `TurnStarted`) BEFORE the items, so the
    /// `items.turn_id REFERENCES turns(id)` FK holds on the write path. Queries
    /// the live storage directly (NOT a rebuilt copy), the path that previously
    /// produced an empty item index because no `TurnStarted` preceded the items.
    #[tokio::test]
    async fn fork_items_land_in_live_sql_index_under_fork_turn() {
        use crate::persistence::writer::StorageWriteOp;

        let (inner, _dir, storage) = inner_with_storage().await;
        let source = tid("thread:native/fork-live-sql");
        seed_source(&storage, &source, 3).await;

        let ForkReply::Forked {
            new_thread_id,
            items_replayed,
            ..
        } = inner
            .fork_thread(source, None, false)
            .await
            .expect("fork must succeed");
        assert_eq!(items_replayed, 3);

        // Drain the writer deterministically: enqueue an ack'd Flush on the
        // forked thread and await it. Because the writer applies ops in order,
        // the ack only fires once every fork op (TurnStarted → items →
        // TurnEnded → SetLeaf → Flush) has been applied to the live storage.
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        inner.enqueue_storage_op(StorageWriteOp::Flush {
            thread_id: new_thread_id.clone(),
            ack: Some(ack_tx),
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), ack_rx)
            .await
            .expect("flush ack must arrive")
            .expect("flush ack channel must not drop");

        // The LIVE (not rebuilt) SQL item index has the replayed items under the
        // fork turn. Before the fix this was empty: the items' FK to a missing
        // turns row failed silently on the write path.
        let fork_turn = TurnId(Arc::from(format!("turn:{}::forked", new_thread_id.0)));
        let items = storage.state.get_turn_items(&fork_turn).await.unwrap();
        assert_eq!(
            items.len(),
            3,
            "LIVE storage must index all replayed items under the fork turn"
        );
    }

    /// Fork refuses when the engine is busy (phase not Idle).
    #[tokio::test]
    async fn fork_busy_when_not_idle() {
        let (inner, _dir, storage) = inner_with_storage().await;
        let source = tid("thread:native/fork-busy");
        seed_source(&storage, &source, 1).await;

        inner
            .try_set_phase_atomic(EnginePhase::Idle, EnginePhase::Turn)
            .expect("seed phase to Turn");

        let err = inner
            .fork_thread(source, None, false)
            .await
            .expect_err("fork must refuse when busy");
        assert!(matches!(err, ForkError::EngineBusy { .. }));
        // Phase untouched by the refused fork.
        assert_eq!(*inner.phase_lock(), EnginePhase::Turn);
    }

    /// The `BranchSummaryGuard` rolls the phase back to `Idle` even when the
    /// scope it guards unwinds via a panic — proving a panic inside
    /// `fork_inner` can never wedge the engine in `BranchSummary`.
    #[tokio::test]
    async fn branch_summary_guard_rolls_back_on_panic() {
        use super::BranchSummaryGuard;

        let (inner, _dir, _storage) = inner_with_storage().await;
        let thread_id = tid("thread:native/guard-panic");

        // Claim the phase exactly as `fork_thread` does.
        inner
            .try_set_phase_atomic(EnginePhase::Idle, EnginePhase::BranchSummary)
            .expect("seed phase to BranchSummary");

        // Run a scope that arms the guard and then panics. `catch_unwind`
        // contains the panic; the guard's Drop must still fire during unwind.
        // `AssertUnwindSafe` is sound here: we only inspect the phase afterward,
        // and a poisoned lock is read via `into_inner` by `phase_lock`.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = BranchSummaryGuard {
                inner: &inner,
                thread_id: &thread_id,
            };
            panic!("simulated fork_inner panic");
        }));
        assert!(result.is_err(), "the scope must have panicked");

        // The guard unwound the phase back to Idle despite the panic.
        assert_eq!(
            *inner.phase_lock(),
            EnginePhase::Idle,
            "BranchSummaryGuard must roll the phase back to Idle on panic unwind"
        );
    }

    /// Unknown source (no rollout, not resident) yields `SourceNotFound` and
    /// rolls the phase back to `Idle`.
    #[tokio::test]
    async fn fork_unknown_source_not_found() {
        let (inner, _dir, _storage) = inner_with_storage().await;
        let err = inner
            .fork_thread(tid("thread:native/no-such"), None, false)
            .await
            .expect_err("unknown source must fail");
        assert!(matches!(err, ForkError::SourceNotFound));
        assert_eq!(*inner.phase_lock(), EnginePhase::Idle);
    }

    /// Fork to an intermediate item (`up_to=Some`) truncates the replay
    /// inclusively.
    #[tokio::test]
    async fn fork_up_to_truncates_replay() {
        let (inner, _dir, storage) = inner_with_storage().await;
        let source = tid("thread:native/fork-trunc");
        let ids = seed_source(&storage, &source, 4).await;

        // up_to = the second item → inclusive truncation to 2 items.
        let reply = inner
            .fork_thread(source, Some(ids[1].clone()), false)
            .await
            .expect("fork must succeed");
        let ForkReply::Forked { items_replayed, .. } = reply;
        assert_eq!(
            items_replayed, 2,
            "inclusive truncation keeps items 0 and 1"
        );
    }

    /// `storage = None` (in-memory engine) makes cross-thread fork return
    /// `SourceNotFound` even if a thread with that id is resident.
    #[tokio::test]
    async fn fork_without_storage_is_source_not_found() {
        let (events_tx, _) = broadcast::channel::<EngineEvent>(16);
        let inner = Arc::new(EngineInner::new(events_tx, noop_provider()));
        // Even a resident thread cannot be forked without a rollout to read.
        let source = tid("thread:native/in-mem");
        let _ = inner.threads().get_or_init(&source).await;

        let err = inner
            .fork_thread(source, None, false)
            .await
            .expect_err("fork without storage must fail");
        assert!(matches!(err, ForkError::SourceNotFound));
        // Phase must be untouched (we never claimed it).
        assert_eq!(*inner.phase_lock(), EnginePhase::Idle);
    }

    /// A summarising fork prepends a branch-summary `AgentMessage` and reports
    /// `summarized = true`. Uses a scripted model so no network is hit.
    #[tokio::test]
    async fn fork_with_summary_prepends_branch_summary() {
        use llmsdk::language_model::StreamPart;

        let model = ScriptedModel::new(
            "t",
            "m",
            vec![
                StreamPart::TextStart {
                    id: "b".into(),
                    provider_metadata: None,
                },
                StreamPart::TextDelta {
                    id: "b".into(),
                    delta: "SUMMARY".into(),
                    provider_metadata: None,
                },
                StreamPart::TextEnd {
                    id: "b".into(),
                    provider_metadata: None,
                },
            ],
        )
        .into_dyn();
        let (inner, _dir, storage) = inner_with_storage_provider(model).await;
        let source = tid("thread:native/fork-sum");
        seed_source(&storage, &source, 2).await;

        let reply = inner
            .fork_thread(source, None, true)
            .await
            .expect("fork must succeed");
        let ForkReply::Forked {
            new_thread_id,
            items_replayed,
            summarized,
        } = reply;
        assert!(summarized);
        assert_eq!(items_replayed, 2);

        let handle = inner.threads().get(&new_thread_id).await.unwrap();
        let tail: Vec<Item> = handle.items_snapshot().await;
        // First resident item is the branch summary.
        match &tail[0] {
            Item::AgentMessage { text, .. } => {
                assert!(text.starts_with("[branch summary]\n"));
                assert!(text.contains("SUMMARY"));
            }
            other => panic!("expected branch summary AgentMessage first, got {other:?}"),
        }
        // Summary + 2 replayed items.
        assert_eq!(tail.len(), 3);
    }

    /// The forked thread's rollout is self-contained: first line is a Session
    /// with `parent_session = source`, the replayed items follow, and the file
    /// ends with a fork Leaf. Drains the writer (shutdown) before reading.
    #[tokio::test]
    async fn forked_rollout_is_self_contained_and_rebuildable() {
        use crate::persistence::{RolloutEntry, read_all, writer::rebuild_state_from_rollout};

        let (inner, _dir, storage) = inner_with_storage().await;
        let source = tid("thread:native/fork-rebuild-src");
        seed_source(&storage, &source, 2).await;

        let ForkReply::Forked { new_thread_id, .. } = inner
            .fork_thread(source.clone(), None, false)
            .await
            .expect("fork must succeed");

        // Shut the engine down so the persistence writer drains and fsyncs the
        // forked rollout before we read it. `run` consumes Shutdown and awaits
        // the writer handle; here we drive it directly via the inner channel by
        // dropping the storage writer (simulated through the public shutdown on
        // a wrapping Engine is not available, so we wait for the queued ops to
        // flush by forcing a drain via an explicit Flush already enqueued, then
        // give the writer task a moment).
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let path = storage.rollout_path(&new_thread_id.0);
        let entries = read_all(&path).await.expect("forked rollout must exist");
        // First line: Session naming the source as parent.
        match &entries[0] {
            RolloutEntry::Session { parent_session, .. } => {
                assert_eq!(parent_session.as_deref(), Some(source.0.as_ref()));
            }
            other => panic!("first line must be Session header, got {other:?}"),
        }
        // Replayed items present.
        let item_count = entries
            .iter()
            .filter(|e| matches!(e, RolloutEntry::Item { .. }))
            .count();
        assert_eq!(item_count, 2, "two items replayed into the forked rollout");
        // Ends with a fork Leaf (target_id = Some).
        assert!(
            matches!(
                entries.last(),
                Some(RolloutEntry::Leaf { target_id: Some(_) })
            ),
            "forked rollout must end with a fork leaf, got {:?}",
            entries.last()
        );

        // The forked rollout rebuilds independently, recovering forked_from.
        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh = Storage::open(fresh_dir.path()).await.unwrap();
        rebuild_state_from_rollout(&fresh.state, &path)
            .await
            .unwrap();
        let t = fresh
            .state
            .get_thread(&new_thread_id)
            .await
            .unwrap()
            .expect("rebuilt forked thread must be present");
        assert_eq!(
            t.forked_from.as_ref().map(|f| f.0.to_string()).as_deref(),
            Some(source.0.as_ref()),
            "rebuild must recover forked_from from the rollout"
        );

        let turn_id = TurnId(Arc::from(format!("turn:{}::forked", new_thread_id.0)));
        let items = fresh.state.get_turn_items(&turn_id).await.unwrap();
        assert_eq!(items.len(), 2, "rebuilt SQL index has the replayed items");
    }
}

// Rust guideline compliant 2026-06-03

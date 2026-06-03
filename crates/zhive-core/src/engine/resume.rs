//! Historical thread listing and resume.
//!
//! Two read-mostly operations that let a UI present a "recent sessions" list
//! and re-open one of them to keep talking:
//!
//! - [`EngineInner::list_threads`] returns the queryable thread index
//!   (most-recently-updated first), straight from the state-database
//!   projection.
//! - [`EngineInner::resume_thread`] reads a persisted thread's **full** history
//!   from its JSONL rollout (the source of truth, including history outside any
//!   prior in-memory window), seeds that history into a resident
//!   [`ThreadHandle`], and leaves the thread `Idle`. The seed is what makes
//!   resume meaningful: the next [`super::lifecycle`] `start_turn` builds its
//!   prompt from the handle's transcript (see [`super::prompt::build_call_options`]),
//!   so the model sees the prior conversation.
//!
//! ## Why read the rollout, not the in-memory window
//!
//! After a process restart the thread store is empty, so there is nothing in
//! memory to resume from. Even within one process, the in-memory transcript is
//! a bounded window (see [`crate::state::TurnHistoryBuffer`]) while the rollout
//! is complete. Reading the rollout therefore restores the whole conversation,
//! grouped back into its original turns.
//!
//! ## Phase
//!
//! Resume mutates the thread store, so it requires the engine to be `Idle`; a
//! resume submitted while a turn or compaction is in flight is refused with
//! [`ResumeError::EngineBusy`]. Both operations run on the engine actor task,
//! serialised with every other submission.

use std::collections::HashMap;
use std::sync::Arc;

use zhive_proto::domain::{Item, Thread, ThreadId, TurnId, TurnStatus};
use zhive_proto::hook::EnginePhase;
use zhive_proto::permission::RequestPermissionRequest;

use crate::permission::{RequestContext, pending::RequestKey};
use crate::persistence::RolloutEntry;
use crate::persistence::writer::StorageWriteOp;
use crate::state::ThreadHandle;

use super::inner::EngineInner;
use super::submission::{GetItemsError, ResumeError, ResumeReply};

/// How long resume waits for the thread's rollout flush `ack` before reading.
///
/// Mirrors `super::fork`'s flush handshake so a resume taken right after a turn
/// does not miss items still buffered in the writer. Large enough that a
/// healthy writer always drains first; small enough that a wedged or
/// shutting-down writer cannot stall resume. On timeout we read the current
/// on-disk state (best-effort), matching fork's semantics.
const RESUME_FLUSH_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A pending permission request recovered from the rollout during resume (B6).
///
/// Holds the decoded [`RequestKey`] (so the engine can re-register it without
/// re-issuing a new number) plus the turn context needed to emit `TurnResumed`
/// and the payload needed to re-emit `PermissionRequested` to a reconnecting
/// client.
struct RestoredPending {
    /// The numeric key that maps to the wire form `perm:<key>`.
    key: RequestKey,
    /// Turn context (thread + turn) for `TurnResumed` emission.
    context: RequestContext,
    /// The full permission request payload, re-emitted on resume.
    request: Box<RequestPermissionRequest>,
}

/// Rollout items grouped by turn plus any unresolved pending permissions (B6).
struct RestoredHistory {
    /// Items per turn, in file order.
    turns: Vec<(TurnId, Vec<Item>)>,
    /// Pending permission requests not yet superseded by a `PermissionResolved`.
    pending: Vec<RestoredPending>,
}

impl EngineInner {
    /// Returns persisted threads, most-recently-updated first.
    ///
    /// Reads the state-database projection ([`crate::persistence::StateDb::list_threads`]).
    /// When `cwd_filter` is `Some(path)`, only threads created under that
    /// working directory are returned (codex-style per-project listing);
    /// `None` returns every thread.
    ///
    /// When no persistent storage is configured the engine is purely in-memory
    /// and there is no index to read, so an **empty** list is returned (never an
    /// error) — an in-memory engine simply has no historical sessions. A
    /// database read error is logged and also collapses to an empty list so the
    /// listing call never fails the actor.
    ///
    /// The returned [`Thread`] values have an empty `turns` field (the index
    /// does not eager-load turn items); callers fetch items separately via
    /// [`Self::resume_thread`] or the `thread/get_items` server method.
    pub(in crate::engine) async fn list_threads(&self, cwd_filter: Option<&str>) -> Vec<Thread> {
        let Some(storage) = self.storage() else {
            return Vec::new();
        };
        match storage.state.list_threads(cwd_filter).await {
            Ok(threads) => threads,
            Err(err) => {
                tracing::warn!(
                    name: "zhive.engine.resume.list_threads_failed",
                    error = %err,
                    "listing persisted threads failed; returning an empty list"
                );
                Vec::new()
            }
        }
    }

    /// Resumes a persisted thread, making its full history resident in memory.
    ///
    /// Confirms the thread exists in the index, reads its complete rollout,
    /// registers a resident [`ThreadHandle`] (seeding the transcript turn by
    /// turn), and leaves the thread `Idle`. After resume, the next `start_turn`
    /// on this thread builds its prompt from the restored transcript, so the
    /// model continues the prior conversation.
    ///
    /// # Errors
    ///
    /// * [`ResumeError::StorageUnavailable`] — the engine has no storage backend.
    /// * [`ResumeError::ThreadNotFound`] — no row for `thread_id` in the index.
    /// * [`ResumeError::EngineBusy`] — engine phase was not `Idle`.
    /// * [`ResumeError::ReplayFailed`] — reading the rollout failed.
    #[expect(
        clippy::too_many_lines,
        reason = "resume_thread spans the flush + rollout-read + seed + B6-pending restore \
                  steps as one logical unit; splitting would require threading more \
                  references without a readability gain"
    )]
    pub(in crate::engine) async fn resume_thread(
        self: &Arc<Self>,
        thread_id: ThreadId,
    ) -> Result<ResumeReply, ResumeError> {
        // 1. Resume reads persisted history; without storage there is nothing
        //    to resume from.
        let storage = Arc::clone(self.storage().ok_or(ResumeError::StorageUnavailable)?);

        // 2. The thread must exist in the persistent index.
        let thread = storage
            .state
            .get_thread(&thread_id)
            .await
            .map_err(|e| ResumeError::ReplayFailed {
                message: e.to_string(),
            })?
            .ok_or(ResumeError::ThreadNotFound)?;

        // 3. Resume mutates the thread store; require Idle so it cannot race a
        //    live turn or compaction. Unlike fork it does not need a dedicated
        //    phase for its duration (the work is short and append-only), so we
        //    only assert the engine is currently Idle rather than claiming a
        //    phase across the await points below.
        {
            let guard = self.phase_lock();
            if *guard != EnginePhase::Idle {
                return Err(ResumeError::EngineBusy { current: *guard });
            }
        }

        // 4. Flush any buffered rollout writes for this thread before reading,
        //    so a resume taken right after a turn sees the latest items (the
        //    same handshake fork uses). A timeout bounds the wait; on timeout
        //    we read whatever is on disk.
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.enqueue_storage_op(StorageWriteOp::Flush {
            thread_id: thread_id.clone(),
            ack: Some(ack_tx),
        });
        if tokio::time::timeout(RESUME_FLUSH_ACK_TIMEOUT, ack_rx)
            .await
            .is_err()
        {
            tracing::warn!(
                name: "zhive.engine.resume.flush_ack_timeout",
                thread_id = %thread_id.0,
                "resume flush ack did not arrive within the timeout; reading current on-disk state"
            );
        }

        // 5. Read the full rollout, preserving turn grouping. `replay_thread_items`
        //    flattens away turn ids; reading the entries directly keeps the
        //    original per-turn structure so the restored transcript faithfully
        //    mirrors the persisted one (and `turns_restored` is accurate).
        //    Also recovers any unresolved pending permission requests (B6).
        let path = storage.rollout_path(&thread_id.0);
        let history = read_rollout_turns(&path).await?;

        let items_restored: u32 = history
            .turns
            .iter()
            .map(|(_, items)| u32::try_from(items.len()).unwrap_or(u32::MAX))
            .fold(0u32, u32::saturating_add);
        let turns_restored = u32::try_from(history.turns.len()).unwrap_or(u32::MAX);

        // 6. Register the resident handle and seed each restored turn into the
        //    in-memory transcript. `get_or_init` reuses an already-resident
        //    handle (idempotent re-resume) or creates a fresh idle one.
        let handle = self.threads().get_or_init(&thread_id).await;
        seed_history(&handle, history.turns).await;

        // 7. Re-register any unresolved pending permission requests so a
        //    reconnecting client can answer them via `session/resume_permission`
        //    (B6).  For each pending entry we:
        //      a) re-register the key in `PendingPermissions` (so the resolver can
        //         find it by wire id) and get a fresh receiver,
        //      b) re-emit `PermissionRequested` + `TurnSuspended` so the client sees
        //         the approval prompt again,
        //      c) spawn a lightweight "cleanup" task that awaits the outcome and
        //         writes `PermissionResolved` to persist the answer.
        //
        //    **Scope of B6**: this restores the *visibility and answerability* of the
        //    prompt; automatic turn-body replay after the permission is answered is
        //    out of scope (see `RolloutEntry::PendingPermission` doc comment).
        if !history.pending.is_empty() {
            let reducer = self.permission_reducer();
            let events_tx = self.events_tx().clone();
            let inner_arc = Arc::clone(self);

            for p in history.pending {
                // a) Re-register the key and obtain a fresh receiver.
                let rx = reducer.reinstate_for_resume(p.key, p.context.clone());

                // b) Re-emit PermissionRequested + TurnSuspended.
                let wire_id = p.key.to_wire();
                let _ = events_tx.send(super::event::EngineEvent::PermissionRequested {
                    request_id: wire_id.clone(),
                    request: p.request,
                });
                let _ = events_tx.send(super::event::EngineEvent::TurnSuspended {
                    thread_id: p.context.thread_id.clone(),
                    turn_id: p.context.turn_id.clone(),
                    request_id: wire_id.clone(),
                    reason: Some("resumed: awaiting prior deferred permission decision".into()),
                });

                // c) Spawn a cleanup task that awaits the outcome and persists
                //    `PermissionResolved`, plus optionally emits `TurnResumed`.
                //    This task intentionally does NOT re-run the turn body — that
                //    is a future enhancement (see B6 spec §4 scope note).
                let inner_for_task = Arc::clone(&inner_arc);
                let reducer_for_task = reducer.clone();
                let wire_id_str = wire_id.0.to_string();
                let p_thread_id = p.context.thread_id.clone();
                let p_turn_id = p.context.turn_id.clone();
                tokio::spawn(async move {
                    match reducer_for_task.wait_unbounded(rx).await {
                        Ok(_outcome) => {
                            // Persist the resolution so a second resume does not
                            // re-surface this prompt.
                            inner_for_task.enqueue_storage_op(StorageWriteOp::PermissionResolved {
                                thread_id: p_thread_id.clone(),
                                request_id: wire_id_str.clone(),
                                timestamp: super::lifecycle::unix_now_pub(),
                            });
                            // Emit TurnResumed so observers know the turn is no
                            // longer suspended (even though it won't auto-continue).
                            let _ = inner_for_task.events_tx().send(
                                super::event::EngineEvent::TurnResumed {
                                    thread_id: p_thread_id,
                                    turn_id: p_turn_id,
                                },
                            );
                        }
                        Err(err) => {
                            tracing::warn!(
                                name: "zhive.engine.resume.pending_perm_abandoned",
                                request_id = %wire_id_str,
                                error = %err,
                                "reinstated pending permission was abandoned without resolution"
                            );
                        }
                    }
                });
            }
        }

        // 7. Ensure the thread is Idle (a freshly created handle already is;
        //    this normalises a possibly stale status on a re-resume).
        *handle.status.write().await = zhive_proto::domain::ThreadStatus::Idle;

        tracing::debug!(
            name: "zhive.engine.resume.completed",
            thread_id = %thread_id.0,
            items_restored,
            turns_restored,
            preview = %thread.preview,
            "resumed thread history into memory"
        );

        Ok(ResumeReply {
            thread_id,
            items_restored,
            turns_restored,
        })
    }

    /// Reads persisted history items for rendering a resumed conversation.
    ///
    /// When `turn_id` is `Some`, returns that turn's items from the state-database
    /// index — paged via [`crate::persistence::StateDb::load_items_page`] when
    /// both `offset` and `limit` are provided, otherwise the whole turn via
    /// [`crate::persistence::StateDb::get_turn_items`]. When `turn_id` is `None`,
    /// returns the thread's **full** item history (read from the rollout, the
    /// source of truth) in conversation order.
    ///
    /// This is a pure read: it does not register a handle or change any state, so
    /// it does not require the engine to be `Idle`. A thread with no rollout / no
    /// indexed items yields an empty list.
    ///
    /// # Errors
    ///
    /// * [`GetItemsError::StorageUnavailable`] — the engine has no storage.
    /// * [`GetItemsError::ReadFailed`] — the index / rollout read failed.
    pub(in crate::engine) async fn get_items(
        &self,
        thread_id: ThreadId,
        turn_id: Option<TurnId>,
        offset: Option<i64>,
        limit: Option<i64>,
    ) -> Result<Vec<Item>, GetItemsError> {
        let storage = self.storage().ok_or(GetItemsError::StorageUnavailable)?;

        match turn_id {
            Some(turn) => {
                // Per-turn read: page only when both bounds are supplied so a
                // caller asking for "the whole turn" is not silently truncated.
                let result = match (offset, limit) {
                    (Some(off), Some(lim)) => storage.state.load_items_page(&turn, off, lim).await,
                    _ => storage.state.get_turn_items(&turn).await,
                };
                result.map_err(|e| GetItemsError::ReadFailed {
                    message: e.to_string(),
                })
            }
            None => {
                // Full-thread read from the rollout (source of truth). `offset` /
                // `limit` are turn-scoped knobs and are intentionally ignored for
                // a full-history read.
                storage
                    .replay_thread_items(&thread_id, None)
                    .await
                    .map_err(|e| GetItemsError::ReadFailed {
                        message: e.to_string(),
                    })
            }
        }
    }
}

/// Reads a rollout file and groups its items into turns, in file order.
///
/// Returns a [`RestoredHistory`] containing:
///
/// - one `(TurnId, Vec<Item>)` entry per distinct turn (ordered by first
///   appearance in the rollout), and
/// - any [`RestoredPending`] entries for permission requests that were written
///   as [`RolloutEntry::PendingPermission`] but never superseded by a matching
///   [`RolloutEntry::PermissionResolved`].
///
/// A missing rollout file yields an empty history (a thread whose rollout was
/// never written has no history to restore). `Session` / `Leaf` entries are
/// ignored. The `Compaction` entry discards all prior accumulated turns.
///
/// Pending permission recovery (B6): the map is keyed by `request_id` (the
/// wire string).  `PendingPermission` inserts; `PermissionResolved` removes.
/// `Compaction` does **not** clear the pending map — pending requests are
/// control-flow state independent of the item history.
async fn read_rollout_turns(path: &std::path::Path) -> Result<RestoredHistory, ResumeError> {
    // B8: use the tolerant reader so a crash-truncated trailing line does not
    // abort the whole resume.  A corrupt mid-file line still surfaces as
    // `ResumeError::ReplayFailed`.
    let entries = match crate::persistence::rollout::read_all_tolerant(path).await {
        Ok(e) => e,
        Err(crate::persistence::StorageError::Io(io))
            if io.kind() == std::io::ErrorKind::NotFound =>
        {
            return Ok(RestoredHistory {
                turns: Vec::new(),
                pending: Vec::new(),
            });
        }
        Err(other) => {
            return Err(ResumeError::ReplayFailed {
                message: other.to_string(),
            });
        }
    };

    // Preserve turn order by first appearance while accumulating items per turn.
    //
    // On encountering a `Compaction` entry, all previously accumulated turns
    // are discarded and replaced with the compaction's `replacement` slice.
    // This means that after resume, the in-memory transcript starts from the
    // most recent compaction point, not from the full un-compacted history.
    // Any turns written after the compaction entry are then appended normally.
    let mut order: Vec<TurnId> = Vec::new();
    let mut by_turn: HashMap<TurnId, Vec<Item>> = HashMap::new();

    // B6: collect pending permissions.  Keyed by wire request_id string.
    // PendingPermission inserts; PermissionResolved removes.
    // On decode failure of the numeric key we skip the entry with a warning
    // (best-effort: a malformed key in the rollout should not abort resume).
    let mut pending_map: HashMap<String, RestoredPending> = HashMap::new();

    for entry in entries {
        match entry {
            RolloutEntry::Item { turn_id, item, .. } => {
                let tid = TurnId(Arc::from(turn_id.as_str()));
                by_turn
                    .entry(tid.clone())
                    .or_insert_with(|| {
                        order.push(tid.clone());
                        Vec::new()
                    })
                    .push(*item);
            }
            RolloutEntry::Compaction {
                turn_id,
                replacement,
                ..
            } => {
                // Discard all prior turns and replace with the compaction
                // replacement transcript. Turns written after this entry in
                // the rollout will be appended normally via the `Item` arm.
                // The pending permission map is intentionally NOT cleared here:
                // a permission request is control-flow state, not item history.
                order.clear();
                by_turn.clear();
                let tid = TurnId(Arc::from(turn_id.as_str()));
                order.push(tid.clone());
                by_turn.insert(tid, replacement.into_iter().map(|b| *b).collect());
            }
            RolloutEntry::PendingPermission {
                thread_id,
                turn_id,
                request_id,
                request,
                ..
            } => {
                // Decode the numeric key from the wire form.
                use crate::engine::submission::PermissionRequestId;
                use crate::permission::pending::InvalidRequestId;
                let wire = PermissionRequestId(Arc::from(request_id.as_str()));
                match RequestKey::from_wire(&wire) {
                    Ok(key) => {
                        let context = RequestContext {
                            thread_id: ThreadId(Arc::from(thread_id.as_str())),
                            turn_id: TurnId(Arc::from(turn_id.as_str())),
                        };
                        pending_map.insert(
                            request_id,
                            RestoredPending {
                                key,
                                context,
                                request,
                            },
                        );
                    }
                    Err(InvalidRequestId(bad)) => {
                        tracing::warn!(
                            name: "zhive.engine.resume.pending_perm_bad_key",
                            request_id = %bad,
                            "PendingPermission entry has unparseable request_id; skipping"
                        );
                    }
                }
            }
            RolloutEntry::PermissionResolved { request_id, .. } => {
                // Remove the matching pending entry; the request was answered.
                pending_map.remove(&request_id);
            }
            _ => {} // Session / Leaf: ignored for history reconstruction.
        }
    }

    let turns = order
        .into_iter()
        .map(|tid| {
            let items = by_turn.remove(&tid).unwrap_or_default();
            (tid, items)
        })
        .collect();

    let pending = pending_map.into_values().collect();

    Ok(RestoredHistory { turns, pending })
}

/// Seeds restored turns into the handle's in-memory transcript.
///
/// Each turn is opened with [`ThreadHandle::start_turn_buffer`], its items are
/// pushed in order, and the turn is finalised as `Completed` so the handle ends
/// up `Idle` with a complete (non-active) history — exactly the shape the
/// prompt builder expects when the next live turn starts.
async fn seed_history(handle: &Arc<ThreadHandle>, turns: Vec<(TurnId, Vec<Item>)>) {
    // A synthetic timestamp: resumed turns are historical, so their exact
    // wall-clock boundaries are not reconstructed here (the rollout retains the
    // original timestamps; the in-memory buffer only needs ordering).
    let now = super::lifecycle::unix_now_pub();
    for (turn_id, items) in turns {
        handle.start_turn_buffer(turn_id, now).await;
        for item in items {
            handle.push_item(item).await;
        }
        // duration_ms = Some(0): a restored turn has no replayed wall-clock span.
        handle
            .finish_turn_buffer(TurnStatus::Completed, now, Some(0))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::broadcast;
    use zhive_proto::domain::{Item, ItemId, ThreadId};
    use zhive_proto::hook::EnginePhase;

    use crate::engine::event::EngineEvent;
    use crate::engine::inner::EngineInner;
    use crate::engine::submission::ResumeError;
    use crate::persistence::Storage;
    use crate::persistence::writer::PersistenceWriter;
    use crate::provider::{DynLanguageModel, ScriptedModel};

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    fn noop_provider() -> DynLanguageModel {
        ScriptedModel::new("noop", "noop", vec![]).into_dyn()
    }

    /// Builds an engine inner backed by a real on-disk `Storage`.
    async fn inner_with_storage() -> (Arc<EngineInner>, tempfile::TempDir, Arc<Storage>) {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(Storage::open(dir.path()).await.unwrap());
        let (tx, handle) = PersistenceWriter::spawn(Arc::clone(&storage));
        let (events_tx, _) = broadcast::channel::<EngineEvent>(64);
        let inner = Arc::new(EngineInner::new_with_hooks_tools_storage(
            events_tx,
            noop_provider(),
            Arc::new(crate::hooks::HookHost::new()),
            Arc::new(crate::tools::ToolRegistry::new()),
            crate::engine::TurnLimits::default(),
            None,
            Some(tx),
            Some(handle),
            None,
            Some(Arc::clone(&storage)),
            std::path::PathBuf::from("."),
        ));
        (inner, dir, storage)
    }

    /// Seeds a thread row + a two-turn rollout, returning nothing (the thread
    /// id is the caller's). Turn 0 has two items, turn 1 has one.
    async fn seed_two_turn_thread(storage: &Storage, thread: &ThreadId) {
        use std::path::PathBuf;
        use zhive_proto::domain::{Thread, ThreadSource, ThreadStatus};

        use crate::persistence::{RolloutEntry, RolloutWriter};

        // Index row (so get_thread finds it).
        storage
            .state
            .upsert_thread(&Thread {
                id: thread.clone(),
                session_id: None,
                forked_from: None,
                subagent_parent: None,
                preview: "resume me".into(),
                ephemeral: false,
                model_provider: "anthropic".into(),
                created_at: 1,
                updated_at: 2,
                status: ThreadStatus::Idle,
                cwd: PathBuf::from("/"),
                source: ThreadSource::User,
                name: None,
                turns: vec![],
            })
            .await
            .unwrap();

        // Rollout: Session + two turns of items.
        let mut w = RolloutWriter::open(storage.rollout_path(&thread.0))
            .await
            .unwrap();
        w.append(&RolloutEntry::Session {
            version: 3,
            id: thread.0.to_string(),
            timestamp: 0,
            cwd: "/".into(),
            parent_session: None,
            subagent_parent: None,
            source: None,
        })
        .await
        .unwrap();
        let turn0 = format!("turn:{}/0", thread.0);
        let turn1 = format!("turn:{}/1", thread.0);
        for (turn, n, text) in [
            (&turn0, "0", "first"),
            (&turn0, "1", "second"),
            (&turn1, "2", "third"),
        ] {
            w.append(&RolloutEntry::Item {
                thread_id: thread.0.to_string(),
                turn_id: turn.clone(),
                timestamp: 0,
                item: Box::new(Item::AgentMessage {
                    id: ItemId(Arc::from(format!("item:{n}").as_str())),
                    text: text.to_owned(),
                }),
            })
            .await
            .unwrap();
        }
        w.sync_all().await.unwrap();
    }

    /// `list_threads` returns the persisted index; an in-memory engine returns
    /// an empty list rather than erroring.
    #[tokio::test]
    async fn list_threads_reads_index_and_empty_without_storage() {
        let (inner, _dir, storage) = inner_with_storage().await;
        assert!(
            inner.list_threads(None).await.is_empty(),
            "fresh index is empty"
        );

        let thread = tid("thread:native/list-me");
        seed_two_turn_thread(&storage, &thread).await;
        let listed = inner.list_threads(None).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, thread);

        // In-memory engine: no storage → empty list, no error.
        let (events_tx, _) = broadcast::channel::<EngineEvent>(8);
        let mem = Arc::new(EngineInner::new(events_tx, noop_provider()));
        assert!(mem.list_threads(None).await.is_empty());
    }

    /// `resume_thread` restores the full rollout history, grouped by turn, and
    /// reports the item / turn counts.
    #[tokio::test]
    async fn resume_restores_full_history_grouped_by_turn() {
        let (inner, _dir, storage) = inner_with_storage().await;
        let thread = tid("thread:native/resume-me");
        seed_two_turn_thread(&storage, &thread).await;

        let reply = inner
            .resume_thread(thread.clone())
            .await
            .expect("resume must succeed");
        assert_eq!(reply.thread_id, thread);
        assert_eq!(reply.items_restored, 3, "all three items restored");
        assert_eq!(reply.turns_restored, 2, "two turns restored");

        // The thread is now resident with the items in conversation order.
        let handle = inner
            .threads()
            .get(&thread)
            .await
            .expect("resumed thread must be resident");
        assert_eq!(handle.item_count().await, 3);
        let texts: Vec<String> = handle
            .items_snapshot()
            .await
            .into_iter()
            .map(|i| match i {
                Item::AgentMessage { text, .. } => text,
                other => panic!("expected AgentMessage, got {other:?}"),
            })
            .collect();
        assert_eq!(texts, vec!["first", "second", "third"]);

        // No active turn: resume leaves the thread Idle/ready.
        assert!(handle.active_turn.lock().await.is_none());
    }

    /// Resuming a thread that does not exist in the index is `ThreadNotFound`.
    #[tokio::test]
    async fn resume_unknown_thread_is_not_found() {
        let (inner, _dir, _storage) = inner_with_storage().await;
        let err = inner
            .resume_thread(tid("thread:native/no-such"))
            .await
            .expect_err("unknown thread must fail");
        assert!(matches!(err, ResumeError::ThreadNotFound));
    }

    /// Resume on an engine without storage is `StorageUnavailable`.
    #[tokio::test]
    async fn resume_without_storage_is_unavailable() {
        let (events_tx, _) = broadcast::channel::<EngineEvent>(8);
        let inner = Arc::new(EngineInner::new(events_tx, noop_provider()));
        let err = inner
            .resume_thread(tid("thread:native/x"))
            .await
            .expect_err("no storage must fail");
        assert!(matches!(err, ResumeError::StorageUnavailable));
    }

    /// Resume refuses when the engine is busy (phase not Idle).
    #[tokio::test]
    async fn resume_busy_when_not_idle() {
        let (inner, _dir, storage) = inner_with_storage().await;
        let thread = tid("thread:native/resume-busy");
        seed_two_turn_thread(&storage, &thread).await;

        inner
            .try_set_phase_atomic(EnginePhase::Idle, EnginePhase::Turn)
            .expect("seed phase to Turn");

        let err = inner
            .resume_thread(thread)
            .await
            .expect_err("resume must refuse when busy");
        assert!(matches!(err, ResumeError::EngineBusy { .. }));
        // Phase untouched by the refused resume.
        assert_eq!(*inner.phase_lock(), EnginePhase::Turn);
    }

    /// A rollout with `[Session, Item×3(turn0), Compaction, Item×1(turn1)]`
    /// resumes to a memory transcript of `[marker, summary, turn1_item]` only —
    /// the original `turn0` items are discarded by the `Compaction` checkpoint.
    ///
    /// This validates B2's core invariant: a resume after compaction does NOT
    /// replay the full un-compacted history, preventing provider context overflow.
    #[expect(
        clippy::too_many_lines,
        reason = "linear scenario test: seeds a multi-entry rollout then asserts the post-compaction resume transcript"
    )]
    #[tokio::test]
    async fn resume_after_compaction_discards_pre_compaction_history() {
        use std::path::PathBuf;
        use zhive_proto::domain::{Thread, ThreadSource, ThreadStatus};

        use crate::persistence::{RolloutEntry, RolloutWriter};

        let (inner, _dir, storage) = inner_with_storage().await;
        let thread = tid("thread:native/resume-compact");

        // Seed the SQL index row.
        storage
            .state
            .upsert_thread(&Thread {
                id: thread.clone(),
                session_id: None,
                forked_from: None,
                subagent_parent: None,
                preview: "compacted".into(),
                ephemeral: false,
                model_provider: "test".into(),
                created_at: 1,
                updated_at: 2,
                status: ThreadStatus::Idle,
                cwd: PathBuf::from("/"),
                source: ThreadSource::User,
                name: None,
                turns: vec![],
            })
            .await
            .unwrap();

        // Build the rollout: Session + 3 pre-compaction items + Compaction + 1
        // post-compaction item.
        let rollout_path = storage.rollout_path(&thread.0);
        let mut w = RolloutWriter::open(rollout_path).await.unwrap();
        w.append(&RolloutEntry::Session {
            version: 3,
            id: thread.0.to_string(),
            timestamp: 0,
            cwd: "/".into(),
            parent_session: None,
            subagent_parent: None,
            source: None,
        })
        .await
        .unwrap();

        let turn0 = format!("turn:{}/0", thread.0);
        for n in 0u32..3 {
            w.append(&RolloutEntry::Item {
                thread_id: thread.0.to_string(),
                turn_id: turn0.clone(),
                timestamp: 10 + i64::from(n),
                item: Box::new(Item::AgentMessage {
                    id: ItemId(Arc::from(format!("item:pre/{n}").as_str())),
                    text: format!("pre-compaction-{n}"),
                }),
            })
            .await
            .unwrap();
        }

        let compact_turn = format!("{}::compaction-1", thread.0);
        let marker = Item::ContextCompaction {
            id: ItemId(Arc::from(
                format!("{}::compaction-1-marker", thread.0).as_str(),
            )),
        };
        let summary_item = Item::AgentMessage {
            id: ItemId(Arc::from(
                format!("{}::compaction-1-summary", thread.0).as_str(),
            )),
            text: "[context summary]\nHANDOFF SUMMARY".to_owned(),
        };
        w.append(&RolloutEntry::Compaction {
            thread_id: thread.0.to_string(),
            turn_id: compact_turn.clone(),
            timestamp: 20,
            summary: "HANDOFF SUMMARY".to_owned(),
            replacement: vec![Box::new(marker.clone()), Box::new(summary_item.clone())],
            entries_compacted: 3,
        })
        .await
        .unwrap();

        let turn1 = format!("turn:{}/1", thread.0);
        w.append(&RolloutEntry::Item {
            thread_id: thread.0.to_string(),
            turn_id: turn1.clone(),
            timestamp: 30,
            item: Box::new(Item::AgentMessage {
                id: ItemId(Arc::from("item:post/0")),
                text: "post-compaction".to_owned(),
            }),
        })
        .await
        .unwrap();
        w.sync_all().await.unwrap();

        // Resume the thread.
        let reply = inner
            .resume_thread(thread.clone())
            .await
            .expect("resume must succeed");

        // items_restored counts only what ends up in memory after processing the
        // Compaction entry: 2 replacement items + 1 post-compaction item = 3.
        assert_eq!(
            reply.items_restored, 3,
            "must restore [marker, summary, post-compaction] only"
        );
        // turns_restored: compaction turn + post-compaction turn = 2.
        assert_eq!(reply.turns_restored, 2);

        let handle = inner
            .threads()
            .get(&thread)
            .await
            .expect("thread must be resident");

        let snapshot: Vec<Item> = handle.items_snapshot().await;
        assert_eq!(snapshot.len(), 3, "only 3 items must be in memory");

        // First item: ContextCompaction marker.
        assert!(
            matches!(snapshot[0], Item::ContextCompaction { .. }),
            "first item must be ContextCompaction marker"
        );
        // Second item: summary AgentMessage.
        match &snapshot[1] {
            Item::AgentMessage { text, .. } => {
                assert!(
                    text.contains("HANDOFF SUMMARY"),
                    "second item must carry the handoff summary"
                );
            }
            other => panic!("expected AgentMessage summary, got {other:?}"),
        }
        // Third item: post-compaction item.
        match &snapshot[2] {
            Item::AgentMessage { text, .. } => {
                assert_eq!(text, "post-compaction");
            }
            other => panic!("expected post-compaction AgentMessage, got {other:?}"),
        }

        // The pre-compaction texts must NOT appear in the transcript.
        for item in &snapshot {
            if let Item::AgentMessage { text, .. } = item {
                assert!(
                    !text.starts_with("pre-compaction"),
                    "pre-compaction item must not appear after resume: {text}"
                );
            }
        }
    }

    // ----------------------------------------------------------------
    // B6: pending permission resume tests
    // ----------------------------------------------------------------

    /// Helpers shared by the B6 resume tests.
    fn make_perm_request_entry(
        tid: &str,
        tool: &str,
    ) -> zhive_proto::permission::RequestPermissionRequest {
        serde_json::from_value(serde_json::json!({
            "threadId": tid,
            "resourceType": "tool",
            "name": tool,
            "reason": "test",
            "options": []
        }))
        .expect("perm request fixture")
    }

    /// Seeds a thread with a `PendingPermission` entry but no matching
    /// `PermissionResolved` → resume must re-register the pending request.
    #[tokio::test]
    async fn resume_restores_pending_permission() {
        use crate::engine::submission::PermissionRequestId;
        use crate::persistence::{RolloutEntry, RolloutWriter};
        use std::path::PathBuf;
        use zhive_proto::domain::{Thread, ThreadSource, ThreadStatus};

        let (inner, _dir, storage) = inner_with_storage().await;
        let thread = tid("thread:native/perm-resume");

        // Seed SQL index row.
        storage
            .state
            .upsert_thread(&Thread {
                id: thread.clone(),
                session_id: None,
                forked_from: None,
                subagent_parent: None,
                preview: "perm test".into(),
                ephemeral: false,
                model_provider: "test".into(),
                created_at: 1,
                updated_at: 2,
                status: ThreadStatus::Idle,
                cwd: PathBuf::from("/"),
                source: ThreadSource::User,
                name: None,
                turns: vec![],
            })
            .await
            .unwrap();

        // Write a rollout: Session + one Item + one PendingPermission (no Resolved).
        let rollout_path = storage.rollout_path(&thread.0);
        let mut w = RolloutWriter::open(rollout_path).await.unwrap();
        w.append(&RolloutEntry::Session {
            version: 4,
            id: thread.0.to_string(),
            timestamp: 0,
            cwd: "/".into(),
            parent_session: None,
            subagent_parent: None,
            source: None,
        })
        .await
        .unwrap();
        let turn0 = format!("turn:{}/0", thread.0);
        w.append(&RolloutEntry::Item {
            thread_id: thread.0.to_string(),
            turn_id: turn0.clone(),
            timestamp: 1,
            item: Box::new(Item::AgentMessage {
                id: ItemId(Arc::from("item:0")),
                text: "hello".into(),
            }),
        })
        .await
        .unwrap();
        w.append(&RolloutEntry::PendingPermission {
            thread_id: thread.0.to_string(),
            turn_id: turn0.clone(),
            timestamp: 2,
            request_id: "perm:3".into(),
            request: Box::new(make_perm_request_entry(&thread.0, "bash")),
        })
        .await
        .unwrap();
        w.sync_all().await.unwrap();
        drop(w);

        // Resume the thread.
        let reply = inner
            .resume_thread(thread.clone())
            .await
            .expect("resume must succeed");
        assert_eq!(reply.items_restored, 1, "one item restored");

        // After resume, the pending permission map must have one entry for
        // the reinstated key (perm:3 → key 3).
        let reducer = inner.permission_reducer();
        assert_eq!(
            reducer.pending().len(),
            1,
            "reinstated pending permission must be registered"
        );

        // Resolving via wire id must succeed (key is re-registered).
        let wire = PermissionRequestId(Arc::from("perm:3"));
        reducer
            .resolve_by_wire_id(
                &wire,
                zhive_proto::permission::PermissionOutcome::Selected {
                    option_id: "allow-once".into(),
                },
            )
            .expect("resolve of reinstated request must succeed");
    }

    /// If a rollout contains `PendingPermission` AND a matching
    /// `PermissionResolved`, resume must NOT re-register the request.
    #[tokio::test]
    async fn resume_skips_resolved_pending_permission() {
        use crate::persistence::{RolloutEntry, RolloutWriter};
        use std::path::PathBuf;
        use zhive_proto::domain::{Thread, ThreadSource, ThreadStatus};

        let (inner, _dir, storage) = inner_with_storage().await;
        let thread = tid("thread:native/perm-resolved");

        storage
            .state
            .upsert_thread(&Thread {
                id: thread.clone(),
                session_id: None,
                forked_from: None,
                subagent_parent: None,
                preview: "perm resolved".into(),
                ephemeral: false,
                model_provider: "test".into(),
                created_at: 1,
                updated_at: 2,
                status: ThreadStatus::Idle,
                cwd: PathBuf::from("/"),
                source: ThreadSource::User,
                name: None,
                turns: vec![],
            })
            .await
            .unwrap();

        // Rollout: Session + PendingPermission + PermissionResolved.
        let rollout_path = storage.rollout_path(&thread.0);
        let mut w = RolloutWriter::open(rollout_path).await.unwrap();
        w.append(&RolloutEntry::Session {
            version: 4,
            id: thread.0.to_string(),
            timestamp: 0,
            cwd: "/".into(),
            parent_session: None,
            subagent_parent: None,
            source: None,
        })
        .await
        .unwrap();
        w.append(&RolloutEntry::PendingPermission {
            thread_id: thread.0.to_string(),
            turn_id: format!("turn:{}/0", thread.0),
            timestamp: 1,
            request_id: "perm:5".into(),
            request: Box::new(make_perm_request_entry(&thread.0, "read_file")),
        })
        .await
        .unwrap();
        w.append(&RolloutEntry::PermissionResolved {
            thread_id: thread.0.to_string(),
            request_id: "perm:5".into(),
            timestamp: 2,
        })
        .await
        .unwrap();
        w.sync_all().await.unwrap();
        drop(w);

        inner
            .resume_thread(thread.clone())
            .await
            .expect("resume must succeed");

        // No pending permission must be registered — the Resolved cancelled it.
        let reducer = inner.permission_reducer();
        assert_eq!(
            reducer.pending().len(),
            0,
            "resolved permission must not be re-registered on resume"
        );
    }

    /// After resume, the prompt the engine would build for the next turn
    /// contains the restored history (the core resume guarantee).
    #[tokio::test]
    async fn resume_then_prompt_includes_history() {
        use crate::engine::prompt::build_call_options;
        use zhive_proto::permission::PermissionScope;

        let (inner, _dir, storage) = inner_with_storage().await;
        let thread = tid("thread:native/resume-prompt");
        seed_two_turn_thread(&storage, &thread).await;

        inner.resume_thread(thread.clone()).await.unwrap();
        let handle = inner.threads().get(&thread).await.unwrap();

        let opts = build_call_options(
            &handle,
            &crate::tools::ToolRegistry::new(),
            None,
            &PermissionScope::default_turn_scope(),
        )
        .await;

        // Each restored AgentMessage maps to an assistant message carrying its
        // text, so the rebuilt prompt contains all three restored turns.
        let assistant_texts: Vec<String> = opts
            .prompt
            .iter()
            .filter_map(|m| match m {
                llmsdk::language_model::Message::Assistant { content, .. } => {
                    content.iter().find_map(|p| match p {
                        llmsdk::language_model::AssistantPart::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            assistant_texts,
            vec!["first", "second", "third"],
            "resumed history must appear in the next turn's prompt"
        );
    }
}

// Rust guideline compliant 2026-02-21

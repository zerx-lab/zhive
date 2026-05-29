//! Subagent spawn logic for [`EngineInner`].
//!
//! Contains the three methods that constitute the subagent lifecycle —
//! `spawn_subagent`, `start_child_turn`, `run_child_turn_and_deliver`, and
//! `deliver_subagent_outcome` — extracted from [`super::inner`] to keep
//! both files under the 600-line soft limit.
//!
//! They remain part of the same logical `impl EngineInner` block; Rust
//! allows multiple `impl` blocks across different files in the same module
//! tree.
//!
//! ## Invariants
//!
//! - Child threads do **not** participate in the global [`EnginePhase`]
//!   machine; only the parent (top-level) turn raises and lowers the phase.
//! - Persistence (when storage is configured) mirrors `lifecycle::start_turn`:
//!   `ThreadUpserted(Subagent, Active)` → `TurnStarted`, then at finish
//!   `TurnEnded` → `ThreadUpserted(Subagent, Idle)`.
//! - The final message is delivered via both the in-process
//!   [`SubagentFinalEvent`] channel on the child handle **and** the
//!   broadcast [`EngineEvent::SubagentCompleted`].
//!
//! [`EnginePhase`]: zhive_proto::hook::EnginePhase
//! [`SubagentFinalEvent`]: crate::subagent::SubagentFinalEvent

use std::sync::Arc;

use tracing::Instrument as _;
use zhive_proto::domain::ThreadId;
use zhive_proto::permission::{PermissionScope, SubagentDefinition};

use crate::persistence::writer::StorageWriteOp;
use crate::state::{ActiveTurn, ThreadHandle};
use crate::subagent::{SubagentError, prepare_child_scope};

use super::event::EngineEvent;
use super::inner::EngineInner;
use super::submission::SubagentSpawnError;

impl EngineInner {
    /// Spawns a subagent child thread and starts its first turn.
    ///
    /// Enforces the three Claude Code hard constraints via
    /// [`prepare_child_scope`]:
    ///
    /// 1. **No recursion** — rejects when `parent.parent_thread_id.is_some()`.
    /// 2. **Child spawn disabled** — rejects when
    ///    `definition.allow_subagent_spawn == true`.
    /// 3. **Scope can only narrow** — delegates to `prepare_child_scope`.
    ///
    /// On success: inserts a new [`ThreadHandle`] into `self.threads`,
    /// spawns a turn task for the child, and returns the new [`ThreadId`].
    /// The child transcript starts empty (fresh context window).
    ///
    /// The child thread participates fully in persistence (if storage is
    /// configured): `start_child_turn` enqueues `ThreadUpserted` +
    /// `TurnStarted` under the child's own thread id so the JSONL rollout
    /// and SQL indices are populated for crash recovery (D-011 / B3 §7.3).
    ///
    /// The engine does NOT change the global phase when spawning subagents;
    /// the parent turn is already in `Turn` phase and the child simply adds
    /// a new thread entry.
    pub(super) async fn spawn_subagent(
        self: &Arc<Self>,
        parent_thread_id: ThreadId,
        definition: SubagentDefinition,
    ) -> Result<ThreadId, SubagentSpawnError> {
        // (a) Look up the parent thread.
        let parent_handle = self
            .threads()
            .get(&parent_thread_id)
            .await
            .ok_or(SubagentSpawnError::ParentNotFound)?;

        // (b) Determine whether the parent is itself a subagent.
        let parent_is_subagent = parent_handle.parent_thread_id.is_some();

        // (c) Obtain the parent's current permission scope.
        //
        // If the parent has an active turn, use its stored scope. If the
        // parent is idle (e.g. a test without a live turn), fall back to
        // the default scope so the narrowing check has a sensible baseline.
        let parent_scope: PermissionScope = {
            let active = parent_handle.active_turn.lock().await;
            active
                .as_ref()
                .map_or_else(PermissionScope::default_turn_scope, |a| a.scope.clone())
        };

        // (d) Allocate child thread id.
        // Format: "thread:subagent/<parent-stem>/<counter>"
        let counter = self
            .turn_counter()
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let parent_stem = parent_thread_id
            .0
            .strip_prefix("thread:")
            .unwrap_or(&parent_thread_id.0);
        let child_thread_id = ThreadId(Arc::from(format!(
            "thread:subagent/{parent_stem}/{counter}"
        )));

        // (e) Validate constraints and compute the child scope.
        let child_scope = prepare_child_scope(
            &parent_scope,
            parent_is_subagent,
            &definition,
            child_thread_id.clone(),
        )
        .map_err(|err| match err {
            SubagentError::ParentIsSubagent => SubagentSpawnError::RecursionForbidden,
            SubagentError::ChildSpawnRequested => SubagentSpawnError::ChildSpawnRequested,
            SubagentError::InvalidNarrowing(_) | SubagentError::ScopeConstruction(_) => {
                SubagentSpawnError::ScopeWideningRejected
            }
        })?;

        // (f) Build a child ThreadHandle with a fresh context window and
        //     register it in the thread store.
        //
        // `new_child` returns `(handle, rx)`: the `Sender` is stored on the
        // handle so `run_child_turn_and_deliver` can fire the in-process
        // channel; the `Receiver` (`_subagent_rx`) is returned to the
        // spawner so it can `await` the child result directly from within a
        // parent tool-call handler.  For now the spawner at this call-site
        // drops it — the broadcast bus remains the delivery path for external
        // observers, and a future increment will wire `_subagent_rx` into the
        // Agent-tool handler.
        let (child_handle_inner, _subagent_rx) =
            ThreadHandle::new_child(child_thread_id.clone(), parent_thread_id.clone());
        let child_handle = Arc::new(child_handle_inner);
        self.threads()
            .write_guard()
            .await
            .insert(child_thread_id.clone(), Arc::clone(&child_handle));

        // (g) Install active turn + build user-input items and broadcast,
        //     then spawn the provider task.
        let child_turn_id = {
            let seq = self
                .turn_counter()
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            zhive_proto::domain::TurnId(Arc::from(format!("turn:{}/{seq}", child_thread_id.0)))
        };
        let child_cancel = self.cancel_tree().child_for_turn();
        self.start_child_turn(
            &child_handle,
            child_thread_id.clone(),
            child_turn_id.clone(),
            child_cancel.clone(),
            child_scope.scope.clone(),
            child_scope.prompt.clone(),
        )
        .await;

        // Spawn the task that runs the child turn and delivers its final event.
        let inner = Arc::clone(self);
        let child_tid = child_thread_id.clone();
        let parent_tid = parent_thread_id.clone();
        tokio::spawn(Self::run_child_turn_and_deliver(
            inner,
            child_handle,
            child_tid,
            child_turn_id,
            child_cancel,
            parent_tid,
        ));

        tracing::debug!(
            name: "zhive.engine.subagent.spawned",
            parent_thread_id = %parent_thread_id.0,
            child_thread_id = %child_thread_id.0,
            "subagent child thread spawned"
        );

        Ok(child_thread_id)
    }

    /// Installs an active turn on the child handle, pushes the initial
    /// user-input item, and enqueues the two persistence ops required to
    /// make the child thread durable under its own thread id and rollout.
    ///
    /// The persistence ops mirror those in [`lifecycle::start_turn`]:
    /// - [`StorageWriteOp::ThreadUpserted`] — writes the rollout header so
    ///   crash recovery can reconstruct the child transcript (D-011 / B3 §7.3).
    /// - [`StorageWriteOp::TurnStarted`] — records the turn start timestamp
    ///   in the SQL index so the turn appears in the crash-recovery sweep.
    ///
    /// Both ops are no-ops when no storage backend is configured
    /// (`enqueue_storage_op` is a no-op in that case).
    ///
    /// [`lifecycle::start_turn`]: super::lifecycle
    async fn start_child_turn(
        &self,
        child_handle: &Arc<ThreadHandle>,
        child_thread_id: ThreadId,
        child_turn_id: zhive_proto::domain::TurnId,
        child_cancel: tokio_util::sync::CancellationToken,
        scope: PermissionScope,
        prompt: String,
    ) {
        let started_at = crate::engine::lifecycle::unix_now_pub();
        let child_active_turn = ActiveTurn::new_with_cancel_and_scope(
            child_turn_id.clone(),
            started_at,
            child_cancel,
            scope,
        );
        *child_handle.active_turn.lock().await = Some(child_active_turn);
        *child_handle.status.write().await = zhive_proto::domain::ThreadStatus::Active {
            active_flags: vec![zhive_proto::domain::ThreadActiveFlag::TurnInProgress],
        };

        // Build and broadcast the prompt as a UserMessage item.
        if !prompt.is_empty() {
            let id_str = format!("item:subagent:{}/prompt", child_thread_id.0);
            let item_id = zhive_proto::domain::ItemId(Arc::from(id_str.as_str()));
            let item = zhive_proto::domain::Item::UserMessage {
                id: item_id,
                content: vec![zhive_proto::domain::ItemContent::Text {
                    text: prompt,
                    annotations: None,
                }],
            };
            child_handle.push_item(item.clone()).await;
            let _ = self.events_tx().send(EngineEvent::ItemAppended {
                thread_id: child_thread_id.clone(),
                turn_id: child_turn_id.clone(),
                item: Box::new(item),
            });
        }

        let _ = self.events_tx().send(EngineEvent::TurnStarted {
            thread_id: child_thread_id.clone(),
            turn_id: child_turn_id.clone(),
        });

        // Persistence: enqueue the two ops that make the child thread
        // durable under its own thread id and rollout (same pattern as
        // `start_turn` in lifecycle.rs). The `ThreadSource::Subagent`
        // discriminant lets the crash-recovery path distinguish child
        // threads from top-level ones.
        //
        // ThreadUpserted first — ensures the JSONL rollout header is
        // written before TurnStarted so the writer can open the file.
        let child_snapshot = zhive_proto::domain::Thread {
            id: child_thread_id.clone(),
            session_id: None,
            forked_from: None,
            preview: String::new(),
            ephemeral: false,
            model_provider: "unknown".to_owned(),
            created_at: started_at,
            updated_at: started_at,
            status: zhive_proto::domain::ThreadStatus::Active {
                active_flags: vec![zhive_proto::domain::ThreadActiveFlag::TurnInProgress],
            },
            cwd: std::path::PathBuf::from("."),
            source: zhive_proto::domain::ThreadSource::Subagent,
            name: None,
            turns: vec![],
        };
        self.enqueue_storage_op(StorageWriteOp::ThreadUpserted(Box::new(child_snapshot)));
        self.enqueue_storage_op(StorageWriteOp::TurnStarted {
            thread_id: child_thread_id,
            turn_id: child_turn_id,
            started_at,
        });
    }

    /// Runs the child provider turn, then delivers the outcome via both the
    /// in-process [`crate::subagent::SubagentFinalEvent`] channel stored on
    /// `child_handle` and the engine-wide broadcast
    /// [`EngineEvent::SubagentCompleted`].
    ///
    /// Opens a `zhive.subagent` span with `session.id` (child) and
    /// `zhive.parent.session.id` (parent) fields.  The span wraps the entire
    /// child turn lifetime so an OTLP backend can visualise subagent depth.
    ///
    /// ## Delivery ordering
    ///
    /// 1. The in-process `subagent_final_tx` is sent first so the spawning
    ///    context (e.g. a parent Agent-tool handler holding the receiver) can
    ///    react immediately without subscribing to the bus.
    /// 2. The broadcast event is sent second for external observers.
    ///
    /// The two deliveries are consistent: both carry the same `final_message`.
    ///
    /// ## Error vs. Completed
    ///
    /// `run_turn` returns `true` when the turn ended with a `TurnFailed`
    /// broadcast (provider error, in-stream error). In that case
    /// `final_message` is `None` in both delivery paths, matching the
    /// Claude Code contract ("child error = tool result error, parent sees
    /// no final message").
    async fn run_child_turn_and_deliver(
        inner: Arc<Self>,
        child_handle: Arc<ThreadHandle>,
        child_tid: ThreadId,
        child_turn_id: zhive_proto::domain::TurnId,
        child_cancel: tokio_util::sync::CancellationToken,
        parent_tid: ThreadId,
    ) {
        // Open a `zhive.subagent` span that spans the entire child turn.
        //
        // Span name is a literal; spans::SUBAGENT is the single source of
        // truth (see observability tests).  `Instrument` ensures the span
        // is entered/exited correctly across every await point.
        let span = tracing::info_span!(
            "zhive.subagent",
            "session.id"              = %child_tid.0,
            "zhive.parent.session.id" = %parent_tid.0,
        );

        Self::run_child_turn_inner(
            inner,
            child_handle,
            child_tid,
            child_turn_id,
            child_cancel,
            parent_tid,
        )
        .instrument(span)
        .await;
    }

    /// Inner body of [`Self::run_child_turn_and_deliver`], instrumented by
    /// the caller with the `zhive.subagent` span.
    async fn run_child_turn_inner(
        inner: Arc<Self>,
        child_handle: Arc<ThreadHandle>,
        child_tid: ThreadId,
        child_turn_id: zhive_proto::domain::TurnId,
        child_cancel: tokio_util::sync::CancellationToken,
        parent_tid: ThreadId,
    ) {
        let child_failed = super::turn::run_turn(
            &inner,
            Arc::clone(&child_handle),
            child_tid.clone(),
            child_turn_id,
            child_cancel,
        )
        .await;

        // `run_turn` has already called `finish_turn`, so the child transcript
        // is stable and the `active_turn` slot is `None`.
        let final_msg = if child_failed {
            // The child turn emitted TurnFailed; surface no final message to
            // the parent so the Claude Code "child error = tool result error"
            // contract is satisfied.
            None
        } else {
            let Some(child_h) = inner.threads().get(&child_tid).await else {
                // Thread removed concurrently (should not happen in normal flow).
                Self::deliver_subagent_outcome(
                    &inner,
                    &child_handle,
                    parent_tid,
                    child_tid,
                    None,
                    child_failed,
                )
                .await;
                return;
            };
            let tail: Vec<zhive_proto::domain::Item> =
                child_h.items_tail.read().await.iter().cloned().collect();
            crate::subagent::extract_final_message(&tail)
        };

        Self::deliver_subagent_outcome(
            &inner,
            &child_handle,
            parent_tid,
            child_tid,
            final_msg,
            child_failed,
        )
        .await;
    }

    /// Sends [`crate::subagent::SubagentFinalEvent`] on the in-process channel
    /// (if wired) and then broadcasts [`EngineEvent::SubagentCompleted`].
    ///
    /// Extracted from [`Self::run_child_turn_and_deliver`] to avoid code
    /// duplication across the two early-return paths.
    async fn deliver_subagent_outcome(
        inner: &Arc<Self>,
        child_handle: &Arc<ThreadHandle>,
        parent_tid: ThreadId,
        child_tid: ThreadId,
        final_msg: Option<std::sync::Arc<zhive_proto::domain::Item>>,
        child_failed: bool,
    ) {
        use crate::subagent::SubagentFinalEvent;
        use zhive_proto::domain::TurnError;

        // In-process delivery: fire the mpsc channel stored on the child
        // handle so the spawner can `await` the result directly.
        if let Some(tx) = &child_handle.subagent_final_tx {
            let event = if child_failed {
                SubagentFinalEvent::Errored {
                    child_thread_id: child_tid.clone(),
                    // Phase 1: error detail is not yet propagated from
                    // `run_turn`; use a sentinel error string. A future
                    // increment can thread the real `TurnError` through here.
                    error: TurnError {
                        message: "subagent turn failed".to_owned(),
                        additional_details: None,
                    },
                }
            } else {
                SubagentFinalEvent::Completed {
                    child_thread_id: child_tid.clone(),
                    final_message: final_msg.clone(),
                }
            };
            // Ignore send errors: the spawner may have dropped the receiver
            // if it only cares about the broadcast bus.
            let _ = tx.send(event).await;
        }

        // Broadcast so external subscribers observe the outcome.
        let _ = inner.events_tx().send(EngineEvent::SubagentCompleted {
            parent_thread_id: parent_tid,
            child_thread_id: child_tid,
            final_message: final_msg,
        });
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::broadcast;
    use zhive_proto::domain::ThreadId;
    use zhive_proto::hook::EnginePhase;

    use crate::engine::event::EngineEvent;
    use crate::engine::inner::EngineInner;
    use crate::provider::DynLanguageModel;

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    fn noop_provider() -> DynLanguageModel {
        use crate::provider::ScriptedModel;
        ScriptedModel::new("noop", "noop", vec![]).into_dyn()
    }

    fn new_inner() -> Arc<EngineInner> {
        let (events_tx, _) = broadcast::channel::<EngineEvent>(16);
        Arc::new(EngineInner::new(events_tx, noop_provider()))
    }

    /// `spawn_subagent` must reject when the parent thread does not exist
    /// in the thread store (`ParentNotFound`).
    #[tokio::test]
    async fn spawn_subagent_rejects_missing_parent() {
        let inner = new_inner();
        let parent_id = tid("thread:native/missing-parent");
        let definition: zhive_proto::permission::SubagentDefinition =
            serde_json::from_value(serde_json::json!({
                "name": "scout",
                "description": "test",
                "prompt": "Hello.",
            }))
            .expect("fixture");

        let result = inner.spawn_subagent(parent_id, definition).await;
        assert!(
            matches!(
                result,
                Err(crate::engine::submission::SubagentSpawnError::ParentNotFound)
            ),
            "expected ParentNotFound, got {result:?}"
        );
    }

    /// `spawn_subagent` must reject when the parent thread is itself a
    /// subagent (recursion ban).
    #[tokio::test]
    async fn spawn_subagent_rejects_recursion() {
        let inner = new_inner();

        // Register a child handle to simulate a subagent parent.
        let fake_parent_id = tid("thread:subagent/native/root/0");
        let (child_handle_inner, _rx) = crate::state::ThreadHandle::new_child(
            fake_parent_id.clone(),
            tid("thread:native/root"),
        );
        {
            let mut guard = inner.threads().write_guard().await;
            guard.insert(fake_parent_id.clone(), Arc::new(child_handle_inner))
        };

        let definition: zhive_proto::permission::SubagentDefinition =
            serde_json::from_value(serde_json::json!({
                "name": "scout",
                "description": "test",
                "prompt": "Hello.",
            }))
            .expect("fixture");

        let result = inner.spawn_subagent(fake_parent_id, definition).await;
        assert!(
            matches!(
                result,
                Err(crate::engine::submission::SubagentSpawnError::RecursionForbidden)
            ),
            "expected RecursionForbidden, got {result:?}"
        );
    }

    /// After `spawn_subagent` succeeds, the child thread handle must have
    /// `parent_thread_id` set and the child thread id must carry the subagent
    /// prefix.
    #[tokio::test]
    async fn spawn_subagent_inserts_child_handle_with_parent_set() {
        let inner = new_inner();

        // Register a top-level parent handle.
        let parent_id = tid("thread:native/parent-insert");
        let _parent_handle = inner.threads().get_or_init(&parent_id).await;

        let definition: zhive_proto::permission::SubagentDefinition =
            serde_json::from_value(serde_json::json!({
                "name": "scout",
                "description": "test",
                "prompt": "Check.",
            }))
            .expect("fixture");

        let child_id = inner
            .spawn_subagent(parent_id.clone(), definition)
            .await
            .expect("spawn_subagent must succeed");

        assert!(
            child_id.0.starts_with("thread:subagent/"),
            "child id must carry the subagent prefix, got {}",
            child_id.0
        );

        let child_handle = inner
            .threads()
            .get(&child_id)
            .await
            .expect("child thread must be registered");
        assert_eq!(
            child_handle.parent_thread_id.as_ref(),
            Some(&parent_id),
            "child handle must record its parent"
        );

        // Engine phase must still be Idle (spawn_subagent does NOT raise it).
        assert_eq!(
            *inner.phase_lock(),
            EnginePhase::Idle,
            "spawn_subagent must not change the global engine phase"
        );
    }
}

// Rust guideline compliant 2026-02-21

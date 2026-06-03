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

mod spawner;

pub(crate) use spawner::EngineSubagentSpawner;

use std::sync::Arc;

use tracing::Instrument as _;
use zhive_proto::domain::ThreadId;
use zhive_proto::permission::{PermissionScope, SubagentDefinition, ToolName};

use crate::persistence::writer::StorageWriteOp;
use crate::state::{ActiveTurn, ThreadHandle};
use crate::subagent::{
    SubagentDecisionRequest, SubagentError, SubagentFinalEvent, prepare_child_scope,
};

use super::event::EngineEvent;
use super::inner::EngineInner;
use super::submission::SubagentSpawnError;

/// The spawn results returned by [`EngineInner::spawn_subagent_awaitable`].
///
/// Bundles the child thread id with the two in-process channels the parent
/// spawner needs: the final-result receiver and the per-tool-call permission
/// handshake receiver. Aliased so the long tuple does not repeat across the
/// `spawn_subagent` / `spawn_subagent_awaitable` / `spawner` boundary.
pub(crate) type AwaitableSpawn = (
    ThreadId,
    tokio::sync::mpsc::Receiver<SubagentFinalEvent>,
    tokio::sync::mpsc::Receiver<SubagentDecisionRequest>,
);

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
    /// This is the actor-dispatch entry point: it delegates to
    /// [`Self::spawn_subagent_awaitable`] and then drops the in-process
    /// [`SubagentFinalEvent`] receiver, so external observers rely on the
    /// broadcast [`EngineEvent::SubagentCompleted`] for the outcome. The
    /// model-callable `agent` tool uses `spawn_subagent_awaitable` directly so
    /// it can `await` the child result.
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
        let (child_thread_id, _final_rx, _decision_rx) = self
            .spawn_subagent_awaitable(parent_thread_id, definition)
            .await?;
        // The actor-dispatch path does not await the child result; external
        // observers consume the broadcast `SubagentCompleted` event instead.
        // Both in-process receivers are dropped: with no parent select loop
        // consuming `decision_rx`, the child's per-tool-call handshake `send`
        // fails and its dispatch falls back to a conservative deny — which is
        // the correct behaviour for an unsupervised actor-dispatch spawn.
        Ok(child_thread_id)
    }

    /// Spawns a subagent and returns its id plus the in-process result channel.
    ///
    /// Identical to [`Self::spawn_subagent`] but hands the
    /// [`SubagentFinalEvent`] receiver back to the caller so a model-callable
    /// tool can `await` the child's final message directly, without
    /// subscribing to the broadcast bus. The receiver yields exactly one event
    /// (`Completed` or `Errored`) when the child turn finishes.
    ///
    /// # Errors
    ///
    /// Returns the same [`SubagentSpawnError`] variants as
    /// [`Self::spawn_subagent`] (`ParentNotFound`, `RecursionForbidden`,
    /// `ChildSpawnRequested`, `ScopeWideningRejected`).
    pub(crate) async fn spawn_subagent_awaitable(
        self: &Arc<Self>,
        parent_thread_id: ThreadId,
        definition: SubagentDefinition,
    ) -> Result<AwaitableSpawn, SubagentSpawnError> {
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
        let mut child_scope = prepare_child_scope(
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

        // (e2) Exclude tools that are flagged as subagent-unavailable from the
        // child scope. This enforces the `disable_in_subagent` contract for
        // skill tools (and any future implementors) without touching the parent
        // turn's tool set. Tools already in `disallowed_tools` are not
        // re-inserted to keep the list deduplicated.
        for (name, tool) in self.tools().iter() {
            if !tool.available_in_subagent() {
                let tool_name = ToolName(Arc::from(name.as_str()));
                if !child_scope.scope.disallowed_tools.contains(&tool_name) {
                    child_scope.scope.disallowed_tools.push(tool_name);
                }
            }
        }

        // (f) Build a child ThreadHandle with a fresh context window and
        //     register it in the thread store.
        //
        // `new_child` returns `(handle, rx)`: the `Sender` is stored on the
        // handle so `run_child_turn_and_deliver` can fire the in-process
        // channel; the `Receiver` (`subagent_rx`) is returned to the caller so
        // it can `await` the child result directly from within a parent
        // tool-call handler (the `agent` tool). Callers that only care about
        // the broadcast bus (the actor-dispatch `spawn_subagent`) simply drop
        // it.
        let (child_handle_inner, subagent_rx, decision_rx) =
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

        // Broadcast `SubagentStarted` so external observers learn the
        // parent ↔ child relationship up front and can route the child
        // thread's subsequent `ItemAppended` / `TurnStarted` events (which
        // carry the child thread id) back to the parent. `agent_type` and
        // `description` are mirrored from the definition; empty strings are
        // normalised to `None` so the wire payload omits blank fields.
        let agent_type = Some(definition.name.clone()).filter(|s| !s.is_empty());
        let description = Some(definition.description.clone()).filter(|s| !s.is_empty());
        let _ = self.events_tx().send(EngineEvent::SubagentStarted {
            parent_thread_id: parent_thread_id.clone(),
            child_thread_id: child_thread_id.clone(),
            agent_type,
            description,
        });

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

        Ok((child_thread_id, subagent_rx, decision_rx))
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
        // Seed the history buffer's active turn so `push_item` below has a turn
        // to append to (mirrors `start_turn` in lifecycle.rs).
        child_handle
            .start_turn_buffer(child_turn_id.clone(), started_at)
            .await;
        *child_handle.status.write().await = zhive_proto::domain::ThreadStatus::Active {
            active_flags: vec![zhive_proto::domain::ThreadActiveFlag::TurnInProgress],
        };

        // Derive the child preview from its spawn prompt (deterministic, no
        // LLM) BEFORE `prompt` is moved into the UserMessage item below. This
        // mirrors the top-level `start_turn` preview derivation.
        let child_preview = crate::engine::lifecycle::truncate_preview(&prompt);

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
        //
        // The parent link (`subagent_parent`) is taken from the child handle so
        // resume / rebuild can recover the parent-child relationship.
        let child_snapshot = zhive_proto::domain::Thread {
            id: child_thread_id.clone(),
            session_id: None,
            forked_from: None,
            subagent_parent: child_handle.parent_thread_id.clone(),
            preview: child_preview,
            ephemeral: false,
            model_provider: "unknown".to_owned(),
            created_at: started_at,
            updated_at: started_at,
            status: zhive_proto::domain::ThreadStatus::Active {
                active_flags: vec![zhive_proto::domain::ThreadActiveFlag::TurnInProgress],
            },
            cwd: self.cwd().to_path_buf(),
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
        // `None` = clean completion or cancellation; `Some(e)` = the real
        // failure error. This value is threaded verbatim into
        // `deliver_subagent_outcome` below, which maps `Some(e)` to
        // `SubagentFinalEvent::Errored { error: e }` so the parent sees the
        // actual provider/turn failure rather than a sentinel.
        let child_error = super::turn::run_turn(
            &inner,
            Arc::clone(&child_handle),
            child_tid.clone(),
            child_turn_id,
            child_cancel,
        )
        .await;

        // `run_turn` has already called `finish_turn`, so the child transcript
        // is stable and the `active_turn` slot is `None`.
        let final_msg = if child_error.is_some() {
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
                    child_error,
                )
                .await;
                return;
            };
            let tail: Vec<zhive_proto::domain::Item> = child_h.items_snapshot().await;
            crate::subagent::extract_final_message(&tail)
        };

        Self::deliver_subagent_outcome(
            &inner,
            &child_handle,
            parent_tid,
            child_tid,
            final_msg,
            child_error,
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
        child_error: Option<zhive_proto::domain::TurnError>,
    ) {
        // In-process delivery: fire the mpsc channel stored on the child
        // handle so the spawner can `await` the result directly.
        if let Some(tx) = &child_handle.subagent_final_tx {
            let event = match child_error {
                // The real `TurnError` from `run_turn` is threaded through
                // verbatim, so the parent sees the actual failure cause.
                Some(error) => SubagentFinalEvent::Errored {
                    child_thread_id: child_tid.clone(),
                    error,
                },
                None => SubagentFinalEvent::Completed {
                    child_thread_id: child_tid.clone(),
                    final_message: final_msg.clone(),
                },
            };
            // Ignore send errors: the spawner may have dropped the receiver
            // if it only cares about the broadcast bus.
            let _ = tx.send(event).await;
        }

        // Broadcast so external subscribers observe the outcome.
        // Clone `child_tid` before the move so we can remove the handle
        // from the store afterwards (see below).
        let _ = inner.events_tx().send(EngineEvent::SubagentCompleted {
            parent_thread_id: parent_tid,
            child_thread_id: child_tid.clone(),
            final_message: final_msg,
        });

        // Remove the child handle from the thread store now that both delivery
        // paths have completed.  This prevents long-lived sessions that spawn
        // many subagents from accumulating handles in memory indefinitely.
        //
        // Ordering: removal happens strictly *after* both the in-process send
        // and the broadcast event, so any subscriber that holds a reference to
        // the child thread id via those events can still look it up before this
        // call returns.  The child's on-disk rollout and SQL rows are
        // unaffected — history remains queryable via persistence.
        //
        // `ThreadStore::remove` is a no-op for unknown ids, so this is safe to
        // call even if the thread was already removed by a concurrent path.
        inner.threads().remove(&child_tid).await;
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
    use zhive_proto::permission::ToolName;

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
        let (child_handle_inner, _final_rx, _decision_rx) = crate::state::ThreadHandle::new_child(
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

    /// `spawn_subagent_awaitable` (the path the `agent` tool uses) must apply
    /// the same recursion ban as `spawn_subagent`.
    #[tokio::test]
    async fn spawn_awaitable_rejects_recursion() {
        let inner = new_inner();

        let fake_parent_id = tid("thread:subagent/native/root/0");
        let (child_handle_inner, _final_rx, _decision_rx) = crate::state::ThreadHandle::new_child(
            fake_parent_id.clone(),
            tid("thread:native/root"),
        );
        inner
            .threads()
            .write_guard()
            .await
            .insert(fake_parent_id.clone(), Arc::new(child_handle_inner));

        let definition: zhive_proto::permission::SubagentDefinition =
            serde_json::from_value(serde_json::json!({
                "name": "scout",
                "description": "test",
                "prompt": "Hello.",
            }))
            .expect("fixture");

        let result = inner
            .spawn_subagent_awaitable(fake_parent_id, definition)
            .await;
        assert!(
            matches!(
                result,
                Err(crate::engine::submission::SubagentSpawnError::RecursionForbidden)
            ),
            "spawn_subagent_awaitable must reject recursion, got {:?}",
            result.as_ref().map(|(id, _, _)| id.clone())
        );
    }

    /// `EngineSubagentSpawner::spawn_and_await` (the tool-facing bridge) must
    /// surface the recursion ban as an `Err(String)` when invoked from a
    /// subagent context, so the `agent` tool reports a clean failure.
    #[tokio::test]
    async fn spawner_bridge_reports_recursion_error_string() {
        use crate::tools::SubagentSpawner as _;

        let inner = new_inner();
        let fake_parent_id = tid("thread:subagent/native/root/0");
        let (child_handle_inner, _final_rx, _decision_rx) = crate::state::ThreadHandle::new_child(
            fake_parent_id.clone(),
            tid("thread:native/root"),
        );
        inner
            .threads()
            .write_guard()
            .await
            .insert(fake_parent_id.clone(), Arc::new(child_handle_inner));

        let spawner = super::EngineSubagentSpawner::new(Arc::clone(&inner), fake_parent_id);
        let err = spawner
            .spawn_and_await("scout".to_owned(), "probe".to_owned(), "do work".to_owned())
            .await
            .expect_err("recursion from a subagent context must fail");
        assert!(
            err.contains("recursion forbidden"),
            "error string must mention recursion, got {err:?}"
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

    /// `spawn_subagent` must broadcast `SubagentStarted` carrying the parent ↔
    /// child relationship plus the definition's name / description, so external
    /// observers can route the child thread's later events back to the parent.
    #[tokio::test]
    async fn spawn_subagent_broadcasts_subagent_started() {
        let (events_tx, mut events_rx) = broadcast::channel::<EngineEvent>(64);
        let inner = Arc::new(EngineInner::new(events_tx, noop_provider()));

        let parent_id = tid("thread:native/parent-started");
        let _parent_handle = inner.threads().get_or_init(&parent_id).await;

        let definition: zhive_proto::permission::SubagentDefinition =
            serde_json::from_value(serde_json::json!({
                "name": "scout",
                "description": "read-only scout",
                "prompt": "Check the environment.",
            }))
            .expect("fixture");

        let child_id = inner
            .spawn_subagent(parent_id.clone(), definition)
            .await
            .expect("spawn_subagent must succeed");

        // Scan the broadcast for the SubagentStarted event.
        let mut saw_started = false;
        while let Ok(ev) = events_rx.try_recv() {
            if let EngineEvent::SubagentStarted {
                parent_thread_id,
                child_thread_id,
                agent_type,
                description,
            } = ev
            {
                assert_eq!(parent_thread_id, parent_id);
                assert_eq!(child_thread_id, child_id);
                assert_eq!(agent_type.as_deref(), Some("scout"));
                assert_eq!(description.as_deref(), Some("read-only scout"));
                saw_started = true;
                break;
            }
        }
        assert!(
            saw_started,
            "spawn_subagent must broadcast SubagentStarted for the child"
        );
    }

    /// A failing child turn must surface its **real** `TurnError` message to
    /// the parent via `SubagentFinalEvent::Errored` — not the old sentinel
    /// `"subagent turn failed"`.
    #[tokio::test]
    async fn deliver_subagent_outcome_errored_carries_real_message() {
        use crate::subagent::SubagentFinalEvent;
        use zhive_proto::domain::TurnError;

        let inner = new_inner();
        let child_tid = tid("thread:subagent/native/root/0");
        let parent_tid = tid("thread:native/root");
        let (child_handle, mut rx, _decision_rx) =
            crate::state::ThreadHandle::new_child(child_tid.clone(), parent_tid.clone());
        let child_handle = Arc::new(child_handle);

        let real = TurnError {
            message: "provider exploded: no such model".to_owned(),
            additional_details: None,
        };
        EngineInner::deliver_subagent_outcome(
            &inner,
            &child_handle,
            parent_tid,
            child_tid.clone(),
            None,
            Some(real.clone()),
        )
        .await;

        match rx.recv().await.expect("final event") {
            SubagentFinalEvent::Errored { error, .. } => {
                assert_eq!(
                    error.message, real.message,
                    "Errored must carry the real provider error, not a sentinel"
                );
                assert_ne!(error.message, "subagent turn failed");
            }
            other => panic!("expected Errored, got {other:?}"),
        }
    }

    /// `None` error (clean completion or cancellation) yields `Completed`.
    #[tokio::test]
    async fn deliver_subagent_outcome_completed_when_no_error() {
        use crate::subagent::SubagentFinalEvent;

        let inner = new_inner();
        let child_tid = tid("thread:subagent/native/root/1");
        let parent_tid = tid("thread:native/root");
        let (child_handle, mut rx, _decision_rx) =
            crate::state::ThreadHandle::new_child(child_tid.clone(), parent_tid.clone());
        let child_handle = Arc::new(child_handle);

        EngineInner::deliver_subagent_outcome(
            &inner,
            &child_handle,
            parent_tid,
            child_tid,
            None,
            None,
        )
        .await;

        match rx.recv().await.expect("final event") {
            SubagentFinalEvent::Completed { final_message, .. } => {
                assert!(
                    final_message.is_none(),
                    "passing final_msg=None must yield Completed with no message"
                );
            }
            other => panic!("no error must map to Completed, got {other:?}"),
        }
    }

    /// B11: after `deliver_subagent_outcome` the child handle must have been
    /// removed from the thread store so a long-lived session that spawns many
    /// subagents does not accumulate handles in memory.
    #[tokio::test]
    async fn deliver_subagent_outcome_removes_child_handle_from_store() {
        let inner = new_inner();
        let child_tid = tid("thread:subagent/native/root/b11");
        let parent_tid = tid("thread:native/root");

        let (child_handle_inner, _final_rx, _decision_rx) =
            crate::state::ThreadHandle::new_child(child_tid.clone(), parent_tid.clone());
        let child_handle = Arc::new(child_handle_inner);

        // Register the child handle in the store (mirrors spawn path).
        inner
            .threads()
            .write_guard()
            .await
            .insert(child_tid.clone(), Arc::clone(&child_handle));

        // Verify it's in the store before delivery.
        assert!(
            inner.threads().get(&child_tid).await.is_some(),
            "child handle must be in the store before delivery"
        );

        // Deliver the outcome (clean completion).
        EngineInner::deliver_subagent_outcome(
            &inner,
            &child_handle,
            parent_tid,
            child_tid.clone(),
            None,
            None,
        )
        .await;

        // After delivery the handle must be gone from the store.
        assert!(
            inner.threads().get(&child_tid).await.is_none(),
            "child handle must be removed from the store after delivery (B11)"
        );
    }

    /// B11: the removal also happens on the error path — a failed child turn
    /// must still clean up the handle.
    #[tokio::test]
    async fn deliver_subagent_outcome_removes_child_handle_on_error_path() {
        use zhive_proto::domain::TurnError;

        let inner = new_inner();
        let child_tid = tid("thread:subagent/native/root/b11-err");
        let parent_tid = tid("thread:native/root");

        let (child_handle_inner, _final_rx, _decision_rx) =
            crate::state::ThreadHandle::new_child(child_tid.clone(), parent_tid.clone());
        let child_handle = Arc::new(child_handle_inner);

        inner
            .threads()
            .write_guard()
            .await
            .insert(child_tid.clone(), Arc::clone(&child_handle));

        let err = TurnError {
            message: "provider exploded".to_owned(),
            additional_details: None,
        };
        EngineInner::deliver_subagent_outcome(
            &inner,
            &child_handle,
            parent_tid,
            child_tid.clone(),
            None,
            Some(err),
        )
        .await;

        assert!(
            inner.threads().get(&child_tid).await.is_none(),
            "child handle must be removed from the store even on error path (B11)"
        );
    }

    /// A tool whose `available_in_subagent()` returns `false` must appear in
    /// the child scope's `disallowed_tools` after `spawn_subagent_awaitable`.
    ///
    /// Concretely: build an engine with a `SubagentBlockedTool` pre-registered,
    /// spawn a child scope, and assert the child's `disallowed_tools` contains
    /// the tool name.  The parent's `disallowed_tools` must remain unchanged.
    #[tokio::test]
    async fn spawn_subagent_excludes_subagent_unavailable_tools() {
        use async_trait::async_trait;

        use crate::tools::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};

        /// Toy tool that declares itself subagent-unavailable.
        #[derive(Debug)]
        struct SubagentBlockedTool;

        #[async_trait]
        impl Tool for SubagentBlockedTool {
            fn name(&self) -> &'static str {
                "blocked-in-subagent"
            }

            fn available_in_subagent(&self) -> bool {
                false
            }

            async fn execute(
                &self,
                _args: serde_json::Value,
                _ctx: &ToolContext,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::text("never"))
            }
        }

        // Build the engine with the blocked tool pre-registered.
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(SubagentBlockedTool));
        let (events_tx, _) = broadcast::channel::<EngineEvent>(16);
        let inner = Arc::new(EngineInner::new_with_hooks_tools_storage(
            events_tx,
            noop_provider(),
            Arc::new(crate::hooks::HookHost::new()),
            Arc::new(reg),
            crate::engine::TurnLimits::default(),
            None,
            None,
            None,
            None,
            None,
            std::path::PathBuf::from("."),
        ));

        let parent_id = tid("thread:native/parent-tool-filter");
        let _parent_handle = inner.threads().get_or_init(&parent_id).await;

        let definition: zhive_proto::permission::SubagentDefinition =
            serde_json::from_value(serde_json::json!({
                "name": "worker",
                "description": "test",
                "prompt": "go",
            }))
            .expect("fixture");

        let (child_id, _final_rx, _decision_rx) = inner
            .spawn_subagent_awaitable(parent_id.clone(), definition)
            .await
            .expect("spawn must succeed");

        let child_handle = inner
            .threads()
            .get(&child_id)
            .await
            .expect("child handle must be registered");

        let child_scope = child_handle
            .active_turn
            .lock()
            .await
            .as_ref()
            .map(|t| t.scope.disallowed_tools.clone())
            .unwrap_or_default();

        let blocked_name = ToolName(Arc::from("blocked-in-subagent"));
        assert!(
            child_scope.contains(&blocked_name),
            "child scope must disallow the subagent-blocked tool; got {child_scope:?}"
        );

        // Parent scope must remain pristine (not mutated by the spawn logic).
        let parent_handle = inner
            .threads()
            .get(&parent_id)
            .await
            .expect("parent handle");
        let parent_disallowed = parent_handle
            .active_turn
            .lock()
            .await
            .as_ref()
            .map_or_else(Vec::new, |t| t.scope.disallowed_tools.clone());
        assert!(
            !parent_disallowed.contains(&blocked_name),
            "parent disallowed_tools must NOT be modified by spawn"
        );
    }
}

// Rust guideline compliant 2026-02-21

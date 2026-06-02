//! Engine actor body.
//!
//! Owns the live thread map and the broadcast event channel, and
//! consumes the [`Submission`] stream serially. The actor dispatches
//! `StartTurn` by immediately replying with the assigned `TurnId`, then
//! spawns a separate async task (`run_turn`) that calls the LLM provider
//! and streams items back. This keeps the actor loop responsive to
//! `CancelTurn` while a turn is in flight.
//!
//! Turn lifecycle methods (`start_turn`, `finish_turn`, `cancel_turn`)
//! live in the sibling module [`super::lifecycle`] to keep both files
//! under the 600-line soft limit.
//!
//! ## Phase invariant
//!
//! The engine carries a single global [`EnginePhase`] (D-006 / B1).
//! `StartTurn` requires `Idle → Turn`; if the engine is already busy
//! the submission is refused via [`EngineEvent::TurnRejected`] rather
//! than silently dropped, so callers observe a deterministic outcome.
//! Phase + per-thread [`ThreadStatus`] transitions for one turn happen
//! inside the same critical section so external observers can never see
//! the two views disagree.
//!
//! ## Prompt mapping (Phase 1, documented deviations)
//!
//! `run_turn` maps the thread's `items_tail` to `llmsdk::Prompt` with
//! the following Phase-1 rules:
//! - `Item::UserMessage` → `Message::User { content: [UserPart::Text(…)] }`
//! - `Item::AgentMessage` → `Message::Assistant { content: [AssistantPart::Text(…)] }`
//! - All other item kinds are skipped (no tool results, no context items, …).
//!
//! If the resulting prompt is empty (no convertible history), the call is
//! still issued — the provider may generate a greeting or refuse; both
//! outcomes are valid.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::{Mutex, MutexGuard};

use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use zhive_proto::hook::EnginePhase;

use crate::cancel::CancellationTree;
use crate::hooks::HookHost;
use crate::permission::{PermissionReducer, ReducerError};
use crate::persistence::writer::StorageWriteOp;
use crate::provider::DynLanguageModel;
use crate::queues::QueueTarget;
use crate::state::ThreadStore;
use crate::tools::ToolRegistry;

use super::event::EngineEvent;
use super::phase::allows_transition;
use super::submission::{
    PermissionRequestId, ResumePermissionReply, Submission, SubmissionEnvelope, SubmissionReply,
};

// ============================================================
// StorageWriterState
// ============================================================

/// Bundles the persistence writer sender and its join handle so both can be
/// held in a single `Mutex` and taken together at shutdown.
struct StorageWriterState {
    /// Sender end of the write-op channel; drop to close the channel and
    /// signal the writer task to drain and exit.
    tx: Option<mpsc::Sender<StorageWriteOp>>,
    /// Join handle for the background writer task.
    handle: Option<JoinHandle<()>>,
}

// ============================================================
// EngineInner
// ============================================================

/// Shared state owned by the engine actor.
///
/// Wrapped in `Arc` so the actor loop and each spawned turn task can
/// both hold a reference without lifetime coupling.
pub(crate) struct EngineInner {
    threads: Arc<ThreadStore>,
    events_tx: broadcast::Sender<EngineEvent>,
    /// Engine-wide phase. A short-held `std::sync::Mutex` is preferred
    /// over a `tokio::sync::RwLock` because every transition runs to
    /// completion without an `await` point, and the synchronous lock
    /// makes the compare-and-set atomic against the broadcast emission.
    phase: Mutex<EnginePhase>,
    turn_counter: AtomicU64,
    /// Permission reducer shared with the reverse-RPC tracker.
    ///
    /// Exposed through [`Self::permission_reducer`] so the server module
    /// (and future hook host) can use the same `PendingPermissions`
    /// store for outbound prompts.
    permission: PermissionReducer,
    /// LLM provider used for every turn. A no-op [`crate::provider::ScriptedModel`]
    /// (empty stream) is supplied by [`super::Engine::spawn`] for backward
    /// compatibility; real providers are injected via
    /// [`super::Engine::spawn_with_provider`].
    provider: DynLanguageModel,
    /// Hook host shared across all turn tasks spawned by this engine.
    pub(in crate::engine) hook_host: Arc<HookHost>,
    /// Tool registry shared across all turn tasks spawned by this engine.
    pub(in crate::engine) tools: Arc<ToolRegistry>,
    /// Per-turn iteration limit applied by [`super::turn::run_turn`].
    turn_limits: super::TurnLimits,
    /// Optional system prompt prepended to every provider call.
    ///
    /// Cloned cheaply (`Arc`) into each [`super::prompt::build_call_options`]
    /// call; see [`super::EngineConfig::system_prompt`].
    system_prompt: Option<Arc<str>>,
    /// Engine-wide cancellation hierarchy.
    ///
    /// The root token represents engine shutdown.  Each turn gets a child
    /// token (`child_for_turn`) so that `shutdown` propagates to all
    /// in-flight turns without the per-turn cancel affecting other turns
    /// or the engine itself.  Each tool call gets a further child via
    /// `child_for_tool`.
    cancel_tree: CancellationTree,
    /// Optional persistence write-through sender.
    ///
    /// When `Some`, every lifecycle event (thread upserted, turn started,
    /// item appended, turn ended) is enqueued here.  The [`PersistenceWriter`]
    /// task drains it asynchronously.
    ///
    /// Wrapped in `Mutex<Option<…>>` so `run()` can `take()` the sender on
    /// shutdown (dropping it closes the channel, signalling the writer to
    /// drain and exit).
    ///
    /// [`PersistenceWriter`]: crate::persistence::writer::PersistenceWriter
    storage_writer: Mutex<StorageWriterState>,
}

impl EngineInner {
    pub(crate) fn new(
        events_tx: broadcast::Sender<EngineEvent>,
        provider: DynLanguageModel,
    ) -> Self {
        Self::new_with_hooks_tools_storage(
            events_tx,
            provider,
            Arc::new(HookHost::new()),
            Arc::new(ToolRegistry::new()),
            super::TurnLimits::default(),
            None,
            None,
            None,
        )
    }

    /// Full constructor used by [`super::Engine::spawn_with_config`].
    #[expect(
        clippy::too_many_arguments,
        reason = "constructor mirrors EngineConfig fields; a builder would add more complexity"
    )]
    pub(crate) fn new_with_hooks_tools_storage(
        events_tx: broadcast::Sender<EngineEvent>,
        provider: DynLanguageModel,
        hook_host: Arc<HookHost>,
        tools: Arc<ToolRegistry>,
        turn_limits: super::TurnLimits,
        system_prompt: Option<Arc<str>>,
        storage_tx: Option<mpsc::Sender<StorageWriteOp>>,
        storage_handle: Option<JoinHandle<()>>,
    ) -> Self {
        Self {
            threads: Arc::new(ThreadStore::new()),
            events_tx,
            phase: Mutex::new(EnginePhase::Idle),
            turn_counter: AtomicU64::new(0),
            permission: PermissionReducer::new(),
            provider,
            hook_host,
            tools,
            turn_limits,
            system_prompt,
            cancel_tree: CancellationTree::new(),
            storage_writer: Mutex::new(StorageWriterState {
                tx: storage_tx,
                handle: storage_handle,
            }),
        }
    }

    pub(crate) fn permission_reducer(&self) -> PermissionReducer {
        self.permission.clone()
    }

    /// Returns the event broadcast sender for sibling modules within
    /// the `engine` module (e.g. [`super::turn`]).
    pub(in crate::engine) fn events_tx(&self) -> &broadcast::Sender<EngineEvent> {
        &self.events_tx
    }

    /// Returns the LLM provider for sibling modules within
    /// the `engine` module (e.g. [`super::turn`]).
    pub(in crate::engine) fn provider(&self) -> &DynLanguageModel {
        &self.provider
    }

    /// Returns the hook host for sibling modules.
    pub(in crate::engine) fn hook_host(&self) -> &Arc<HookHost> {
        &self.hook_host
    }

    /// Returns the tool registry for sibling modules.
    pub(in crate::engine) fn tools(&self) -> &Arc<ToolRegistry> {
        &self.tools
    }

    /// Returns the configured system prompt for sibling modules, if any.
    pub(in crate::engine) fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    /// Returns the effective per-turn iteration cap for [`super::turn`].
    pub(in crate::engine) fn max_turn_iterations(&self) -> u32 {
        self.turn_limits.effective_cap()
    }

    /// Returns the cancellation tree for sibling modules (e.g. lifecycle).
    pub(in crate::engine) fn cancel_tree(&self) -> &CancellationTree {
        &self.cancel_tree
    }

    /// Non-blocking enqueue of a persistence write op.
    ///
    /// Logs at `warn` when the channel is full (back-pressure exceeded) but
    /// does not block or return an error — persistence is best-effort and
    /// must never stall the turn loop.
    pub(in crate::engine) fn enqueue_storage_op(&self, op: StorageWriteOp) {
        let guard = self
            .storage_writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(tx) = &guard.tx
            && let Err(err) = tx.try_send(op)
        {
            tracing::warn!(
                name: "zhive.engine.storage.enqueue_failed",
                error = %err,
                "persistence write op dropped; writer channel full or closed"
            );
        }
    }

    /// Returns the turn counter for sibling modules (e.g. lifecycle).
    pub(in crate::engine) fn turn_counter(&self) -> &AtomicU64 {
        &self.turn_counter
    }

    /// Runs the actor loop, consuming submissions until `Shutdown`.
    ///
    /// Takes `Arc<Self>` so the loop can clone the `Arc` into each
    /// spawned turn task without capturing `self` by reference.
    ///
    /// On receiving [`Submission::Shutdown`], the actor fires
    /// `cancel_tree.cancel_all()` so every in-flight turn task aborts
    /// promptly instead of waiting for its provider stream to drain.
    pub(crate) async fn run(
        self: Arc<Self>,
        mut submission_rx: mpsc::Receiver<SubmissionEnvelope>,
    ) {
        while let Some(env) = submission_rx.recv().await {
            let SubmissionEnvelope { submission, reply } = env;
            if matches!(submission, Submission::Shutdown) {
                // Cancel all in-flight turns before acknowledging the
                // shutdown so the spawned turn tasks abort promptly.
                self.cancel_tree.cancel_all();

                // Drop the persistence sender (closes the channel) then await
                // the writer task (best-effort, 5 s cap) so the last
                // completed turn is durable before the reply fires.
                let (writer_tx_drop, writer_handle) = {
                    let mut guard = self
                        .storage_writer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // Take both fields out of the mutex guard so they are
                    // dropped *outside* the lock (the handle.await below must
                    // not hold the Mutex).
                    (guard.tx.take(), guard.handle.take())
                };
                // Drop the sender here to signal the writer task.
                drop(writer_tx_drop);
                if let Some(h) = writer_handle {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h).await;
                }

                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::Shutdown);
                }
                break;
            }
            self.dispatch(submission, reply).await;
        }
    }

    async fn dispatch(
        self: &Arc<Self>,
        sub: Submission,
        reply: Option<tokio::sync::oneshot::Sender<SubmissionReply>>,
    ) {
        // Other submissions (injection, spawn subagent) land in
        // B7 / B8. Phase 1 records the intent without acting on it.
        // `Shutdown` is filtered out before dispatch by [`Self::run`].
        match sub {
            Submission::StartTurn {
                thread_id,
                user_input,
                scope: _,
            } => {
                let outcome = self.start_turn(thread_id, user_input).await;
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::StartTurn(outcome));
                }
            }
            Submission::CancelTurn { thread_id } => {
                let outcome = self.cancel_turn(thread_id).await;
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::CancelTurn(outcome));
                }
            }
            Submission::ResumePermission {
                request_id,
                outcome,
            } => {
                let r = self.resume_permission(&request_id, outcome);
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::ResumePermission(r));
                }
            }
            Submission::EnqueueInjection {
                thread_id,
                behavior,
                items,
            } => {
                self.enqueue_injection(thread_id, behavior, items).await;
                // Fire-and-forget: no typed reply for injection submissions.
                // If a reply channel was attached, drop it so the awaiter
                // surfaces `EngineError::ReplyDropped` immediately rather
                // than timing out.
                drop(reply);
            }
            Submission::EnqueueNextTurn { thread_id, items } => {
                self.enqueue_next_turn(thread_id, items).await;
                drop(reply);
            }
            Submission::SpawnSubagent {
                parent_thread_id,
                definition,
            } => {
                let outcome = self.spawn_subagent(parent_thread_id, definition).await;
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::SpawnSubagent(outcome));
                }
            }
            Submission::Compact { thread_id, trigger } => {
                let outcome = self.compact(thread_id, trigger).await;
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::Compact(outcome));
                }
            }
            other => {
                tracing::debug!(
                    name: "zhive.engine.submission.unhandled",
                    submission_kind = ?std::mem::discriminant(&other),
                    "submission landed but this increment does not act on it yet"
                );
                // If the caller attached a reply oneshot, drop it
                // explicitly so the awaiter surfaces
                // `EngineError::ReplyDropped` instead of timing out.
                drop(reply);
            }
        }
    }

    /// Enqueues items into the steer or follow-up queue of the target thread.
    ///
    /// `behavior` is mapped via [`QueueTarget::try_from`]; unknown
    /// `StreamingBehavior` variants are logged and dropped rather than
    /// propagating a wire error.
    ///
    /// If the thread does not yet exist, it is created (idle) so the items
    /// are queued for when a turn eventually starts.  Items submitted before
    /// any turn runs accumulate and are drained at the appropriate point once
    /// the turn loop begins.
    async fn enqueue_injection(
        &self,
        thread_id: zhive_proto::domain::ThreadId,
        behavior: zhive_proto::permission::StreamingBehavior,
        items: Vec<zhive_proto::domain::Item>,
    ) {
        let target = match QueueTarget::try_from(behavior) {
            Ok(t) => t,
            Err(unknown) => {
                tracing::warn!(
                    name: "zhive.engine.injection.unknown_behavior",
                    ?unknown,
                    "EnqueueInjection: unknown StreamingBehavior variant; items dropped"
                );
                return;
            }
        };
        let handle = self.threads.get_or_init(&thread_id).await;
        handle.injection_lock().push_back(target, items);
        tracing::debug!(
            name: "zhive.engine.injection.enqueued",
            thread_id = %thread_id.0,
            queue = ?target,
            "items pushed to injection queue"
        );
    }

    /// Enqueues items into the next-turn queue of the target thread.
    ///
    /// Next-turn items survive `cancel_turn` and are drained at the **start**
    /// of the next turn, prepended before the user input.  They may be
    /// enqueued at any time (including while a turn is in progress or when
    /// the thread is idle).
    ///
    /// If the thread does not yet exist, it is created so the items persist
    /// until the next `StartTurn` drains them.
    async fn enqueue_next_turn(
        &self,
        thread_id: zhive_proto::domain::ThreadId,
        items: Vec<zhive_proto::domain::Item>,
    ) {
        let handle = self.threads.get_or_init(&thread_id).await;
        handle
            .injection_lock()
            .push_back(QueueTarget::NextTurn, items);
        tracing::debug!(
            name: "zhive.engine.injection.next_turn.enqueued",
            thread_id = %thread_id.0,
            "items pushed to next-turn queue"
        );
    }

    /// Resolves a pending permission request driven by the client's
    /// answer. Logs a warning on protocol errors so they are visible
    /// at the engine boundary rather than silently dropped, and
    /// returns a typed [`ResumePermissionReply`] so the synchronous
    /// caller learns the outcome.
    fn resume_permission(
        &self,
        request_id: &PermissionRequestId,
        outcome: zhive_proto::permission::PermissionOutcome,
    ) -> ResumePermissionReply {
        match self.permission.resolve_by_wire_id(request_id, outcome) {
            Ok(()) => ResumePermissionReply::Resolved,
            Err(err) => {
                let rid = std::sync::Arc::<str>::clone(&request_id.0);
                let kind = error_kind(&err);
                let message = err.to_string();
                tracing::warn!(
                    name: "zhive.permission.resume.rejected",
                    request_id = %rid,
                    error_type = %kind,
                    error_message = %message,
                    "ResumePermission could not be applied"
                );
                match err {
                    ReducerError::UnknownRequest(_) => ResumePermissionReply::UnknownRequest,
                    ReducerError::InvalidRequestId(_) => ResumePermissionReply::InvalidRequestId,
                    // Reducer does not surface TimedOut on the resolve
                    // path (timeouts originate in `wait`), but the
                    // pattern is exhaustive over `#[non_exhaustive]`
                    // anyway — fall back to Abandoned which is the
                    // closest semantic match for a delivery failure.
                    ReducerError::Abandoned | ReducerError::TimedOut(_) => {
                        ResumePermissionReply::Abandoned
                    }
                }
            }
        }
    }

    /// Performs an atomic phase compare-and-set.
    ///
    /// Returns `Ok(())` when the transition was applied; otherwise
    /// returns a [`PhaseTransitionError`] that tells the caller whether
    /// the failure was caused by a precondition mismatch or by an
    /// illegal transition (so they can be logged distinctly).
    pub(in crate::engine) fn try_set_phase_atomic(
        &self,
        from: EnginePhase,
        to: EnginePhase,
    ) -> Result<(), PhaseTransitionError> {
        // Take the lock once for both the legality check and the CAS
        // so the caller sees a self-consistent observation of `actual`
        // (a separate-lock attempt could report a phase that has
        // already moved on between the two acquires).
        let mut guard = self.phase_lock();
        if !allows_transition(from, to) {
            return Err(PhaseTransitionError::Illegal {
                from,
                to,
                actual: *guard,
            });
        }
        if *guard != from {
            return Err(PhaseTransitionError::PreconditionMismatch {
                expected: from,
                actual: *guard,
            });
        }
        *guard = to;
        Ok(())
    }

    pub(in crate::engine) fn phase_lock(&self) -> MutexGuard<'_, EnginePhase> {
        // Poisoned phase lock is a programming error; the only writer
        // is this actor task and it does not panic in normal paths. If
        // it happens, recover by returning the inner value rather than
        // tearing down the actor.
        match self.phase.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub(crate) fn threads(&self) -> &Arc<ThreadStore> {
        &self.threads
    }
}

// ============================================================
// PhaseTransitionError
// ============================================================

/// Failure modes for [`EngineInner::try_set_phase_atomic`].
///
/// Distinguishes between a caller passing a forbidden `from→to` pair
/// (programming error) and the engine simply not being in the
/// expected `from` phase right now (a race that may be benign).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::engine) enum PhaseTransitionError {
    /// `from→to` is not in the legality table; observed phase
    /// reported for diagnostics only.
    Illegal {
        from: EnginePhase,
        to: EnginePhase,
        actual: EnginePhase,
    },
    /// `from→to` is legal but the current phase is not `from`.
    PreconditionMismatch {
        expected: EnginePhase,
        actual: EnginePhase,
    },
}

impl PhaseTransitionError {
    /// Observed phase at the time of the failed CAS.
    pub(in crate::engine) fn actual(self) -> EnginePhase {
        match self {
            Self::Illegal { actual, .. } | Self::PreconditionMismatch { actual, .. } => actual,
        }
    }
}

// ============================================================
// Helpers
// ============================================================

/// Stable short classifier for [`ReducerError`] used in trace fields.
///
/// `ReducerError` is `#[non_exhaustive]`; this match deliberately omits
/// the wildcard so a future variant forces a deliberate review here
/// rather than being grouped under a generic `"other"` label.
fn error_kind(err: &ReducerError) -> &'static str {
    match err {
        ReducerError::UnknownRequest(_) => "unknown_request",
        ReducerError::InvalidRequestId(_) => "invalid_request_id",
        ReducerError::Abandoned => "abandoned",
        ReducerError::TimedOut(_) => "timed_out",
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn noop_provider() -> DynLanguageModel {
        use crate::provider::ScriptedModel;
        ScriptedModel::new("noop", "noop", vec![]).into_dyn()
    }

    fn new_inner() -> Arc<EngineInner> {
        let (events_tx, _) = broadcast::channel::<EngineEvent>(16);
        Arc::new(EngineInner::new(events_tx, noop_provider()))
    }

    #[test]
    fn try_set_phase_atomic_reports_illegal_vs_mismatch() {
        let inner = new_inner();
        // Illegal: Idle → Retry is not in the legality table.
        let err = inner
            .try_set_phase_atomic(EnginePhase::Idle, EnginePhase::Retry)
            .unwrap_err();
        assert!(matches!(err, PhaseTransitionError::Illegal { .. }));
        // Precondition mismatch: Turn → Idle is legal but phase is Idle.
        let err = inner
            .try_set_phase_atomic(EnginePhase::Turn, EnginePhase::Idle)
            .unwrap_err();
        assert!(matches!(
            err,
            PhaseTransitionError::PreconditionMismatch {
                expected: EnginePhase::Turn,
                actual: EnginePhase::Idle,
            }
        ));
    }
}

// Rust guideline compliant 2026-02-21

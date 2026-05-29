//! Engine actor surface.
//!
//! The engine is a Tokio-driven actor: callers submit a stream of
//! [`Submission`] commands and observe outcomes through a broadcast
//! stream of [`EngineEvent`] values. Inside, [`EnginePhase`] gates the
//! state machine (`Idle` → `Turn` → `Compaction` / `BranchSummary` /
//! `Retry` → `Idle`) and a [`crate::state::ThreadStore`] owns live
//! thread handles.
//!
//! ## Synchronous reply pattern
//!
//! The four submissions that have an immediate result (`StartTurn`,
//! `CancelTurn`, `ResumePermission`, `Shutdown`) carry an optional
//! [`tokio::sync::oneshot::Sender`] so callers can `await` the
//! engine's verdict without polling the broadcast channel. Convenience
//! methods on [`Engine`] wrap the oneshot dance:
//! [`Engine::start_turn`], [`Engine::cancel_turn`],
//! [`Engine::resume_permission`], [`Engine::shutdown`].
//!
//! Subscribers that want streaming updates (e.g. live `ItemAppended`,
//! mid-turn `PhaseChanged`) still call [`Engine::subscribe`].
//!
//! ## Provider injection
//!
//! [`Engine::spawn`] supplies a no-op [`crate::provider::ScriptedModel`]
//! (empty stream) so all 107 pre-increment-2 tests remain green — they
//! only assert `TurnStarted` → `TurnCompleted`, which still holds.
//! [`Engine::spawn_with_provider`] injects a real or scripted provider
//! for callers and tests that need to observe actual item output.
//!
//! [`EnginePhase`]: zhive_proto::hook::EnginePhase

pub mod event;
mod inner;
pub mod phase;
pub mod submission;
mod turn;

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use zhive_proto::domain::{Item, ThreadId, TurnId};
use zhive_proto::hook::EnginePhase;
use zhive_proto::permission::{PermissionOutcome, PermissionScope};

use inner::EngineInner;

use crate::provider::DynLanguageModel;

#[doc(inline)]
pub use event::{EngineEvent, TurnRejectionReason};
#[doc(inline)]
pub use phase::allows_transition;
#[doc(inline)]
pub use submission::{PermissionRequestId, Submission, SubmissionEnvelope};

/// Cap on the in-flight [`Submission`] queue.
///
/// The actor consumes serially so the limit acts as backpressure; the
/// chosen value matches Pi's `submissionBuffer` configuration.
const SUBMISSION_CHANNEL_CAP: usize = 512;

/// Cap on the broadcast [`EngineEvent`] backlog per subscriber.
///
/// Subscribers that fall behind by more than this many events receive a
/// [`broadcast::error::RecvError::Lagged`] and must resync.
const EVENT_CHANNEL_CAP: usize = 1024;

/// Default deadline for [`Engine::start_turn`] / [`Engine::cancel_turn`]
/// when callers do not pick their own.
///
/// The reply path runs entirely inside the actor task and never crosses
/// a slow boundary, so this is a safety net rather than a feature —
/// most calls return in microseconds.
const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Top-level [`Engine`] failure surface.
///
/// Reasons a synchronous submission (e.g. [`Engine::start_turn`]) can
/// fail. The variants mirror the channel-level dispatcher's outcomes
/// (`TurnRejected` events, dropped reply senders, timeouts) so callers
/// never need to subscribe to the broadcast just to learn whether
/// their own submission landed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EngineError {
    /// Actor task has exited (typically after [`Submission::Shutdown`]).
    #[error("engine actor has stopped accepting submissions")]
    ActorStopped,

    /// Engine refused a `StartTurn` because its phase is not `Idle`.
    #[error("engine busy in phase {current:?}; cannot start a new turn")]
    EngineBusy {
        /// Phase observed at submission time.
        current: EnginePhase,
    },

    /// Reply path observed an internal failure (oneshot sender
    /// dropped, etc.). Should never happen in practice.
    #[error("engine actor dropped the reply channel without answering")]
    ReplyDropped,

    /// The synchronous reply did not arrive within the configured
    /// timeout.
    #[error("engine reply timed out after {0:?}")]
    ReplyTimedOut(Duration),
}

impl EngineError {
    fn from_submit(_err: mpsc::error::SendError<SubmissionEnvelope>) -> Self {
        Self::ActorStopped
    }
}

/// Cheap, clonable handle to a running engine.
///
/// The actor task lives independently of any one `Engine` clone; the
/// last clone goes out of scope only after [`Engine::shutdown`] (or
/// after every clone is dropped without an explicit shutdown — in
/// which case the actor exits because its submission channel closes).
#[derive(Debug, Clone)]
pub struct Engine {
    submission_tx: mpsc::Sender<SubmissionEnvelope>,
    events_tx: broadcast::Sender<EngineEvent>,
    threads: Arc<crate::state::ThreadStore>,
    permission: crate::permission::PermissionReducer,
    reply_timeout: Duration,
}

/// Backwards-compatible alias retained while callers migrate to
/// [`EngineError`].
pub type SubmitError = EngineError;

impl Engine {
    /// Spawns a fresh engine actor with a no-op provider and returns a handle.
    ///
    /// The no-op provider is a [`crate::provider::ScriptedModel`] that
    /// yields an **empty** stream (zero `StreamPart`s). A turn started
    /// against it produces no items and completes cleanly
    /// (`TurnStarted` → `TurnCompleted`), keeping all pre-increment-2
    /// tests green.
    ///
    /// The actor runs on the current Tokio runtime. Drop the last
    /// [`Engine`] clone to let the actor exit, or call
    /// [`Engine::shutdown`] for an explicit, awaited shutdown.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zhive_core::engine::Engine;
    /// # async fn demo() {
    /// let engine = Engine::spawn();
    /// engine.shutdown().await.unwrap();
    /// # }
    /// ```
    #[must_use]
    pub fn spawn() -> Self {
        use crate::provider::ScriptedModel;
        let noop = ScriptedModel::new("noop", "noop", vec![]).into_dyn();
        Self::spawn_with_provider(noop)
    }

    /// Spawns a fresh engine actor injecting a specific LLM provider.
    ///
    /// Use this constructor in tests or production code that needs to
    /// observe real (or scripted) model output. The `provider` is called
    /// once per turn; see [`crate::provider::ScriptedModel`] for an
    /// in-memory deterministic implementation suitable for testing.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use llmsdk::language_model::StreamPart;
    /// use zhive_core::engine::Engine;
    /// use zhive_core::provider::ScriptedModel;
    ///
    /// # async fn demo() {
    /// let model = ScriptedModel::new(
    ///     "test-provider",
    ///     "test-model",
    ///     vec![
    ///         StreamPart::TextStart { id: "b0".into(), provider_metadata: None },
    ///         StreamPart::TextDelta { id: "b0".into(), delta: "hello".into(), provider_metadata: None },
    ///         StreamPart::TextEnd   { id: "b0".into(), provider_metadata: None },
    ///     ],
    /// );
    /// let engine = Engine::spawn_with_provider(model.into_dyn());
    /// engine.shutdown().await.unwrap();
    /// # }
    /// ```
    #[must_use]
    pub fn spawn_with_provider(provider: DynLanguageModel) -> Self {
        let (submission_tx, submission_rx) = mpsc::channel(SUBMISSION_CHANNEL_CAP);
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAP);
        let inner = Arc::new(EngineInner::new(events_tx.clone(), provider));
        let threads = Arc::clone(inner.threads());
        let permission = inner.permission_reducer();
        tokio::spawn(inner.run(submission_rx));
        Self {
            submission_tx,
            events_tx,
            threads,
            permission,
            reply_timeout: DEFAULT_REPLY_TIMEOUT,
        }
    }

    /// Overrides the synchronous reply timeout (default
    /// [`DEFAULT_REPLY_TIMEOUT`]). Mostly for tests.
    #[must_use]
    pub fn with_reply_timeout(mut self, timeout: Duration) -> Self {
        self.reply_timeout = timeout;
        self
    }

    /// Hands a fire-and-forget [`Submission`] to the actor.
    ///
    /// Callers that need the typed reply should use one of the
    /// dedicated helpers ([`Self::start_turn`] etc.) instead.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::ActorStopped`] when the actor task has
    /// already finished processing a [`Submission::Shutdown`].
    pub async fn submit(&self, sub: Submission) -> Result<(), EngineError> {
        self.submission_tx
            .send(SubmissionEnvelope::fire_and_forget(sub))
            .await
            .map_err(EngineError::from_submit)
    }

    /// Submits and awaits a typed reply.
    async fn submit_with_reply(
        &self,
        sub: Submission,
    ) -> Result<submission::SubmissionReply, EngineError> {
        let (env, rx) = SubmissionEnvelope::with_reply(sub);
        self.submission_tx
            .send(env)
            .await
            .map_err(EngineError::from_submit)?;
        match tokio::time::timeout(self.reply_timeout, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_recv_err)) => Err(EngineError::ReplyDropped),
            Err(_elapsed) => Err(EngineError::ReplyTimedOut(self.reply_timeout)),
        }
    }

    /// Returns a fresh broadcast subscription to the [`EngineEvent`] stream.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events_tx.subscribe()
    }

    /// Returns the live [`crate::state::ThreadStore`] handle.
    #[must_use]
    pub fn threads(&self) -> Arc<crate::state::ThreadStore> {
        Arc::clone(&self.threads)
    }

    /// Returns the permission reducer shared with the actor.
    ///
    /// Cheap clone (`Arc`-shared). Useful for the reverse-RPC sink and
    /// the hook host so they share one `PendingPermissions` store with
    /// the engine actor's `ResumePermission` handler.
    #[must_use]
    pub fn permission_reducer(&self) -> crate::permission::PermissionReducer {
        self.permission.clone()
    }

    // --------------------------------------------------------------
    // High-level helpers (synchronous reply)
    // --------------------------------------------------------------

    /// Starts a new turn and awaits the actor's [`TurnId`] reply.
    ///
    /// # Errors
    ///
    /// * [`EngineError::EngineBusy`] when the engine phase is not
    ///   `Idle` at dispatch time (matches the broadcast
    ///   [`EngineEvent::TurnRejected`]).
    /// * [`EngineError::ActorStopped`] / [`EngineError::ReplyDropped`]
    ///   / [`EngineError::ReplyTimedOut`] on channel-level failures.
    pub async fn start_turn(
        &self,
        thread_id: ThreadId,
        user_input: Vec<Item>,
        scope: Option<PermissionScope>,
    ) -> Result<TurnId, EngineError> {
        let reply = self
            .submit_with_reply(Submission::StartTurn {
                thread_id,
                user_input,
                scope,
            })
            .await?;
        match reply {
            submission::SubmissionReply::StartTurn(Ok(ok)) => Ok(ok.turn_id),
            submission::SubmissionReply::StartTurn(Err(err)) => match err {
                submission::StartTurnError::EngineBusy { current } => {
                    Err(EngineError::EngineBusy { current })
                }
            },
            _ => Err(EngineError::ReplyDropped),
        }
    }

    /// Cancels the active turn on `thread_id`.
    ///
    /// Returns the cancelled [`TurnId`] when there was an active turn,
    /// or `None` when the cancel was a no-op.
    ///
    /// # Errors
    ///
    /// Channel-level [`EngineError`] variants only — the engine never
    /// surfaces a domain failure for cancel.
    pub async fn cancel_turn(&self, thread_id: ThreadId) -> Result<Option<TurnId>, EngineError> {
        let reply = self
            .submit_with_reply(Submission::CancelTurn { thread_id })
            .await?;
        match reply {
            submission::SubmissionReply::CancelTurn(submission::CancelTurnReply::Cancelled {
                turn_id,
            }) => Ok(Some(turn_id)),
            submission::SubmissionReply::CancelTurn(submission::CancelTurnReply::NoActiveTurn) => {
                Ok(None)
            }
            _ => Err(EngineError::ReplyDropped),
        }
    }

    /// Resolves a pending permission request and awaits the engine's
    /// acknowledgement.
    ///
    /// # Errors
    ///
    /// Channel-level [`EngineError`] variants. The reducer's own
    /// failures (unknown id, abandoned waiter, etc.) are folded into
    /// [`submission::ResumePermissionReply`] which is the function's
    /// `Ok` value.
    pub async fn resume_permission(
        &self,
        request_id: PermissionRequestId,
        outcome: PermissionOutcome,
    ) -> Result<submission::ResumePermissionReply, EngineError> {
        let reply = self
            .submit_with_reply(Submission::ResumePermission {
                request_id,
                outcome,
            })
            .await?;
        match reply {
            submission::SubmissionReply::ResumePermission(r) => Ok(r),
            _ => Err(EngineError::ReplyDropped),
        }
    }

    /// Sends a graceful shutdown and awaits the actor's acknowledgement.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::ActorStopped`] / [`EngineError::ReplyDropped`]
    /// / [`EngineError::ReplyTimedOut`] on channel failure paths.
    pub async fn shutdown(&self) -> Result<(), EngineError> {
        let reply = self.submit_with_reply(Submission::Shutdown).await?;
        match reply {
            submission::SubmissionReply::Shutdown => Ok(()),
            _ => Err(EngineError::ReplyDropped),
        }
    }
}

// ============================================================
// Test helpers (always compiled with #[cfg(test)] on the module)
// ============================================================

#[cfg(test)]
mod test_helpers {
    use async_trait::async_trait;
    use futures::stream;
    use llmsdk::LanguageModel;
    use llmsdk::language_model::{
        BoxStream, CallOptions, FinishReason, FinishReasonKind, GenerateResult, StreamPart,
        StreamResult,
    };
    use std::sync::Arc as StdArc;
    use tokio::sync::Barrier;

    /// A model that blocks on a [`Barrier`] inside `do_stream`.
    ///
    /// Used to keep the engine in Turn phase long enough for a second
    /// `start_turn` to observe `EngineBusy`, and for cancel tests.
    #[derive(Debug)]
    pub(super) struct BarrierModel {
        pub(super) barrier: StdArc<Barrier>,
    }

    impl BarrierModel {
        pub(super) fn new_pair(parties: usize) -> (StdArc<Barrier>, Self) {
            let b = StdArc::new(Barrier::new(parties));
            let model = Self {
                barrier: StdArc::clone(&b),
            };
            (b, model)
        }
    }

    #[async_trait]
    impl LanguageModel for BarrierModel {
        fn provider(&self) -> &'static str {
            "test"
        }
        fn model_id(&self) -> &'static str {
            "barrier"
        }
        async fn do_generate(&self, _opts: CallOptions) -> llmsdk::error::Result<GenerateResult> {
            Ok(GenerateResult {
                content: vec![],
                finish_reason: FinishReason::new(FinishReasonKind::Stop),
                usage: llmsdk::language_model::Usage::default(),
                provider_metadata: None,
                request: None,
                response: None,
                warnings: vec![],
            })
        }
        async fn do_stream(&self, _opts: CallOptions) -> llmsdk::error::Result<StreamResult> {
            // Block until the test calls barrier.wait() — this keeps the
            // engine in Turn phase for the duration needed by the test.
            self.barrier.wait().await;
            let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::empty());
            Ok(StreamResult {
                stream: s,
                request: None,
                response: None,
            })
        }
    }

    /// A model whose stream yields exactly one `Err` item.
    ///
    /// Used to verify that the in-stream error path emits `TurnFailed`
    /// and does NOT follow it with `TurnCompleted`.
    #[derive(Debug)]
    pub(super) struct ErrorStreamModel;

    #[async_trait]
    impl LanguageModel for ErrorStreamModel {
        fn provider(&self) -> &'static str {
            "test"
        }
        fn model_id(&self) -> &'static str {
            "error-stream"
        }
        async fn do_generate(&self, _opts: CallOptions) -> llmsdk::error::Result<GenerateResult> {
            Ok(GenerateResult {
                content: vec![],
                finish_reason: FinishReason::new(FinishReasonKind::Stop),
                usage: llmsdk::language_model::Usage::default(),
                provider_metadata: None,
                request: None,
                response: None,
                warnings: vec![],
            })
        }
        async fn do_stream(&self, _opts: CallOptions) -> llmsdk::error::Result<StreamResult> {
            let err = llmsdk::ProviderError::no_such_model("test", "languageModel");
            let s: BoxStream<llmsdk::error::Result<StreamPart>> =
                Box::pin(stream::iter(vec![Err(err)]));
            Ok(StreamResult {
                stream: s,
                request: None,
                response: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ScriptedModel;
    use llmsdk::language_model::StreamPart;

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    /// Helper: receive events up to `limit` times, looking for `pred`.
    async fn collect_events_until(
        rx: &mut broadcast::Receiver<EngineEvent>,
        limit: usize,
        mut pred: impl FnMut(&EngineEvent) -> bool,
    ) -> bool {
        for _ in 0..limit {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(ev)) if pred(&ev) => return true,
                Ok(Ok(_)) => {}
                _ => return false,
            }
        }
        false
    }

    #[tokio::test]
    async fn start_turn_emits_started_then_completed() {
        let engine = Engine::spawn();
        let mut events = engine.subscribe();
        let turn_id = engine
            .start_turn(tid("thread:native/a"), Vec::new(), None)
            .await
            .unwrap();
        assert!(turn_id.0.starts_with("turn:thread:native/a/"));

        let mut saw_started = false;
        let mut saw_completed = false;
        for _ in 0..32 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("timeout")
                .expect("broadcast")
            {
                EngineEvent::TurnStarted { .. } => saw_started = true,
                EngineEvent::TurnCompleted { .. } => saw_completed = true,
                _ => {}
            }
            if saw_started && saw_completed {
                break;
            }
        }
        assert!(saw_started, "expected TurnStarted");
        assert!(saw_completed, "expected TurnCompleted");
        engine.shutdown().await.unwrap();
    }

    /// A turn started while the engine is already in Turn phase must
    /// surface `EngineBusy`. We use a [`test_helpers::BarrierModel`] that
    /// blocks inside `do_stream`, keeping the engine in Turn phase long
    /// enough for the second `start_turn` call to observe the conflict.
    ///
    /// Synchronization is done by subscribing to the event channel before
    /// the first `start_turn`, then awaiting `TurnStarted`. Once the
    /// subscriber observes `TurnStarted` the phase is guaranteed to be
    /// `Turn`, so the subsequent `start_turn` will always surface
    /// `EngineBusy`. This avoids the inherent race in `yield_now` loops.
    #[tokio::test]
    async fn start_turn_returns_busy_when_engine_phase_not_idle() {
        let (barrier, model) = test_helpers::BarrierModel::new_pair(2);
        let engine = Engine::spawn_with_provider(DynLanguageModel::new(model))
            .with_reply_timeout(std::time::Duration::from_secs(5));

        // Subscribe BEFORE the first start_turn so we can observe TurnStarted.
        let mut events = engine.subscribe();

        // First start_turn — the actor spawns a turn task that blocks on
        // the barrier inside do_stream, keeping the engine in Turn phase.
        engine
            .start_turn(tid("thread:native/busy-1"), Vec::new(), None)
            .await
            .unwrap();

        // Wait for TurnStarted: once we see it, the phase is guaranteed
        // Turn and any subsequent start_turn will surface EngineBusy.
        let saw_started = collect_events_until(&mut events, 16, |ev| {
            matches!(ev, EngineEvent::TurnStarted { .. })
        })
        .await;
        assert!(saw_started, "expected TurnStarted from first turn");

        // Second start_turn while engine is still in Turn phase.
        let result = engine
            .start_turn(tid("thread:native/busy-2"), Vec::new(), None)
            .await;

        // Unblock the first turn so it can complete.
        barrier.wait().await;

        assert!(
            matches!(result, Err(EngineError::EngineBusy { .. })),
            "expected EngineBusy, got {result:?}"
        );
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn phase_changed_events_carry_thread_id() {
        let engine = Engine::spawn();
        let mut events = engine.subscribe();
        let id = tid("thread:native/phase");
        engine
            .start_turn(id.clone(), Vec::new(), None)
            .await
            .unwrap();

        let mut saw_idle_to_turn = false;
        let mut saw_turn_to_idle = false;
        for _ in 0..32 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("event recv must not time out")
                .expect("broadcast send error");
            if let EngineEvent::PhaseChanged {
                thread_id,
                from,
                to,
            } = ev
            {
                assert_eq!(
                    thread_id.as_ref(),
                    Some(&id),
                    "PhaseChanged must name thread"
                );
                match (from, to) {
                    (
                        zhive_proto::hook::EnginePhase::Idle,
                        zhive_proto::hook::EnginePhase::Turn,
                    ) => saw_idle_to_turn = true,
                    (
                        zhive_proto::hook::EnginePhase::Turn,
                        zhive_proto::hook::EnginePhase::Idle,
                    ) => saw_turn_to_idle = true,
                    _ => {}
                }
            }
            if saw_idle_to_turn && saw_turn_to_idle {
                break;
            }
        }
        assert!(saw_idle_to_turn && saw_turn_to_idle);
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn resume_permission_routes_to_reducer() {
        use zhive_proto::permission::PermissionOutcome;
        let engine = Engine::spawn();
        let reducer = engine.permission_reducer();

        let request: zhive_proto::permission::RequestPermissionRequest =
            serde_json::from_value(serde_json::json!({
                "threadId": "thread:native/a",
                "resourceType": "tool",
                "name": "read_file",
                "reason": "test",
                "options": []
            }))
            .unwrap();
        let (key, _req, rx) = reducer.enroll(request);
        let wire_id = key.to_wire();

        let reply = engine
            .resume_permission(
                wire_id,
                PermissionOutcome::Selected {
                    option_id: "allow_once".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(reply, submission::ResumePermissionReply::Resolved);

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), reducer.wait(rx))
            .await
            .expect("must resolve via ResumePermission")
            .unwrap();
        assert!(matches!(outcome, PermissionOutcome::Selected { .. }));
        engine.shutdown().await.unwrap();
    }

    // The full "cancel_turn cancels every pending permission" path is
    // exercised in `engine/inner.rs#cancel_turn_cancels_pending_permissions`
    // via a direct EngineInner test.

    #[tokio::test]
    async fn cancel_turn_with_no_active_turn_is_noop() {
        let engine = Engine::spawn();
        let cancelled = engine
            .cancel_turn(tid("thread:native/missing"))
            .await
            .unwrap();
        assert!(cancelled.is_none());
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_stops_actor() {
        let engine = Engine::spawn();
        engine.shutdown().await.unwrap();
        let mut last = Ok(());
        for _ in 0..20 {
            last = engine
                .submit(Submission::CancelTurn {
                    thread_id: tid("x"),
                })
                .await;
            if last.is_err() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(matches!(last, Err(EngineError::ActorStopped)));
    }

    // ============================================================
    // Increment-2 provider-driven turn tests
    // ============================================================

    /// A turn over a scripted text response must emit
    /// `ItemAppended(AgentMessage { text: "hello world" })` then `TurnCompleted`.
    #[tokio::test]
    async fn scripted_text_turn_emits_item_appended_then_completed() {
        let model = ScriptedModel::new(
            "test",
            "m",
            vec![
                StreamPart::TextStart {
                    id: "b0".into(),
                    provider_metadata: None,
                },
                StreamPart::TextDelta {
                    id: "b0".into(),
                    delta: "hello ".into(),
                    provider_metadata: None,
                },
                StreamPart::TextDelta {
                    id: "b0".into(),
                    delta: "world".into(),
                    provider_metadata: None,
                },
                StreamPart::TextEnd {
                    id: "b0".into(),
                    provider_metadata: None,
                },
            ],
        );
        let engine = Engine::spawn_with_provider(model.into_dyn());
        let mut events = engine.subscribe();

        engine
            .start_turn(tid("thread:native/scripted-text"), Vec::new(), None)
            .await
            .unwrap();

        let mut saw_item = false;
        let mut saw_completed = false;
        for _ in 0..32 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("timeout")
                .expect("broadcast");
            match ev {
                EngineEvent::ItemAppended { item, .. } => {
                    if let zhive_proto::domain::Item::AgentMessage { text, .. } = *item {
                        assert_eq!(text, "hello world");
                        saw_item = true;
                    }
                }
                EngineEvent::TurnCompleted { .. } => saw_completed = true,
                _ => {}
            }
            if saw_item && saw_completed {
                break;
            }
        }

        assert!(saw_item, "expected ItemAppended(AgentMessage)");
        assert!(saw_completed, "expected TurnCompleted");
        engine.shutdown().await.unwrap();
    }

    /// `cancel_turn` mid-stream must stop item emission and yield
    /// `SessionAborted`; `TurnCompleted` must NOT be emitted.
    #[tokio::test]
    async fn cancel_mid_stream_yields_session_aborted_no_completed() {
        // BarrierModel blocks inside do_stream so we can cancel before
        // any items arrive.
        let (barrier, model) = test_helpers::BarrierModel::new_pair(2);
        let engine = Engine::spawn_with_provider(DynLanguageModel::new(model))
            .with_reply_timeout(std::time::Duration::from_secs(5));
        let mut events = engine.subscribe();

        let thread_id = tid("thread:native/cancel-mid");
        engine
            .start_turn(thread_id.clone(), Vec::new(), None)
            .await
            .unwrap();

        // Wait for TurnStarted so the turn task is definitely in-flight.
        let saw_started = collect_events_until(&mut events, 16, |ev| {
            matches!(ev, EngineEvent::TurnStarted { .. })
        })
        .await;
        assert!(saw_started, "expected TurnStarted");

        // Cancel while the model is blocking in do_stream.
        let cancelled = engine.cancel_turn(thread_id).await.unwrap();
        assert!(cancelled.is_some(), "expected a turn id to be cancelled");

        // Unblock the model so its task can exit cleanly.
        barrier.wait().await;

        // SessionAborted must appear; TurnCompleted must NOT.
        let mut saw_aborted = false;
        let mut saw_completed = false;
        for _ in 0..32 {
            match tokio::time::timeout(std::time::Duration::from_millis(300), events.recv()).await {
                Ok(Ok(EngineEvent::SessionAborted(_))) => saw_aborted = true,
                Ok(Ok(EngineEvent::TurnCompleted { .. })) => saw_completed = true,
                Ok(Ok(_)) => {}
                _ => break,
            }
            if saw_aborted {
                break;
            }
        }
        assert!(saw_aborted, "expected SessionAborted after cancel");
        assert!(!saw_completed, "TurnCompleted must NOT appear after cancel");
        engine.shutdown().await.unwrap();
    }

    /// An in-stream provider error must emit `TurnFailed` and must NOT
    /// follow it with `TurnCompleted` — a turn has exactly one terminal
    /// event.
    ///
    /// This test exercises the `Some(Err(…))` arm of the stream loop
    /// in `run_turn`, which previously called `finish_turn` (emitting
    /// `TurnCompleted`) immediately after broadcasting `TurnFailed`.
    #[tokio::test]
    async fn stream_error_emits_turn_failed_not_completed() {
        let engine =
            Engine::spawn_with_provider(DynLanguageModel::new(test_helpers::ErrorStreamModel))
                .with_reply_timeout(std::time::Duration::from_secs(5));
        let mut events = engine.subscribe();

        engine
            .start_turn(tid("thread:native/stream-err"), Vec::new(), None)
            .await
            .unwrap();

        let mut saw_failed = false;
        let mut saw_completed = false;
        // Drain up to 32 events with a short per-event timeout so we
        // don't block forever if TurnCompleted is never emitted.
        for _ in 0..32 {
            match tokio::time::timeout(std::time::Duration::from_millis(500), events.recv()).await {
                Ok(Ok(EngineEvent::TurnFailed { .. })) => saw_failed = true,
                Ok(Ok(EngineEvent::TurnCompleted { .. })) => saw_completed = true,
                Ok(Ok(_)) => {}
                _ => break,
            }
            if saw_failed {
                // Give a brief window for a spurious TurnCompleted to arrive.
                for _ in 0..4 {
                    match tokio::time::timeout(std::time::Duration::from_millis(100), events.recv())
                        .await
                    {
                        Ok(Ok(EngineEvent::TurnCompleted { .. })) => {
                            saw_completed = true;
                            break;
                        }
                        Ok(Ok(_)) => {}
                        _ => break,
                    }
                }
                break;
            }
        }
        assert!(saw_failed, "expected TurnFailed for in-stream error");
        assert!(
            !saw_completed,
            "TurnCompleted must NOT follow TurnFailed for the same turn"
        );
        engine.shutdown().await.unwrap();
    }
}

// Rust guideline compliant 2026-02-21

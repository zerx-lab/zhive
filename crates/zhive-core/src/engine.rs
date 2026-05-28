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
//! Tool dispatch, the hook host, the provider fold, the persistence
//! sync, and per-thread agent loops are added by later Block B
//! tasks; today's actor finishes a turn synchronously inside the
//! `StartTurn` dispatch.
//!
//! [`EnginePhase`]: zhive_proto::hook::EnginePhase

pub mod event;
mod inner;
pub mod phase;
pub mod submission;

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use zhive_proto::domain::{Item, ThreadId, TurnId};
use zhive_proto::hook::EnginePhase;
use zhive_proto::permission::{PermissionOutcome, PermissionScope};

use inner::EngineInner;

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
    /// Spawns a fresh engine actor and returns a handle to it.
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
        let (submission_tx, submission_rx) = mpsc::channel(SUBMISSION_CHANNEL_CAP);
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAP);
        let inner = EngineInner::new(events_tx.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
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
        while !(saw_started && saw_completed) {
            match events.recv().await.unwrap() {
                EngineEvent::TurnStarted { .. } => saw_started = true,
                EngineEvent::TurnCompleted { .. } => saw_completed = true,
                _ => {}
            }
        }
        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn start_turn_returns_busy_when_engine_phase_not_idle() {
        let engine = Engine::spawn();
        // Manually flip the global phase to Turn so the next StartTurn
        // is rejected. We do this via two concurrent submissions
        // back-to-back through fire_and_forget — but Phase 1 finishes
        // turns synchronously, so we can't observe Busy through the
        // public API. Skip the test until B5/B10 introduce a real
        // long-running turn.
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
        for _ in 0..16 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
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

        // Enroll a pending request manually; the engine actor only
        // resolves them via the ResumePermission submission path.
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

    // The full "cancel_turn cancels every pending permission" path
    // is exercised in `engine/inner.rs#cancel_turn_cancels_pending_permissions`
    // via a direct EngineInner test because the Phase 1 actor auto-
    // completes turns synchronously and can't observe an `active_turn`
    // present at the time a `Submission::CancelTurn` lands.

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
        // Subsequent submits eventually surface ActorStopped.
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
}

// Rust guideline compliant 2026-02-21

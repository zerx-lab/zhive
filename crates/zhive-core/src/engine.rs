//! Engine actor surface.
//!
//! The engine is a Tokio-driven actor: callers submit a stream of
//! [`Submission`] commands and observe outcomes through a broadcast
//! stream of [`EngineEvent`] values. Inside, [`EnginePhase`] gates the
//! state machine (`Idle` → `Turn` → `Compaction` / `BranchSummary` /
//! `Retry` → `Idle`) and a [`crate::state::ThreadStore`] owns live
//! thread handles.
//!
//! Phase 1 ships the actor skeleton and the `StartTurn` / `CancelTurn`
//! happy path. Tool dispatch, the hook host, permission negotiation and
//! the provider fold are added by B5 / B6 / B7 / B10.
//!
//! [`EnginePhase`]: zhive_proto::hook::EnginePhase

pub mod event;
mod inner;
pub mod phase;
pub mod submission;

use std::sync::Arc;

use thiserror::Error;
use tokio::sync::{broadcast, mpsc};

use inner::EngineInner;

#[doc(inline)]
pub use event::EngineEvent;
#[doc(inline)]
pub use phase::allows_transition;
#[doc(inline)]
pub use submission::{PermissionRequestId, Submission};

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

/// Cheap, clonable handle to a running engine.
#[derive(Debug, Clone)]
pub struct Engine {
    submission_tx: mpsc::Sender<Submission>,
    events_tx: broadcast::Sender<EngineEvent>,
    threads: Arc<crate::state::ThreadStore>,
}

/// Reasons [`Engine::submit`] can fail.
#[derive(Debug, Error)]
pub enum SubmitError {
    /// The actor task has exited (typically after [`Submission::Shutdown`]).
    #[error("engine actor has stopped accepting submissions")]
    ActorStopped,
}

impl Engine {
    /// Spawns a fresh engine actor and returns a handle to it.
    ///
    /// The actor runs on the current Tokio runtime. Drop the last
    /// [`Engine`] clone to let the actor exit; sending
    /// [`Submission::Shutdown`] is the explicit shutdown path.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zhive_core::engine::{Engine, Submission};
    /// # async fn demo() {
    /// let engine = Engine::spawn();
    /// engine.submit(Submission::Shutdown).await.unwrap();
    /// # }
    /// ```
    #[must_use]
    pub fn spawn() -> Self {
        let (submission_tx, submission_rx) = mpsc::channel(SUBMISSION_CHANNEL_CAP);
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAP);
        let inner = EngineInner::new(events_tx.clone());
        let threads = Arc::clone(inner.threads());
        tokio::spawn(inner.run(submission_rx));
        Self {
            submission_tx,
            events_tx,
            threads,
        }
    }

    /// Hands a [`Submission`] to the actor.
    ///
    /// # Errors
    ///
    /// Returns [`SubmitError::ActorStopped`] when the actor task has
    /// already finished processing a [`Submission::Shutdown`].
    pub async fn submit(&self, sub: Submission) -> Result<(), SubmitError> {
        self.submission_tx
            .send(sub)
            .await
            .map_err(|_send_error| SubmitError::ActorStopped)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhive_proto::domain::ThreadId;

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    #[tokio::test]
    async fn start_turn_emits_started_then_completed() {
        let engine = Engine::spawn();
        let mut events = engine.subscribe();
        engine
            .submit(Submission::StartTurn {
                thread_id: tid("thread:native/a"),
                user_input: Vec::new(),
                scope: None,
            })
            .await
            .unwrap();

        let mut saw_started = false;
        let mut saw_completed = false;
        while !(saw_started && saw_completed) {
            match events.recv().await.unwrap() {
                EngineEvent::TurnStarted { .. } => saw_started = true,
                EngineEvent::TurnCompleted { .. } => saw_completed = true,
                _ => {}
            }
        }
        engine.submit(Submission::Shutdown).await.unwrap();
    }

    #[tokio::test]
    async fn cancel_turn_with_no_active_turn_is_noop() {
        let engine = Engine::spawn();
        // Cancel a thread that was never started; must not error.
        engine
            .submit(Submission::CancelTurn {
                thread_id: tid("thread:native/missing"),
            })
            .await
            .unwrap();
        engine.submit(Submission::Shutdown).await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_stops_actor() {
        let engine = Engine::spawn();
        engine.submit(Submission::Shutdown).await.unwrap();
        // Give the actor a tick to drain.
        tokio::task::yield_now().await;
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
        assert!(matches!(last, Err(SubmitError::ActorStopped)));
    }
}

// Rust guideline compliant 2026-02-21

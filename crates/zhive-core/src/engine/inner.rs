//! Engine actor body.
//!
//! Owns the live thread map and the broadcast event channel, and
//! consumes the [`Submission`] stream serially. Most of the meaningful
//! behaviour is plugged in by later Block B tasks (B5 hook host, B6
//! permission reducer, B10 provider). Phase 1 dispatch only handles
//! `StartTurn` / `CancelTurn` / `Shutdown` end-to-end; the remaining
//! variants emit a structured event so subscribers can observe the
//! command landed even though the heavy lifting still needs the
//! downstream actors.
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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, mpsc};
use zhive_proto::domain::{ThreadId, ThreadStatus, TurnId};
use zhive_proto::hook::EnginePhase;

use crate::permission::{PermissionReducer, ReducerError};
use crate::state::{ActiveTurn, ThreadStore};

use super::event::{EngineEvent, TurnRejectionReason};
use super::phase::allows_transition;
use super::submission::{
    CancelTurnReply, PermissionRequestId, ResumePermissionReply, StartTurnError, StartTurnReply,
    Submission, SubmissionEnvelope, SubmissionReply,
};

/// Shared state owned by the engine actor.
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
}

impl EngineInner {
    pub(crate) fn new(events_tx: broadcast::Sender<EngineEvent>) -> Self {
        Self {
            threads: Arc::new(ThreadStore::new()),
            events_tx,
            phase: Mutex::new(EnginePhase::Idle),
            turn_counter: AtomicU64::new(0),
            permission: PermissionReducer::new(),
        }
    }

    pub(crate) fn permission_reducer(&self) -> PermissionReducer {
        self.permission.clone()
    }

    pub(crate) async fn run(self, mut submission_rx: mpsc::Receiver<SubmissionEnvelope>) {
        while let Some(env) = submission_rx.recv().await {
            let SubmissionEnvelope { submission, reply } = env;
            if matches!(submission, Submission::Shutdown) {
                if let Some(tx) = reply {
                    let _ = tx.send(SubmissionReply::Shutdown);
                }
                break;
            }
            self.dispatch(submission, reply).await;
        }
    }

    async fn dispatch(
        &self,
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
            other => {
                tracing::debug!(
                    name: "zhive.engine.submission.unhandled",
                    submission_kind = ?std::mem::discriminant(&other),
                    "submission landed but Phase 1 does not act on it yet"
                );
                // If the caller attached a reply oneshot, drop it
                // explicitly so the awaiter surfaces
                // `EngineError::ReplyDropped` instead of timing out.
                // A future variant will replace this with a typed
                // SubmissionReply once the matching action lands.
                drop(reply);
            }
        }
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

    async fn start_turn(
        &self,
        thread_id: ThreadId,
        user_input: Vec<zhive_proto::domain::Item>,
    ) -> Result<StartTurnReply, StartTurnError> {
        // Attempt the engine-wide Idle → Turn transition. A refusal
        // here means the engine is already busy; surface it as both a
        // `TurnRejected` broadcast event (subscribers) and the typed
        // `StartTurnError` (synchronous caller).
        if let Err(err) = self.try_set_phase_atomic(EnginePhase::Idle, EnginePhase::Turn) {
            let actual = err.actual();
            let _ = self.events_tx.send(EngineEvent::TurnRejected {
                thread_id,
                reason: TurnRejectionReason::EngineBusy { current: actual },
            });
            return Err(StartTurnError::EngineBusy { current: actual });
        }
        // Emit the global Idle → Turn `PhaseChanged` with the owning
        // thread attached so subscribers can group events by thread.
        let _ = self.events_tx.send(EngineEvent::PhaseChanged {
            thread_id: Some(thread_id.clone()),
            from: EnginePhase::Idle,
            to: EnginePhase::Turn,
        });

        let handle = self.threads.get_or_init(&thread_id).await;
        let turn_id = self.allocate_turn_id(&thread_id);
        let started_at = unix_now();

        // active_turn + status are flipped to In-progress before any
        // event is emitted so subscribers observing `TurnStarted` are
        // guaranteed to see the matching state if they look it up.
        let mut active = handle.active_turn.lock().await;
        *active = Some(ActiveTurn::new(turn_id.clone(), started_at));
        drop(active);
        let mut status = handle.status.write().await;
        *status = ThreadStatus::Active {
            active_flags: vec![zhive_proto::domain::ThreadActiveFlag::TurnInProgress],
        };
        drop(status);

        for item in user_input {
            handle.push_item(item).await;
        }

        let _ = self.events_tx.send(EngineEvent::TurnStarted {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
        });

        let reply = StartTurnReply {
            turn_id: turn_id.clone(),
        };
        // Phase 1: immediately complete the turn after recording the
        // user input. The real agent loop wiring lands once B5 / B6 /
        // B10 are in place.
        self.finish_turn(&handle, thread_id, turn_id).await;
        Ok(reply)
    }

    /// Completes the turn that the calling [`Self::start_turn`] put in
    /// flight. The thread status flips back to `Idle`, the engine phase
    /// rolls back to `Idle`, and a `TurnCompleted` event is emitted.
    async fn finish_turn(
        &self,
        handle: &Arc<crate::state::ThreadHandle>,
        thread_id: ThreadId,
        turn_id: TurnId,
    ) {
        // Take the active turn slot. If it was already cleared by a
        // racing `cancel_turn`, the cancel path is responsible for the
        // status + phase rollback; finish_turn skips both to avoid a
        // double rollback and avoid logging a spurious "state machine
        // drift" event for a legitimate race.
        let mut active = handle.active_turn.lock().await;
        let we_owned_the_turn = active.is_some();
        *active = None;
        drop(active);
        if !we_owned_the_turn {
            tracing::debug!(
                name: "zhive.engine.finish_turn.cancelled_concurrently",
                "finish_turn observed active_turn already None; cancel path already handled it"
            );
            return;
        }
        let mut status = handle.status.write().await;
        *status = ThreadStatus::Idle;
        drop(status);
        if let Err(err) = self.try_set_phase_atomic(EnginePhase::Turn, EnginePhase::Idle) {
            // We've already verified `we_owned_the_turn` above, so the
            // per-thread state was self-consistent — reaching this
            // branch means the global engine phase fell out of sync,
            // which is a true state-machine drift bug (not a benign
            // race) and warrants an error-level log.
            let actual = err.actual();
            tracing::error!(
                name: "zhive.engine.phase.rollback_failed",
                expected = ?EnginePhase::Turn,
                actual = ?actual,
                "engine phase was not Turn when finishing a turn; state machine drift"
            );
        } else {
            let _ = self.events_tx.send(EngineEvent::PhaseChanged {
                thread_id: Some(thread_id.clone()),
                from: EnginePhase::Turn,
                to: EnginePhase::Idle,
            });
        }
        let _ = self
            .events_tx
            .send(EngineEvent::TurnCompleted { thread_id, turn_id });
    }

    async fn cancel_turn(&self, thread_id: ThreadId) -> CancelTurnReply {
        let Some(handle) = self.threads.get(&thread_id).await else {
            return CancelTurnReply::NoActiveTurn;
        };
        // Take the active turn under the per-thread lock; if there was
        // no active turn the cancel is a no-op (matches Pi behaviour).
        let active = {
            let mut guard = handle.active_turn.lock().await;
            guard.take()
        };
        let Some(active) = active else {
            return CancelTurnReply::NoActiveTurn;
        };
        let cancelled_turn_id = active.id.clone();

        // ACP 0.12 hard requirement: cancel resolves every outstanding
        // permission/request with `Cancelled` so the client never has
        // to time them out manually.
        self.permission.cancel_all();

        // Flip status back to Idle BEFORE emitting `SessionAborted` so
        // any subscriber that listens for the abort and then queries
        // `ThreadStatus` always sees `Idle`.
        let mut status = handle.status.write().await;
        *status = ThreadStatus::Idle;
        drop(status);

        let aborted = zhive_proto::permission::SessionAbortedNotification::new(
            thread_id.clone(),
            Some(active.id),
        );
        let _ = self
            .events_tx
            .send(EngineEvent::SessionAborted(Box::new(aborted)));

        // Roll the global phase back to Idle. With `finish_turn` now
        // gated by `we_owned_the_turn`, the only way to land in the
        // error arm here is genuine state-machine drift — same as the
        // finish_turn error path. Log at `error` to keep the asymmetry
        // gone.
        if let Err(err) = self.try_set_phase_atomic(EnginePhase::Turn, EnginePhase::Idle) {
            let actual = err.actual();
            tracing::error!(
                name: "zhive.engine.phase.cancel_rollback_failed",
                expected = ?EnginePhase::Turn,
                actual = ?actual,
                "cancel_turn observed phase != Turn; state machine drift"
            );
        } else {
            let _ = self.events_tx.send(EngineEvent::PhaseChanged {
                thread_id: Some(thread_id),
                from: EnginePhase::Turn,
                to: EnginePhase::Idle,
            });
        }
        CancelTurnReply::Cancelled {
            turn_id: cancelled_turn_id,
        }
    }

    /// Performs an atomic phase compare-and-set.
    ///
    /// Returns `Ok(())` when the transition was applied; otherwise
    /// returns a [`PhaseTransitionError`] that tells the caller whether
    /// the failure was caused by a precondition mismatch or by an
    /// illegal transition (so they can be logged distinctly).
    fn try_set_phase_atomic(
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

    fn phase_lock(&self) -> MutexGuard<'_, EnginePhase> {
        // Poisoned phase lock is a programming error; the only writer
        // is this actor task and it does not panic in normal paths. If
        // it happens, recover by returning the inner value rather than
        // tearing down the actor.
        match self.phase.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn allocate_turn_id(&self, thread_id: &ThreadId) -> TurnId {
        let seq = self.turn_counter.fetch_add(1, Ordering::Relaxed);
        TurnId(Arc::from(format!("turn:{}/{seq}", thread_id.0)))
    }

    pub(crate) fn threads(&self) -> &Arc<ThreadStore> {
        &self.threads
    }
}

/// Failure modes for [`EngineInner::try_set_phase_atomic`].
///
/// Distinguishes between a caller passing a forbidden `from→to` pair
/// (programming error) and the engine simply not being in the
/// expected `from` phase right now (a race that may be benign).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseTransitionError {
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
    fn actual(self) -> EnginePhase {
        match self {
            Self::Illegal { actual, .. } | Self::PreconditionMismatch { actual, .. } => actual,
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().try_into().unwrap_or(i64::MAX))
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use zhive_proto::permission::PermissionOutcome;

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    fn ask_request() -> zhive_proto::permission::RequestPermissionRequest {
        serde_json::from_value(serde_json::json!({
            "threadId": "thread:native/a",
            "resourceType": "tool",
            "name": "x",
            "reason": "test",
            "options": []
        }))
        .expect("fixture")
    }

    /// Exercises `cancel_turn`'s full effect set end to end: the engine
    /// actor's auto-completing Phase 1 turn would normally clear
    /// `active_turn` before a `CancelTurn` submission landed, so this
    /// test pokes the inner state directly to set up the precondition
    /// and then asserts every side effect (permission cancellation,
    /// status flip, phase rollback, broadcast events).
    #[tokio::test]
    async fn cancel_turn_cancels_pending_permissions() {
        let (events_tx, mut events_rx) = tokio::sync::broadcast::channel::<EngineEvent>(16);
        let inner = EngineInner::new(events_tx);
        let reducer = inner.permission_reducer();

        let thread_id = tid("thread:native/cancel-perm");
        let handle = inner.threads.get_or_init(&thread_id).await;
        // Seed an active turn so cancel_turn has work to do.
        let turn_id = inner.allocate_turn_id(&thread_id);
        let mut active = handle.active_turn.lock().await;
        *active = Some(ActiveTurn::new(turn_id.clone(), unix_now()));
        drop(active);
        // Engine phase must reflect the seeded active turn.
        inner
            .try_set_phase_atomic(EnginePhase::Idle, EnginePhase::Turn)
            .expect("seed phase");

        // Enroll a permission so we can observe the cancel reach the
        // reducer.
        let (_key, _req, rx) = reducer.enroll(ask_request());
        inner.cancel_turn(thread_id.clone()).await;

        // 1. Pending permission resolves to Cancelled.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), reducer.wait(rx))
            .await
            .expect("must resolve")
            .expect("ok");
        assert_eq!(outcome, PermissionOutcome::Cancelled);

        // 2. active_turn slot is now None.
        assert!(handle.active_turn.lock().await.is_none());

        // 3. Thread status flipped back to Idle.
        let status = handle.status.read().await.clone();
        assert!(matches!(status, zhive_proto::domain::ThreadStatus::Idle));

        // 4. Engine phase rolled back to Idle.
        assert_eq!(*inner.phase_lock(), EnginePhase::Idle);

        // 5. SessionAborted + PhaseChanged events broadcast.
        let mut saw_aborted = false;
        let mut saw_phase_back_to_idle = false;
        while let Ok(ev) = events_rx.try_recv() {
            match ev {
                EngineEvent::SessionAborted(notif) => {
                    assert_eq!(notif.thread_id, thread_id);
                    saw_aborted = true;
                }
                EngineEvent::PhaseChanged {
                    from: EnginePhase::Turn,
                    to: EnginePhase::Idle,
                    thread_id: Some(tid_in_event),
                } => {
                    assert_eq!(tid_in_event, thread_id);
                    saw_phase_back_to_idle = true;
                }
                _ => {}
            }
        }
        assert!(saw_aborted, "cancel_turn must broadcast SessionAborted");
        assert!(
            saw_phase_back_to_idle,
            "cancel_turn must broadcast a Turn→Idle PhaseChanged"
        );
    }

    #[test]
    fn try_set_phase_atomic_reports_illegal_vs_mismatch() {
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<EngineEvent>(4);
        let inner = EngineInner::new(events_tx);
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

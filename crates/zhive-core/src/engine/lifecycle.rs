//! Turn lifecycle methods for [`EngineInner`].
//!
//! Contains the three turn state-machine entry points — `start_turn`,
//! `finish_turn`, and `cancel_turn` — plus the private helpers that
//! only those methods use (`allocate_turn_id`, `unix_now`).
//!
//! All three methods were split from [`super::inner`] to keep each file
//! under the 600-line soft limit.  They remain part of the same logical
//! `impl EngineInner` block; Rust allows multiple `impl` blocks for a
//! single type across different files in the same module tree.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use zhive_proto::domain::{Thread, ThreadId, ThreadSource, ThreadStatus, TurnId, TurnStatus};
use zhive_proto::hook::EnginePhase;

use crate::persistence::writer::StorageWriteOp;
use crate::queues::QueueTarget;
use crate::state::ActiveTurn;

use super::event::{EngineEvent, TurnRejectionReason};
use super::inner::EngineInner;
use super::submission::{CancelTurnReply, StartTurnError, StartTurnReply};

// ============================================================
// Turn lifecycle
// ============================================================

impl EngineInner {
    /// Accepts a turn submission: transitions the phase, installs
    /// `ActiveTurn`, pushes user-input items, emits `TurnStarted`,
    /// then **spawns** the provider task and returns `StartTurnReply`
    /// immediately — without waiting for the provider to finish.
    ///
    /// The spawned task runs [`super::turn::run_turn`] and calls
    /// [`Self::finish_turn`] when it completes.
    pub(super) async fn start_turn(
        self: &Arc<Self>,
        thread_id: ThreadId,
        user_input: Vec<zhive_proto::domain::Item>,
    ) -> Result<StartTurnReply, StartTurnError> {
        // Attempt the engine-wide Idle → Turn transition. A refusal
        // here means the engine is already busy; surface it as both a
        // `TurnRejected` broadcast event (subscribers) and the typed
        // `StartTurnError` (synchronous caller).
        if let Err(err) = self.try_set_phase_atomic(EnginePhase::Idle, EnginePhase::Turn) {
            let actual = err.actual();
            let _ = self.events_tx().send(EngineEvent::TurnRejected {
                thread_id,
                reason: TurnRejectionReason::EngineBusy { current: actual },
            });
            return Err(StartTurnError::EngineBusy { current: actual });
        }
        // Emit the global Idle → Turn `PhaseChanged` with the owning
        // thread attached so subscribers can group events by thread.
        let _ = self.events_tx().send(EngineEvent::PhaseChanged {
            thread_id: Some(thread_id.clone()),
            from: EnginePhase::Idle,
            to: EnginePhase::Turn,
        });

        let handle = self.threads().get_or_init(&thread_id).await;
        let turn_id = self.allocate_turn_id(&thread_id);
        let started_at = unix_now();

        // Drain the NextTurn queue and prepend those items before user_input.
        //
        // NextTurn items survive `cancel_turn` (they are NOT cleared by
        // `abort()`).  At the start of the next turn, they are consumed and
        // prepended to the initial user input, seeding the LLM context with
        // any cross-abort continuations the client staged.
        //
        // Deferred note: pendingSessionWrites buffer/flush is NOT implemented
        // here; that flush belongs to persistence and lands in increment 5.
        let next_turn_seed: Vec<zhive_proto::domain::Item> = {
            let mut q = handle.injection_lock();
            q.drain(QueueTarget::NextTurn)
        };
        let mut full_input: Vec<zhive_proto::domain::Item> = next_turn_seed;
        full_input.extend(user_input);

        // Install the ActiveTurn (including its cancel token) and flip
        // the thread status to Active. Both happen before TurnStarted
        // is emitted so subscribers observing that event see a
        // consistent in-memory state.
        //
        // The per-turn cancel token is a child of the engine-wide root so
        // that `cancel_tree.cancel_all()` (called on shutdown) propagates
        // to every in-flight turn.  Firing the child (via `cancel_turn`)
        // does NOT affect the root or sibling turns.
        let cancel = {
            let mut active = handle.active_turn.lock().await;
            let turn_cancel = self.cancel_tree().child_for_turn();
            let active_turn = ActiveTurn::new_with_cancel(turn_id.clone(), started_at, turn_cancel);
            let cancel = active_turn.cancel.clone();
            *active = Some(active_turn);
            cancel
        };

        let mut status = handle.status.write().await;
        *status = ThreadStatus::Active {
            active_flags: vec![zhive_proto::domain::ThreadActiveFlag::TurnInProgress],
        };
        drop(status);

        // Push the combined input items (next-turn seeds + user_input) and
        // emit ItemAppended for each one.
        for item in full_input {
            handle.push_item(item.clone()).await;
            let _ = self.events_tx().send(EngineEvent::ItemAppended {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                item: Box::new(item),
            });
        }

        let _ = self.events_tx().send(EngineEvent::TurnStarted {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
        });

        // Enqueue persistence ops for the new thread and turn.
        // ThreadUpserted first (ensures the rollout header is written).
        // The persisted thread status reflects the live state: Active while the
        // turn is running.  Note: active_flags is serialised as the generic
        // "active" discriminant in state_db — the individual flags (e.g.
        // TurnInProgress) are intentionally not persisted because they are
        // transient and always correct in memory; on DB round-trip the flags
        // field is restored as an empty vec (see thread_status_from_str).
        // DELIBERATE: active_flags detail is not persisted to state.db.
        //
        // Derive the source from the live handle so that subagent (child)
        // threads are persisted with `ThreadSource::Subagent` rather than
        // being silently overwritten to `ThreadSource::User`.
        let source = thread_source_for_handle(&handle);
        let thread_snapshot = Thread {
            id: thread_id.clone(),
            session_id: None,
            forked_from: None,
            preview: String::new(),
            ephemeral: false,
            model_provider: "unknown".to_owned(),
            created_at: started_at,
            updated_at: started_at,
            status: ThreadStatus::Active {
                active_flags: vec![zhive_proto::domain::ThreadActiveFlag::TurnInProgress],
            },
            cwd: PathBuf::from("."),
            source,
            name: None,
            turns: vec![],
        };
        self.enqueue_storage_op(StorageWriteOp::ThreadUpserted(Box::new(thread_snapshot)));
        self.enqueue_storage_op(StorageWriteOp::TurnStarted {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            started_at,
        });

        // Build the reply before spawning so the borrow of `turn_id` is
        // still available here.
        let reply = StartTurnReply {
            turn_id: turn_id.clone(),
        };

        // Spawn the provider task. The actor returns to consuming
        // submissions immediately so CancelTurn can be processed
        // while the turn is in flight.
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            super::turn::run_turn(&inner, handle, thread_id, turn_id, cancel).await;
        });

        Ok(reply)
    }

    /// Completes the turn that `run_turn` put in flight.
    ///
    /// Thread status flips back to `Idle`, the engine phase rolls back to
    /// `Idle`, and a `TurnCompleted` event is emitted — unless `failed` is
    /// `true`, in which case `TurnCompleted` is suppressed because `TurnFailed`
    /// was already broadcast (a turn has exactly one terminal event).
    ///
    /// If `cancel_turn` already took the `active_turn` slot
    /// (`we_owned_the_turn == false`), this method is a no-op so there is
    /// no double rollback.
    ///
    /// Exposed as `pub(in crate::engine)` so [`super::turn::run_turn`] can
    /// call it from the spawned turn task.
    pub(in crate::engine) async fn finish_turn(
        &self,
        handle: &Arc<crate::state::ThreadHandle>,
        thread_id: ThreadId,
        turn_id: TurnId,
        failed: bool,
    ) {
        // Take the active turn slot only if it belongs to *this* turn.
        //
        // There are two cases where the slot no longer belongs to us:
        //
        // 1. `cancel_turn` raced us: it cleared `active_turn`, emitted
        //    `SessionAborted`, and rolled the engine phase back to Idle
        //    before we got here. `active` is `None`.
        //
        // 2. A stale `run_turn` task from a *cancelled* turn resumed after
        //    the actor had already processed a new `StartTurn`: `active` is
        //    `Some(new_turn)` where `new_turn.id != turn_id`. Clobbering that
        //    slot would destroy the new turn's state and produce a spurious
        //    `TurnCompleted` for an already-cancelled turn.
        //
        // Both cases are handled by the `map_or(false, |a| a.id == turn_id)`
        // guard: if the slot is absent or belongs to a different turn, this
        // method becomes a complete no-op and all rollback / event emission
        // responsibility rests with whoever owns (or owned) that slot.
        let mut active = handle.active_turn.lock().await;
        let we_owned_the_turn = active.as_ref().is_some_and(|a| a.id == turn_id);
        // Capture started_at before clearing the slot so we can compute duration.
        let turn_started_at = active.as_ref().map(|a| a.started_at);
        if we_owned_the_turn {
            *active = None;
        }
        drop(active);
        if !we_owned_the_turn {
            tracing::debug!(
                name: "zhive.engine.finish_turn.not_owner",
                ?turn_id,
                "finish_turn skipped: active_turn slot absent or belongs to a different turn"
            );
            return;
        }
        let mut status = handle.status.write().await;
        *status = ThreadStatus::Idle;
        drop(status);

        // Subagent (child) threads do NOT own a slot in the global
        // EnginePhase machine. The parent turn raised the phase to Turn
        // and will lower it when it finishes. Calling try_set_phase_atomic
        // here for a child thread would either (a) prematurely roll the
        // phase back to Idle while the parent turn is still running, or
        // (b) fire a PreconditionMismatch error when the engine is already
        // Idle (the test case where spawn is called after the parent
        // completes). Either outcome is wrong, so we skip the phase
        // rollback entirely for child threads.
        if handle.parent_thread_id.is_none() {
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
                let _ = self.events_tx().send(EngineEvent::PhaseChanged {
                    thread_id: Some(thread_id.clone()),
                    from: EnginePhase::Turn,
                    to: EnginePhase::Idle,
                });
            }
        }

        // Only emit TurnCompleted when the turn did not fail. A turn that
        // already broadcast TurnFailed must not also emit TurnCompleted —
        // each turn has exactly one terminal event.
        if !failed {
            let _ = self.events_tx().send(EngineEvent::TurnCompleted {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
            });
        }

        // Enqueue persistence TurnEnded op and flip the thread row back to Idle.
        // Ordering: TurnEnded first (triggers fsync save point), then
        // ThreadUpserted (updates the thread status column).
        let completed_at = unix_now();
        let duration_ms = turn_started_at.map(|s| (completed_at - s).saturating_mul(1_000));
        self.enqueue_storage_op(StorageWriteOp::TurnEnded {
            thread_id: thread_id.clone(),
            turn_id,
            status: if failed {
                TurnStatus::Failed
            } else {
                TurnStatus::Completed
            },
            error: None,
            completed_at,
            duration_ms,
        });
        // Update the persisted thread status to Idle now that the turn has ended.
        // Preserve the thread source so that subagent threads remain
        // `ThreadSource::Subagent` after their turn ends.
        let idle_source = thread_source_for_handle(handle);
        let idle_snapshot = Thread {
            id: thread_id.clone(),
            session_id: None,
            forked_from: None,
            preview: String::new(),
            ephemeral: false,
            model_provider: "unknown".to_owned(),
            created_at: completed_at,
            updated_at: completed_at,
            status: ThreadStatus::Idle,
            cwd: PathBuf::from("."),
            source: idle_source,
            name: None,
            turns: vec![],
        };
        self.enqueue_storage_op(StorageWriteOp::ThreadUpserted(Box::new(idle_snapshot)));
    }

    pub(super) async fn cancel_turn(&self, thread_id: ThreadId) -> CancelTurnReply {
        let Some(handle) = self.threads().get(&thread_id).await else {
            return CancelTurnReply::NoActiveTurn;
        };
        // Take the active turn under the per-thread lock; if there was
        // no active turn the cancel is a no-op (matches Pi behaviour).
        // Fire the cancel token BEFORE releasing the lock so the
        // `run_turn` select! wakes as soon as possible; ownership of
        // `active` means the cancellation is visible to run_turn's
        // `finish_turn` call (it will see `active_turn == None`).
        let active = {
            let mut guard = handle.active_turn.lock().await;
            let taken = guard.take();
            // Fire the per-turn token so the streaming task exits its
            // select! loop without waiting for the next stream item.
            if let Some(ref a) = taken {
                a.cancel.cancel();
            }
            taken
        };
        let Some(active) = active else {
            return CancelTurnReply::NoActiveTurn;
        };
        let cancelled_turn_id = active.id.clone();
        // Capture started_at before active.id is moved into SessionAbortedNotification.
        let turn_started_at = active.started_at;

        // ACP 0.12 hard requirement: cancel resolves every outstanding
        // permission/request with `Cancelled` so the client never has
        // to time them out manually.
        self.permission_reducer().cancel_all();

        // Drain and snapshot the injection queues so the SessionAborted
        // notification reports what was cleared / retained.  The NextTurn
        // queue is preserved (abort() does not clear it); steer and follow-up
        // are cleared.  This satisfies the B7 abort semantics from A3 §6.2.
        let abort_snap = handle.injection_lock().abort();

        // Flip status back to Idle BEFORE emitting `SessionAborted` so
        // any subscriber that listens for the abort and then queries
        // `ThreadStatus` always sees `Idle`.
        let mut status = handle.status.write().await;
        *status = ThreadStatus::Idle;
        drop(status);

        let mut aborted = zhive_proto::permission::SessionAbortedNotification::new(
            thread_id.clone(),
            Some(active.id),
        );
        aborted.cleared_steer = abort_snap.cleared_steer;
        aborted.cleared_follow_up = abort_snap.cleared_follow_up;
        aborted.next_turn_retained_count = abort_snap.next_turn_retained_count;

        let _ = self
            .events_tx()
            .send(EngineEvent::SessionAborted(Box::new(aborted)));

        // Persist the cancelled turn as Interrupted.  Only enqueued when
        // storage is configured (enqueue_storage_op is a no-op when the
        // sender is absent).  Must be enqueued BEFORE the phase rolls back
        // so the writer's ordering (JSONL → SQL) still sees the turn row as
        // active when it processes the TurnEnded op.
        let cancel_at = unix_now();
        // duration_ms: wall time from turn start to cancellation, in ms.
        // saturating_mul(1_000) converts seconds to milliseconds.
        let duration_ms = Some((cancel_at - turn_started_at).saturating_mul(1_000));
        self.enqueue_storage_op(StorageWriteOp::TurnEnded {
            thread_id: thread_id.clone(),
            turn_id: cancelled_turn_id.clone(),
            status: TurnStatus::Interrupted,
            error: None,
            completed_at: cancel_at,
            duration_ms,
        });
        // Flip the thread row back to Idle in the persistence index.
        // Preserve the thread source so that cancelled subagent turns remain
        // `ThreadSource::Subagent` rather than being overwritten to User.
        let cancel_idle_source = thread_source_for_handle(&handle);
        let idle_snapshot = Thread {
            id: thread_id.clone(),
            session_id: None,
            forked_from: None,
            preview: String::new(),
            ephemeral: false,
            model_provider: "unknown".to_owned(),
            created_at: cancel_at,
            updated_at: cancel_at,
            status: ThreadStatus::Idle,
            cwd: PathBuf::from("."),
            source: cancel_idle_source,
            name: None,
            turns: vec![],
        };
        self.enqueue_storage_op(StorageWriteOp::ThreadUpserted(Box::new(idle_snapshot)));

        // Subagent (child) threads do NOT own a slot in the global
        // EnginePhase machine — the parent turn raised the phase to Turn
        // and will lower it when it finishes. Calling try_set_phase_atomic
        // here for a child thread would prematurely set the phase to Idle
        // while the parent turn is still running, causing the parent's
        // eventual finish_turn to log a spurious 'state machine drift' error
        // and allowing a new start_turn to succeed while the engine is
        // actually still mid-turn (breaking the single-active-turn invariant).
        // Mirror the identical guard added to finish_turn at line 242.
        if handle.parent_thread_id.is_none() {
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
                let _ = self.events_tx().send(EngineEvent::PhaseChanged {
                    thread_id: Some(thread_id),
                    from: EnginePhase::Turn,
                    to: EnginePhase::Idle,
                });
            }
        }
        CancelTurnReply::Cancelled {
            turn_id: cancelled_turn_id,
        }
    }

    /// Allocates the next sequential [`TurnId`] for the given thread.
    fn allocate_turn_id(&self, thread_id: &ThreadId) -> TurnId {
        let seq = self.turn_counter().fetch_add(1, Ordering::Relaxed);
        TurnId(Arc::from(format!("turn:{}/{seq}", thread_id.0)))
    }
}

// ============================================================
// Helpers (used only by this module)
// ============================================================

/// Derives the [`ThreadSource`] from a [`ThreadHandle`] without copying data.
///
/// A handle whose `parent_thread_id` is `Some(_)` was spawned as a subagent
/// and must be persisted as [`ThreadSource::Subagent`]. Top-level threads
/// (no parent) are [`ThreadSource::User`].
///
/// This helper is called at every persistence snapshot site to ensure
/// subagent child threads are never silently downgraded to `User` on upsert.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::{ThreadId, ThreadSource};
/// use zhive_core::state::ThreadHandle;
///
/// let top_level = ThreadHandle::new_idle(ThreadId(Arc::from("thread:native/x")));
/// // thread_source_for_handle is pub(in crate::engine), not accessible here,
/// // but the logic is tested via the inc6b integration tests.
/// ```
fn thread_source_for_handle(handle: &crate::state::ThreadHandle) -> ThreadSource {
    if handle.parent_thread_id.is_some() {
        ThreadSource::Subagent
    } else {
        ThreadSource::User
    }
}

/// Returns the current time as seconds since the Unix epoch.
///
/// Saturates to `0` on clock errors and to [`i64::MAX`] on overflow.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().try_into().unwrap_or(i64::MAX))
}

/// Public re-export of [`unix_now`] for use in sibling modules.
///
/// The inner `unix_now` is private to this module; callers outside the
/// module (e.g. [`super::inner`]) use this thin wrapper.
pub(in crate::engine) fn unix_now_pub() -> i64 {
    unix_now()
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
    use zhive_proto::permission::PermissionOutcome;

    use crate::engine::event::EngineEvent;
    use crate::engine::inner::EngineInner;
    use crate::provider::DynLanguageModel;

    use super::unix_now;

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

    fn noop_provider() -> DynLanguageModel {
        use crate::provider::ScriptedModel;
        ScriptedModel::new("noop", "noop", vec![]).into_dyn()
    }

    fn new_inner() -> Arc<EngineInner> {
        let (events_tx, _) = broadcast::channel::<EngineEvent>(16);
        Arc::new(EngineInner::new(events_tx, noop_provider()))
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
        let inner = Arc::new(EngineInner::new(events_tx, noop_provider()));
        let reducer = inner.permission_reducer();

        let thread_id = tid("thread:native/cancel-perm");
        let handle = inner.threads().get_or_init(&thread_id).await;
        // Seed an active turn so cancel_turn has work to do.
        let turn_id = inner.allocate_turn_id(&thread_id);
        let mut active = handle.active_turn.lock().await;
        *active = Some(crate::state::ActiveTurn::new(turn_id.clone(), unix_now()));
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

    /// Regression guard: `cancel_turn` on a **child** (subagent) thread must
    /// NOT roll the global engine phase back to `Idle`, because the parent
    /// turn is still running and owns the `Turn` phase slot.
    ///
    /// Before the fix, the CAS at the end of `cancel_turn` would succeed and
    /// set the global phase to `Idle`, causing the parent's eventual
    /// `finish_turn` to log a spurious "state machine drift" error and allowing
    /// a new `start_turn` to succeed while the engine was still mid-turn.
    #[tokio::test]
    async fn cancel_child_turn_does_not_roll_back_global_phase() {
        let (events_tx, mut events_rx) = tokio::sync::broadcast::channel::<EngineEvent>(16);
        let inner = Arc::new(EngineInner::new(events_tx, noop_provider()));

        let parent_id = tid("thread:native/parent");
        let child_id = tid("thread:subagent/parent/0");

        // Register a *child* handle — it has parent_thread_id set.
        // `new_child` returns `(handle, rx)`; the receiver is unused in
        // this unit test (we only care about the phase-rollback behaviour).
        let (child_handle_inner, _rx) =
            crate::state::ThreadHandle::new_child(child_id.clone(), parent_id.clone());
        let child_handle = Arc::new(child_handle_inner);
        {
            let mut guard = inner.threads().write_guard().await;
            guard.insert(child_id.clone(), Arc::clone(&child_handle));
        };

        // Seed an active turn on the child.
        let child_turn_id = inner.allocate_turn_id(&child_id);
        {
            let mut active = child_handle.active_turn.lock().await;
            *active = Some(crate::state::ActiveTurn::new(
                child_turn_id.clone(),
                unix_now(),
            ));
        };

        // Simulate the parent turn: engine phase is Turn.
        inner
            .try_set_phase_atomic(EnginePhase::Idle, EnginePhase::Turn)
            .expect("seed phase to Turn");

        // Drain setup events.
        while events_rx.try_recv().is_ok() {}

        // Cancel the child turn — this must NOT affect the global phase.
        let reply = inner.cancel_turn(child_id.clone()).await;
        assert!(
            matches!(
                reply,
                crate::engine::submission::CancelTurnReply::Cancelled { .. }
            ),
            "cancel_turn should report Cancelled for the child"
        );

        // Global phase must still be Turn (parent is still running).
        assert_eq!(
            *inner.phase_lock(),
            EnginePhase::Turn,
            "cancel_turn on a child thread must not roll the global phase back to Idle"
        );

        // No Turn→Idle PhaseChanged event should have been emitted.
        let had_phase_idle = events_rx.try_recv().is_ok_and(|ev| {
            matches!(
                ev,
                EngineEvent::PhaseChanged {
                    from: EnginePhase::Turn,
                    to: EnginePhase::Idle,
                    ..
                }
            )
        });
        assert!(
            !had_phase_idle,
            "cancel_turn on a child thread must not emit a Turn→Idle PhaseChanged event"
        );
    }

    /// Verify that `ActiveTurn` carries a live cancellation token and
    /// that `cancel_turn` fires it before returning.
    #[tokio::test]
    async fn cancel_turn_fires_active_turn_cancel_token() {
        let inner = new_inner();
        let thread_id = tid("thread:native/cancel-token");
        let handle = inner.threads().get_or_init(&thread_id).await;

        let turn_id = inner.allocate_turn_id(&thread_id);
        let active_turn = crate::state::ActiveTurn::new(turn_id.clone(), unix_now());
        let cancel = active_turn.cancel.clone();
        assert!(!cancel.is_cancelled(), "token must start uncancelled");

        let mut guard = handle.active_turn.lock().await;
        *guard = Some(active_turn);
        drop(guard);
        inner
            .try_set_phase_atomic(EnginePhase::Idle, EnginePhase::Turn)
            .expect("seed phase");

        inner.cancel_turn(thread_id).await;
        assert!(
            cancel.is_cancelled(),
            "cancel_turn must fire the per-turn token"
        );
    }

    /// Regression test for the stale-turn clobber scenario:
    ///
    /// A cancelled turn's `run_turn` task may call `finish_turn` *after*
    /// `cancel_turn` has already cleared `active_turn` and a new
    /// `StartTurn` has installed a fresh `ActiveTurn` (different id).
    /// `finish_turn` must be a strict no-op in that case: it must not
    /// clear the new turn's slot, must not flip the engine phase back to
    /// Idle, and must not emit `TurnCompleted` for the old (cancelled) turn.
    #[tokio::test]
    async fn finish_turn_is_noop_when_active_turn_belongs_to_different_id() {
        let (events_tx, mut events_rx) = tokio::sync::broadcast::channel::<EngineEvent>(16);
        let inner = Arc::new(EngineInner::new(events_tx, noop_provider()));
        let thread_id = tid("thread:native/stale-finish");
        let handle = inner.threads().get_or_init(&thread_id).await;

        // Simulate the engine state after a cancel + new StartTurn:
        // Phase is Turn (new turn in flight), active_turn holds the NEW turn.
        let old_turn_id = inner.allocate_turn_id(&thread_id);
        let new_turn_id = inner.allocate_turn_id(&thread_id);

        let new_active = crate::state::ActiveTurn::new(new_turn_id.clone(), unix_now());
        {
            let mut guard = handle.active_turn.lock().await;
            *guard = Some(new_active);
        };
        inner
            .try_set_phase_atomic(EnginePhase::Idle, EnginePhase::Turn)
            .expect("seed phase to Turn");

        // Drain any buffered events from the setup above.
        while events_rx.try_recv().is_ok() {}

        // Now the old (stale) run_turn calls finish_turn with its own id.
        // This must be a complete no-op.
        inner
            .finish_turn(&handle, thread_id.clone(), old_turn_id, false)
            .await;

        // Engine phase must still be Turn (not rolled back to Idle).
        let phase = *inner.phase_lock();
        assert_eq!(
            phase,
            EnginePhase::Turn,
            "stale finish_turn must not roll engine phase back to Idle"
        );

        // active_turn slot must still hold the NEW turn.
        let active = handle.active_turn.lock().await;
        assert!(
            active.as_ref().is_some_and(|a| a.id == new_turn_id),
            "stale finish_turn must not clear the new turn's active_turn slot"
        );
        drop(active);

        // No TurnCompleted event must have been emitted.
        assert!(
            !matches!(events_rx.try_recv(), Ok(EngineEvent::TurnCompleted { .. })),
            "stale finish_turn must not emit TurnCompleted"
        );
    }
}

// Rust guideline compliant 2026-02-21

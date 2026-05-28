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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{RwLock, broadcast, mpsc};
use zhive_proto::domain::{ThreadId, ThreadStatus, TurnId};
use zhive_proto::hook::EnginePhase;

use crate::state::{ActiveTurn, ThreadStore};

use super::event::EngineEvent;
use super::phase::allows_transition;
use super::submission::Submission;

/// Shared state owned by the engine actor.
pub(crate) struct EngineInner {
    threads: Arc<ThreadStore>,
    events_tx: broadcast::Sender<EngineEvent>,
    phase: Arc<RwLock<EnginePhase>>,
    turn_counter: AtomicU64,
}

impl EngineInner {
    pub(crate) fn new(events_tx: broadcast::Sender<EngineEvent>) -> Self {
        Self {
            threads: Arc::new(ThreadStore::new()),
            events_tx,
            phase: Arc::new(RwLock::new(EnginePhase::Idle)),
            turn_counter: AtomicU64::new(0),
        }
    }

    pub(crate) async fn run(self, mut submission_rx: mpsc::Receiver<Submission>) {
        while let Some(sub) = submission_rx.recv().await {
            if matches!(sub, Submission::Shutdown) {
                break;
            }
            self.dispatch(sub).await;
        }
    }

    async fn dispatch(&self, sub: Submission) {
        // Other submissions (injection, resume permission, spawn
        // subagent) land in B5 / B6 / B7 / B8. Phase 1 records the
        // intent without acting on it. `Shutdown` is filtered out before
        // dispatch by [`Self::run`].
        match sub {
            Submission::StartTurn {
                thread_id,
                user_input,
                scope: _,
            } => self.start_turn(thread_id, user_input).await,
            Submission::CancelTurn { thread_id } => self.cancel_turn(thread_id).await,
            _ => {}
        }
    }

    async fn start_turn(&self, thread_id: ThreadId, user_input: Vec<zhive_proto::domain::Item>) {
        if !self
            .try_set_phase(None, EnginePhase::Idle, EnginePhase::Turn)
            .await
        {
            return;
        }

        let handle = self.threads.get_or_init(&thread_id).await;
        let turn_id = self.allocate_turn_id(&thread_id);
        let started_at = unix_now();

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

        // Phase 1: immediately complete the turn after recording the
        // user input. The real agent loop wiring lands once B5 / B6 /
        // B10 are in place.
        let mut active = handle.active_turn.lock().await;
        *active = None;
        drop(active);

        let mut status = handle.status.write().await;
        *status = ThreadStatus::Idle;
        drop(status);
        let _ = self
            .try_set_phase(None, EnginePhase::Turn, EnginePhase::Idle)
            .await;
        let _ = self
            .events_tx
            .send(EngineEvent::TurnCompleted { thread_id, turn_id });
    }

    async fn cancel_turn(&self, thread_id: ThreadId) {
        let Some(handle) = self.threads.get(&thread_id).await else {
            return;
        };
        let active = {
            let mut guard = handle.active_turn.lock().await;
            guard.take()
        };
        if let Some(active) = active {
            let _ = self.events_tx.send(EngineEvent::SessionAborted(Box::new(
                zhive_proto::permission::SessionAbortedNotification::new(
                    thread_id,
                    Some(active.id),
                ),
            )));
            let mut status = handle.status.write().await;
            *status = ThreadStatus::Idle;
            drop(status);
            let _ = self
                .try_set_phase(None, EnginePhase::Turn, EnginePhase::Idle)
                .await;
        }
    }

    async fn try_set_phase(
        &self,
        thread_id: Option<ThreadId>,
        from: EnginePhase,
        to: EnginePhase,
    ) -> bool {
        if !allows_transition(from, to) {
            return false;
        }
        let mut phase = self.phase.write().await;
        if *phase != from {
            return false;
        }
        *phase = to;
        drop(phase);
        let _ = self.events_tx.send(EngineEvent::PhaseChanged {
            thread_id,
            from,
            to,
        });
        true
    }

    fn allocate_turn_id(&self, thread_id: &ThreadId) -> TurnId {
        let seq = self.turn_counter.fetch_add(1, Ordering::Relaxed);
        TurnId(Arc::from(format!("turn:{}/{seq}", thread_id.0)))
    }

    pub(crate) fn threads(&self) -> &Arc<ThreadStore> {
        &self.threads
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().try_into().unwrap_or(i64::MAX))
}

// Rust guideline compliant 2026-02-21

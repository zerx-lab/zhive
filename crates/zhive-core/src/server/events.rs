//! Wire-form mapping for [`crate::engine::EngineEvent`].
//!
//! The engine fans events out over a [`tokio::sync::broadcast`]
//! channel; the server forwards each event as a JSON-RPC
//! [`Notification`] so connected clients can subscribe via a single
//! `events/*` method namespace. The mapping is intentionally lossy
//! where it has to be — the `Item` and `Request` payloads are
//! serialised verbatim through `serde_json::Value`.
//!
//! ## Method names
//!
//! | `EngineEvent` variant      | JSON-RPC method                |
//! |----------------------------|--------------------------------|
//! | `TurnStarted`              | `events/turn_started`          |
//! | `TurnRejected`             | `events/turn_rejected`         |
//! | `TurnCompleted`            | `events/turn_completed`        |
//! | `TurnFailed`               | `events/turn_failed`           |
//! | `ItemAppended`             | `events/item_appended`         |
//! | `ItemDelta`                | `events/item_delta`            |
//! | `PhaseChanged`             | `events/phase_changed`         |
//! | `SessionAborted`           | `events/session_aborted`       |
//! | `PermissionRequested`      | `events/permission_requested`  |
//! | `Usage`                    | `events/usage`                 |
//! | `TurnSuspended`            | `events/turn_suspended`        |
//! | `TurnResumed`              | `events/turn_resumed`          |
//! | `ThreadForked`             | `events/thread_forked`         |
//! | `SubagentStarted`          | `events/subagent_started`      |
//! | `SubagentCompleted`        | `events/subagent_completed`    |
//! | `CompactionStarted`        | `events/compaction_started`    |
//! | `CompactionDelta`          | `events/compaction_delta`      |
//! | `CompactionCompleted`      | `events/compaction_completed`  |
//! | `CompactionFailed`         | `events/compaction_failed`     |
//!
//! ## Per-connection filtering
//!
//! [`EventFilter`] controls which `events/*` notifications a connection
//! receives. The default (empty allowed-set) means **allow all**, so
//! connections that never call `events/subscribe` continue to receive
//! every notification (backward-compatible). A client calls
//! `events/subscribe` to restrict delivery to a named set of methods;
//! `events/unsubscribe` resets the filter back to allow-all.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use zhive_proto::Message;
use zhive_proto::Notification;
use zhive_proto::events::{
    CompactionCompletedPayload, CompactionDeltaPayload, CompactionFailedPayload,
    CompactionStartedPayload, ItemAppendedPayload, ItemDeltaPayload, PermissionRequestedPayload,
    PhaseChangedPayload, SubagentCompletedPayload, SubagentStartedPayload, ThreadForkedPayload,
    TurnCompletedPayload, TurnFailedPayload, TurnRejectedPayload, TurnRejectedReason,
    TurnStartedPayload, UsagePayload,
};
use zhive_proto::methods;
use zhive_proto::permission::{
    METHOD_TURN_RESUMED, METHOD_TURN_SUSPENDED, TurnResumedNotification, TurnSuspendedNotification,
};

use crate::engine::{EngineEvent, TurnRejectionReason};

/// Per-connection event filter controlling which `events/*` notifications are forwarded.
///
/// The filter is shared between the event-forwarder task (reads it) and the
/// connection-level `dispatch_message` handler (writes it via `events/subscribe`
/// and `events/unsubscribe`).
///
/// ## Default: allow-all
///
/// A freshly constructed `EventFilter` (via [`Default`] or [`EventFilter::new`])
/// has an empty allowed-method set, which is interpreted as **pass through
/// everything**. This ensures connections that never call `events/subscribe`
/// continue to receive all notifications — backward compatibility is preserved.
///
/// ## Filtering: allow-listed set
///
/// After a client calls `events/subscribe` with a non-empty `methods` list,
/// only notifications whose method appears in that list are forwarded.
/// Calling `events/unsubscribe` (with no arguments) resets the filter back to
/// allow-all.
///
/// # Examples
///
/// ```
/// use zhive_core::server::events::EventFilter;
///
/// // Default allows every method.
/// let f = EventFilter::default();
/// assert!(f.allows_method("events/turn_started"));
/// assert!(f.allows_method("events/phase_changed"));
///
/// // After subscribing to a specific set, only those pass.
/// let f = EventFilter::for_methods(["events/turn_started", "events/turn_completed"]);
/// assert!(f.allows_method("events/turn_started"));
/// assert!(!f.allows_method("events/phase_changed"));
///
/// // Resetting returns to allow-all.
/// let mut f = EventFilter::for_methods(["events/turn_started"]);
/// f.reset();
/// assert!(f.allows_method("events/phase_changed"));
/// ```
#[derive(Debug, Default, Clone)]
pub struct EventFilter {
    /// `None` ≡ allow all.  `Some(set)` ≡ allow only the listed methods.
    allowed: Option<HashSet<String>>,
}

impl EventFilter {
    /// Creates a new allow-all filter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a filter that allows only the given method names.
    ///
    /// Passing an empty iterator is treated equivalently to `new()` (allow-all).
    #[must_use]
    pub fn for_methods<I, S>(methods: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let set: HashSet<String> = methods.into_iter().map(Into::into).collect();
        if set.is_empty() {
            Self::default()
        } else {
            Self { allowed: Some(set) }
        }
    }

    /// Returns `true` when `method` should be forwarded to the connection.
    ///
    /// An allow-all filter (the default) always returns `true`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::server::events::EventFilter;
    ///
    /// let f = EventFilter::for_methods(["events/turn_started"]);
    /// assert!(f.allows_method("events/turn_started"));
    /// assert!(!f.allows_method("events/turn_completed"));
    /// ```
    #[must_use]
    pub fn allows_method(&self, method: &str) -> bool {
        match &self.allowed {
            None => true,
            Some(set) => set.contains(method),
        }
    }

    /// Replaces the allowed set; an empty `methods` slice resets to allow-all.
    pub fn set_methods<I, S>(&mut self, methods: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let set: HashSet<String> = methods.into_iter().map(Into::into).collect();
        if set.is_empty() {
            self.allowed = None;
        } else {
            self.allowed = Some(set);
        }
    }

    /// Resets the filter to allow-all (clears any subscription).
    pub fn reset(&mut self) {
        self.allowed = None;
    }
}

/// Thread-safe shared handle to a per-connection [`EventFilter`].
///
/// Created once per connection in `spawn_connection` and cloned into
/// both the event-forwarder task and the `dispatch_message` handler.
pub type SharedEventFilter = Arc<Mutex<EventFilter>>;

/// Returns the current time as seconds since the Unix epoch.
///
/// Used to stamp `events/turn_suspended` / `events/turn_resumed` notifications,
/// whose `EngineEvent` does not carry a timestamp. Saturates to `0` on clock
/// errors and to [`i64::MAX`] on overflow, so it never panics.
fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().try_into().unwrap_or(i64::MAX))
}

/// Converts an [`EngineEvent`] into a [`Notification`] ready to ship
/// over the wire.
///
/// Returns `None` for variants whose payload cannot be serialised
/// (currently unreachable but defensively coded so a future enum
/// variant cannot accidentally crash the forwarder task).
#[expect(
    clippy::too_many_lines,
    reason = "exhaustive match over EngineEvent; each arm is a trivial serialization — splitting would add indirection without clarity"
)]
#[must_use]
pub fn engine_event_to_notification(event: &EngineEvent) -> Option<Notification> {
    let (method, params) = match event {
        EngineEvent::TurnStarted { thread_id, turn_id } => (
            methods::EVENT_TURN_STARTED,
            serde_json::to_value(TurnStartedPayload::new(thread_id.clone(), turn_id.clone()))
                .ok()?,
        ),
        EngineEvent::TurnRejected { thread_id, reason } => {
            let TurnRejectionReason::EngineBusy { current } = reason;
            (
                methods::EVENT_TURN_REJECTED,
                serde_json::to_value(TurnRejectedPayload::new(
                    thread_id.clone(),
                    TurnRejectedReason::EngineBusy {
                        current_phase: *current,
                    },
                ))
                .ok()?,
            )
        }
        EngineEvent::TurnCompleted { thread_id, turn_id } => (
            methods::EVENT_TURN_COMPLETED,
            serde_json::to_value(TurnCompletedPayload::new(
                thread_id.clone(),
                turn_id.clone(),
            ))
            .ok()?,
        ),
        EngineEvent::TurnFailed {
            thread_id,
            turn_id,
            error,
        } => (
            methods::EVENT_TURN_FAILED,
            serde_json::to_value(TurnFailedPayload::new(
                thread_id.clone(),
                turn_id.clone(),
                error.clone(),
            ))
            .ok()?,
        ),
        EngineEvent::ItemAppended {
            thread_id,
            turn_id,
            item,
        } => (
            methods::EVENT_ITEM_APPENDED,
            serde_json::to_value(ItemAppendedPayload::new(
                thread_id.clone(),
                turn_id.clone(),
                item.id().clone(),
                (**item).clone(),
            ))
            .ok()?,
        ),
        EngineEvent::ItemDelta {
            thread_id,
            turn_id,
            delta,
            kind,
        } => (
            methods::EVENT_ITEM_DELTA,
            serde_json::to_value(
                ItemDeltaPayload::new(thread_id.clone(), turn_id.clone(), delta.clone())
                    .with_kind(*kind),
            )
            .ok()?,
        ),
        EngineEvent::PhaseChanged {
            thread_id,
            from,
            to,
        } => (
            methods::EVENT_PHASE_CHANGED,
            serde_json::to_value(PhaseChangedPayload::new(thread_id.clone(), *from, *to)).ok()?,
        ),
        EngineEvent::SessionAborted(notif) => (
            methods::EVENT_SESSION_ABORTED,
            serde_json::to_value(notif.as_ref()).ok()?,
        ),
        EngineEvent::PermissionRequested {
            request_id,
            request,
        } => (
            methods::EVENT_PERMISSION_REQUESTED,
            serde_json::to_value(PermissionRequestedPayload::new(
                request_id.0.to_string(),
                (**request).clone(),
            ))
            .ok()?,
        ),
        EngineEvent::Usage {
            thread_id,
            turn_id,
            input_tokens,
            output_tokens,
        } => (
            methods::EVENT_USAGE,
            serde_json::to_value(UsagePayload::new(
                thread_id.clone(),
                turn_id.clone(),
                *input_tokens,
                *output_tokens,
            ))
            .ok()?,
        ),
        EngineEvent::TurnSuspended {
            thread_id,
            turn_id,
            request_id,
            reason,
        } => (
            METHOD_TURN_SUSPENDED,
            serde_json::to_value(TurnSuspendedNotification::new(
                thread_id.clone(),
                turn_id.clone(),
                request_id.0.as_ref(),
                reason.clone(),
                unix_now_secs(),
            ))
            .ok()?,
        ),
        EngineEvent::TurnResumed { thread_id, turn_id } => (
            METHOD_TURN_RESUMED,
            serde_json::to_value(TurnResumedNotification::new(
                thread_id.clone(),
                turn_id.clone(),
                unix_now_secs(),
            ))
            .ok()?,
        ),
        EngineEvent::ThreadForked {
            source_thread_id,
            new_thread_id,
            forked_from_item,
        } => (
            methods::EVENT_THREAD_FORKED,
            serde_json::to_value(ThreadForkedPayload::new(
                source_thread_id.clone(),
                new_thread_id.clone(),
                forked_from_item.clone(),
            ))
            .ok()?,
        ),
        EngineEvent::SubagentStarted {
            parent_thread_id,
            child_thread_id,
            agent_type,
            description,
        } => (
            methods::EVENT_SUBAGENT_STARTED,
            serde_json::to_value(SubagentStartedPayload::new(
                parent_thread_id.clone(),
                child_thread_id.clone(),
                agent_type.clone(),
                description.clone(),
            ))
            .ok()?,
        ),
        EngineEvent::SubagentCompleted {
            parent_thread_id,
            child_thread_id,
            final_message,
        } => (
            methods::EVENT_SUBAGENT_COMPLETED,
            serde_json::to_value(SubagentCompletedPayload::new(
                parent_thread_id.clone(),
                child_thread_id.clone(),
                final_message.is_some(),
            ))
            .ok()?,
        ),
        EngineEvent::CompactionStarted {
            thread_id,
            trigger,
            entries,
        } => (
            methods::EVENT_COMPACTION_STARTED,
            serde_json::to_value(CompactionStartedPayload::new(
                thread_id.clone(),
                *trigger,
                *entries,
            ))
            .ok()?,
        ),
        EngineEvent::CompactionDelta { thread_id, delta } => (
            methods::EVENT_COMPACTION_DELTA,
            serde_json::to_value(CompactionDeltaPayload::new(
                thread_id.clone(),
                delta.clone(),
            ))
            .ok()?,
        ),
        EngineEvent::CompactionCompleted {
            thread_id,
            entries_compacted,
        } => (
            methods::EVENT_COMPACTION_COMPLETED,
            serde_json::to_value(CompactionCompletedPayload::new(
                thread_id.clone(),
                *entries_compacted,
            ))
            .ok()?,
        ),
        EngineEvent::CompactionFailed { thread_id, reason } => (
            methods::EVENT_COMPACTION_FAILED,
            serde_json::to_value(CompactionFailedPayload::new(
                thread_id.clone(),
                reason.clone(),
            ))
            .ok()?,
        ),
        // Internal engine events suppressed from the wire stream in Phase 1.
        //
        // SavePoint is a persistence marker (deferred session writes flushed);
        // clients observe durable completion through TurnCompleted. Restored is
        // delivered to the caller through the `engine/restore` RPC reply (which
        // carries the new branch thread id); a dedicated wire notification for
        // other observers is deferred. Returning `None` skips them on the wire.
        EngineEvent::SavePoint { .. } | EngineEvent::Restored { .. } => return None,
    };
    Some(Notification::new(method, Some(params)))
}

/// Spawns a forwarder task that pumps engine events into a serve-loop
/// outbound queue.
///
/// The task subscribes to the engine via `events_rx`, encodes each
/// event with [`engine_event_to_notification`], applies the per-connection
/// `filter`, and pushes allowed notifications into `outbound_tx`. The task
/// exits when `shutdown` fires, when the outbound channel is closed, or when
/// the upstream broadcast closes.
///
/// The `filter` is the same [`SharedEventFilter`] owned by the connection's
/// `dispatch_message` handler, so subscribe/unsubscribe control messages take
/// effect immediately without any restart of this task.
///
/// A lagged broadcast (subscriber too slow) is logged at `warn` and
/// the task continues; an oversize subscriber will see resync points
/// from later events.
pub fn spawn_event_forwarder(
    mut events_rx: broadcast::Receiver<EngineEvent>,
    outbound_tx: mpsc::Sender<Message>,
    filter: SharedEventFilter,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            let next = tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                ev = events_rx.recv() => ev,
            };
            match next {
                Ok(event) => {
                    let Some(notif) = engine_event_to_notification(&event) else {
                        continue;
                    };
                    // Check the filter BEFORE awaiting the send so the
                    // MutexGuard is never held across an await point
                    // (which would make the future !Send).
                    let allowed = {
                        let guard = filter
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        guard.allows_method(&notif.method)
                    };
                    if !allowed {
                        continue;
                    }
                    if outbound_tx
                        .send(Message::Notification(notif))
                        .await
                        .is_err()
                    {
                        // Outbound channel closed; consumer has gone
                        // away. Stop forwarding.
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(
                        name: "zhive.events.forwarder.lagged",
                        missed,
                        "event forwarder fell behind the engine broadcast"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhive_proto::domain::{Item, ThreadId};

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    fn turn_id(s: &str) -> zhive_proto::domain::TurnId {
        zhive_proto::domain::TurnId(Arc::from(s))
    }

    #[test]
    fn turn_started_maps_to_events_turn_started() {
        let ev = EngineEvent::TurnStarted {
            thread_id: tid("thread:native/a"),
            turn_id: turn_id("turn:thread:native/a/0"),
        };
        let n = engine_event_to_notification(&ev).unwrap();
        assert_eq!(n.method, "events/turn_started");
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["threadId"], "thread:native/a");
        assert_eq!(p["turnId"], "turn:thread:native/a/0");
    }

    #[test]
    fn phase_changed_with_thread_includes_field() {
        use zhive_proto::hook::EnginePhase;
        let ev = EngineEvent::PhaseChanged {
            thread_id: Some(tid("thread:native/p")),
            from: EnginePhase::Idle,
            to: EnginePhase::Turn,
        };
        let n = engine_event_to_notification(&ev).unwrap();
        assert_eq!(n.method, "events/phase_changed");
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["threadId"], "thread:native/p");
        assert_eq!(p["from"], "idle");
        assert_eq!(p["to"], "turn");
    }

    #[test]
    fn phase_changed_without_thread_omits_field() {
        use zhive_proto::hook::EnginePhase;
        let ev = EngineEvent::PhaseChanged {
            thread_id: None,
            from: EnginePhase::Idle,
            to: EnginePhase::Compaction,
        };
        let n = engine_event_to_notification(&ev).unwrap();
        let p = n.params.as_ref().unwrap();
        assert!(p.get("threadId").is_none());
        assert_eq!(p["to"], "compaction");
    }

    #[test]
    fn turn_rejected_carries_engine_busy_reason() {
        use zhive_proto::hook::EnginePhase;
        let ev = EngineEvent::TurnRejected {
            thread_id: tid("t"),
            reason: TurnRejectionReason::EngineBusy {
                current: EnginePhase::Compaction,
            },
        };
        let n = engine_event_to_notification(&ev).unwrap();
        assert_eq!(n.method, "events/turn_rejected");
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["reason"]["kind"], "engine_busy");
        assert_eq!(p["reason"]["currentPhase"], "compaction");
    }

    #[test]
    fn item_appended_includes_item_id_top_level() {
        use zhive_proto::domain::ItemId;
        let item = Item::AgentMessage {
            id: ItemId(Arc::from("item:0")),
            text: "hi".into(),
        };
        let ev = EngineEvent::ItemAppended {
            thread_id: tid("t"),
            turn_id: turn_id("turn:t/0"),
            item: Box::new(item),
        };
        let n = engine_event_to_notification(&ev).unwrap();
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["itemId"], "item:0");
        assert!(p["item"].is_object());
    }

    #[test]
    fn compaction_started_maps_to_events_compaction_started() {
        use zhive_proto::hook::CompactTrigger;
        let ev = EngineEvent::CompactionStarted {
            thread_id: tid("thread:native/c"),
            trigger: CompactTrigger::Manual,
            entries: 9,
        };
        let n = engine_event_to_notification(&ev).unwrap();
        assert_eq!(n.method, "events/compaction_started");
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["threadId"], "thread:native/c");
        assert_eq!(p["trigger"], "manual");
        assert_eq!(p["entries"], 9u32);
    }

    #[test]
    fn compaction_delta_carries_text() {
        let ev = EngineEvent::CompactionDelta {
            thread_id: tid("t"),
            delta: "frag".into(),
        };
        let n = engine_event_to_notification(&ev).unwrap();
        assert_eq!(n.method, "events/compaction_delta");
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["delta"], "frag");
    }

    #[test]
    fn compaction_completed_carries_entry_count() {
        let ev = EngineEvent::CompactionCompleted {
            thread_id: tid("t"),
            entries_compacted: 12,
        };
        let n = engine_event_to_notification(&ev).unwrap();
        assert_eq!(n.method, "events/compaction_completed");
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["entriesCompacted"], 12u32);
    }

    #[test]
    fn compaction_failed_carries_reason() {
        let ev = EngineEvent::CompactionFailed {
            thread_id: tid("t"),
            reason: "provider exploded".into(),
        };
        let n = engine_event_to_notification(&ev).unwrap();
        assert_eq!(n.method, "events/compaction_failed");
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["reason"], "provider exploded");
    }

    #[test]
    fn turn_suspended_maps_to_events_turn_suspended() {
        use crate::engine::PermissionRequestId;
        let ev = EngineEvent::TurnSuspended {
            thread_id: tid("thread:native/s"),
            turn_id: turn_id("turn:thread:native/s/0"),
            request_id: PermissionRequestId(Arc::from("perm:3")),
            reason: Some("awaiting user".into()),
        };
        let n = engine_event_to_notification(&ev).unwrap();
        assert_eq!(n.method, "events/turn_suspended");
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["threadId"], "thread:native/s");
        assert_eq!(p["turnId"], "turn:thread:native/s/0");
        assert_eq!(p["requestId"], "perm:3");
        assert_eq!(p["reason"], "awaiting user");
        assert!(p["suspendedAt"].is_i64());
    }

    #[test]
    fn turn_resumed_maps_to_events_turn_resumed() {
        let ev = EngineEvent::TurnResumed {
            thread_id: tid("thread:native/r"),
            turn_id: turn_id("turn:thread:native/r/0"),
        };
        let n = engine_event_to_notification(&ev).unwrap();
        assert_eq!(n.method, "events/turn_resumed");
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["threadId"], "thread:native/r");
        assert_eq!(p["turnId"], "turn:thread:native/r/0");
        assert!(p["resumedAt"].is_i64());
    }

    #[test]
    fn thread_forked_maps_to_events_thread_forked() {
        use zhive_proto::domain::ItemId;
        let ev = EngineEvent::ThreadForked {
            source_thread_id: tid("thread:native/src"),
            new_thread_id: tid("thread:native/fork/src/3"),
            forked_from_item: Some(ItemId(Arc::from("item:src/1"))),
        };
        let n = engine_event_to_notification(&ev).unwrap();
        assert_eq!(n.method, "events/thread_forked");
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["sourceThreadId"], "thread:native/src");
        assert_eq!(p["newThreadId"], "thread:native/fork/src/3");
        assert_eq!(p["forkedFromItem"], "item:src/1");
    }

    #[test]
    fn thread_forked_full_history_omits_item() {
        let ev = EngineEvent::ThreadForked {
            source_thread_id: tid("thread:native/src"),
            new_thread_id: tid("thread:native/fork/src/4"),
            forked_from_item: None,
        };
        let n = engine_event_to_notification(&ev).unwrap();
        let p = n.params.as_ref().unwrap();
        assert!(p.get("forkedFromItem").is_none());
    }

    #[test]
    fn subagent_started_maps_to_events_subagent_started() {
        let ev = EngineEvent::SubagentStarted {
            parent_thread_id: tid("thread:native/parent"),
            child_thread_id: tid("thread:subagent/native/parent/0"),
            agent_type: Some("scout".into()),
            description: Some("read-only scout".into()),
        };
        let n = engine_event_to_notification(&ev).unwrap();
        assert_eq!(n.method, "events/subagent_started");
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["parentThreadId"], "thread:native/parent");
        assert_eq!(p["childThreadId"], "thread:subagent/native/parent/0");
        assert_eq!(p["agentType"], "scout");
        assert_eq!(p["description"], "read-only scout");
    }

    #[test]
    fn subagent_started_omits_absent_optional_fields() {
        let ev = EngineEvent::SubagentStarted {
            parent_thread_id: tid("thread:native/parent"),
            child_thread_id: tid("thread:subagent/native/parent/1"),
            agent_type: None,
            description: None,
        };
        let n = engine_event_to_notification(&ev).unwrap();
        let p = n.params.as_ref().unwrap();
        assert!(p.get("agentType").is_none());
        assert!(p.get("description").is_none());
    }

    #[test]
    fn subagent_completed_maps_to_events_subagent_completed() {
        use zhive_proto::domain::ItemId;
        let ev = EngineEvent::SubagentCompleted {
            parent_thread_id: tid("thread:native/parent"),
            child_thread_id: tid("thread:subagent/native/parent/2"),
            final_message: Some(Arc::new(Item::AgentMessage {
                id: ItemId(Arc::from("item:0")),
                text: "done".into(),
            })),
        };
        let n = engine_event_to_notification(&ev).unwrap();
        assert_eq!(n.method, "events/subagent_completed");
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["parentThreadId"], "thread:native/parent");
        assert_eq!(p["childThreadId"], "thread:subagent/native/parent/2");
        assert_eq!(p["hasFinalMessage"], true);
    }

    #[test]
    fn subagent_completed_without_final_message_reports_false() {
        let ev = EngineEvent::SubagentCompleted {
            parent_thread_id: tid("thread:native/parent"),
            child_thread_id: tid("thread:subagent/native/parent/3"),
            final_message: None,
        };
        let n = engine_event_to_notification(&ev).unwrap();
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["hasFinalMessage"], false);
    }

    #[test]
    fn usage_maps_to_events_usage_with_camel_case_fields() {
        let ev = EngineEvent::Usage {
            thread_id: tid("thread:native/u"),
            turn_id: turn_id("turn:thread:native/u/0"),
            input_tokens: 200,
            output_tokens: 50,
        };
        let n = engine_event_to_notification(&ev).unwrap();
        assert_eq!(n.method, "events/usage");
        let p = n.params.as_ref().unwrap();
        assert_eq!(p["threadId"], "thread:native/u");
        assert_eq!(p["turnId"], "turn:thread:native/u/0");
        assert_eq!(p["inputTokens"], 200u64);
        assert_eq!(p["outputTokens"], 50u64);
    }
}

// Rust guideline compliant 2026-02-21

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

use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use zhive_proto::Message;
use zhive_proto::Notification;
use zhive_proto::domain::{ItemId, ThreadId, TurnError, TurnId};
use zhive_proto::hook::EnginePhase;

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

/// Wire-form payload for [`EngineEvent::Usage`].
///
/// Carries the token counts reported by one provider call, identified by
/// the owning thread and turn so clients can aggregate across iterations.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UsagePayload<'a> {
    thread_id: &'a ThreadId,
    turn_id: &'a TurnId,
    input_tokens: u64,
    output_tokens: u64,
}

/// Wire-form payload for [`EngineEvent::TurnStarted`].
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnStartedPayload<'a> {
    thread_id: &'a ThreadId,
    turn_id: &'a TurnId,
}

/// Wire-form payload for [`EngineEvent::TurnRejected`].
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnRejectedPayload<'a> {
    thread_id: &'a ThreadId,
    reason: TurnRejectedReason,
}

/// Mirrors [`TurnRejectionReason`] on the wire.
#[derive(Debug, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
enum TurnRejectedReason {
    EngineBusy { current_phase: EnginePhase },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnCompletedPayload<'a> {
    thread_id: &'a ThreadId,
    turn_id: &'a TurnId,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TurnFailedPayload<'a> {
    thread_id: &'a ThreadId,
    turn_id: &'a TurnId,
    error: &'a TurnError,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemAppendedPayload<'a> {
    thread_id: &'a ThreadId,
    turn_id: &'a TurnId,
    /// Pulled out so subscribers can index by item id without
    /// re-deriving it from the embedded item.
    item_id: &'a ItemId,
    /// The full item payload (serialised verbatim).
    item: &'a zhive_proto::domain::Item,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemDeltaPayload<'a> {
    thread_id: &'a ThreadId,
    turn_id: &'a TurnId,
    delta: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PhaseChangedPayload<'a> {
    /// Optional thread id; `None` for engine-global transitions.
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<&'a ThreadId>,
    from: EnginePhase,
    to: EnginePhase,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PermissionRequestedPayload<'a> {
    request_id: &'a str,
    request: &'a zhive_proto::permission::RequestPermissionRequest,
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
            "events/turn_started",
            serde_json::to_value(TurnStartedPayload { thread_id, turn_id }).ok()?,
        ),
        EngineEvent::TurnRejected { thread_id, reason } => {
            let TurnRejectionReason::EngineBusy { current } = reason;
            (
                "events/turn_rejected",
                serde_json::to_value(TurnRejectedPayload {
                    thread_id,
                    reason: TurnRejectedReason::EngineBusy {
                        current_phase: *current,
                    },
                })
                .ok()?,
            )
        }
        EngineEvent::TurnCompleted { thread_id, turn_id } => (
            "events/turn_completed",
            serde_json::to_value(TurnCompletedPayload { thread_id, turn_id }).ok()?,
        ),
        EngineEvent::TurnFailed {
            thread_id,
            turn_id,
            error,
        } => (
            "events/turn_failed",
            serde_json::to_value(TurnFailedPayload {
                thread_id,
                turn_id,
                error,
            })
            .ok()?,
        ),
        EngineEvent::ItemAppended {
            thread_id,
            turn_id,
            item,
        } => (
            "events/item_appended",
            serde_json::to_value(ItemAppendedPayload {
                thread_id,
                turn_id,
                item_id: item.id(),
                item,
            })
            .ok()?,
        ),
        EngineEvent::ItemDelta {
            thread_id,
            turn_id,
            delta,
        } => (
            "events/item_delta",
            serde_json::to_value(ItemDeltaPayload {
                thread_id,
                turn_id,
                delta,
            })
            .ok()?,
        ),
        EngineEvent::PhaseChanged {
            thread_id,
            from,
            to,
        } => (
            "events/phase_changed",
            serde_json::to_value(PhaseChangedPayload {
                thread_id: thread_id.as_ref(),
                from: *from,
                to: *to,
            })
            .ok()?,
        ),
        EngineEvent::SessionAborted(notif) => (
            "events/session_aborted",
            serde_json::to_value(notif.as_ref()).ok()?,
        ),
        EngineEvent::PermissionRequested {
            request_id,
            request,
        } => (
            "events/permission_requested",
            serde_json::to_value(PermissionRequestedPayload {
                request_id: &request_id.0,
                request: request.as_ref(),
            })
            .ok()?,
        ),
        EngineEvent::Usage {
            thread_id,
            turn_id,
            input_tokens,
            output_tokens,
        } => (
            "events/usage",
            serde_json::to_value(UsagePayload {
                thread_id,
                turn_id,
                input_tokens: *input_tokens,
                output_tokens: *output_tokens,
            })
            .ok()?,
        ),
        // SubagentCompleted is an internal engine event. It is
        // suppressed from the wire notification stream in Phase 1 —
        // external clients observe subagent outcomes via ItemAppended
        // events on the child thread rather than a dedicated wire type.
        // Returning `None` causes the forwarder to silently skip it.
        EngineEvent::SubagentCompleted { .. } => return None,
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
    use zhive_proto::domain::Item;

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    fn turn_id(s: &str) -> TurnId {
        TurnId(Arc::from(s))
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

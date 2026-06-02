//! Decoding of engine push-notifications into a typed event the UI folds over.
//!
//! The engine forwards every domain event as a JSON-RPC *notification* (no id)
//! under the `events/*` method namespace; see `zhive-core`
//! `engine_event_to_notification`. Those wire payloads are distinct from the
//! same-named `zhive_proto::domain` notification structs (e.g. wire
//! `events/turn_started` is `{threadId, turnId}`, not the proto
//! `TurnStartedNotification` which embeds a whole `Turn`), so this module
//! deserializes the *actual* wire shapes into [`EngineNotification`].

use serde::Deserialize;
use serde_json::Value;
use zhive_proto::domain::{Item, ThreadId, TurnError, TurnId};
use zhive_proto::hook::EnginePhase;
use zhive_proto::permission::{RequestPermissionRequest, SessionAbortedNotification};

/// A decoded engine notification, ready for the conversation reducer.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EngineNotification {
    /// A turn entered the in-progress state.
    TurnStarted {
        /// Owning thread.
        thread_id: ThreadId,
        /// The turn that started.
        turn_id: TurnId,
    },
    /// A turn completed normally.
    TurnCompleted {
        /// Owning thread.
        thread_id: ThreadId,
        /// The turn that completed.
        turn_id: TurnId,
    },
    /// A turn failed; carries the engine's error detail.
    TurnFailed {
        /// Owning thread.
        thread_id: ThreadId,
        /// The turn that failed.
        turn_id: TurnId,
        /// Failure detail.
        error: TurnError,
    },
    /// `start_turn` was refused before a turn ran (engine busy).
    TurnRejected {
        /// Human-readable rejection reason.
        reason: String,
    },
    /// An item was appended to a turn's transcript (the main render driver).
    ItemAppended {
        /// Owning thread.
        thread_id: ThreadId,
        /// Owning turn.
        turn_id: TurnId,
        /// The appended item (boxed: `Item` is a large enum).
        item: Box<Item>,
    },
    /// A streamed text fragment for the active turn (token-by-token output).
    ItemDelta {
        /// Owning thread.
        thread_id: ThreadId,
        /// Owning turn.
        turn_id: TurnId,
        /// The incremental text fragment.
        delta: String,
    },
    /// The engine phase machine transitioned.
    PhaseChanged {
        /// Owning thread, if the transition was thread-driven.
        thread_id: Option<String>,
        /// Phase transitioned from.
        from: EnginePhase,
        /// Phase transitioned to.
        to: EnginePhase,
    },
    /// A session was aborted (e.g. after a cancel).
    SessionAborted(Box<SessionAbortedNotification>),
    /// The engine needs a permission decision; reply via `engine/resume_permission`.
    PermissionRequested {
        /// Opaque request id to echo back when resolving.
        request_id: String,
        /// The permission prompt to render.
        request: Box<RequestPermissionRequest>,
    },
    /// A recognized-but-unmodeled or unknown notification method.
    Unhandled {
        /// The notification method string.
        method: String,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadTurn {
    thread_id: ThreadId,
    turn_id: TurnId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnFailedPayload {
    thread_id: ThreadId,
    turn_id: TurnId,
    error: TurnError,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemAppendedPayload {
    thread_id: ThreadId,
    turn_id: TurnId,
    item: Item,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemDeltaPayload {
    thread_id: ThreadId,
    turn_id: TurnId,
    delta: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PhaseChangedPayload {
    #[serde(default)]
    thread_id: Option<String>,
    from: EnginePhase,
    to: EnginePhase,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionRequestedPayload {
    request_id: String,
    request: RequestPermissionRequest,
}

/// Decodes a wire notification `(method, params)` into an [`EngineNotification`].
///
/// Unknown methods and payloads that fail to deserialize both collapse to
/// [`EngineNotification::Unhandled`] so a single malformed event can never
/// crash the render loop.
///
/// # Examples
///
/// ```
/// use zhive_tui::protocol::{decode, EngineNotification};
/// let params = serde_json::json!({ "threadId": "thread:native/x", "turnId": "turn:1/0" });
/// let n = decode("events/turn_started", Some(params));
/// assert!(matches!(n, EngineNotification::TurnStarted { .. }));
/// ```
#[must_use]
pub fn decode(method: &str, params: Option<Value>) -> EngineNotification {
    let params = params.unwrap_or(Value::Null);
    let unhandled = || EngineNotification::Unhandled {
        method: method.to_owned(),
    };
    match method {
        "events/turn_started" => match serde_json::from_value::<ThreadTurn>(params) {
            Ok(p) => EngineNotification::TurnStarted {
                thread_id: p.thread_id,
                turn_id: p.turn_id,
            },
            Err(_) => unhandled(),
        },
        "events/turn_completed" => match serde_json::from_value::<ThreadTurn>(params) {
            Ok(p) => EngineNotification::TurnCompleted {
                thread_id: p.thread_id,
                turn_id: p.turn_id,
            },
            Err(_) => unhandled(),
        },
        "events/turn_failed" => match serde_json::from_value::<TurnFailedPayload>(params) {
            Ok(p) => EngineNotification::TurnFailed {
                thread_id: p.thread_id,
                turn_id: p.turn_id,
                error: p.error,
            },
            Err(_) => unhandled(),
        },
        "events/turn_rejected" => {
            // The reason is a tagged enum; surface the phase it names. The TUI
            // only needs the reason (to clear `busy` and flash it), so a missing
            // field never drops this to Unhandled — which would otherwise wedge
            // the busy spinner forever.
            let reason = params
                .get("reason")
                .and_then(|r| r.get("currentPhase"))
                .and_then(Value::as_str)
                .map_or_else(
                    || "engine busy".to_owned(),
                    |phase| format!("engine busy (phase: {phase})"),
                );
            EngineNotification::TurnRejected { reason }
        }
        "events/item_appended" => match serde_json::from_value::<ItemAppendedPayload>(params) {
            Ok(p) => EngineNotification::ItemAppended {
                thread_id: p.thread_id,
                turn_id: p.turn_id,
                item: Box::new(p.item),
            },
            Err(_) => unhandled(),
        },
        "events/item_delta" => match serde_json::from_value::<ItemDeltaPayload>(params) {
            Ok(p) => EngineNotification::ItemDelta {
                thread_id: p.thread_id,
                turn_id: p.turn_id,
                delta: p.delta,
            },
            Err(_) => unhandled(),
        },
        "events/phase_changed" => match serde_json::from_value::<PhaseChangedPayload>(params) {
            Ok(p) => EngineNotification::PhaseChanged {
                thread_id: p.thread_id,
                from: p.from,
                to: p.to,
            },
            Err(_) => unhandled(),
        },
        "events/session_aborted" => {
            match serde_json::from_value::<SessionAbortedNotification>(params) {
                Ok(p) => EngineNotification::SessionAborted(Box::new(p)),
                Err(_) => unhandled(),
            }
        }
        "events/permission_requested" => {
            match serde_json::from_value::<PermissionRequestedPayload>(params) {
                Ok(p) => EngineNotification::PermissionRequested {
                    request_id: p.request_id,
                    request: Box::new(p.request),
                },
                Err(_) => unhandled(),
            }
        }
        _ => unhandled(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_turn_lifecycle() {
        let started = decode(
            "events/turn_started",
            Some(serde_json::json!({"threadId": "thread:native/a", "turnId": "turn:a/0"})),
        );
        assert!(matches!(started, EngineNotification::TurnStarted { .. }));

        let completed = decode(
            "events/turn_completed",
            Some(serde_json::json!({"threadId": "thread:native/a", "turnId": "turn:a/0"})),
        );
        assert!(matches!(
            completed,
            EngineNotification::TurnCompleted { .. }
        ));
    }

    #[test]
    fn decodes_item_appended_agent_message() {
        let n = decode(
            "events/item_appended",
            Some(serde_json::json!({
                "threadId": "thread:native/a",
                "turnId": "turn:a/0",
                "itemId": "item:turn:a/0/1",
                "item": { "itemKind": "agent_message", "id": "item:turn:a/0/1", "text": "hello" }
            })),
        );
        match n {
            EngineNotification::ItemAppended { item, .. } => {
                assert!(matches!(*item, Item::AgentMessage { .. }));
            }
            other => panic!("expected ItemAppended, got {other:?}"),
        }
    }

    #[test]
    fn unknown_method_is_unhandled() {
        let n = decode("events/made_up", None);
        assert!(matches!(n, EngineNotification::Unhandled { .. }));
    }

    #[test]
    fn malformed_payload_is_unhandled_not_panic() {
        let n = decode("events/turn_started", Some(serde_json::json!({"bogus": 1})));
        assert!(matches!(n, EngineNotification::Unhandled { .. }));
    }
}

// Rust guideline compliant 2026-02-21

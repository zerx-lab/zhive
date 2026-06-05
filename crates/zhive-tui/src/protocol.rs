//! Decoding of engine push-notifications into a typed event the UI folds over.
//!
//! The engine forwards every domain event as a JSON-RPC *notification* (no id)
//! under the `events/*` method namespace; see `zhive-core`
//! `engine_event_to_notification`. Those wire payloads are distinct from the
//! same-named `zhive_proto::domain` notification structs (e.g. wire
//! `events/turn_started` is `{threadId, turnId}`, not the proto
//! `TurnStartedNotification` which embeds a whole `Turn`), so this module
//! deserializes the *actual* wire shapes into [`EngineNotification`].
//!
//! All payload types are imported from [`zhive_proto::events`]; no local
//! hand-copy structs remain in this module.

use serde_json::Value;
use zhive_proto::domain::{Item, ThreadId, TurnError, TurnId};
use zhive_proto::events::{
    CompactionCompletedPayload, CompactionDeltaPayload, CompactionFailedPayload,
    CompactionStartedPayload, ItemAppendedPayload, ItemDeltaPayload, PermissionRequestedPayload,
    PhaseChangedPayload, SubagentCompletedPayload, SubagentStartedPayload, TurnCompletedPayload,
    TurnFailedPayload, TurnStartedPayload, UsagePayload,
};
use zhive_proto::hook::{CompactTrigger, EnginePhase};
use zhive_proto::methods as m;
use zhive_proto::permission::SessionAbortedNotification;

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
        request: Box<zhive_proto::permission::RequestPermissionRequest>,
    },
    /// Token usage reported at the end of a provider call.
    Usage {
        /// Input tokens consumed by this provider call.
        input_tokens: u64,
        /// Output tokens produced by this provider call.
        output_tokens: u64,
    },
    /// A subagent (child thread) was spawned by a thread.
    ///
    /// The child thread's own `turn_started` / `item_appended` events also
    /// stream in (with the child's `thread_id`); the conversation reducer uses
    /// this event to learn the parent↔child link and route them.
    SubagentStarted {
        /// The parent thread that spawned the subagent.
        parent_thread_id: ThreadId,
        /// The child thread the subagent runs in.
        child_thread_id: ThreadId,
        /// The subagent definition name, if the agent was named.
        agent_type: Option<String>,
        /// The task description, if the spawn provided one.
        description: Option<String>,
    },
    /// A subagent finished and delivered its outcome to the parent.
    SubagentCompleted {
        /// The parent thread.
        parent_thread_id: ThreadId,
        /// The child thread that completed.
        child_thread_id: ThreadId,
        /// Whether the subagent produced a final message.
        has_final: bool,
    },
    /// Context compaction entered the summarization phase (manual or auto).
    CompactionStarted {
        /// Thread being compacted.
        thread_id: ThreadId,
        /// Why compaction fired (manual `/compact` vs automatic threshold).
        trigger: CompactTrigger,
        /// Transcript items being folded into the summary.
        entries: u32,
    },
    /// A streamed fragment of the compaction summary.
    CompactionDelta {
        /// Thread being compacted.
        thread_id: ThreadId,
        /// Incremental summary fragment.
        delta: String,
    },
    /// Context compaction finished successfully.
    CompactionCompleted {
        /// Thread that was compacted.
        thread_id: ThreadId,
        /// Transcript items folded into the summary.
        entries_compacted: u32,
    },
    /// Context compaction failed; carries the reason to display.
    CompactionFailed {
        /// Thread whose compaction failed.
        thread_id: ThreadId,
        /// Human-readable failure reason.
        reason: String,
    },
    /// A recognized-but-unmodeled or unknown notification method.
    Unhandled {
        /// The notification method string.
        method: String,
    },
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

    if matches!(
        method,
        m::EVENT_TURN_STARTED
            | m::EVENT_TURN_COMPLETED
            | m::EVENT_TURN_FAILED
            | m::EVENT_TURN_REJECTED
    ) {
        return decode_turn(method, params);
    }

    if method == m::EVENT_ITEM_APPENDED {
        return match serde_json::from_value::<ItemAppendedPayload>(params) {
            Ok(p) => EngineNotification::ItemAppended {
                thread_id: p.thread_id,
                turn_id: p.turn_id,
                item: Box::new(p.item),
            },
            // A decode failure here silently drops a record (e.g. a tool call)
            // from the live view, so make it visible rather than swallowing it.
            Err(err) => {
                tracing::warn!(
                    name: "zhive.tui.decode_failed",
                    method = m::EVENT_ITEM_APPENDED,
                    error = %err,
                    "dropped events/item_appended: payload failed to decode",
                );
                unhandled()
            }
        };
    }

    if method == m::EVENT_ITEM_DELTA {
        return match serde_json::from_value::<ItemDeltaPayload>(params) {
            Ok(p) => EngineNotification::ItemDelta {
                thread_id: p.thread_id,
                turn_id: p.turn_id,
                delta: p.delta,
            },
            Err(_) => unhandled(),
        };
    }

    if method == m::EVENT_PHASE_CHANGED {
        return match serde_json::from_value::<PhaseChangedPayload>(params) {
            Ok(p) => EngineNotification::PhaseChanged {
                // PhaseChangedPayload holds Option<ThreadId>; map to Option<String>
                // so the EngineNotification variant stays wire-type-agnostic.
                thread_id: p.thread_id.map(|t| t.0.to_string()),
                from: p.from,
                to: p.to,
            },
            Err(_) => unhandled(),
        };
    }

    if method == m::EVENT_SESSION_ABORTED {
        return match serde_json::from_value::<SessionAbortedNotification>(params) {
            Ok(p) => EngineNotification::SessionAborted(Box::new(p)),
            Err(_) => unhandled(),
        };
    }

    if method == m::EVENT_PERMISSION_REQUESTED {
        return match serde_json::from_value::<PermissionRequestedPayload>(params) {
            Ok(p) => EngineNotification::PermissionRequested {
                request_id: p.request_id,
                request: Box::new(p.request),
            },
            Err(_) => unhandled(),
        };
    }

    if method == m::EVENT_USAGE {
        return match serde_json::from_value::<UsagePayload>(params) {
            Ok(p) => EngineNotification::Usage {
                input_tokens: p.input_tokens,
                output_tokens: p.output_tokens,
            },
            Err(_) => unhandled(),
        };
    }

    if method == m::EVENT_SUBAGENT_STARTED || method == m::EVENT_SUBAGENT_COMPLETED {
        return decode_subagent(method, params);
    }

    if matches!(
        method,
        m::EVENT_COMPACTION_STARTED
            | m::EVENT_COMPACTION_DELTA
            | m::EVENT_COMPACTION_COMPLETED
            | m::EVENT_COMPACTION_FAILED
    ) {
        return decode_compaction(method, params);
    }

    unhandled()
}

/// Decodes the four `events/compaction_*` notifications.
fn decode_compaction(method: &str, params: Value) -> EngineNotification {
    let unhandled = || EngineNotification::Unhandled {
        method: method.to_owned(),
    };

    if method == m::EVENT_COMPACTION_STARTED {
        return match serde_json::from_value::<CompactionStartedPayload>(params) {
            Ok(p) => EngineNotification::CompactionStarted {
                thread_id: p.thread_id,
                trigger: p.trigger,
                entries: p.entries,
            },
            Err(_) => unhandled(),
        };
    }

    if method == m::EVENT_COMPACTION_DELTA {
        return match serde_json::from_value::<CompactionDeltaPayload>(params) {
            Ok(p) => EngineNotification::CompactionDelta {
                thread_id: p.thread_id,
                delta: p.delta,
            },
            Err(_) => unhandled(),
        };
    }

    if method == m::EVENT_COMPACTION_COMPLETED {
        return match serde_json::from_value::<CompactionCompletedPayload>(params) {
            Ok(p) => EngineNotification::CompactionCompleted {
                thread_id: p.thread_id,
                entries_compacted: p.entries_compacted,
            },
            Err(_) => unhandled(),
        };
    }

    if method == m::EVENT_COMPACTION_FAILED {
        return match serde_json::from_value::<CompactionFailedPayload>(params) {
            Ok(p) => EngineNotification::CompactionFailed {
                thread_id: p.thread_id,
                reason: p.reason,
            },
            Err(_) => unhandled(),
        };
    }

    unhandled()
}

/// Decodes the four turn-lifecycle notifications into [`EngineNotification`].
fn decode_turn(method: &str, params: Value) -> EngineNotification {
    let unhandled = || EngineNotification::Unhandled {
        method: method.to_owned(),
    };

    if method == m::EVENT_TURN_STARTED {
        return match serde_json::from_value::<TurnStartedPayload>(params) {
            Ok(p) => EngineNotification::TurnStarted {
                thread_id: p.thread_id,
                turn_id: p.turn_id,
            },
            Err(_) => unhandled(),
        };
    }

    if method == m::EVENT_TURN_COMPLETED {
        return match serde_json::from_value::<TurnCompletedPayload>(params) {
            Ok(p) => EngineNotification::TurnCompleted {
                thread_id: p.thread_id,
                turn_id: p.turn_id,
            },
            Err(_) => unhandled(),
        };
    }

    if method == m::EVENT_TURN_FAILED {
        return match serde_json::from_value::<TurnFailedPayload>(params) {
            Ok(p) => EngineNotification::TurnFailed {
                thread_id: p.thread_id,
                turn_id: p.turn_id,
                error: p.error,
            },
            Err(_) => unhandled(),
        };
    }

    // turn_rejected: the reason is a tagged enum; surface the phase it names.
    // A missing field still yields a reason so the busy spinner never wedges.
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

/// Decodes the two subagent lifecycle notifications into [`EngineNotification`].
fn decode_subagent(method: &str, params: Value) -> EngineNotification {
    let unhandled = || EngineNotification::Unhandled {
        method: method.to_owned(),
    };

    if method == m::EVENT_SUBAGENT_STARTED {
        return match serde_json::from_value::<SubagentStartedPayload>(params) {
            Ok(p) => EngineNotification::SubagentStarted {
                parent_thread_id: p.parent_thread_id,
                child_thread_id: p.child_thread_id,
                agent_type: p.agent_type,
                description: p.description,
            },
            Err(_) => unhandled(),
        };
    }

    if method == m::EVENT_SUBAGENT_COMPLETED {
        return match serde_json::from_value::<SubagentCompletedPayload>(params) {
            Ok(p) => EngineNotification::SubagentCompleted {
                parent_thread_id: p.parent_thread_id,
                child_thread_id: p.child_thread_id,
                // Proto payload field is `has_final_message`; TUI variant
                // field is `has_final` (shorter, no redundancy with variant name).
                has_final: p.has_final_message,
            },
            Err(_) => unhandled(),
        };
    }

    unhandled()
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
    fn decodes_item_appended_tool_call() {
        // The exact wire shape the engine emits for a completed bash tool call
        // (snake_case fields, `itemKind` discriminator). Must NOT collapse to
        // Unhandled — otherwise the tool record is silently dropped live.
        let n = decode(
            "events/item_appended",
            Some(serde_json::json!({
                "threadId": "thread:native/a",
                "turnId": "turn:thread:native/a/0",
                "itemId": "item:turn:thread:native/a/0/0",
                "item": {
                    "itemKind": "tool_call",
                    "id": "item:turn:thread:native/a/0/0",
                    "name": "bash",
                    "kind": "execute",
                    "status": "completed",
                    "content": [
                        { "type": "content", "content": { "type": "text", "text": "Exit code: 0\nREADME.md" } }
                    ],
                    "raw_input": { "command": "ls", "cwd": "/repo" }
                }
            })),
        );
        match n {
            EngineNotification::ItemAppended { item, .. } => {
                assert!(matches!(*item, Item::ToolCall { .. }), "got {item:?}");
            }
            other => panic!("expected ItemAppended(ToolCall), got {other:?}"),
        }
    }

    #[test]
    fn unknown_method_is_unhandled() {
        let n = decode("events/made_up", None);
        assert!(matches!(n, EngineNotification::Unhandled { .. }));
    }

    #[test]
    fn decodes_subagent_started() {
        let n = decode(
            "events/subagent_started",
            Some(serde_json::json!({
                "parentThreadId": "thread:native/p",
                "childThreadId": "thread:subagent/p/1",
                "agentType": "researcher",
                "description": "find the bug"
            })),
        );
        match n {
            EngineNotification::SubagentStarted {
                child_thread_id,
                agent_type,
                ..
            } => {
                assert_eq!(child_thread_id.0.as_ref(), "thread:subagent/p/1");
                assert_eq!(agent_type.as_deref(), Some("researcher"));
            }
            other => panic!("expected SubagentStarted, got {other:?}"),
        }
    }

    #[test]
    fn decodes_subagent_completed_defaults_missing_has_final() {
        let n = decode(
            "events/subagent_completed",
            Some(serde_json::json!({
                "parentThreadId": "thread:native/p",
                "childThreadId": "thread:subagent/p/1"
            })),
        );
        match n {
            EngineNotification::SubagentCompleted { has_final, .. } => {
                assert!(!has_final, "missing hasFinalMessage defaults to false");
            }
            other => panic!("expected SubagentCompleted, got {other:?}"),
        }
    }

    #[test]
    fn malformed_payload_is_unhandled_not_panic() {
        let n = decode("events/turn_started", Some(serde_json::json!({"bogus": 1})));
        assert!(matches!(n, EngineNotification::Unhandled { .. }));
    }
}

// Rust guideline compliant 2026-02-21

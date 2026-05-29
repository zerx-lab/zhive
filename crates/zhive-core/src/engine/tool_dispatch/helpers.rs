//! Pure helper functions for the tool-dispatch pipeline.
//!
//! These functions are extracted from the main dispatch module to keep
//! [`super::dispatch_tool_call`] focused on orchestration.  All items here
//! are stateless and have no side-effects beyond allocating and returning
//! values.

use zhive_proto::domain::{Item, ItemContent, ItemId, ItemToolCallContent, ToolCallStatus};
use zhive_proto::permission::RequestPermissionRequest;

use super::DispatchOutcome;

/// Builds a blocked [`DispatchOutcome`] (status=Failed) with the given reason.
///
/// `tool_use_id` is the provider's stable id for this call; it is preserved on
/// the failed item's `provider_tool_call_id` so prompt reconstruction can emit
/// a matching `tool_use` / `tool_result` id pair even for blocked calls (a
/// denied tool still produces a `Message::Tool` result the provider must
/// correlate).
pub(super) fn blocked_outcome(
    item_id: ItemId,
    tool_name: &str,
    raw_args: serde_json::Value,
    tool_use_id: &str,
    reason: String,
    stop_loop: bool,
) -> DispatchOutcome {
    let content = vec![ItemToolCallContent::Content {
        content: ItemContent::Text {
            text: reason,
            annotations: None,
        },
    }];
    let item = Item::ToolCall {
        id: item_id,
        name: tool_name.to_owned(),
        kind: zhive_proto::domain::ToolKind::Other,
        status: ToolCallStatus::Failed,
        content,
        locations: vec![],
        raw_input: Some(raw_args),
        raw_output: None,
        provider_tool_call_id: Some(tool_use_id.to_owned()),
    };
    DispatchOutcome::Blocked { item, stop_loop }
}

/// Builds the blocked outcome used when the turn is cancelled mid-execution.
///
/// Returned when either [`crate::tools::Tool::execute`] or the `PostToolUse`
/// dispatch loses its `tokio::select!` race against the turn cancel token.
/// The item carries no `raw_output` (the result is abandoned) but keeps the
/// provider tool-call id so the caller can decide whether to thread it.
pub(super) fn cancelled_during_execution(
    item_id: ItemId,
    tool_name: &str,
    raw_args: serde_json::Value,
    tool_use_id: &str,
    stop_loop: bool,
) -> DispatchOutcome {
    blocked_outcome(
        item_id,
        tool_name,
        raw_args,
        tool_use_id,
        "turn cancelled during tool execution".to_owned(),
        stop_loop,
    )
}

/// Builds a minimal [`RequestPermissionRequest`] for a tool call.
///
/// # Errors
///
/// Returns a `serde_json::Error` if JSON deserialization fails.
/// In practice this cannot happen because the engine controls the template,
/// but the function returns `Result` so callers can handle it without
/// `.expect()` in non-test code.
pub(super) fn build_permission_request(
    thread_id_str: &str,
    tool_name: &str,
) -> Result<RequestPermissionRequest, serde_json::Error> {
    // RequestPermissionRequest and PermissionOption are #[non_exhaustive];
    // use JSON construction to stay future-safe.
    serde_json::from_value(serde_json::json!({
        "threadId": thread_id_str,
        "resourceType": "tool",
        "name": tool_name,
        "reason": format!("agent wants to call tool: {tool_name}"),
        "options": [
            {
                "id": "allow_once",
                "kind": "AllowOnce",
                "description": "Allow once"
            },
            {
                "id": "reject_once",
                "kind": "RejectOnce",
                "description": "Reject"
            }
        ]
    }))
}

// Rust guideline compliant 2026-02-21

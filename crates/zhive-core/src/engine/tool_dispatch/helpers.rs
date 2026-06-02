//! Pure helper functions for the tool-dispatch pipeline.
//!
//! These functions are extracted from the main dispatch module to keep
//! [`super::dispatch_tool_call`] focused on orchestration.  All items here
//! are stateless and have no side-effects beyond allocating and returning
//! values.

use zhive_proto::domain::{Item, ItemContent, ItemId, ItemToolCallContent, ToolCallStatus};
use zhive_proto::permission::{PermissionOption, PermissionOptionKind, RequestPermissionRequest};

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

/// Stable option id for a one-shot allow choice.
///
/// Hyphenated to match the rest of the codebase (the ACP bridge emits the
/// same id). The dispatch classifier also accepts the underscore form for
/// backward compatibility, but new requests advertise this form.
pub(super) const OPT_ALLOW_ONCE: &str = "allow-once";
/// Stable option id for a persistent allow choice (records allow-always).
pub(super) const OPT_ALLOW_ALWAYS: &str = "allow-always";
/// Stable option id for a one-shot reject choice.
pub(super) const OPT_REJECT_ONCE: &str = "reject-once";
/// Stable option id for a persistent reject choice.
pub(super) const OPT_REJECT_ALWAYS: &str = "reject-always";

/// Builds a minimal [`RequestPermissionRequest`] for a tool call.
///
/// Advertises all four ACP option kinds — `AllowOnce`, `AllowAlways`,
/// `RejectOnce`, `RejectAlways` — so the client can persist an
/// allow/reject decision for the tool. The dispatch path classifies the
/// returned `option_id` structurally by [`zhive_proto::permission::PermissionOptionKind`]
/// (never by string prefix) and records allow-always on the reducer when
/// the user picks `AllowAlways`.
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
    // use JSON construction to stay future-safe. `PermissionOptionKind`
    // serializes with its variant names verbatim (no rename_all), so the
    // "AllowOnce" / "AllowAlways" / … strings below are the wire form.
    serde_json::from_value(serde_json::json!({
        "threadId": thread_id_str,
        "resourceType": "tool",
        "name": tool_name,
        "reason": format!("agent wants to call tool: {tool_name}"),
        "options": [
            {
                "id": OPT_ALLOW_ONCE,
                "kind": "AllowOnce",
                "description": "Allow once / 允许一次"
            },
            {
                "id": OPT_ALLOW_ALWAYS,
                "kind": "AllowAlways",
                "description": "Always allow this tool / 始终允许此工具"
            },
            {
                "id": OPT_REJECT_ONCE,
                "kind": "RejectOnce",
                "description": "Reject once / 拒绝一次"
            },
            {
                "id": OPT_REJECT_ALWAYS,
                "kind": "RejectAlways",
                "description": "Always reject this tool / 始终拒绝此工具"
            }
        ]
    }))
}

/// Classifies a selected `option_id` into its [`PermissionOptionKind`].
///
/// Resolution order, picking the first that matches:
///
/// 1. The kind of the matching option in `options` (the authoritative
///    source — what the request actually advertised).
/// 2. A fallback mapping of the well-known stable ids, tolerant of both
///    the hyphenated (`allow-always`) and underscored (`allow_always`)
///    forms, so clients/tests that echo a legacy id still classify.
///
/// Returns `None` when the id matches neither, signalling the dispatch
/// path to deny conservatively (an unrecognised choice is never an
/// implicit allow).
///
/// This is the structural replacement for the former
/// `option_id.starts_with("allow")` string-prefix heuristic: classification
/// is driven by [`PermissionOptionKind`], so a tool named, e.g.,
/// `allow_dangerous` can never be mistaken for an allow vote.
pub(super) fn classify_option_id(
    options: &[PermissionOption],
    option_id: &str,
) -> Option<PermissionOptionKind> {
    if let Some(opt) = options.iter().find(|o| o.id == option_id) {
        return Some(opt.kind);
    }
    match option_id {
        OPT_ALLOW_ONCE | "allow_once" => Some(PermissionOptionKind::AllowOnce),
        OPT_ALLOW_ALWAYS | "allow_always" => Some(PermissionOptionKind::AllowAlways),
        OPT_REJECT_ONCE | "reject_once" => Some(PermissionOptionKind::RejectOnce),
        OPT_REJECT_ALWAYS | "reject_always" => Some(PermissionOptionKind::RejectAlways),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Vec<PermissionOption> {
        build_permission_request("thread:native/t", "read_file")
            .expect("request fixture")
            .options
    }

    #[test]
    fn build_request_advertises_all_four_kinds() {
        let opts = options();
        let kinds: Vec<PermissionOptionKind> = opts.iter().map(|o| o.kind).collect();
        assert!(kinds.contains(&PermissionOptionKind::AllowOnce));
        assert!(kinds.contains(&PermissionOptionKind::AllowAlways));
        assert!(kinds.contains(&PermissionOptionKind::RejectOnce));
        assert!(kinds.contains(&PermissionOptionKind::RejectAlways));
    }

    #[test]
    fn classify_uses_advertised_option_kind() {
        let opts = options();
        assert_eq!(
            classify_option_id(&opts, OPT_ALLOW_ALWAYS),
            Some(PermissionOptionKind::AllowAlways)
        );
        assert_eq!(
            classify_option_id(&opts, OPT_REJECT_ONCE),
            Some(PermissionOptionKind::RejectOnce)
        );
    }

    #[test]
    fn classify_falls_back_to_legacy_underscore_ids() {
        // Empty option set forces the fallback path; legacy underscore ids
        // (used by older clients and existing tests) must still classify.
        assert_eq!(
            classify_option_id(&[], "allow_once"),
            Some(PermissionOptionKind::AllowOnce)
        );
        assert_eq!(
            classify_option_id(&[], "reject_always"),
            Some(PermissionOptionKind::RejectAlways)
        );
    }

    #[test]
    fn classify_unknown_id_is_none() {
        // A tool-name-looking id must NOT be mistaken for an allow vote.
        assert_eq!(classify_option_id(&[], "allow_dangerous"), None);
        assert_eq!(classify_option_id(&options(), "totally-unknown"), None);
    }
}

// Rust guideline compliant 2026-02-21

//! Pure helper functions for the tool-dispatch pipeline.
//!
//! These functions are extracted from the main dispatch module to keep
//! [`super::dispatch_tool_call`] focused on orchestration.  All items here
//! are stateless and have no side-effects beyond allocating and returning
//! values.

use tokio_util::sync::CancellationToken;
use zhive_proto::domain::{Item, ItemContent, ItemId, ItemToolCallContent, ToolCallStatus};
use zhive_proto::permission::{
    PermissionDecision, PermissionOption, PermissionOptionKind, RequestPermissionRequest,
};

use crate::subagent::{ParentVerdict, SubagentDecisionRequest};

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

/// Outcome of a child → parent permission handshake.
///
/// Returned by [`handshake_with_parent`] so the caller maps it onto the
/// dispatch flow: `Allow` proceeds to execution, `Deny` blocks the call.
pub(super) enum HandshakeVerdict {
    /// The parent permitted the call; proceed to execute.
    Allow,
    /// The parent blocked the call (deny, cancel, or a broken channel).
    Deny,
}

/// Reports a child tool-call decision to the parent and parks for the verdict.
///
/// Sends a [`SubagentDecisionRequest`] (carrying `tool_name` / `raw_args` so the
/// parent can re-dispatch its own `PreToolUse` hooks) over `decision_tx`, then
/// awaits the parent's [`ParentVerdict`] on a fresh reply oneshot — raced
/// against the child turn's `cancel` token.
///
/// Every failure mode degrades to [`HandshakeVerdict::Deny`] so a child can
/// never *widen* its own permission by losing the handshake:
///
/// * `cancel` fires first → deny (the turn is being torn down).
/// * the reply oneshot is dropped (parent spawner exited / panicked) → deny.
/// * `decision_tx.send` fails (parent dropped the receiver) → deny.
///
/// This mirrors codex `codex_delegate.rs`, where every approval is routed to
/// the parent session and a lost/cancelled handshake resolves to a rejection.
pub(super) async fn handshake_with_parent(
    decision_tx: &tokio::sync::mpsc::Sender<SubagentDecisionRequest>,
    tool_use_id: &str,
    tool_name: &str,
    raw_args: &serde_json::Value,
    child_decision: PermissionDecision,
    cancel: &CancellationToken,
) -> HandshakeVerdict {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<ParentVerdict>();
    let request = SubagentDecisionRequest {
        tool_use_id: tool_use_id.to_owned(),
        tool_name: tool_name.to_owned(),
        raw_args: raw_args.clone(),
        child_decision,
        reply: reply_tx,
    };

    if decision_tx.send(request).await.is_err() {
        // Parent spawner already dropped its receiver (it exited the select
        // loop). Deny conservatively rather than execute unsupervised.
        tracing::warn!(
            name: "zhive.subagent.handshake.send_failed",
            tool = tool_name,
            decision = "deny",
            "parent decision channel closed; blocking child tool call"
        );
        return HandshakeVerdict::Deny;
    }

    // Park on the parent's verdict, biased so a cancel is observed first.
    tokio::select! {
        biased;
        () = cancel.cancelled() => {
            tracing::debug!(
                name: "zhive.subagent.handshake.cancelled",
                tool = tool_name,
                decision = "deny",
                "child turn cancelled while awaiting parent verdict; blocking tool call"
            );
            HandshakeVerdict::Deny
        }
        verdict = reply_rx => match verdict {
            Ok(ParentVerdict::Allow) => HandshakeVerdict::Allow,
            Ok(ParentVerdict::Deny) => HandshakeVerdict::Deny,
            // Reply sender dropped (parent panicked / exited): deny, never widen.
            Err(_recv_err) => {
                tracing::warn!(
                    name: "zhive.subagent.handshake.reply_dropped",
                    tool = tool_name,
                    decision = "deny",
                    "parent verdict channel dropped; blocking child tool call"
                );
                HandshakeVerdict::Deny
            }
        },
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

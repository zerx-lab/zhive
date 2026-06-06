//! Permission inversion: engine notification ↔ ACP reverse request.
//!
//! The two protocols invert request/response around a permission decision:
//!
//! * **zhive** *emits* an [`EngineEvent::PermissionRequested`] notification and
//!   waits for the decision to be fed back out-of-band via
//!   [`zhive_core::engine::Engine::resume_permission`].
//! * **ACP** has the agent *send* a `session/request_permission` request and
//!   *await* the client's [`RequestPermissionResponse`].
//!
//! Because the engine is embedded in-process, the bridge collapses the
//! inversion to a straight line inside the prompt task: build the ACP request
//! from the engine's pending tool call, send it, await the client, translate
//! the outcome, and resume the engine. This module holds the two pure mapping
//! helpers; the await/spawn wiring lives in `lib.rs` where it has the
//! connection handle.

use agent_client_protocol::schema::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    SessionId, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
};

use zhive_proto::permission::{
    PermissionOutcome, RequestPermissionRequest as ZRequestPermissionRequest,
};

/// Builds the ACP `session/request_permission` request from a zhive request.
///
/// Always offers the four standard [`PermissionOptionKind`] options. The zhive
/// `name` (tool / resource) becomes the [`ToolCallUpdate`] title so the editor
/// can render which tool is asking; `reason` is carried via the title text.
///
/// # Examples
///
/// ```
/// use zhive_bridge_acp::permission::build_request;
/// use zhive_proto::permission::RequestPermissionRequest;
///
/// let req: RequestPermissionRequest = serde_json::from_value(serde_json::json!({
///     "threadId": "thread:acp/0",
///     "resourceType": "tool",
///     "name": "bash",
///     "reason": "run a command",
///     "options": []
/// })).unwrap();
/// let acp = build_request(&"acp-0".into(), &req);
/// assert_eq!(acp.options.len(), 4);
/// ```
#[must_use]
pub fn build_request(
    session_id: &SessionId,
    request: &ZRequestPermissionRequest,
) -> RequestPermissionRequest {
    // Prefer the request's tool-call id so the permission prompt correlates with
    // the `tool_call` card already announced for this call; fall back to the
    // resource name for older callers that carry no id. The reason is surfaced
    // in the title for the editor's permission prompt.
    let raw_id = request
        .tool_call_id
        .as_deref()
        .unwrap_or(request.name.as_str());
    let tool_call_id = ToolCallId::new(std::sync::Arc::<str>::from(raw_id));
    let title = format!("{}: {}", request.name, request.reason);
    let fields = ToolCallUpdateFields::new().title(title);
    let tool_call = ToolCallUpdate::new(tool_call_id, fields);

    RequestPermissionRequest::new(session_id.clone(), tool_call, standard_options())
}

/// Returns the four standard permission options offered to the client.
#[must_use]
pub fn standard_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new("allow-once", "Allow", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            "allow-always",
            "Always allow",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new("reject-once", "Reject", PermissionOptionKind::RejectOnce),
        PermissionOption::new(
            "reject-always",
            "Always reject",
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

/// Translates an ACP permission outcome into a zhive [`PermissionOutcome`].
///
/// `Cancelled` maps straight through (ACP mandates this when a turn is
/// cancelled mid-request). A `Selected` option id is echoed verbatim into
/// [`PermissionOutcome::Selected`]; the engine's reducer interprets the id
/// (`allow-once`, `reject-always`, …).
///
/// # Examples
///
/// ```
/// use agent_client_protocol::schema::{RequestPermissionOutcome, SelectedPermissionOutcome};
/// use zhive_bridge_acp::permission::outcome_to_engine;
/// use zhive_proto::permission::PermissionOutcome;
///
/// let acp = RequestPermissionOutcome::Selected(
///     SelectedPermissionOutcome::new("allow-once"),
/// );
/// assert!(matches!(
///     outcome_to_engine(acp),
///     PermissionOutcome::Selected { option_id } if option_id == "allow-once"
/// ));
///
/// assert!(matches!(
///     outcome_to_engine(RequestPermissionOutcome::Cancelled),
///     PermissionOutcome::Cancelled
/// ));
/// ```
#[must_use]
pub fn outcome_to_engine(outcome: RequestPermissionOutcome) -> PermissionOutcome {
    match outcome {
        RequestPermissionOutcome::Selected(selected) => PermissionOutcome::Selected {
            option_id: selected.option_id.0.to_string(),
        },
        // `Cancelled` and any unknown future outcome (the enum is
        // `#[non_exhaustive]`) settle conservatively as a cancellation.
        _ => PermissionOutcome::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::SelectedPermissionOutcome;

    fn sample_request() -> ZRequestPermissionRequest {
        serde_json::from_value(serde_json::json!({
            "threadId": "thread:acp/0",
            "resourceType": "tool",
            "name": "read_file",
            "reason": "read a file",
            "options": []
        }))
        .expect("fixture")
    }

    #[test]
    fn build_request_offers_four_options() {
        let acp = build_request(&SessionId::new("acp-0"), &sample_request());
        assert_eq!(acp.options.len(), 4);
        assert_eq!(acp.session_id.0.as_ref(), "acp-0");
    }

    #[test]
    fn build_request_titles_carry_tool_and_reason() {
        let acp = build_request(&SessionId::new("acp-0"), &sample_request());
        let title = acp.tool_call.fields.title.expect("title set");
        assert!(title.contains("read_file"));
        assert!(title.contains("read a file"));
    }

    #[test]
    fn selected_round_trips() {
        let acp =
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new("allow-always"));
        match outcome_to_engine(acp) {
            PermissionOutcome::Selected { option_id } => assert_eq!(option_id, "allow-always"),
            other => panic!("expected Selected, got {other:?}"),
        }
    }

    #[test]
    fn cancelled_round_trips() {
        assert!(matches!(
            outcome_to_engine(RequestPermissionOutcome::Cancelled),
            PermissionOutcome::Cancelled
        ));
    }
}

// Rust guideline compliant 2026-02-21

//! Subagent scheduling helpers (D-008 + Claude Code hard constraints).
//!
//! Phase 1 keeps subagents as **same-engine** child threads — there is
//! no separate engine instance per subagent. Three hard constraints
//! from the Claude Code Subagents docs are enforced here:
//!
//! * **Fresh context window** — the child thread starts with an empty
//!   transcript; nothing from the parent's history leaks in.
//! * **Only the final message returns** — the parent sees one
//!   [`zhive_proto::domain::Item::AgentMessage`] (or [`Item::SystemNotice`])
//!   per subagent; intermediate items stay scoped to the child rollout.
//! * **No recursion** — a subagent cannot spawn another subagent;
//!   [`prepare_child_scope`] forces `allow_subagent_spawn = false`.
//!
//! [`Item::SystemNotice`]: zhive_proto::domain::Item::SystemNotice

use std::sync::Arc;

use thiserror::Error;
use zhive_proto::domain::{Item, ThreadId, TurnError};
use zhive_proto::permission::{
    PermissionDecision, PermissionMode, PermissionScope, ScopeError, SubagentDefinition,
};

/// Outcome variants for a finished subagent turn.
///
/// ## Delivery
///
/// Subagent outcomes are delivered via two paths:
///
/// 1. **In-process channel**: `ThreadHandle::subagent_final_tx` holds a
///    `Sender<SubagentFinalEvent>`.  The matching `Receiver` is returned by
///    [`crate::state::ThreadHandle::new_child`] to the spawn site so the
///    spawner can `await` the child result directly (e.g. from within a
///    parent Agent-tool handler), without subscribing to the broadcast bus.
///
/// 2. **Broadcast bus**: [`crate::engine::event::EngineEvent::SubagentCompleted`]
///    is always emitted for external observers regardless of whether the
///    in-process channel receiver was retained.
///
/// The [`SubagentFinalEvent::Suspended`] variant is reserved for a future
/// child-suspends-independently path (B8-O6) and is NOT constructed today: under
/// the current full-handshake architecture a deferring child routes its decision
/// to the parent's second fold and the parent resolves it inline (Allow / Deny)
/// before the child continues, so no terminal `Suspended` is emitted. See the
/// variant's own docs for details.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::{ItemId, ThreadId};
/// use zhive_core::subagent::{SubagentFinalEvent, extract_final_message};
/// use zhive_proto::domain::Item;
///
/// let tid = ThreadId(Arc::from("thread:subagent/child/0"));
/// let items = vec![Item::AgentMessage { id: ItemId(Arc::from("a1")), text: "done".into() }];
/// let final_msg = extract_final_message(&items);
/// let event = SubagentFinalEvent::Completed { child_thread_id: tid, final_message: final_msg };
/// assert!(matches!(event, SubagentFinalEvent::Completed { .. }));
/// ```
///
/// Constructing a `Suspended` event:
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::ThreadId;
/// use zhive_core::subagent::SubagentFinalEvent;
///
/// let tid = ThreadId(Arc::from("thread:subagent/child/0"));
/// let event = SubagentFinalEvent::Suspended {
///     child_thread_id: tid,
///     child_request_id: "perm:7".to_owned(),
/// };
/// assert!(matches!(event, SubagentFinalEvent::Suspended { .. }));
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SubagentFinalEvent {
    /// The subagent turn completed normally.
    ///
    /// `final_message` is the single [`Item::AgentMessage`] (or
    /// [`Item::SystemNotice`] fallback) extracted by
    /// [`extract_final_message`]; it is `None` when the child transcript
    /// contained neither.
    Completed {
        /// Stable id of the child thread.
        child_thread_id: ThreadId,
        /// The one item delivered back to the parent context, or `None`.
        final_message: Option<Arc<Item>>,
    },
    /// The subagent turn failed (provider error, stream error, etc.).
    ///
    /// On failure the engine emits `SubagentCompleted` with
    /// `final_message = None` rather than surfacing a hard error to the
    /// parent, matching the Claude Code "child error = tool result error"
    /// contract.
    Errored {
        /// Stable id of the child thread.
        child_thread_id: ThreadId,
        /// Failure detail from the turn.
        error: TurnError,
    },
    /// The subagent turn parked on a deferred permission request.
    ///
    /// NOT CONSTRUCTED under the current full-handshake architecture: a child
    /// tool call that folds to [`PermissionDecision::Defer`] routes the decision
    /// to the parent's second fold over the in-process `subagent_decision_tx`
    /// channel, and the parent resolves it (Allow / Deny) inline before the
    /// child continues — so a child never emits a terminal `Suspended` event.
    /// The variant is retained for a future architecture where a child could
    /// suspend independently of its parent (the spawner already has a forwarding
    /// path for it; see `EngineSubagentSpawner::spawn_and_await`). When that
    /// path is reached today, the spawner logs a `warn` rather than silently
    /// dropping the event.
    ///
    /// `child_request_id` is the wire-form pending request id the client passes
    /// back to resume; resuming it discharges the request on the shared engine
    /// reducer (request ids are globally unique).
    Suspended {
        /// Stable id of the suspended child thread.
        child_thread_id: ThreadId,
        /// Wire-form pending request id the client passes back to resume.
        child_request_id: String,
    },
}

/// Parent-side verdict returned to a child tool call after the parent's
/// second fold.
///
/// Sent back over the [`SubagentDecisionRequest::reply`] oneshot. The enum is
/// deliberately limited to terminal states: any `Ask` / `Defer` the parent
/// raises is resolved inside the spawner's reverse-RPC loop before a verdict
/// is returned, so the child only ever sees `Allow` or `Deny`. This is the
/// type-level guarantee that a child never re-enters a reverse-RPC of its own.
///
/// # Examples
///
/// ```ignore
/// use zhive_core::subagent::ParentVerdict;
/// let v = ParentVerdict::Deny;
/// assert!(matches!(v, ParentVerdict::Deny));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub(crate) enum ParentVerdict {
    /// The parent permits the child tool call; the child executes it.
    Allow,
    /// The parent blocks the child tool call; the child records a failure.
    Deny,
}

/// One child → parent permission handshake request.
///
/// A child turn sends this over `ThreadHandle::subagent_decision_tx` after it
/// folds a non-`Deny` decision for a tool call, then parks on `reply` until
/// the parent's second fold returns a [`ParentVerdict`]. `tool_name` and
/// `raw_args` are carried so the parent can re-dispatch its own `PreToolUse`
/// hooks with full tool context (the parent's second review must see what the
/// child is about to do, not just the decision).
///
/// The `reply` oneshot makes the struct non-`Clone`; a hand-written `Debug`
/// skips it. The value only ever flows over an in-process channel and never
/// enters an [`crate::engine::event::EngineEvent`] (which requires `Clone`).
pub(crate) struct SubagentDecisionRequest {
    /// Provider-side stable id for the child's tool call.
    pub tool_use_id: String,
    /// Name of the tool the child wants to call.
    pub tool_name: String,
    /// Effective (possibly hook-mutated) input arguments.
    pub raw_args: serde_json::Value,
    /// The child's own folded decision (never `Deny`; that short-circuits).
    pub child_decision: PermissionDecision,
    /// Channel the parent uses to return its terminal verdict to the child.
    pub reply: tokio::sync::oneshot::Sender<ParentVerdict>,
}

impl std::fmt::Debug for SubagentDecisionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentDecisionRequest")
            .field("tool_use_id", &self.tool_use_id)
            .field("tool_name", &self.tool_name)
            .field("raw_args", &self.raw_args)
            .field("child_decision", &self.child_decision)
            .finish_non_exhaustive()
    }
}

/// Reasons a subagent cannot be spawned.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SubagentError {
    /// Parent thread is itself a subagent and tried to spawn one.
    #[error("parent thread is already a subagent; recursion forbidden")]
    ParentIsSubagent,

    /// The subagent definition opted into spawning further subagents,
    /// which Claude Code's hard constraint forbids.
    #[error("subagent definition requested allow_subagent_spawn=true; recursion forbidden")]
    ChildSpawnRequested,

    /// The proposed child scope does not narrow the parent scope.
    #[error("child scope widens parent: {0}")]
    InvalidNarrowing(#[from] ScopeError),

    /// Building the child [`PermissionScope`] from JSON failed.
    ///
    /// This indicates that `PermissionScope`'s serde schema changed in a
    /// way that is incompatible with the JSON literal used to construct the
    /// candidate scope in [`prepare_child_scope`].
    #[error("failed to construct child PermissionScope: {0}")]
    ScopeConstruction(#[from] serde_json::Error),
}

/// Snapshot returned by [`prepare_child_scope`].
#[derive(Debug, Clone)]
pub struct ChildScope {
    /// Stable child thread id (allocated by the engine).
    pub thread_id: ThreadId,
    /// Narrowed [`PermissionScope`] that the child carries.
    pub scope: PermissionScope,
    /// Optional captured system prompt copied from the definition.
    pub prompt: String,
    /// Human-readable description copied from the definition.
    pub description: String,
}

/// Computes the child scope for a subagent spawn and validates the
/// Claude Code hard constraints.
///
/// `parent_is_subagent` should be `true` when the spawn request comes
/// from inside an existing subagent (in which case the call is
/// rejected outright).
///
/// # Errors
///
/// * [`SubagentError::ParentIsSubagent`] when `parent_is_subagent`
///   (Claude Code forbids subagent → subagent recursion).
/// * [`SubagentError::ChildSpawnRequested`] when the supplied
///   `definition.allow_subagent_spawn` is `true`; a freshly spawned
///   child must never opt back into the recursion path.
/// * [`SubagentError::InvalidNarrowing`] when the child scope widens
///   the parent scope.
pub fn prepare_child_scope(
    parent: &PermissionScope,
    parent_is_subagent: bool,
    definition: &SubagentDefinition,
    child_thread_id: ThreadId,
) -> Result<ChildScope, SubagentError> {
    if parent_is_subagent {
        return Err(SubagentError::ParentIsSubagent);
    }
    if definition.allow_subagent_spawn {
        return Err(SubagentError::ChildSpawnRequested);
    }

    // Resolve inheritance per D-008: `None` fields take the parent's
    // value, `Some` fields narrow it. `BypassPermissions` parents force
    // their mode onto every child (Claude Code safety hazard).
    let allowed_tools = definition
        .tools
        .clone()
        .or_else(|| parent.allowed_tools.clone());

    let mut disallowed_tools = parent.disallowed_tools.clone();
    for t in &definition.disallowed_tools {
        if !disallowed_tools.contains(t) {
            disallowed_tools.push(t.clone());
        }
    }

    let permission_mode = if matches!(
        parent.permission_mode,
        Some(PermissionMode::BypassPermissions)
    ) {
        Some(PermissionMode::BypassPermissions)
    } else {
        definition.permission_mode.or(parent.permission_mode)
    };

    // Build the candidate child scope through JSON because
    // `PermissionScope` is `#[non_exhaustive]` and has no constructor that
    // accepts all fields at once. A serde round-trip is the only stable way
    // to construct it without depending on private fields. If the schema
    // changes, this returns `SubagentError::ScopeConstruction` so the
    // engine can surface it as a tool-call failure rather than panicking.
    let candidate: PermissionScope = serde_json::from_value(serde_json::json!({
        "allowedTools": allowed_tools,
        "disallowedTools": disallowed_tools,
        "permissionMode": permission_mode,
        "allowSubagentSpawn": false,
    }))?;

    parent.narrowed_into(&candidate)?;

    Ok(ChildScope {
        thread_id: child_thread_id,
        scope: candidate,
        prompt: definition.prompt.clone(),
        description: definition.description.clone(),
    })
}

/// Reduces a subagent's transcript to the single item delivered to the
/// parent — by Claude Code convention, the last
/// [`Item::AgentMessage`] (falling back to the last
/// [`Item::SystemNotice`]).
///
/// Returns `None` when the transcript contains neither.
#[must_use]
pub fn extract_final_message(transcript: &[Item]) -> Option<Arc<Item>> {
    let mut final_msg: Option<&Item> = None;
    let mut fallback: Option<&Item> = None;
    for item in transcript {
        match item {
            Item::AgentMessage { .. } => final_msg = Some(item),
            Item::SystemNotice { .. } => fallback = Some(item),
            _ => {}
        }
    }
    final_msg.or(fallback).cloned().map(Arc::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhive_proto::domain::ItemId;

    fn parent_scope() -> PermissionScope {
        serde_json::from_value(serde_json::json!({
            "permissionMode": "default",
            "allowSubagentSpawn": true,
        }))
        .expect("scope fixture")
    }

    fn definition(spawn: bool) -> SubagentDefinition {
        serde_json::from_value(serde_json::json!({
            "name": "explorer",
            "description": "read-only scout",
            "prompt": "Look around.",
            "allowSubagentSpawn": spawn,
        }))
        .expect("definition fixture")
    }

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    #[test]
    fn recursion_rejected_when_parent_is_subagent() {
        let parent = parent_scope();
        let err = prepare_child_scope(
            &parent,
            true,
            &definition(false),
            tid("thread:native/child"),
        )
        .unwrap_err();
        assert!(matches!(err, SubagentError::ParentIsSubagent));
    }

    #[test]
    fn child_cannot_request_recursion() {
        let parent = parent_scope();
        let err = prepare_child_scope(
            &parent,
            false,
            &definition(true),
            tid("thread:native/child"),
        )
        .unwrap_err();
        assert!(matches!(err, SubagentError::ChildSpawnRequested));
    }

    #[test]
    fn happy_path_forces_no_spawn_on_child() {
        let parent = parent_scope();
        let child = prepare_child_scope(
            &parent,
            false,
            &definition(false),
            tid("thread:native/child"),
        )
        .unwrap();
        assert!(!child.scope.allow_subagent_spawn);
        assert_eq!(child.thread_id, tid("thread:native/child"));
    }

    #[test]
    fn extract_final_message_prefers_agent_message() {
        let items = vec![
            Item::SystemNotice {
                id: ItemId(Arc::from("n1")),
                level: zhive_proto::domain::NoticeLevel::Info,
                message: "ignored".into(),
            },
            Item::AgentMessage {
                id: ItemId(Arc::from("a1")),
                text: "final".into(),
            },
        ];
        let final_item = extract_final_message(&items).expect("must find final");
        match &*final_item {
            Item::AgentMessage { text, .. } => assert_eq!(text, "final"),
            other => panic!("expected AgentMessage, got {other:?}"),
        }
    }
}

// Rust guideline compliant 2026-02-21

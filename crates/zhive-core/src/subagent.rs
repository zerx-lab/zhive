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
use zhive_proto::permission::{PermissionMode, PermissionScope, ScopeError, SubagentDefinition};

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
/// `Suspended` is intentionally omitted from Phase 1 — the Defer /
/// parent-suspended path is tracked as TODO B8-O6 and will land in a
/// later increment.
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

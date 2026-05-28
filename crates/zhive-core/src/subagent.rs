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
use zhive_proto::domain::{Item, ThreadId};
use zhive_proto::permission::{PermissionMode, PermissionScope, ScopeError, SubagentDefinition};

/// Reasons a subagent cannot be spawned.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SubagentError {
    /// Parent thread is itself a subagent and tried to spawn one.
    #[error("subagent recursion forbidden (Claude Code hard constraint)")]
    RecursionForbidden,

    /// The proposed child scope does not narrow the parent scope.
    #[error("child scope widens parent: {0}")]
    InvalidNarrowing(#[from] ScopeError),
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
/// * [`SubagentError::RecursionForbidden`] when `parent_is_subagent`.
/// * [`SubagentError::InvalidNarrowing`] when the child scope widens
///   the parent scope.
pub fn prepare_child_scope(
    parent: &PermissionScope,
    parent_is_subagent: bool,
    definition: &SubagentDefinition,
    child_thread_id: ThreadId,
) -> Result<ChildScope, SubagentError> {
    if parent_is_subagent || definition.allow_subagent_spawn {
        return Err(SubagentError::RecursionForbidden);
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
    // `PermissionScope` is `#[non_exhaustive]`.
    let candidate: PermissionScope = serde_json::from_value(serde_json::json!({
        "allowedTools": allowed_tools,
        "disallowedTools": disallowed_tools,
        "permissionMode": permission_mode,
        "allowSubagentSpawn": false,
    }))
    .expect("scope fixture");

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
        assert!(matches!(err, SubagentError::RecursionForbidden));
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
        assert!(matches!(err, SubagentError::RecursionForbidden));
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

//! Permission, streaming and subagent wire schema (D-008 revised).
//!
//! This module is the single source of truth for three intertwined topics:
//!
//! * **Permission negotiation** — four-state [`PermissionDecision`] enum,
//!   the [`PermissionScope`] carrier that subagents inherit, and the
//!   reverse-RPC payloads aligned with ACP 0.12.
//! * **Streaming behaviour** — the [`StreamingBehavior`] tag used on the
//!   wire by `session/enqueue_steer` / `session/enqueue_follow_up`. The
//!   third Pi queue (`NextTurn`) is intentionally driven by its own
//!   `session/next_turn` method, **not** by this enum, so the wire
//!   surface still matches Pi's `streamingBehavior?: "steer" | "followUp"`
//!   string set.
//! * **Subagent inheritance** — [`SubagentDefinition`], the carrier that
//!   the client sends at thread start. Inheritance happens through
//!   `Option::None` semantics rather than an explicit
//!   `inherited_permissions` field, matching Claude Code's behaviour.
//!
//! # Cancellation outcome
//!
//! Per ACP 0.12 the server **must** resolve every pending
//! `permission/request` when a session is cancelled. The
//! [`PermissionOutcome::Cancelled`] variant is reserved for that path and
//! must never be selected by user choice.
//!
//! # Aborted notification
//!
//! The `session/aborted` notification reports which `steer` and
//! `follow_up` items the abort discarded; the `next_turn` queue is
//! preserved across aborts so the client can decide whether to resume.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(feature = "schema")]
use schemars::JsonSchema;

use crate::domain::{Item, ThreadId, TurnId};

// ============================================================
// PermissionDecision / PermissionMode / PermissionScope
// ============================================================

/// Four-state outcome of a permission check.
///
/// Aligned verbatim with Claude Code Agent SDK
/// `hookSpecificOutput.permissionDecision`. The folding order is
/// `Deny > Defer > Ask > Allow`: any `Deny` short-circuits, otherwise
/// the most restrictive remaining vote wins.
///
/// # Examples
///
/// ```
/// use zhive_proto::permission::{reduce, PermissionDecision};
/// let r = reduce(&[
///     PermissionDecision::Allow,
///     PermissionDecision::Ask,
/// ]);
/// assert_eq!(r, PermissionDecision::Ask);
///
/// let r = reduce(&[
///     PermissionDecision::Allow,
///     PermissionDecision::Deny,
///     PermissionDecision::Defer,
/// ]);
/// assert_eq!(r, PermissionDecision::Deny);
/// ```
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PermissionDecision {
    /// Hard refusal; the tool call must not execute. Highest priority.
    Deny,
    /// Suspend the turn pending follow-up user input.
    Defer,
    /// Ask the user via reverse RPC before proceeding.
    Ask,
    /// Allow the tool call. Lowest priority.
    Allow,
}

/// Folds a slice of hook decisions into a single outcome.
///
/// An empty slice resolves to [`PermissionDecision::Allow`] (no hook ⇒
/// no objection). Priority order is `Deny > Defer > Ask > Allow`.
///
/// # Examples
///
/// ```
/// use zhive_proto::permission::{reduce, PermissionDecision};
/// assert_eq!(reduce(&[]), PermissionDecision::Allow);
/// ```
#[must_use]
pub fn reduce(decisions: &[PermissionDecision]) -> PermissionDecision {
    use PermissionDecision::{Allow, Ask, Defer, Deny};
    let mut best = Allow;
    for &d in decisions {
        best = match (best, d) {
            (Deny, _) | (_, Deny) => Deny,
            (Defer, _) | (_, Defer) => Defer,
            (Ask, _) | (_, Ask) => Ask,
            _ => Allow,
        };
    }
    best
}

/// Permission mode aligned with Claude Code `permissionMode`.
///
/// `BypassPermissions` is the documented "safety hazard" mode: when the
/// parent thread runs in this mode, every subagent inherits it without
/// the ability to narrow.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum PermissionMode {
    /// Every tool call goes through the reducer and may trigger
    /// reverse-RPC.
    Default,
    /// Read-only checks still go through the reducer; edits are
    /// auto-allowed.
    AcceptEdits,
    /// All tool calls are auto-allowed (subagents inherit unconditionally).
    BypassPermissions,
    /// Conservative mode that denies by default; used by `plan` skill.
    Plan,
}

/// Opaque tool identifier used in scope lists.
///
/// A newtype keeps the wire shape (a plain JSON string) while letting
/// the engine swap in a richer type later without breaking the schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(transparent)]
pub struct ToolName(pub Arc<str>);

impl fmt::Display for ToolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Permission envelope inherited by a subagent.
///
/// `allowed_tools = None` means *inherit the parent set*; `Some(vec)`
/// means *exactly these tools*. `disallowed_tools` is union-merged with
/// the parent. `permission_mode = None` means *inherit*. The
/// `allow_subagent_spawn` flag is forced to `false` for child scopes —
/// Claude Code does not let a subagent spawn another subagent.
///
/// # Examples
///
/// ```
/// use zhive_proto::permission::PermissionScope;
/// let parent: PermissionScope =
///     serde_json::from_str(r#"{"allowSubagentSpawn": true}"#).unwrap();
/// // child narrows by clearing the spawn flag
/// let child: PermissionScope = serde_json::from_str("{}").unwrap();
/// assert!(parent.narrowed_into(&child).is_ok());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PermissionScope {
    /// `None` inherits the parent set; `Some` is an explicit allowlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<ToolName>>,

    /// Always union-merged with the parent denylist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disallowed_tools: Vec<ToolName>,

    /// `None` inherits the parent mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,

    /// Forced to `false` for child scopes by [`narrowed_into`].
    ///
    /// [`narrowed_into`]: PermissionScope::narrowed_into
    #[serde(default)]
    pub allow_subagent_spawn: bool,
}

/// Reason an inherited scope is rejected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase", tag = "kind")]
#[non_exhaustive]
pub enum ScopeError {
    /// Child listed a tool absent from the parent allowlist.
    #[error("tool {tool} not present in parent allowlist")]
    ToolNotInherited {
        /// Offending tool name.
        tool: ToolName,
    },
    /// Parent restricts tools but the child omits the allowlist
    /// (equivalent to widening).
    #[error("child must repeat the parent allowlist when narrowing")]
    ChildMustExplicitlyNarrow,
    /// Child dropped a parent denylist entry.
    #[error("tool {tool} present in parent denylist but missing in child")]
    DisallowedToolDropped {
        /// Offending tool name.
        tool: ToolName,
    },
    /// Child requested a broader permission mode than the parent.
    #[error("child mode {child:?} widens parent mode {parent:?}")]
    ModeWidened {
        /// Parent mode.
        parent: PermissionMode,
        /// Attempted child mode.
        child: PermissionMode,
    },
    /// Child set `allow_subagent_spawn = true`, forbidden by Claude Code.
    #[error("subagent recursion forbidden")]
    RecursionForbidden,
}

impl PermissionScope {
    /// Returns a conservative default scope suitable for engine-internal turns.
    ///
    /// All tool access is allowed (no allowlist or denylist), no specific
    /// permission mode is set, and subagent spawning is disabled.
    /// Used by the engine when the client does not supply an explicit scope.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::permission::PermissionScope;
    /// let scope = PermissionScope::default_turn_scope();
    /// assert!(scope.allowed_tools.is_none());
    /// assert!(!scope.allow_subagent_spawn);
    /// ```
    #[must_use]
    pub fn default_turn_scope() -> Self {
        Self {
            allowed_tools: None,
            disallowed_tools: vec![],
            permission_mode: None,
            allow_subagent_spawn: false,
        }
    }

    /// Returns `true` when `tool_name` is permitted by this scope.
    ///
    /// A tool is permitted when it is **not** in `disallowed_tools` and, when
    /// `allowed_tools` is `Some`, it appears in that allowlist. `disallowed`
    /// always wins over `allowed`, so a tool present in both is rejected.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::permission::{PermissionScope, ToolName};
    /// let mut scope = PermissionScope::default_turn_scope();
    /// assert!(scope.permits("read"), "default scope permits everything");
    /// scope.disallowed_tools.push(ToolName(Arc::from("bash")));
    /// assert!(!scope.permits("bash"), "disallowed tool is rejected");
    /// assert!(scope.permits("read"));
    /// ```
    #[must_use]
    pub fn permits(&self, tool_name: &str) -> bool {
        if self
            .disallowed_tools
            .iter()
            .any(|t| t.0.as_ref() == tool_name)
        {
            return false;
        }
        match &self.allowed_tools {
            Some(allow) => allow.iter().any(|t| t.0.as_ref() == tool_name),
            None => true,
        }
    }

    /// Returns `Ok` when `child` is a legal narrowing of `self`.
    ///
    /// # Errors
    ///
    /// See [`ScopeError`] for the rejection reasons. Subagent spawning
    /// recursively is always rejected.
    pub fn narrowed_into(&self, child: &PermissionScope) -> Result<(), ScopeError> {
        if let (Some(parent_set), Some(child_set)) = (&self.allowed_tools, &child.allowed_tools) {
            for t in child_set {
                if !parent_set.contains(t) {
                    return Err(ScopeError::ToolNotInherited { tool: t.clone() });
                }
            }
        } else if self.allowed_tools.is_some() && child.allowed_tools.is_none() {
            return Err(ScopeError::ChildMustExplicitlyNarrow);
        }

        for t in &self.disallowed_tools {
            if !child.disallowed_tools.contains(t) {
                return Err(ScopeError::DisallowedToolDropped { tool: t.clone() });
            }
        }

        match (self.permission_mode, child.permission_mode) {
            (Some(parent), Some(c)) if !mode_narrows(parent, c) => {
                return Err(ScopeError::ModeWidened { parent, child: c });
            }
            _ => {}
        }

        if child.allow_subagent_spawn {
            return Err(ScopeError::RecursionForbidden);
        }

        Ok(())
    }
}

/// Returns `true` when `child` is at most as permissive as `parent`.
const fn mode_narrows(parent: PermissionMode, child: PermissionMode) -> bool {
    rank(child) <= rank(parent)
}

const fn rank(m: PermissionMode) -> u8 {
    match m {
        PermissionMode::BypassPermissions => 3,
        PermissionMode::AcceptEdits => 2,
        PermissionMode::Default => 1,
        PermissionMode::Plan => 0,
    }
}

// ============================================================
// StreamingBehavior
// ============================================================

/// Wire tag for the two mid-turn injection queues.
///
/// `Steer` and `FollowUp` ride on the existing
/// `streamingBehavior?: "steer" | "followUp"` string in the Pi wire
/// schema. The third zhive queue (`NextTurn`) is intentionally **not**
/// part of this enum: it is driven by the dedicated `session/next_turn`
/// RPC so the wire shape stays compatible with Pi.
///
/// # Examples
///
/// ```
/// use zhive_proto::permission::StreamingBehavior;
/// let s = serde_json::to_string(&StreamingBehavior::FollowUp).unwrap();
/// assert_eq!(s, "\"followUp\"");
/// ```
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum StreamingBehavior {
    /// Item is appended to the `steer` queue; drained before the next
    /// LLM request inside the active turn.
    Steer,
    /// Item is appended to the `follow_up` queue; drained when the agent
    /// is about to stop, keeping the turn alive.
    FollowUp,
}

// ============================================================
// Subagent definition
// ============================================================

/// Subagent declaration the client sends to define a child thread.
///
/// Inheritance happens entirely through `Option::None` semantics — there
/// is **no** `inherited_permissions` field on the wire. Mirrors Claude
/// Code `AgentDefinition`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SubagentDefinition {
    /// Unique handle (used for routing log lines back to the subagent).
    pub name: String,
    /// Free-form description surfaced to the user.
    pub description: String,
    /// System prompt prepended to the subagent's fresh context.
    pub prompt: String,
    /// `None` inherits the parent allowlist; `Some` is an explicit subset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolName>>,
    /// Always union-merged with the parent denylist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disallowed_tools: Vec<ToolName>,
    /// `None` inherits the parent permission mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    /// Forced to `false` by D-008 when spawning a subagent.
    #[serde(default)]
    pub allow_subagent_spawn: bool,
}

// ============================================================
// permission/request reverse RPC
// ============================================================

/// Server-to-client `session/request_permission` request body.
///
/// Mirrors ACP 0.12 `RequestPermissionRequest`. The server uses this when
/// a hook returns [`PermissionDecision::Ask`] and a user choice is
/// required.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RequestPermissionRequest {
    /// Thread the request applies to.
    pub thread_id: ThreadId,
    /// Resource class (e.g. `"tool"`, `"file"`).
    pub resource_type: String,
    /// Specific resource name (tool name, path, …).
    pub name: String,
    /// Human-readable explanation shown to the user.
    pub reason: String,
    /// Available option set; order matters for UI rendering.
    pub options: Vec<PermissionOption>,
}

/// One choice surfaced by [`RequestPermissionRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PermissionOption {
    /// Stable id echoed back in [`PermissionOutcome::Selected`].
    pub id: String,
    /// Semantic classification of the option.
    pub kind: PermissionOptionKind,
    /// Display label.
    pub description: String,
}

/// Semantic classification of a [`PermissionOption`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[non_exhaustive]
pub enum PermissionOptionKind {
    /// One-shot allow.
    AllowOnce,
    /// Persistent allow (caller stores).
    AllowAlways,
    /// One-shot reject.
    RejectOnce,
    /// Persistent reject (caller stores).
    RejectAlways,
}

/// Outcome the client returns for a `permission/request` reverse RPC.
///
/// The `Cancelled` variant is **mandatory** when a session is cancelled
/// with a permission request still in flight (ACP 0.12 schema). The
/// server must inject `Cancelled` itself if the client disconnects.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "outcome", rename_all = "camelCase")]
#[non_exhaustive]
pub enum PermissionOutcome {
    /// User picked the option with the given id.
    Selected {
        /// Echoes [`PermissionOption::id`].
        option_id: String,
    },
    /// Session was cancelled before the user answered.
    Cancelled,
}

// ============================================================
// session/aborted notification
// ============================================================

/// Payload of the `session/aborted` server-to-client notification.
///
/// Lists which items the abort discarded (`cleared_steer`,
/// `cleared_follow_up`) and how many `next_turn` items survived the
/// abort. The next-turn count lets the client decide whether to resume
/// or to flush the queue with `session/clear_next_turn`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SessionAbortedNotification {
    /// Aborted thread.
    pub thread_id: ThreadId,
    /// Turn id when the abort fired, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    /// Items the abort drained from the `steer` queue.
    #[serde(default)]
    pub cleared_steer: Vec<Item>,
    /// Items the abort drained from the `follow_up` queue.
    #[serde(default)]
    pub cleared_follow_up: Vec<Item>,
    /// Count of `next_turn` items preserved across the abort.
    pub next_turn_retained_count: u32,
}

impl SessionAbortedNotification {
    /// Builds a notification with empty queue snapshots.
    ///
    /// Use this when the caller has no `cleared_steer` /
    /// `cleared_follow_up` items to report; mutate the fields directly
    /// when reporting drained queues. The `#[non_exhaustive]` attribute
    /// prevents external record-struct construction, so this helper is
    /// the supported way to build instances from `zhive-core`.
    #[must_use]
    pub fn new(thread_id: ThreadId, turn_id: Option<TurnId>) -> Self {
        Self {
            thread_id,
            turn_id,
            cleared_steer: Vec::new(),
            cleared_follow_up: Vec::new(),
            next_turn_retained_count: 0,
        }
    }
}

// ============================================================
// hook output (Claude Code shape)
// ============================================================

/// Top-level hook callback output, byte-aligned with Claude Code.
///
/// Every field is optional. The host treats `None` as *no opinion*.
///
/// # Examples
///
/// ```
/// use zhive_proto::permission::{HookOutput, HookSpecificOutput, PermissionDecision};
/// let payload = r#"{
///     "hookSpecificOutput": {
///         "hookEventName": "PreToolUse",
///         "permissionDecision": "allow"
///     }
/// }"#;
/// let out: HookOutput = serde_json::from_str(payload).unwrap();
/// assert!(out.system_message.is_none());
/// assert!(matches!(
///     out.hook_specific_output,
///     Some(HookSpecificOutput::PreToolUse {
///         permission_decision: PermissionDecision::Allow,
///         ..
///     })
/// ));
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct HookOutput {
    /// Free-form message surfaced to the user transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
    /// Whether the agent loop should continue after this hook.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "continue")]
    pub continue_loop: Option<bool>,
    /// Run the hook in fire-and-forget mode.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "async")]
    pub async_mode: Option<bool>,
    /// Async-mode timeout in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub async_timeout: Option<u64>,
    /// Event-specific payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

/// Event-specific portion of a [`HookOutput`].
///
/// The discriminator key is `hookEventName`, matching Claude Code wire
/// verbatim (the field is both the discriminator *and* a payload field
/// on the JSON object).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "hookEventName")]
#[non_exhaustive]
pub enum HookSpecificOutput {
    /// Decision and optional mutation for a pre-tool-use hook.
    #[serde(rename = "PreToolUse", rename_all = "camelCase")]
    PreToolUse {
        /// Required four-state vote.
        permission_decision: PermissionDecision,
        /// Optional rationale shown to the user.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_decision_reason: Option<String>,
        /// Optional replacement input. Ignored when
        /// `permission_decision == Defer`; the host re-validates the
        /// schema before dispatching.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_input: Option<Value>,
    },
    /// Optional context / output rewrite for a post-tool-use hook.
    #[serde(rename = "PostToolUse", rename_all = "camelCase")]
    PostToolUse {
        /// Extra context to splice into the transcript.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        additional_context: Option<String>,
        /// Replacement payload that the agent sees instead of the raw
        /// tool output.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_tool_output: Option<Value>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduce_priority_order() {
        use PermissionDecision::{Allow, Ask, Defer, Deny};
        assert_eq!(reduce(&[]), Allow);
        assert_eq!(reduce(&[Allow, Allow]), Allow);
        assert_eq!(reduce(&[Allow, Ask]), Ask);
        assert_eq!(reduce(&[Ask, Defer]), Defer);
        assert_eq!(reduce(&[Defer, Deny, Allow]), Deny);
    }

    #[test]
    fn scope_recursion_rejected() {
        let parent = PermissionScope {
            allowed_tools: None,
            disallowed_tools: Vec::new(),
            permission_mode: None,
            allow_subagent_spawn: true,
        };
        let child = PermissionScope {
            allowed_tools: None,
            disallowed_tools: Vec::new(),
            permission_mode: None,
            allow_subagent_spawn: true,
        };
        assert!(matches!(
            parent.narrowed_into(&child),
            Err(ScopeError::RecursionForbidden)
        ));
    }

    #[test]
    fn scope_mode_widening_rejected() {
        let parent = PermissionScope {
            allowed_tools: None,
            disallowed_tools: Vec::new(),
            permission_mode: Some(PermissionMode::Default),
            allow_subagent_spawn: true,
        };
        let child = PermissionScope {
            allowed_tools: None,
            disallowed_tools: Vec::new(),
            permission_mode: Some(PermissionMode::BypassPermissions),
            allow_subagent_spawn: false,
        };
        assert!(matches!(
            parent.narrowed_into(&child),
            Err(ScopeError::ModeWidened { .. })
        ));
    }

    #[test]
    fn streaming_behavior_wire_form() {
        assert_eq!(
            serde_json::to_string(&StreamingBehavior::Steer).unwrap(),
            "\"steer\""
        );
        assert_eq!(
            serde_json::to_string(&StreamingBehavior::FollowUp).unwrap(),
            "\"followUp\""
        );
    }

    #[test]
    fn permission_outcome_cancelled_round_trip() {
        let v = serde_json::to_value(&PermissionOutcome::Cancelled).unwrap();
        assert_eq!(v["outcome"], "cancelled");
        let back: PermissionOutcome = serde_json::from_value(v).unwrap();
        assert_eq!(back, PermissionOutcome::Cancelled);
    }

    #[test]
    fn hook_specific_output_pre_tool_use_wire_form() {
        let out = HookSpecificOutput::PreToolUse {
            permission_decision: PermissionDecision::Allow,
            permission_decision_reason: Some("user trusted".into()),
            updated_input: None,
        };
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(v["hookEventName"], "PreToolUse");
        assert_eq!(v["permissionDecision"], "allow");
        assert_eq!(v["permissionDecisionReason"], "user trusted");
    }
}

// Rust guideline compliant 2026-02-21

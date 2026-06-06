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
    /// ACP tool-call id this permission applies to.
    ///
    /// The provider-assigned tool-use id (or its synthetic fallback) of the
    /// call awaiting authorization, so a client can correlate the permission
    /// prompt with the tool-call card it already announced. `None` for callers
    /// that pre-date this field (e.g. resource-scoped requests with no call id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
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
/// The `Defer` variant is a zhive-private extension that lets a hook
/// suspend the turn until follow-up user input arrives; clients resume it
/// later via `session/resume_permission` (see [`ResumeOutcome`] and
/// [`ResumePermissionParams`]).
///
/// # Examples
///
/// ```
/// use zhive_proto::permission::PermissionOutcome;
/// let v = serde_json::to_value(&PermissionOutcome::Defer {
///     reason: Some("awaiting user".into()),
/// })
/// .unwrap();
/// assert_eq!(v["outcome"], "defer");
/// assert_eq!(v["reason"], "awaiting user");
/// let back: PermissionOutcome = serde_json::from_value(v).unwrap();
/// assert!(matches!(back, PermissionOutcome::Defer { .. }));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "outcome", rename_all = "camelCase")]
#[non_exhaustive]
pub enum PermissionOutcome {
    /// User picked the option with the given id.
    #[serde(rename_all = "camelCase")]
    Selected {
        /// Echoes [`PermissionOption::id`].
        option_id: String,
    },
    /// Session was cancelled before the user answered.
    Cancelled,
    /// Hook suspended the turn pending follow-up user input.
    ///
    /// Carries an optional human-readable rationale surfaced to the user
    /// while the turn is parked. The turn is later unblocked by
    /// `session/resume_permission`; a [`ResumeOutcome`] is **structurally
    /// prevented** from deferring again, so a resumed request can never
    /// re-suspend.
    Defer {
        /// Optional rationale shown while the turn is suspended.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

// ============================================================
// Permission defer / resume wire
// ============================================================

/// Method name for the server-to-client `session/request_permission` reverse RPC.
///
/// The server sends this when a hook returns [`PermissionDecision::Ask`] and
/// requires a live user decision. The client replies with
/// [`PermissionOutcome`] on the same request channel. The body is
/// [`RequestPermissionRequest`].
///
/// **Note:** as of Phase 1 the engine broadcasts a
/// `events/permission_requested` event instead of using this reverse-RPC
/// path. This constant is reserved for the Phase B reverse-RPC
/// implementation; the `client-native` adapter is already wired to it.
///
/// # Examples
///
/// ```
/// use zhive_proto::permission::METHOD_REQUEST_PERMISSION;
/// assert_eq!(METHOD_REQUEST_PERMISSION, "session/request_permission");
/// ```
pub const METHOD_REQUEST_PERMISSION: &str = "session/request_permission";

/// Method name for the client-to-server `session/resume_permission` request.
///
/// Sent to unblock a turn that a hook previously suspended with
/// [`PermissionOutcome::Defer`]. The body is [`ResumePermissionParams`].
///
/// # Examples
///
/// ```
/// use zhive_proto::permission::METHOD_RESUME_PERMISSION;
/// assert_eq!(METHOD_RESUME_PERMISSION, "session/resume_permission");
/// ```
pub const METHOD_RESUME_PERMISSION: &str = "session/resume_permission";

/// Method name for the server-to-client `events/turn_suspended` notification.
///
/// Emitted when a turn parks on a deferred permission request; the body is
/// [`TurnSuspendedNotification`]. Lives in the `events/` namespace
/// alongside the other engine event notifications.
///
/// # Examples
///
/// ```
/// use zhive_proto::permission::METHOD_TURN_SUSPENDED;
/// assert_eq!(METHOD_TURN_SUSPENDED, "events/turn_suspended");
/// ```
pub const METHOD_TURN_SUSPENDED: &str = "events/turn_suspended";

/// Method name for the server-to-client `events/turn_resumed` notification.
///
/// Emitted when a previously suspended turn is unblocked; the body is
/// [`TurnResumedNotification`]. Lives in the `events/` namespace alongside
/// the other engine event notifications.
///
/// # Examples
///
/// ```
/// use zhive_proto::permission::METHOD_TURN_RESUMED;
/// assert_eq!(METHOD_TURN_RESUMED, "events/turn_resumed");
/// ```
pub const METHOD_TURN_RESUMED: &str = "events/turn_resumed";

/// Resolution a client supplies to `session/resume_permission`.
///
/// Deliberately narrower than [`PermissionOutcome`]: it offers only
/// `Selected` and `Cancelled`, so a resumed request can never defer
/// again. This type-level restriction is the only guard against an
/// infinite suspend loop; see the [`From`] conversion that lifts a
/// `ResumeOutcome` back into a [`PermissionOutcome`].
///
/// # Examples
///
/// ```
/// use zhive_proto::permission::ResumeOutcome;
/// let v = serde_json::to_value(&ResumeOutcome::Selected {
///     option_id: "allow_once".into(),
/// })
/// .unwrap();
/// assert_eq!(v["outcome"], "selected");
/// assert_eq!(v["optionId"], "allow_once");
///
/// // A `defer` payload is rejected — resume can never re-suspend.
/// let deferred = serde_json::json!({ "outcome": "defer" });
/// assert!(serde_json::from_value::<ResumeOutcome>(deferred).is_err());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "outcome", rename_all = "camelCase")]
#[non_exhaustive]
pub enum ResumeOutcome {
    /// User picked the option with the given id.
    #[serde(rename_all = "camelCase")]
    Selected {
        /// Echoes [`PermissionOption::id`].
        option_id: String,
    },
    /// User cancelled instead of choosing an option.
    Cancelled,
}

impl From<ResumeOutcome> for PermissionOutcome {
    /// Lifts a resume resolution into the broader permission outcome.
    ///
    /// The mapping is total and cannot produce [`PermissionOutcome::Defer`],
    /// which is what makes re-suspension impossible at the type level.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::permission::{PermissionOutcome, ResumeOutcome};
    /// let out: PermissionOutcome = ResumeOutcome::Cancelled.into();
    /// assert_eq!(out, PermissionOutcome::Cancelled);
    /// ```
    fn from(value: ResumeOutcome) -> Self {
        match value {
            ResumeOutcome::Selected { option_id } => Self::Selected { option_id },
            ResumeOutcome::Cancelled => Self::Cancelled,
        }
    }
}

/// Body of the `session/resume_permission` request.
///
/// Pairs the in-flight request id with the user's [`ResumeOutcome`]. The
/// `request_id` echoes the id the server surfaced when it suspended the
/// turn (carried by [`TurnSuspendedNotification::request_id`]).
///
/// # Examples
///
/// ```
/// use zhive_proto::permission::{ResumeOutcome, ResumePermissionParams};
/// let payload = r#"{
///     "requestId": "perm:1",
///     "outcome": { "outcome": "selected", "optionId": "allow_once" }
/// }"#;
/// let params: ResumePermissionParams = serde_json::from_str(payload).unwrap();
/// assert_eq!(params.request_id, "perm:1");
/// assert!(matches!(params.outcome, ResumeOutcome::Selected { .. }));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ResumePermissionParams {
    /// Pending request id echoed from the suspend notification.
    pub request_id: String,
    /// Client's resolution; cannot defer again (see [`ResumeOutcome`]).
    pub outcome: ResumeOutcome,
}

impl ResumePermissionParams {
    /// Builds a resume request body from a request id and outcome.
    ///
    /// `#[non_exhaustive]` blocks external record-struct construction, so
    /// this is the supported way to build instances outside the crate.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::permission::{ResumeOutcome, ResumePermissionParams};
    /// let params = ResumePermissionParams::new("perm:1", ResumeOutcome::Cancelled);
    /// assert_eq!(params.request_id, "perm:1");
    /// ```
    #[must_use]
    pub fn new(request_id: impl Into<String>, outcome: ResumeOutcome) -> Self {
        Self {
            request_id: request_id.into(),
            outcome,
        }
    }
}

/// Payload of the `events/turn_suspended` server-to-client notification.
///
/// Reports that the named turn parked on a deferred permission request.
/// The client unblocks it by calling `session/resume_permission` with the
/// same `request_id`. Without a resume (or a `session/cancel`) the turn
/// stays suspended indefinitely — the engine applies no timeout.
///
/// # Examples
///
/// ```
/// use zhive_proto::domain::{ThreadId, TurnId};
/// use zhive_proto::permission::TurnSuspendedNotification;
/// use std::sync::Arc;
/// let n = TurnSuspendedNotification::new(
///     ThreadId(Arc::from("thread:native/abc")),
///     TurnId(Arc::from("turn:thread:native/abc/0")),
///     "perm:1",
///     Some("awaiting user".into()),
///     1_700_000_000,
/// );
/// let v = serde_json::to_value(&n).unwrap();
/// assert_eq!(v["threadId"], "thread:native/abc");
/// assert_eq!(v["requestId"], "perm:1");
/// assert_eq!(v["suspendedAt"], 1_700_000_000_i64);
/// let back: TurnSuspendedNotification = serde_json::from_value(v).unwrap();
/// assert_eq!(back, n);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TurnSuspendedNotification {
    /// Thread whose turn suspended.
    pub thread_id: ThreadId,
    /// Suspended turn.
    pub turn_id: TurnId,
    /// Pending request id the client passes back to resume.
    pub request_id: String,
    /// Optional rationale mirrored from [`PermissionOutcome::Defer`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Unix timestamp in seconds when the turn suspended.
    pub suspended_at: i64,
}

impl TurnSuspendedNotification {
    /// Builds a suspend notification from its component fields.
    ///
    /// `#[non_exhaustive]` blocks external record-struct construction, so
    /// this is the supported way to build instances from `zhive-core`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::domain::{ThreadId, TurnId};
    /// use zhive_proto::permission::TurnSuspendedNotification;
    /// use std::sync::Arc;
    /// let n = TurnSuspendedNotification::new(
    ///     ThreadId(Arc::from("thread:native/abc")),
    ///     TurnId(Arc::from("turn:thread:native/abc/0")),
    ///     "perm:1",
    ///     None,
    ///     1_700_000_000,
    /// );
    /// assert_eq!(n.request_id, "perm:1");
    /// ```
    #[must_use]
    pub fn new(
        thread_id: ThreadId,
        turn_id: TurnId,
        request_id: impl Into<String>,
        reason: Option<String>,
        suspended_at: i64,
    ) -> Self {
        Self {
            thread_id,
            turn_id,
            request_id: request_id.into(),
            reason,
            suspended_at,
        }
    }
}

/// Payload of the `events/turn_resumed` server-to-client notification.
///
/// Dual of [`TurnSuspendedNotification`]: reports that a previously
/// suspended turn was unblocked and resumed execution.
///
/// # Examples
///
/// ```
/// use zhive_proto::domain::{ThreadId, TurnId};
/// use zhive_proto::permission::TurnResumedNotification;
/// use std::sync::Arc;
/// let n = TurnResumedNotification::new(
///     ThreadId(Arc::from("thread:native/abc")),
///     TurnId(Arc::from("turn:thread:native/abc/0")),
///     1_700_000_005,
/// );
/// let v = serde_json::to_value(&n).unwrap();
/// assert_eq!(v["threadId"], "thread:native/abc");
/// assert_eq!(v["resumedAt"], 1_700_000_005_i64);
/// let back: TurnResumedNotification = serde_json::from_value(v).unwrap();
/// assert_eq!(back, n);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TurnResumedNotification {
    /// Thread whose turn resumed.
    pub thread_id: ThreadId,
    /// Resumed turn.
    pub turn_id: TurnId,
    /// Unix timestamp in seconds when the turn resumed.
    pub resumed_at: i64,
}

impl TurnResumedNotification {
    /// Builds a resume notification from its component fields.
    ///
    /// `#[non_exhaustive]` blocks external record-struct construction, so
    /// this is the supported way to build instances from `zhive-core`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::domain::{ThreadId, TurnId};
    /// use zhive_proto::permission::TurnResumedNotification;
    /// use std::sync::Arc;
    /// let n = TurnResumedNotification::new(
    ///     ThreadId(Arc::from("thread:native/abc")),
    ///     TurnId(Arc::from("turn:thread:native/abc/0")),
    ///     1_700_000_005,
    /// );
    /// assert_eq!(n.resumed_at, 1_700_000_005);
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, turn_id: TurnId, resumed_at: i64) -> Self {
        Self {
            thread_id,
            turn_id,
            resumed_at,
        }
    }
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
    fn request_permission_tool_call_id_round_trips_and_is_optional() {
        // Present: serializes as camelCase `toolCallId` and round-trips.
        let req: RequestPermissionRequest = serde_json::from_value(serde_json::json!({
            "threadId": "thread:native/t",
            "resourceType": "tool",
            "name": "bash",
            "toolCallId": "toolu_1",
            "reason": "run a command",
            "options": []
        }))
        .expect("with id");
        assert_eq!(req.tool_call_id.as_deref(), Some("toolu_1"));
        let v = serde_json::to_value(&req).expect("serialize");
        assert_eq!(v["toolCallId"], "toolu_1");

        // Absent: old callers omit the key; it defaults to None and is skipped
        // on the wire so the form stays backward compatible.
        let legacy: RequestPermissionRequest = serde_json::from_value(serde_json::json!({
            "threadId": "thread:native/t",
            "resourceType": "tool",
            "name": "bash",
            "reason": "run a command",
            "options": []
        }))
        .expect("without id");
        assert_eq!(legacy.tool_call_id, None);
        let v = serde_json::to_value(&legacy).expect("serialize");
        assert!(v.get("toolCallId").is_none(), "None must be omitted");
    }

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
    fn permission_outcome_defer_round_trip() {
        let outcome = PermissionOutcome::Defer {
            reason: Some("awaiting user".into()),
        };
        let v = serde_json::to_value(&outcome).unwrap();
        assert_eq!(v["outcome"], "defer");
        assert_eq!(v["reason"], "awaiting user");
        let back: PermissionOutcome = serde_json::from_value(v).unwrap();
        assert_eq!(back, outcome);
    }

    #[test]
    fn permission_outcome_defer_omits_none_reason() {
        let v = serde_json::to_value(&PermissionOutcome::Defer { reason: None }).unwrap();
        assert_eq!(v["outcome"], "defer");
        assert!(v.get("reason").is_none(), "None reason is skipped on wire");
    }

    #[test]
    fn resume_outcome_round_trip() {
        let outcome = ResumeOutcome::Selected {
            option_id: "allow_once".into(),
        };
        let v = serde_json::to_value(&outcome).unwrap();
        assert_eq!(v["outcome"], "selected");
        assert_eq!(v["optionId"], "allow_once");
        let back: ResumeOutcome = serde_json::from_value(v).unwrap();
        assert_eq!(back, outcome);

        let v = serde_json::to_value(ResumeOutcome::Cancelled).unwrap();
        assert_eq!(v["outcome"], "cancelled");
    }

    #[test]
    fn resume_outcome_rejects_defer() {
        // The whole point of the narrower enum: a resumed request can
        // never re-suspend, so a `defer` payload must fail to deserialize.
        let deferred = serde_json::json!({ "outcome": "defer" });
        assert!(serde_json::from_value::<ResumeOutcome>(deferred).is_err());
    }

    #[test]
    fn resume_outcome_into_permission_outcome() {
        let selected: PermissionOutcome = ResumeOutcome::Selected {
            option_id: "allow_once".into(),
        }
        .into();
        assert_eq!(
            selected,
            PermissionOutcome::Selected {
                option_id: "allow_once".into()
            }
        );

        let cancelled: PermissionOutcome = ResumeOutcome::Cancelled.into();
        assert_eq!(cancelled, PermissionOutcome::Cancelled);
    }

    #[test]
    fn resume_permission_params_deserialize() {
        let payload = r#"{
            "requestId": "perm:1",
            "outcome": { "outcome": "selected", "optionId": "allow_once" }
        }"#;
        let params: ResumePermissionParams = serde_json::from_str(payload).unwrap();
        assert_eq!(params.request_id, "perm:1");
        assert_eq!(
            params.outcome,
            ResumeOutcome::Selected {
                option_id: "allow_once".into()
            }
        );
    }

    #[test]
    fn turn_suspended_notification_camel_case_round_trip() {
        let n = TurnSuspendedNotification {
            thread_id: ThreadId(Arc::from("thread:native/abc")),
            turn_id: TurnId(Arc::from("turn:thread:native/abc/0")),
            request_id: "perm:1".into(),
            reason: Some("awaiting user".into()),
            suspended_at: 1_700_000_000,
        };
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["threadId"], "thread:native/abc");
        assert_eq!(v["turnId"], "turn:thread:native/abc/0");
        assert_eq!(v["requestId"], "perm:1");
        assert_eq!(v["reason"], "awaiting user");
        assert_eq!(v["suspendedAt"], 1_700_000_000_i64);
        let back: TurnSuspendedNotification = serde_json::from_value(v).unwrap();
        assert_eq!(back, n);
    }

    #[test]
    fn turn_resumed_notification_camel_case_round_trip() {
        let n = TurnResumedNotification {
            thread_id: ThreadId(Arc::from("thread:native/abc")),
            turn_id: TurnId(Arc::from("turn:thread:native/abc/0")),
            resumed_at: 1_700_000_005,
        };
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["threadId"], "thread:native/abc");
        assert_eq!(v["turnId"], "turn:thread:native/abc/0");
        assert_eq!(v["resumedAt"], 1_700_000_005_i64);
        let back: TurnResumedNotification = serde_json::from_value(v).unwrap();
        assert_eq!(back, n);
    }

    #[test]
    fn resume_permission_method_constants() {
        assert_eq!(METHOD_RESUME_PERMISSION, "session/resume_permission");
        assert_eq!(METHOD_TURN_SUSPENDED, "events/turn_suspended");
        assert_eq!(METHOD_TURN_RESUMED, "events/turn_resumed");
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

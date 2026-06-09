//! Hook event payloads (D-012 plus the proposed 15th event).
//!
//! The 14 events listed in D-012 plus the proposed 15th [`PhaseTransition`]
//! are encoded as a single [`HookEvent`] enum tagged by `hook_event_name`.
//! Every payload carries a [`HookEventBase`] flattened at the JSON root so
//! generic fields (session, cwd, `registered_by`, …) live alongside the
//! event-specific ones, matching Claude Code Agent SDK wire shape.
//!
//! # Why `registered_by` is mandatory
//!
//! Red line 10: every hook payload must trace back to the
//! [`ExtensionRef`] that registered it. Settings-level loose
//! registration is rejected; hooks ship through extension manifests
//! only. This module enforces the contract at the type level by making
//! the field required on the base struct.
//!
//! # Unknown events
//!
//! [`HookEvent`] is `#[non_exhaustive]` and decodes any unrecognised
//! `hook_event_name` into the [`HookEvent::Unknown`] variant rather than
//! failing — a future SDK event must never crash the dispatch loop (red
//! line A4). The variant is produced **only** by deserialization: it is
//! not subscribable, the dispatch loop never constructs it, and it has no
//! defined serialization (the [`Serialize`] derive skips it). A
//! hand-written [`serde::Deserialize`] impl reads the tag first, routes
//! known events through the typed payload structs, and parks everything
//! else (or a missing tag) under `Unknown` with the full payload
//! preserved.
//!
//! [`PhaseTransition`]: HookEvent::PhaseTransition

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(feature = "schema")]
use schemars::JsonSchema;

use crate::permission::PermissionMode;

// ============================================================
// ExtensionRef + ExtensionSource
// ============================================================

/// Provenance record attached to every hook invocation.
///
/// Always present on [`HookEventBase::registered_by`]; the host emits
/// the value from the manifest that owned the registration.
///
/// # Examples
///
/// ```
/// use zhive_proto::hook::ExtensionRef;
/// let r: ExtensionRef = serde_json::from_str(
///     r#"{"id": "git-helper", "version": "0.1.0", "source": "project"}"#,
/// )
/// .unwrap();
/// assert_eq!(r.to_string(), "git-helper@0.1.0");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ExtensionRef {
    /// Stable id (e.g. `"git-helper"`).
    pub id: String,
    /// Semver string of the manifest that registered the hook.
    pub version: String,
    /// Discovery source of the manifest.
    pub source: ExtensionSource,
}

impl ExtensionRef {
    /// Builds a provenance record from its parts.
    ///
    /// Hosts mint this from the manifest that owns a registration so every
    /// contribution carries `registered_by` (red line 10), without resorting
    /// to a JSON round-trip for the `#[non_exhaustive]` struct.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::hook::{ExtensionRef, ExtensionSource};
    /// let r = ExtensionRef::new("git-helper", "0.1.0", ExtensionSource::Project);
    /// assert_eq!(r.to_string(), "git-helper@0.1.0");
    /// ```
    #[must_use]
    pub fn new(id: impl Into<String>, version: impl Into<String>, source: ExtensionSource) -> Self {
        Self {
            id: id.into(),
            version: version.into(),
            source,
        }
    }
}

impl std::fmt::Display for ExtensionRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.id, self.version)
    }
}

/// Discovery source of an [`ExtensionRef`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExtensionSource {
    /// Bundled with the zhive binary.
    Builtin,
    /// `~/.zhive/` (per-user).
    User,
    /// `<repo>/.zhive/` (per-project, committed).
    Project,
    /// `<repo>/.zhive.local/` (per-project, gitignored).
    Local,
    /// Sourced from an MCP server's `resources/list`.
    Mcp,
}

// ============================================================
// EnginePhase
// ============================================================

/// Engine state used by [`HookEvent::PhaseTransition`].
///
/// Mirrors Pi `AgentHarnessPhase` plus a `Retry` lane that captures
/// llmsdk retry behaviour. Kept here (not in `permission.rs`) because
/// the hook wire is the only public consumer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EnginePhase {
    /// No active turn.
    Idle,
    /// Inside the agent loop.
    Turn,
    /// Compacting transcript history.
    Compaction,
    /// Running a branch summary.
    BranchSummary,
    /// Retrying after a provider failure.
    Retry,
    /// Reverting workspace files (and conversation) to an earlier checkpoint.
    Restore,
}

// ============================================================
// HookEventBase
// ============================================================

/// Fields shared by every hook payload, flattened into the JSON root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct HookEventBase {
    /// Owning thread / session id (aligns with [`crate::domain::ThreadId`]
    /// after URI prefix stripping).
    pub session_id: String,
    /// Working directory of the agent process when the hook fires.
    pub cwd: String,
    /// Manifest that registered the hook (red line 10, required).
    pub registered_by: ExtensionRef,
    /// Subagent id when the hook fires inside one; `None` at top level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Subagent type label, e.g. `"explore"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Parent tool-use id that spawned the subagent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
    /// Permission mode snapshot at fire time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    /// Path to the JSONL rollout for the current thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
}

// ============================================================
// HookEvent
// ============================================================

/// 14 + 1 hook events emitted by the engine, plus an `Unknown` fallback.
///
/// Internally tagged by `hook_event_name`. Every typed variant's payload
/// flattens [`HookEventBase`] into the JSON root. A hand-written
/// [`Deserialize`] impl downgrades any unrecognised tag (or a payload with
/// no tag at all) to [`HookEvent::Unknown`] instead of erroring, so a
/// future SDK event can never crash the dispatch loop.
///
/// # Examples
///
/// ```
/// use zhive_proto::hook::HookEvent;
/// let payload = r#"{
///     "hook_event_name": "Stop",
///     "sessionId": "thread:native/abc",
///     "cwd": "/tmp",
///     "registeredBy": {"id": "test", "version": "0.1.0", "source": "builtin"},
///     "stopHookActive": false
/// }"#;
/// let ev: HookEvent = serde_json::from_str(payload).unwrap();
/// assert!(matches!(ev, HookEvent::Stop(_)));
///
/// // An event the SDK adds in the future degrades to `Unknown`.
/// let future = r#"{ "hook_event_name": "FutureEvent", "extra": 42 }"#;
/// let ev: HookEvent = serde_json::from_str(future).unwrap();
/// assert!(matches!(ev, HookEvent::Unknown { .. }));
/// ```
#[derive(Debug, Clone, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "hook_event_name")]
#[non_exhaustive]
pub enum HookEvent {
    /// Fired before a tool call is dispatched. `PreToolUse` hooks may
    /// mutate `tool_input`; the host re-validates the schema afterwards
    /// (red line 11).
    PreToolUse(PreToolUseInput),
    /// Fired after a tool call returns successfully.
    PostToolUse(PostToolUseInput),
    /// Fired after a tool call fails or is cancelled.
    PostToolUseFailure(PostToolUseFailureInput),
    /// Fired when the user submits a new prompt.
    UserPromptSubmit(UserPromptSubmitInput),
    /// Fired when a new thread is opened.
    SessionStart(SessionStartInput),
    /// Fired when a thread closes.
    SessionEnd(SessionEndInput),
    /// Fired when a subagent thread starts.
    SubagentStart(SubagentStartInput),
    /// Fired when a subagent thread stops.
    SubagentStop(SubagentStopInput),
    /// Fired immediately before context compaction.
    PreCompact(PreCompactInput),
    /// Fired immediately after context compaction completes.
    PostCompact(PostCompactInput),
    /// Reserved: fired immediately before a branch summary runs.
    ///
    /// Type reserved for the fork/branch-summary feature; dispatch wiring
    /// is deferred. Aligns with decision-diffs §1.7.
    PreBranchSummary(PreBranchSummaryInput),
    /// Reserved: fired immediately after a branch summary completes.
    ///
    /// Type reserved for the fork/branch-summary feature; dispatch wiring
    /// is deferred. Aligns with decision-diffs §1.7.
    PostBranchSummary(PostBranchSummaryInput),
    /// Fired when a permission/request reverse RPC is about to ship.
    PermissionRequest(PermissionRequestInput),
    /// Fired when the agent decides to stop.
    Stop(StopInput),
    /// Generic notification surface (permission prompts, etc.).
    Notification(NotificationInput),
    /// First-run / maintenance hook (Claude Code TS-only equivalent).
    Setup(SetupInput),
    /// Fired when a tool's approval state changes.
    ToolApprovalChange(ToolApprovalChangeInput),
    /// Fired when the engine moves between phases (15th event).
    PhaseTransition(PhaseTransitionInput),
    /// Graceful fallback for an unrecognised `hook_event_name`.
    ///
    /// Produced **only** by [`Deserialize`]; the dispatch loop never
    /// constructs it and it is not subscribable. It preserves the
    /// original tag (`name`) and the full JSON `payload` so callers can
    /// inspect or log a future SDK event without losing data. The
    /// [`Serialize`] derive skips this variant: it has no defined wire
    /// form (an unknown event is decode-only), and attempting to
    /// serialize it returns an `Err` rather than emitting bytes.
    #[serde(skip)]
    #[cfg_attr(feature = "schema", schemars(skip))]
    Unknown {
        /// Raw `hook_event_name`, or an empty string when the tag was
        /// absent.
        name: String,
        /// The complete JSON object as received.
        payload: Value,
    },
}

/// The `hook_event_name` discriminator key, shared by serde and the
/// hand-written [`Deserialize`] impl below.
const HOOK_EVENT_NAME_KEY: &str = "hook_event_name";

impl<'de> Deserialize<'de> for HookEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Decode the whole object once, then dispatch on the tag. Routing
        // each known tag back through its typed payload preserves the
        // `#[serde(flatten)] base` behaviour, while any unrecognised tag
        // (or a missing one) falls back to `Unknown` instead of erroring.
        let payload = Value::deserialize(deserializer)?;
        let name = payload
            .get(HOOK_EVENT_NAME_KEY)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        // Routes a known tag through its typed payload struct.
        macro_rules! typed {
            ($variant:ident) => {
                serde_json::from_value(payload.clone())
                    .map(HookEvent::$variant)
                    .map_err(serde::de::Error::custom)
            };
        }

        match name.as_str() {
            "PreToolUse" => typed!(PreToolUse),
            "PostToolUse" => typed!(PostToolUse),
            "PostToolUseFailure" => typed!(PostToolUseFailure),
            "UserPromptSubmit" => typed!(UserPromptSubmit),
            "SessionStart" => typed!(SessionStart),
            "SessionEnd" => typed!(SessionEnd),
            "SubagentStart" => typed!(SubagentStart),
            "SubagentStop" => typed!(SubagentStop),
            "PreCompact" => typed!(PreCompact),
            "PostCompact" => typed!(PostCompact),
            "PreBranchSummary" => typed!(PreBranchSummary),
            "PostBranchSummary" => typed!(PostBranchSummary),
            "PermissionRequest" => typed!(PermissionRequest),
            "Stop" => typed!(Stop),
            "Notification" => typed!(Notification),
            "Setup" => typed!(Setup),
            "ToolApprovalChange" => typed!(ToolApprovalChange),
            "PhaseTransition" => typed!(PhaseTransition),
            _ => Ok(HookEvent::Unknown { name, payload }),
        }
    }
}

// ============================================================
// Event payloads (alphabetical within Claude Code groups)
// ============================================================

/// Payload of [`HookEvent::PreToolUse`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PreToolUseInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Tool that is about to run.
    pub tool_name: String,
    /// Raw arguments handed to the tool; `PreToolUse` hooks may mutate
    /// this and the host will re-validate.
    pub tool_input: Value,
    /// Stable id for this tool call.
    pub tool_use_id: String,
}

/// Payload of [`HookEvent::PostToolUse`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PostToolUseInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Tool that produced the response.
    pub tool_name: String,
    /// Arguments the tool ran with (post-mutation).
    pub tool_input: Value,
    /// Raw output returned by the tool.
    pub tool_response: Value,
    /// Stable id for this tool call.
    pub tool_use_id: String,
}

/// Coarse classification of why a tool call failed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolErrorKind {
    /// Exceeded the tool-specific deadline.
    Timeout,
    /// Cancelled by `session/cancel` or shutdown.
    Cancelled,
    /// Failed argument validation.
    InvalidInput,
    /// Refused by the permission reducer.
    PermissionDenied,
    /// Runtime error inside the tool body.
    ExecutionError,
    /// Anything else.
    Other,
}

/// Payload of [`HookEvent::PostToolUseFailure`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PostToolUseFailureInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Tool that failed.
    pub tool_name: String,
    /// Arguments the tool ran with.
    pub tool_input: Value,
    /// Stable id for this tool call.
    pub tool_use_id: String,
    /// Human-readable error message.
    pub error: String,
    /// Coarse error class.
    pub error_kind: ToolErrorKind,
}

/// Payload of [`HookEvent::UserPromptSubmit`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct UserPromptSubmitInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// User-supplied text after slash-command expansion.
    pub prompt: String,
}

/// Reason a [`HookEvent::SessionStart`] fires.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionStartSource {
    /// Cold start.
    Startup,
    /// Resumed from rollout.
    Resume,
    /// Cleared and restarted.
    Clear,
    /// Created by a compaction.
    Compact,
    /// Forked from another thread.
    Fork,
}

/// Payload of [`HookEvent::SessionStart`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SessionStartInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Reason the thread was created.
    pub source: SessionStartSource,
    /// Provider model identifier (`None` until B10 lands).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Reason a [`HookEvent::SessionEnd`] fires.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionEndReason {
    /// `/clear` slash command.
    Clear,
    /// Closed to resume a different thread.
    Resume,
    /// User logged out.
    Logout,
    /// User exited the prompt input loop.
    PromptInputExit,
    /// `BypassPermissions` mode was forcibly disabled.
    BypassPermissionsDisabled,
    /// Anything else.
    Other,
}

/// Payload of [`HookEvent::SessionEnd`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SessionEndInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Why the thread was closed.
    pub reason: SessionEndReason,
}

/// Payload of [`HookEvent::SubagentStart`].
///
/// `base.agent_id`, `base.agent_type` and `base.parent_tool_use_id` are
/// required for this event (the host fills them before dispatch).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SubagentStartInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Inherited permission scope (typed handle pending A3 wire stub).
    pub inherited_scope: Value,
}

/// Payload of [`HookEvent::SubagentStop`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SubagentStopInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Rollout path scoped to the subagent.
    pub agent_transcript_path: String,
    /// `true` while a parent Stop hook caused this stop (prevents loops).
    #[serde(default)]
    pub stop_hook_active: bool,
}

/// What triggered a [`HookEvent::PreCompact`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompactTrigger {
    /// User-initiated via `/compact`.
    Manual,
    /// Engine-initiated when the context window fills.
    Auto,
}

/// Payload of [`HookEvent::PreCompact`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PreCompactInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Why compaction fires.
    pub trigger: CompactTrigger,
    /// Number of items about to be compacted.
    pub entries_count: u32,
    /// Optional user-supplied summarisation guidance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<String>,
}

/// Payload of [`HookEvent::PostCompact`].
///
/// Dual of [`PreCompactInput`], fired immediately after the engine finishes
/// replacing the transcript history with a summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PostCompactInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Why compaction fired (mirrors the originating `PreCompact`).
    pub trigger: CompactTrigger,
    /// Number of items that were compacted away.
    pub entries_compacted: u32,
}

/// Payload of [`HookEvent::PreBranchSummary`] (reserved).
///
/// Reserved for the fork/branch-summary feature. Mirrors the shape of
/// [`PreCompactInput`]: it carries the source thread a branch was forked
/// from plus how many transcript entries the summary will cover. Dispatch
/// wiring is deferred; only the type and its serde shape exist today.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PreBranchSummaryInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Thread the branch was forked from.
    pub source_thread_id: String,
    /// Number of transcript entries the branch summary will cover.
    pub entries_count: u32,
}

/// Payload of [`HookEvent::PostBranchSummary`] (reserved).
///
/// Dual of [`PreBranchSummaryInput`]. Reserved for the fork/branch-summary
/// feature; dispatch wiring is deferred.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PostBranchSummaryInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Thread the branch was forked from.
    pub source_thread_id: String,
    /// Number of transcript entries the branch summary covered.
    pub entries_summarized: u32,
}

/// Payload of [`HookEvent::PermissionRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PermissionRequestInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Tool the user is being asked about.
    pub tool_name: String,
    /// Arguments the tool would run with.
    pub tool_input: Value,
    /// Stable id for the pending tool call.
    pub tool_use_id: String,
    /// Requested scope (typed handle pending A3 wire stub).
    pub requested_scope: Value,
}

/// Payload of [`HookEvent::Stop`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct StopInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// `true` when the engine is already inside a Stop hook chain.
    #[serde(default)]
    pub stop_hook_active: bool,
}

/// Category of a generic [`HookEvent::Notification`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NotificationCategory {
    /// Permission prompt surfaced to the user.
    PermissionPrompt,
    /// Idle reminder.
    IdlePrompt,
    /// Authentication completed.
    AuthSuccess,
    /// Elicitation: dialog opened.
    ElicitationDialog,
    /// Elicitation: user responded.
    ElicitationResponse,
    /// Elicitation: flow complete.
    ElicitationComplete,
}

/// Payload of [`HookEvent::Notification`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct NotificationInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Notification class.
    pub category: NotificationCategory,
    /// Body text.
    pub message: String,
    /// Optional title for UIs that show it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Reason a [`HookEvent::Setup`] fires.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SetupTrigger {
    /// First-time bootstrap.
    Init,
    /// Periodic maintenance.
    Maintenance,
}

/// Payload of [`HookEvent::Setup`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SetupInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Why setup fires.
    pub trigger: SetupTrigger,
}

/// Persistent approval state of a tool.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolApprovalState {
    /// Persistent allow.
    Allow,
    /// One-shot allow.
    AllowOnce,
    /// Ask before each call.
    Ask,
    /// Persistent deny.
    Deny,
}

/// Source of the change in [`HookEvent::ToolApprovalChange`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolApprovalOrigin {
    /// User toggled the permission UI.
    UserDecision,
    /// A hook callback updated the approval state.
    HookDecision,
    /// Permission scope changed (e.g. subagent narrowing).
    ScopeChange,
}

/// Payload of [`HookEvent::ToolApprovalChange`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ToolApprovalChangeInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Tool whose approval changed.
    pub tool_name: String,
    /// Previous state.
    pub previous: ToolApprovalState,
    /// New state.
    pub current: ToolApprovalState,
    /// What caused the change.
    pub origin: ToolApprovalOrigin,
}

/// Payload of [`HookEvent::PhaseTransition`] (the proposed 15th event).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PhaseTransitionInput {
    /// Flattened generic fields.
    #[serde(flatten)]
    pub base: HookEventBase,
    /// Phase the engine left.
    pub from: EnginePhase,
    /// Phase the engine entered.
    pub to: EnginePhase,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> HookEventBase {
        HookEventBase {
            session_id: "thread:native/abc".into(),
            cwd: "/tmp".into(),
            registered_by: ExtensionRef {
                id: "test".into(),
                version: "0.1.0".into(),
                source: ExtensionSource::Builtin,
            },
            agent_id: None,
            agent_type: None,
            parent_tool_use_id: None,
            permission_mode: None,
            transcript_path: None,
        }
    }

    #[test]
    fn stop_event_tag_and_flatten() {
        let ev = HookEvent::Stop(StopInput {
            base: base(),
            stop_hook_active: false,
        });
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["hook_event_name"], "Stop");
        assert_eq!(v["sessionId"], "thread:native/abc");
        assert_eq!(v["registeredBy"]["id"], "test");
    }

    #[test]
    fn pre_tool_use_round_trip() {
        let ev = HookEvent::PreToolUse(PreToolUseInput {
            base: base(),
            tool_name: "read_file".into(),
            tool_input: serde_json::json!({"path": "/tmp/x"}),
            tool_use_id: "tool-1".into(),
        });
        let s = serde_json::to_string(&ev).unwrap();
        let back: HookEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn phase_transition_serialises() {
        let ev = HookEvent::PhaseTransition(PhaseTransitionInput {
            base: base(),
            from: EnginePhase::Idle,
            to: EnginePhase::Turn,
        });
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["hook_event_name"], "PhaseTransition");
        assert_eq!(v["from"], "idle");
        assert_eq!(v["to"], "turn");
    }

    #[test]
    fn extension_ref_display() {
        let r = ExtensionRef {
            id: "foo".into(),
            version: "1.2.3".into(),
            source: ExtensionSource::User,
        };
        assert_eq!(r.to_string(), "foo@1.2.3");
    }

    #[test]
    fn session_end_reason_snake_case() {
        let ev = HookEvent::SessionEnd(SessionEndInput {
            base: base(),
            reason: SessionEndReason::PromptInputExit,
        });
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["reason"], "prompt_input_exit");
    }

    #[test]
    fn unknown_tag_falls_back_and_preserves_payload() {
        let payload = r#"{
            "hook_event_name": "FutureEvent",
            "sessionId": "thread:native/abc",
            "extra": 42
        }"#;
        let ev: HookEvent = serde_json::from_str(payload).unwrap();
        match ev {
            HookEvent::Unknown { name, payload } => {
                assert_eq!(name, "FutureEvent");
                // Full payload is preserved, including the unknown field.
                assert_eq!(payload["extra"], 42);
                assert_eq!(payload["hook_event_name"], "FutureEvent");
                assert_eq!(payload["sessionId"], "thread:native/abc");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn missing_tag_falls_back_to_unknown() {
        // A payload with no `hook_event_name` must not panic or error.
        let payload = r#"{ "sessionId": "thread:native/abc" }"#;
        let ev: HookEvent = serde_json::from_str(payload).unwrap();
        match ev {
            HookEvent::Unknown { name, payload } => {
                assert_eq!(name, "", "absent tag yields empty name");
                assert_eq!(payload["sessionId"], "thread:native/abc");
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn known_tag_still_typed_not_unknown() {
        // Regression guard: known events keep decoding to their typed
        // variant rather than degrading to Unknown.
        let ev = HookEvent::PreToolUse(PreToolUseInput {
            base: base(),
            tool_name: "read_file".into(),
            tool_input: serde_json::json!({"path": "/tmp/x"}),
            tool_use_id: "tool-1".into(),
        });
        let s = serde_json::to_string(&ev).unwrap();
        let back: HookEvent = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, HookEvent::PreToolUse(_)));
        assert_eq!(ev, back);
    }

    #[test]
    fn unknown_variant_is_not_serialized() {
        // `Unknown` has no defined wire form: the `#[serde(skip)]` derive
        // refuses to serialize it, returning an `Err` instead of panicking
        // or emitting bytes. This enforces the decode-only contract.
        let ev = HookEvent::Unknown {
            name: "FutureEvent".into(),
            payload: serde_json::json!({"hook_event_name": "FutureEvent"}),
        };
        assert!(serde_json::to_value(&ev).is_err());
    }

    #[test]
    fn pre_branch_summary_round_trip() {
        let ev = HookEvent::PreBranchSummary(PreBranchSummaryInput {
            base: base(),
            source_thread_id: "thread:native/src".into(),
            entries_count: 7,
        });
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["hook_event_name"], "PreBranchSummary");
        assert_eq!(v["sourceThreadId"], "thread:native/src");
        assert_eq!(v["entriesCount"], 7);
        assert_eq!(v["sessionId"], "thread:native/abc");
        let back: HookEvent = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn post_branch_summary_round_trip() {
        let ev = HookEvent::PostBranchSummary(PostBranchSummaryInput {
            base: base(),
            source_thread_id: "thread:native/src".into(),
            entries_summarized: 7,
        });
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["hook_event_name"], "PostBranchSummary");
        assert_eq!(v["entriesSummarized"], 7);
        let back: HookEvent = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
        assert_eq!(ev, back);
    }
}

// Rust guideline compliant 2026-02-21

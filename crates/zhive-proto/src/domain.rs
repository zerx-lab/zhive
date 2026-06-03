//! Three-tier primitives (`Thread` / `Turn` / `Item`) shared across all transports.
//!
//! This module is the single source of truth for the content primitives that
//! cross every zhive process boundary. The same `serde`-derived types act as
//! both wire schema (JSON-RPC over stdio / UDS / future remote) and in-memory
//! representation in `zhive-core::state`, satisfying D-006 "single schema
//! source" verbatim.
//!
//! # Layout
//!
//! * IDs — [`ThreadId`], [`TurnId`], [`ItemId`], [`AcpSessionId`],
//!   [`Provenance`]. URI-style namespaces let `grep` distinguish bridge origin
//!   at a glance.
//! * [`Thread`] — top-level conversation handle plus metadata.
//! * [`Turn`] — one user-input-to-final-answer cycle within a thread. Synthesised
//!   by the bridge for MCP `tools/call` sequences (see [`TurnStartedNotification`]).
//! * [`Item`] — leaf content primitive, 14 variants covering reasoning,
//!   tool calls, file edits, plans, etc.
//! * Leaves ([`ItemContent`], [`ItemToolCallContent`], [`ToolKind`], ...) —
//!   1:1 isomorphic with ACP `ContentBlock` / `ToolCallContent` for cheap bridge
//!   mapping.
//! * [`ThreadBridgeBinding`] — many-to-one binding from ACP `SessionId`
//!   (or synthesised MCP session) to a zhive [`ThreadId`].
//!
//! # Discriminator choice
//!
//! [`Item`] uses `#[serde(tag = "kind")]` to avoid clashing with codex
//! (`tag = "type"`) and ACP (`tag = "sessionUpdate"`). Leaf enums
//! ([`ItemContent`], [`ItemToolCallContent`]) follow ACP's `tag = "type"` for
//! 1:1 wire compatibility.
//!
//! Every public enum is `#[non_exhaustive]`: callers must always handle an
//! unknown variant downgrade path. The bridge crate is expected to map unknown
//! ACP / MCP variants onto [`Item::SystemNotice`] with [`NoticeLevel::Warn`]
//! rather than panic.
//!
//! # References
//!
//! Field naming and case lists derive from a 2026-05-28 cross-reference of:
//! codex `app-server-protocol/src/protocol/v2/{thread_data,turn,item}.rs`,
//! `agent-client-protocol-schema 0.12.0`, and rmcp 1.7. See
//! `plans/phase1-core-native-research/deliverables/A1-thread-turn-item.md`.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(feature = "schema")]
use schemars::JsonSchema;

// ============================================================
// IDs
// ============================================================

/// Stable thread handle of the form `thread:<provenance>/<uuid-v7>`.
///
/// The provenance prefix (one of `native` / `acp` / `mcp`) lets logs and
/// rollouts be filtered by bridge of origin. UUID v7 sorts by creation time,
/// which makes `SQLite` indices and JSONL append order coherent.
///
/// Note: the URI prefix `thread:` is a literal scheme, not a Rust path.
///
/// # Examples
///
/// ```
/// use zhive_proto::domain::ThreadId;
/// let id = ThreadId(std::sync::Arc::from("thread:native/01900000-0000-7000-8000-000000000000"));
/// assert!(id.0.starts_with("thread:native/"));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(transparent)]
pub struct ThreadId(pub Arc<str>);

/// Turn handle scoped to a thread, of the form `turn:<thread_id>/<seq>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(transparent)]
pub struct TurnId(pub Arc<str>);

/// Item handle scoped to a turn, of the form `item:<turn_id>/<seq>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(transparent)]
pub struct ItemId(pub Arc<str>);

/// Opaque ACP `SessionId` wire wrapper, retained only by the bridge crate.
///
/// zhive-core does not key its state by this id; instead it joins through
/// [`ThreadBridgeBinding`] in the bridge crate. Mirrors ACP
/// `agent-client-protocol-schema 0.12.0/src/lib.rs:99-110`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(transparent)]
pub struct AcpSessionId(pub Arc<str>);

/// Where a thread originated from, encoded in the [`ThreadId`] prefix.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Provenance {
    /// Native zhive client (CLI / TUI / future Web UI).
    Native,
    /// ACP bridge session.
    Acp,
    /// MCP bridge synthesised session.
    Mcp,
}

// ============================================================
// Thread
// ============================================================

/// Top-level conversation handle plus metadata.
///
/// `turns` is populated lazily; most read paths return a Thread with `turns`
/// empty and a separate request fetches the items.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    /// Stable URI-style id, see [`ThreadId`].
    pub id: ThreadId,
    /// External session handle when the thread was opened via a bridge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<AcpSessionId>,
    /// Source thread id when this one was forked off another.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<ThreadId>,
    /// Parent thread id when this one was spawned as a subagent.
    ///
    /// Distinct from `forked_from`: a fork branches an existing conversation,
    /// whereas a subagent is a child task spawned by a running turn. Recorded
    /// so resume/rebuild can recover the parent-child relationship.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_parent: Option<ThreadId>,
    /// First user message excerpt, truncated to ~200 chars for list views.
    pub preview: String,
    /// When `true`, the thread is in-memory only and never written to JSONL.
    pub ephemeral: bool,
    /// LLM provider identifier (free-form, set at thread creation).
    pub model_provider: String,
    /// Unix timestamp in seconds.
    pub created_at: i64,
    /// Unix timestamp in seconds.
    pub updated_at: i64,
    /// Current lifecycle status.
    pub status: ThreadStatus,
    /// Working directory the thread was created in.
    pub cwd: PathBuf,
    /// Whether the thread was started by a user, a subagent, or memory consolidation.
    pub source: ThreadSource,
    /// User-supplied label, distinct from `preview`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Lazily populated turn list (empty for list endpoints).
    #[serde(default)]
    pub turns: Vec<Turn>,
}

/// Lifecycle state of a [`Thread`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "type", rename_all = "camelCase")]
#[non_exhaustive]
pub enum ThreadStatus {
    /// Indexed but not yet read into memory.
    NotLoaded,
    /// Resident in memory, no active turn.
    Idle,
    /// At least one in-progress turn or awaiting input.
    Active {
        /// Flags describing what the thread is currently waiting on.
        active_flags: Vec<ThreadActiveFlag>,
    },
    /// Engine-level fatal error; the thread cannot accept further turns.
    SystemError,
}

/// Reason the thread is in [`ThreadStatus::Active`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ThreadActiveFlag {
    /// Blocked on a `permission/request` round-trip.
    WaitingOnApproval,
    /// Blocked on user input (e.g. `defer` outcome).
    WaitingOnUserInput,
    /// A turn is executing.
    TurnInProgress,
    /// A subagent spawned by this thread is executing.
    SubagentInProgress,
}

/// Origin of the thread; mirrors codex `ThreadSource`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ThreadSource {
    /// Started directly by a user prompt.
    User,
    /// Spawned by another thread as a subagent.
    Subagent,
    /// Created by an internal memory consolidation job.
    MemoryConsolidation,
}

// ============================================================
// Turn
// ============================================================

/// One user-input-to-final-answer cycle within a [`Thread`].
///
/// For MCP, the bridge synthesises a turn boundary around each `tools/call`
/// sequence by listening to the [`TurnStartedNotification`] /
/// [`TurnCompletedNotification`] notifications emitted by the engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    /// Stable turn id, see [`TurnId`].
    pub id: TurnId,
    /// Ordered, append-only item list.
    #[serde(default)]
    pub items: Vec<Item>,
    /// Whether `items` is fully populated, summarised, or unloaded.
    #[serde(default)]
    pub items_view: TurnItemsView,
    /// Lifecycle status of the turn.
    pub status: TurnStatus,
    /// Failure details; populated only when `status == Failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<TurnError>,
    /// Unix timestamp in seconds when the turn started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// Unix timestamp in seconds when the turn ended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
    /// `completed_at - started_at`, expressed in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
}

/// Whether [`Turn::items`] has been loaded from the JSONL rollout.
#[derive(Default, Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum TurnItemsView {
    /// Items not yet read from persistence; the field is intentionally empty.
    NotLoaded,
    /// Items collapsed to a single summary entry (history compaction).
    Summary,
    /// Fully loaded items.
    #[default]
    Full,
}

/// Lifecycle state of a [`Turn`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum TurnStatus {
    /// Active; never appears in `turn/completed` notifications.
    InProgress,
    /// Reached a final agent message without errors.
    Completed,
    /// Cancelled by `session/cancel` or user abort.
    Interrupted,
    /// Engine or provider raised a fatal error.
    Failed,
}

/// Failure details attached to a [`Turn`] with `status == Failed`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, thiserror::Error)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[error("{message}")]
pub struct TurnError {
    /// Short human-readable summary of the failure.
    pub message: String,
    /// Free-form diagnostic payload (provider response, exit code, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_details: Option<String>,
}

// ============================================================
// Item (14 cases; discriminator = "kind"; snake_case)
// ============================================================

/// Leaf content primitive of a [`Turn`]; 14 variants covering all observable events.
///
/// `#[non_exhaustive]` lets the schema evolve without breaking downstream
/// match arms. Bridge crates must downgrade unknown wire variants to
/// [`Item::SystemNotice`] with [`NoticeLevel::Warn`] rather than panic.
///
/// The discriminator is `itemKind` (not `kind`) so that the
/// [`Item::ToolCall::kind`] inner field can keep its ACP-aligned name; serde
/// disallows a tag and a variant field sharing one name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "itemKind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Item {
    /// Direct user input.
    UserMessage {
        /// Per-turn item id.
        id: ItemId,
        /// Multi-part content (text / image / audio / resource).
        content: Vec<ItemContent>,
    },
    /// Final or interim assistant message.
    AgentMessage {
        /// Per-turn item id.
        id: ItemId,
        /// Plain-text body.
        text: String,
    },
    /// Streaming thought chunk surfaced by the model.
    AgentThought {
        /// Per-turn item id.
        id: ItemId,
        /// Plain-text body.
        text: String,
    },
    /// Structured reasoning trace (codex-only; ACP bridges write `AgentThought`).
    Reasoning {
        /// Per-turn item id.
        id: ItemId,
        /// Ordered summary fragments.
        #[serde(default)]
        summary: Vec<String>,
    },
    /// Tool call dispatched by the agent (provider or extension tools).
    ToolCall {
        /// Per-turn item id.
        id: ItemId,
        /// Tool name (e.g. `read_file`, `bash`).
        name: String,
        /// Coarse classification, useful for UI grouping.
        #[serde(default)]
        kind: ToolKind,
        /// Tool call lifecycle status.
        #[serde(default)]
        status: ToolCallStatus,
        /// Tool call output / progress content.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        content: Vec<ItemToolCallContent>,
        /// File / location hints surfaced to UI.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        locations: Vec<ToolCallLocation>,
        /// Raw JSON arguments handed to the tool.
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_input: Option<Value>,
        /// Raw JSON output returned by the tool.
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_output: Option<Value>,
        /// Provider-assigned tool call id (e.g. `toolu_01...` from Anthropic).
        ///
        /// Preserved from the [`StreamPart::ToolCall`] or `ToolInputStart`
        /// frame so the engine can round-trip it back in `Message::Tool`
        /// `tool_call_id` without minting a synthetic replacement.
        ///
        /// `None` for items that pre-date this field or originate outside
        /// the provider stream (e.g. synthetic items in tests).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_tool_call_id: Option<String>,
    },
    /// Shell command execution.
    CommandExecution {
        /// Per-turn item id.
        id: ItemId,
        /// Command line as provided to the shell.
        command: String,
        /// Working directory the command ran in.
        cwd: PathBuf,
        /// Command lifecycle status.
        status: CommandExecutionStatus,
        /// Process exit code, populated when complete.
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// Merged stdout + stderr capture.
        #[serde(skip_serializing_if = "Option::is_none")]
        aggregated_output: Option<String>,
        /// Wall-clock duration in milliseconds.
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<i64>,
    },
    /// File edit applied as a multi-file patch.
    FileEdit {
        /// Per-turn item id.
        id: ItemId,
        /// One entry per file touched.
        changes: Vec<FileUpdateChange>,
        /// Overall patch application status.
        status: PatchApplyStatus,
    },
    /// Standalone diff entry (used when a diff is surfaced outside a `ToolCall`).
    Diff {
        /// Per-turn item id.
        id: ItemId,
        /// Affected file.
        path: PathBuf,
        /// Pre-edit text; absent for created files.
        #[serde(skip_serializing_if = "Option::is_none")]
        old_text: Option<String>,
        /// Post-edit text.
        new_text: String,
    },
    /// Embedded terminal handle referenced by the UI.
    Terminal {
        /// Per-turn item id.
        id: ItemId,
        /// Opaque terminal handle issued by the engine.
        terminal_id: Arc<str>,
    },
    /// Agent-authored plan step list.
    Plan {
        /// Per-turn item id.
        id: ItemId,
        /// Ordered steps.
        steps: Vec<PlanStep>,
    },
    /// Refreshed list of slash commands or callable tools.
    AvailableCommands {
        /// Per-turn item id.
        id: ItemId,
        /// Currently advertised commands.
        commands: Vec<AvailableCommand>,
    },
    /// Mode switch (e.g. plan mode toggle).
    ModeChange {
        /// Per-turn item id.
        id: ItemId,
        /// New mode identifier.
        mode_id: Arc<str>,
    },
    /// Context compaction marker; corresponds to `PreCompact` / `PostCompact` hooks.
    ContextCompaction {
        /// Per-turn item id.
        id: ItemId,
    },
    /// Engine-emitted notice (info, warning, error) surfaced into the transcript.
    SystemNotice {
        /// Per-turn item id.
        id: ItemId,
        /// Severity classification.
        level: NoticeLevel,
        /// Human-readable message.
        message: String,
    },
}

impl Item {
    /// Returns the [`ItemId`] of any variant.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::domain::{Item, ItemId};
    /// let id = ItemId(std::sync::Arc::from("item:turn:foo/0/0"));
    /// let item = Item::AgentMessage { id: id.clone(), text: "hi".into() };
    /// assert_eq!(item.id(), &id);
    /// ```
    #[must_use]
    pub fn id(&self) -> &ItemId {
        match self {
            Self::UserMessage { id, .. }
            | Self::AgentMessage { id, .. }
            | Self::AgentThought { id, .. }
            | Self::Reasoning { id, .. }
            | Self::ToolCall { id, .. }
            | Self::CommandExecution { id, .. }
            | Self::FileEdit { id, .. }
            | Self::Diff { id, .. }
            | Self::Terminal { id, .. }
            | Self::Plan { id, .. }
            | Self::AvailableCommands { id, .. }
            | Self::ModeChange { id, .. }
            | Self::ContextCompaction { id }
            | Self::SystemNotice { id, .. } => id,
        }
    }
}

// ============================================================
// Leaf types (1:1 with ACP / MCP)
// ============================================================

/// Multi-part content block, 1:1 isomorphic with ACP `ContentBlock`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ItemContent {
    /// Plain text.
    Text {
        /// Text body.
        text: String,
        /// Optional ACP-defined annotations payload.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
    },
    /// Base64-encoded image.
    Image {
        /// Base64 data.
        data: String,
        /// MIME type (e.g. `image/png`).
        mime_type: String,
        /// Optional source URI.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
    /// Base64-encoded audio.
    Audio {
        /// Base64 data.
        data: String,
        /// MIME type (e.g. `audio/wav`).
        mime_type: String,
    },
    /// Reference to an external resource by URI.
    ResourceLink {
        /// Resource URI.
        uri: String,
        /// Optional short label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Optional long description.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Optional MIME type hint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    /// Embedded resource payload.
    Resource {
        /// `ResourceContents` payload; strong typing deferred to a follow-up.
        // TODO: strong-type as `ResourceContents` once Phase 1 lands.
        resource: Value,
    },
}

/// Tool-call output content, 1:1 isomorphic with ACP `ToolCallContent`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ItemToolCallContent {
    /// Generic content block (text / image / etc.).
    Content {
        /// Inner content block.
        content: ItemContent,
    },
    /// File diff produced by the tool call.
    Diff {
        /// Affected path.
        path: PathBuf,
        /// Pre-edit text; absent for created files.
        #[serde(skip_serializing_if = "Option::is_none")]
        old_text: Option<String>,
        /// Post-edit text.
        new_text: String,
    },
    /// Reference to an embedded terminal.
    Terminal {
        /// Engine-issued terminal handle.
        terminal_id: Arc<str>,
    },
}

/// Coarse tool classification (mirrors ACP `ToolKind`).
#[derive(Default, Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolKind {
    /// Read-only file or resource access.
    Read,
    /// Modify a file or resource in place.
    Edit,
    /// Delete a file or resource.
    Delete,
    /// Move or rename a file or resource.
    Move,
    /// Search a corpus.
    Search,
    /// Execute a shell command or script.
    Execute,
    /// Internal reasoning (no side effects).
    Think,
    /// Fetch a remote resource.
    Fetch,
    /// Switch the agent mode.
    SwitchMode,
    /// Anything else.
    #[default]
    Other,
}

/// Lifecycle of a tool call (mirrors ACP `ToolCallStatus`).
#[derive(Default, Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolCallStatus {
    /// Queued, not yet dispatched.
    #[default]
    Pending,
    /// Dispatched and running.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Errored out.
    Failed,
}

/// File / line hint surfaced to UI alongside a tool call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ToolCallLocation {
    /// File path.
    pub path: PathBuf,
    /// Optional line number (1-based).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// Lifecycle of a [`Item::CommandExecution`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CommandExecutionStatus {
    /// Spawned, still running.
    InProgress,
    /// Exited with status 0.
    Completed,
    /// Exited non-zero or killed.
    Failed,
}

/// Patch apply outcome for [`Item::FileEdit`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PatchApplyStatus {
    /// Patch built but not yet applied.
    Pending,
    /// All hunks applied cleanly.
    Applied,
    /// At least one hunk rejected.
    Failed,
}

/// One file's change within a [`Item::FileEdit`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct FileUpdateChange {
    /// Affected path.
    pub path: PathBuf,
    /// Kind of change applied.
    pub kind: PatchChangeKind,
    /// Pre-edit text; absent for created files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    /// Post-edit text; absent for deleted files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
}

/// Kind of [`FileUpdateChange`] applied to a file.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PatchChangeKind {
    /// File created.
    Create,
    /// Existing file modified.
    Update,
    /// File deleted.
    Delete,
    /// File moved or renamed.
    Rename,
}

/// One step within an agent-authored plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    /// Step description.
    pub step: String,
    /// Step lifecycle status.
    pub status: PlanStepStatus,
}

/// Lifecycle of a [`PlanStep`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum PlanStepStatus {
    /// Not yet started.
    Pending,
    /// In progress.
    InProgress,
    /// Done.
    Completed,
}

/// Advertised slash command or callable tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommand {
    /// Command name (e.g. `commit`, `pr`).
    pub name: String,
    /// Short human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Severity of an [`Item::SystemNotice`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NoticeLevel {
    /// Informational.
    Info,
    /// Warning; the engine kept running.
    Warn,
    /// Recoverable error surfaced to the transcript.
    Error,
}

// ============================================================
// Turn lifecycle notification payloads
// ============================================================

/// Bridge-synthesised `turn/started` payload carrying the full [`Turn`] snapshot.
///
/// Bridge crates subscribe to `events/turn_started` to synthesise Turn
/// boundaries for transports (ACP / MCP) whose native protocol has no Turn
/// primitive. This type is produced **by the bridge**, not by the engine.
///
/// > **Disambiguation**: the engine itself emits
/// > [`crate::events::TurnStartedPayload`] (`{threadId, turnId}`) on the
/// > `events/turn_started` wire method. Bridges reframe that leaner payload as
/// > a `TurnStartedNotification` with the full `Turn` embedded. The two types
/// > coexist intentionally.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TurnStartedNotification {
    /// Thread the turn belongs to.
    pub thread_id: ThreadId,
    /// Initial turn snapshot with `status = InProgress`.
    pub turn: Turn,
}

/// Bridge-synthesised `turn/completed` payload carrying the final [`Turn`] snapshot.
///
/// The final turn status is one of `Completed`, `Interrupted`, or `Failed`.
/// This type is produced **by the bridge**, not by the engine directly.
///
/// > **Disambiguation**: the engine itself emits
/// > [`crate::events::TurnCompletedPayload`] (`{threadId, turnId}`) on the
/// > `events/turn_completed` wire method. Bridges reframe that leaner payload as
/// > a `TurnCompletedNotification` with the full `Turn` embedded. The two types
/// > coexist intentionally.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedNotification {
    /// Thread the turn belongs to.
    pub thread_id: ThreadId,
    /// Final turn snapshot; `status` is one of `Completed` / `Interrupted` / `Failed`.
    pub turn: Turn,
}

// ============================================================
// Bridge binding table
// ============================================================

/// Many-to-one binding from an ACP / synthesised MCP session to a [`Thread`].
///
/// Multiple sessions can point at the same thread (e.g. ACP `session/load`
/// reusing an existing thread, or two MCP `tools/call` sequences feeding into
/// one persistent thread). The bridge crate is the sole writer of this table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ThreadBridgeBinding {
    /// Bound zhive thread id.
    pub thread_id: ThreadId,
    /// External session handle (ACP `SessionId` or synthesised MCP id).
    pub bridge_session_id: AcpSessionId,
    /// Which bridge issued this binding.
    pub bridge_kind: BridgeKind,
    /// Unix timestamp in seconds the binding was created.
    pub created_at: i64,
}

/// Which bridge a [`ThreadBridgeBinding`] originated from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BridgeKind {
    /// Agent Client Protocol session.
    Acp,
    /// Synthesised by the MCP bridge around a `tools/call` sequence.
    McpSynthesized,
}

// Rust guideline compliant 2026-02-21

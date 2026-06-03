//! Strong-typed Params and Result for every JSON-RPC method (A1 contract freeze).
//!
//! Previously these types were private structs inside `zhive-core`, making
//! them unreachable from client SDKs. This module promotes them to `pub`,
//! adds [`schemars::JsonSchema`] derives (behind the `schema` feature), and
//! preserves every `serde` attribute byte-for-byte so the JSON wire shape is
//! unchanged.
//!
//! # Sections
//!
//! * [`engine/*`](#engine-rpcs) — turn lifecycle and compaction.
//! * [`thread/*`](#thread-rpcs) — fork, list, item fetch.
//! * [`engine/resume_thread`](#resume-thread) — session restore.
//! * [`session/*`](#session-rpcs) — injection queues and cancel.
//!
//! # Note on `#[non_exhaustive]`
//!
//! All structs and enums are `#[non_exhaustive]`. Params types are only
//! deserialized by the server, so `non_exhaustive` imposes no burden on
//! callers. Result types are constructed by the server and deserialized by
//! clients; each exposes a `pub fn new(...)` constructor so cross-crate
//! construction remains possible.
//!
//! # Wire stability contract
//!
//! The `serde` attributes on every type in this module are copied verbatim
//! from the original private definitions in `zhive-core::server::handlers`.
//! Any change to field names or `rename_all` / `skip_serializing_if`
//! attributes here **is** a breaking wire change and requires a version bump.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(feature = "schema")]
use schemars::JsonSchema;

use crate::domain::{Item, ItemId, Thread, ThreadId, ToolKind, TurnId};
use crate::hook::CompactTrigger;
use crate::permission::PermissionScope;

// ============================================================
// Helpers
// ============================================================

/// Default value function for [`CompactParams::trigger`].
///
/// Client-initiated compaction is always manual; the auto trigger is
/// reserved for engine threshold-driven compaction.
fn manual_trigger() -> CompactTrigger {
    CompactTrigger::Manual
}

// ============================================================
// engine/* RPCs
// ============================================================

// --- engine/start_turn ---

/// Params of the `engine/start_turn` RPC.
///
/// Starts a new turn on `thread_id`, optionally seeding it with
/// `user_input` items and narrowing the permission `scope`.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::StartTurnParams;
/// let json = r#"{"threadId":"thread:native/x","userInput":[]}"#;
/// let p: StartTurnParams = serde_json::from_str(json).unwrap();
/// assert_eq!(p.thread_id.0.as_ref(), "thread:native/x");
/// assert!(p.user_input.is_empty());
/// assert!(p.scope.is_none());
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct StartTurnParams {
    /// Thread to start the turn on.
    pub thread_id: ThreadId,
    /// Initial user items; empty when omitted.
    #[serde(default)]
    pub user_input: Vec<Item>,
    /// Optional permission scope override; `None` inherits the parent scope.
    #[serde(default)]
    pub scope: Option<PermissionScope>,
}

impl StartTurnParams {
    /// Constructs params for starting a turn with the given inputs.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::StartTurnParams;
    /// let p = StartTurnParams::new(ThreadId(Arc::from("thread:native/x")), vec![], None);
    /// assert!(p.user_input.is_empty());
    /// assert!(p.scope.is_none());
    /// ```
    #[must_use]
    pub fn new(
        thread_id: ThreadId,
        user_input: Vec<crate::domain::Item>,
        scope: Option<crate::permission::PermissionScope>,
    ) -> Self {
        Self {
            thread_id,
            user_input,
            scope,
        }
    }
}

/// Result of the `engine/start_turn` RPC.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::TurnId;
/// use zhive_proto::rpc::StartTurnResult;
/// let r = StartTurnResult::new(TurnId(Arc::from("turn:t/0")));
/// let v = serde_json::to_value(&r).unwrap();
/// assert_eq!(v["turnId"], "turn:t/0");
/// let back: StartTurnResult = serde_json::from_value(v).unwrap();
/// assert_eq!(back, r);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct StartTurnResult {
    /// Newly created turn id.
    pub turn_id: TurnId,
}

impl StartTurnResult {
    /// Constructs a result carrying the given `turn_id`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::TurnId;
    /// use zhive_proto::rpc::StartTurnResult;
    /// let r = StartTurnResult::new(TurnId(Arc::from("turn:t/0")));
    /// assert_eq!(r.turn_id.0.as_ref(), "turn:t/0");
    /// ```
    #[must_use]
    pub fn new(turn_id: TurnId) -> Self {
        Self { turn_id }
    }
}

// --- engine/cancel_turn ---

/// Params of the `engine/cancel_turn` RPC.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::CancelTurnParams;
/// let p: CancelTurnParams =
///     serde_json::from_str(r#"{"threadId":"thread:native/x"}"#).unwrap();
/// assert_eq!(p.thread_id.0.as_ref(), "thread:native/x");
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CancelTurnParams {
    /// Thread whose active turn should be cancelled.
    pub thread_id: ThreadId,
}

/// Result of the `engine/cancel_turn` RPC.
///
/// `turn_id` is `Some` when a turn was active and cancelled; `None` when
/// no active turn was found. **Note**: the field is always present on the
/// wire (`"turnId": null` when `None`); do not add `skip_serializing_if`
/// here — existing clients assert that the field exists even when null.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::CancelTurnResult;
/// let r = CancelTurnResult::new(None);
/// let v = serde_json::to_value(&r).unwrap();
/// assert!(v["turnId"].is_null());
/// let back: CancelTurnResult = serde_json::from_value(v).unwrap();
/// assert_eq!(back.turn_id, None);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CancelTurnResult {
    /// Id of the cancelled turn, or `null` when no turn was active.
    pub turn_id: Option<TurnId>,
}

impl CancelTurnResult {
    /// Constructs a result from an optional cancelled turn id.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::rpc::CancelTurnResult;
    /// let r = CancelTurnResult::new(None);
    /// assert!(r.turn_id.is_none());
    /// ```
    #[must_use]
    pub fn new(turn_id: Option<TurnId>) -> Self {
        Self { turn_id }
    }
}

// --- engine/resume_permission ---

/// Wire classifier for the `session/resume_permission` reply.
///
/// Mirrors `ResumePermissionReply` in `zhive-core` one-to-one so the
/// server's exhaustive match keeps all variants in sync.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::ResumePermissionStatus;
/// let v = serde_json::to_value(ResumePermissionStatus::UnknownRequest).unwrap();
/// assert_eq!(v, "unknown_request");
/// let back: ResumePermissionStatus = serde_json::from_value(v).unwrap();
/// assert_eq!(back, ResumePermissionStatus::UnknownRequest);
/// ```
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ResumePermissionStatus {
    /// The deferred request was resolved successfully.
    Resolved,
    /// No pending request matched the given id.
    UnknownRequest,
    /// The request id was syntactically invalid.
    InvalidRequestId,
    /// The turn was abandoned before the request was resolved.
    Abandoned,
}

/// Result of the `session/resume_permission` (and legacy `engine/resume_permission`) RPC.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::{ResumePermissionResult, ResumePermissionStatus};
/// let r = ResumePermissionResult::new(ResumePermissionStatus::Resolved);
/// let v = serde_json::to_value(&r).unwrap();
/// assert_eq!(v["status"], "resolved");
/// let back: ResumePermissionResult = serde_json::from_value(v).unwrap();
/// assert_eq!(back, r);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ResumePermissionResult {
    /// Outcome of the deferred-permission resolution attempt.
    pub status: ResumePermissionStatus,
}

impl ResumePermissionResult {
    /// Constructs a result with the given status.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::rpc::{ResumePermissionResult, ResumePermissionStatus};
    /// let r = ResumePermissionResult::new(ResumePermissionStatus::Abandoned);
    /// assert_eq!(r.status, ResumePermissionStatus::Abandoned);
    /// ```
    #[must_use]
    pub fn new(status: ResumePermissionStatus) -> Self {
        Self { status }
    }
}

// --- engine/compact ---

/// Params of the `engine/compact` RPC.
///
/// `trigger` defaults to [`CompactTrigger::Manual`] when omitted: a
/// client-initiated compaction is always manual by definition.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::CompactParams;
/// use zhive_proto::hook::CompactTrigger;
/// let p: CompactParams =
///     serde_json::from_str(r#"{"threadId":"thread:native/x"}"#).unwrap();
/// assert_eq!(p.trigger, CompactTrigger::Manual);
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CompactParams {
    /// Thread to compact.
    pub thread_id: ThreadId,
    /// Why compaction was triggered; defaults to `manual` when omitted.
    #[serde(default = "manual_trigger")]
    pub trigger: CompactTrigger,
}

impl CompactParams {
    /// Constructs params for a manually triggered compaction.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::hook::CompactTrigger;
    /// use zhive_proto::rpc::CompactParams;
    /// let p = CompactParams::new(ThreadId(Arc::from("thread:native/x")), CompactTrigger::Manual);
    /// assert_eq!(p.trigger, CompactTrigger::Manual);
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, trigger: crate::hook::CompactTrigger) -> Self {
        Self { thread_id, trigger }
    }
}

/// Wire classifier for the `engine/compact` reply.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::CompactStatus;
/// let v = serde_json::to_value(CompactStatus::NothingToCompact).unwrap();
/// assert_eq!(v, "nothing_to_compact");
/// let back: CompactStatus = serde_json::from_value(v).unwrap();
/// assert_eq!(back, CompactStatus::NothingToCompact);
/// ```
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CompactStatus {
    /// Compaction ran and replaced transcript items with a summary.
    Compacted,
    /// The transcript was already compact; nothing to do.
    NothingToCompact,
}

/// Result of the `engine/compact` RPC.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::{CompactResult, CompactStatus};
/// let r = CompactResult::new(CompactStatus::Compacted, 42);
/// let v = serde_json::to_value(&r).unwrap();
/// assert_eq!(v["status"], "compacted");
/// assert_eq!(v["entriesCompacted"], 42u32);
/// let back: CompactResult = serde_json::from_value(v).unwrap();
/// assert_eq!(back, r);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CompactResult {
    /// Whether items were actually compacted or the transcript was already clean.
    pub status: CompactStatus,
    /// Number of transcript items folded into the summary (0 if nothing ran).
    pub entries_compacted: u32,
}

impl CompactResult {
    /// Constructs a compaction result.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::rpc::{CompactResult, CompactStatus};
    /// let r = CompactResult::new(CompactStatus::NothingToCompact, 0);
    /// assert_eq!(r.entries_compacted, 0);
    /// ```
    #[must_use]
    pub fn new(status: CompactStatus, entries_compacted: u32) -> Self {
        Self {
            status,
            entries_compacted,
        }
    }
}

// ============================================================
// thread/* RPCs
// ============================================================

// --- thread/fork ---

/// Params of the `thread/fork` RPC.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::ForkParams;
/// let p: ForkParams =
///     serde_json::from_str(r#"{"sourceThreadId":"thread:native/src"}"#).unwrap();
/// assert_eq!(p.source_thread_id.0.as_ref(), "thread:native/src");
/// assert!(p.up_to_item.is_none());
/// assert!(!p.summarize);
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ForkParams {
    /// Thread to fork from.
    pub source_thread_id: ThreadId,
    /// Inclusive item boundary; `None` forks the full history.
    #[serde(default)]
    pub up_to_item: Option<ItemId>,
    /// When `true`, generate an LLM branch summary as the new thread's opener.
    #[serde(default)]
    pub summarize: bool,
}

impl ForkParams {
    /// Constructs params for forking a thread at an optional boundary.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::ForkParams;
    /// let p = ForkParams::new(ThreadId(Arc::from("thread:native/src")), None, false);
    /// assert!(!p.summarize);
    /// ```
    #[must_use]
    pub fn new(
        source_thread_id: ThreadId,
        up_to_item: Option<crate::domain::ItemId>,
        summarize: bool,
    ) -> Self {
        Self {
            source_thread_id,
            up_to_item,
            summarize,
        }
    }
}

/// Result of the `thread/fork` RPC.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::ThreadId;
/// use zhive_proto::rpc::ForkResult;
/// let r = ForkResult::new(ThreadId(Arc::from("thread:native/fork/0")), 5, false);
/// let v = serde_json::to_value(&r).unwrap();
/// assert_eq!(v["newThreadId"], "thread:native/fork/0");
/// assert_eq!(v["itemsReplayed"], 5u32);
/// assert_eq!(v["summarized"], false);
/// let back: ForkResult = serde_json::from_value(v).unwrap();
/// assert_eq!(back, r);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ForkResult {
    /// Id of the newly created forked thread.
    pub new_thread_id: ThreadId,
    /// Number of source items replayed into the new thread.
    pub items_replayed: u32,
    /// Whether a branch summary was generated and prepended.
    pub summarized: bool,
}

impl ForkResult {
    /// Constructs a fork result.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::ForkResult;
    /// let r = ForkResult::new(ThreadId(Arc::from("thread:native/fork/0")), 3, true);
    /// assert!(r.summarized);
    /// ```
    #[must_use]
    pub fn new(new_thread_id: ThreadId, items_replayed: u32, summarized: bool) -> Self {
        Self {
            new_thread_id,
            items_replayed,
            summarized,
        }
    }
}

// --- thread/list ---

/// Optional params for the `thread/list` RPC.
///
/// Omitting params (or sending `null`) lists every thread. Sending
/// `{ "cwd": "/path" }` restricts the result to threads created under
/// that directory.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::ListThreadsParams;
/// let p: ListThreadsParams = serde_json::from_str("{}").unwrap();
/// assert!(p.cwd.is_none());
/// let p: ListThreadsParams =
///     serde_json::from_str(r#"{"cwd":"/work"}"#).unwrap();
/// assert_eq!(p.cwd.as_deref(), Some("/work"));
/// ```
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ListThreadsParams {
    /// Optional working-directory filter; `None` lists all threads.
    #[serde(default)]
    pub cwd: Option<String>,
}

impl ListThreadsParams {
    /// Constructs params with an optional working-directory filter.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::rpc::ListThreadsParams;
    /// let p = ListThreadsParams::new(None);
    /// assert!(p.cwd.is_none());
    /// let p2 = ListThreadsParams::new(Some("/work".into()));
    /// assert_eq!(p2.cwd.as_deref(), Some("/work"));
    /// ```
    #[must_use]
    pub fn new(cwd: Option<String>) -> Self {
        Self { cwd }
    }
}

/// Result of the `thread/list` RPC.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::ListThreadsResult;
/// let r = ListThreadsResult::new(vec![]);
/// let v = serde_json::to_value(&r).unwrap();
/// assert_eq!(v["threads"], serde_json::json!([]));
/// let back: ListThreadsResult = serde_json::from_value(v).unwrap();
/// assert_eq!(back.threads.len(), 0);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ListThreadsResult {
    /// Threads ordered most-recently-updated first; `turns` is always empty.
    pub threads: Vec<Thread>,
}

impl ListThreadsResult {
    /// Constructs a list result from the given threads.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::rpc::ListThreadsResult;
    /// let r = ListThreadsResult::new(vec![]);
    /// assert!(r.threads.is_empty());
    /// ```
    #[must_use]
    pub fn new(threads: Vec<Thread>) -> Self {
        Self { threads }
    }
}

// ============================================================
// Resume thread
// ============================================================

/// Params of the `engine/resume_thread` RPC.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::ResumeThreadParams;
/// let p: ResumeThreadParams =
///     serde_json::from_str(r#"{"threadId":"thread:native/abc"}"#).unwrap();
/// assert_eq!(p.thread_id.0.as_ref(), "thread:native/abc");
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ResumeThreadParams {
    /// Thread to restore into engine memory.
    pub thread_id: ThreadId,
}

impl ResumeThreadParams {
    /// Constructs params for resuming a thread into engine memory.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::ResumeThreadParams;
    /// let p = ResumeThreadParams::new(ThreadId(Arc::from("thread:native/abc")));
    /// assert_eq!(p.thread_id.0.as_ref(), "thread:native/abc");
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId) -> Self {
        Self { thread_id }
    }
}

/// Result of the `engine/resume_thread` RPC.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::ThreadId;
/// use zhive_proto::rpc::ResumeThreadResult;
/// let r = ResumeThreadResult::new(ThreadId(Arc::from("thread:native/abc")), 10, 2);
/// let v = serde_json::to_value(&r).unwrap();
/// assert_eq!(v["threadId"], "thread:native/abc");
/// assert_eq!(v["itemsRestored"], 10u32);
/// assert_eq!(v["turnsRestored"], 2u32);
/// let back: ResumeThreadResult = serde_json::from_value(v).unwrap();
/// assert_eq!(back, r);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ResumeThreadResult {
    /// Id of the resumed thread.
    pub thread_id: ThreadId,
    /// Number of history items restored into memory.
    pub items_restored: u32,
    /// Number of turns spanned by the restored items.
    pub turns_restored: u32,
}

impl ResumeThreadResult {
    /// Constructs a resume result.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::ResumeThreadResult;
    /// let r = ResumeThreadResult::new(ThreadId(Arc::from("thread:native/x")), 4, 1);
    /// assert_eq!(r.turns_restored, 1);
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, items_restored: u32, turns_restored: u32) -> Self {
        Self {
            thread_id,
            items_restored,
            turns_restored,
        }
    }
}

// --- thread/get_items ---

/// Params of the `thread/get_items` RPC.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::GetItemsParams;
/// let p: GetItemsParams =
///     serde_json::from_str(r#"{"threadId":"thread:native/x"}"#).unwrap();
/// assert!(p.turn_id.is_none());
/// assert!(p.offset.is_none());
/// assert!(p.limit.is_none());
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct GetItemsParams {
    /// Thread to read items from.
    pub thread_id: ThreadId,
    /// Scope to a single turn; `None` returns full history.
    #[serde(default)]
    pub turn_id: Option<TurnId>,
    /// Page offset (applied only with `turn_id`).
    #[serde(default)]
    pub offset: Option<i64>,
    /// Page limit (applied only with `turn_id`).
    #[serde(default)]
    pub limit: Option<i64>,
}

impl GetItemsParams {
    /// Constructs params for fetching thread history items.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::GetItemsParams;
    /// let p = GetItemsParams::new(ThreadId(Arc::from("thread:native/x")), None, None, None);
    /// assert!(p.turn_id.is_none());
    /// assert!(p.offset.is_none());
    /// ```
    #[must_use]
    pub fn new(
        thread_id: ThreadId,
        turn_id: Option<crate::domain::TurnId>,
        offset: Option<i64>,
        limit: Option<i64>,
    ) -> Self {
        Self {
            thread_id,
            turn_id,
            offset,
            limit,
        }
    }
}

/// Result of the `thread/get_items` RPC.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::GetItemsResult;
/// let r = GetItemsResult::new(vec![]);
/// let v = serde_json::to_value(&r).unwrap();
/// assert_eq!(v["items"], serde_json::json!([]));
/// let back: GetItemsResult = serde_json::from_value(v).unwrap();
/// assert!(back.items.is_empty());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct GetItemsResult {
    /// History items in conversation order (or paged subset).
    pub items: Vec<Item>,
}

impl GetItemsResult {
    /// Constructs a get-items result.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::rpc::GetItemsResult;
    /// let r = GetItemsResult::new(vec![]);
    /// assert!(r.items.is_empty());
    /// ```
    #[must_use]
    pub fn new(items: Vec<Item>) -> Self {
        Self { items }
    }
}

// ============================================================
// session/* RPCs
// ============================================================

/// Shared params for the three injection-queue methods.
///
/// Used by `session/enqueue_steer`, `session/enqueue_follow_up`, and
/// `session/enqueue_next_turn`. The target queue is selected by the method
/// name, not a payload field.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::InjectionParams;
/// let p: InjectionParams =
///     serde_json::from_str(r#"{"threadId":"thread:native/x","items":[]}"#).unwrap();
/// assert!(p.items.is_empty());
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct InjectionParams {
    /// Target thread.
    pub thread_id: ThreadId,
    /// Items to enqueue; empty by default.
    #[serde(default)]
    pub items: Vec<Item>,
}

impl InjectionParams {
    /// Constructs injection params for a target thread with the given items.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::InjectionParams;
    /// let p = InjectionParams::new(ThreadId(Arc::from("thread:native/x")), vec![]);
    /// assert!(p.items.is_empty());
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, items: Vec<crate::domain::Item>) -> Self {
        Self { thread_id, items }
    }
}

/// Acknowledgement returned by the three injection-queue methods.
///
/// The wire shape is always `{"accepted":true}`; a `false` value is reserved
/// for future use but currently cannot be emitted by the server.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::InjectionAck;
/// let ack = InjectionAck::accepted();
/// let v = serde_json::to_value(&ack).unwrap();
/// assert_eq!(v["accepted"], true);
/// let back: InjectionAck = serde_json::from_value(v).unwrap();
/// assert_eq!(back, ack);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct InjectionAck {
    /// `true` when the items were accepted into the queue.
    pub accepted: bool,
}

impl InjectionAck {
    /// Constructs the canonical `{ "accepted": true }` acknowledgement.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::rpc::InjectionAck;
    /// let ack = InjectionAck::accepted();
    /// assert!(ack.accepted);
    /// ```
    #[must_use]
    pub fn accepted() -> Self {
        Self { accepted: true }
    }
}

/// Params of the `session/cancel` notification.
///
/// Sent by the client to cancel the current turn (or session) on a thread.
/// On the notification channel the server discards the return value; an
/// error is only logged.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::SessionCancelParams;
/// let p: SessionCancelParams =
///     serde_json::from_str(r#"{"threadId":"thread:native/x"}"#).unwrap();
/// assert_eq!(p.thread_id.0.as_ref(), "thread:native/x");
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SessionCancelParams {
    /// Thread whose active turn should be cancelled.
    pub thread_id: ThreadId,
}

// ============================================================
// thread/delete
// ============================================================

/// Params of the `thread/delete` RPC.
///
/// The server rejects deletion when the named thread has an active turn,
/// returning a structured error rather than silently cancelling the turn.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::ThreadId;
/// use zhive_proto::rpc::DeleteThreadParams;
/// let p = DeleteThreadParams::new(ThreadId(Arc::from("thread:native/abc")));
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["threadId"], "thread:native/abc");
/// let back: DeleteThreadParams = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DeleteThreadParams {
    /// Thread to delete.
    pub thread_id: ThreadId,
}

impl DeleteThreadParams {
    /// Constructs params for deleting `thread_id`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::DeleteThreadParams;
    /// let p = DeleteThreadParams::new(ThreadId(Arc::from("thread:native/x")));
    /// assert_eq!(p.thread_id.0.as_ref(), "thread:native/x");
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId) -> Self {
        Self { thread_id }
    }
}

/// Result of the `thread/delete` RPC.
///
/// `deleted` is `true` when a persistent row or rollout file was removed.
/// `false` indicates the thread was not found (idempotent delete). When the
/// thread has an active turn the server returns an error instead of this type.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::DeleteThreadResult;
/// let r = DeleteThreadResult::new(true);
/// let v = serde_json::to_value(&r).unwrap();
/// assert_eq!(v["deleted"], true);
/// let back: DeleteThreadResult = serde_json::from_value(v).unwrap();
/// assert_eq!(back, r);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DeleteThreadResult {
    /// `true` when a row/rollout existed and was removed; `false` when the
    /// thread was unknown (idempotent).
    pub deleted: bool,
}

impl DeleteThreadResult {
    /// Constructs a delete result.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::rpc::DeleteThreadResult;
    /// let r = DeleteThreadResult::new(false);
    /// assert!(!r.deleted);
    /// ```
    #[must_use]
    pub fn new(deleted: bool) -> Self {
        Self { deleted }
    }
}

// ============================================================
// thread/rename
// ============================================================

/// Params of the `thread/rename` RPC.
///
/// An empty `name` clears the label (stored as `NULL` in the database). The
/// operation is best-effort: rename is accepted and queued to the async
/// persistence writer; `renamed: true` in the result means the request was
/// enqueued, not that it has already hit disk.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::ThreadId;
/// use zhive_proto::rpc::RenameThreadParams;
/// let p = RenameThreadParams::new(
///     ThreadId(Arc::from("thread:native/abc")),
///     "My feature branch".into(),
/// );
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["name"], "My feature branch");
/// let back: RenameThreadParams = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RenameThreadParams {
    /// Thread to rename.
    pub thread_id: ThreadId,
    /// New display name; an empty string clears the name (stored as `NULL`).
    pub name: String,
}

impl RenameThreadParams {
    /// Constructs params for renaming a thread.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::RenameThreadParams;
    /// let p = RenameThreadParams::new(ThreadId(Arc::from("thread:native/x")), "foo".into());
    /// assert_eq!(p.name, "foo");
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, name: String) -> Self {
        Self { thread_id, name }
    }
}

/// Result of the `thread/rename` RPC.
///
/// `renamed` is `true` when the rename was accepted into the persistence
/// queue. It does **not** guarantee the SQL row was updated before this
/// response was sent (the writer is async). `false` is returned when
/// persistence is unavailable (in-memory-only engine).
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::RenameThreadResult;
/// let r = RenameThreadResult::new(true);
/// let v = serde_json::to_value(&r).unwrap();
/// assert_eq!(v["renamed"], true);
/// let back: RenameThreadResult = serde_json::from_value(v).unwrap();
/// assert_eq!(back, r);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RenameThreadResult {
    /// `true` when the rename was accepted (best-effort); `false` when
    /// persistence is unavailable.
    pub renamed: bool,
}

impl RenameThreadResult {
    /// Constructs a rename result.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::rpc::RenameThreadResult;
    /// let r = RenameThreadResult::new(true);
    /// assert!(r.renamed);
    /// ```
    #[must_use]
    pub fn new(renamed: bool) -> Self {
        Self { renamed }
    }
}

// ============================================================
// thread/search
// ============================================================

/// Params of the `thread/search` RPC.
///
/// Matches threads whose `name`, `preview`, or `cwd` contains `query` as a
/// case-insensitive substring. An empty `query` returns all threads (subject
/// to the optional `cwd` filter).
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::SearchThreadsParams;
/// let p = SearchThreadsParams::new("refactor".into(), None);
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["query"], "refactor");
/// assert!(v.get("cwd").is_none());
/// let back: SearchThreadsParams = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SearchThreadsParams {
    /// Case-insensitive substring matched against `name`, `preview`, and `cwd`.
    pub query: String,
    /// Optional cwd narrowing applied before the substring match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl SearchThreadsParams {
    /// Constructs search params with an optional cwd pre-filter.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::rpc::SearchThreadsParams;
    /// let p = SearchThreadsParams::new("foo".into(), Some("/work".into()));
    /// assert_eq!(p.cwd.as_deref(), Some("/work"));
    /// ```
    #[must_use]
    pub fn new(query: String, cwd: Option<String>) -> Self {
        Self { query, cwd }
    }
}

/// Result of the `thread/search` RPC.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::SearchThreadsResult;
/// let r = SearchThreadsResult::new(vec![]);
/// let v = serde_json::to_value(&r).unwrap();
/// assert_eq!(v["threads"], serde_json::json!([]));
/// let back: SearchThreadsResult = serde_json::from_value(v).unwrap();
/// assert!(back.threads.is_empty());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SearchThreadsResult {
    /// Matching threads ordered most-recently-updated first; `turns` is always
    /// empty.
    pub threads: Vec<Thread>,
}

impl SearchThreadsResult {
    /// Constructs a search result from the given threads.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::rpc::SearchThreadsResult;
    /// let r = SearchThreadsResult::new(vec![]);
    /// assert!(r.threads.is_empty());
    /// ```
    #[must_use]
    pub fn new(threads: Vec<Thread>) -> Self {
        Self { threads }
    }
}

// ============================================================
// tools/* — tool discovery
// ============================================================

/// Coarse classification of a discoverable tool.
///
/// Wire mirror of the engine's `domain::ToolKind`; defined as a separate type
/// so that the discovery and display concerns can evolve independently.
/// Zero-cost conversion from [`ToolKind`] is provided via [`From`].
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::ToolSpecKind;
/// let v = serde_json::to_value(ToolSpecKind::Read).unwrap();
/// assert_eq!(v, "read");
/// let back: ToolSpecKind = serde_json::from_value(v).unwrap();
/// assert_eq!(back, ToolSpecKind::Read);
/// ```
#[derive(Default, Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolSpecKind {
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

impl From<ToolKind> for ToolSpecKind {
    /// Converts a domain [`ToolKind`] to its discovery wire form.
    ///
    /// The mapping is 1:1; both enums share the same variant set.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::domain::ToolKind;
    /// use zhive_proto::rpc::ToolSpecKind;
    /// assert_eq!(ToolSpecKind::from(ToolKind::Read), ToolSpecKind::Read);
    /// assert_eq!(ToolSpecKind::from(ToolKind::Other), ToolSpecKind::Other);
    /// ```
    fn from(k: ToolKind) -> Self {
        match k {
            ToolKind::Read => Self::Read,
            ToolKind::Edit => Self::Edit,
            ToolKind::Delete => Self::Delete,
            ToolKind::Move => Self::Move,
            ToolKind::Search => Self::Search,
            ToolKind::Execute => Self::Execute,
            ToolKind::Think => Self::Think,
            ToolKind::Fetch => Self::Fetch,
            ToolKind::SwitchMode => Self::SwitchMode,
            _ => Self::Other,
        }
    }
}

/// Metadata descriptor for one engine-registered tool.
///
/// Returned by the `tools/list` RPC to let clients discover which tools the
/// connected engine instance exposes, their input schema, and a coarse
/// classification for UI grouping.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::{ToolSpec, ToolSpecKind};
/// let spec = ToolSpec::new(
///     "read_file".into(),
///     Some("Read a file from the filesystem.".into()),
///     ToolSpecKind::Read,
///     serde_json::json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
/// );
/// let v = serde_json::to_value(&spec).unwrap();
/// assert_eq!(v["name"], "read_file");
/// assert_eq!(v["kind"], "read");
/// let back: ToolSpec = serde_json::from_value(v).unwrap();
/// assert_eq!(back, spec);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ToolSpec {
    /// Stable tool name matching the engine's dispatch key.
    pub name: String,
    /// Human-readable description, if the tool provides one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Coarse classification for UI grouping.
    #[serde(default)]
    pub kind: ToolSpecKind,
    /// JSON Schema (object) describing the tool's input arguments.
    ///
    /// Intentionally kept as opaque `Value`: tool input schemas are
    /// tool-defined and not validated at the proto layer.
    pub input_schema: Value,
}

impl ToolSpec {
    /// Constructs a tool spec from its core fields.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::rpc::{ToolSpec, ToolSpecKind};
    /// let spec = ToolSpec::new(
    ///     "bash".into(),
    ///     Some("Run a shell command.".into()),
    ///     ToolSpecKind::Execute,
    ///     serde_json::json!({}),
    /// );
    /// assert_eq!(spec.name, "bash");
    /// ```
    #[must_use]
    pub fn new(
        name: String,
        description: Option<String>,
        kind: ToolSpecKind,
        input_schema: Value,
    ) -> Self {
        Self {
            name,
            description,
            kind,
            input_schema,
        }
    }
}

/// Params of the `tools/list` RPC.
///
/// Currently carries no fields; accepts `null` or `{}` on the wire.
/// Reserved as `#[non_exhaustive]` so filtering fields can be added without
/// a wire break.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::ToolListParams;
/// // Both null and {} are accepted on the wire.
/// let _p: ToolListParams = serde_json::from_str("{}").unwrap();
/// let _p2 = ToolListParams::default();
/// ```
// The empty braces are intentional: this is an extensible params type. The
// unit-struct form does not roundtrip `{}` through serde_json correctly, and
// removing the braces would break future field additions without a wire break.
#[expect(
    clippy::empty_structs_with_brackets,
    reason = "extensible params placeholder; unit struct breaks serde {} roundtrip"
)]
#[derive(Default, Debug, Clone, Deserialize, Serialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ToolListParams {}

/// Result of the `tools/list` RPC.
///
/// # Examples
///
/// ```
/// use zhive_proto::rpc::ToolListResult;
/// let r = ToolListResult::new(vec![]);
/// let v = serde_json::to_value(&r).unwrap();
/// assert_eq!(v["tools"], serde_json::json!([]));
/// let back: ToolListResult = serde_json::from_value(v).unwrap();
/// assert!(back.tools.is_empty());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ToolListResult {
    /// All engine-registered tools, sorted by name.
    pub tools: Vec<ToolSpec>,
}

impl ToolListResult {
    /// Constructs a tool list result.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::rpc::ToolListResult;
    /// let r = ToolListResult::new(vec![]);
    /// assert!(r.tools.is_empty());
    /// ```
    #[must_use]
    pub fn new(tools: Vec<ToolSpec>) -> Self {
        Self { tools }
    }
}

// Rust guideline compliant 2026-02-21

//! Owned event payload types for every engine notification (A2 contract freeze).
//!
//! Previously these types were private, borrowed structs (`struct Foo<'a>`)
//! inside `zhive-core::server::events`. This module provides owned equivalents
//! so that client SDKs and the TUI can deserialize them without access to
//! core internals.
//!
//! # Wire stability
//!
//! Every `serde` attribute is copied verbatim from the original private
//! definitions. Any change to field names, `rename_all`, or
//! `skip_serializing_if` attributes is a breaking wire change.
//!
//! # Relationship to `domain` notification types
//!
//! [`crate::domain::TurnStartedNotification`] and
//! [`crate::domain::TurnCompletedNotification`] embed an entire [`Turn`] and
//! are synthesised by the ACP/MCP bridge layer. The payloads in this module
//! (`TurnStartedPayload`, `TurnCompletedPayload`) carry only `threadId` and
//! `turnId` and are what the engine actually emits on the wire. Both coexist
//! intentionally: the `domain` types are for bridge-level Turn boundary
//! synthesis; the `events` types are the live event stream.
//!
//! Three event types remain in [`crate::permission`] because they were already
//! owned and canonical there:
//!
//! * `events/turn_suspended` → [`crate::permission::TurnSuspendedNotification`]
//! * `events/turn_resumed`   → [`crate::permission::TurnResumedNotification`]
//! * `events/session_aborted` → [`crate::permission::SessionAbortedNotification`]
//!
//! [`Turn`]: crate::domain::Turn

use serde::{Deserialize, Serialize};

#[cfg(feature = "schema")]
use schemars::JsonSchema;

use crate::domain::{Item, ItemId, ThreadId, TurnError, TurnId};
use crate::hook::{CompactTrigger, EnginePhase};
use crate::permission::RequestPermissionRequest;

// ============================================================
// events/usage
// ============================================================

/// Payload of the `events/usage` notification.
///
/// Carries the token counts reported by a single provider call, identified
/// by the owning thread and turn.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::{ThreadId, TurnId};
/// use zhive_proto::events::UsagePayload;
/// let p = UsagePayload::new(
///     ThreadId(Arc::from("thread:native/a")),
///     TurnId(Arc::from("turn:thread:native/a/0")),
///     100,
///     50,
/// );
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["threadId"], "thread:native/a");
/// assert_eq!(v["inputTokens"], 100u64);
/// assert_eq!(v["outputTokens"], 50u64);
/// let back: UsagePayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct UsagePayload {
    /// Thread the token usage belongs to.
    pub thread_id: ThreadId,
    /// Turn the token usage belongs to.
    pub turn_id: TurnId,
    /// Input tokens consumed by the provider call.
    pub input_tokens: u64,
    /// Output tokens produced by the provider call.
    pub output_tokens: u64,
}

impl UsagePayload {
    /// Constructs a usage payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::{ThreadId, TurnId};
    /// use zhive_proto::events::UsagePayload;
    /// let p = UsagePayload::new(
    ///     ThreadId(Arc::from("t")),
    ///     TurnId(Arc::from("t/0")),
    ///     10, 5,
    /// );
    /// assert_eq!(p.input_tokens, 10);
    /// ```
    #[must_use]
    pub fn new(
        thread_id: ThreadId,
        turn_id: TurnId,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Self {
        Self {
            thread_id,
            turn_id,
            input_tokens,
            output_tokens,
        }
    }
}

// ============================================================
// events/turn_started
// ============================================================

/// Payload of the `events/turn_started` notification.
///
/// The engine emits only `threadId` and `turnId` here; the full `Turn`
/// snapshot is available via [`crate::domain::TurnStartedNotification`]
/// which is synthesised at the bridge layer.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::{ThreadId, TurnId};
/// use zhive_proto::events::TurnStartedPayload;
/// let p = TurnStartedPayload::new(
///     ThreadId(Arc::from("thread:native/a")),
///     TurnId(Arc::from("turn:thread:native/a/0")),
/// );
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["threadId"], "thread:native/a");
/// assert_eq!(v["turnId"], "turn:thread:native/a/0");
/// let back: TurnStartedPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TurnStartedPayload {
    /// Thread the turn started on.
    pub thread_id: ThreadId,
    /// Newly started turn id.
    pub turn_id: TurnId,
}

impl TurnStartedPayload {
    /// Constructs a turn-started payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::{ThreadId, TurnId};
    /// use zhive_proto::events::TurnStartedPayload;
    /// let p = TurnStartedPayload::new(
    ///     ThreadId(Arc::from("t")), TurnId(Arc::from("t/0")),
    /// );
    /// assert_eq!(p.turn_id.0.as_ref(), "t/0");
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, turn_id: TurnId) -> Self {
        Self { thread_id, turn_id }
    }
}

// ============================================================
// events/turn_rejected
// ============================================================

/// Wire mirror of the engine's `TurnRejectionReason`.
///
/// The three serde attributes (`tag`, `rename_all`, `rename_all_fields`) must
/// be kept in sync with the original private definition in `zhive-core` so
/// the JSON shape `{"kind":"engine_busy","currentPhase":"..."}` is preserved.
///
/// # Examples
///
/// ```
/// use zhive_proto::hook::EnginePhase;
/// use zhive_proto::events::TurnRejectedReason;
/// let r = TurnRejectedReason::EngineBusy { current_phase: EnginePhase::Compaction };
/// let v = serde_json::to_value(&r).unwrap();
/// assert_eq!(v["kind"], "engine_busy");
/// assert_eq!(v["currentPhase"], "compaction");
/// let back: TurnRejectedReason = serde_json::from_value(v).unwrap();
/// assert_eq!(back, r);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
pub enum TurnRejectedReason {
    /// The engine was busy with another phase when the turn was requested.
    EngineBusy {
        /// Phase the engine was in when it rejected the turn.
        current_phase: EnginePhase,
    },
}

/// Payload of the `events/turn_rejected` notification.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::ThreadId;
/// use zhive_proto::hook::EnginePhase;
/// use zhive_proto::events::{TurnRejectedPayload, TurnRejectedReason};
/// let p = TurnRejectedPayload::new(
///     ThreadId(Arc::from("thread:native/t")),
///     TurnRejectedReason::EngineBusy { current_phase: EnginePhase::Compaction },
/// );
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["reason"]["kind"], "engine_busy");
/// assert_eq!(v["reason"]["currentPhase"], "compaction");
/// let back: TurnRejectedPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TurnRejectedPayload {
    /// Thread whose turn was rejected.
    pub thread_id: ThreadId,
    /// Reason the turn was rejected.
    pub reason: TurnRejectedReason,
}

impl TurnRejectedPayload {
    /// Constructs a turn-rejected payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::hook::EnginePhase;
    /// use zhive_proto::events::{TurnRejectedPayload, TurnRejectedReason};
    /// let p = TurnRejectedPayload::new(
    ///     ThreadId(Arc::from("t")),
    ///     TurnRejectedReason::EngineBusy { current_phase: EnginePhase::Turn },
    /// );
    /// assert_eq!(p.thread_id.0.as_ref(), "t");
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, reason: TurnRejectedReason) -> Self {
        Self { thread_id, reason }
    }
}

// ============================================================
// events/turn_completed
// ============================================================

/// Payload of the `events/turn_completed` notification.
///
/// The engine emits only `threadId` and `turnId` here; the full `Turn`
/// snapshot is available via [`crate::domain::TurnCompletedNotification`]
/// which is synthesised at the bridge layer.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::{ThreadId, TurnId};
/// use zhive_proto::events::TurnCompletedPayload;
/// let p = TurnCompletedPayload::new(
///     ThreadId(Arc::from("thread:native/a")),
///     TurnId(Arc::from("turn:thread:native/a/0")),
/// );
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["threadId"], "thread:native/a");
/// assert_eq!(v["turnId"], "turn:thread:native/a/0");
/// let back: TurnCompletedPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TurnCompletedPayload {
    /// Thread the turn completed on.
    pub thread_id: ThreadId,
    /// Completed turn id.
    pub turn_id: TurnId,
}

impl TurnCompletedPayload {
    /// Constructs a turn-completed payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::{ThreadId, TurnId};
    /// use zhive_proto::events::TurnCompletedPayload;
    /// let p = TurnCompletedPayload::new(
    ///     ThreadId(Arc::from("t")), TurnId(Arc::from("t/0")),
    /// );
    /// assert_eq!(p.turn_id.0.as_ref(), "t/0");
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, turn_id: TurnId) -> Self {
        Self { thread_id, turn_id }
    }
}

// ============================================================
// events/turn_failed
// ============================================================

/// Payload of the `events/turn_failed` notification.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::{ThreadId, TurnError, TurnId};
/// use zhive_proto::events::TurnFailedPayload;
/// let err: TurnError = serde_json::from_str(r#"{"message":"oops"}"#).unwrap();
/// let p = TurnFailedPayload::new(
///     ThreadId(Arc::from("thread:native/a")),
///     TurnId(Arc::from("turn:thread:native/a/0")),
///     err.clone(),
/// );
/// let v = serde_json::to_value(&p).unwrap();
/// // TurnError is nested under the "error" field.
/// assert_eq!(v["error"]["message"], "oops");
/// let back: TurnFailedPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back.error, err);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TurnFailedPayload {
    /// Thread the turn failed on.
    pub thread_id: ThreadId,
    /// Failed turn id.
    pub turn_id: TurnId,
    /// Failure details.
    pub error: TurnError,
}

impl TurnFailedPayload {
    /// Constructs a turn-failed payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::{ThreadId, TurnError, TurnId};
    /// use zhive_proto::events::TurnFailedPayload;
    /// let err: TurnError = serde_json::from_str(r#"{"message":"err"}"#).unwrap();
    /// let p = TurnFailedPayload::new(
    ///     ThreadId(Arc::from("t")),
    ///     TurnId(Arc::from("t/0")),
    ///     err,
    /// );
    /// assert_eq!(p.error.message, "err");
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, turn_id: TurnId, error: TurnError) -> Self {
        Self {
            thread_id,
            turn_id,
            error,
        }
    }
}

// ============================================================
// events/item_appended
// ============================================================

/// Payload of the `events/item_appended` notification.
///
/// `item_id` is surfaced at the top level so subscribers can index by id
/// without re-deriving it from the embedded item.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::{Item, ItemId, ThreadId, TurnId};
/// use zhive_proto::events::ItemAppendedPayload;
/// let item = Item::AgentMessage {
///     id: ItemId(Arc::from("item:t/0/0")),
///     text: "hello".into(),
/// };
/// let p = ItemAppendedPayload::new(
///     ThreadId(Arc::from("thread:native/t")),
///     TurnId(Arc::from("turn:thread:native/t/0")),
///     ItemId(Arc::from("item:t/0/0")),
///     item.clone(),
/// );
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["itemId"], "item:t/0/0");
/// assert!(v["item"].is_object());
/// let back: ItemAppendedPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back.item, item);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ItemAppendedPayload {
    /// Thread the item was appended to.
    pub thread_id: ThreadId,
    /// Turn the item was appended to.
    pub turn_id: TurnId,
    /// Top-level item id (mirrors `item.id()` for indexing convenience).
    pub item_id: ItemId,
    /// The full item.
    pub item: Item,
}

impl ItemAppendedPayload {
    /// Constructs an item-appended payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::{Item, ItemId, ThreadId, TurnId};
    /// use zhive_proto::events::ItemAppendedPayload;
    /// let item = Item::AgentMessage {
    ///     id: ItemId(Arc::from("item:0")),
    ///     text: "hi".into(),
    /// };
    /// let p = ItemAppendedPayload::new(
    ///     ThreadId(Arc::from("t")),
    ///     TurnId(Arc::from("t/0")),
    ///     ItemId(Arc::from("item:0")),
    ///     item,
    /// );
    /// assert_eq!(p.item_id.0.as_ref(), "item:0");
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, turn_id: TurnId, item_id: ItemId, item: Item) -> Self {
        Self {
            thread_id,
            turn_id,
            item_id,
            item,
        }
    }
}

// ============================================================
// events/item_delta
// ============================================================

/// Payload of the `events/item_delta` notification.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::{ThreadId, TurnId};
/// use zhive_proto::events::ItemDeltaPayload;
/// let p = ItemDeltaPayload::new(
///     ThreadId(Arc::from("thread:native/t")),
///     TurnId(Arc::from("turn:thread:native/t/0")),
///     "Hello".into(),
/// );
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["delta"], "Hello");
/// let back: ItemDeltaPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ItemDeltaPayload {
    /// Thread the delta belongs to.
    pub thread_id: ThreadId,
    /// Turn the delta belongs to.
    pub turn_id: TurnId,
    /// Streaming text fragment.
    pub delta: String,
}

impl ItemDeltaPayload {
    /// Constructs an item-delta payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::{ThreadId, TurnId};
    /// use zhive_proto::events::ItemDeltaPayload;
    /// let p = ItemDeltaPayload::new(
    ///     ThreadId(Arc::from("t")), TurnId(Arc::from("t/0")), "x".into(),
    /// );
    /// assert_eq!(p.delta, "x");
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, turn_id: TurnId, delta: String) -> Self {
        Self {
            thread_id,
            turn_id,
            delta,
        }
    }
}

// ============================================================
// events/phase_changed
// ============================================================

/// Payload of the `events/phase_changed` notification.
///
/// `thread_id` is `None` for engine-global phase transitions and is
/// omitted from the wire (`skip_serializing_if`) when absent.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::ThreadId;
/// use zhive_proto::hook::EnginePhase;
/// use zhive_proto::events::PhaseChangedPayload;
/// // With thread id.
/// let p = PhaseChangedPayload::new(
///     Some(ThreadId(Arc::from("thread:native/t"))),
///     EnginePhase::Idle,
///     EnginePhase::Turn,
/// );
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["threadId"], "thread:native/t");
/// assert_eq!(v["from"], "idle");
/// assert_eq!(v["to"], "turn");
/// // Without thread id — field is absent.
/// let p2 = PhaseChangedPayload::new(None, EnginePhase::Idle, EnginePhase::Compaction);
/// let v2 = serde_json::to_value(&p2).unwrap();
/// assert!(v2.get("threadId").is_none());
/// assert_eq!(v2["to"], "compaction");
/// let back: PhaseChangedPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PhaseChangedPayload {
    /// Thread involved in the transition; `None` for engine-global events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<ThreadId>,
    /// Phase the engine left.
    pub from: EnginePhase,
    /// Phase the engine entered.
    pub to: EnginePhase,
}

impl PhaseChangedPayload {
    /// Constructs a phase-changed payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::hook::EnginePhase;
    /// use zhive_proto::events::PhaseChangedPayload;
    /// let p = PhaseChangedPayload::new(None, EnginePhase::Idle, EnginePhase::Turn);
    /// assert!(p.thread_id.is_none());
    /// ```
    #[must_use]
    pub fn new(thread_id: Option<ThreadId>, from: EnginePhase, to: EnginePhase) -> Self {
        Self {
            thread_id,
            from,
            to,
        }
    }
}

// ============================================================
// events/compaction_started
// ============================================================

/// Payload of the `events/compaction_started` notification.
///
/// Emitted when context compaction enters the summarization phase. Anchors
/// the `compaction_delta` / `compaction_completed` / `compaction_failed`
/// bracket that follows for the same thread.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::ThreadId;
/// use zhive_proto::hook::CompactTrigger;
/// use zhive_proto::events::CompactionStartedPayload;
/// let p = CompactionStartedPayload::new(
///     ThreadId(Arc::from("thread:native/t")),
///     CompactTrigger::Manual,
///     12,
/// );
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["threadId"], "thread:native/t");
/// assert_eq!(v["trigger"], "manual");
/// assert_eq!(v["entries"], 12u32);
/// let back: CompactionStartedPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CompactionStartedPayload {
    /// Thread being compacted.
    pub thread_id: ThreadId,
    /// Why compaction fired (`manual` for `/compact`, `auto` for the threshold).
    pub trigger: CompactTrigger,
    /// Transcript items that will be folded into the summary.
    pub entries: u32,
}

impl CompactionStartedPayload {
    /// Constructs a compaction-started payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::hook::CompactTrigger;
    /// use zhive_proto::events::CompactionStartedPayload;
    /// let p = CompactionStartedPayload::new(
    ///     ThreadId(Arc::from("t")), CompactTrigger::Auto, 3,
    /// );
    /// assert_eq!(p.entries, 3);
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, trigger: CompactTrigger, entries: u32) -> Self {
        Self {
            thread_id,
            trigger,
            entries,
        }
    }
}

// ============================================================
// events/compaction_delta
// ============================================================

/// Payload of the `events/compaction_delta` notification.
///
/// One streamed fragment of the compaction summary as the model produces it.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::ThreadId;
/// use zhive_proto::events::CompactionDeltaPayload;
/// let p = CompactionDeltaPayload::new(ThreadId(Arc::from("t")), "Hello".into());
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["delta"], "Hello");
/// let back: CompactionDeltaPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CompactionDeltaPayload {
    /// Thread being compacted.
    pub thread_id: ThreadId,
    /// Streaming summary fragment.
    pub delta: String,
}

impl CompactionDeltaPayload {
    /// Constructs a compaction-delta payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::events::CompactionDeltaPayload;
    /// let p = CompactionDeltaPayload::new(ThreadId(Arc::from("t")), "x".into());
    /// assert_eq!(p.delta, "x");
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, delta: String) -> Self {
        Self { thread_id, delta }
    }
}

// ============================================================
// events/compaction_completed
// ============================================================

/// Payload of the `events/compaction_completed` notification.
///
/// Closes the delta bracket; the persisted summary item also arrives via
/// `events/item_appended`.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::ThreadId;
/// use zhive_proto::events::CompactionCompletedPayload;
/// let p = CompactionCompletedPayload::new(ThreadId(Arc::from("t")), 42);
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["entriesCompacted"], 42u32);
/// let back: CompactionCompletedPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CompactionCompletedPayload {
    /// Thread that was compacted.
    pub thread_id: ThreadId,
    /// Transcript items folded into the summary.
    pub entries_compacted: u32,
}

impl CompactionCompletedPayload {
    /// Constructs a compaction-completed payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::events::CompactionCompletedPayload;
    /// let p = CompactionCompletedPayload::new(ThreadId(Arc::from("t")), 7);
    /// assert_eq!(p.entries_compacted, 7);
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, entries_compacted: u32) -> Self {
        Self {
            thread_id,
            entries_compacted,
        }
    }
}

// ============================================================
// events/compaction_failed
// ============================================================

/// Payload of the `events/compaction_failed` notification.
///
/// Carries the failure reason because the `engine/compact` reply already
/// returned `Started` and the error can no longer travel back through it.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::ThreadId;
/// use zhive_proto::events::CompactionFailedPayload;
/// let p = CompactionFailedPayload::new(ThreadId(Arc::from("t")), "boom".into());
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["reason"], "boom");
/// let back: CompactionFailedPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct CompactionFailedPayload {
    /// Thread whose compaction failed.
    pub thread_id: ThreadId,
    /// Human-readable failure reason.
    pub reason: String,
}

impl CompactionFailedPayload {
    /// Constructs a compaction-failed payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::events::CompactionFailedPayload;
    /// let p = CompactionFailedPayload::new(ThreadId(Arc::from("t")), "x".into());
    /// assert_eq!(p.reason, "x");
    /// ```
    #[must_use]
    pub fn new(thread_id: ThreadId, reason: String) -> Self {
        Self { thread_id, reason }
    }
}

// ============================================================
// events/permission_requested
// ============================================================

/// Payload of the `events/permission_requested` notification.
///
/// # Examples
///
/// ```
/// use zhive_proto::permission::RequestPermissionRequest;
/// use zhive_proto::events::PermissionRequestedPayload;
/// // Use deserialization to construct `#[non_exhaustive]` RequestPermissionRequest.
/// let req: RequestPermissionRequest = serde_json::from_str(r#"{
///     "threadId": "thread:native/t",
///     "resourceType": "tool",
///     "name": "bash",
///     "reason": "run test",
///     "options": []
/// }"#).unwrap();
/// let p = PermissionRequestedPayload::new("perm:1".into(), req);
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["requestId"], "perm:1");
/// assert_eq!(v["request"]["name"], "bash");
/// let back: PermissionRequestedPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back.request_id, "perm:1");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PermissionRequestedPayload {
    /// Opaque request id; the client echoes this to `session/resume_permission`.
    pub request_id: String,
    /// The full permission request details.
    pub request: RequestPermissionRequest,
}

impl PermissionRequestedPayload {
    /// Constructs a permission-requested payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_proto::permission::RequestPermissionRequest;
    /// use zhive_proto::events::PermissionRequestedPayload;
    /// let req: RequestPermissionRequest = serde_json::from_str(r#"{
    ///     "threadId": "t", "resourceType": "tool",
    ///     "name": "bash", "reason": "test", "options": []
    /// }"#).unwrap();
    /// let p = PermissionRequestedPayload::new("perm:1".into(), req);
    /// assert_eq!(p.request_id, "perm:1");
    /// ```
    #[must_use]
    pub fn new(request_id: String, request: RequestPermissionRequest) -> Self {
        Self {
            request_id,
            request,
        }
    }
}

// ============================================================
// events/subagent_started
// ============================================================

/// Payload of the `events/subagent_started` notification.
///
/// Optional fields are omitted from the wire when absent.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::ThreadId;
/// use zhive_proto::events::SubagentStartedPayload;
/// let p = SubagentStartedPayload::new(
///     ThreadId(Arc::from("thread:native/parent")),
///     ThreadId(Arc::from("thread:native/child")),
///     Some("scout".into()),
///     Some("read-only scout".into()),
/// );
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["agentType"], "scout");
/// // Without optional fields — they must be absent on the wire.
/// let p2 = SubagentStartedPayload::new(
///     ThreadId(Arc::from("p")), ThreadId(Arc::from("c")), None, None,
/// );
/// let v2 = serde_json::to_value(&p2).unwrap();
/// assert!(v2.get("agentType").is_none());
/// assert!(v2.get("description").is_none());
/// let back: SubagentStartedPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SubagentStartedPayload {
    /// Parent thread that spawned the subagent.
    pub parent_thread_id: ThreadId,
    /// Newly started child thread.
    pub child_thread_id: ThreadId,
    /// Subagent type label (e.g. `"scout"`); omitted when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Subagent human-readable description; omitted when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl SubagentStartedPayload {
    /// Constructs a subagent-started payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::events::SubagentStartedPayload;
    /// let p = SubagentStartedPayload::new(
    ///     ThreadId(Arc::from("p")),
    ///     ThreadId(Arc::from("c")),
    ///     None,
    ///     None,
    /// );
    /// assert!(p.agent_type.is_none());
    /// ```
    #[must_use]
    pub fn new(
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        agent_type: Option<String>,
        description: Option<String>,
    ) -> Self {
        Self {
            parent_thread_id,
            child_thread_id,
            agent_type,
            description,
        }
    }
}

// ============================================================
// events/subagent_completed
// ============================================================

/// Payload of the `events/subagent_completed` notification.
///
/// The child's `final_message` item is intentionally not serialised;
/// external clients observe the child's items via `events/item_appended`
/// on the child thread. This notification carries only the parent–child
/// relationship and a boolean indicating whether a final message was
/// produced.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::ThreadId;
/// use zhive_proto::events::SubagentCompletedPayload;
/// let p = SubagentCompletedPayload::new(
///     ThreadId(Arc::from("thread:native/parent")),
///     ThreadId(Arc::from("thread:native/child")),
///     true,
/// );
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["hasFinalMessage"], true);
/// let back: SubagentCompletedPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SubagentCompletedPayload {
    /// Parent thread that spawned the subagent.
    pub parent_thread_id: ThreadId,
    /// Completed child thread.
    pub child_thread_id: ThreadId,
    /// `true` when the child turn produced a non-empty final message.
    ///
    /// Always serialized on the wire (including `false`); `#[serde(default)]`
    /// only relaxes deserialization so a missing field parses as `false`.
    #[serde(default)]
    pub has_final_message: bool,
}

impl SubagentCompletedPayload {
    /// Constructs a subagent-completed payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::events::SubagentCompletedPayload;
    /// let p = SubagentCompletedPayload::new(
    ///     ThreadId(Arc::from("p")),
    ///     ThreadId(Arc::from("c")),
    ///     false,
    /// );
    /// assert!(!p.has_final_message);
    /// ```
    #[must_use]
    pub fn new(
        parent_thread_id: ThreadId,
        child_thread_id: ThreadId,
        has_final_message: bool,
    ) -> Self {
        Self {
            parent_thread_id,
            child_thread_id,
            has_final_message,
        }
    }
}

// ============================================================
// events/thread_forked
// ============================================================

/// Payload of the `events/thread_forked` notification.
///
/// `forked_from_item` is `None` for a full-history fork and is omitted
/// from the wire when absent.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::{ItemId, ThreadId};
/// use zhive_proto::events::ThreadForkedPayload;
/// // With item boundary.
/// let p = ThreadForkedPayload::new(
///     ThreadId(Arc::from("thread:native/src")),
///     ThreadId(Arc::from("thread:native/fork/0")),
///     Some(ItemId(Arc::from("item:src/1"))),
/// );
/// let v = serde_json::to_value(&p).unwrap();
/// assert_eq!(v["sourceThreadId"], "thread:native/src");
/// assert_eq!(v["forkedFromItem"], "item:src/1");
/// // Full-history fork — field absent.
/// let p2 = ThreadForkedPayload::new(
///     ThreadId(Arc::from("thread:native/src")),
///     ThreadId(Arc::from("thread:native/fork/1")),
///     None,
/// );
/// let v2 = serde_json::to_value(&p2).unwrap();
/// assert!(v2.get("forkedFromItem").is_none());
/// let back: ThreadForkedPayload = serde_json::from_value(v).unwrap();
/// assert_eq!(back, p);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ThreadForkedPayload {
    /// Source thread that was forked.
    pub source_thread_id: ThreadId,
    /// Newly created fork thread.
    pub new_thread_id: ThreadId,
    /// Inclusive item the fork was taken at; omitted for a full-history fork.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forked_from_item: Option<ItemId>,
}

impl ThreadForkedPayload {
    /// Constructs a thread-forked payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::events::ThreadForkedPayload;
    /// let p = ThreadForkedPayload::new(
    ///     ThreadId(Arc::from("src")),
    ///     ThreadId(Arc::from("fork")),
    ///     None,
    /// );
    /// assert!(p.forked_from_item.is_none());
    /// ```
    #[must_use]
    pub fn new(
        source_thread_id: ThreadId,
        new_thread_id: ThreadId,
        forked_from_item: Option<ItemId>,
    ) -> Self {
        Self {
            source_thread_id,
            new_thread_id,
            forked_from_item,
        }
    }
}

// Rust guideline compliant 2026-02-21

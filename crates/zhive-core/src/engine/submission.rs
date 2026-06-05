//! Client → engine commands.
//!
//! [`Submission`] is the inbound side of the engine actor: every client
//! action (start a turn, queue an injection, resume a deferred
//! permission) lands here. The actor consumes the stream serially so
//! ordering inside a thread is deterministic; concurrency lives at the
//! cross-thread layer.
//!
//! ## Reply pattern
//!
//! Each submission is wrapped in a [`SubmissionEnvelope`] that carries
//! an optional [`tokio::sync::oneshot::Sender`]. The engine actor
//! always tries to discharge the sender exactly once with a typed
//! reply (see [`StartTurnReply`] / [`CancelTurnReply`] / etc.). When
//! the caller did not supply a reply channel the envelope is fire-and-
//! forget; subscribers can still observe outcomes via the broadcast
//! [`crate::engine::event::EngineEvent`] stream.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use zhive_proto::domain::{Item, ItemId, ThreadId, TurnId};
use zhive_proto::hook::{CompactTrigger, EnginePhase};
use zhive_proto::permission::{
    PermissionOutcome, PermissionScope, StreamingBehavior, SubagentDefinition,
};

/// Stable identifier for a pending `permission/request` reverse RPC.
///
/// Allocated by [`crate::permission`] when the engine emits a permission
/// prompt; the matching [`Submission::ResumePermission`] echoes the same
/// value to discharge the wait. Serialises as a JSON string on the wire
/// (e.g. `"perm:42"`) so the JSON-RPC envelope stays compact.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PermissionRequestId(pub Arc<str>);

/// Successful outcome of a `StartTurn` dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartTurnReply {
    /// Newly issued turn id.
    pub turn_id: TurnId,
}

/// Reasons a `StartTurn` submission failed inside the actor.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StartTurnError {
    /// Engine phase was not `Idle` at dispatch time.
    EngineBusy {
        /// Observed phase.
        current: EnginePhase,
    },
}

/// Outcome of a `CancelTurn` dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelTurnReply {
    /// The target thread had an active turn; it was cancelled.
    Cancelled {
        /// Id of the turn that was cancelled.
        turn_id: TurnId,
    },
    /// Target thread had no active turn; cancel was a no-op.
    NoActiveTurn,
}

/// Outcome of a `ResumePermission` dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResumePermissionReply {
    /// The pending request was resolved.
    Resolved,
    /// The request id was unknown to the reducer (stale or duplicate).
    UnknownRequest,
    /// The request id did not parse as `perm:<n>`.
    InvalidRequestId,
    /// The awaiter was dropped before the resume arrived.
    Abandoned,
}

/// Outcome of a `Compact` dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompactReply {
    /// Compaction ran; `entries_compacted` items were replaced by a summary.
    Compacted {
        /// Number of transcript items folded into the summary.
        entries_compacted: u32,
    },
    /// The target thread had no items to compact; nothing was done.
    NothingToCompact,
    /// Compaction entered the async summarize phase; the outcome arrives via
    /// the `compaction_completed` / `compaction_failed` events rather than
    /// this reply.
    Started,
}

/// Reasons a `Compact` submission failed inside the actor.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompactError {
    /// The target thread does not exist in the thread store.
    ThreadNotFound,
    /// Engine phase was not `Idle` at dispatch time; compaction requires an
    /// idle engine so it does not race a live turn mutating the transcript.
    EngineBusy {
        /// Observed phase.
        current: EnginePhase,
    },
    /// The summarisation provider call failed.
    SummarizationFailed {
        /// Human-readable provider error.
        message: String,
    },
    /// A `PreCompact` hook returned a blocking decision
    /// (`continue_loop = false` or `permission_decision = Deny`),
    /// aborting the compaction before it begins.
    BlockedByHook {
        /// Human-readable reason from the hook, if any.
        reason: Option<String>,
    },
}

impl fmt::Display for CompactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ThreadNotFound => f.write_str("thread not found"),
            Self::EngineBusy { current } => {
                write!(
                    f,
                    "engine busy (phase {current:?}); compaction requires Idle"
                )
            }
            Self::SummarizationFailed { message } => {
                write!(f, "summarization failed: {message}")
            }
            Self::BlockedByHook { reason } => match reason {
                Some(r) => write!(f, "compaction blocked by PreCompact hook: {r}"),
                None => f.write_str("compaction blocked by PreCompact hook"),
            },
        }
    }
}

impl std::error::Error for CompactError {}

/// Outcome of a `Fork` dispatch.
///
/// Forking creates a brand-new thread seeded with the source thread's
/// transcript (replayed from its JSONL rollout up to an optional boundary
/// item). The new thread records its origin via [`zhive_proto::domain::Thread::forked_from`]
/// and a `parent_session` rollout header, so it can be resumed and rebuilt
/// independently.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::ThreadId;
/// use zhive_core::engine::submission::ForkReply;
///
/// let reply = ForkReply::Forked {
///     new_thread_id: ThreadId(Arc::from("thread:native/fork/src/0")),
///     items_replayed: 3,
///     summarized: false,
/// };
/// // `ForkReply` is `#[non_exhaustive]`, so match (not an irrefutable `let`).
/// match reply {
///     ForkReply::Forked { items_replayed, summarized, .. } => {
///         assert_eq!(items_replayed, 3);
///         assert!(!summarized);
///     }
///     _ => unreachable!("only one variant today"),
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ForkReply {
    /// A new thread was created from the source history.
    Forked {
        /// Id of the freshly allocated forked thread.
        new_thread_id: ThreadId,
        /// Number of source items replayed into the new thread.
        items_replayed: u32,
        /// Whether a branch-summary item was generated and prepended.
        summarized: bool,
    },
}

/// Reasons a `Fork` submission failed inside the actor.
///
/// Hand-written `Display` + `Error` (matching [`CompactError`]) rather than a
/// `thiserror` derive, to keep the engine submission module's error style
/// uniform.
///
/// # Examples
///
/// ```
/// use zhive_core::engine::submission::ForkError;
///
/// let err = ForkError::SourceNotFound;
/// assert!(matches!(err, ForkError::SourceNotFound));
/// // Display renders a human-readable cause.
/// assert_eq!(
///     ForkError::ReplayFailed { message: "torn line".to_owned() }.to_string(),
///     "fork replay failed: torn line",
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ForkError {
    /// The source thread has no readable history: it is unknown to the thread
    /// store and no rollout exists for it (also returned when storage is not
    /// configured, since cross-thread fork reads the source rollout).
    SourceNotFound,
    /// Engine phase was not `Idle` at dispatch time; fork claims the
    /// `BranchSummary` phase and so cannot race a live turn or compaction.
    EngineBusy {
        /// Observed phase.
        current: EnginePhase,
    },
    /// Replaying the source rollout into the new thread failed (I/O or a
    /// corrupt rollout line).
    ReplayFailed {
        /// Human-readable cause.
        message: String,
    },
    /// The optional branch-summary provider call failed.
    SummarizationFailed {
        /// Human-readable provider error.
        message: String,
    },
}

impl fmt::Display for ForkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceNotFound => f.write_str("source thread not found (no history to fork)"),
            Self::EngineBusy { current } => {
                write!(f, "engine busy (phase {current:?}); fork requires Idle")
            }
            Self::ReplayFailed { message } => write!(f, "fork replay failed: {message}"),
            Self::SummarizationFailed { message } => {
                write!(f, "branch summary failed: {message}")
            }
        }
    }
}

impl std::error::Error for ForkError {}

/// Outcome of a [`Submission::ResumeThread`] dispatch.
///
/// Resume reads a persisted thread's full history from its JSONL rollout, makes
/// the thread resident in memory (so the next turn's prompt includes the prior
/// context), and reports how much history was restored.
///
/// `#[non_exhaustive]`, so it is produced only by the engine
/// ([`crate::engine::Engine::resume_thread`]); read its fields rather than
/// constructing it.
///
/// # Examples
///
/// ```no_run
/// use zhive_core::engine::Engine;
/// use zhive_proto::domain::ThreadId;
/// # async fn demo() {
/// let engine = Engine::spawn();
/// if let Ok(Ok(reply)) = engine
///     .resume_thread(ThreadId(std::sync::Arc::from("thread:native/01")))
///     .await
/// {
///     println!("restored {} items in {} turns", reply.items_restored, reply.turns_restored);
/// }
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResumeReply {
    /// Id of the resumed thread (echoes the request).
    pub thread_id: ThreadId,
    /// Number of history items restored into the in-memory transcript.
    pub items_restored: u32,
    /// Number of turns the restored items spanned.
    pub turns_restored: u32,
}

/// Reasons a [`Submission::ResumeThread`] dispatch failed inside the actor.
///
/// Hand-written `Display` + `Error` (matching [`ForkError`] / [`CompactError`])
/// to keep the engine submission module's error style uniform.
///
/// # Examples
///
/// ```
/// use zhive_core::engine::submission::ResumeError;
///
/// assert_eq!(
///     ResumeError::ThreadNotFound.to_string(),
///     "thread not found (no persisted history to resume)",
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResumeError {
    /// No persistent storage is configured, so there is no rollout to resume
    /// from (an in-memory engine cannot resume a historical thread).
    StorageUnavailable,
    /// The thread is not present in the persistent index.
    ThreadNotFound,
    /// Engine phase was not `Idle` at dispatch time; resume mutates the thread
    /// store and so cannot race a live turn or compaction.
    EngineBusy {
        /// Observed phase.
        current: EnginePhase,
    },
    /// Reading the thread's rollout failed (I/O or a corrupt rollout line).
    ReplayFailed {
        /// Human-readable cause.
        message: String,
    },
}

impl fmt::Display for ResumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StorageUnavailable => {
                f.write_str("no persistent storage configured; cannot resume a thread")
            }
            Self::ThreadNotFound => {
                f.write_str("thread not found (no persisted history to resume)")
            }
            Self::EngineBusy { current } => {
                write!(f, "engine busy (phase {current:?}); resume requires Idle")
            }
            Self::ReplayFailed { message } => write!(f, "resume replay failed: {message}"),
        }
    }
}

impl std::error::Error for ResumeError {}

/// Reasons a [`Submission::GetItems`] dispatch failed inside the actor.
///
/// # Examples
///
/// ```
/// use zhive_core::engine::submission::GetItemsError;
///
/// assert_eq!(
///     GetItemsError::StorageUnavailable.to_string(),
///     "no persistent storage configured; cannot read items",
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GetItemsError {
    /// No persistent storage is configured (an in-memory engine has no
    /// item index to read).
    StorageUnavailable,
    /// Reading the items from the index failed (I/O or a corrupt payload).
    ReadFailed {
        /// Human-readable cause.
        message: String,
    },
}

impl fmt::Display for GetItemsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StorageUnavailable => {
                f.write_str("no persistent storage configured; cannot read items")
            }
            Self::ReadFailed { message } => write!(f, "reading items failed: {message}"),
        }
    }
}

impl std::error::Error for GetItemsError {}

/// One inbound command for the engine actor.
///
/// `Clone` is intentionally NOT derived: a submission can carry a
/// `oneshot::Sender` (via [`SubmissionEnvelope`]) which is single-shot
/// by construction. Callers that need to fan a payload out should
/// construct multiple envelopes.
#[derive(Debug)]
#[non_exhaustive]
pub enum Submission {
    /// Start a new turn on an existing or freshly-allocated thread.
    StartTurn {
        /// Thread the new turn belongs to.
        thread_id: ThreadId,
        /// User-supplied input items (typically a single
        /// [`Item::UserMessage`]).
        user_input: Vec<Item>,
        /// Optional explicit scope; `None` inherits the thread scope.
        scope: Option<PermissionScope>,
        /// Requested reasoning depth; `None` leaves the provider default.
        reasoning: Option<zhive_proto::domain::ThinkingEffort>,
    },
    /// Cancel the active turn on the given thread.
    CancelTurn {
        /// Target thread; missing or already-idle threads no-op.
        thread_id: ThreadId,
    },
    /// Append items into the steer or follow-up queue.
    EnqueueInjection {
        /// Target thread (must be in turn).
        thread_id: ThreadId,
        /// Which queue receives the items.
        behavior: StreamingBehavior,
        /// Ordered items to splice in.
        items: Vec<Item>,
    },
    /// Append items into the next-turn queue (preserved across aborts).
    EnqueueNextTurn {
        /// Target thread.
        thread_id: ThreadId,
        /// Ordered items to splice in.
        items: Vec<Item>,
    },
    /// Resolve a deferred or asked permission with the user's choice.
    ResumePermission {
        /// Echoes the request id emitted in the original prompt.
        request_id: PermissionRequestId,
        /// User decision.
        outcome: PermissionOutcome,
    },
    /// Spawn a subagent thread under the given parent.
    SpawnSubagent {
        /// Parent thread to inherit from.
        parent_thread_id: ThreadId,
        /// Subagent declaration.
        definition: SubagentDefinition,
    },
    /// Compact the transcript history of a thread into a summary.
    Compact {
        /// Thread whose transcript is compacted.
        thread_id: ThreadId,
        /// Why compaction fires (`Manual` for `/compact`, `Auto` for the
        /// engine-initiated token/length threshold).
        trigger: CompactTrigger,
    },
    /// Fork a new thread from a source thread's history at an optional point.
    ///
    /// Reads the source thread's rollout (the source of truth, including
    /// history outside the in-memory window), allocates a fresh thread id, and
    /// replays the source items into the new thread up to `up_to_item`
    /// (inclusive) or in full when `None`. Records the fork origin so the new
    /// thread can be resumed and rebuilt on its own.
    Fork {
        /// Thread whose history seeds the new thread.
        source_thread_id: ThreadId,
        /// Inclusive truncation point; `None` replays the full history.
        up_to_item: Option<ItemId>,
        /// When `true`, generate an LLM branch summary and prepend it as the
        /// new thread's opening context.
        summarize: bool,
    },
    /// List persisted threads (most-recently-updated first).
    ///
    /// Reads the queryable state-database projection; returns an empty list
    /// when no persistent storage is configured. When `cwd` is `Some(path)`,
    /// only threads created under that working directory are returned
    /// (codex-style per-project listing); `None` lists every thread.
    ListThreads {
        /// Optional working-directory filter.
        cwd: Option<String>,
    },
    /// Resume a persisted thread, making its history resident in memory.
    ///
    /// Reads the thread's full rollout (the source of truth, including history
    /// outside any prior in-memory window), seeds the in-memory transcript so a
    /// subsequent [`Submission::StartTurn`] includes the prior context, and
    /// leaves the thread `Idle`.
    ResumeThread {
        /// Thread to resume from persistent storage.
        thread_id: ThreadId,
    },
    /// Read persisted history items for a thread, for rendering a resumed
    /// conversation.
    ///
    /// When `turn_id` is `Some`, the items of that single turn are returned
    /// (paged by `offset` / `limit` when both are present). When `turn_id` is
    /// `None`, the thread's full item history is returned in conversation order.
    GetItems {
        /// Thread whose items are read.
        thread_id: ThreadId,
        /// Optional single turn to scope the read to.
        turn_id: Option<TurnId>,
        /// Optional page offset (only applied with a `turn_id`).
        offset: Option<i64>,
        /// Optional page limit (only applied with a `turn_id`).
        limit: Option<i64>,
    },
    /// Gracefully stop the engine actor.
    Shutdown,
    /// Delete a thread, its rollout, and all SQL-indexed data.
    ///
    /// Refused when the thread has an active turn in progress to prevent
    /// deleting a thread while items are still being appended to it.
    Delete {
        /// Thread to delete.
        thread_id: ThreadId,
    },
    /// Rename a persisted thread's human-facing display name.
    ///
    /// An empty name clears the stored name (reverts to `NULL` in the index).
    /// The rename is queued asynchronously to the persistence writer; the reply
    /// is acknowledged immediately rather than waiting for the write to land.
    Rename {
        /// Thread to rename.
        thread_id: ThreadId,
        /// New display name; empty string clears it.
        name: String,
    },
    /// Search threads by a substring query against name, preview, and cwd.
    ///
    /// Returns an empty list when no storage is configured or when no threads
    /// match. Does not error on an in-memory engine.
    Search {
        /// Case-insensitive substring to match.
        query: String,
        /// Optional cwd pre-filter applied before the substring search.
        cwd: Option<String>,
    },
    /// List all tools known to this engine's registry.
    ///
    /// Returns a `Vec<proto::rpc::ToolSpec>` sorted by name. The list is
    /// derived from the in-memory registry and is always available regardless
    /// of storage configuration.
    ListTools,
}

/// Reasons a [`Submission::SpawnSubagent`] dispatch failed.
///
/// Distinct from [`StartTurnError`] because spawning a subagent has
/// additional preconditions (recursion ban, scope narrowing) that do not
/// apply to ordinary turn starts.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SubagentSpawnError {
    /// The parent thread was not found in the thread store.
    ParentNotFound,
    /// The parent thread is itself a subagent; recursion is forbidden.
    RecursionForbidden,
    /// The subagent definition requested `allow_subagent_spawn = true`.
    ChildSpawnRequested,
    /// The proposed child scope is wider than the parent scope.
    ScopeWideningRejected,
}

impl fmt::Display for SubagentSpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParentNotFound => f.write_str("parent thread not found"),
            Self::RecursionForbidden => {
                f.write_str("subagent recursion forbidden (parent is already a subagent)")
            }
            Self::ChildSpawnRequested => f.write_str(
                "subagent definition requested allow_subagent_spawn=true; recursion forbidden",
            ),
            Self::ScopeWideningRejected => {
                f.write_str("child scope widens the parent scope; narrowing required")
            }
        }
    }
}

impl std::error::Error for SubagentSpawnError {}

/// Successful outcome of a [`Submission::Delete`] dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteReply {
    /// `true` when the thread row and/or rollout existed and was removed;
    /// `false` when the thread was unknown (idempotent delete).
    pub deleted: bool,
}

/// Reasons a [`Submission::Delete`] dispatch failed inside the actor.
///
/// # Examples
///
/// ```
/// use zhive_core::engine::submission::DeleteError;
///
/// assert_eq!(
///     DeleteError::ThreadHasActiveTurn.to_string(),
///     "cannot delete a thread with an active turn in progress",
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeleteError {
    /// No persistent storage is configured; delete is a no-op on in-memory
    /// engines.
    StorageUnavailable,
    /// The thread has an active turn; delete is refused to avoid corrupting the
    /// in-flight append.
    ThreadHasActiveTurn,
    /// The underlying storage operation failed.
    DeleteFailed {
        /// Human-readable cause.
        message: String,
    },
}

impl fmt::Display for DeleteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StorageUnavailable => {
                f.write_str("no persistent storage configured; cannot delete thread")
            }
            Self::ThreadHasActiveTurn => {
                f.write_str("cannot delete a thread with an active turn in progress")
            }
            Self::DeleteFailed { message } => write!(f, "thread deletion failed: {message}"),
        }
    }
}

impl std::error::Error for DeleteError {}

/// Successful outcome of a [`Submission::Rename`] dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameReply {
    /// `true` when the rename was accepted (queued for the persistence writer);
    /// `false` when storage was unavailable.
    pub renamed: bool,
}

/// Reasons a [`Submission::Rename`] dispatch failed inside the actor.
///
/// # Examples
///
/// ```
/// use zhive_core::engine::submission::RenameError;
///
/// assert_eq!(
///     RenameError::StorageUnavailable.to_string(),
///     "no persistent storage configured; cannot rename thread",
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RenameError {
    /// No persistent storage is configured.
    StorageUnavailable,
    /// The underlying storage operation failed.
    RenameFailed {
        /// Human-readable cause.
        message: String,
    },
}

impl fmt::Display for RenameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StorageUnavailable => {
                f.write_str("no persistent storage configured; cannot rename thread")
            }
            Self::RenameFailed { message } => write!(f, "thread rename failed: {message}"),
        }
    }
}

impl std::error::Error for RenameError {}

/// Typed reply discharged on a [`SubmissionEnvelope::reply`] sender.
///
/// One variant per submission kind that has a synchronous reply. A
/// fire-and-forget envelope (no reply channel attached) never produces
/// a `SubmissionReply`.
///
/// Only [`PartialEq`] is derived (not [`Eq`]): the [`SubmissionReply::ListThreads`]
/// payload carries [`zhive_proto::domain::Thread`], which is `PartialEq` but not
/// `Eq` (its `cwd` / metadata make a total-equality contract undesirable).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum SubmissionReply {
    /// Reply to a [`Submission::StartTurn`].
    StartTurn(Result<StartTurnReply, StartTurnError>),
    /// Reply to a [`Submission::CancelTurn`].
    CancelTurn(CancelTurnReply),
    /// Reply to a [`Submission::ResumePermission`].
    ResumePermission(ResumePermissionReply),
    /// Reply to a [`Submission::SpawnSubagent`].
    SpawnSubagent(Result<ThreadId, SubagentSpawnError>),
    /// Reply to a [`Submission::Compact`].
    Compact(Result<CompactReply, CompactError>),
    /// Reply to a [`Submission::Fork`].
    Fork(Result<ForkReply, ForkError>),
    /// Reply to a [`Submission::ListThreads`].
    ///
    /// `Vec<Thread>` is boxed so the enum stays small even though a thread list
    /// can be large.
    ListThreads(Box<Vec<zhive_proto::domain::Thread>>),
    /// Reply to a [`Submission::ResumeThread`].
    ResumeThread(Result<ResumeReply, ResumeError>),
    /// Reply to a [`Submission::GetItems`].
    ///
    /// The item list is boxed so the enum stays small even for a large turn.
    GetItems(Result<Box<Vec<Item>>, GetItemsError>),
    /// Reply to a [`Submission::Shutdown`].
    Shutdown,
    /// Reply to a [`Submission::Delete`].
    Delete(Result<DeleteReply, DeleteError>),
    /// Reply to a [`Submission::Rename`].
    Rename(Result<RenameReply, RenameError>),
    /// Reply to a [`Submission::Search`].
    ///
    /// `Vec<Thread>` is boxed so the enum stays small even for a large list.
    Search(Box<Vec<zhive_proto::domain::Thread>>),
    /// Reply to a [`Submission::ListTools`].
    ///
    /// `Vec<ToolSpec>` is boxed so the enum stays small.
    ListTools(Box<Vec<zhive_proto::rpc::ToolSpec>>),
}

/// Wraps a [`Submission`] with an optional reply oneshot.
#[derive(Debug)]
pub struct SubmissionEnvelope {
    /// The command itself.
    pub submission: Submission,
    /// When `Some`, the actor sends a typed [`SubmissionReply`] on
    /// completion; when `None`, the submission is fire-and-forget.
    pub reply: Option<oneshot::Sender<SubmissionReply>>,
}

impl SubmissionEnvelope {
    /// Builds a fire-and-forget envelope.
    #[must_use]
    pub fn fire_and_forget(submission: Submission) -> Self {
        Self {
            submission,
            reply: None,
        }
    }

    /// Builds an envelope plus the matching receiver.
    #[must_use]
    pub fn with_reply(submission: Submission) -> (Self, oneshot::Receiver<SubmissionReply>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                submission,
                reply: Some(tx),
            },
            rx,
        )
    }
}

// Rust guideline compliant 2026-02-21

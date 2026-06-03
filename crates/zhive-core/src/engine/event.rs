//! Engine → subscriber events.
//!
//! Fanned out over a [`tokio::sync::broadcast`] channel: every connected
//! client (and the bridge crates) subscribes and re-emits whatever it
//! cares about. The events are not request/response — discrete RPC
//! roundtrips ride on the [`crate::engine::submission`] path instead.

use std::sync::Arc;

use zhive_proto::domain::{Item, ItemId, ThreadId, TurnError, TurnId};
use zhive_proto::hook::EnginePhase;
use zhive_proto::permission::{RequestPermissionRequest, SessionAbortedNotification};

use super::submission::PermissionRequestId;

/// Reasons a [`crate::engine::submission::Submission::StartTurn`] submission
/// can be refused before a [`TurnId`] is allocated.
///
/// Emitted as [`EngineEvent::TurnRejected`] so subscribed clients can
/// surface a meaningful diagnostic instead of timing out waiting for
/// `TurnStarted`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnRejectionReason {
    /// The engine is already in a non-`Idle` phase (another turn,
    /// compaction, branch summary, retry, …). The engine refuses to
    /// queue further turns to keep the global phase machine consistent
    /// with the Pi / codex single-active-turn invariant.
    EngineBusy {
        /// Phase the engine was in when the submission arrived.
        current: EnginePhase,
    },
}

/// One outbound event surfaced by the engine.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum EngineEvent {
    /// A turn entered [`zhive_proto::domain::TurnStatus::InProgress`].
    TurnStarted {
        /// Owning thread.
        thread_id: ThreadId,
        /// Newly issued turn id.
        turn_id: TurnId,
    },
    /// A [`crate::engine::submission::Submission::StartTurn`] was refused
    /// before a [`TurnId`] could be allocated. The engine attaches a
    /// machine-readable reason so subscribers do not have to guess why
    /// they will not see a matching `TurnStarted`.
    TurnRejected {
        /// Thread the submission targeted.
        thread_id: ThreadId,
        /// Why the engine refused to start the turn.
        reason: TurnRejectionReason,
    },
    /// An item was appended to the active turn.
    ItemAppended {
        /// Owning thread.
        thread_id: ThreadId,
        /// Active turn id.
        turn_id: TurnId,
        /// The new item (boxed because [`Item`] is large).
        item: Box<Item>,
    },
    /// A streamed text fragment for the active turn's agent message.
    ///
    /// Emitted once per provider `TextDelta` so clients can render output
    /// token-by-token. The block still finalises as a single
    /// [`EngineEvent::ItemAppended`] carrying the complete
    /// [`Item::AgentMessage`], so a client that ignores deltas loses nothing.
    ItemDelta {
        /// Owning thread.
        thread_id: ThreadId,
        /// Active turn id.
        turn_id: TurnId,
        /// The incremental text fragment.
        delta: String,
    },
    /// A turn reached
    /// [`zhive_proto::domain::TurnStatus::Completed`].
    TurnCompleted {
        /// Owning thread.
        thread_id: ThreadId,
        /// Completed turn id.
        turn_id: TurnId,
    },
    /// A turn failed.
    TurnFailed {
        /// Owning thread.
        thread_id: ThreadId,
        /// Failed turn id.
        turn_id: TurnId,
        /// Failure details.
        error: TurnError,
    },
    /// A session was aborted; payload is the wire notification.
    SessionAborted(Box<SessionAbortedNotification>),
    /// Engine phase changed.
    ///
    /// `thread_id` is `Some` for transitions driven by a specific
    /// thread (turn start / completion / cancel) and `None` for
    /// engine-global transitions (compaction, branch summary).
    PhaseChanged {
        /// Optional thread the transition belongs to.
        thread_id: Option<ThreadId>,
        /// Phase the engine just left.
        from: EnginePhase,
        /// Phase the engine just entered.
        to: EnginePhase,
    },
    /// Engine emitted a permission prompt; clients must respond via
    /// [`crate::engine::submission::Submission::ResumePermission`].
    PermissionRequested {
        /// Stable id used to discharge the wait.
        request_id: PermissionRequestId,
        /// Wire payload for the reverse RPC.
        request: Box<RequestPermissionRequest>,
    },
    /// Token usage reported by the provider at the end of a turn iteration.
    ///
    /// Emitted once per provider stream call (i.e. once per tool-call loop
    /// iteration that produced usage data). A multi-step (tool-using) turn
    /// therefore emits one `Usage` per LLM call. The values are sourced
    /// directly from [`llmsdk::language_model::Usage`]; a zero value
    /// indicates the provider reported no usage (e.g. a scripted test model).
    ///
    /// Clients that need per-turn token accounting should sum all `Usage`
    /// events for the same `turn_id`.
    Usage {
        /// Owning thread.
        thread_id: ThreadId,
        /// Active turn id.
        turn_id: TurnId,
        /// Total input tokens consumed by this provider call.
        input_tokens: u64,
        /// Total output tokens produced by this provider call.
        output_tokens: u64,
    },

    /// A persistence save point was reached and the thread's deferred
    /// session writes were flushed.
    ///
    /// Emitted by [`crate::engine::lifecycle`] when the engine reaches a
    /// consistent state (turn end, or just before falling back to `Idle`) and
    /// drains the thread's [`crate::state::PendingSessionWrites`] buffer to the
    /// persistence writer.  `had_pending_mutations` is `true` when the buffer
    /// held at least one deferred write at flush time, so subscribers can tell
    /// a save point that actually persisted work apart from a no-op one.
    SavePoint {
        /// Thread whose buffer was flushed.
        thread_id: ThreadId,
        /// Whether the buffer held deferred writes when flushed.
        had_pending_mutations: bool,
    },

    /// A subagent child turn has just started.
    ///
    /// Emitted once per spawned subagent, immediately after the child turn is
    /// installed, so external observers can route the child thread's subsequent
    /// `ItemAppended` / `TurnStarted` events (which carry the child thread id)
    /// back to the parent. The `agent_type` and `description` are mirrored from
    /// the spawning [`zhive_proto::permission::SubagentDefinition`] when present.
    SubagentStarted {
        /// Thread id of the parent that spawned the subagent.
        parent_thread_id: ThreadId,
        /// Thread id of the newly spawned subagent child.
        child_thread_id: ThreadId,
        /// The subagent's declared name / type, if any.
        agent_type: Option<String>,
        /// The subagent's declared description, if any.
        description: Option<String>,
    },

    /// A subagent turn finished (completed or failed).
    ///
    /// External observers see exactly one `SubagentCompleted` per spawned
    /// subagent. Intermediate items produced by the child turn are scoped
    /// to the child thread and do not appear here.
    ///
    /// `final_message` is `None` when the child transcript contained no
    /// [`Item::AgentMessage`] or [`Item::SystemNotice`], or when the child
    /// turn failed — both are treated the same way by external subscribers
    /// because the precise failure detail is already on [`EngineEvent::TurnFailed`]
    /// for the child thread.
    SubagentCompleted {
        /// Thread id of the parent that spawned the subagent.
        parent_thread_id: ThreadId,
        /// Thread id of the subagent that just finished.
        child_thread_id: ThreadId,
        /// The single item delivered back to the parent, or `None`.
        final_message: Option<Arc<Item>>,
    },

    /// A turn parked on a deferred permission request (suspended).
    ///
    /// Emitted when a tool call folds to
    /// [`zhive_proto::permission::PermissionDecision::Defer`] and the engine
    /// suspends the turn until a matching `session/resume_permission` arrives.
    /// `request_id` is the globally unique pending id the client echoes back to
    /// resume. The server maps this to an `events/turn_suspended` notification.
    TurnSuspended {
        /// Owning thread.
        thread_id: ThreadId,
        /// Suspended turn.
        turn_id: TurnId,
        /// Pending permission request id the client passes back to resume.
        request_id: PermissionRequestId,
        /// Optional rationale mirrored from the deferring hook, if any.
        reason: Option<String>,
    },

    /// A previously suspended turn was unblocked and resumed.
    ///
    /// Dual of [`EngineEvent::TurnSuspended`]: emitted once the deferred
    /// permission request is resolved (allow / deny / cancel) and the turn
    /// continues. The server maps this to an `events/turn_resumed`
    /// notification.
    TurnResumed {
        /// Owning thread.
        thread_id: ThreadId,
        /// Resumed turn.
        turn_id: TurnId,
    },

    /// A new thread was forked from a source thread's history.
    ///
    /// Emitted once the fork path has replayed the source transcript into the
    /// new thread and registered it. `forked_from_item` echoes the inclusive
    /// truncation point requested by the caller (`None` = full history). The
    /// server maps this to an `events/thread_forked` notification so UIs can
    /// open the new branch.
    ThreadForked {
        /// Thread whose history seeded the fork.
        source_thread_id: ThreadId,
        /// Newly allocated forked thread.
        new_thread_id: ThreadId,
        /// Inclusive item the fork was taken at, or `None` for full history.
        forked_from_item: Option<ItemId>,
    },
}

// Rust guideline compliant 2026-02-21

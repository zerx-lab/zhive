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

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use zhive_proto::domain::{Item, ThreadId, TurnId};
use zhive_proto::hook::EnginePhase;
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
    /// Gracefully stop the engine actor.
    Shutdown,
}

/// Typed reply discharged on a [`SubmissionEnvelope::reply`] sender.
///
/// One variant per submission kind that has a synchronous reply. A
/// fire-and-forget envelope (no reply channel attached) never produces
/// a `SubmissionReply`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SubmissionReply {
    /// Reply to a [`Submission::StartTurn`].
    StartTurn(Result<StartTurnReply, StartTurnError>),
    /// Reply to a [`Submission::CancelTurn`].
    CancelTurn(CancelTurnReply),
    /// Reply to a [`Submission::ResumePermission`].
    ResumePermission(ResumePermissionReply),
    /// Reply to a [`Submission::Shutdown`].
    Shutdown,
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

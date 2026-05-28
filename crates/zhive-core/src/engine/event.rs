//! Engine → subscriber events.
//!
//! Fanned out over a [`tokio::sync::broadcast`] channel: every connected
//! client (and the bridge crates) subscribes and re-emits whatever it
//! cares about. The events are not request/response — discrete RPC
//! roundtrips ride on the [`crate::engine::submission`] path instead.

use zhive_proto::domain::{Item, ThreadId, TurnError, TurnId};
use zhive_proto::hook::EnginePhase;
use zhive_proto::permission::{RequestPermissionRequest, SessionAbortedNotification};

use super::submission::PermissionRequestId;

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
    /// An item was appended to the active turn.
    ItemAppended {
        /// Owning thread.
        thread_id: ThreadId,
        /// Active turn id.
        turn_id: TurnId,
        /// The new item (boxed because [`Item`] is large).
        item: Box<Item>,
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
    /// Engine phase changed; thread is `None` for global transitions.
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
}

// Rust guideline compliant 2026-02-21

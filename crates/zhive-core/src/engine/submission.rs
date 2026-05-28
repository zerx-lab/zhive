//! Client → engine commands.
//!
//! [`Submission`] is the inbound side of the engine actor: every client
//! action (start a turn, queue an injection, resume a deferred
//! permission) lands here. The actor consumes the stream serially so
//! ordering inside a thread is deterministic; concurrency lives at the
//! cross-thread layer.

use std::sync::Arc;

use zhive_proto::domain::{Item, ThreadId};
use zhive_proto::permission::{
    PermissionOutcome, PermissionScope, StreamingBehavior, SubagentDefinition,
};

/// Stable identifier for a pending `permission/request` reverse RPC.
///
/// Allocated by [`crate::permission`] when the engine emits a permission
/// prompt; the matching [`Submission::ResumePermission`] echoes the same
/// value to discharge the wait.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PermissionRequestId(pub Arc<str>);

/// One inbound command for the engine actor.
#[derive(Debug, Clone)]
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

// Rust guideline compliant 2026-02-21

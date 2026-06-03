//! RPC method name and event name constants (A3 — contract freeze).
//!
//! This module is the single authoritative directory of every JSON-RPC
//! method string used by the zhive wire protocol. Client SDK generators,
//! server handlers, and event forwarders should import from here instead
//! of using inline string literals.
//!
//! # Organisation
//!
//! Constants are grouped into four sections:
//!
//! * **`engine/*`** — turn lifecycle, compaction, resume, and shutdown RPCs.
//! * **`thread/*`** — history surface: fork, list, item fetch.
//! * **`session/*`** — injection queues, cancel, resume-permission, and
//!   lifecycle control; `events/subscribe` / `events/unsubscribe` are also
//!   included here as they manage the session's event stream.
//! * **`events/*`** — server-to-client event notifications.
//!
//! The three constants that were originally defined in
//! [`crate::permission`] are re-exported here so this module can serve as
//! the sole import point, while the original definitions remain in place
//! (existing imports in `zhive-core` continue to compile unchanged).
//!
//! # Examples
//!
//! ```
//! use zhive_proto::methods;
//! assert_eq!(methods::METHOD_START_TURN, "engine/start_turn");
//! assert_eq!(methods::EVENT_TURN_STARTED, "events/turn_started");
//! assert_eq!(methods::EVENT_TURN_SUSPENDED, "events/turn_suspended");
//! ```

// ============================================================
// engine/* — turn lifecycle, compaction, resume, shutdown
// ============================================================

/// Client-to-server: start a new turn on a thread.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_START_TURN;
/// assert_eq!(METHOD_START_TURN, "engine/start_turn");
/// ```
pub const METHOD_START_TURN: &str = "engine/start_turn";

/// Client-to-server: cancel the active turn on a thread.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_CANCEL_TURN;
/// assert_eq!(METHOD_CANCEL_TURN, "engine/cancel_turn");
/// ```
pub const METHOD_CANCEL_TURN: &str = "engine/cancel_turn";

/// Legacy alias for `session/resume_permission` (kept for backward compat).
///
/// Both routes map to the same handler. New clients should use the
/// canonical [`METHOD_RESUME_PERMISSION`] re-export instead.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_RESUME_PERMISSION_LEGACY;
/// assert_eq!(METHOD_RESUME_PERMISSION_LEGACY, "engine/resume_permission");
/// ```
pub const METHOD_RESUME_PERMISSION_LEGACY: &str = "engine/resume_permission";

/// Client-to-server: compact transcript history on a thread.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_COMPACT;
/// assert_eq!(METHOD_COMPACT, "engine/compact");
/// ```
pub const METHOD_COMPACT: &str = "engine/compact";

/// Client-to-server: restore a persisted thread into engine memory.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_RESUME_THREAD;
/// assert_eq!(METHOD_RESUME_THREAD, "engine/resume_thread");
/// ```
pub const METHOD_RESUME_THREAD: &str = "engine/resume_thread";

/// Client-to-server: request a graceful engine shutdown.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_SHUTDOWN;
/// assert_eq!(METHOD_SHUTDOWN, "engine/shutdown");
/// ```
pub const METHOD_SHUTDOWN: &str = "engine/shutdown";

// ============================================================
// thread/* — history surface
// ============================================================

/// Client-to-server: fork an existing thread at an optional item boundary.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_THREAD_FORK;
/// assert_eq!(METHOD_THREAD_FORK, "thread/fork");
/// ```
pub const METHOD_THREAD_FORK: &str = "thread/fork";

/// Client-to-server: list persisted threads, optionally filtered by cwd.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_THREAD_LIST;
/// assert_eq!(METHOD_THREAD_LIST, "thread/list");
/// ```
pub const METHOD_THREAD_LIST: &str = "thread/list";

/// Client-to-server: fetch history items for a thread or specific turn.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_THREAD_GET_ITEMS;
/// assert_eq!(METHOD_THREAD_GET_ITEMS, "thread/get_items");
/// ```
pub const METHOD_THREAD_GET_ITEMS: &str = "thread/get_items";

// ============================================================
// session/* — injection queues, cancel, and permission resume
// ============================================================

/// Client-to-server: enqueue a steer item into the active turn.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_ENQUEUE_STEER;
/// assert_eq!(METHOD_ENQUEUE_STEER, "session/enqueue_steer");
/// ```
pub const METHOD_ENQUEUE_STEER: &str = "session/enqueue_steer";

/// Client-to-server: enqueue a follow-up item into the active turn.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_ENQUEUE_FOLLOW_UP;
/// assert_eq!(METHOD_ENQUEUE_FOLLOW_UP, "session/enqueue_follow_up");
/// ```
pub const METHOD_ENQUEUE_FOLLOW_UP: &str = "session/enqueue_follow_up";

/// Client-to-server: enqueue an item for the next turn.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_ENQUEUE_NEXT_TURN;
/// assert_eq!(METHOD_ENQUEUE_NEXT_TURN, "session/enqueue_next_turn");
/// ```
pub const METHOD_ENQUEUE_NEXT_TURN: &str = "session/enqueue_next_turn";

/// Client-to-server: cancel the session / active turn on a thread.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_SESSION_CANCEL;
/// assert_eq!(METHOD_SESSION_CANCEL, "session/cancel");
/// ```
pub const METHOD_SESSION_CANCEL: &str = "session/cancel";

/// Client-to-server: subscribe to a specific set of event method names.
///
/// An empty methods list resets the connection back to allow-all.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_EVENTS_SUBSCRIBE;
/// assert_eq!(METHOD_EVENTS_SUBSCRIBE, "events/subscribe");
/// ```
pub const METHOD_EVENTS_SUBSCRIBE: &str = "events/subscribe";

/// Client-to-server: unsubscribe, resetting the filter to allow-all.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_EVENTS_UNSUBSCRIBE;
/// assert_eq!(METHOD_EVENTS_UNSUBSCRIBE, "events/unsubscribe");
/// ```
pub const METHOD_EVENTS_UNSUBSCRIBE: &str = "events/unsubscribe";

// ============================================================
// initialize handshake
// ============================================================

/// Client-to-server handshake request on connection open.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_INITIALIZE;
/// assert_eq!(METHOD_INITIALIZE, "initialize");
/// ```
pub const METHOD_INITIALIZE: &str = "initialize";

/// Server-to-client notification confirming the handshake was accepted.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_INITIALIZED;
/// assert_eq!(METHOD_INITIALIZED, "initialized");
/// ```
pub const METHOD_INITIALIZED: &str = "initialized";

// ============================================================
// Re-exports from permission.rs (canonical definitions stay in place)
// ============================================================

/// Canonical name for `session/resume_permission` (re-exported from [`crate::permission`]).
///
/// New code should use this constant rather than the legacy
/// `engine/resume_permission` alias ([`METHOD_RESUME_PERMISSION_LEGACY`]).
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::METHOD_RESUME_PERMISSION;
/// assert_eq!(METHOD_RESUME_PERMISSION, "session/resume_permission");
/// ```
#[doc(inline)]
pub use crate::permission::METHOD_RESUME_PERMISSION;

// ============================================================
// events/* — server-to-client notifications
// ============================================================

/// Server notification: a new turn has started on a thread.
///
/// Body: [`crate::events::TurnStartedPayload`].
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_TURN_STARTED;
/// assert_eq!(EVENT_TURN_STARTED, "events/turn_started");
/// ```
pub const EVENT_TURN_STARTED: &str = "events/turn_started";

/// Server notification: the engine rejected a turn start request.
///
/// Body: [`crate::events::TurnRejectedPayload`].
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_TURN_REJECTED;
/// assert_eq!(EVENT_TURN_REJECTED, "events/turn_rejected");
/// ```
pub const EVENT_TURN_REJECTED: &str = "events/turn_rejected";

/// Server notification: a turn has completed successfully.
///
/// Body: [`crate::events::TurnCompletedPayload`].
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_TURN_COMPLETED;
/// assert_eq!(EVENT_TURN_COMPLETED, "events/turn_completed");
/// ```
pub const EVENT_TURN_COMPLETED: &str = "events/turn_completed";

/// Server notification: a turn has failed with an error.
///
/// Body: [`crate::events::TurnFailedPayload`].
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_TURN_FAILED;
/// assert_eq!(EVENT_TURN_FAILED, "events/turn_failed");
/// ```
pub const EVENT_TURN_FAILED: &str = "events/turn_failed";

/// Server notification: a new item was appended to a turn.
///
/// Body: [`crate::events::ItemAppendedPayload`].
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_ITEM_APPENDED;
/// assert_eq!(EVENT_ITEM_APPENDED, "events/item_appended");
/// ```
pub const EVENT_ITEM_APPENDED: &str = "events/item_appended";

/// Server notification: a streaming text delta was produced.
///
/// Body: [`crate::events::ItemDeltaPayload`].
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_ITEM_DELTA;
/// assert_eq!(EVENT_ITEM_DELTA, "events/item_delta");
/// ```
pub const EVENT_ITEM_DELTA: &str = "events/item_delta";

/// Server notification: the engine moved between phases.
///
/// Body: [`crate::events::PhaseChangedPayload`].
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_PHASE_CHANGED;
/// assert_eq!(EVENT_PHASE_CHANGED, "events/phase_changed");
/// ```
pub const EVENT_PHASE_CHANGED: &str = "events/phase_changed";

/// Server notification: a session was aborted and injection queues drained.
///
/// Body: [`crate::permission::SessionAbortedNotification`].
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_SESSION_ABORTED;
/// assert_eq!(EVENT_SESSION_ABORTED, "events/session_aborted");
/// ```
pub const EVENT_SESSION_ABORTED: &str = "events/session_aborted";

/// Server notification: a permission reverse-RPC request is in flight.
///
/// Body: [`crate::events::PermissionRequestedPayload`].
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_PERMISSION_REQUESTED;
/// assert_eq!(EVENT_PERMISSION_REQUESTED, "events/permission_requested");
/// ```
pub const EVENT_PERMISSION_REQUESTED: &str = "events/permission_requested";

/// Server notification: LLM token usage for the most recent provider call.
///
/// Body: [`crate::events::UsagePayload`].
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_USAGE;
/// assert_eq!(EVENT_USAGE, "events/usage");
/// ```
pub const EVENT_USAGE: &str = "events/usage";

/// Server notification: a thread was forked from a source thread.
///
/// Body: [`crate::events::ThreadForkedPayload`].
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_THREAD_FORKED;
/// assert_eq!(EVENT_THREAD_FORKED, "events/thread_forked");
/// ```
pub const EVENT_THREAD_FORKED: &str = "events/thread_forked";

/// Server notification: a subagent child thread has started.
///
/// Body: [`crate::events::SubagentStartedPayload`].
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_SUBAGENT_STARTED;
/// assert_eq!(EVENT_SUBAGENT_STARTED, "events/subagent_started");
/// ```
pub const EVENT_SUBAGENT_STARTED: &str = "events/subagent_started";

/// Server notification: a subagent child thread has completed.
///
/// Body: [`crate::events::SubagentCompletedPayload`].
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_SUBAGENT_COMPLETED;
/// assert_eq!(EVENT_SUBAGENT_COMPLETED, "events/subagent_completed");
/// ```
pub const EVENT_SUBAGENT_COMPLETED: &str = "events/subagent_completed";

/// Server notification: a turn was suspended on a deferred permission request.
///
/// Body: [`crate::permission::TurnSuspendedNotification`].
/// Re-exported from [`crate::permission`]; the `events/` prefix alias
/// is provided here for uniform directory access.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_TURN_SUSPENDED;
/// assert_eq!(EVENT_TURN_SUSPENDED, "events/turn_suspended");
/// ```
pub const EVENT_TURN_SUSPENDED: &str = crate::permission::METHOD_TURN_SUSPENDED;

/// Server notification: a previously suspended turn was resumed.
///
/// Body: [`crate::permission::TurnResumedNotification`].
/// Re-exported from [`crate::permission`]; the `events/` prefix alias
/// is provided here for uniform directory access.
///
/// # Examples
///
/// ```
/// use zhive_proto::methods::EVENT_TURN_RESUMED;
/// assert_eq!(EVENT_TURN_RESUMED, "events/turn_resumed");
/// ```
pub const EVENT_TURN_RESUMED: &str = crate::permission::METHOD_TURN_RESUMED;

/// Re-export of [`crate::permission::METHOD_TURN_SUSPENDED`] for path-uniform access.
///
/// Identical value to [`EVENT_TURN_SUSPENDED`]; provided so callers can use
/// either name consistently.
#[doc(inline)]
pub use crate::permission::METHOD_TURN_SUSPENDED;

/// Re-export of [`crate::permission::METHOD_TURN_RESUMED`] for path-uniform access.
///
/// Identical value to [`EVENT_TURN_RESUMED`]; provided so callers can use
/// either name consistently.
#[doc(inline)]
pub use crate::permission::METHOD_TURN_RESUMED;

// Rust guideline compliant 2026-02-21

//! Per-thread event stream, a second broadcast layer beside `EngineEvent`.
//!
//! [`ThreadEvent`] is scoped to a single thread and fanned out on a per-handle
//! [`tokio::sync::broadcast`] channel. It is **additive**: the engine-wide
//! [`crate::engine::event::EngineEvent`] bus is unchanged, and every existing
//! subscriber keeps working. A consumer that only cares about one thread (a UI
//! pane, a server `thread/read` handler) subscribes to that thread's channel
//! instead of filtering the global bus by `thread_id`.
//!
//! ## Delivery semantics
//!
//! Broadcast delivery is lossy under back-pressure: a slow receiver that lags
//! past the channel capacity observes a `RecvError::Lagged` and skips ahead,
//! exactly like the engine bus. Producers therefore ignore the send result
//! (`let _ = tx.send(..)`); a dropped event never blocks the turn loop.

use zhive_proto::domain::{Item, ThreadStatus, TurnId, TurnStatus};

/// Capacity of each per-thread [`ThreadEvent`] broadcast channel.
///
/// One slot per recent transcript event; a UI consumer that falls this far
/// behind is re-synced from persistence rather than blocking the producer.
pub const THREAD_EVENT_CAP: usize = 256;

/// An event observed on a single thread's lifecycle.
///
/// Mirrors the thread-relevant subset of
/// [`crate::engine::event::EngineEvent`] without the `thread_id` field (the
/// channel is already thread-scoped). `#[non_exhaustive]` so new variants do
/// not break match arms downstream.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::TurnId;
/// use zhive_core::state::ThreadEvent;
///
/// let ev = ThreadEvent::TurnStarted {
///     turn_id: TurnId(Arc::from("turn:t/0")),
///     started_at: 1_700_000_000,
/// };
/// assert!(matches!(ev, ThreadEvent::TurnStarted { .. }));
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ThreadEvent {
    /// A turn started on this thread.
    TurnStarted {
        /// The new turn id.
        turn_id: TurnId,
        /// Unix-seconds start timestamp.
        started_at: i64,
    },
    /// An item was appended inside an active turn.
    ItemAppended {
        /// Containing turn id.
        turn_id: TurnId,
        /// Boxed payload to keep the enum size moderate.
        item: Box<Item>,
    },
    /// A turn reached a terminal state.
    TurnCompleted {
        /// The ending turn id.
        turn_id: TurnId,
        /// Final turn status.
        status: TurnStatus,
    },
    /// Thread-level metadata changed (name and/or lifecycle status).
    MetadataChanged {
        /// New session name, when it changed.
        name: Option<String>,
        /// New lifecycle status, when it changed.
        status: Option<ThreadStatus>,
        /// Unix-seconds timestamp of the change.
        updated_at: i64,
    },
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn turn_started_variant_constructs() {
        let ev = ThreadEvent::TurnStarted {
            turn_id: TurnId(Arc::from("turn:t/0")),
            started_at: 42,
        };
        match ev {
            ThreadEvent::TurnStarted {
                turn_id,
                started_at,
            } => {
                assert_eq!(turn_id.0.as_ref(), "turn:t/0");
                assert_eq!(started_at, 42);
            }
            other => panic!("expected TurnStarted, got {other:?}"),
        }
    }

    #[test]
    fn item_appended_is_clonable() {
        // Broadcast requires Clone; verify the boxed item clones.
        let ev = ThreadEvent::ItemAppended {
            turn_id: TurnId(Arc::from("turn:t/0")),
            item: Box::new(Item::AgentMessage {
                id: zhive_proto::domain::ItemId(Arc::from("i0")),
                text: "hi".into(),
            }),
        };
        let cloned = ev.clone();
        assert!(matches!(cloned, ThreadEvent::ItemAppended { .. }));
    }
}

// Rust guideline compliant 2026-02-21

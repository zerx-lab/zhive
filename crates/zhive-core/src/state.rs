//! Engine-resident state: threads, active turns, and the turn-dimensioned
//! transcript ([`TurnHistoryBuffer`]) retained in memory with lazy eviction.
//!
//! State lives in process memory; the [`TurnHistoryBuffer`] keeps a rolling
//! window of recent turns and evicts the oldest completed turns' items (B2),
//! while [`ThreadEvent`] fans out per-thread lifecycle events beside the
//! engine-wide bus. B3 wires the `SQLite` indices in for durable rebuild.

pub mod pending_writes;
pub mod thread;
pub mod thread_event;
pub mod turn_buffer;

#[doc(inline)]
pub use pending_writes::{PendingFlushError, PendingSessionWrite, PendingSessionWrites};
#[doc(inline)]
pub use thread::{ActiveTurn, ThreadHandle, ThreadStore};
#[doc(inline)]
pub use thread_event::{THREAD_EVENT_CAP, ThreadEvent};
#[doc(inline)]
pub use turn_buffer::{IN_MEMORY_TURN_CAP, TurnHistoryBuffer};

// Rust guideline compliant 2026-02-21

//! Client-side identifier generation for threads and user-input items.
//!
//! The engine never allocates ids for client-initiated work: `engine/start_turn`
//! accepts a caller-chosen [`ThreadId`] and auto-creates the thread on first use
//! (see `zhive-core` `ThreadStore::get_or_init`). The TUI therefore mints its
//! own ids here. The canonical spec format is `thread:<provenance>/<uuid-v7>`,
//! but the engine accepts any syntactically valid string, so — to avoid pulling
//! in a `uuid` dependency — ids are built from a millisecond timestamp plus a
//! per-process monotonic counter, which is unique within a session and sorts by
//! creation order just like a v7 UUID would.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use zhive_proto::domain::{ItemId, ThreadId};

/// Per-process monotonic counter that disambiguates ids minted in the same
/// millisecond. Wrapping is irrelevant: collisions would require 2^64 ids.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Milliseconds since the Unix epoch, or `0` if the clock is before the epoch.
fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Returns the next value of the per-process disambiguation counter.
fn next_seq() -> u64 {
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Generates a fresh native thread id like `thread:native/<millis>-<seq>`.
///
/// # Examples
///
/// ```
/// use zhive_tui::id::new_thread_id;
/// let a = new_thread_id();
/// let b = new_thread_id();
/// assert!(a.0.starts_with("thread:native/"));
/// assert_ne!(a, b);
/// ```
#[must_use]
pub fn new_thread_id() -> ThreadId {
    ThreadId(std::sync::Arc::from(
        format!("thread:native/{}-{}", now_millis(), next_seq()).as_str(),
    ))
}

/// Generates a fresh user-input item id scoped under `thread`.
///
/// The shape mirrors the engine's `item:<...>/<seq>` convention closely enough
/// that the prompt-reconstruction code can use it as a stable `tool_call_id`
/// fallback, while staying globally unique within the session.
///
/// # Examples
///
/// ```
/// use zhive_tui::id::{new_thread_id, new_user_item_id};
/// let thread = new_thread_id();
/// let item = new_user_item_id(&thread);
/// assert!(item.0.starts_with("item:"));
/// ```
#[must_use]
pub fn new_user_item_id(thread: &ThreadId) -> ItemId {
    ItemId(std::sync::Arc::from(
        format!("item:{}/u{}", thread.0, next_seq()).as_str(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_ids_are_unique_and_prefixed() {
        let ids: Vec<_> = (0..100).map(|_| new_thread_id()).collect();
        for id in &ids {
            assert!(id.0.starts_with("thread:native/"));
        }
        let unique: std::collections::HashSet<_> = ids.iter().map(|i| i.0.to_string()).collect();
        assert_eq!(unique.len(), ids.len(), "all minted ids must be distinct");
    }

    #[test]
    fn user_item_ids_are_scoped_under_thread() {
        let thread = new_thread_id();
        let item = new_user_item_id(&thread);
        assert!(item.0.starts_with("item:thread:native/"));
    }
}

// Rust guideline compliant 2026-02-21

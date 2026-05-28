//! Thread / turn handles owned by the engine.
//!
//! Phase 1 keeps the surface intentionally thin: the [`ThreadHandle`]
//! carries enough state for the engine actor to route messages and emit
//! events; the lazy-loaded transcript window and the persistence sync
//! point land in B2 / B3.

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};
use zhive_proto::domain::{Item, ItemId, ThreadId, ThreadStatus, TurnId};

/// Single owner record for an active or recently-loaded thread.
#[derive(Debug)]
pub struct ThreadHandle {
    /// Stable thread id.
    pub id: ThreadId,
    /// Lifecycle status reflected back into the wire schema.
    pub status: RwLock<ThreadStatus>,
    /// At most one active turn at a time (B7 may relax this).
    pub active_turn: Mutex<Option<ActiveTurn>>,
    /// Bounded tail of recent items kept in memory; older items live in
    /// the JSONL rollout (B3).
    pub items_tail: RwLock<VecDeque<Item>>,
    /// Maximum tail length before older items are evicted to the rollout
    /// loader.
    pub items_tail_capacity: usize,
}

impl ThreadHandle {
    /// Default capacity for [`Self::items_tail`].
    ///
    /// Sized to cover a typical mid-length turn (≈ 200 items) with
    /// headroom; tunable per-thread via [`Self::with_capacity`].
    pub const DEFAULT_TAIL_CAPACITY: usize = 256;

    /// Builds a fresh handle in the [`ThreadStatus::Idle`] state.
    #[must_use]
    pub fn new_idle(id: ThreadId) -> Self {
        Self::with_capacity(id, Self::DEFAULT_TAIL_CAPACITY)
    }

    /// Same as [`Self::new_idle`] with an explicit tail size cap.
    #[must_use]
    pub fn with_capacity(id: ThreadId, capacity: usize) -> Self {
        Self {
            id,
            status: RwLock::new(ThreadStatus::Idle),
            active_turn: Mutex::new(None),
            items_tail: RwLock::new(VecDeque::with_capacity(capacity)),
            items_tail_capacity: capacity,
        }
    }

    /// Appends an item to the tail, evicting the oldest entry once the
    /// capacity is reached.
    pub async fn push_item(&self, item: Item) {
        let mut tail = self.items_tail.write().await;
        if tail.len() == self.items_tail_capacity {
            tail.pop_front();
        }
        tail.push_back(item);
    }

    /// Returns a snapshot of every retained item id.
    pub async fn item_ids(&self) -> Vec<ItemId> {
        self.items_tail
            .read()
            .await
            .iter()
            .map(|i| i.id().clone())
            .collect()
    }
}

/// In-progress turn metadata; created when the engine accepts
/// [`crate::engine::submission::Submission::StartTurn`].
#[derive(Debug, Clone)]
pub struct ActiveTurn {
    /// Stable turn id.
    pub id: TurnId,
    /// Unix-seconds timestamp at acceptance.
    pub started_at: i64,
    /// Sequence counter for the next [`ItemId`].
    pub next_item_seq: u32,
}

impl ActiveTurn {
    /// Builds a fresh active-turn record.
    #[must_use]
    pub fn new(id: TurnId, started_at: i64) -> Self {
        Self {
            id,
            started_at,
            next_item_seq: 0,
        }
    }
}

/// Owns the live set of [`ThreadHandle`] instances.
///
/// Phase 1 keeps an in-memory map; B2 attaches the persistence-driven
/// `lazy_load_from_jsonl` loader behind the same surface so the engine
/// does not have to learn about the storage layout.
#[derive(Debug, Default)]
pub struct ThreadStore {
    inner: RwLock<std::collections::HashMap<ThreadId, Arc<ThreadHandle>>>,
}

impl ThreadStore {
    /// Builds an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the handle for `id`, creating a fresh idle one when it is
    /// missing.
    pub async fn get_or_init(&self, id: &ThreadId) -> Arc<ThreadHandle> {
        if let Some(h) = self.inner.read().await.get(id) {
            return Arc::clone(h);
        }
        let mut guard = self.inner.write().await;
        if let Some(h) = guard.get(id) {
            return Arc::clone(h);
        }
        let handle = Arc::new(ThreadHandle::new_idle(id.clone()));
        guard.insert(id.clone(), Arc::clone(&handle));
        handle
    }

    /// Returns the handle for `id` if it is already resident.
    pub async fn get(&self, id: &ThreadId) -> Option<Arc<ThreadHandle>> {
        self.inner.read().await.get(id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    fn item_id(s: &str) -> ItemId {
        ItemId(Arc::from(s))
    }

    #[tokio::test]
    async fn push_item_respects_capacity() {
        let h = ThreadHandle::with_capacity(tid("thread:native/x"), 2);
        h.push_item(Item::AgentMessage {
            id: item_id("a"),
            text: "1".into(),
        })
        .await;
        h.push_item(Item::AgentMessage {
            id: item_id("b"),
            text: "2".into(),
        })
        .await;
        h.push_item(Item::AgentMessage {
            id: item_id("c"),
            text: "3".into(),
        })
        .await;
        let ids: Vec<_> = h.item_ids().await.into_iter().map(|i| i.0).collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(&*ids[0], "b");
        assert_eq!(&*ids[1], "c");
    }

    #[tokio::test]
    async fn store_get_or_init_is_idempotent() {
        let store = ThreadStore::new();
        let id = tid("thread:native/y");
        let a = store.get_or_init(&id).await;
        let b = store.get_or_init(&id).await;
        assert!(Arc::ptr_eq(&a, &b));
    }
}

// Rust guideline compliant 2026-02-21

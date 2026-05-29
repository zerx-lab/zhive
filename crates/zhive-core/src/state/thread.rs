//! Thread / turn handles owned by the engine.
//!
//! Phase 1 keeps the surface intentionally thin: the [`ThreadHandle`]
//! carries enough state for the engine actor to route messages and emit
//! events; the lazy-loaded transcript window and the persistence sync
//! point land in B2 / B3.
//!
//! ## Injection queues
//!
//! Each [`ThreadHandle`] owns an [`InjectionQueues`] wrapped in a
//! `std::sync::Mutex` (not `tokio::sync::Mutex`).  Pushes and drains are
//! synchronous and always complete immediately — they never cross an
//! `await` point — so the lighter std lock is appropriate.  The lock
//! is taken and released within a single non-async statement, matching
//! the same pattern used for `EngineInner::phase`.

use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use zhive_proto::domain::{Item, ItemId, ThreadId, ThreadStatus, TurnId};
use zhive_proto::permission::PermissionScope;

use crate::queues::InjectionQueues;

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
    /// Three-queue injection buffer for this thread.
    ///
    /// Steer items are drained before each LLM request within the active
    /// turn; follow-up items extend the turn at its natural boundary;
    /// next-turn items seed the *next* turn and survive `cancel_turn`.
    ///
    /// A `std::sync::Mutex` is used instead of a `tokio::sync::Mutex`
    /// because `push_back`, `drain`, and `abort` are synchronous and
    /// complete immediately without yielding — the same rationale as
    /// `EngineInner::phase`.  Lock poison is recovered via
    /// `into_inner()` (consistent with `phase_lock`).
    pub injection: std::sync::Mutex<InjectionQueues>,
    /// For subagent threads: the id of the thread that spawned this one.
    ///
    /// `Some` means this thread is a child (subagent). `None` means it is
    /// a top-level thread. Used to enforce the no-recursion constraint and
    /// to skip the global `EnginePhase` rollback when the child turn finishes
    /// (only top-level threads participate in the global phase machine).
    pub parent_thread_id: Option<ThreadId>,
    /// In-process delivery channel for the subagent final result.
    ///
    /// A subagent (child) thread holds a `Sender` so the engine can
    /// deliver [`crate::subagent::SubagentFinalEvent`] to whoever spawned
    /// it directly, without requiring the spawner to subscribe to the
    /// engine-wide broadcast bus and filter by `child_thread_id`.
    ///
    /// `None` for top-level threads. `Some` for child threads; the
    /// matching `Receiver` is returned from [`ThreadHandle::new_child`]
    /// to the spawn site so it can `await` the child result directly.
    ///
    /// The broadcast bus path ([`crate::engine::event::EngineEvent::SubagentCompleted`])
    /// remains active for external observers regardless of whether this
    /// channel is wired.
    pub subagent_final_tx: Option<tokio::sync::mpsc::Sender<crate::subagent::SubagentFinalEvent>>,
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
            injection: std::sync::Mutex::new(InjectionQueues::new()),
            parent_thread_id: None,
            subagent_final_tx: None,
        }
    }

    /// Builds a fresh handle for a **subagent** (child) thread.
    ///
    /// The handle starts `Idle` with an empty transcript (fresh context
    /// window). `parent_thread_id` is stored so the engine can:
    /// - Enforce the no-recursion constraint (`parent.parent_thread_id.is_some()`).
    /// - Skip the global `EnginePhase` rollback in `finish_turn` (child threads
    ///   do not own a slot in the phase machine; only top-level threads do).
    ///
    /// Returns `(handle, rx)` where `rx` is the in-process receiver for the
    /// child's [`crate::subagent::SubagentFinalEvent`].  The spawning site
    /// retains `rx` and can `await` it to get the child result directly,
    /// without subscribing to the broadcast bus.
    ///
    /// The broadcast path ([`crate::engine::event::EngineEvent::SubagentCompleted`])
    /// is always emitted in addition to the in-process channel delivery —
    /// external observers can use either mechanism.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_core::state::ThreadHandle;
    ///
    /// let parent_id = ThreadId(Arc::from("thread:native/parent"));
    /// let child_id  = ThreadId(Arc::from("thread:subagent/parent/0"));
    /// let (handle, _rx) = ThreadHandle::new_child(child_id.clone(), parent_id.clone());
    /// assert_eq!(handle.parent_thread_id, Some(parent_id));
    /// ```
    #[must_use]
    pub fn new_child(
        id: ThreadId,
        parent_thread_id: ThreadId,
    ) -> (
        Self,
        tokio::sync::mpsc::Receiver<crate::subagent::SubagentFinalEvent>,
    ) {
        // Channel capacity 1: exactly one final event per subagent turn.
        // The spawner may drop `rx` without consuming it if it only cares
        // about the broadcast bus; the `Sender` side simply fails silently
        // (the broadcast path still delivers the outcome).
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let handle = Self {
            id,
            status: RwLock::new(ThreadStatus::Idle),
            active_turn: Mutex::new(None),
            items_tail: RwLock::new(VecDeque::with_capacity(Self::DEFAULT_TAIL_CAPACITY)),
            items_tail_capacity: Self::DEFAULT_TAIL_CAPACITY,
            injection: std::sync::Mutex::new(InjectionQueues::new()),
            parent_thread_id: Some(parent_thread_id),
            subagent_final_tx: Some(tx),
        };
        (handle, rx)
    }

    /// Returns a lock guard on the injection queues.
    ///
    /// Recovers from mutex poison (same pattern as `EngineInner::phase_lock`):
    /// a poisoned lock means another thread panicked while holding it, which
    /// is a programming error; recovering the inner value lets the engine
    /// continue rather than tearing down the actor.
    pub(crate) fn injection_lock(&self) -> std::sync::MutexGuard<'_, InjectionQueues> {
        match self.injection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Appends an item to the tail, evicting older entries when over capacity.
    ///
    /// The loop tolerates the tail already exceeding `items_tail_capacity`
    /// (e.g. after a future API lowers the cap at runtime); the strict `==`
    /// guard previously used would have stopped evicting in that case.
    pub async fn push_item(&self, item: Item) {
        let mut tail = self.items_tail.write().await;
        while tail.len() >= self.items_tail_capacity {
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
///
/// The `cancel` token is fired by `cancel_turn` so the spawned turn task
/// exits its streaming `select!` loop without waiting for the stream to
/// drain. `next_item_seq` is reserved for any engine-minted items; items
/// produced by `StreamFold` get their IDs from the fold's own counter and
/// do not consume from `next_item_seq`.
///
/// `scope` records the [`PermissionScope`] the turn runs under so that
/// a child subagent can narrow it during spawn validation.
#[derive(Debug, Clone)]
pub struct ActiveTurn {
    /// Stable turn id.
    pub id: TurnId,
    /// Unix-seconds timestamp at acceptance.
    pub started_at: i64,
    /// Sequence counter for engine-minted items (user-input items and any
    /// internal notices the engine itself creates outside of `StreamFold`).
    ///
    /// Provider-output items have their `ItemId` minted by `StreamFold`,
    /// so this field is not consumed by the provider streaming path.
    pub next_item_seq: u32,
    /// Per-turn cancellation token; cancelled by [`cancel_turn`] to stop
    /// the in-flight provider stream task.
    ///
    /// [`cancel_turn`]: crate::engine::submission::Submission::CancelTurn
    pub cancel: CancellationToken,
    /// The permission scope this turn executes under.
    ///
    /// Stored here so that a subagent spawn request can access the
    /// parent turn's live scope without acquiring extra locks.
    pub scope: PermissionScope,
}

impl ActiveTurn {
    /// Builds a fresh active-turn record with a new cancellation token.
    ///
    /// The turn scope defaults to [`PermissionScope::default_turn_scope`].
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_proto::domain::TurnId;
    /// use zhive_core::state::ActiveTurn;
    ///
    /// let id = TurnId(Arc::from("turn:t/0"));
    /// let active = ActiveTurn::new(id.clone(), 0);
    /// assert_eq!(active.id, id);
    /// assert!(!active.cancel.is_cancelled());
    /// ```
    #[must_use]
    pub fn new(id: TurnId, started_at: i64) -> Self {
        Self {
            id,
            started_at,
            next_item_seq: 0,
            cancel: CancellationToken::new(),
            scope: PermissionScope::default_turn_scope(),
        }
    }

    /// Builds a fresh active-turn record with an **explicit** cancellation
    /// token supplied by the caller.
    ///
    /// This variant is used by the engine when the turn cancel token must be
    /// a child of the engine-wide [`crate::cancel::CancellationTree`] root so
    /// that `cancel_tree.cancel_all()` (engine shutdown) propagates to all
    /// in-flight turns automatically.
    ///
    /// The turn scope defaults to [`PermissionScope::default_turn_scope`].
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use tokio_util::sync::CancellationToken;
    /// use zhive_proto::domain::TurnId;
    /// use zhive_core::state::ActiveTurn;
    ///
    /// let parent = CancellationToken::new();
    /// let child = parent.child_token();
    /// let id = TurnId(Arc::from("turn:t/0"));
    /// let active = ActiveTurn::new_with_cancel(id.clone(), 0, child);
    /// assert!(!active.cancel.is_cancelled());
    /// parent.cancel();
    /// assert!(active.cancel.is_cancelled(), "child inherits parent cancel");
    /// ```
    #[must_use]
    pub fn new_with_cancel(id: TurnId, started_at: i64, cancel: CancellationToken) -> Self {
        Self {
            id,
            started_at,
            next_item_seq: 0,
            cancel,
            scope: PermissionScope::default_turn_scope(),
        }
    }

    /// Builds a fresh active-turn record with an explicit cancellation token
    /// and an explicit [`PermissionScope`].
    ///
    /// Used when spawning subagent turns where the child scope has already
    /// been computed by [`crate::subagent::prepare_child_scope`].
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use tokio_util::sync::CancellationToken;
    /// use zhive_proto::domain::TurnId;
    /// use zhive_proto::permission::PermissionScope;
    /// use zhive_core::state::ActiveTurn;
    ///
    /// let token = CancellationToken::new();
    /// let scope = PermissionScope::default_turn_scope();
    /// let id = TurnId(Arc::from("turn:t/0"));
    /// let active = ActiveTurn::new_with_cancel_and_scope(id.clone(), 0, token, scope);
    /// assert!(!active.cancel.is_cancelled());
    /// assert!(!active.scope.allow_subagent_spawn);
    /// ```
    #[must_use]
    pub fn new_with_cancel_and_scope(
        id: TurnId,
        started_at: i64,
        cancel: CancellationToken,
        scope: PermissionScope,
    ) -> Self {
        Self {
            id,
            started_at,
            next_item_seq: 0,
            cancel,
            scope,
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

    /// Returns a write-locked guard on the inner map.
    ///
    /// Used by the engine to atomically insert a new child thread handle
    /// without going through `get_or_init` (which creates an idle handle).
    pub(crate) async fn write_guard(
        &self,
    ) -> tokio::sync::RwLockWriteGuard<'_, std::collections::HashMap<ThreadId, Arc<ThreadHandle>>>
    {
        self.inner.write().await
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

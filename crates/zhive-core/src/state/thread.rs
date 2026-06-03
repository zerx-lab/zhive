//! Thread / turn handles owned by the engine.
//!
//! The [`ThreadHandle`] carries enough state for the engine actor to route
//! messages and emit events: a turn-dimensioned transcript
//! ([`crate::state::TurnHistoryBuffer`]) with lazy eviction (B2) and a
//! per-thread [`crate::state::ThreadEvent`] broadcast. The persistence sync
//! point lands in B3.
//!
//! ## Injection queues
//!
//! Each [`ThreadHandle`] owns an [`InjectionQueues`] wrapped in a
//! `std::sync::Mutex` (not `tokio::sync::Mutex`).  Pushes and drains are
//! synchronous and always complete immediately — they never cross an
//! `await` point — so the lighter std lock is appropriate.  The lock
//! is taken and released within a single non-async statement, matching
//! the same pattern used for `EngineInner::phase`.

use std::sync::Arc;

use tokio::sync::{Mutex, RwLock, broadcast};
use tokio_util::sync::CancellationToken;
use zhive_proto::domain::{Item, ItemId, ThreadId, ThreadStatus, TurnId, TurnStatus};
use zhive_proto::permission::PermissionScope;

use crate::queues::InjectionQueues;
use crate::state::PendingSessionWrites;
use crate::state::thread_event::{THREAD_EVENT_CAP, ThreadEvent};
use crate::state::turn_buffer::TurnHistoryBuffer;

/// Single owner record for an active or recently-loaded thread.
#[derive(Debug)]
pub struct ThreadHandle {
    /// Stable thread id.
    pub id: ThreadId,
    /// Lifecycle status reflected back into the wire schema.
    pub status: RwLock<ThreadStatus>,
    /// At most one active turn at a time (B7 may relax this).
    pub active_turn: Mutex<Option<ActiveTurn>>,
    /// Turn-dimensioned in-memory transcript with lazy eviction.
    ///
    /// Replaces the former flat `items_tail` (B2): the buffer keeps a rolling
    /// window of recent turns and evicts the oldest completed turns' items to
    /// [`zhive_proto::domain::TurnItemsView::NotLoaded`], leaving headers
    /// resident so they can be lazily reloaded from the rollout. This is the
    /// single in-memory item store on the handle; flat-view consumers use
    /// [`Self::items_snapshot`].
    ///
    /// A `tokio::sync::Mutex` (not `std::sync::Mutex`) is used because the
    /// push / snapshot call sites in the turn loop are async; the buffer
    /// methods themselves never await, so the lock is always short-held.
    pub(crate) history: Mutex<TurnHistoryBuffer>,
    /// Per-thread broadcast of [`ThreadEvent`]s, a second event layer beside
    /// the engine-wide [`crate::engine::event::EngineEvent`] bus.
    ///
    /// Additive: the engine bus is unchanged. Senders ignore the result, so a
    /// lagging subscriber never blocks the turn loop.
    pub(crate) events: broadcast::Sender<ThreadEvent>,
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
    /// Per-thread buffer of session writes deferred outside the `Idle` phase.
    ///
    /// While the engine phase is non-`Idle` (turn / compaction / subagent /
    /// retry), session writes are buffered here instead of reaching storage
    /// immediately, so a crash mid-turn cannot leave half-finished state on
    /// disk.  At the next save point the engine flushes the buffer to the
    /// persistence writer (B7).
    ///
    /// A `std::sync::Mutex` is used rather than a `tokio::sync::Mutex` for the
    /// same reason as [`Self::injection`]: pushes and flushes complete
    /// synchronously without crossing an `await` point.  Lock poison is
    /// recovered via `into_inner()` (consistent with `injection_lock`).
    pub(crate) pending_session_writes: std::sync::Mutex<PendingSessionWrites>,
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
    /// In-process child → parent per-tool-call permission handshake channel.
    ///
    /// A subagent (child) thread holds a `Sender` so its tool dispatch can
    /// report each non-`Deny` decision to the parent spawner and park on the
    /// reply before executing (the full per-tool-call handshake). The matching
    /// `Receiver` is returned by [`ThreadHandle::new_child`] to the spawn site,
    /// where the parent's `spawn_and_await` select loop consumes it and runs a
    /// second fold (parent hooks + the child decision) before replying.
    ///
    /// `None` for top-level threads (whose tool dispatch runs its own
    /// `Ask` / `Defer` reverse-RPC directly) and for any idle / lazily reloaded
    /// thread; the channel lifetime is strictly contained within one child
    /// turn. The capacity is 1 because a child resolves its tool calls
    /// serially in Phase 1.
    pub(crate) subagent_decision_tx:
        Option<tokio::sync::mpsc::Sender<crate::subagent::SubagentDecisionRequest>>,
}

impl ThreadHandle {
    /// Default number of completed turns kept resident in memory.
    ///
    /// Forwards to [`crate::state::turn_buffer::IN_MEMORY_TURN_CAP`]; tunable
    /// per-thread via [`Self::with_capacity`].
    pub const DEFAULT_TURN_CAPACITY: usize = crate::state::turn_buffer::IN_MEMORY_TURN_CAP;

    /// Builds a fresh handle in the [`ThreadStatus::Idle`] state.
    #[must_use]
    pub fn new_idle(id: ThreadId) -> Self {
        Self::with_capacity(id, Self::DEFAULT_TURN_CAPACITY)
    }

    /// Same as [`Self::new_idle`] with an explicit completed-turn cap.
    ///
    /// `capacity` is the number of completed turns kept fully resident before
    /// the oldest are evicted to
    /// [`zhive_proto::domain::TurnItemsView::NotLoaded`] (B2 turn-dimensioned
    /// model), not a flat item count.
    #[must_use]
    pub fn with_capacity(id: ThreadId, capacity: usize) -> Self {
        let (events, _rx) = broadcast::channel(THREAD_EVENT_CAP);
        Self {
            id,
            status: RwLock::new(ThreadStatus::Idle),
            active_turn: Mutex::new(None),
            history: Mutex::new(TurnHistoryBuffer::with_cap(capacity)),
            events,
            injection: std::sync::Mutex::new(InjectionQueues::new()),
            pending_session_writes: std::sync::Mutex::new(PendingSessionWrites::new()),
            parent_thread_id: None,
            subagent_final_tx: None,
            subagent_decision_tx: None,
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
    /// Returns `(handle, final_rx, decision_rx)`:
    /// - `final_rx` is the in-process receiver for the child's
    ///   [`crate::subagent::SubagentFinalEvent`] (completion / error /
    ///   suspension); the spawn site `await`s it for the child result.
    /// - `decision_rx` is the per-tool-call handshake receiver carrying
    ///   [`crate::subagent::SubagentDecisionRequest`]s; the parent spawner
    ///   consumes it in its select loop to run a second permission fold and
    ///   reply with a [`crate::subagent::ParentVerdict`] before the child
    ///   executes the call.
    ///
    /// Both channels have capacity 1: a child turn produces exactly one final
    /// event and resolves its tool calls serially in Phase 1.
    ///
    /// The broadcast path ([`crate::engine::event::EngineEvent::SubagentCompleted`])
    /// is always emitted in addition to the in-process channel delivery —
    /// external observers can use either mechanism.
    ///
    /// Visibility is `pub(crate)` because the returned `decision_rx` carries
    /// [`crate::subagent::SubagentDecisionRequest`] (an in-process,
    /// non-`Clone` handshake type that never crosses the crate boundary); the
    /// engine is the only legitimate creator of child threads. See the engine
    /// `subagent_spawn` integration tests for usage.
    #[must_use]
    pub(crate) fn new_child(
        id: ThreadId,
        parent_thread_id: ThreadId,
    ) -> (
        Self,
        tokio::sync::mpsc::Receiver<crate::subagent::SubagentFinalEvent>,
        tokio::sync::mpsc::Receiver<crate::subagent::SubagentDecisionRequest>,
    ) {
        // Channel capacity 1: exactly one final event per subagent turn.
        // The spawner may drop `final_rx` without consuming it if it only
        // cares about the broadcast bus; the `Sender` side simply fails
        // silently (the broadcast path still delivers the outcome).
        let (final_tx, final_rx) = tokio::sync::mpsc::channel(1);
        // Decision channel capacity 1: the child resolves tool calls serially,
        // so at most one handshake is in flight at a time. If the parent
        // spawner already dropped `decision_rx`, the child's `send` fails and
        // its dispatch falls back to a conservative deny.
        let (decision_tx, decision_rx) = tokio::sync::mpsc::channel(1);
        let (events, _erx) = broadcast::channel(THREAD_EVENT_CAP);
        let handle = Self {
            id,
            status: RwLock::new(ThreadStatus::Idle),
            active_turn: Mutex::new(None),
            history: Mutex::new(TurnHistoryBuffer::with_cap(Self::DEFAULT_TURN_CAPACITY)),
            events,
            injection: std::sync::Mutex::new(InjectionQueues::new()),
            pending_session_writes: std::sync::Mutex::new(PendingSessionWrites::new()),
            parent_thread_id: Some(parent_thread_id),
            subagent_final_tx: Some(final_tx),
            subagent_decision_tx: Some(decision_tx),
        };
        (handle, final_rx, decision_rx)
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

    /// Returns a lock guard on the pending session-write buffer.
    ///
    /// Recovers from mutex poison the same way as [`Self::injection_lock`]:
    /// a poisoned lock means another thread panicked while holding it, which
    /// is a programming error; recovering the inner value lets the engine
    /// continue rather than tearing down the actor.
    pub(crate) fn pending_writes_lock(&self) -> std::sync::MutexGuard<'_, PendingSessionWrites> {
        match self.pending_session_writes.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Returns a fresh subscription to this thread's [`ThreadEvent`] stream.
    ///
    /// Independent of the engine-wide [`crate::engine::event::EngineEvent`]
    /// bus; both fire for the same lifecycle transitions.
    #[must_use]
    pub fn subscribe_events(&self) -> broadcast::Receiver<ThreadEvent> {
        self.events.subscribe()
    }

    /// Installs a fresh active turn in the history buffer.
    ///
    /// Called by the engine when a turn is accepted, before any item is
    /// pushed, so [`Self::push_item`] has a turn to append to.
    pub(crate) async fn start_turn_buffer(&self, turn_id: TurnId, started_at: i64) {
        use zhive_proto::domain::{Turn, TurnItemsView};
        let turn = Turn {
            id: turn_id,
            items: vec![],
            items_view: TurnItemsView::Full,
            status: TurnStatus::InProgress,
            error: None,
            started_at: Some(started_at),
            completed_at: None,
            duration_ms: None,
        };
        self.history.lock().await.start_turn(turn);
    }

    /// Finalises the active turn in the history buffer.
    ///
    /// Moves the active turn into the completed window with its terminal
    /// `status`, then enforces the in-memory turn cap.
    pub(crate) async fn finish_turn_buffer(
        &self,
        status: TurnStatus,
        completed_at: i64,
        duration_ms: Option<i64>,
    ) {
        self.history
            .lock()
            .await
            .finish_turn(status, completed_at, duration_ms);
    }

    /// Appends an item to the active turn's transcript.
    ///
    /// Items only arrive inside a turn; when no turn is active the buffer drops
    /// the item with a debug log (an engine-side ordering bug, not a
    /// recoverable condition).
    ///
    /// A [`ThreadEvent::ItemAppended`] is fanned out on the thread-scoped
    /// broadcast channel for every push that lands in an active turn. This is
    /// additive: the engine-wide [`crate::engine::event::EngineEvent`] bus is
    /// emitted separately by the engine and is unaffected.
    pub async fn push_item(&self, item: Item) {
        let turn_id = {
            let mut hist = self.history.lock().await;
            let turn_id = hist.active_turn_id();
            hist.push_item(item.clone());
            turn_id
        };
        if let Some(turn_id) = turn_id {
            let _ = self.events.send(ThreadEvent::ItemAppended {
                turn_id,
                item: Box::new(item),
            });
        }
    }

    /// Returns a snapshot of every resident item id in conversation order.
    pub async fn item_ids(&self) -> Vec<ItemId> {
        self.history.lock().await.all_item_ids()
    }

    /// Returns a flat, in-order clone of every resident item.
    ///
    /// Walks completed turns then the active turn (see
    /// [`TurnHistoryBuffer::iter_items`]); evicted turns contribute nothing.
    /// This is the flat-view entry point for the prompt builder, compaction
    /// snapshot, and subagent final-message extraction.
    pub async fn items_snapshot(&self) -> Vec<Item> {
        self.history.lock().await.iter_items().cloned().collect()
    }

    /// Returns a clone of only the **active** turn's items, in append order.
    ///
    /// Thin async wrapper over [`TurnHistoryBuffer::active_turn_items`]. The
    /// turn runner calls this at the start of a turn to enqueue the input
    /// items (user message / next-turn seeds / subagent prompt) for storage —
    /// they are pushed before the provider loop and would otherwise never be
    /// persisted. Returns an empty `Vec` when no turn is active.
    ///
    /// [`TurnHistoryBuffer::active_turn_items`]: crate::state::TurnHistoryBuffer::active_turn_items
    pub async fn active_turn_items(&self) -> Vec<Item> {
        self.history.lock().await.active_turn_items()
    }

    /// Returns the count of resident items across active + completed turns.
    pub async fn item_count(&self) -> usize {
        self.history.lock().await.item_count()
    }

    /// Replaces the whole transcript with a single compaction-handoff turn.
    ///
    /// Used by context compaction to swap the live history for `[marker,
    /// summary]`; see [`TurnHistoryBuffer::replace_with_compaction`].
    pub(crate) async fn replace_history_with_compaction(&self, turn_id: TurnId, items: Vec<Item>) {
        self.history
            .lock()
            .await
            .replace_with_compaction(turn_id, items);
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

    /// Removes the handle for `id` from the store, if present.
    ///
    /// Called by the engine after a thread is deleted so in-memory state does
    /// not outlive the persistent record. A missing `id` is a no-op.
    pub(crate) async fn remove(&self, id: &ThreadId) {
        self.inner.write().await.remove(id);
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

    #[test]
    fn new_idle_starts_with_empty_pending_writes() {
        let h = ThreadHandle::new_idle(tid("thread:native/pw"));
        assert!(h.pending_writes_lock().is_empty());
    }

    #[test]
    fn new_child_starts_with_empty_pending_writes() {
        let (h, _final_rx, _decision_rx) =
            ThreadHandle::new_child(tid("thread:subagent/p/0"), tid("thread:native/p"));
        assert!(h.pending_writes_lock().is_empty());
    }

    #[tokio::test]
    async fn push_item_appends_to_active_turn() {
        let h = ThreadHandle::new_idle(tid("thread:native/x"));
        // No active turn yet: pushes are dropped (engine seeds the turn first).
        h.push_item(Item::AgentMessage {
            id: item_id("orphan"),
            text: "x".into(),
        })
        .await;
        assert!(h.item_ids().await.is_empty());

        // Seed a turn, then pushed items are retained in order.
        h.start_turn_buffer(TurnId(Arc::from("turn:x/0")), 0).await;
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
        let ids: Vec<_> = h.item_ids().await.into_iter().map(|i| i.0).collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(&*ids[0], "a");
        assert_eq!(&*ids[1], "b");
        assert_eq!(h.item_count().await, 2);
    }

    #[tokio::test]
    async fn subscribe_events_receives_broadcast() {
        let h = ThreadHandle::new_idle(tid("thread:native/ev"));
        let mut rx = h.subscribe_events();
        let _ = h.events.send(ThreadEvent::TurnStarted {
            turn_id: TurnId(Arc::from("turn:ev/0")),
            started_at: 7,
        });
        let ev = rx.try_recv().expect("event delivered");
        assert!(matches!(ev, ThreadEvent::TurnStarted { started_at: 7, .. }));
    }

    #[tokio::test]
    async fn store_get_or_init_is_idempotent() {
        let store = ThreadStore::new();
        let id = tid("thread:native/y");
        let a = store.get_or_init(&id).await;
        let b = store.get_or_init(&id).await;
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[tokio::test]
    async fn store_remove_drops_handle() {
        let store = ThreadStore::new();
        let id = tid("thread:native/del");
        store.get_or_init(&id).await;
        assert!(
            store.get(&id).await.is_some(),
            "handle must be present after init"
        );
        store.remove(&id).await;
        assert!(
            store.get(&id).await.is_none(),
            "handle must be absent after remove"
        );
        // Removing a non-existent id must not panic.
        store.remove(&id).await;
    }
}

// Rust guideline compliant 2026-02-21

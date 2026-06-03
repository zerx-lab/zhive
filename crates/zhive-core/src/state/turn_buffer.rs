//! Turn-dimensioned in-memory transcript with lazy eviction.
//!
//! [`TurnHistoryBuffer`] replaces the flat per-thread item tail with a rolling
//! window of [`Turn`]s: one optional in-progress `active` turn plus a bounded
//! deque of `completed` turns. When the completed window exceeds its cap, the
//! oldest turn's items are evicted in place (its [`TurnItemsView`] drops to
//! [`TurnItemsView::NotLoaded`] and the items are cleared), and
//! `lazy_unloaded_count` is bumped. The turn header (id / timestamps / status)
//! is always retained so the turn list stays complete and the items can be
//! re-fetched on demand from persistence (see
//! [`crate::persistence::StateDb::load_items_page`]).
//!
//! ## Single source of truth
//!
//! This buffer is the *only* in-memory item store on a thread handle; there is
//! no parallel flat tail. Consumers that need a flat, in-order item view (the
//! prompt builder, compaction snapshot, subagent final-message extraction) call
//! [`TurnHistoryBuffer::iter_items`], which walks `completed` then `active` in
//! order. Only items still resident (`Full` turns and the active turn) are
//! yielded; evicted turns contribute nothing until refilled.
//!
//! ## Three-state items view (Phase 1: one-level eviction)
//!
//! [`TurnItemsView`] has three states — `Full`, `Summary`, `NotLoaded`. This
//! phase implements a single eviction step (`Full → NotLoaded`); the `Summary`
//! state is reserved for compaction-generated summaries and is not produced
//! here.
//!
//! ## Synchrony
//!
//! Every method is synchronous and allocation-bounded; the buffer is meant to
//! live behind a short-held lock on the thread handle and never crosses an
//! `await` point.

use std::collections::VecDeque;

use zhive_proto::domain::{Item, ItemId, Turn, TurnItemsView, TurnStatus};

/// Default number of completed turns kept fully resident in memory.
///
/// Sized so a long session keeps recent context hot while older turns are
/// evicted to persistence. Tunable via [`TurnHistoryBuffer::with_cap`].
/// Chosen above the auto-compaction item threshold so compaction (which folds
/// the live transcript) typically fires before turn-level eviction loses
/// history.
pub const IN_MEMORY_TURN_CAP: usize = 50;

/// Rolling window of turns with lazy eviction of the oldest completed turns.
///
/// See the module header for the eviction model and the single-source-of-truth
/// contract.
#[derive(Debug)]
pub struct TurnHistoryBuffer {
    /// The in-progress turn, if any. Mirrors the runtime `ActiveTurn` by id.
    active: Option<Turn>,
    /// Completed turns, oldest at the front.
    completed: VecDeque<Turn>,
    /// Maximum number of completed turns kept fully resident before the oldest
    /// are evicted to [`TurnItemsView::NotLoaded`].
    in_memory_turn_cap: usize,
    /// Number of completed turns whose items have been evicted from memory.
    lazy_unloaded_count: usize,
}

impl Default for TurnHistoryBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnHistoryBuffer {
    /// Builds an empty buffer with the default [`IN_MEMORY_TURN_CAP`].
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::state::TurnHistoryBuffer;
    /// let buf = TurnHistoryBuffer::new();
    /// assert_eq!(buf.item_count(), 0);
    /// assert_eq!(buf.lazy_unloaded_count(), 0);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::with_cap(IN_MEMORY_TURN_CAP)
    }

    /// Builds an empty buffer with an explicit completed-turn cap.
    ///
    /// A `cap` of `0` is raised to `1` so there is always room for at least one
    /// completed turn before eviction (a zero cap would evict every turn the
    /// instant it completes, which is never useful).
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::state::TurnHistoryBuffer;
    /// let buf = TurnHistoryBuffer::with_cap(8);
    /// assert_eq!(buf.completed_len(), 0);
    /// ```
    #[must_use]
    pub fn with_cap(cap: usize) -> Self {
        Self {
            active: None,
            completed: VecDeque::new(),
            in_memory_turn_cap: cap.max(1),
            lazy_unloaded_count: 0,
        }
    }

    /// Installs `turn` as the active turn.
    ///
    /// Any previously-active turn that was never finished is moved into the
    /// completed window first (defensive: the engine always finishes a turn
    /// before starting the next, but this keeps the invariant local).
    pub fn start_turn(&mut self, turn: Turn) {
        if let Some(prev) = self.active.take() {
            self.completed.push_back(prev);
            self.enforce_cap();
        }
        self.active = Some(turn);
    }

    /// Appends `item` to the active turn's items.
    ///
    /// When no turn is active the item is dropped with a debug log: items only
    /// ever arrive inside a turn, so this signals an engine-side ordering bug
    /// rather than a recoverable condition.
    pub fn push_item(&mut self, item: Item) {
        match self.active.as_mut() {
            Some(turn) => turn.items.push(item),
            None => {
                tracing::debug!(
                    name: "zhive.state.turn_buffer.push_without_active",
                    item_id = %item.id().0,
                    "push_item called with no active turn; item dropped"
                );
            }
        }
    }

    /// Finalises the active turn and moves it into the completed window.
    ///
    /// Sets the turn's terminal `status`, `completed_at`, and `duration_ms`,
    /// then enforces the in-memory cap. A no-op when there is no active turn.
    pub fn finish_turn(&mut self, status: TurnStatus, completed_at: i64, duration_ms: Option<i64>) {
        let Some(mut turn) = self.active.take() else {
            tracing::debug!(
                name: "zhive.state.turn_buffer.finish_without_active",
                "finish_turn called with no active turn; ignored"
            );
            return;
        };
        turn.status = status;
        turn.completed_at = Some(completed_at);
        turn.duration_ms = duration_ms;
        self.completed.push_back(turn);
        self.enforce_cap();
    }

    /// Evicts the oldest completed turns until the window fits the cap.
    ///
    /// Eviction is in place: the turn header is retained, its items are
    /// cleared, and its [`TurnItemsView`] is set to [`TurnItemsView::NotLoaded`]
    /// (one-level downgrade). Already-evicted turns are skipped so the counter
    /// is not double-incremented if the cap is lowered at runtime.
    fn enforce_cap(&mut self) {
        // Number of completed turns that must shed their items so that at most
        // `in_memory_turn_cap` turns remain fully resident. We never *remove*
        // turns from the deque — the header is the durable turn list — we only
        // drop their item payloads.
        let resident = self
            .completed
            .iter()
            .filter(|t| t.items_view != TurnItemsView::NotLoaded)
            .count();
        if resident <= self.in_memory_turn_cap {
            return;
        }
        let mut to_evict = resident - self.in_memory_turn_cap;
        for turn in &mut self.completed {
            if to_evict == 0 {
                break;
            }
            if turn.items_view == TurnItemsView::NotLoaded {
                continue;
            }
            turn.items.clear();
            turn.items_view = TurnItemsView::NotLoaded;
            self.lazy_unloaded_count = self.lazy_unloaded_count.saturating_add(1);
            to_evict -= 1;
        }
    }

    /// Returns the total count of resident items across active + completed.
    ///
    /// Evicted turns contribute `0` (their items are cleared).
    #[must_use]
    pub fn item_count(&self) -> usize {
        let active = self.active.as_ref().map_or(0, |t| t.items.len());
        let completed: usize = self.completed.iter().map(|t| t.items.len()).sum();
        active + completed
    }

    /// Number of completed turns whose items have been evicted from memory.
    #[must_use]
    pub fn lazy_unloaded_count(&self) -> usize {
        self.lazy_unloaded_count
    }

    /// Number of completed turns retained (resident or evicted headers).
    #[must_use]
    pub fn completed_len(&self) -> usize {
        self.completed.len()
    }

    /// Returns `true` while a turn is active.
    #[must_use]
    pub fn has_active(&self) -> bool {
        self.active.is_some()
    }

    /// Returns the id of the active turn, if any.
    #[must_use]
    pub fn active_turn_id(&self) -> Option<zhive_proto::domain::TurnId> {
        self.active.as_ref().map(|t| t.id.clone())
    }

    /// Iterates resident items in conversation order (completed then active).
    ///
    /// Evicted (`NotLoaded`) turns yield no items. This is the flat-view entry
    /// point used by the prompt builder, compaction, and subagent extraction.
    pub fn iter_items(&self) -> impl Iterator<Item = &Item> {
        self.completed
            .iter()
            .flat_map(|t| t.items.iter())
            .chain(self.active.iter().flat_map(|t| t.items.iter()))
    }

    /// Returns every resident item id in conversation order.
    #[must_use]
    pub fn all_item_ids(&self) -> Vec<ItemId> {
        self.iter_items().map(|i| i.id().clone()).collect()
    }

    /// Returns a clone of the **active** turn's items, in append order.
    ///
    /// Unlike [`Self::iter_items`], this yields only the in-progress turn's
    /// items and never the completed window. The turn runner uses it at the
    /// start of a turn to persist the input items (user message, next-turn
    /// seeds, subagent prompt) that `start_turn` / `start_child_turn` pushed
    /// before the provider loop began — these were previously the only items
    /// never enqueued for storage. Returns an empty `Vec` when no turn is
    /// active.
    #[must_use]
    pub fn active_turn_items(&self) -> Vec<Item> {
        self.active
            .as_ref()
            .map_or_else(Vec::new, |t| t.items.clone())
    }

    /// Returns a clone of recent completed turns within `[offset, offset+limit)`.
    ///
    /// Turns are ordered newest-first: `offset = 0` is the most recently
    /// completed turn. The active turn is not included (it is in-progress). An
    /// `offset` past the end yields an empty `Vec`.
    #[must_use]
    pub fn recent_turns(&self, offset: usize, limit: usize) -> Vec<Turn> {
        self.completed
            .iter()
            .rev()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect()
    }

    /// Replaces the entire transcript with a single synthetic completed turn
    /// holding `items` (compaction handoff).
    ///
    /// Used by context compaction to swap the live history for `[marker,
    /// summary]`. The active turn (if any) and all completed turns are
    /// discarded, and `lazy_unloaded_count` is reset because there is no longer
    /// any evicted history to track. The synthetic turn is marked
    /// [`TurnStatus::Completed`] with [`TurnItemsView::Full`].
    pub fn replace_with_compaction(
        &mut self,
        turn_id: zhive_proto::domain::TurnId,
        items: Vec<Item>,
    ) {
        self.active = None;
        self.completed.clear();
        self.lazy_unloaded_count = 0;
        self.completed.push_back(Turn {
            id: turn_id,
            items,
            items_view: TurnItemsView::Full,
            status: TurnStatus::Completed,
            error: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zhive_proto::domain::TurnId;

    use super::*;

    fn turn(id: &str) -> Turn {
        Turn {
            id: TurnId(Arc::from(id)),
            items: vec![],
            items_view: TurnItemsView::Full,
            status: TurnStatus::InProgress,
            error: None,
            started_at: Some(0),
            completed_at: None,
            duration_ms: None,
        }
    }

    fn msg(id: &str) -> Item {
        Item::AgentMessage {
            id: ItemId(Arc::from(id)),
            text: id.to_owned(),
        }
    }

    #[test]
    fn start_push_finish_round_trip() {
        let mut buf = TurnHistoryBuffer::new();
        buf.start_turn(turn("turn:t/0"));
        buf.push_item(msg("i0"));
        buf.push_item(msg("i1"));
        assert!(buf.has_active());
        assert_eq!(buf.item_count(), 2);

        buf.finish_turn(TurnStatus::Completed, 100, Some(50));
        assert!(!buf.has_active(), "active cleared after finish");
        assert_eq!(buf.completed_len(), 1);

        let recent = buf.recent_turns(0, 1);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].status, TurnStatus::Completed);
        assert_eq!(recent[0].completed_at, Some(100));
        assert_eq!(recent[0].duration_ms, Some(50));
        assert_eq!(recent[0].items.len(), 2, "finished turn keeps its items");
    }

    /// `active_turn_items` returns only the in-progress turn's items and never
    /// the completed window, so the turn runner can persist just the new
    /// input items without re-enqueuing a prior turn's tail.
    #[test]
    fn active_turn_items_excludes_completed_turns() {
        let mut buf = TurnHistoryBuffer::new();
        // No active turn yet → empty.
        assert!(buf.active_turn_items().is_empty());

        // First (completed) turn.
        buf.start_turn(turn("turn:t/0"));
        buf.push_item(msg("old0"));
        buf.finish_turn(TurnStatus::Completed, 100, None);

        // Second (active) turn holds only its own items.
        buf.start_turn(turn("turn:t/1"));
        buf.push_item(msg("new0"));
        buf.push_item(msg("new1"));

        let active = buf.active_turn_items();
        let ids: Vec<&str> = active.iter().map(|i| i.id().0.as_ref()).collect();
        assert_eq!(
            ids,
            vec!["new0", "new1"],
            "active_turn_items must yield only the active turn's items in order"
        );
    }

    #[test]
    fn enforce_cap_evicts_oldest_turns_to_not_loaded() {
        // cap = 3, push cap + 3 = 6 completed turns; the oldest 3 are evicted.
        let mut buf = TurnHistoryBuffer::with_cap(3);
        for n in 0..6 {
            buf.start_turn(turn(&format!("turn:t/{n}")));
            buf.push_item(msg(&format!("i{n}")));
            buf.finish_turn(TurnStatus::Completed, i64::from(n), None);
        }

        assert_eq!(buf.completed_len(), 6, "headers are never removed");
        assert_eq!(buf.lazy_unloaded_count(), 3, "oldest 3 turns evicted");

        // The oldest three turns are NotLoaded with empty items; their headers
        // (id / status) survive.
        let evicted: Vec<&Turn> = buf
            .completed
            .iter()
            .filter(|t| t.items_view == TurnItemsView::NotLoaded)
            .collect();
        assert_eq!(evicted.len(), 3);
        for t in &evicted {
            assert!(t.items.is_empty(), "evicted turn has no resident items");
            assert_eq!(t.status, TurnStatus::Completed, "header status retained");
        }
        // The three newest remain Full and resident.
        let resident = buf.item_count();
        assert_eq!(resident, 3, "only the 3 newest turns keep their items");
    }

    #[test]
    fn iter_items_walks_completed_then_active_skipping_evicted() {
        let mut buf = TurnHistoryBuffer::with_cap(1);
        // Turn 0 (will be evicted once turn 1 completes).
        buf.start_turn(turn("turn:t/0"));
        buf.push_item(msg("old"));
        buf.finish_turn(TurnStatus::Completed, 0, None);
        // Turn 1 completes — cap = 1, so turn 0 is evicted.
        buf.start_turn(turn("turn:t/1"));
        buf.push_item(msg("kept"));
        buf.finish_turn(TurnStatus::Completed, 1, None);
        // Active turn 2 with a live item.
        buf.start_turn(turn("turn:t/2"));
        buf.push_item(msg("live"));

        let ids: Vec<String> = buf.all_item_ids().iter().map(|i| i.0.to_string()).collect();
        assert_eq!(
            ids,
            vec!["kept".to_owned(), "live".to_owned()],
            "evicted 'old' is skipped; resident completed then active in order"
        );
        assert_eq!(buf.lazy_unloaded_count(), 1);
    }

    #[test]
    fn replace_with_compaction_swaps_history() {
        let mut buf = TurnHistoryBuffer::with_cap(2);
        for n in 0..3 {
            buf.start_turn(turn(&format!("turn:t/{n}")));
            buf.push_item(msg(&format!("i{n}")));
            buf.finish_turn(TurnStatus::Completed, 0, None);
        }
        assert!(buf.lazy_unloaded_count() > 0);

        buf.replace_with_compaction(
            TurnId(Arc::from("turn:t/compaction")),
            vec![msg("marker"), msg("summary")],
        );
        assert!(!buf.has_active());
        assert_eq!(buf.completed_len(), 1, "single synthetic turn");
        assert_eq!(buf.item_count(), 2);
        assert_eq!(buf.lazy_unloaded_count(), 0, "eviction counter reset");
        let ids: Vec<String> = buf.all_item_ids().iter().map(|i| i.0.to_string()).collect();
        assert_eq!(ids, vec!["marker".to_owned(), "summary".to_owned()]);
    }

    #[test]
    fn push_without_active_is_dropped() {
        let mut buf = TurnHistoryBuffer::new();
        buf.push_item(msg("orphan"));
        assert_eq!(buf.item_count(), 0, "orphan item dropped, no panic");
    }
}

// Rust guideline compliant 2026-02-21

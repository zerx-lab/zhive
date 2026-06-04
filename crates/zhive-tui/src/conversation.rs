//! The conversation state the UI renders, folded purely from engine events.
//!
//! The engine is the single source of truth: it echoes the user's own input
//! back as an `events/item_appended` (see `zhive-core` `lifecycle`), so the TUI
//! never inserts items optimistically — it only reduces [`EngineNotification`]s
//! into [`Conversation`]. Turns are keyed by [`TurnId`]; an item that arrives
//! for an unseen turn lazily creates it, which keeps the reducer robust to the
//! inherent race between an RPC reply and the broadcast events it triggers.

use zhive_proto::domain::{Item, ThreadId, ToolCallStatus, TurnId};

use crate::protocol::EngineNotification;

/// Lifecycle state of a single turn as observed from the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnLifecycle {
    /// The turn is running.
    InProgress,
    /// The turn finished normally.
    Completed,
    /// The turn failed; carries the engine's message.
    Failed {
        /// Human-readable failure message.
        message: String,
    },
    /// The turn was interrupted (cancel / session abort).
    Interrupted,
}

/// One turn's transcript: a stable id, a status, and its appended items.
#[derive(Debug, Clone)]
pub struct TurnView {
    /// The engine-allocated turn id.
    pub id: TurnId,
    /// Current lifecycle state.
    pub status: TurnLifecycle,
    /// Items appended to the turn, in arrival order.
    pub items: Vec<Item>,
}

impl TurnView {
    fn new(id: TurnId) -> Self {
        Self {
            id,
            status: TurnLifecycle::InProgress,
            items: Vec::new(),
        }
    }

    /// Replaces an item with the same id, or appends it if new.
    fn upsert(&mut self, item: Item) {
        let id = item.id();
        if let Some(slot) = self.items.iter_mut().find(|existing| existing.id() == id) {
            *slot = item;
        } else {
            self.items.push(item);
        }
    }
}

/// Lifecycle status of a spawned subagent's child session.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SubagentStatus {
    /// The subagent's child turn is still running.
    Running,
    /// The subagent finished; `has_final` records whether it returned a message.
    Completed {
        /// Whether the subagent produced a final message for the parent.
        has_final: bool,
    },
    /// The subagent's child turn failed.
    Failed,
}

/// A subagent spawned by the conversation: its own child-thread transcript
/// plus enough state to render the inline live-summary row in the parent flow.
///
/// Mirrors opencode's model — a subagent is an independent child session, the
/// parent flow shows a one-line summary, and the full child transcript is kept
/// here for an optional drill-in view.
#[derive(Debug, Clone)]
pub struct SubagentView {
    /// The child thread the subagent runs in.
    pub child_thread_id: ThreadId,
    /// The subagent definition name, if the agent was named.
    pub agent_type: Option<String>,
    /// The task description, if the spawn provided one.
    pub description: Option<String>,
    /// The child session's turns (same shape as the parent's).
    pub turns: Vec<TurnView>,
    /// Lifecycle status.
    pub status: SubagentStatus,
}

impl SubagentView {
    fn new(
        child_thread_id: ThreadId,
        agent_type: Option<String>,
        description: Option<String>,
    ) -> Self {
        Self {
            child_thread_id,
            agent_type,
            description,
            turns: Vec::new(),
            status: SubagentStatus::Running,
        }
    }

    fn turn_mut(&mut self, id: &TurnId) -> &mut TurnView {
        let pos = if let Some(pos) = self.turns.iter().position(|t| &t.id == id) {
            pos
        } else {
            self.turns.push(TurnView::new(id.clone()));
            self.turns.len() - 1
        };
        &mut self.turns[pos]
    }

    /// Builds a completed subagent summary from a child thread's restored
    /// history.
    ///
    /// Used on resume to reconstruct the nested-subagent summaries from
    /// persistence: the flat `items` (in conversation order, from
    /// `thread/get_items`) are regrouped into completed turns by the turn id
    /// encoded in each item's id — the same scheme [`Conversation::load_history`]
    /// uses for the main transcript. The result is marked
    /// [`SubagentStatus::Completed`] because a persisted subagent has, by
    /// definition, already finished; `has_final` records whether its last turn
    /// produced an agent message for the parent.
    #[must_use]
    pub fn from_history(
        child_thread_id: ThreadId,
        agent_type: Option<String>,
        description: Option<String>,
        items: Vec<Item>,
    ) -> Self {
        let mut view = Self::new(child_thread_id, agent_type, description);
        for item in items {
            let turn_id = history_turn_id(&item)
                .unwrap_or_else(|| TurnId(std::sync::Arc::from("turn:resumed/subagent")));
            let turn = view.turn_mut(&turn_id);
            turn.status = TurnLifecycle::Completed;
            turn.upsert(item);
        }
        let has_final = view.turns.last().is_some_and(|t| {
            t.items
                .iter()
                .any(|i| matches!(i, Item::AgentMessage { .. }))
        });
        view.status = SubagentStatus::Completed { has_final };
        view
    }

    /// Folds a child-thread event into this subagent's own transcript.
    fn apply_thread_event(&mut self, event: &EngineNotification) {
        match event {
            EngineNotification::TurnStarted { turn_id, .. } => {
                self.turn_mut(turn_id).status = TurnLifecycle::InProgress;
            }
            EngineNotification::ItemAppended { turn_id, item, .. } => {
                self.turn_mut(turn_id).upsert((**item).clone());
            }
            EngineNotification::TurnCompleted { turn_id, .. } => {
                self.turn_mut(turn_id).status = TurnLifecycle::Completed;
            }
            EngineNotification::TurnFailed { turn_id, error, .. } => {
                self.turn_mut(turn_id).status = TurnLifecycle::Failed {
                    message: error.message.clone(),
                };
                self.status = SubagentStatus::Failed;
            }
            // ItemDelta and non-thread events are not surfaced in the summary.
            _ => {}
        }
    }

    /// Number of tool calls the subagent has issued (drives the `N toolcalls` summary).
    #[must_use]
    pub fn tool_call_count(&self) -> usize {
        self.turns
            .iter()
            .flat_map(|t| &t.items)
            .filter(|i| matches!(i, Item::ToolCall { .. }))
            .count()
    }

    /// The name of the tool currently running, if any (the last in-flight call).
    #[must_use]
    pub fn current_tool(&self) -> Option<&str> {
        let mut current = None;
        for turn in &self.turns {
            for item in &turn.items {
                if let Item::ToolCall {
                    name,
                    status: ToolCallStatus::Pending | ToolCallStatus::InProgress,
                    ..
                } = item
                {
                    current = Some(name.as_str());
                }
            }
        }
        current
    }
}

/// The whole rendered conversation: a thread, its turns, and live status.
#[derive(Debug, Clone)]
pub struct Conversation {
    /// The thread all turns belong to.
    pub thread_id: ThreadId,
    /// Turns in start order.
    pub turns: Vec<TurnView>,
    /// `true` while a turn is in flight (drives the busy spinner).
    pub busy: bool,
    /// Live partial text for the in-flight agent message (token streaming).
    ///
    /// Accumulated from `ItemDelta` events and shown as a provisional message;
    /// cleared the moment the finalised `Item::AgentMessage` is appended, so
    /// the real item supersedes it without duplication.
    pub streaming: String,
    /// Last transient error (rejected turn, transport hiccup) for the status bar.
    pub last_error: Option<String>,
    /// Subagents spawned within this conversation, in spawn order.
    ///
    /// Child-thread events are routed here by [`Conversation::apply`] so they
    /// render as nested summaries instead of leaking into the main transcript.
    pub subagents: Vec<SubagentView>,
}

impl Conversation {
    /// Creates an empty conversation bound to `thread_id`.
    #[must_use]
    pub fn new(thread_id: ThreadId) -> Self {
        Self {
            thread_id,
            turns: Vec::new(),
            busy: false,
            streaming: String::new(),
            last_error: None,
            subagents: Vec::new(),
        }
    }

    /// `true` when no turn has produced any item yet (drives the welcome view).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.turns.iter().all(|t| t.items.is_empty())
    }

    /// Total item count across all turns.
    #[must_use]
    pub fn item_count(&self) -> usize {
        self.turns.iter().map(|t| t.items.len()).sum()
    }

    /// Replaces the transcript with `items` restored from a resumed thread.
    ///
    /// History items arrive flat (in conversation order) from
    /// `thread/get_items`; this groups them back into completed turns by the
    /// turn id encoded in each [`Item`]'s id (`item:<turn_id>/<seq>`), so the
    /// resumed view renders with the same turn structure as a live session.
    /// Items whose id does not encode a turn fall into a trailing synthetic
    /// turn rather than being dropped. Existing turns and subagents are cleared.
    pub fn load_history(&mut self, items: Vec<Item>) {
        self.turns.clear();
        self.subagents.clear();
        self.streaming.clear();
        self.busy = false;
        for item in items {
            let turn_id = history_turn_id(&item)
                .unwrap_or_else(|| TurnId(std::sync::Arc::from("turn:resumed/history")));
            let turn = self.turn_mut(&turn_id);
            turn.status = TurnLifecycle::Completed;
            turn.upsert(item);
        }
    }

    /// Restores nested-subagent summaries from persisted child threads.
    ///
    /// Each tuple is `(child_thread_id, agent_type, description, items)` for one
    /// subagent that ran under this conversation's thread; the items are the
    /// child's restored history (from `thread/get_items`). Existing subagents
    /// are replaced. Call after [`Self::load_history`] when resuming a thread so
    /// the historical subagent rows render alongside the main transcript.
    ///
    /// A child whose history is empty is still recorded (as a completed,
    /// no-output subagent) so the parent flow shows that the spawn happened.
    pub fn restore_subagents(&mut self, subagents: Vec<SubagentView>) {
        self.subagents = subagents;
    }

    /// Folds a single engine notification into the conversation state.
    ///
    /// Events carrying a `thread_id` are routed by it: those for the main
    /// thread shape the main transcript; those for a registered subagent child
    /// thread fold into that [`SubagentView`] instead of leaking into the main
    /// turns. `SubagentStarted` / `SubagentCompleted` register and close
    /// subagents. Permission prompts and phase changes are handled by the
    /// caller and ignored here.
    pub fn apply(&mut self, event: &EngineNotification) {
        match event {
            EngineNotification::SubagentStarted {
                parent_thread_id,
                child_thread_id,
                agent_type,
                description,
            } => {
                if parent_thread_id == &self.thread_id
                    && self.subagent_mut(child_thread_id).is_none()
                {
                    self.subagents.push(SubagentView::new(
                        child_thread_id.clone(),
                        agent_type.clone(),
                        description.clone(),
                    ));
                }
            }
            EngineNotification::SubagentCompleted {
                child_thread_id,
                has_final,
                ..
            } => {
                if let Some(sub) = self.subagent_mut(child_thread_id) {
                    sub.status = SubagentStatus::Completed {
                        has_final: *has_final,
                    };
                }
            }
            // Thread-scoped events for a *child* thread fold into its subagent.
            // The guard fails for the main thread, so main-thread events fall
            // through to the main-transcript arms below.
            EngineNotification::TurnStarted { thread_id, .. }
            | EngineNotification::TurnCompleted { thread_id, .. }
            | EngineNotification::TurnFailed { thread_id, .. }
            | EngineNotification::ItemAppended { thread_id, .. }
            | EngineNotification::ItemDelta { thread_id, .. }
                if thread_id != &self.thread_id =>
            {
                if let Some(sub) = self.subagent_mut(thread_id) {
                    sub.apply_thread_event(event);
                }
                // Unknown thread (neither main nor a known subagent): ignored.
            }
            _ => self.apply_main_thread(event),
        }
    }

    /// Folds a main-thread event into the primary transcript.
    fn apply_main_thread(&mut self, event: &EngineNotification) {
        match event {
            EngineNotification::TurnStarted { turn_id, .. } => {
                self.turn_mut(turn_id).status = TurnLifecycle::InProgress;
                self.busy = true;
                self.streaming.clear();
                self.last_error = None;
            }
            EngineNotification::ItemAppended { turn_id, item, .. } => {
                // A finalised item supersedes any provisional streamed text.
                self.streaming.clear();
                self.turn_mut(turn_id).upsert((**item).clone());
            }
            EngineNotification::ItemDelta { delta, .. } => {
                self.streaming.push_str(delta);
                self.busy = true;
            }
            EngineNotification::TurnCompleted { turn_id, .. } => {
                self.turn_mut(turn_id).status = TurnLifecycle::Completed;
                self.busy = false;
                self.streaming.clear();
            }
            EngineNotification::TurnFailed { turn_id, error, .. } => {
                let message = error.message.clone();
                self.turn_mut(turn_id).status = TurnLifecycle::Failed {
                    message: message.clone(),
                };
                self.busy = false;
                self.streaming.clear();
                self.last_error = Some(message);
            }
            EngineNotification::TurnRejected { reason } => {
                self.busy = false;
                self.streaming.clear();
                self.last_error = Some(reason.clone());
            }
            EngineNotification::SessionAborted(_) => {
                if let Some(turn) = self
                    .turns
                    .iter_mut()
                    .rfind(|t| t.status == TurnLifecycle::InProgress)
                {
                    turn.status = TurnLifecycle::Interrupted;
                }
                self.busy = false;
                self.streaming.clear();
            }
            // Subagent lifecycle (handled in `apply`) and non-transcript events
            // (phase / permission / usage / unhandled) need no main-thread fold.
            _ => {}
        }
    }

    /// Returns the subagent owning child thread `child`, if registered.
    fn subagent_mut(&mut self, child: &ThreadId) -> Option<&mut SubagentView> {
        self.subagents
            .iter_mut()
            .find(|s| &s.child_thread_id == child)
    }

    /// Returns the turn with `id`, creating it (in-progress) if not yet seen.
    fn turn_mut(&mut self, id: &TurnId) -> &mut TurnView {
        let pos = if let Some(pos) = self.turns.iter().position(|t| &t.id == id) {
            pos
        } else {
            self.turns.push(TurnView::new(id.clone()));
            self.turns.len() - 1
        };
        &mut self.turns[pos]
    }
}

/// Extracts the owning turn id from an item's id (`item:<turn_id>/<seq>`).
///
/// Returns `None` when the id does not follow that convention (e.g. a
/// synthetic id), letting [`Conversation::load_history`] fall back gracefully.
fn history_turn_id(item: &Item) -> Option<TurnId> {
    let raw = item.id().0.as_ref();
    // Strip the `item:` prefix, then drop the trailing `/<seq>` segment to
    // recover the turn id (`turn:<thread>/<n>`), which itself contains slashes.
    let body = raw.strip_prefix("item:")?;
    let (turn, _seq) = body.rsplit_once('/')?;
    if turn.is_empty() {
        None
    } else {
        Some(TurnId(std::sync::Arc::from(turn)))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zhive_proto::domain::{ItemId, ToolKind, TurnError};

    use super::*;

    fn tid() -> ThreadId {
        ThreadId(Arc::from("thread:native/test"))
    }

    fn turn(n: &str) -> TurnId {
        TurnId(Arc::from(n))
    }

    fn agent_item(id: &str, text: &str) -> Item {
        Item::AgentMessage {
            id: ItemId(Arc::from(id)),
            text: text.to_owned(),
        }
    }

    #[test]
    fn full_turn_lifecycle_is_folded() {
        let mut conv = Conversation::new(tid());
        assert!(conv.is_empty());

        conv.apply(&EngineNotification::TurnStarted {
            thread_id: tid(),
            turn_id: turn("turn:1/0"),
        });
        assert!(conv.busy);

        conv.apply(&EngineNotification::ItemAppended {
            thread_id: tid(),
            turn_id: turn("turn:1/0"),
            item: Box::new(agent_item("item:1", "hi there")),
        });
        assert_eq!(conv.item_count(), 1);
        assert!(!conv.is_empty());

        conv.apply(&EngineNotification::TurnCompleted {
            thread_id: tid(),
            turn_id: turn("turn:1/0"),
        });
        assert!(!conv.busy);
        assert_eq!(conv.turns[0].status, TurnLifecycle::Completed);
    }

    #[test]
    fn item_with_same_id_is_replaced_not_duplicated() {
        let mut conv = Conversation::new(tid());
        let t = turn("turn:1/0");
        conv.apply(&EngineNotification::ItemAppended {
            thread_id: tid(),
            turn_id: t.clone(),
            item: Box::new(agent_item("item:x", "partial")),
        });
        conv.apply(&EngineNotification::ItemAppended {
            thread_id: tid(),
            turn_id: t,
            item: Box::new(agent_item("item:x", "final")),
        });
        assert_eq!(conv.item_count(), 1, "same id upserts in place");
        match &conv.turns[0].items[0] {
            Item::AgentMessage { text, .. } => assert_eq!(text, "final"),
            other => panic!("unexpected item {other:?}"),
        }
    }

    #[test]
    fn item_for_unseen_turn_lazily_creates_it() {
        let mut conv = Conversation::new(tid());
        conv.apply(&EngineNotification::ItemAppended {
            thread_id: tid(),
            turn_id: turn("turn:1/7"),
            item: Box::new(agent_item("item:a", "racey")),
        });
        assert_eq!(conv.turns.len(), 1);
        assert_eq!(conv.item_count(), 1);
    }

    #[test]
    fn turn_failure_records_message_and_clears_busy() {
        let mut conv = Conversation::new(tid());
        conv.busy = true;
        conv.apply(&EngineNotification::TurnFailed {
            thread_id: tid(),
            turn_id: turn("turn:1/0"),
            error: TurnError {
                message: "provider exploded".to_owned(),
                additional_details: None,
            },
        });
        assert!(!conv.busy);
        assert_eq!(conv.last_error.as_deref(), Some("provider exploded"));
    }

    #[test]
    fn streamed_deltas_accumulate_then_final_item_supersedes() {
        let mut conv = Conversation::new(tid());
        conv.apply(&EngineNotification::TurnStarted {
            thread_id: tid(),
            turn_id: turn("turn:1/0"),
        });
        conv.apply(&EngineNotification::ItemDelta {
            thread_id: tid(),
            turn_id: turn("turn:1/0"),
            delta: "hel".to_owned(),
        });
        conv.apply(&EngineNotification::ItemDelta {
            thread_id: tid(),
            turn_id: turn("turn:1/0"),
            delta: "lo".to_owned(),
        });
        assert_eq!(conv.streaming, "hello");
        assert!(conv.busy);

        conv.apply(&EngineNotification::ItemAppended {
            thread_id: tid(),
            turn_id: turn("turn:1/0"),
            item: Box::new(agent_item("item:a", "hello")),
        });
        assert!(conv.streaming.is_empty(), "final item clears the buffer");
        assert_eq!(conv.item_count(), 1, "no duplicate from the stream");
    }

    fn child_tid() -> ThreadId {
        ThreadId(Arc::from("thread:subagent/test/1"))
    }

    fn tool_call_item(id: &str, name: &str, status: ToolCallStatus) -> Item {
        Item::ToolCall {
            id: ItemId(Arc::from(id)),
            name: name.to_owned(),
            kind: ToolKind::default(),
            status,
            content: Vec::new(),
            locations: Vec::new(),
            raw_input: None,
            raw_output: None,
            provider_tool_call_id: None,
        }
    }

    #[test]
    fn subagent_child_events_route_to_subagent_not_main() {
        let mut conv = Conversation::new(tid());
        conv.apply(&EngineNotification::SubagentStarted {
            parent_thread_id: tid(),
            child_thread_id: child_tid(),
            agent_type: Some("researcher".to_owned()),
            description: Some("find it".to_owned()),
        });
        assert_eq!(conv.subagents.len(), 1);

        // A child-thread item lands in the subagent, never the main transcript.
        conv.apply(&EngineNotification::ItemAppended {
            thread_id: child_tid(),
            turn_id: turn("turn:subagent/test/1/0"),
            item: Box::new(agent_item("item:c1", "child thinking")),
        });
        assert!(
            conv.is_empty(),
            "child item must not leak into the main transcript"
        );
        assert_eq!(conv.subagents[0].turns.len(), 1);
        assert_eq!(conv.subagents[0].turns[0].items.len(), 1);

        conv.apply(&EngineNotification::SubagentCompleted {
            parent_thread_id: tid(),
            child_thread_id: child_tid(),
            has_final: true,
        });
        assert_eq!(
            conv.subagents[0].status,
            SubagentStatus::Completed { has_final: true }
        );
    }

    #[test]
    fn subagent_summary_counts_tool_calls_and_tracks_current() {
        let mut conv = Conversation::new(tid());
        conv.apply(&EngineNotification::SubagentStarted {
            parent_thread_id: tid(),
            child_thread_id: child_tid(),
            agent_type: None,
            description: None,
        });
        conv.apply(&EngineNotification::ItemAppended {
            thread_id: child_tid(),
            turn_id: turn("turn:subagent/test/1/0"),
            item: Box::new(tool_call_item(
                "item:t1",
                "bash",
                ToolCallStatus::InProgress,
            )),
        });
        let sub = &conv.subagents[0];
        assert_eq!(sub.tool_call_count(), 1);
        assert_eq!(sub.current_tool(), Some("bash"));
    }

    #[test]
    fn load_history_groups_items_by_encoded_turn_id() {
        let mut conv = Conversation::new(tid());
        // Two turns' worth of items arriving flat, in conversation order.
        let items = vec![
            agent_item("item:turn:test/0/0", "first turn user"),
            agent_item("item:turn:test/0/1", "first turn reply"),
            agent_item("item:turn:test/1/0", "second turn reply"),
        ];
        conv.load_history(items);
        assert_eq!(conv.turns.len(), 2, "items regroup into their two turns");
        assert_eq!(conv.turns[0].items.len(), 2);
        assert_eq!(conv.turns[1].items.len(), 1);
        assert!(
            conv.turns
                .iter()
                .all(|t| t.status == TurnLifecycle::Completed),
            "restored turns render as completed"
        );
        assert!(!conv.busy, "resumed view is not busy");
    }

    #[test]
    fn load_history_restores_tool_calls_into_their_turn() {
        // A resumed thread must bring back tool-call records, not just messages.
        let mut conv = Conversation::new(tid());
        conv.load_history(vec![
            agent_item("item:turn:test/0/0", "run ls"),
            tool_call_item("item:turn:test/0/1", "bash", ToolCallStatus::Completed),
            agent_item("item:turn:test/0/2", "here is the listing"),
        ]);
        assert_eq!(conv.turns.len(), 1, "all items regroup into the one turn");
        let items = &conv.turns[0].items;
        assert_eq!(items.len(), 3, "the tool call survives alongside messages");
        assert!(
            items.iter().any(|i| matches!(i, Item::ToolCall { .. })),
            "the restored turn keeps the tool-call record"
        );
    }

    #[test]
    fn load_history_falls_back_for_unparseable_ids() {
        let mut conv = Conversation::new(tid());
        conv.load_history(vec![agent_item("weird-id", "no turn encoded")]);
        assert_eq!(conv.item_count(), 1, "unparseable items are still kept");
    }

    #[test]
    fn restore_subagents_rebuilds_completed_summaries_from_history() {
        let mut conv = Conversation::new(tid());
        // Main history first (as resume does), then subagent restoration.
        conv.load_history(vec![agent_item("item:turn:test/0/0", "main reply")]);

        let child_items = vec![
            tool_call_item("item:turn:sub/0/0", "bash", ToolCallStatus::Completed),
            agent_item("item:turn:sub/0/1", "subagent done"),
        ];
        conv.restore_subagents(vec![SubagentView::from_history(
            child_tid(),
            Some("researcher".to_owned()),
            Some("dig in".to_owned()),
            child_items,
        )]);

        assert_eq!(conv.subagents.len(), 1);
        let sub = &conv.subagents[0];
        assert_eq!(sub.child_thread_id, child_tid());
        assert_eq!(sub.agent_type.as_deref(), Some("researcher"));
        // The child's two items regrouped into its single completed turn.
        assert_eq!(sub.turns.len(), 1);
        assert_eq!(sub.turns[0].status, TurnLifecycle::Completed);
        assert_eq!(sub.tool_call_count(), 1);
        // Restored subagent is Completed with a final agent message present.
        assert_eq!(sub.status, SubagentStatus::Completed { has_final: true });
        // The main transcript is untouched by the restoration.
        assert_eq!(conv.item_count(), 1);
    }

    #[test]
    fn restore_subagents_marks_empty_child_completed_without_final() {
        let mut conv = Conversation::new(tid());
        conv.restore_subagents(vec![SubagentView::from_history(
            child_tid(),
            None,
            None,
            Vec::new(),
        )]);
        assert_eq!(conv.subagents.len(), 1);
        assert_eq!(
            conv.subagents[0].status,
            SubagentStatus::Completed { has_final: false }
        );
        assert!(conv.subagents[0].turns.is_empty());
    }

    #[test]
    fn unknown_child_thread_event_is_ignored() {
        let mut conv = Conversation::new(tid());
        // No subagent registered for this child thread → event is dropped,
        // never folded into the main transcript.
        conv.apply(&EngineNotification::ItemAppended {
            thread_id: child_tid(),
            turn_id: turn("turn:subagent/test/1/0"),
            item: Box::new(agent_item("item:x", "orphan")),
        });
        assert!(conv.is_empty());
        assert!(conv.subagents.is_empty());
    }
}

// Rust guideline compliant 2026-02-21

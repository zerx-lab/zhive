//! The conversation state the UI renders, folded purely from engine events.
//!
//! The engine is the single source of truth: it echoes the user's own input
//! back as an `events/item_appended` (see `zhive-core` `lifecycle`), so the TUI
//! never inserts items optimistically — it only reduces [`EngineNotification`]s
//! into [`Conversation`]. Turns are keyed by [`TurnId`]; an item that arrives
//! for an unseen turn lazily creates it, which keeps the reducer robust to the
//! inherent race between an RPC reply and the broadcast events it triggers.

use zhive_proto::domain::{Item, ThreadId, ToolCallStatus, TurnId};
use zhive_proto::events::ItemDeltaKind;

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
    /// Accumulated from `ItemDelta` events with [`ItemDeltaKind::Text`] and
    /// shown as a provisional message; cleared the moment the finalised
    /// `Item::AgentMessage` is appended, so the real item supersedes it without
    /// duplication.
    pub streaming: String,
    /// Byte offset into `streaming` up to which text has been revealed to the
    /// renderer (the smooth-reveal cursor). Always a char boundary; reset to 0
    /// whenever the buffer is cleared via [`Conversation::clear_streaming`].
    revealed: usize,
    /// Live partial reasoning trace for the in-flight turn (token streaming).
    ///
    /// Accumulated from `ItemDelta` events with [`ItemDeltaKind::Reasoning`] and
    /// shown as a provisional thinking block; cleared alongside [`Self::streaming`]
    /// when the turn's items finalise.
    pub streaming_reasoning: String,
    /// Smooth-reveal cursor into [`Self::streaming_reasoning`] (see [`Self::revealed`]).
    revealed_reasoning: usize,
    /// Last transient error (rejected turn, transport hiccup) for the status bar.
    pub last_error: Option<String>,
    /// Subagents spawned within this conversation, in spawn order.
    ///
    /// Child-thread events are routed here by [`Conversation::apply`] so they
    /// render as nested summaries instead of leaking into the main transcript.
    pub subagents: Vec<SubagentView>,
}

/// Minimum chars revealed per tick (keeps slow streams visibly progressing).
///
/// Sized so the very start of a reply — when the backlog is still tiny and the
/// geometric term below rounds down to almost nothing — keeps pace with the
/// provider's token rate instead of crawling a few chars per tick (the "first
/// appearance" stutter). At the 50ms tick this is a ~200 chars/s floor.
const REVEAL_FLOOR: usize = 10;
/// Fraction of the remaining backlog revealed per tick (geometric drain).
///
/// Higher than a token-smoothing minimum so a burst that arrives mid-reply
/// (e.g. after a tool call) catches up within a few ticks rather than lagging
/// visibly behind the real stream.
const REVEAL_RATIO: f64 = 0.30;

/// Returns the already-revealed prefix of a streaming buffer.
///
/// Clamps defensively so a `revealed` cursor left past the buffer end — or
/// mid-multibyte — can never cause a slice panic.
fn revealed_prefix(buf: &str, revealed: usize) -> &str {
    let mut end = revealed.min(buf.len());
    while end > 0 && !buf.is_char_boundary(end) {
        end -= 1;
    }
    &buf[..end]
}

/// Advances a smooth-reveal cursor over `buf` by one adaptive step.
///
/// Reveals `max(REVEAL_FLOOR, ceil(backlog * REVEAL_RATIO))` chars so the
/// backlog drains geometrically. Returns the new cursor (a char boundary);
/// a no-op once caught up.
fn advance_cursor(buf: &str, revealed: usize) -> usize {
    // Defensive clamp first: a stale cursor must never index past the end or
    // land mid-multibyte.
    let mut revealed = revealed.min(buf.len());
    while revealed > 0 && !buf.is_char_boundary(revealed) {
        revealed -= 1;
    }
    if revealed >= buf.len() {
        return revealed;
    }
    let tail = &buf[revealed..];
    let backlog_chars = tail.chars().count();
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "reveal step is a small count; f64 round-trip is exact at these sizes"
    )]
    let step = ((backlog_chars as f64) * REVEAL_RATIO).ceil() as usize;
    let step = step.max(REVEAL_FLOOR).min(backlog_chars);
    let advance_bytes = tail
        .char_indices()
        .nth(step)
        .map_or(tail.len(), |(byte_off, _)| byte_off);
    revealed + advance_bytes
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
            revealed: 0,
            streaming_reasoning: String::new(),
            revealed_reasoning: 0,
            last_error: None,
            subagents: Vec::new(),
        }
    }

    /// Clears both streaming buffers (answer and reasoning) and their cursors.
    ///
    /// The single clear site keeps each cursor from ever stranding past a now
    /// shorter (or empty) buffer.
    pub(crate) fn clear_streaming(&mut self) {
        self.streaming = String::new();
        self.revealed = 0;
        self.streaming_reasoning = String::new();
        self.revealed_reasoning = 0;
    }

    /// Returns the already-revealed prefix of the live answer buffer.
    pub(crate) fn revealed_streaming(&self) -> &str {
        revealed_prefix(&self.streaming, self.revealed)
    }

    /// Returns the already-revealed prefix of the live reasoning buffer.
    pub(crate) fn revealed_reasoning(&self) -> &str {
        revealed_prefix(&self.streaming_reasoning, self.revealed_reasoning)
    }

    /// Advances both reveal cursors by an adaptive step (called once per tick).
    ///
    /// Reveals `max(REVEAL_FLOOR, ceil(backlog * REVEAL_RATIO))` chars per
    /// channel so each backlog drains geometrically — long replies finish fast,
    /// slow streams still progress. A no-op for a channel once caught up.
    pub(crate) fn advance_reveal(&mut self) {
        self.revealed = advance_cursor(&self.streaming, self.revealed);
        self.revealed_reasoning =
            advance_cursor(&self.streaming_reasoning, self.revealed_reasoning);
    }

    /// `true` when every received byte (both channels) has been revealed.
    pub(crate) fn reveal_caught_up(&self) -> bool {
        self.revealed >= self.streaming.len()
            && self.revealed_reasoning >= self.streaming_reasoning.len()
    }

    /// `true` when buffered text remains to be revealed (keeps ticks firing).
    pub(crate) fn is_revealing(&self) -> bool {
        !self.reveal_caught_up()
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

    /// Text of the most recent finalized assistant message, if any.
    ///
    /// Scans turns and items newest-first; mirrors opencode's "copy last
    /// assistant message" command. `None` when no agent message exists yet.
    pub(crate) fn last_agent_text(&self) -> Option<String> {
        self.turns
            .iter()
            .rev()
            .flat_map(|turn| turn.items.iter().rev())
            .find_map(|item| match item {
                Item::AgentMessage { text, .. } => Some(text.clone()),
                _ => None,
            })
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
        self.clear_streaming();
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
                self.clear_streaming();
                self.last_error = None;
            }
            EngineNotification::ItemAppended { turn_id, item, .. } => {
                // A finalised item supersedes any provisional streamed text.
                self.clear_streaming();
                self.turn_mut(turn_id).upsert((**item).clone());
            }
            EngineNotification::ItemDelta { delta, kind, .. } => {
                match kind {
                    ItemDeltaKind::Reasoning => self.streaming_reasoning.push_str(delta),
                    // Text and any future channel default to the answer body.
                    _ => self.streaming.push_str(delta),
                }
                self.busy = true;
            }
            EngineNotification::TurnCompleted { turn_id, .. } => {
                self.turn_mut(turn_id).status = TurnLifecycle::Completed;
                self.busy = false;
                self.clear_streaming();
            }
            EngineNotification::TurnFailed { turn_id, error, .. } => {
                let message = error.message.clone();
                self.turn_mut(turn_id).status = TurnLifecycle::Failed {
                    message: message.clone(),
                };
                self.busy = false;
                self.clear_streaming();
                self.last_error = Some(message);
            }
            EngineNotification::TurnRejected { reason } => {
                self.busy = false;
                self.clear_streaming();
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
                self.clear_streaming();
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

    fn user_item(id: &str, text: &str) -> Item {
        Item::UserMessage {
            id: ItemId(Arc::from(id)),
            content: vec![zhive_proto::domain::ItemContent::Text {
                text: text.to_owned(),
                annotations: None,
            }],
        }
    }

    // ---- smooth-reveal cursor (Plan C) ----

    #[test]
    fn reveal_progresses_then_catches_up() {
        let mut conv = Conversation::new(tid());
        conv.streaming = "hello world, this is a longer streamed message".to_owned();
        assert!(conv.is_revealing());
        let before = conv.revealed_streaming().len();
        conv.advance_reveal();
        assert!(conv.revealed_streaming().len() > before, "cursor advanced");
        for _ in 0..50 {
            conv.advance_reveal();
        }
        assert!(conv.reveal_caught_up());
        assert_eq!(conv.revealed_streaming(), conv.streaming);
    }

    #[test]
    fn reveal_clamps_stale_cursor_without_panic() {
        let mut conv = Conversation::new(tid());
        conv.streaming = "hi".to_owned();
        // Simulate a stale cursor left past a now-shorter buffer.
        conv.revealed = 999;
        assert_eq!(conv.revealed_streaming(), "hi");
        conv.advance_reveal(); // must not panic, must self-heal
        assert!(conv.reveal_caught_up());
    }

    #[test]
    fn reveal_respects_multibyte_char_boundaries() {
        let mut conv = Conversation::new(tid());
        conv.streaming = "你好世界一二三四五六七八九十".to_owned();
        // Stepping byte-agnostic must never slice mid-codepoint (would panic).
        for _ in 0..40 {
            conv.advance_reveal();
            let _ = conv.revealed_streaming();
        }
        assert_eq!(conv.revealed_streaming(), conv.streaming);
    }

    #[test]
    fn clear_streaming_resets_cursor() {
        let mut conv = Conversation::new(tid());
        conv.streaming = "abc".to_owned();
        conv.advance_reveal();
        conv.clear_streaming();
        assert!(conv.streaming.is_empty());
        assert_eq!(conv.revealed_streaming(), "");
        assert!(conv.reveal_caught_up() && !conv.is_revealing());
    }

    #[test]
    fn empty_buffer_is_caught_up() {
        let conv = Conversation::new(tid());
        assert!(conv.reveal_caught_up() && !conv.is_revealing());
        assert_eq!(conv.revealed_streaming(), "");
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
            kind: ItemDeltaKind::Text,
        });
        conv.apply(&EngineNotification::ItemDelta {
            thread_id: tid(),
            turn_id: turn("turn:1/0"),
            delta: "lo".to_owned(),
            kind: ItemDeltaKind::Text,
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

    #[test]
    fn reasoning_deltas_stream_separately_from_answer() {
        let mut conv = Conversation::new(tid());
        conv.apply(&EngineNotification::TurnStarted {
            thread_id: tid(),
            turn_id: turn("turn:1/0"),
        });
        // Reasoning and answer fragments interleave on the same notification but
        // accumulate into separate buffers, keyed by `kind`.
        conv.apply(&EngineNotification::ItemDelta {
            thread_id: tid(),
            turn_id: turn("turn:1/0"),
            delta: "let me think".to_owned(),
            kind: ItemDeltaKind::Reasoning,
        });
        conv.apply(&EngineNotification::ItemDelta {
            thread_id: tid(),
            turn_id: turn("turn:1/0"),
            delta: "the answer".to_owned(),
            kind: ItemDeltaKind::Text,
        });
        assert_eq!(conv.streaming_reasoning, "let me think");
        assert_eq!(conv.streaming, "the answer");

        // Finalising the turn clears both provisional buffers.
        conv.apply(&EngineNotification::ItemAppended {
            thread_id: tid(),
            turn_id: turn("turn:1/0"),
            item: Box::new(agent_item("item:a", "the answer")),
        });
        assert!(conv.streaming.is_empty());
        assert!(conv.streaming_reasoning.is_empty());
    }

    #[test]
    fn reveal_advances_both_channels() {
        let mut conv = Conversation::new(tid());
        conv.streaming = "answer body here".to_owned();
        conv.streaming_reasoning = "reasoning trace here".to_owned();
        assert!(conv.is_revealing());
        // Drain both backlogs; neither reveal may panic on multibyte clamps.
        for _ in 0..40 {
            conv.advance_reveal();
        }
        assert!(conv.reveal_caught_up());
        assert_eq!(conv.revealed_streaming(), conv.streaming);
        assert_eq!(conv.revealed_reasoning(), conv.streaming_reasoning);
    }

    fn child_tid() -> ThreadId {
        ThreadId(Arc::from("thread:subagent/test/1"))
    }

    fn tool_call_item(id: &str, name: &str, status: ToolCallStatus) -> Item {
        Item::ToolCall {
            id: ItemId(Arc::from(id)),
            name: name.to_owned(),
            title: None,
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
    fn load_history_keeps_user_messages_in_their_own_turn() {
        // Regression: a two-turn history where each turn's user message carries
        // a turn-encoded id (as the engine now persists). The user messages must
        // stay with their turn's reply, in order — not collapse to the front.
        let mut conv = Conversation::new(tid());
        let items = vec![
            user_item("item:turn:test/0/0", "delete first line"),
            agent_item("item:turn:test/0/1", "done with line 1"),
            user_item("item:turn:test/1/0", "delete second line"),
            agent_item("item:turn:test/1/1", "done with line 2"),
        ];
        conv.load_history(items);
        assert_eq!(conv.turns.len(), 2, "two distinct turns");
        // Turn 0 leads with its user message, then the reply.
        assert!(matches!(conv.turns[0].items[0], Item::UserMessage { .. }));
        assert!(matches!(conv.turns[0].items[1], Item::AgentMessage { .. }));
        // Turn 1 likewise — the second user message did NOT jump to the front.
        assert!(matches!(conv.turns[1].items[0], Item::UserMessage { .. }));
        assert!(matches!(conv.turns[1].items[1], Item::AgentMessage { .. }));
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

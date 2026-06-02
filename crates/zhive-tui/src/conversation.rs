//! The conversation state the UI renders, folded purely from engine events.
//!
//! The engine is the single source of truth: it echoes the user's own input
//! back as an `events/item_appended` (see `zhive-core` `lifecycle`), so the TUI
//! never inserts items optimistically — it only reduces [`EngineNotification`]s
//! into [`Conversation`]. Turns are keyed by [`TurnId`]; an item that arrives
//! for an unseen turn lazily creates it, which keeps the reducer robust to the
//! inherent race between an RPC reply and the broadcast events it triggers.

use zhive_proto::domain::{Item, ThreadId, TurnId};

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

    /// Folds a single engine notification into the conversation state.
    ///
    /// Conversation-shaping events (turn lifecycle, item append, abort) mutate
    /// state here; permission prompts and phase changes are handled by the
    /// caller and ignored.
    pub fn apply(&mut self, event: &EngineNotification) {
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
            EngineNotification::PhaseChanged { .. }
            | EngineNotification::PermissionRequested { .. }
            | EngineNotification::Unhandled { .. } => {}
        }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zhive_proto::domain::{ItemId, TurnError};

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
}

// Rust guideline compliant 2026-02-21

//! Three-queue injection buffer (Pi pattern).
//!
//! Three independent queues hold items between the producer
//! ([`crate::engine::Submission::EnqueueInjection`] /
//! [`crate::engine::Submission::EnqueueNextTurn`]) and the agent loop:
//!
//! * **Steer** — drained inside the active turn before every LLM call;
//!   `abort()` clears it and reports the cleared items.
//! * **Follow-up** — drained at turn boundary to keep the agent
//!   running; `abort()` clears it.
//! * **Next-turn** — drained at the start of the **next** turn;
//!   `abort()` **does not** clear it (recovery semantics).
//!
//! The Pi `drainQueuedMessages` failure semantics are reproduced by
//! [`InjectionQueues::drain`] + [`InjectionQueues::restore_front`]: if
//! the consumer fails to apply a drained batch, the items can be
//! returned to the front of the queue with the original order
//! preserved.

use std::collections::VecDeque;

use zhive_proto::domain::Item;
use zhive_proto::permission::StreamingBehavior;

/// Drain policy applied per queue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum QueueMode {
    /// Drain every queued item in one shot.
    All,
    /// Drain exactly one item per call (default for steer / follow-up).
    #[default]
    OneAtATime,
}

/// Identifies one of the three buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueTarget {
    /// Steering buffer (mid-turn).
    Steer,
    /// Follow-up buffer (post-turn).
    FollowUp,
    /// Next-turn buffer (cross-abort survivor).
    NextTurn,
}

impl TryFrom<StreamingBehavior> for QueueTarget {
    type Error = StreamingBehavior;

    /// `StreamingBehavior` is `#[non_exhaustive]`; unknown variants map
    /// to `Err(value)` so the engine can decide whether to drop the
    /// message or treat it as a protocol error.
    fn try_from(value: StreamingBehavior) -> Result<Self, Self::Error> {
        match value {
            StreamingBehavior::Steer => Ok(Self::Steer),
            StreamingBehavior::FollowUp => Ok(Self::FollowUp),
            other => Err(other),
        }
    }
}

/// Snapshot returned by [`InjectionQueues::abort`].
#[derive(Debug, Default)]
pub struct AbortSnapshot {
    /// Items the abort drained from the steer queue.
    pub cleared_steer: Vec<Item>,
    /// Items the abort drained from the follow-up queue.
    pub cleared_follow_up: Vec<Item>,
    /// Items remaining in the next-turn queue (preserved).
    pub next_turn_retained_count: u32,
}

/// Container for the three injection queues.
#[derive(Debug, Default)]
pub struct InjectionQueues {
    steer: VecDeque<Item>,
    steer_mode: QueueMode,
    follow_up: VecDeque<Item>,
    follow_up_mode: QueueMode,
    next_turn: VecDeque<Item>,
}

impl InjectionQueues {
    /// Builds empty queues using the default modes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the drain policy of a queue.
    ///
    /// Has no effect for [`QueueTarget::NextTurn`] (always `All`).
    pub fn set_mode(&mut self, target: QueueTarget, mode: QueueMode) {
        match target {
            QueueTarget::Steer => self.steer_mode = mode,
            QueueTarget::FollowUp => self.follow_up_mode = mode,
            QueueTarget::NextTurn => {}
        }
    }

    /// Pushes `items` to the back of the named queue.
    pub fn push_back(&mut self, target: QueueTarget, items: impl IntoIterator<Item = Item>) {
        let buf = self.buf_mut(target);
        for item in items {
            buf.push_back(item);
        }
    }

    /// Drains items from `target` according to its configured
    /// [`QueueMode`] (or [`QueueMode::All`] for `NextTurn`).
    pub fn drain(&mut self, target: QueueTarget) -> Vec<Item> {
        let mode = match target {
            QueueTarget::Steer => self.steer_mode,
            QueueTarget::FollowUp => self.follow_up_mode,
            QueueTarget::NextTurn => QueueMode::All,
        };
        let buf = self.buf_mut(target);
        match mode {
            QueueMode::All => buf.drain(..).collect(),
            QueueMode::OneAtATime => buf.pop_front().into_iter().collect(),
        }
    }

    /// Puts `items` back at the head of `target` in their original
    /// order (Pi `drainQueuedMessages` failure semantics).
    pub fn restore_front(&mut self, target: QueueTarget, items: Vec<Item>) {
        let buf = self.buf_mut(target);
        for item in items.into_iter().rev() {
            buf.push_front(item);
        }
    }

    /// Returns the current size of `target`.
    #[must_use]
    pub fn len(&self, target: QueueTarget) -> usize {
        match target {
            QueueTarget::Steer => self.steer.len(),
            QueueTarget::FollowUp => self.follow_up.len(),
            QueueTarget::NextTurn => self.next_turn.len(),
        }
    }

    /// Returns `true` when every queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steer.is_empty() && self.follow_up.is_empty() && self.next_turn.is_empty()
    }

    /// Clears the steer and follow-up queues and reports a snapshot.
    ///
    /// The next-turn queue is **preserved**; its retained count is
    /// echoed in [`AbortSnapshot::next_turn_retained_count`] so the
    /// caller can decide whether to resume.
    pub fn abort(&mut self) -> AbortSnapshot {
        let cleared_steer: Vec<Item> = self.steer.drain(..).collect();
        let cleared_follow_up: Vec<Item> = self.follow_up.drain(..).collect();
        AbortSnapshot {
            cleared_steer,
            cleared_follow_up,
            next_turn_retained_count: u32::try_from(self.next_turn.len()).unwrap_or(u32::MAX),
        }
    }

    fn buf_mut(&mut self, target: QueueTarget) -> &mut VecDeque<Item> {
        match target {
            QueueTarget::Steer => &mut self.steer,
            QueueTarget::FollowUp => &mut self.follow_up,
            QueueTarget::NextTurn => &mut self.next_turn,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use zhive_proto::domain::ItemId;

    fn msg(id: &str) -> Item {
        Item::AgentMessage {
            id: ItemId(Arc::from(id)),
            text: id.into(),
        }
    }

    #[test]
    fn drain_one_at_a_time_returns_single_item() {
        let mut q = InjectionQueues::new();
        q.push_back(QueueTarget::Steer, [msg("a"), msg("b"), msg("c")]);
        let drained = q.drain(QueueTarget::Steer);
        assert_eq!(drained.len(), 1);
        assert_eq!(q.len(QueueTarget::Steer), 2);
    }

    #[test]
    fn drain_all_mode_returns_every_item() {
        let mut q = InjectionQueues::new();
        q.set_mode(QueueTarget::FollowUp, QueueMode::All);
        q.push_back(QueueTarget::FollowUp, [msg("a"), msg("b")]);
        let drained = q.drain(QueueTarget::FollowUp);
        assert_eq!(drained.len(), 2);
    }

    #[test]
    fn restore_front_preserves_order() {
        let mut q = InjectionQueues::new();
        q.set_mode(QueueTarget::Steer, QueueMode::All);
        q.push_back(QueueTarget::Steer, [msg("a"), msg("b")]);
        let drained = q.drain(QueueTarget::Steer);
        q.restore_front(QueueTarget::Steer, drained);
        let drained_again = q.drain(QueueTarget::Steer);
        assert_eq!(drained_again.len(), 2);
        match &drained_again[0] {
            Item::AgentMessage { id, .. } => assert_eq!(&*id.0, "a"),
            other => panic!("expected a, got {other:?}"),
        }
    }

    #[test]
    fn abort_clears_steer_and_follow_up_but_preserves_next_turn() {
        let mut q = InjectionQueues::new();
        q.push_back(QueueTarget::Steer, [msg("s1")]);
        q.push_back(QueueTarget::FollowUp, [msg("f1"), msg("f2")]);
        q.push_back(QueueTarget::NextTurn, [msg("n1"), msg("n2"), msg("n3")]);
        let snap = q.abort();
        assert_eq!(snap.cleared_steer.len(), 1);
        assert_eq!(snap.cleared_follow_up.len(), 2);
        assert_eq!(snap.next_turn_retained_count, 3);
        assert_eq!(q.len(QueueTarget::Steer), 0);
        assert_eq!(q.len(QueueTarget::FollowUp), 0);
        assert_eq!(q.len(QueueTarget::NextTurn), 3);
    }
}

// Rust guideline compliant 2026-02-21

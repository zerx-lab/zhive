//! Per-thread buffer for session writes deferred outside the `Idle` phase.
//!
//! The engine must not let half-finished work reach durable storage while a
//! turn is mid-flight, a transcript is being compacted, a subagent is
//! running, or a provider call is being retried.  During any such non-`Idle`
//! [`EnginePhase`] the engine routes session writes (model changes, session
//! metadata, transcript items it wants buffered) into a
//! [`PendingSessionWrites`] buffer instead of emitting them immediately.  At
//! the next *save point* — a deliberate, consistent state the engine reaches
//! at the end of a turn or just before it falls back to `Idle` — the buffer is
//! flushed in FIFO order to the persistence writer.
//!
//! ## When to buffer
//!
//! [`PendingSessionWrites::push_or_enqueue`] decides per write:
//!
//! - phase is [`EnginePhase::Idle`] → the write is forwarded immediately, the
//!   buffer stays empty;
//! - phase is anything else → the write is appended to the buffer and flushed
//!   later at a save point.
//!
//! This keeps the buffer decision in one place so call sites never re-implement
//! the phase check.
//!
//! ## Flush semantics
//!
//! [`PendingSessionWrites::flush`] drains the buffer front-to-back, translating
//! each [`PendingSessionWrite`] into a [`StorageWriteOp`] and handing it to the
//! supplied enqueue closure.  If the closure reports an error the flush stops
//! at that write and returns; entries already drained are *not* restored (they
//! were successfully enqueued), matching the upstream agent-harness behaviour
//! where a flush failure does not roll back prior progress.
//!
//! Variants without a Phase 1 [`StorageWriteOp`] counterpart
//! ([`PendingSessionWrite::ThinkingLevelChange`], [`PendingSessionWrite::Label`],
//! [`PendingSessionWrite::Custom`], [`PendingSessionWrite::CustomMessage`]) are
//! logged at `warn` and skipped — the in-memory write is consumed but nothing
//! is persisted, because the JSONL `RolloutEntry` schema has no row for them
//! yet.  Their durable encoding lands in a later phase.

use std::collections::VecDeque;

use thiserror::Error;
use zhive_proto::domain::{Item, ItemId, ThreadId, TurnId};
use zhive_proto::hook::EnginePhase;

use crate::persistence::writer::StorageWriteOp;

/// A single session write that may be deferred past the active phase.
///
/// Produced by the engine whenever it would otherwise persist session state
/// while a turn / compaction / subagent / retry is in flight.  Only the first
/// four variants have a Phase 1 [`StorageWriteOp`] counterpart; the remainder
/// are accepted into the buffer but skipped (with a `warn`) at flush time until
/// their durable encoding lands.
#[derive(Debug)]
#[non_exhaustive]
pub enum PendingSessionWrite {
    /// A transcript item to append to the active turn.
    Item {
        /// Owning thread.
        thread_id: ThreadId,
        /// Containing turn.
        turn_id: TurnId,
        /// Monotonic per-turn sequence number.
        seq: i64,
        /// The item payload (boxed because [`Item`] is large).
        item: Box<Item>,
    },
    /// The active model for a thread changed (provider and/or model id).
    ModelChanged {
        /// Thread whose model changed.
        thread_id: ThreadId,
        /// New provider identifier (e.g. `"anthropic"`).
        provider: String,
        /// New model identifier (e.g. `"claude-opus-4"`).
        model_id: String,
    },
    /// Session-level metadata changed, such as the human-facing session name.
    SessionInfo {
        /// Thread whose metadata changed.
        thread_id: ThreadId,
        /// New session name, or `None` to clear it.
        name: Option<String>,
    },
    /// A leaf / completion marker for the thread's rollout.
    Leaf {
        /// Thread the leaf belongs to.
        thread_id: ThreadId,
    },
    /// The reasoning / thinking budget level changed (no durable row yet).
    ThinkingLevelChange {
        /// New thinking level.
        level: u8,
    },
    /// A label was attached to an item (no durable row yet).
    Label {
        /// Item the label targets.
        target_id: ItemId,
        /// Label text.
        label: String,
    },
    /// An extension-defined structured write (no durable row yet).
    Custom {
        /// Caller-defined discriminator.
        custom_type: String,
        /// Arbitrary structured payload.
        data: serde_json::Value,
    },
    /// An extension-defined message write (no durable row yet).
    CustomMessage {
        /// Caller-defined discriminator.
        custom_type: String,
        /// Free-form message body.
        content: String,
    },
}

/// FIFO buffer of [`PendingSessionWrite`]s for one thread.
///
/// Owned by `ThreadHandle` behind a `std::sync::Mutex`.  Writes accumulate
/// here while the engine phase is non-`Idle` and drain at the next save point.
/// The buffer is per-thread, so each thread's deferred writes flush
/// independently.
#[derive(Debug, Default)]
pub struct PendingSessionWrites {
    queue: VecDeque<PendingSessionWrite>,
}

impl PendingSessionWrites {
    /// Builds an empty buffer.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::state::PendingSessionWrites;
    ///
    /// let buf = PendingSessionWrites::new();
    /// assert!(buf.is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forwards `write` immediately when `Idle`, otherwise buffers it.
    ///
    /// When `phase` is [`EnginePhase::Idle`] the engine is at a consistent
    /// state, so `enqueue` is called right away and the buffer is untouched.
    /// For any other phase the write is appended to the buffer and will be
    /// drained by a later [`Self::flush`] at a save point.  Centralising the
    /// phase check here keeps call sites from re-implementing it.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::cell::RefCell;
    /// use std::sync::Arc;
    /// use zhive_core::state::{PendingSessionWrite, PendingSessionWrites};
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::hook::EnginePhase;
    ///
    /// let mut buf = PendingSessionWrites::new();
    /// let forwarded = RefCell::new(0_usize);
    /// let leaf = || PendingSessionWrite::Leaf {
    ///     thread_id: ThreadId(Arc::from("thread:native/x")),
    /// };
    ///
    /// // Idle: forwarded immediately, buffer stays empty.
    /// buf.push_or_enqueue(EnginePhase::Idle, leaf(), |_w| {
    ///     *forwarded.borrow_mut() += 1;
    /// });
    /// assert_eq!(*forwarded.borrow(), 1);
    /// assert!(buf.is_empty());
    ///
    /// // Turn: buffered, enqueue closure is not called.
    /// buf.push_or_enqueue(EnginePhase::Turn, leaf(), |_w| {
    ///     *forwarded.borrow_mut() += 1;
    /// });
    /// assert_eq!(*forwarded.borrow(), 1);
    /// assert_eq!(buf.len(), 1);
    /// ```
    pub fn push_or_enqueue(
        &mut self,
        phase: EnginePhase,
        write: PendingSessionWrite,
        enqueue: impl FnOnce(PendingSessionWrite),
    ) {
        if matches!(phase, EnginePhase::Idle) {
            enqueue(write);
        } else {
            self.queue.push_back(write);
        }
    }

    /// Drains buffered writes FIFO, translating each to a [`StorageWriteOp`].
    ///
    /// Each drained [`PendingSessionWrite`] is converted to its
    /// [`StorageWriteOp`] counterpart and passed to `enqueue`.  Variants
    /// without a Phase 1 durable encoding are logged at `warn` and skipped
    /// (they still leave the buffer).  Returns the number of writes that
    /// produced a [`StorageWriteOp`] handed to `enqueue` (skipped variants are
    /// not counted).
    ///
    /// On the first `enqueue` error the flush stops and returns the error;
    /// writes already drained are not restored, matching the upstream
    /// flush-does-not-roll-back behaviour.
    ///
    /// # Errors
    ///
    /// Returns [`PendingFlushError`] when `enqueue` reports a failure (e.g. the
    /// persistence channel is closed).  Remaining buffered writes stay queued
    /// for a later flush attempt.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::cell::RefCell;
    /// use std::sync::Arc;
    /// use zhive_core::state::{PendingSessionWrite, PendingSessionWrites};
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::hook::EnginePhase;
    ///
    /// let mut buf = PendingSessionWrites::new();
    /// buf.push_or_enqueue(
    ///     EnginePhase::Turn,
    ///     PendingSessionWrite::ModelChanged {
    ///         thread_id: ThreadId(Arc::from("thread:native/x")),
    ///         provider: "anthropic".into(),
    ///         model_id: "claude-opus-4".into(),
    ///     },
    ///     |_w| {},
    /// );
    ///
    /// let count = RefCell::new(0_usize);
    /// let flushed = buf
    ///     .flush(|_op| {
    ///         *count.borrow_mut() += 1;
    ///         Ok(())
    ///     })
    ///     .expect("flush succeeds");
    /// assert_eq!(flushed, 1);
    /// assert_eq!(*count.borrow(), 1);
    /// assert!(buf.is_empty());
    /// ```
    pub fn flush(
        &mut self,
        mut enqueue: impl FnMut(StorageWriteOp) -> Result<(), PendingFlushError>,
    ) -> Result<usize, PendingFlushError> {
        let mut flushed = 0_usize;
        while let Some(write) = self.queue.pop_front() {
            let Some(op) = pending_to_storage_op(write) else {
                continue;
            };
            enqueue(op)?;
            flushed += 1;
        }
        Ok(flushed)
    }

    /// Returns `true` when there are no buffered writes.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::state::PendingSessionWrites;
    ///
    /// assert!(PendingSessionWrites::new().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Returns the number of buffered writes.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::state::PendingSessionWrites;
    ///
    /// assert_eq!(PendingSessionWrites::new().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }
}

/// Translates a [`PendingSessionWrite`] to its [`StorageWriteOp`] counterpart.
///
/// Returns `None` for variants without a Phase 1 durable encoding; the caller
/// (flush) logs a `warn` for those and skips them.
fn pending_to_storage_op(write: PendingSessionWrite) -> Option<StorageWriteOp> {
    match write {
        PendingSessionWrite::Item {
            thread_id,
            turn_id,
            seq,
            item,
        } => Some(StorageWriteOp::ItemAppended {
            thread_id,
            turn_id,
            seq,
            item,
        }),
        PendingSessionWrite::ModelChanged {
            thread_id,
            provider,
            model_id,
        } => Some(StorageWriteOp::ModelChanged {
            thread_id,
            provider,
            model_id,
        }),
        PendingSessionWrite::SessionInfo { thread_id, name } => {
            // A `None` name means "clear the session name"; the writer only
            // has a set-name op, so a clear is represented as an empty string.
            Some(StorageWriteOp::SessionNameSet {
                thread_id,
                name: name.unwrap_or_default(),
            })
        }
        // Deferred-buffer flush is fire-and-forget: no awaiter, so `ack: None`.
        PendingSessionWrite::Leaf { thread_id } => Some(StorageWriteOp::Flush {
            thread_id,
            ack: None,
        }),
        other => {
            tracing::warn!(
                name: "zhive.state.pending_writes.skip_unsupported",
                write_kind = ?std::mem::discriminant(&other),
                "pending session write has no Phase 1 durable encoding; skipped"
            );
            None
        }
    }
}

/// Failure modes for [`PendingSessionWrites::flush`].
///
/// # Examples
///
/// ```
/// use zhive_core::state::PendingFlushError;
///
/// let err = PendingFlushError::EnqueueRejected("channel closed".into());
/// // Implements Display via thiserror.
/// let msg = err.to_string();
/// assert!(msg.contains("channel closed"));
/// ```
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PendingFlushError {
    /// The persistence channel rejected an enqueued write op.
    ///
    /// Carries a short human-readable cause (e.g. `"channel closed"`); the
    /// detailed failure is logged by the enqueue closure at its own site.
    #[error("pending session write flush rejected by persistence: {0}")]
    EnqueueRejected(String),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    fn turn(s: &str) -> TurnId {
        TurnId(Arc::from(s))
    }

    fn model_changed() -> PendingSessionWrite {
        PendingSessionWrite::ModelChanged {
            thread_id: tid("thread:native/x"),
            provider: "anthropic".into(),
            model_id: "claude-opus-4".into(),
        }
    }

    #[test]
    fn push_or_enqueue_idle_forwards_immediately() {
        let mut buf = PendingSessionWrites::new();
        let mut forwarded = 0_usize;
        buf.push_or_enqueue(EnginePhase::Idle, model_changed(), |_w| {
            forwarded += 1;
        });
        assert_eq!(forwarded, 1);
        assert!(buf.is_empty());
    }

    #[test]
    fn push_or_enqueue_turn_buffers() {
        let mut buf = PendingSessionWrites::new();
        let mut forwarded = 0_usize;
        buf.push_or_enqueue(EnginePhase::Turn, model_changed(), |_w| {
            forwarded += 1;
        });
        assert_eq!(forwarded, 0, "non-Idle must not forward immediately");
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn flush_emits_in_fifo_order_and_returns_count() {
        let mut buf = PendingSessionWrites::new();
        for phase in [EnginePhase::Turn, EnginePhase::Turn] {
            buf.push_or_enqueue(phase, model_changed(), |_w| {});
        }
        buf.push_or_enqueue(
            EnginePhase::Turn,
            PendingSessionWrite::Leaf {
                thread_id: tid("thread:native/x"),
            },
            |_w| {},
        );

        let mut kinds: Vec<&'static str> = Vec::new();
        let flushed = buf
            .flush(|op| {
                kinds.push(match op {
                    StorageWriteOp::ModelChanged { .. } => "model",
                    StorageWriteOp::Flush { .. } => "flush",
                    _ => "other",
                });
                Ok(())
            })
            .expect("flush ok");
        assert_eq!(flushed, 3);
        assert_eq!(kinds, vec!["model", "model", "flush"]);
        assert!(buf.is_empty());
    }

    #[test]
    fn flush_stops_at_first_error_without_restoring_drained() {
        let mut buf = PendingSessionWrites::new();
        for _ in 0..3 {
            buf.push_or_enqueue(EnginePhase::Turn, model_changed(), |_w| {});
        }

        let mut seen = 0_usize;
        let err = buf
            .flush(|_op| {
                seen += 1;
                if seen == 2 {
                    Err(PendingFlushError::EnqueueRejected("channel closed".into()))
                } else {
                    Ok(())
                }
            })
            .expect_err("second write must fail");
        assert!(matches!(err, PendingFlushError::EnqueueRejected(_)));
        assert_eq!(seen, 2, "flush stops at the failing write");
        // The first write was drained+enqueued, the second was drained but
        // failed and is NOT restored, so only the third remains buffered.
        assert_eq!(buf.len(), 1, "drained writes are not restored on failure");
    }

    #[test]
    fn flush_empty_buffer_returns_zero() {
        let mut buf = PendingSessionWrites::new();
        let flushed = buf.flush(|_op| Ok(())).expect("flush ok");
        assert_eq!(flushed, 0);
    }

    #[test]
    fn flush_skips_unsupported_variants_without_counting() {
        let mut buf = PendingSessionWrites::new();
        buf.push_or_enqueue(
            EnginePhase::Turn,
            PendingSessionWrite::ThinkingLevelChange { level: 2 },
            |_w| {},
        );
        buf.push_or_enqueue(
            EnginePhase::Turn,
            PendingSessionWrite::Item {
                thread_id: tid("thread:native/x"),
                turn_id: turn("turn:thread:native/x/0"),
                seq: 0,
                item: Box::new(Item::AgentMessage {
                    id: ItemId(Arc::from("item:x/0")),
                    text: "hi".into(),
                }),
            },
            |_w| {},
        );

        let mut emitted = 0_usize;
        let flushed = buf
            .flush(|_op| {
                emitted += 1;
                Ok(())
            })
            .expect("flush ok");
        assert_eq!(flushed, 1, "only the Item variant is durable in Phase 1");
        assert_eq!(emitted, 1);
        assert!(buf.is_empty(), "skipped variants still leave the buffer");
    }
}

// Rust guideline compliant 2026-02-21

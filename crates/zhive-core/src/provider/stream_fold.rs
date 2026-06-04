//! Stream-fold machinery: accumulates llmsdk [`StreamPart`]s into zhive [`Item`]s.
//!
//! This sub-module contains the pure (no async, no I/O) fold state machine
//! used by the engine to convert provider stream parts into finalized items.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::warn;

use llmsdk::language_model::StreamPart;

use zhive_proto::domain::{Item, ItemId, NoticeLevel, ToolCallStatus, ToolKind, TurnId};

// ============================================================
// Tool-input parsing
// ============================================================

/// Parses accumulated tool-input JSON into a value for a `tool_use` block.
///
/// Tool inputs are JSON **objects** by contract. Empty text (a tool called with
/// no arguments), malformed JSON, or a non-object value all fall back to the
/// empty object `{}` — never a string or null, both of which providers (e.g.
/// the Anthropic Messages API) reject with `tool_use.input: Input should be an
/// object`. Returning `{}` keeps prompt reconstruction round-trippable for
/// argument-less tool calls.
fn parse_tool_input(text: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value @ serde_json::Value::Object(_)) => value,
        _ => serde_json::json!({}),
    }
}

// ============================================================
// BlockKind
// ============================================================

/// Discriminates between text and reasoning blocks stored in `text_bufs`.
///
/// `TextStart` and `ReasoningStart` both insert into the shared `text_bufs`
/// map. This tag lets [`StreamFold::finish`] emit the correct [`Item`] variant
/// when a stream is truncated and the `*End` frame never arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BlockKind {
    /// A normal text block — finalises as [`Item::AgentMessage`].
    Text,
    /// A reasoning/thinking block — finalises as [`Item::Reasoning`].
    Reasoning,
}

// ============================================================
// BlockBuf
// ============================================================

/// Per-block accumulator used internally by [`StreamFold`].
#[derive(Debug)]
pub(super) struct BlockBuf {
    /// The zhive [`ItemId`] assigned to this provider block.
    pub(super) item_id: ItemId,
    /// Accumulated text or tool-input JSON.
    pub(super) text: String,
    /// Tool name, present only for tool-input blocks.
    pub(super) tool_name: Option<String>,
    /// Whether this buffer originated from `TextStart` or `ReasoningStart`.
    pub(super) kind: BlockKind,
}

// ============================================================
// StreamFold
// ============================================================

/// Folds a sequence of llmsdk [`StreamPart`]s into finalized zhive [`Item`]s.
///
/// Maintains per-block buffers keyed by the provider block id. On each
/// `*End` boundary (or on [`StreamFold::finish`] for a truncated stream) the
/// accumulated state is emitted as **exactly one** finalized item per block.
/// Delta parts accumulate text into the buffer but emit nothing, keeping the
/// persisted item stream free of partial-text duplicates.
///
/// ## Emission invariant
///
/// `StreamFold` emits exactly one finalized [`Item`] per provider block (on
/// its `*End` boundary, or on `finish()` for a truncated stream). The
/// [`ItemId`] is minted when the block opens and used for that single
/// emission, so the same id is never emitted twice and never collides with
/// the persistence primary key. Live token-by-token streaming, if needed, is
/// a separate chunk-notification concern layered by the engine, not part of
/// the persisted [`Item`] stream.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use llmsdk::language_model::StreamPart;
/// use zhive_proto::domain::{Item, TurnId};
/// use zhive_core::provider::StreamFold;
///
/// let turn_id = TurnId(Arc::from("turn:t/0/1"));
/// let mut fold = StreamFold::new(&turn_id);
///
/// let parts = vec![
///     StreamPart::TextStart { id: "b0".into(), provider_metadata: None },
///     StreamPart::TextDelta { id: "b0".into(), delta: "Hello".into(), provider_metadata: None },
///     StreamPart::TextDelta { id: "b0".into(), delta: ", world".into(), provider_metadata: None },
///     StreamPart::TextEnd   { id: "b0".into(), provider_metadata: None },
/// ];
///
/// let mut items: Vec<Item> = Vec::new();
/// for part in parts {
///     items.extend(fold.fold(part));
/// }
/// items.extend(fold.finish());
///
/// // TextStart/TextDelta emit nothing; TextEnd emits exactly one AgentMessage
/// // containing the full accumulated text.
/// let msg_items: Vec<_> = items.iter()
///     .filter(|i| matches!(i, Item::AgentMessage { .. }))
///     .collect();
/// assert_eq!(msg_items.len(), 1, "exactly one AgentMessage per text block");
///
/// // The single item carries the full concatenated text.
/// assert!(matches!(msg_items[0], Item::AgentMessage { text, .. } if text == "Hello, world"));
/// ```
#[derive(Debug)]
pub struct StreamFold {
    /// Prefix used to mint new [`ItemId`]s (e.g. the turn URI).
    prefix: Arc<str>,
    /// Sequence counter for generating unique item ids within a turn.
    seq: u64,
    /// Per-block text / reasoning accumulator.
    text_bufs: HashMap<String, BlockBuf>,
    /// Per-block tool-input accumulator.
    tool_bufs: HashMap<String, BlockBuf>,
    /// Provider tool-call ids already finalized via [`StreamPart::ToolInputEnd`].
    ///
    /// Providers (Anthropic, `OpenAI` Chat + Responses) emit a streamed input
    /// block (`ToolInputStart`/`Delta`/`End`) *and* a trailing atomic
    /// [`StreamPart::ToolCall`] sharing the same `tool_call_id`. The streamed
    /// `End` already emitted the item with the fully accumulated arguments; the
    /// atomic frame often carries an empty `{}` input. Tracking finalized ids
    /// lets the atomic branch suppress that duplicate instead of emitting a
    /// second `Item::ToolCall` with empty arguments.
    finalized_tool_ids: HashSet<String>,
    /// Final token usage, populated on [`StreamPart::Finish`].
    usage: Option<llmsdk::language_model::Usage>,
}

impl StreamFold {
    /// Create a new fold context scoped to `turn_id`.
    ///
    /// All [`ItemId`]s minted during this fold will embed the turn prefix so
    /// they sort coherently in JSONL / `SQLite`.
    #[must_use]
    pub fn new(turn_id: &TurnId) -> Self {
        Self {
            prefix: Arc::clone(&turn_id.0),
            seq: 0,
            text_bufs: HashMap::new(),
            tool_bufs: HashMap::new(),
            finalized_tool_ids: HashSet::new(),
            usage: None,
        }
    }

    /// Creates a fold that continues minting item ids from `start_seq`.
    ///
    /// A turn drives the provider in a loop, building a fresh fold per
    /// iteration, but item ids must stay unique across the **whole turn**. A
    /// per-iteration reset to 0 would collide — e.g. iteration 0's tool call and
    /// iteration 1's reply would both be `item:<turn>/0`, so the reply would
    /// overwrite the tool call on every id-keyed store. Callers thread
    /// [`StreamFold::next_seq`] from one iteration into the next.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_core::provider::StreamFold;
    /// use zhive_proto::domain::TurnId;
    ///
    /// let turn = TurnId(Arc::from("turn:t/0"));
    /// let fold = StreamFold::resuming_at(&turn, 5);
    /// assert_eq!(fold.next_seq(), 5);
    /// ```
    #[must_use]
    pub fn resuming_at(turn_id: &TurnId, start_seq: u64) -> Self {
        let mut fold = Self::new(turn_id);
        fold.seq = start_seq;
        fold
    }

    /// Returns the next item-id sequence number this fold will mint.
    ///
    /// Thread this into [`StreamFold::resuming_at`] for the next turn iteration
    /// so item ids stay unique across the whole turn.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_core::provider::StreamFold;
    /// use zhive_proto::domain::TurnId;
    ///
    /// let turn = TurnId(Arc::from("turn:t/0"));
    /// let fold = StreamFold::new(&turn);
    /// assert_eq!(fold.next_seq(), 0);
    /// ```
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.seq
    }

    /// Token-usage summary, available after a [`StreamPart::Finish`] is folded.
    #[must_use]
    pub fn usage(&self) -> Option<&llmsdk::language_model::Usage> {
        self.usage.as_ref()
    }

    /// Fold one [`StreamPart`] and return 0..1 [`Item`]s to emit.
    ///
    /// `*Start` and `*Delta` parts accumulate state into the internal buffer
    /// and return an empty `Vec`. Exactly one finalized [`Item`] is returned
    /// on each matching `*End` boundary.
    ///
    /// This method never panics. Malformed tool-input JSON falls back to
    /// `serde_json::Value::String(raw_json_text)` rather than returning an
    /// error or panicking (B10 §3 requirement).
    #[must_use]
    #[expect(
        clippy::too_many_lines,
        reason = "Match on 20+ StreamPart variants; splitting into sub-functions adds \
                  indirection without improving readability. All arms are short."
    )]
    pub fn fold(&mut self, part: StreamPart) -> Vec<Item> {
        match part {
            // ---- text start -----------------------------------------------
            StreamPart::TextStart { id, .. } => {
                let item_id = self.next_item_id();
                self.text_bufs.entry(id).or_insert(BlockBuf {
                    item_id,
                    text: String::new(),
                    tool_name: None,
                    kind: BlockKind::Text,
                });
                vec![]
            }

            // ---- reasoning start ------------------------------------------
            StreamPart::ReasoningStart { id, .. } => {
                let item_id = self.next_item_id();
                self.text_bufs.entry(id).or_insert(BlockBuf {
                    item_id,
                    text: String::new(),
                    tool_name: None,
                    kind: BlockKind::Reasoning,
                });
                vec![]
            }

            // ---- text delta -------------------------------------------------
            StreamPart::TextDelta { id, delta, .. } => {
                // Accumulate only; the finalized item is emitted on TextEnd.
                // Per the finalize-on-boundary invariant, no per-delta item is
                // emitted here to avoid duplicate ItemIds in persistence.
                if let Some(buf) = self.text_bufs.get_mut(&id) {
                    buf.text.push_str(&delta);
                } else {
                    // Delta arrived without a prior TextStart — open a buffer
                    // defensively so TextEnd can still emit a complete item.
                    let item_id = self.next_item_id();
                    self.text_bufs.insert(
                        id,
                        BlockBuf {
                            item_id,
                            text: delta,
                            tool_name: None,
                            kind: BlockKind::Text,
                        },
                    );
                }
                vec![]
            }
            StreamPart::TextEnd { id, .. } => {
                // Emit exactly ONE Item::AgentMessage carrying the full
                // accumulated text (finalize-on-boundary invariant).
                if let Some(buf) = self.text_bufs.remove(&id) {
                    vec![Item::AgentMessage {
                        id: buf.item_id,
                        text: buf.text,
                    }]
                } else {
                    vec![]
                }
            }

            // ---- reasoning delta/end ----------------------------------------
            StreamPart::ReasoningDelta { id, delta, .. } => {
                // Accumulate only; the finalized Reasoning item is emitted on
                // ReasoningEnd (finalize-on-boundary invariant).
                if let Some(buf) = self.text_bufs.get_mut(&id) {
                    buf.text.push_str(&delta);
                } else {
                    // Delta arrived without a prior ReasoningStart — open
                    // defensively so ReasoningEnd can still finalize.
                    let item_id = self.next_item_id();
                    self.text_bufs.insert(
                        id,
                        BlockBuf {
                            item_id,
                            text: delta,
                            tool_name: None,
                            kind: BlockKind::Reasoning,
                        },
                    );
                }
                vec![]
            }
            StreamPart::ReasoningEnd { id, .. } => {
                if let Some(buf) = self.text_bufs.remove(&id) {
                    vec![Item::Reasoning {
                        id: buf.item_id,
                        summary: vec![buf.text],
                    }]
                } else {
                    vec![]
                }
            }

            // ---- tool input (streamed JSON) ---------------------------------
            StreamPart::ToolInputStart { id, tool_name, .. } => {
                // Open the buffer and mint the ItemId; emit nothing.
                // Per the finalize-on-boundary invariant a ToolCall item is
                // only emitted once, on ToolInputEnd, carrying the full parsed
                // arguments. Emitting a Pending item here would require
                // reusing the same ItemId on ToolInputEnd, violating the
                // one-id-one-emission invariant and colliding with the
                // persistence primary key.
                //
                // Duplicate ToolInputStart frames (malformed stream / retry)
                // are silently ignored: the first buffer is preserved and no
                // extra seq slot is consumed.
                if self.tool_bufs.contains_key(&id) {
                    return vec![];
                }
                let item_id = self.next_item_id();
                self.tool_bufs.insert(
                    id,
                    BlockBuf {
                        item_id,
                        text: String::new(),
                        tool_name: Some(tool_name),
                        // `kind` is unused for tool bufs but the field is required.
                        kind: BlockKind::Text,
                    },
                );
                vec![]
            }
            StreamPart::ToolInputDelta { id, delta, .. } => {
                // Buffer the JSON fragment; don't emit a partial item to avoid
                // UI flicker from half-parsed arguments.
                if let Some(buf) = self.tool_bufs.get_mut(&id) {
                    buf.text.push_str(&delta);
                }
                vec![]
            }
            StreamPart::ToolInputEnd { id, .. } => {
                if let Some(buf) = self.tool_bufs.remove(&id) {
                    let raw_input = parse_tool_input(&buf.text);
                    // Remember this id so a trailing atomic ToolCall frame for
                    // the same logical call is suppressed instead of emitting a
                    // duplicate item with empty arguments.
                    self.finalized_tool_ids.insert(id.clone());
                    // Preserve the provider block id as `provider_tool_call_id`
                    // so the engine can round-trip it in Message::Tool without
                    // minting a synthetic replacement.
                    let provider_tool_call_id = Some(id);
                    vec![Item::ToolCall {
                        id: buf.item_id,
                        name: buf.tool_name.unwrap_or_default(),
                        kind: ToolKind::Other,
                        status: ToolCallStatus::InProgress,
                        content: vec![],
                        locations: vec![],
                        raw_input: Some(raw_input),
                        raw_output: None,
                        provider_tool_call_id,
                    }]
                } else {
                    vec![]
                }
            }

            // ---- atomic tool call (non-streamed input) ----------------------
            //
            // Spec invariant (inc1b): emit ONE Item::ToolCall per logical tool
            // call. Providers emit a streamed input block AND a trailing atomic
            // frame for the same `tool_call_id`. Three cases:
            //
            //   1. Already finalized via ToolInputEnd → suppress this atomic
            //      frame; the streamed item already carries the full arguments
            //      (the atomic `input` is frequently an empty `{}`).
            //   2. A buffer still exists (ToolInputStart seen, no End) → reuse
            //      its ItemId and consume the buffer so `finish()` can't flush
            //      it again.
            //   3. No prior buffer (pure-atomic path) → mint a fresh ItemId.
            StreamPart::ToolCall(call_part) => {
                if self.finalized_tool_ids.contains(&call_part.tool_call_id) {
                    return vec![];
                }
                let item_id = self
                    .tool_bufs
                    .remove(&call_part.tool_call_id)
                    .map(|buf| buf.item_id)
                    .unwrap_or_else(|| self.next_item_id());
                // Preserve the provider-assigned tool_call_id so the engine
                // can return it verbatim in Message::Tool.tool_call_id.
                let provider_tool_call_id = Some(call_part.tool_call_id.clone());
                vec![Item::ToolCall {
                    id: item_id,
                    name: call_part.tool_name,
                    kind: ToolKind::Other,
                    status: ToolCallStatus::InProgress,
                    content: vec![],
                    locations: vec![],
                    raw_input: Some(call_part.input),
                    raw_output: None,
                    provider_tool_call_id,
                }]
            }

            // ---- in-stream error → SystemNotice ----------------------------
            StreamPart::Error { error } => {
                let item_id = self.next_item_id();
                let msg = match &error {
                    serde_json::Value::String(s) => format!("provider error: {s}"),
                    other => format!("provider error: {other}"),
                };
                vec![Item::SystemNotice {
                    id: item_id,
                    level: NoticeLevel::Error,
                    message: msg,
                }]
            }

            // ---- terminal / usage frame ------------------------------------
            StreamPart::Finish { usage, .. } => {
                self.usage = Some(usage);
                // No Item emitted; usage is accessible via Self::usage().
                vec![]
            }

            // ---- provider-executed tool result (Phase 1: surface as notice) --
            //
            // B10: these parts were silently dropped (warn + empty vec).  Now
            // they surface as `Item::SystemNotice(Warn)` so the user and
            // upper layers can see that the provider sent something zhive does
            // not consume.  The notice is persisted and appears in the turn
            // history identically to an in-stream error notice.
            StreamPart::ToolResult(_) => {
                let item_id = self.next_item_id();
                warn!(
                    name: "provider.fold.tool_result_unhandled",
                    "provider sent a provider-executed tool result; \
                     zhive runs tools itself in Phase 1 — surfaced as SystemNotice"
                );
                vec![Item::SystemNotice {
                    id: item_id,
                    level: NoticeLevel::Warn,
                    message: "provider sent a provider-executed tool result, which zhive \
                              does not consume in Phase 1 (the engine runs tools itself); \
                              the result was surfaced as a notice instead of being applied"
                        .into(),
                }]
            }

            // ---- provider tool-approval request (Phase 1: surface as notice) -
            StreamPart::ToolApprovalRequest(_) => {
                let item_id = self.next_item_id();
                warn!(
                    name: "provider.fold.tool_approval_unhandled",
                    "provider sent a tool-approval request; \
                     zhive does not handle provider-side approvals in Phase 1 — surfaced as SystemNotice"
                );
                vec![Item::SystemNotice {
                    id: item_id,
                    level: NoticeLevel::Warn,
                    message: "provider sent a tool-approval request, which zhive does not \
                              handle in Phase 1; surfaced as a notice"
                        .into(),
                }]
            }

            // ---- stream meta / custom / raw (Phase 1: ignore) --------------
            StreamPart::StreamStart { .. }
            | StreamPart::ResponseMetadata(_)
            | StreamPart::Source(_)
            | StreamPart::File(_)
            | StreamPart::ReasoningFile { .. }
            | StreamPart::Custom { .. }
            | StreamPart::Raw { .. } => vec![],
        }
    }

    /// Flush any open blocks that did not receive an `*End` part.
    ///
    /// Providers that cut a stream short (e.g. on context limit) may omit
    /// terminal `*End` frames. Call `finish()` after the stream drains to
    /// recover any buffered content as finalized items.
    ///
    /// Each open block flushes to **exactly one** item (using its minted
    /// `ItemId`). A block that was already closed by its `*End` frame has
    /// already been removed from the buffer and will not be flushed again.
    ///
    /// - Text block → [`Item::AgentMessage`] with full accumulated text.
    /// - Reasoning block → [`Item::Reasoning`] with `summary: [full_text]`.
    /// - Tool-input block → [`Item::ToolCall`] (`InProgress`, parsed-so-far).
    #[must_use]
    pub fn finish(&mut self) -> Vec<Item> {
        let mut items = Vec::new();

        // Flush all open text/reasoning blocks.
        // Under the finalize-on-boundary model, *Delta parts never emit items,
        // so the buffer's item_id is used here for the first (and only) time.
        for (_id, buf) in self.text_bufs.drain() {
            match buf.kind {
                // Truncated text block → emit the full accumulated text.
                BlockKind::Text => items.push(Item::AgentMessage {
                    id: buf.item_id,
                    text: buf.text,
                }),
                // Truncated reasoning block → emit a Reasoning summary.
                BlockKind::Reasoning => items.push(Item::Reasoning {
                    id: buf.item_id,
                    summary: vec![buf.text],
                }),
            }
        }

        // Flush open tool-input blocks with parsed-so-far arguments.
        for (block_id, buf) in self.tool_bufs.drain() {
            let raw_input = parse_tool_input(&buf.text);
            // Preserve the provider block id as `provider_tool_call_id` so
            // the engine can use it when building the tool-result prompt.
            items.push(Item::ToolCall {
                id: buf.item_id,
                name: buf.tool_name.unwrap_or_default(),
                kind: ToolKind::Other,
                status: ToolCallStatus::InProgress,
                content: vec![],
                locations: vec![],
                raw_input: Some(raw_input),
                raw_output: None,
                provider_tool_call_id: Some(block_id),
            });
        }

        items
    }

    /// Mint the next sequential [`ItemId`] under this fold's turn prefix.
    fn next_item_id(&mut self) -> ItemId {
        let seq = self.seq;
        self.seq += 1;
        ItemId(Arc::from(format!("item:{}/{seq}", self.prefix).as_str()))
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn turn_id(s: &str) -> TurnId {
        TurnId(Arc::from(s))
    }

    // ---- item-id sequence carry across turn iterations ----

    /// Regression: a turn's later iterations must NOT reuse earlier item ids.
    ///
    /// Each iteration builds a fresh fold; resuming from the prior fold's
    /// `next_seq` keeps ids unique so a reply cannot overwrite an earlier
    /// tool call (`item:<turn>/0`) on an id-keyed store.
    #[test]
    fn resuming_at_keeps_item_ids_unique_across_iterations() {
        let turn = turn_id("turn:thread:native/x/1");

        // Iteration 0 mints the tool call's id at seq 0.
        let mut fold0 = StreamFold::new(&turn);
        let id0 = fold0.next_item_id();
        assert_eq!(id0.0.as_ref(), "item:turn:thread:native/x/1/0");

        // Iteration 1 resumes from where iteration 0 left off.
        let mut fold1 = StreamFold::resuming_at(&turn, fold0.next_seq());
        let id1 = fold1.next_item_id();
        assert_eq!(id1.0.as_ref(), "item:turn:thread:native/x/1/1");
        assert_ne!(id0, id1, "iteration 1 must not reuse iteration 0's id");
    }

    // ---- StreamFold text path ----

    /// A complete text block (Start + N Deltas + End) emits exactly ONE
    /// `AgentMessage` on `TextEnd` containing the full concatenated text.
    /// `TextStart` and `TextDelta` emit nothing (finalize-on-boundary).
    #[test]
    fn fold_text_block_emits_one_agent_message_on_end() {
        let mut fold = StreamFold::new(&turn_id("turn:t/1/0"));

        let parts = vec![
            StreamPart::TextStart {
                id: "b0".into(),
                provider_metadata: None,
            },
            StreamPart::TextDelta {
                id: "b0".into(),
                delta: "Hello".into(),
                provider_metadata: None,
            },
            StreamPart::TextDelta {
                id: "b0".into(),
                delta: ", world".into(),
                provider_metadata: None,
            },
            StreamPart::TextEnd {
                id: "b0".into(),
                provider_metadata: None,
            },
        ];

        let mut all_items: Vec<Item> = Vec::new();
        for p in parts {
            all_items.extend(fold.fold(p));
        }
        all_items.extend(fold.finish());

        // Exactly one AgentMessage; carries the full concatenated text.
        let msgs: Vec<_> = all_items
            .iter()
            .filter(|i| matches!(i, Item::AgentMessage { .. }))
            .collect();
        assert_eq!(msgs.len(), 1, "exactly one AgentMessage per text block");

        if let Item::AgentMessage { text, .. } = msgs[0] {
            assert_eq!(text, "Hello, world", "text must be the full accumulation");
        } else {
            unreachable!();
        }
    }

    /// `TextDelta` accumulates text but emits nothing.
    #[test]
    fn fold_text_delta_emits_nothing() {
        let mut fold = StreamFold::new(&turn_id("turn:t/1/1"));
        let _ = fold.fold(StreamPart::TextStart {
            id: "b0".into(),
            provider_metadata: None,
        });
        // Delta must not emit any item.
        let items = fold.fold(StreamPart::TextDelta {
            id: "b0".into(),
            delta: "chunk".into(),
            provider_metadata: None,
        });
        assert!(
            items.is_empty(),
            "TextDelta must emit nothing (finalize-on-boundary)"
        );
    }

    // ---- StreamFold reasoning path ----

    /// A reasoning block (Start + Delta + End) emits exactly ONE `Item::Reasoning`
    /// on `ReasoningEnd` with the full accumulated text as `summary[0]`.
    /// Neither `ReasoningStart` nor `ReasoningDelta` emit items.
    #[test]
    fn fold_reasoning_block_emits_one_reasoning_on_end() {
        let mut fold = StreamFold::new(&turn_id("turn:t/2/0"));

        let mut items = Vec::new();
        items.extend(fold.fold(StreamPart::ReasoningStart {
            id: "r0".into(),
            provider_metadata: None,
        }));
        items.extend(fold.fold(StreamPart::ReasoningDelta {
            id: "r0".into(),
            delta: "think".into(),
            provider_metadata: None,
        }));
        items.extend(fold.fold(StreamPart::ReasoningDelta {
            id: "r0".into(),
            delta: "ing".into(),
            provider_metadata: None,
        }));
        items.extend(fold.fold(StreamPart::ReasoningEnd {
            id: "r0".into(),
            provider_metadata: None,
        }));

        // No AgentThought items; exactly one Reasoning.
        let thoughts: Vec<_> = items
            .iter()
            .filter(|i| matches!(i, Item::AgentThought { .. }))
            .collect();
        let reasonings: Vec<_> = items
            .iter()
            .filter(|i| matches!(i, Item::Reasoning { .. }))
            .collect();

        assert_eq!(
            thoughts.len(),
            0,
            "ReasoningDelta must emit no AgentThought"
        );
        assert_eq!(reasonings.len(), 1, "exactly one Reasoning on ReasoningEnd");

        if let Item::Reasoning { summary, .. } = &reasonings[0] {
            assert_eq!(
                summary,
                &["thinking".to_string()],
                "Reasoning summary must carry full accumulated text"
            );
        }
    }

    // ---- StreamFold tool-call path (streamed JSON) ----

    /// A streamed tool-input block (Start + Deltas + End) emits exactly ONE
    /// `ToolCall(InProgress)` on `ToolInputEnd`. `ToolInputStart` emits nothing.
    #[test]
    fn fold_tool_input_stream_emits_one_in_progress_on_end() {
        let mut fold = StreamFold::new(&turn_id("turn:t/3/0"));

        let mut items = Vec::new();
        items.extend(fold.fold(StreamPart::ToolInputStart {
            id: "tc0".into(),
            tool_name: "read_file".into(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        }));
        items.extend(fold.fold(StreamPart::ToolInputDelta {
            id: "tc0".into(),
            delta: r#"{"path":"#.into(),
            provider_metadata: None,
        }));
        items.extend(fold.fold(StreamPart::ToolInputDelta {
            id: "tc0".into(),
            delta: r#""foo.rs"}"#.into(),
            provider_metadata: None,
        }));
        items.extend(fold.fold(StreamPart::ToolInputEnd {
            id: "tc0".into(),
            provider_metadata: None,
        }));

        let tool_calls: Vec<_> = items
            .iter()
            .filter(|i| matches!(i, Item::ToolCall { .. }))
            .collect();
        // Exactly ONE ToolCall (InProgress) on End — no Pending on Start.
        assert_eq!(
            tool_calls.len(),
            1,
            "exactly one ToolCall per streamed block"
        );

        if let Item::ToolCall {
            status, raw_input, ..
        } = &tool_calls[0]
        {
            assert_eq!(*status, ToolCallStatus::InProgress);
            let input = raw_input.as_ref().expect("raw_input populated on End");
            assert_eq!(input["path"], "foo.rs");
        }
    }

    #[test]
    fn fold_tool_call_atomic_emits_in_progress() {
        use llmsdk::ToolCallPart;
        let mut fold = StreamFold::new(&turn_id("turn:t/4/0"));
        let call = ToolCallPart {
            tool_call_id: "tc1".into(),
            tool_name: "bash".into(),
            input: serde_json::json!({"cmd": "ls"}),
            provider_executed: None,
            dynamic: None,
            provider_options: None,
        };
        let items = fold.fold(StreamPart::ToolCall(call));
        assert_eq!(items.len(), 1);
        if let Item::ToolCall {
            name,
            status,
            raw_input,
            ..
        } = &items[0]
        {
            assert_eq!(name, "bash");
            assert_eq!(*status, ToolCallStatus::InProgress);
            assert_eq!(raw_input.as_ref().unwrap()["cmd"], "ls");
        } else {
            panic!("expected ToolCall");
        }
    }

    /// `ToolInputStart` followed by an atomic `ToolCall` for the **same**
    /// `tool_call_id` must emit exactly ONE `Item::ToolCall` total.
    ///
    /// The atomic `ToolCall` frame reuses the `ItemId` minted by
    /// `ToolInputStart` and removes the open buffer so that `finish()` does
    /// not flush it a second time. This guards the 'exactly one Item per block'
    /// invariant and prevents a collision in the persistence `items.id PRIMARY
    /// KEY` (increment 5).
    #[test]
    fn fold_tool_input_start_then_atomic_tool_call_emits_exactly_one() {
        use llmsdk::ToolCallPart;
        let mut fold = StreamFold::new(&turn_id("turn:t/12/0"));

        // ToolInputStart opens the buffer; emits nothing.
        let start_items = fold.fold(StreamPart::ToolInputStart {
            id: "tc1".into(),
            tool_name: "bash".into(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        });
        assert!(
            start_items.is_empty(),
            "ToolInputStart must emit nothing (finalize-on-boundary)"
        );

        // Atomic ToolCall for the same id — provider switches mid-stream.
        let call = ToolCallPart {
            tool_call_id: "tc1".into(),
            tool_name: "bash".into(),
            input: serde_json::json!({"cmd": "echo hi"}),
            provider_executed: None,
            dynamic: None,
            provider_options: None,
        };
        let call_items = fold.fold(StreamPart::ToolCall(call));

        // Exactly one ToolCall from the atomic frame.
        assert_eq!(
            call_items.len(),
            1,
            "atomic ToolCall must emit exactly one item"
        );
        if let Item::ToolCall {
            status, raw_input, ..
        } = &call_items[0]
        {
            assert_eq!(*status, ToolCallStatus::InProgress);
            assert_eq!(raw_input.as_ref().unwrap()["cmd"], "echo hi");
        } else {
            panic!("expected ToolCall item");
        }

        // finish() must NOT emit a second ToolCall for the same id.
        let finish_items = fold.finish();
        let tool_calls: Vec<_> = finish_items
            .iter()
            .filter(|i| matches!(i, Item::ToolCall { .. }))
            .collect();
        assert_eq!(
            tool_calls.len(),
            0,
            "finish() must not emit a second ToolCall after the buffer was consumed"
        );
    }

    /// Regression: the real provider sequence is
    /// `ToolInputStart → ToolInputDelta → ToolInputEnd → ToolCall(input={})`.
    /// Anthropic and `OpenAI` both stream the input *and* send a trailing atomic
    /// `ToolCall` whose `input` is an empty object. The streamed `End` already
    /// emitted the item with the full arguments, so the atomic frame must be
    /// suppressed — otherwise a second `Item::ToolCall` with empty arguments is
    /// dispatched and tools like `grep` fail with "`pattern` must be a string".
    #[test]
    fn fold_streamed_then_atomic_tool_call_does_not_duplicate_with_empty_args() {
        use llmsdk::ToolCallPart;
        let mut fold = StreamFold::new(&turn_id("turn:t/13/0"));

        let _ = fold.fold(StreamPart::ToolInputStart {
            id: "tc1".into(),
            tool_name: "grep".into(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        });
        let _ = fold.fold(StreamPart::ToolInputDelta {
            id: "tc1".into(),
            delta: r#"{"pattern":"hello"}"#.into(),
            provider_metadata: None,
        });
        let end_items = fold.fold(StreamPart::ToolInputEnd {
            id: "tc1".into(),
            provider_metadata: None,
        });
        assert_eq!(end_items.len(), 1, "ToolInputEnd emits the single item");

        // Trailing atomic frame for the same id, carrying empty input.
        let call = ToolCallPart {
            tool_call_id: "tc1".into(),
            tool_name: "grep".into(),
            input: serde_json::json!({}),
            provider_executed: None,
            dynamic: None,
            provider_options: None,
        };
        let call_items = fold.fold(StreamPart::ToolCall(call));
        assert!(
            call_items.is_empty(),
            "atomic frame for an already-finalized id must be suppressed"
        );

        // The single emitted item carries the streamed arguments, not `{}`.
        if let Item::ToolCall { raw_input, .. } = &end_items[0] {
            assert_eq!(raw_input.as_ref().unwrap()["pattern"], "hello");
        } else {
            panic!("expected ToolCall item from ToolInputEnd");
        }

        assert!(
            fold.finish().is_empty(),
            "finish() must not flush anything for a finalized id"
        );
    }

    #[test]
    fn fold_malformed_tool_json_does_not_panic() {
        let mut fold = StreamFold::new(&turn_id("turn:t/5/0"));
        let _ = fold.fold(StreamPart::ToolInputStart {
            id: "tc2".into(),
            tool_name: "explode".into(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        });
        let _ = fold.fold(StreamPart::ToolInputDelta {
            id: "tc2".into(),
            delta: "NOT VALID JSON {{{".into(),
            provider_metadata: None,
        });
        let items = fold.fold(StreamPart::ToolInputEnd {
            id: "tc2".into(),
            provider_metadata: None,
        });
        // Must not panic; malformed tool input falls back to an empty object
        // `{}` (NOT a string) so providers that require an object accept it.
        assert!(
            !items.is_empty(),
            "should still emit a ToolCall with fallback input"
        );
        if let Item::ToolCall { raw_input, .. } = &items[0] {
            assert_eq!(
                raw_input.as_ref(),
                Some(&serde_json::json!({})),
                "malformed tool input must fall back to an empty object, not a string"
            );
        }
    }

    /// A tool called with NO arguments (empty streamed input) must finalize
    /// with `raw_input = {}` — an object — not an empty string. This is the
    /// real-world Anthropic case: `tool_use.input: Input should be an object`.
    #[test]
    fn fold_empty_tool_input_falls_back_to_empty_object() {
        let turn_id = TurnId(Arc::from("turn:test/empty-input"));
        let mut fold = StreamFold::new(&turn_id);
        let _ = fold.fold(StreamPart::ToolInputStart {
            id: "tc-empty".into(),
            tool_name: "echo".into(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        });
        // No ToolInputDelta at all (argument-less call) → buffer text is "".
        let items = fold.fold(StreamPart::ToolInputEnd {
            id: "tc-empty".into(),
            provider_metadata: None,
        });
        let Some(Item::ToolCall { raw_input, .. }) = items.first() else {
            panic!("expected a ToolCall item");
        };
        assert_eq!(
            raw_input.as_ref(),
            Some(&serde_json::json!({})),
            "argument-less tool call must reconstruct as an empty object"
        );
    }

    // ---- B10: provider-executed tool parts surface as SystemNotice(Warn) ----

    /// `StreamPart::ToolResult` must produce a `SystemNotice(Warn)` item
    /// (B10) instead of being silently dropped.  The notice message must
    /// mention "provider-executed".
    #[test]
    fn fold_tool_result_emits_system_notice_warn() {
        use llmsdk::language_model::{ToolResult, ToolResultOutput};

        let mut fold = StreamFold::new(&turn_id("turn:t/b10/0"));
        let result_part = ToolResult {
            tool_call_id: "tc-x".into(),
            tool_name: "some_tool".into(),
            output: ToolResultOutput::Text {
                value: "result text".into(),
                provider_options: None,
            },
            preliminary: None,
            provider_metadata: None,
        };
        let items = fold.fold(StreamPart::ToolResult(result_part));
        assert_eq!(items.len(), 1, "ToolResult must emit exactly one item");
        match &items[0] {
            Item::SystemNotice { level, message, .. } => {
                assert_eq!(*level, NoticeLevel::Warn, "must be Warn, not Error");
                assert!(
                    message.contains("provider-executed"),
                    "message must mention provider-executed; got: {message}"
                );
            }
            other => panic!("expected SystemNotice(Warn), got {other:?}"),
        }
    }

    /// `StreamPart::ToolApprovalRequest` must produce a `SystemNotice(Warn)`
    /// item (B10) instead of being silently dropped.
    #[test]
    fn fold_tool_approval_request_emits_system_notice_warn() {
        use llmsdk::ToolCallPart;
        use llmsdk::language_model::ToolApprovalRequest;

        let mut fold = StreamFold::new(&turn_id("turn:t/b10/1"));
        let approval_part = ToolApprovalRequest {
            approval_id: "appr-1".into(),
            tool_call: ToolCallPart {
                tool_call_id: "tc-y".into(),
                tool_name: "dangerous_tool".into(),
                input: serde_json::json!({}),
                provider_executed: None,
                dynamic: None,
                provider_options: None,
            },
            provider_metadata: None,
        };
        let items = fold.fold(StreamPart::ToolApprovalRequest(approval_part));
        assert_eq!(
            items.len(),
            1,
            "ToolApprovalRequest must emit exactly one item"
        );
        match &items[0] {
            Item::SystemNotice { level, message, .. } => {
                assert_eq!(*level, NoticeLevel::Warn, "must be Warn, not Error");
                assert!(
                    message.contains("approval"),
                    "message must mention approval; got: {message}"
                );
            }
            other => panic!("expected SystemNotice(Warn), got {other:?}"),
        }
    }

    // ---- StreamFold error → SystemNotice ----

    #[test]
    fn fold_error_emits_system_notice() {
        let mut fold = StreamFold::new(&turn_id("turn:t/6/0"));
        let items = fold.fold(StreamPart::Error {
            error: serde_json::json!("rate limited"),
        });
        assert_eq!(items.len(), 1);
        match &items[0] {
            Item::SystemNotice { level, message, .. } => {
                assert_eq!(*level, NoticeLevel::Error);
                assert!(message.contains("rate limited"));
            }
            other => panic!("expected SystemNotice, got {other:?}"),
        }
    }

    // ---- StreamFold finish → usage accessor ----

    #[test]
    fn fold_finish_populates_usage_accessor() {
        let mut fold = StreamFold::new(&turn_id("turn:t/7/0"));
        assert!(fold.usage().is_none());

        let items = fold.fold(StreamPart::Finish {
            usage: llmsdk::language_model::Usage {
                input_tokens: llmsdk::InputTokenUsage {
                    total: Some(100),
                    ..Default::default()
                },
                output_tokens: llmsdk::OutputTokenUsage {
                    total: Some(50),
                    ..Default::default()
                },
                raw: None,
            },
            finish_reason: llmsdk::language_model::FinishReason::new(
                llmsdk::language_model::FinishReasonKind::Stop,
            ),
            provider_metadata: None,
        });
        assert!(items.is_empty(), "Finish emits no Item");
        assert!(fold.usage().is_some());
        assert_eq!(fold.usage().unwrap().input_tokens.total, Some(100));
    }

    // ---- StreamFold multiple interleaved blocks ----

    /// Two blocks open simultaneously; each closes independently and emits
    /// exactly one finalized item. No `AgentThought` items are produced.
    #[test]
    fn fold_interleaved_text_and_reasoning_blocks() {
        let mut fold = StreamFold::new(&turn_id("turn:t/8/0"));
        let mut all = Vec::new();

        // Open two blocks simultaneously.
        all.extend(fold.fold(StreamPart::TextStart {
            id: "txt".into(),
            provider_metadata: None,
        }));
        all.extend(fold.fold(StreamPart::ReasoningStart {
            id: "rsn".into(),
            provider_metadata: None,
        }));
        all.extend(fold.fold(StreamPart::TextDelta {
            id: "txt".into(),
            delta: "A".into(),
            provider_metadata: None,
        }));
        all.extend(fold.fold(StreamPart::ReasoningDelta {
            id: "rsn".into(),
            delta: "R".into(),
            provider_metadata: None,
        }));
        all.extend(fold.fold(StreamPart::TextEnd {
            id: "txt".into(),
            provider_metadata: None,
        }));
        all.extend(fold.fold(StreamPart::ReasoningEnd {
            id: "rsn".into(),
            provider_metadata: None,
        }));

        let msgs: Vec<_> = all
            .iter()
            .filter(|i| matches!(i, Item::AgentMessage { .. }))
            .collect();
        let reasonings: Vec<_> = all
            .iter()
            .filter(|i| matches!(i, Item::Reasoning { .. }))
            .collect();
        let thoughts: Vec<_> = all
            .iter()
            .filter(|i| matches!(i, Item::AgentThought { .. }))
            .collect();

        // Exactly one finalized item per block; no incremental AgentThought.
        assert_eq!(msgs.len(), 1, "one AgentMessage from text block");
        assert_eq!(reasonings.len(), 1, "one Reasoning from reasoning block");
        assert_eq!(
            thoughts.len(),
            0,
            "no AgentThought items (finalize-on-boundary)"
        );

        // Text block carries the delta content.
        if let Item::AgentMessage { text, .. } = msgs[0] {
            assert_eq!(text, "A");
        }
        // Reasoning block carries the delta content.
        if let Item::Reasoning { summary, .. } = &reasonings[0] {
            assert_eq!(summary, &["R".to_string()]);
        }
    }

    // ---- StreamFold truncated stream: finish() must emit correct types ----

    /// `finish()` on a stream truncated mid-reasoning must produce
    /// `Item::Reasoning`, not `Item::AgentMessage`.
    ///
    /// This guards against the regression where both `TextStart` and
    /// `ReasoningStart` shared a type-untagged `BlockBuf`, causing `finish()`
    /// to always emit `AgentMessage`.
    #[test]
    fn fold_truncated_reasoning_finish_emits_reasoning() {
        let mut fold = StreamFold::new(&turn_id("turn:t/9/0"));

        // Open a reasoning block and deliver one delta — no ReasoningEnd.
        let _ = fold.fold(StreamPart::ReasoningStart {
            id: "r1".into(),
            provider_metadata: None,
        });
        let _ = fold.fold(StreamPart::ReasoningDelta {
            id: "r1".into(),
            delta: "partial thought".into(),
            provider_metadata: None,
        });

        // Stream is truncated; call finish() to flush open blocks.
        let items = fold.finish();

        // Must produce exactly one Item::Reasoning, never Item::AgentMessage.
        let reasonings: Vec<_> = items
            .iter()
            .filter(|i| matches!(i, Item::Reasoning { .. }))
            .collect();
        let agent_msgs: Vec<_> = items
            .iter()
            .filter(|i| matches!(i, Item::AgentMessage { .. }))
            .collect();

        assert_eq!(
            reasonings.len(),
            1,
            "truncated reasoning block must flush as Item::Reasoning"
        );
        assert_eq!(
            agent_msgs.len(),
            0,
            "truncated reasoning block must NOT produce Item::AgentMessage"
        );

        if let Item::Reasoning { summary, .. } = &reasonings[0] {
            assert_eq!(summary, &["partial thought".to_string()]);
        }
    }

    // ---- finish() truncated text block: must emit one AgentMessage ----

    /// A truncated text block (stream cut before `TextEnd`) is flushed by
    /// `finish()` as exactly one `AgentMessage` carrying all accumulated text.
    ///
    /// Under the finalize-on-boundary model `TextDelta` never emits items, so
    /// the buffer's `ItemId` is used for the first (and only) time in
    /// `finish()`.  There is no prior item to collide with.
    #[test]
    fn fold_truncated_text_finish_emits_one_agent_message() {
        let mut fold = StreamFold::new(&turn_id("turn:t/10/0"));

        // Open a text block, deliver two deltas — no TextEnd.
        let _ = fold.fold(StreamPart::TextStart {
            id: "t1".into(),
            provider_metadata: None,
        });
        let d1 = fold.fold(StreamPart::TextDelta {
            id: "t1".into(),
            delta: "partial".into(),
            provider_metadata: None,
        });
        let d2 = fold.fold(StreamPart::TextDelta {
            id: "t1".into(),
            delta: " text".into(),
            provider_metadata: None,
        });
        // Deltas must emit nothing under the new model.
        assert!(d1.is_empty(), "TextDelta must not emit items");
        assert!(d2.is_empty(), "TextDelta must not emit items");

        // Stream is truncated; finish() must emit exactly one AgentMessage.
        let finish_items = fold.finish();
        let agent_msgs: Vec<_> = finish_items
            .iter()
            .filter(|i| matches!(i, Item::AgentMessage { .. }))
            .collect();
        assert_eq!(
            agent_msgs.len(),
            1,
            "finish() must emit one AgentMessage for truncated text block"
        );
        if let Item::AgentMessage { text, .. } = agent_msgs[0] {
            assert_eq!(text, "partial text", "must carry full accumulated text");
        }
    }

    // ---- ToolInputStart idempotency under duplicate frames ----

    /// A duplicate `ToolInputStart` for the same block id must be silently
    /// ignored: the first buffer is preserved, no extra item is emitted, and
    /// `ToolInputEnd` emits the single `InProgress` item using the original
    /// `ItemId`.
    ///
    /// This guards against a provider re-emitting `ToolInputStart` on a
    /// malformed stream / retry causing an extra seq-slot waste.
    #[test]
    fn fold_tool_input_start_duplicate_is_idempotent() {
        let mut fold = StreamFold::new(&turn_id("turn:t/11/0"));

        // First ToolInputStart — must emit nothing (finalize-on-boundary).
        let items1 = fold.fold(StreamPart::ToolInputStart {
            id: "dup".into(),
            tool_name: "read_file".into(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        });
        assert!(
            items1.is_empty(),
            "ToolInputStart must emit nothing (finalize-on-boundary)"
        );

        // Duplicate ToolInputStart — must also emit nothing.
        let items2 = fold.fold(StreamPart::ToolInputStart {
            id: "dup".into(),
            tool_name: "read_file".into(),
            provider_executed: None,
            dynamic: None,
            title: None,
            provider_metadata: None,
        });
        assert!(
            items2.is_empty(),
            "duplicate ToolInputStart must emit nothing"
        );

        // Deliver delta and end — must emit exactly one InProgress ToolCall.
        let _ = fold.fold(StreamPart::ToolInputDelta {
            id: "dup".into(),
            delta: r#"{"x":1}"#.into(),
            provider_metadata: None,
        });
        let end_items = fold.fold(StreamPart::ToolInputEnd {
            id: "dup".into(),
            provider_metadata: None,
        });
        assert_eq!(
            end_items.len(),
            1,
            "ToolInputEnd emits exactly one InProgress ToolCall"
        );
        if let Item::ToolCall {
            status, raw_input, ..
        } = &end_items[0]
        {
            assert_eq!(*status, ToolCallStatus::InProgress);
            // Arguments must be parsed from the accumulated delta.
            let input = raw_input.as_ref().expect("raw_input populated on End");
            assert_eq!(input["x"], 1);
        } else {
            panic!("expected ToolCall on ToolInputEnd");
        }
    }
}

// Rust guideline compliant 2026-02-21

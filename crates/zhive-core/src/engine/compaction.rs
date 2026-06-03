//! Context compaction: replace a thread's transcript history with an
//! LLM-generated handoff summary.
//!
//! Compaction is a core-harness maintenance operation (mirrors codex
//! `core/src/compact.rs`). It runs in the [`EnginePhase::Compaction`] phase,
//! brackets the work with the [`HookEvent::PreCompact`] / [`HookEvent::PostCompact`]
//! hooks, and emits a `zhive.compaction` span.
//!
//! ## Durability
//!
//! When compaction completes it enqueues a [`crate::persistence::writer::StorageWriteOp::Compaction`]
//! that appends a [`crate::persistence::RolloutEntry::Compaction`] checkpoint
//! to the JSONL rollout and fsyncs it (save point). On a subsequent resume or
//! crash-rebuild the checkpoint is read and the prior items are replaced by the
//! `replacement` slice, so the provider never sees the full un-compacted
//! history.
//!
//! ## Token-based auto-compaction (B5)
//!
//! In addition to the item-count threshold
//! ([`AUTO_COMPACT_ITEM_THRESHOLD`]), the post-turn check in `turn.rs` also
//! triggers compaction when the estimated token count of the transcript meets
//! or exceeds a budget ([`default_compact_threshold`]). The budget is derived
//! from [`DEFAULT_CONTEXT_BUDGET_TOKENS`] × [`COMPACT_BUDGET_FRACTION`] when
//! the host does not supply an explicit token threshold via
//! [`super::EngineConfig::compact_token_threshold`]. The token count is taken
//! from the last provider-reported `input_tokens` (most accurate) or falls
//! back to [`estimate_tokens`] (character-count heuristic) when the provider
//! reported zero.

use std::sync::Arc;

use futures::StreamExt;
use tracing::Instrument as _;

use llmsdk::language_model::{CallOptions, Message, StreamPart, TextPart, UserPart};
use zhive_proto::domain::{Item, ItemId, ThreadId, TurnId};
use zhive_proto::hook::{CompactTrigger, HookEvent};

use super::event::EngineEvent;
use super::inner::EngineInner;
use super::submission::{CompactError, CompactReply};
use crate::persistence::writer::StorageWriteOp;
use crate::provider::{DynLanguageModel, ProviderError};
use crate::state::ThreadHandle;
use zhive_proto::hook::EnginePhase;
use zhive_proto::permission::{HookSpecificOutput, PermissionDecision};

/// Transcript length (item count) at or above which a completed turn triggers
/// automatic compaction.
///
/// This is a **fallback** safety valve; the primary trigger is token-budget
/// based (see [`default_compact_threshold`]). Item count is retained because
/// many-but-small-item sessions might not reach the token budget while still
/// growing unboundedly. Both thresholds use `||` so either one fires.
pub(in crate::engine) const AUTO_COMPACT_ITEM_THRESHOLD: usize = 50;

/// Fallback context-window budget (tokens) used when the model's real window
/// is unknown.
///
/// Conservative: modern large models have ≥ 128 k tokens, so 32 k leaves
/// ample headroom and avoids over-eager compaction on tool-heavy sessions.
/// The engine uses [`COMPACT_BUDGET_FRACTION`] of this value as the actual
/// trigger threshold, giving a default of 24 k tokens.
pub(in crate::engine) const DEFAULT_CONTEXT_BUDGET_TOKENS: u64 = 32_000;

/// Returns the default token-budget trigger when no explicit threshold is set.
///
/// Equals 75 % of [`DEFAULT_CONTEXT_BUDGET_TOKENS`] (24 000 tokens), leaving
/// 8 k overhead for the current turn's output before a provider 400. Uses
/// integer arithmetic (multiply by 3 / 4) to avoid floating-point lint errors.
/// This is the threshold used when
/// [`super::EngineConfig::compact_token_threshold`] is `None`.
pub(in crate::engine) fn default_compact_threshold() -> u64 {
    // 75 % via integer fractions: × 3 / 4, lint-clean.
    DEFAULT_CONTEXT_BUDGET_TOKENS * 3 / 4
}

/// Estimates the token count of a transcript from raw UTF-8 character counts.
///
/// Uses a conservative 4 characters per token heuristic. Accuracy is
/// intentionally low (CJK text is underestimated) — this is a *fallback*
/// used only when the provider has not yet reported an `input_tokens` count.
/// When the provider does report usage, the caller uses that value instead.
pub(in crate::engine) fn estimate_tokens(items: &[Item]) -> u64 {
    /// Conservative chars-per-token divisor.
    ///
    /// English prose averages ~4 chars/token; CJK text is ~1–2 chars/token.
    /// Using 4 underestimates CJK, but the default budget (24 k) is already
    /// far below the real model window (128 k+) so a small underestimate
    /// does not materially raise the provider-400 risk.
    const CHARS_PER_TOKEN: u64 = 4;
    let bytes: usize = items.iter().map(item_text_len).sum();
    (bytes as u64) / CHARS_PER_TOKEN
}

/// Returns the total UTF-8 byte length of the human-readable text in `item`.
///
/// Conservative: only text content is counted. Binary payloads and structured
/// data are skipped because they do not contribute meaningfully to the
/// character-per-token estimate.
fn item_text_len(item: &Item) -> usize {
    match item {
        Item::UserMessage { content, .. } => content
            .iter()
            .map(|c| match c {
                zhive_proto::domain::ItemContent::Text { text, .. } => text.len(),
                _ => 0,
            })
            .sum(),
        Item::AgentMessage { text, .. } | Item::AgentThought { text, .. } => text.len(),
        Item::ToolCall { content, .. } => content
            .iter()
            .map(|c| match c {
                zhive_proto::domain::ItemToolCallContent::Content {
                    content: zhive_proto::domain::ItemContent::Text { text, .. },
                } => text.len(),
                zhive_proto::domain::ItemToolCallContent::Diff { new_text, .. } => new_text.len(),
                _ => 0,
            })
            .sum(),
        _ => 0,
    }
}

/// Prefix stamped on the summary item so UI / event consumers can tell a
/// compaction handoff apart from a normal agent message.
const SUMMARY_PREFIX: &str = "[context summary]\n";

/// Instruction handed to the model when summarising. Mirrors the handoff
/// structure used by codex `core/templates/compact/prompt.md`.
const SUMMARY_INSTRUCTION: &str = "\
You are performing a CONTEXT CHECKPOINT COMPACTION. Write a concise handoff \
summary for another assistant that will resume this task. Include: current \
progress and key decisions, important context / constraints / user \
preferences, what remains to be done, and any critical data or references \
needed to continue. Respond with the summary only.\n\n\
--- TRANSCRIPT ---\n";

impl EngineInner {
    /// Compacts the transcript of `thread_id` into a summary.
    ///
    /// Looks the thread up in the store, then runs [`Self::run_compaction`].
    ///
    /// # Errors
    ///
    /// * [`CompactError::ThreadNotFound`] — no such thread.
    /// * [`CompactError::EngineBusy`] — engine phase was not `Idle`.
    /// * [`CompactError::SummarizationFailed`] — the provider call failed.
    pub(in crate::engine) async fn compact(
        self: &Arc<Self>,
        thread_id: ThreadId,
        trigger: CompactTrigger,
    ) -> Result<CompactReply, CompactError> {
        let handle = self
            .threads()
            .get(&thread_id)
            .await
            .ok_or(CompactError::ThreadNotFound)?;
        self.run_compaction(&handle, thread_id, trigger).await
    }

    /// Core compaction routine shared by the manual entry point and the
    /// inline auto-trigger at the end of a turn.
    ///
    /// Requires the engine to be `Idle`; the `Idle → Compaction` CAS fails
    /// cleanly (returning [`CompactError::EngineBusy`]) if a turn is in
    /// flight, so compaction never races a live turn mutating the transcript.
    pub(in crate::engine) async fn run_compaction(
        self: &Arc<Self>,
        handle: &Arc<ThreadHandle>,
        thread_id: ThreadId,
        trigger: CompactTrigger,
    ) -> Result<CompactReply, CompactError> {
        // 1. Snapshot the transcript. Nothing to do on an empty thread.
        let snapshot: Vec<Item> = handle.items_snapshot().await;
        if snapshot.is_empty() {
            return Ok(CompactReply::NothingToCompact);
        }
        let entries = u32::try_from(snapshot.len()).unwrap_or(u32::MAX);

        // 2. Claim the engine phase. Compaction requires Idle.
        if let Err(err) = self.try_set_phase_atomic(EnginePhase::Idle, EnginePhase::Compaction) {
            return Err(CompactError::EngineBusy {
                current: err.actual(),
            });
        }
        let _ = self.events_tx().send(EngineEvent::PhaseChanged {
            thread_id: Some(thread_id.clone()),
            from: EnginePhase::Idle,
            to: EnginePhase::Compaction,
        });

        // 3. PreCompact hook — may block (FULL §exit-code contract).
        //    A hook that returns a blocking decision (continue_loop=false or
        //    Deny) aborts the compaction cleanly; the phase is rolled back.
        if let Err(blocked) = self
            .dispatch_compact_hook(true, &thread_id, trigger, entries)
            .await
        {
            self.leave_compaction(&thread_id);
            return Err(blocked);
        }

        // 4. Summarise via the provider, inside the `zhive.compaction` span.
        //    Use `.instrument()` (not `.enter()`) so the span is correctly
        //    attached across the await on a multi-thread runtime.
        let summary = summarize(self.provider(), &snapshot)
            .instrument(tracing::info_span!("zhive.compaction", "session.id" = %thread_id.0))
            .await;
        let summary = match summary {
            Ok(s) => s,
            Err(e) => {
                // Roll the phase back before surfacing the error.
                self.leave_compaction(&thread_id);
                return Err(CompactError::SummarizationFailed {
                    message: e.to_string(),
                });
            }
        };

        // 5. Replace the in-memory transcript with [marker, summary].
        //    A monotonic counter distinguishes repeated compactions on the same
        //    thread so JSONL and SQL ids do not collide.
        let seq = self.next_compaction_seq();
        let marker = Item::ContextCompaction {
            id: compaction_item_id(&thread_id, seq, "marker"),
        };
        let summary_item = Item::AgentMessage {
            id: compaction_item_id(&thread_id, seq, "summary"),
            text: format!("{SUMMARY_PREFIX}{summary}"),
        };
        let compaction_turn = TurnId(Arc::from(format!("{}::compaction-{seq}", thread_id.0)));
        let replacement = vec![marker.clone(), summary_item.clone()];
        handle
            .replace_history_with_compaction(compaction_turn.clone(), replacement.clone())
            .await;

        // Enqueue the durable compaction checkpoint BEFORE the phase rolls back
        // to Idle so the checkpoint is sequenced after all prior ItemAppended ops.
        // The writer fsyncs this entry as a save point so a crash after this
        // point recovers with the compacted history.
        let now_ts = {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs().try_into().unwrap_or(i64::MAX))
        };
        self.enqueue_storage_op(StorageWriteOp::Compaction {
            thread_id: thread_id.clone(),
            turn_id: compaction_turn.clone(),
            timestamp: now_ts,
            // Store summary without prefix for diagnostics; the prefix is only
            // for the in-memory AgentMessage item.
            summary: summary.clone(),
            replacement: replacement.iter().map(|i| Box::new(i.clone())).collect(),
            entries_compacted: entries,
        });

        // Broadcast for live observers (UI).
        for item in [marker, summary_item] {
            let _ = self.events_tx().send(EngineEvent::ItemAppended {
                thread_id: thread_id.clone(),
                turn_id: compaction_turn.clone(),
                item: Box::new(item),
            });
        }

        // 6. PostCompact hook (log-and-proceed; a block here cannot undo
        //    the completed compaction — dispatch_compact_hook logs and
        //    returns Ok regardless for the post case).
        let _ = self
            .dispatch_compact_hook(false, &thread_id, trigger, entries)
            .await;

        // 7. Compaction → Idle.
        self.leave_compaction(&thread_id);

        Ok(CompactReply::Compacted {
            entries_compacted: entries,
        })
    }

    /// Restores `Idle` from `Compaction` and broadcasts the `PhaseChanged`.
    fn leave_compaction(&self, thread_id: &ThreadId) {
        if let Err(err) = self.try_set_phase_atomic(EnginePhase::Compaction, EnginePhase::Idle) {
            tracing::error!(
                name: "zhive.engine.phase.compaction_rollback_failed",
                actual = ?err.actual(),
                "engine phase was not Compaction when finishing compaction; state machine drift"
            );
        } else {
            let _ = self.events_tx().send(EngineEvent::PhaseChanged {
                thread_id: Some(thread_id.clone()),
                from: EnginePhase::Compaction,
                to: EnginePhase::Idle,
            });
        }
    }

    /// Dispatches the `PreCompact` (`pre = true`) or `PostCompact`
    /// (`pre = false`) hook.
    ///
    /// For `PreCompact` (`pre = true`): if any hook output signals a blocking
    /// decision — `continue_loop == Some(false)` or
    /// `HookSpecificOutput::PreToolUse { permission_decision: Deny }` — the
    /// method returns `Err(CompactError::BlockedByHook { reason })` so the
    /// caller can abort the compaction before it begins. Hook dispatch errors
    /// and event-build failures are logged and treated as "proceed" to match
    /// the internal-maintenance semantics.
    ///
    /// For `PostCompact` (`pre = false`): outputs are inspected for the same
    /// block signals, but a block at this stage cannot undo the already-completed
    /// compaction, so `Ok(())` is returned regardless (logged at WARN).
    async fn dispatch_compact_hook(
        &self,
        pre: bool,
        thread_id: &ThreadId,
        trigger: CompactTrigger,
        entries: u32,
    ) -> Result<(), CompactError> {
        let (name, count_field) = if pre {
            ("PreCompact", "entriesCount")
        } else {
            ("PostCompact", "entriesCompacted")
        };
        let trigger_str = match trigger {
            CompactTrigger::Auto => "auto",
            // `Manual` and any future non-exhaustive variant default to manual.
            _ => "manual",
        };
        let event: Result<HookEvent, _> = serde_json::from_value(serde_json::json!({
            "hook_event_name": name,
            "sessionId": thread_id.0.as_ref(),
            "cwd": ".",
            "registeredBy": { "id": "zhive", "version": env!("CARGO_PKG_VERSION"), "source": "builtin" },
            "trigger": trigger_str,
            count_field: entries,
        }));
        let ev = match event {
            Ok(ev) => ev,
            Err(err) => {
                tracing::warn!(
                    name: "zhive.compaction.hook_build_failed",
                    hook = name,
                    error = %err,
                    "failed to build compaction hook event; skipping hook"
                );
                return Ok(());
            }
        };

        let outputs = match self.hook_host().dispatch(&ev).await {
            Ok(outputs) => outputs,
            Err(err) => {
                tracing::warn!(
                    name: "zhive.compaction.hook_failed",
                    hook = name,
                    error = %err,
                    "compaction hook dispatch failed; proceeding (maintenance action)"
                );
                return Ok(());
            }
        };

        // Inspect every output for a blocking decision.
        for output in &outputs {
            // `continue_loop = Some(false)` is the general "stop" signal.
            let loop_blocked = output.continue_loop == Some(false);
            // `PreToolUse { Deny }` may appear when the subprocess hook
            // dispatched a PreCompact event and synthesized a PreToolUse-typed
            // block (historic behaviour before the block_output fix).
            let deny_blocked = matches!(
                &output.hook_specific_output,
                Some(HookSpecificOutput::PreToolUse {
                    permission_decision: PermissionDecision::Deny,
                    ..
                })
            );

            if loop_blocked || deny_blocked {
                let reason = output.system_message.clone().or_else(|| {
                    if let Some(HookSpecificOutput::PreToolUse {
                        permission_decision_reason,
                        ..
                    }) = &output.hook_specific_output
                    {
                        permission_decision_reason.clone()
                    } else {
                        None
                    }
                });

                if pre {
                    tracing::info!(
                        name: "zhive.compaction.blocked_by_hook",
                        hook = name,
                        reason = ?reason,
                        "PreCompact hook blocked compaction; aborting"
                    );
                    return Err(CompactError::BlockedByHook { reason });
                }
                // PostCompact block cannot undo the compaction; log only.
                tracing::warn!(
                    name: "zhive.compaction.post_hook_blocked_too_late",
                    hook = name,
                    reason = ?reason,
                    "PostCompact hook signaled a block after compaction completed; ignored"
                );
            }
        }

        Ok(())
    }
}

/// Deterministic item id for a compaction-generated item.
///
/// Incorporates the monotonic `seq` counter so repeated compactions on the
/// same thread produce distinct ids in both the JSONL rollout and the SQL
/// index. The counter is process-scoped (starts at 1); cross-restart id
/// uniqueness is guaranteed by the `Compaction` rollout entry: on resume, all
/// prior compaction turns are discarded from memory, so there is no risk of a
/// counter restart emitting a duplicate that would collide with a live in-memory
/// item.
fn compaction_item_id(thread_id: &ThreadId, seq: u64, suffix: &str) -> ItemId {
    ItemId(Arc::from(format!(
        "{}::compaction-{seq}-{suffix}",
        thread_id.0
    )))
}

/// Renders `items` to plain text and asks `provider` for a handoff summary.
///
/// Collects every `TextDelta` from the streamed response into a single
/// string. Returns the trimmed summary text.
///
/// Shared with [`super::fork`], which reuses the same provider summarisation
/// path for the optional branch-summary step rather than building a parallel
/// one.
///
/// # Errors
///
/// Returns a [`ProviderError`] if the provider call fails or the stream
/// yields an error part.
pub(in crate::engine) async fn summarize(
    provider: &DynLanguageModel,
    items: &[Item],
) -> Result<String, ProviderError> {
    let mut transcript = String::new();
    for item in items {
        match item {
            Item::UserMessage { content, .. } => {
                let text: String = content
                    .iter()
                    .filter_map(|c| match c {
                        zhive_proto::domain::ItemContent::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                transcript.push_str("User: ");
                transcript.push_str(&text);
                transcript.push('\n');
            }
            Item::AgentMessage { text, .. } => {
                transcript.push_str("Assistant: ");
                transcript.push_str(text);
                transcript.push('\n');
            }
            _ => {}
        }
    }

    let prompt = vec![Message::User {
        content: vec![UserPart::Text(TextPart {
            text: format!("{SUMMARY_INSTRUCTION}{transcript}"),
            provider_options: None,
        })],
        provider_options: None,
    }];
    let opts = CallOptions {
        prompt,
        ..Default::default()
    };

    let mut result = provider
        .do_stream(opts)
        .await
        .map_err(ProviderError::from)?;
    let mut summary = String::new();
    while let Some(part) = result.stream.next().await {
        if let StreamPart::TextDelta { delta, .. } = part.map_err(ProviderError::from)? {
            summary.push_str(&delta);
        }
    }
    Ok(summary.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ScriptedModel;
    use tokio::sync::broadcast;
    use zhive_proto::domain::ItemContent;

    fn inner_with_summary(text: &str) -> Arc<EngineInner> {
        let (tx, _rx) = broadcast::channel::<EngineEvent>(64);
        let model = ScriptedModel::new(
            "t",
            "m",
            vec![
                StreamPart::TextStart {
                    id: "b".into(),
                    provider_metadata: None,
                },
                StreamPart::TextDelta {
                    id: "b".into(),
                    delta: text.into(),
                    provider_metadata: None,
                },
                StreamPart::TextEnd {
                    id: "b".into(),
                    provider_metadata: None,
                },
            ],
        )
        .into_dyn();
        Arc::new(EngineInner::new(tx, model))
    }

    fn user(id: &str, t: &str) -> Item {
        Item::UserMessage {
            id: ItemId(Arc::from(id)),
            content: vec![ItemContent::Text {
                text: t.to_owned(),
                annotations: None,
            }],
        }
    }

    #[tokio::test]
    async fn run_compaction_replaces_history_with_marker_and_summary() {
        let inner = inner_with_summary("SUMMARY");
        let tid = ThreadId(Arc::from("thread:native/c1"));
        let handle = inner.threads().get_or_init(&tid).await;
        handle
            .start_turn_buffer(TurnId(Arc::from("turn:c1/0")), 0)
            .await;
        handle.push_item(user("u0", "hello")).await;
        handle.push_item(user("u1", "world")).await;

        let reply = inner
            .run_compaction(&handle, tid.clone(), CompactTrigger::Manual)
            .await
            .expect("compaction must succeed");
        assert!(matches!(
            reply,
            CompactReply::Compacted {
                entries_compacted: 2
            }
        ));

        let tail: Vec<Item> = handle.items_snapshot().await;
        assert_eq!(
            tail.len(),
            2,
            "history must be replaced by [marker, summary]"
        );
        assert!(matches!(tail[0], Item::ContextCompaction { .. }));
        match &tail[1] {
            Item::AgentMessage { text, .. } => {
                assert!(
                    text.starts_with(SUMMARY_PREFIX),
                    "summary must carry the prefix"
                );
                assert!(
                    text.contains("SUMMARY"),
                    "summary must contain the model output"
                );
            }
            other => panic!("expected AgentMessage summary, got {other:?}"),
        }
        // Phase must have rolled back to Idle.
        assert_eq!(*inner.phase_lock(), EnginePhase::Idle);
    }

    #[tokio::test]
    async fn run_compaction_nothing_when_empty() {
        let inner = inner_with_summary("x");
        let tid = ThreadId(Arc::from("thread:native/c2"));
        let handle = inner.threads().get_or_init(&tid).await;
        let reply = inner
            .run_compaction(&handle, tid, CompactTrigger::Manual)
            .await
            .expect("ok");
        assert!(matches!(reply, CompactReply::NothingToCompact));
        assert_eq!(*inner.phase_lock(), EnginePhase::Idle);
    }

    #[tokio::test]
    async fn run_compaction_busy_when_not_idle() {
        let inner = inner_with_summary("x");
        let tid = ThreadId(Arc::from("thread:native/c3"));
        let handle = inner.threads().get_or_init(&tid).await;
        handle
            .start_turn_buffer(TurnId(Arc::from("turn:c3/0")), 0)
            .await;
        handle.push_item(user("u0", "hi")).await;
        // Simulate a live turn holding the phase.
        inner
            .try_set_phase_atomic(EnginePhase::Idle, EnginePhase::Turn)
            .expect("to Turn");

        let err = inner
            .run_compaction(&handle, tid, CompactTrigger::Manual)
            .await
            .expect_err("compaction must refuse when busy");
        assert!(matches!(err, CompactError::EngineBusy { .. }));
        // History must be untouched.
        assert_eq!(handle.item_count().await, 1);
    }

    // ----------------------------------------------------------------
    // B5: token estimation unit tests
    // ----------------------------------------------------------------

    /// `estimate_tokens` counts UTF-8 bytes of text content and divides by 4.
    #[test]
    fn estimate_tokens_counts_text_bytes() {
        // "hello world" = 11 bytes / 4 = 2 tokens (integer division).
        let item = Item::UserMessage {
            id: ItemId(Arc::from("u0")),
            content: vec![ItemContent::Text {
                text: "hello world".into(),
                annotations: None,
            }],
        };
        assert_eq!(estimate_tokens(&[item]), 2);
    }

    /// Empty transcript yields zero tokens.
    #[test]
    fn estimate_tokens_empty_transcript() {
        assert_eq!(estimate_tokens(&[]), 0);
    }

    /// `default_compact_threshold` returns 75 % of the default budget.
    #[test]
    fn default_compact_threshold_is_75_pct_of_budget() {
        // 75 % of 32 000 = 24 000.
        assert_eq!(
            default_compact_threshold(),
            24_000,
            "default threshold must be 24 000 tokens"
        );
    }

    /// A second compaction on the same thread produces distinct item ids
    /// (monotonic counter prevents key collisions in JSONL and SQL).
    #[tokio::test]
    async fn second_compaction_produces_distinct_item_ids() {
        let inner = inner_with_summary("SUMMARY2");
        let tid = ThreadId(Arc::from("thread:native/c4"));
        let handle = inner.threads().get_or_init(&tid).await;

        // First compaction.
        handle
            .start_turn_buffer(TurnId(Arc::from("turn:c4/0")), 0)
            .await;
        handle.push_item(user("u0", "first")).await;
        inner
            .run_compaction(&handle, tid.clone(), CompactTrigger::Manual)
            .await
            .expect("first compaction");

        // Second compaction: seed new items so there is something to compact.
        handle.push_item(user("u1", "second")).await;
        let reply2 = inner
            .run_compaction(&handle, tid.clone(), CompactTrigger::Manual)
            .await
            .expect("second compaction");
        assert!(matches!(reply2, CompactReply::Compacted { .. }));

        // The in-memory transcript after two compactions must have 2 items
        // ([marker, summary]) from the most recent compaction, with ids
        // containing "-2-" (seq = 2).
        let tail: Vec<Item> = handle.items_snapshot().await;
        assert_eq!(tail.len(), 2);
        let marker_id: &str = &tail[0].id().0;
        assert!(
            marker_id.contains("compaction-2-marker"),
            "second compaction marker must carry seq=2 in its id, got {marker_id}"
        );
    }
}

// Rust guideline compliant 2026-06-01

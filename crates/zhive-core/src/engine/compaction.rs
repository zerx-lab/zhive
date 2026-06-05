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
pub(in crate::engine) const SUMMARY_INSTRUCTION: &str = "\
You are performing a CONTEXT CHECKPOINT COMPACTION. Write a concise handoff \
summary for another assistant that will resume this task. Include: current \
progress and key decisions, important context / constraints / user \
preferences, what remains to be done, and any critical data or references \
needed to continue. Respond with the summary only.\n\n\
--- TRANSCRIPT ---\n";

impl EngineInner {
    /// Compacts `thread_id`, returning once async summarization has *started*.
    ///
    /// Used by the manual `engine/compact` path. The synchronous prelude
    /// (snapshot, phase claim, `PreCompact` hook) runs inline on the actor
    /// loop; on success the slow provider summarization is spawned as a
    /// detached task and this returns [`CompactReply::Started`]. The eventual
    /// outcome is delivered via [`EngineEvent::CompactionCompleted`] /
    /// [`EngineEvent::CompactionFailed`], not this reply.
    ///
    /// # Errors
    ///
    /// * [`CompactError::ThreadNotFound`] — no such thread.
    /// * [`CompactError::EngineBusy`] — engine phase was not `Idle`.
    /// * [`CompactError::BlockedByHook`] — a `PreCompact` hook blocked it.
    pub(in crate::engine) async fn compact_dispatch(
        self: &Arc<Self>,
        thread_id: ThreadId,
        trigger: CompactTrigger,
    ) -> Result<CompactReply, CompactError> {
        let handle = self
            .threads()
            .get(&thread_id)
            .await
            .ok_or(CompactError::ThreadNotFound)?;
        match self.compact_prelude(&handle, &thread_id, trigger).await {
            CompactStart::Done(result) => result,
            CompactStart::Started { snapshot, entries } => {
                let inner = Arc::clone(self);
                tokio::spawn(async move {
                    inner
                        .compact_tail(handle, thread_id, trigger, snapshot, entries)
                        .await;
                });
                Ok(CompactReply::Started)
            }
        }
    }

    /// Synchronous compaction prelude: snapshot, phase claim, `PreCompact`.
    ///
    /// Runs on the actor loop (the only await is the hook dispatch, which
    /// compaction already paid before summarizing). Returns
    /// [`CompactStart::Done`] for a fast terminal outcome — with the phase
    /// already rolled back where it was claimed — or [`CompactStart::Started`]
    /// once the `Idle → Compaction` phase is held, the `PreCompact` hook has
    /// passed, and [`EngineEvent::CompactionStarted`] has been broadcast.
    async fn compact_prelude(
        self: &Arc<Self>,
        handle: &Arc<ThreadHandle>,
        thread_id: &ThreadId,
        trigger: CompactTrigger,
    ) -> CompactStart {
        // 1. Snapshot the transcript. Nothing to do on an empty thread.
        let snapshot: Vec<Item> = handle.items_snapshot().await;
        if snapshot.is_empty() {
            return CompactStart::Done(Ok(CompactReply::NothingToCompact));
        }
        let entries = u32::try_from(snapshot.len()).unwrap_or(u32::MAX);

        // 2. Claim the engine phase. Compaction requires Idle.
        if let Err(err) = self.try_set_phase_atomic(EnginePhase::Idle, EnginePhase::Compaction) {
            return CompactStart::Done(Err(CompactError::EngineBusy {
                current: err.actual(),
            }));
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
            .dispatch_compact_hook(true, thread_id, trigger, entries)
            .await
        {
            self.leave_compaction(thread_id);
            return CompactStart::Done(Err(blocked));
        }

        // Announce the start (anchors the delta bracket) before the tail runs.
        let _ = self.events_tx().send(EngineEvent::CompactionStarted {
            thread_id: thread_id.clone(),
            trigger,
            entries,
        });

        CompactStart::Started { snapshot, entries }
    }

    /// Core compaction routine shared by the post-turn auto-trigger and tests.
    ///
    /// Runs the prelude then the tail inline (no spawn), so callers that want
    /// to await the full compaction — the auto-trigger in [`super::turn`] and
    /// unit tests — get a synchronous result. The manual RPC path uses
    /// [`Self::compact_dispatch`] instead, which spawns the tail.
    ///
    /// Requires the engine to be `Idle`; the `Idle → Compaction` CAS fails
    /// cleanly (returning [`CompactError::EngineBusy`]) if a turn is in flight.
    ///
    /// # Errors
    ///
    /// * [`CompactError::EngineBusy`] — engine phase was not `Idle`. A
    ///   summarization failure is reported via [`EngineEvent::CompactionFailed`],
    ///   not the return value.
    pub(in crate::engine) async fn run_compaction(
        self: &Arc<Self>,
        handle: &Arc<ThreadHandle>,
        thread_id: ThreadId,
        trigger: CompactTrigger,
    ) -> Result<CompactReply, CompactError> {
        match self.compact_prelude(handle, &thread_id, trigger).await {
            CompactStart::Done(result) => result,
            CompactStart::Started { snapshot, entries } => {
                Arc::clone(self)
                    .compact_tail(Arc::clone(handle), thread_id, trigger, snapshot, entries)
                    .await;
                Ok(CompactReply::Compacted {
                    entries_compacted: entries,
                })
            }
        }
    }

    /// Async compaction tail: summarize (streaming), replace history, persist,
    /// broadcast, `PostCompact`, leave the phase.
    ///
    /// Runs spawned (manual path) or inline (auto path / tests). A
    /// [`CompactionPhaseGuard`] is the backstop: if this returns early or
    /// panics while still holding `Compaction`, the guard restores `Idle` and
    /// broadcasts [`EngineEvent::CompactionFailed`] so a subscriber is never
    /// left waiting. Compaction is **not** cancellable in this pass — the
    /// transcript swap is a single atomic replace applied only after a
    /// successful summary, so a partial stream is harmless.
    async fn compact_tail(
        self: Arc<Self>,
        handle: Arc<ThreadHandle>,
        thread_id: ThreadId,
        trigger: CompactTrigger,
        snapshot: Vec<Item>,
        entries: u32,
    ) {
        let mut guard = CompactionPhaseGuard::new(Arc::clone(&self), thread_id.clone());

        // 4. Summarise via the provider, streaming each delta to subscribers,
        //    inside the `zhive.compaction` span. Use `.instrument()` (not
        //    `.enter()`) so the span survives the await on a multi-thread runtime.
        let summary = summarize_streaming(
            self.provider(),
            &snapshot,
            self.compaction_instruction(),
            |delta| {
                let _ = self.events_tx().send(EngineEvent::CompactionDelta {
                    thread_id: thread_id.clone(),
                    delta: delta.to_owned(),
                });
            },
        )
        .instrument(tracing::info_span!("zhive.compaction", "session.id" = %thread_id.0))
        .await;
        let summary = match summary {
            Ok(s) => s,
            Err(e) => {
                self.finish_failed(&mut guard, &thread_id, e.to_string());
                return;
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

        // 7. Compaction → Idle, then announce successful completion.
        self.leave_compaction(&thread_id);
        let _ = self.events_tx().send(EngineEvent::CompactionCompleted {
            thread_id: thread_id.clone(),
            entries_compacted: entries,
        });
        guard.disarm();
    }

    /// Rolls the phase back to `Idle` and broadcasts a failure event.
    ///
    /// Disarms `guard` so its `Drop` backstop does not double-report the
    /// failure just surfaced here.
    fn finish_failed(
        &self,
        guard: &mut CompactionPhaseGuard,
        thread_id: &ThreadId,
        reason: String,
    ) {
        self.leave_compaction(thread_id);
        let _ = self.events_tx().send(EngineEvent::CompactionFailed {
            thread_id: thread_id.clone(),
            reason,
        });
        guard.disarm();
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

/// Outcome of [`EngineInner::compact_prelude`].
enum CompactStart {
    /// A fast terminal outcome; the caller replies immediately. The phase is
    /// already rolled back where it was claimed, or was never claimed.
    Done(Result<CompactReply, CompactError>),
    /// The `Compaction` phase is held and `PreCompact` passed; the caller must
    /// drive [`EngineInner::compact_tail`] with this snapshot.
    Started {
        /// Transcript snapshot to summarize.
        snapshot: Vec<Item>,
        /// Item count being folded into the summary.
        entries: u32,
    },
}

/// RAII backstop restoring `Idle` if a compaction tail exits while still
/// holding the `Compaction` phase.
///
/// The normal success and failure paths leave the phase explicitly and then
/// call [`Self::disarm`]. If the tail returns early or panics while armed,
/// `Drop` runs the conditional `Compaction → Idle` CAS (a logged no-op when
/// the phase already moved) and broadcasts [`EngineEvent::CompactionFailed`],
/// so a subscriber is never left waiting on a started-but-silent compaction.
struct CompactionPhaseGuard {
    inner: Arc<EngineInner>,
    thread_id: ThreadId,
    armed: bool,
}

impl CompactionPhaseGuard {
    /// Arms a guard for `thread_id`'s in-flight compaction.
    fn new(inner: Arc<EngineInner>, thread_id: ThreadId) -> Self {
        Self {
            inner,
            thread_id,
            armed: true,
        }
    }

    /// Disarms the guard after the tail has left the phase normally.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CompactionPhaseGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.inner.leave_compaction(&self.thread_id);
        let _ = self.inner.events_tx().send(EngineEvent::CompactionFailed {
            thread_id: self.thread_id.clone(),
            reason: "compaction task terminated unexpectedly".to_owned(),
        });
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

/// Asks `provider` for a handoff summary, streaming each fragment to `on_delta`.
///
/// `instruction` is the summarization request prepended to the rendered
/// transcript; callers pass [`super::inner::EngineInner::compaction_instruction`],
/// which is the host-configured prompt or the built-in [`SUMMARY_INSTRUCTION`].
/// `on_delta` is invoked once per provider `TextDelta` with the raw fragment,
/// letting the caller surface the summary as it is generated. The complete,
/// trimmed summary is also returned.
///
/// # Errors
///
/// Returns a [`ProviderError`] if the provider call fails or the stream
/// yields an error part.
pub(in crate::engine) async fn summarize_streaming<F>(
    provider: &DynLanguageModel,
    items: &[Item],
    instruction: &str,
    mut on_delta: F,
) -> Result<String, ProviderError>
where
    F: FnMut(&str),
{
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
            text: format!("{instruction}{transcript}"),
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
            on_delta(&delta);
            summary.push_str(&delta);
        }
    }
    Ok(summary.trim().to_owned())
}

/// Renders `items` to plain text and asks `provider` for a handoff summary.
///
/// Non-streaming wrapper over [`summarize_streaming`]; collects every
/// `TextDelta` into the returned string without surfacing intermediate
/// fragments. Shared with [`super::fork`], which reuses the same provider
/// summarisation path for the optional branch-summary step rather than
/// building a parallel one.
///
/// # Errors
///
/// Returns a [`ProviderError`] if the provider call fails or the stream
/// yields an error part.
pub(in crate::engine) async fn summarize(
    provider: &DynLanguageModel,
    items: &[Item],
    instruction: &str,
) -> Result<String, ProviderError> {
    summarize_streaming(provider, items, instruction, |_| {}).await
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

    fn inner_with_model(model: DynLanguageModel) -> Arc<EngineInner> {
        let (tx, _rx) = broadcast::channel::<EngineEvent>(64);
        Arc::new(EngineInner::new(tx, model))
    }

    /// Provider whose calls always fail, to exercise the compaction failure path.
    #[derive(Debug)]
    struct FailingModel;

    #[async_trait::async_trait]
    impl llmsdk::LanguageModel for FailingModel {
        fn provider(&self) -> &'static str {
            "fail"
        }
        fn model_id(&self) -> &'static str {
            "fail"
        }
        async fn do_generate(
            &self,
            _opts: CallOptions,
        ) -> llmsdk::error::Result<llmsdk::language_model::GenerateResult> {
            Err(llmsdk::ProviderError::no_such_model(
                "fail",
                "languageModel",
            ))
        }
        async fn do_stream(
            &self,
            _opts: CallOptions,
        ) -> llmsdk::error::Result<llmsdk::language_model::StreamResult> {
            Err(llmsdk::ProviderError::no_such_model(
                "fail",
                "languageModel",
            ))
        }
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

    #[tokio::test]
    async fn summarize_streaming_invokes_on_delta_per_fragment() {
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
                    delta: "Hel".into(),
                    provider_metadata: None,
                },
                StreamPart::TextDelta {
                    id: "b".into(),
                    delta: "lo".into(),
                    provider_metadata: None,
                },
                StreamPart::TextEnd {
                    id: "b".into(),
                    provider_metadata: None,
                },
            ],
        )
        .into_dyn();
        let items = vec![user("u0", "hi")];
        let mut fragments: Vec<String> = Vec::new();
        let summary = summarize_streaming(&model, &items, SUMMARY_INSTRUCTION, |d| {
            fragments.push(d.to_owned());
        })
        .await
        .expect("summarize must succeed");
        assert_eq!(fragments, vec!["Hel".to_owned(), "lo".to_owned()]);
        assert_eq!(summary, "Hello");
    }

    #[tokio::test]
    async fn compact_dispatch_returns_started_then_completes_in_background() {
        use std::time::Duration;

        let inner = inner_with_summary("SUMMARY");
        let tid = ThreadId(Arc::from("thread:native/cd"));
        let handle = inner.threads().get_or_init(&tid).await;
        handle
            .start_turn_buffer(TurnId(Arc::from("turn:cd/0")), 0)
            .await;
        handle.push_item(user("u0", "hello")).await;

        let mut rx = inner.events_tx().subscribe();
        let reply = inner
            .compact_dispatch(tid.clone(), CompactTrigger::Manual)
            .await
            .expect("dispatch reachable");
        assert!(
            matches!(reply, CompactReply::Started),
            "manual dispatch must return Started immediately"
        );

        let mut saw_started = false;
        let mut saw_delta = false;
        let mut saw_completed = false;
        for _ in 0..64 {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("event timeout")
                .expect("broadcast recv")
            {
                EngineEvent::CompactionStarted {
                    entries, trigger, ..
                } => {
                    assert_eq!(entries, 1);
                    assert_eq!(trigger, CompactTrigger::Manual);
                    saw_started = true;
                }
                EngineEvent::CompactionDelta { delta, .. } => {
                    assert!(!delta.is_empty());
                    saw_delta = true;
                }
                EngineEvent::CompactionCompleted {
                    entries_compacted, ..
                } => {
                    assert_eq!(entries_compacted, 1);
                    saw_completed = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_started, "must broadcast CompactionStarted");
        assert!(saw_delta, "must stream at least one CompactionDelta");
        assert!(saw_completed, "must broadcast CompactionCompleted");

        // History was replaced with [marker, summary].
        let tail: Vec<Item> = handle.items_snapshot().await;
        assert_eq!(tail.len(), 2);
        assert_eq!(*inner.phase_lock(), EnginePhase::Idle);
    }

    #[tokio::test]
    async fn compaction_failure_broadcasts_failed_and_restores_idle() {
        use std::time::Duration;

        let inner = inner_with_model(DynLanguageModel::new(FailingModel));
        let tid = ThreadId(Arc::from("thread:native/cf"));
        let handle = inner.threads().get_or_init(&tid).await;
        handle
            .start_turn_buffer(TurnId(Arc::from("turn:cf/0")), 0)
            .await;
        handle.push_item(user("u0", "hi")).await;

        let mut rx = inner.events_tx().subscribe();
        // Inline so the tail completes before we inspect; a provider failure is
        // surfaced via CompactionFailed, not the return value.
        let _ = inner
            .run_compaction(&handle, tid.clone(), CompactTrigger::Manual)
            .await;

        let mut saw_failed = false;
        for _ in 0..64 {
            if let EngineEvent::CompactionFailed { reason, .. } =
                tokio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .expect("event timeout")
                    .expect("broadcast recv")
            {
                assert!(!reason.is_empty(), "failure must carry a reason");
                saw_failed = true;
                break;
            }
        }
        assert!(
            saw_failed,
            "summary failure must broadcast CompactionFailed"
        );
        assert_eq!(
            *inner.phase_lock(),
            EnginePhase::Idle,
            "phase must be restored to Idle after a failed compaction"
        );
    }
}

// Rust guideline compliant 2026-06-01

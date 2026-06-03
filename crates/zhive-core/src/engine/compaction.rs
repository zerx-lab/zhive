//! Context compaction: replace a thread's transcript history with an
//! LLM-generated handoff summary.
//!
//! Compaction is a core-harness maintenance operation (mirrors codex
//! `core/src/compact.rs`). It runs in the [`EnginePhase::Compaction`] phase,
//! brackets the work with the [`HookEvent::PreCompact`] / [`HookEvent::PostCompact`]
//! hooks, and emits a `zhive.compaction` span.
//!
//! ## Durability boundary (Phase 1)
//!
//! Compaction is **in-memory only**. The JSONL rollout is an append-only
//! `Session/Item/Leaf` log with no truncation entry, so persisting the
//! replaced history would make a rebuilt engine diverge from the live one
//! (rebuild would replay every original item plus the summary). We therefore
//! emit **no** `StorageWriteOp` here — `ItemAppended` is broadcast for live
//! observers only. A rebuilt engine simply sees the un-compacted history and
//! re-triggers auto-compaction. Durable compaction (a rollout compaction
//! entry + rebuild handling) is deferred together with suspended-turn
//! persistence.

use std::sync::Arc;

use futures::StreamExt;
use tracing::Instrument as _;

use llmsdk::language_model::{CallOptions, Message, StreamPart, TextPart, UserPart};
use zhive_proto::domain::{Item, ItemId, ThreadId, TurnId};
use zhive_proto::hook::{CompactTrigger, HookEvent};

use super::event::EngineEvent;
use super::inner::EngineInner;
use super::submission::{CompactError, CompactReply};
use crate::provider::{DynLanguageModel, ProviderError};
use crate::state::ThreadHandle;
use zhive_proto::hook::EnginePhase;
use zhive_proto::permission::{HookSpecificOutput, PermissionDecision};

/// Transcript length (item count) at or above which a completed turn triggers
/// automatic compaction.
///
/// Phase-1 auto-compaction keys off **transcript item count**, not a token
/// total: the per-request prompt-token count vs the model `context_window`
/// (the codex `full_context_window_limit_reached` signal) is not yet plumbed
/// through the engine, and `ScriptedModel` reports `Usage::default()` (zero),
/// so a token threshold would never fire. Item count is a real proxy for
/// context growth; a token-based refinement lands when providers surface
/// usage and the context window is reachable. Item-count compaction stays a
/// useful safety valve before the history buffer evicts old turns.
pub(in crate::engine) const AUTO_COMPACT_ITEM_THRESHOLD: usize = 50;

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
        //    NO storage op — see the module-level durability note.
        let marker = Item::ContextCompaction {
            id: compaction_item_id(&thread_id, "marker"),
        };
        let summary_item = Item::AgentMessage {
            id: compaction_item_id(&thread_id, "summary"),
            text: format!("{SUMMARY_PREFIX}{summary}"),
        };
        let compaction_turn = TurnId(Arc::from(format!("{}::compaction", thread_id.0)));
        handle
            .replace_history_with_compaction(
                compaction_turn.clone(),
                vec![marker.clone(), summary_item.clone()],
            )
            .await;
        // Broadcast for live observers (UI). Not persisted.
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
/// TODO(durable-compaction): once compaction is persisted to the rollout, this
/// must incorporate a monotonic counter or timestamp — otherwise repeated
/// compactions of the same thread emit duplicate-key `Item`s into the JSONL
/// log. In-memory this is safe because the history is replaced wholesale
/// (`replace_history_with_compaction`) before the summary turn is installed.
fn compaction_item_id(thread_id: &ThreadId, suffix: &str) -> ItemId {
    ItemId(Arc::from(format!("{}::compaction-{suffix}", thread_id.0)))
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
}

// Rust guideline compliant 2026-06-01

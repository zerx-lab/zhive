//! Turn execution logic for the engine actor.
//!
//! This module contains the streaming provider loop that runs inside the
//! dedicated `tokio::spawn` task created by
//! [`super::inner::EngineInner::start_turn`].  It is intentionally kept
//! separate from the actor-loop code in [`super::inner`] so that the two
//! concerns — actor dispatch / state management vs. per-turn LLM I/O —
//! each remain well under the 600-line soft limit.
//!
//! Prompt construction (`build_call_options` and the item→message mapping
//! helpers) lives in the sibling module [`super::prompt`] so that both
//! files stay under the 600-line soft limit.
//!
//! ## Inner tool-call loop
//!
//! `run_turn` is a **bounded loop** (cap: [`MAX_TURN_ITERATIONS`]):
//!
//! 1. Build `CallOptions` by reconstructing the prompt from the thread tail
//!    (via [`super::prompt::build_call_options`]).
//! 2. Call `provider.do_stream(call_options)`.  On outer `Err` → `TurnFailed`.
//! 3. Stream loop: `tokio::select!` on `cancel.cancelled()` vs next part.
//!    On each part: fold → push items → emit `ItemAppended`.
//!    On inner stream error: `TurnFailed` + finish.
//! 4. After the stream: if the model emitted `ToolCall` items, run
//!    [`super::tool_dispatch::dispatch_tool_call`] for each, push the finalized
//!    `ToolCall` item (which carries `provider_tool_call_id`) to the tail, and
//!    loop. The next iteration's prompt is rebuilt from that updated tail.
//! 5. If no tool calls, or if a hook requested `stop_loop`, or if the
//!    iteration cap fires: call `fold.finish()` then `finish_turn`.
//!    Cancel does *not* emit `TurnCompleted` — that is handled by
//!    `cancel_turn` (which emits `SessionAborted`).

use std::sync::Arc;

use futures::StreamExt as _;
use llmsdk::language_model::StreamPart;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;
use zhive_proto::domain::{Item, ItemId, NoticeLevel, ThreadId, TurnError, TurnId};
use zhive_proto::hook::CompactTrigger;
use zhive_proto::permission::PermissionScope;

use crate::persistence::writer::StorageWriteOp;
use crate::provider::{ProviderError, StreamFold};
use crate::queues::QueueTarget;
use crate::state::ThreadHandle;

use super::event::EngineEvent;
use super::inner::EngineInner;
use super::prompt::build_call_options;
use super::tool_dispatch::dispatch_tool_call;

// ============================================================
// Constants
// ============================================================

/// Maximum number of provider call iterations within a single turn.
///
/// Prevents runaway tool-calling loops.  At this limit the turn ends
/// cleanly (with an optional `SystemNotice`) rather than looping
/// indefinitely.  32 matches the Claude Code default.
const MAX_TURN_ITERATIONS: u32 = 32;

// ============================================================
// Turn execution
// ============================================================

/// Drives the provider streaming loop for one turn, including tool dispatch.
///
/// Runs in a dedicated `tokio::spawn` task; holds no lock across await
/// points so `cancel_turn` can take the `active_turn` slot at any time.
///
/// Returns `Some(error)` carrying the real [`TurnError`] when the turn ended
/// in a failure state (`TurnFailed` was broadcast), or `None` on a clean
/// completion **or** cancellation (cancellation is not a failure). Callers that
/// need to distinguish outcomes — notably [`super::inner::EngineInner::run_child_turn_and_deliver`]
/// — use the return value to decide which payload to include in
/// [`crate::engine::event::EngineEvent::SubagentCompleted`]: a cancelled
/// subagent stays Completed, only `Some(_)` surfaces as Errored.
///
/// Opens a `zhive.turn` OTel-aligned span for the entire turn lifetime,
/// with `thread.id` and `turn.id` fields populated from the arguments.
///
/// ## Steps (per iteration, up to [`MAX_TURN_ITERATIONS`])
///
/// 1. Build `CallOptions` from the thread history.
/// 2. Call `provider.do_stream(call_options)`.  On outer `Err` → `TurnFailed`.
/// 3. Stream loop: `tokio::select!` on `cancel.cancelled()` vs next part.
///    On each part: fold → push items → emit `ItemAppended`.
///    On inner stream error: `TurnFailed` + finish.
/// 4. After the stream ends: if tool-call items were produced, dispatch each
///    via `dispatch_tool_call`.  If a hook requested `stop_loop`, finish early.
///    If no tool calls, the turn is complete.
/// 5. After the loop (including on cancel): call `fold.finish()` then
///    `finish_turn`.  Cancel suppresses `TurnCompleted`.
#[must_use = "a returned TurnError carries the real failure cause; subagent callers must surface it"]
pub(super) async fn run_turn(
    inner: &Arc<EngineInner>,
    handle: Arc<ThreadHandle>,
    thread_id: ThreadId,
    turn_id: TurnId,
    cancel: CancellationToken,
) -> Option<TurnError> {
    // Open a `zhive.turn` span for the whole turn's lifetime.
    //
    // Span name and field names are string literals here (macro
    // requirement); the constants `spans::TURN`, `fields::THREAD_ID`,
    // and `fields::TURN_ID` are the single source of truth and compile-
    // time assertions in the observability tests verify the literals
    // stay in sync.
    // Span name and field names are string literals (macro requirement).
    // The constants spans::TURN / fields::THREAD_ID / fields::TURN_ID are
    // the single source of truth; the test `span_literals_match_constants`
    // in observability::tests asserts the literals stay in sync.
    let span = tracing::info_span!(
        "zhive.turn",
        "session.id"    = %thread_id.0,
        "zhive.turn.id" = %turn_id.0,
    );
    run_turn_inner(inner, handle, thread_id, turn_id, cancel)
        .instrument(span)
        .await
}

/// Inner async body of [`run_turn`], instrumented by the caller with the
/// `zhive.turn` span.
#[expect(
    clippy::too_many_lines,
    reason = "run_turn_inner is the top-level turn state machine; see run_turn for context"
)]
async fn run_turn_inner(
    inner: &Arc<EngineInner>,
    handle: Arc<ThreadHandle>,
    thread_id: ThreadId,
    turn_id: TurnId,
    cancel: CancellationToken,
) -> Option<TurnError> {
    // Read the turn scope from the active turn record.
    //
    // Top-level turns store `PermissionScope::default_turn_scope()` (set by
    // `start_turn` via `ActiveTurn::new_with_cancel`). Child (subagent) turns
    // carry a computed narrowed scope that `prepare_child_scope` produced and
    // `start_child_turn` stored via `ActiveTurn::new_with_cancel_and_scope`.
    // In either case the authoritative value is in `handle.active_turn.scope`,
    // so we always read it from there to honour the narrowing guarantee.
    let scope: PermissionScope = {
        let guard = handle.active_turn.lock().await;
        guard
            .as_ref()
            .map_or_else(PermissionScope::default_turn_scope, |a| a.scope.clone())
    };

    // Per-turn item sequence counter for persistence.  Monotonically
    // incremented on every ItemAppended enqueue so the writer can order
    // items within a turn.
    let mut item_seq: i64 = 0;

    // Fallback counter used ONLY when the provider did not supply a
    // tool_call_id (should not happen with conformant providers, but
    // protects against malformed streams in tests / Phase-1 scripted
    // models that omit the id field). The same id is written into the
    // finalized item's `provider_tool_call_id` so prompt reconstruction can
    // still emit a matching tool_use / tool_result pair.
    let mut fallback_id_counter: u64 = 0;

    'outer: for iteration in 0..MAX_TURN_ITERATIONS {
        // ── Steer drain (Pi model §3.1): drain BEFORE each LLM request ────────
        //
        // Steer items are injected as user-turn items visible to the *next*
        // LLM call.  They do NOT cancel in-flight tool calls; the in-flight
        // work continues to completion and steer messages only influence the
        // following model decision.
        //
        // Steer items cannot cause a meaningful failure after being pushed
        // (push_item + ItemAppended are infallible wrt item content), so we
        // do NOT need `restore_front` rollback here — the Pi failure-semantics
        // path (drain + rollback on downstream failure) is only relevant when
        // the consumer may reject the drained batch, which does not apply to
        // this integration.
        {
            let steer_items: Vec<Item> = handle.injection_lock().drain(QueueTarget::Steer);
            for item in steer_items {
                handle.push_item(item.clone()).await;
                let _ = inner.events_tx().send(EngineEvent::ItemAppended {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                    item: Box::new(item.clone()),
                });
                inner.enqueue_storage_op(StorageWriteOp::ItemAppended {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                    seq: item_seq,
                    item: Box::new(item),
                });
                item_seq += 1;
            }
        }

        // 1. Build the prompt by reconstructing it from the thread's item tail
        //    (the single source of truth; see `build_call_options`).
        let call_options = build_call_options(&handle).await;

        // 2. Call the provider, racing against the per-turn cancel token.
        //
        // `biased` ensures the cancel arm is always polled first so that a
        // token already cancelled before we enter the select! (e.g. from an
        // immediate cancel_turn or shutdown) is observed without starting the
        // provider call at all.  When the cancel arm wins — whether because
        // the token was pre-cancelled or because cancel_tree.cancel_all()
        // fires during the await — we call finish_turn (failed=false; the
        // cancel path, not an error) and return promptly, satisfying the
        // "abort promptly on shutdown" requirement.
        let stream_result = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                inner.finish_turn(&handle, thread_id, turn_id, false).await;
                return None;
            }
            r = inner.provider().do_stream(call_options) => r,
        };
        let mut stream = match stream_result {
            Ok(r) => r.stream,
            Err(err) => {
                let provider_err = ProviderError::from(err);
                let turn_error = TurnError {
                    message: provider_err.to_string(),
                    additional_details: None,
                };
                let _ = inner.events_tx().send(EngineEvent::TurnFailed {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                    error: turn_error.clone(),
                });
                inner.finish_turn(&handle, thread_id, turn_id, true).await;
                return Some(turn_error);
            }
        };

        // 3. Stream loop with per-turn cancellation.
        let mut fold = StreamFold::new(&turn_id);
        // Captures the real error when the stream errors mid-flight, so it can
        // be threaded out of the function. Stays `None` on cancellation (cancel
        // is not a failure); `failure.is_some()` is the single source of truth
        // for "did this iteration fail".
        let mut failure: Option<TurnError> = None;
        // Track which tool-call items the model produced this iteration.
        let mut new_tool_call_items: Vec<Item> = Vec::new();

        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    break;
                }
                maybe_part = stream.next() => {
                    match maybe_part {
                        None => break,
                        Some(Ok(part)) => {
                            // Stream text fragments live so clients can render
                            // token-by-token; the block still finalises as one
                            // AgentMessage via fold below.
                            if let StreamPart::TextDelta { delta, .. } = &part
                                && !delta.is_empty()
                            {
                                let _ = inner.events_tx().send(EngineEvent::ItemDelta {
                                    thread_id: thread_id.clone(),
                                    turn_id: turn_id.clone(),
                                    delta: delta.clone(),
                                });
                            }
                            for item in fold.fold(part) {
                                if matches!(&item, Item::ToolCall { .. }) {
                                    // Accumulate tool-call items for post-stream dispatch.
                                    // Do NOT push to the thread or broadcast here —
                                    // the final Completed/Failed item is pushed after
                                    // dispatch to avoid two entries for the same ItemId.
                                    new_tool_call_items.push(item);
                                } else {
                                    handle.push_item(item.clone()).await;
                                    let _ = inner.events_tx().send(EngineEvent::ItemAppended {
                                        thread_id: thread_id.clone(),
                                        turn_id: turn_id.clone(),
                                        item: Box::new(item.clone()),
                                    });
                                    inner.enqueue_storage_op(StorageWriteOp::ItemAppended {
                                        thread_id: thread_id.clone(),
                                        turn_id: turn_id.clone(),
                                        seq: item_seq,
                                        item: Box::new(item),
                                    });
                                    item_seq += 1;
                                }
                            }
                        }
                        Some(Err(err)) => {
                            let provider_err = ProviderError::from(err);
                            let turn_error = TurnError {
                                message: provider_err.to_string(),
                                additional_details: None,
                            };
                            let _ = inner.events_tx().send(EngineEvent::TurnFailed {
                                thread_id: thread_id.clone(),
                                turn_id: turn_id.clone(),
                                error: turn_error.clone(),
                            });
                            failure = Some(turn_error);
                            break;
                        }
                    }
                }
            }
        }

        // Drain any open fold buffers (partial items before cancel/error).
        for item in fold.finish() {
            if matches!(&item, Item::ToolCall { .. }) {
                // Same rule as in the stream loop: accumulate only; the
                // final push happens after dispatch below.
                new_tool_call_items.push(item);
            } else {
                handle.push_item(item.clone()).await;
                let _ = inner.events_tx().send(EngineEvent::ItemAppended {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                    item: Box::new(item.clone()),
                });
                inner.enqueue_storage_op(StorageWriteOp::ItemAppended {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                    seq: item_seq,
                    item: Box::new(item),
                });
                item_seq += 1;
            }
        }

        if failure.is_some() || cancel.is_cancelled() {
            // finish_turn handles the Turn→Idle rollback; pass failed=true
            // when the stream errored so TurnCompleted is not emitted.
            inner
                .finish_turn(&handle, thread_id, turn_id, failure.is_some())
                .await;
            // `failure` is `Some` only on a stream error; on cancellation it
            // stays `None`, so a cancelled turn is reported as non-failure.
            return failure;
        }

        // 4. Tool dispatch for all tool-call items produced this iteration.
        if new_tool_call_items.is_empty() {
            // ── FollowUp drain (Pi model §3.2): at the turn boundary ──────────
            //
            // Before finishing the turn, check whether the follow-up queue
            // holds any items.  If it does, inject them as user messages and
            // continue the outer loop (keeping the turn alive for one more
            // provider iteration).  If the queue is empty, the turn ends
            // normally.
            //
            // Like the steer drain above, `restore_front` is not needed here
            // because push_item + ItemAppended are infallible.
            let follow_up_items: Vec<Item> = handle.injection_lock().drain(QueueTarget::FollowUp);
            if follow_up_items.is_empty() {
                // No follow-up items — turn is done normally.
                break 'outer;
            }
            // Inject follow-up items and continue the loop.
            for item in follow_up_items {
                handle.push_item(item.clone()).await;
                let _ = inner.events_tx().send(EngineEvent::ItemAppended {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                    item: Box::new(item.clone()),
                });
                inner.enqueue_storage_op(StorageWriteOp::ItemAppended {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                    seq: item_seq,
                    item: Box::new(item),
                });
                item_seq += 1;
            }
            // Fall through to the next iteration (the iteration cap still
            // applies — if we are at MAX_TURN_ITERATIONS the notice below
            // fires before continuing).
            if iteration + 1 >= MAX_TURN_ITERATIONS {
                let notice_id = ItemId(Arc::from(format!("item:{}/max-iter-fu", turn_id.0)));
                let notice = Item::SystemNotice {
                    id: notice_id,
                    level: NoticeLevel::Warn,
                    message: format!(
                        "max turn iterations reached ({MAX_TURN_ITERATIONS}); turn ended"
                    ),
                };
                handle.push_item(notice.clone()).await;
                let _ = inner.events_tx().send(EngineEvent::ItemAppended {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                    item: Box::new(notice.clone()),
                });
                inner.enqueue_storage_op(StorageWriteOp::ItemAppended {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                    seq: item_seq,
                    item: Box::new(notice),
                });
                // item_seq not incremented here since we break immediately
                break 'outer;
            }
            continue 'outer;
        }

        let hook_host = Arc::clone(inner.hook_host());
        let tools = Arc::clone(inner.tools());
        let reducer = inner.permission_reducer();
        let thread_id_str = thread_id.0.as_ref();

        let mut stop_requested = false;
        for tool_item in &new_tool_call_items {
            let Item::ToolCall {
                id: item_id,
                name: tool_name,
                raw_input: Some(raw_args),
                provider_tool_call_id: maybe_provider_id,
                ..
            } = tool_item
            else {
                continue;
            };

            // Use the provider's original tool_call_id so Message::Tool
            // round-trips it correctly.  Fall back to a synthetic id only
            // for non-conformant streams (scripted test models, etc.).
            let tool_use_id: String = match maybe_provider_id.as_deref() {
                Some(id) if !id.is_empty() => id.to_owned(),
                _ => {
                    fallback_id_counter += 1;
                    format!("tc-fallback-{fallback_id_counter}")
                }
            };

            let outcome = dispatch_tool_call(
                inner,
                &hook_host,
                &tools,
                &reducer,
                thread_id_str,
                &turn_id,
                item_id.clone(),
                tool_name,
                raw_args.clone(),
                &tool_use_id,
                &scope,
                &cancel,
            )
            .await;

            if outcome.stop_loop() {
                stop_requested = true;
            }

            // If the turn was cancelled during this dispatch (the tool body or
            // PostToolUse lost its select! race), the outcome item is an
            // abandoned result. Do NOT push or broadcast it — emitting an
            // ItemAppended for a result the engine has rolled back from would
            // be an orphan event. Just finish and exit.
            if cancel.is_cancelled() {
                inner.finish_turn(&handle, thread_id, turn_id, false).await;
                return None;
            }

            // The finalized item already carries `provider_tool_call_id`
            // (set by `dispatch_tool_call` from `tool_use_id`), so pushing it
            // to the tail is all that prompt reconstruction needs — no side
            // accumulator. `tool_use_id` itself is consumed only as the
            // dispatch argument above.
            let final_item = outcome.item().clone();

            // Push the finalized ToolCall item (replaces the InProgress one).
            handle.push_item(final_item.clone()).await;
            let _ = inner.events_tx().send(EngineEvent::ItemAppended {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                item: Box::new(final_item.clone()),
            });
            inner.enqueue_storage_op(StorageWriteOp::ItemAppended {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                seq: item_seq,
                item: Box::new(final_item),
            });
            item_seq += 1;
        }

        if stop_requested {
            break 'outer;
        }

        // If we are at the last allowed iteration, append a notice.
        if iteration + 1 >= MAX_TURN_ITERATIONS {
            let notice_id = ItemId(Arc::from(format!("item:{}/max-iter", turn_id.0)));
            let notice = Item::SystemNotice {
                id: notice_id,
                level: NoticeLevel::Warn,
                message: format!("max tool iterations reached ({MAX_TURN_ITERATIONS}); turn ended"),
            };
            handle.push_item(notice.clone()).await;
            let _ = inner.events_tx().send(EngineEvent::ItemAppended {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                item: Box::new(notice.clone()),
            });
            inner.enqueue_storage_op(StorageWriteOp::ItemAppended {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                seq: item_seq,
                item: Box::new(notice),
            });
            // item_seq not incremented here since we break immediately
            break 'outer;
        }
    }

    // 5. finish_turn handles the Turn→Idle rollback and TurnCompleted
    //    emission. Pass failed=false (normal completion).
    inner
        .finish_turn(&handle, thread_id.clone(), turn_id, false)
        .await;

    // 6. Auto-compaction: now that the engine is Idle again, fold the
    //    transcript down if it has grown past the threshold. A concurrent
    //    StartTurn may win the Idle→Compaction CAS, in which case
    //    run_compaction returns EngineBusy and compaction is skipped this
    //    round (it re-arms after the next turn).
    //
    //    Skip for subagent (child) threads: like `finish_turn` / `cancel_turn`
    //    they do NOT own the global EnginePhase slot (the parent turn does), so
    //    driving Idle→Compaction here would fight the parent's phase ownership.
    //    Child-thread compaction, if ever wanted, must be parent-coordinated.
    if handle.parent_thread_id.is_none()
        && handle.items_tail.read().await.len() >= super::compaction::AUTO_COMPACT_ITEM_THRESHOLD
    {
        let _ = inner
            .run_compaction(&handle, thread_id, CompactTrigger::Auto)
            .await;
    }
    None
}

// Rust guideline compliant 2026-02-21

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
//! `run_turn` is a **bounded loop** (cap: [`super::inner::EngineInner::max_turn_iterations`]):
//!
//! 1. Build `CallOptions` by reconstructing the prompt from the thread tail
//!    (via [`super::prompt::build_call_options`]).
//! 2. Call `provider.do_stream(call_options)`.  On outer `Err` → `TurnFailed`.
//! 3. Stream loop: `tokio::select!` on `cancel.cancelled()` vs next part.
//!    On each part: fold → push items → emit `ItemAppended`.
//!    On inner stream error: `TurnFailed` + finish.
//! 4. After the stream: if the model emitted `ToolCall` items, dispatch them in
//!    three phases — serial permission resolution
//!    ([`super::tool_dispatch::resolve_tool_permission`]), parallel execution
//!    ([`super::tool_dispatch::execute_resolved_tool`]), then serial item
//!    commit — pushing each finalized `ToolCall` item (which carries
//!    `provider_tool_call_id`) to the tail in model-emit order, and loop. The
//!    next iteration's prompt is rebuilt from that updated tail.
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
use super::subagent_spawn::EngineSubagentSpawner;
use super::tool_dispatch::{
    DispatchOutcome, ToolResolution, execute_resolved_tool, resolve_tool_permission,
};

// ============================================================
// Tool-dispatch phasing
// ============================================================

/// Outcome of PHASE 1 (serial permission resolution) for one tool call.
///
/// Pairs a per-call's owned dispatch data with its resolution so PHASE 2 can
/// execute approved calls concurrently and PHASE 3 can commit results in the
/// original model-emit order.
enum Resolved {
    /// Permission denied / schema-failed / cancelled: the item is already final.
    Blocked(DispatchOutcome),
    /// Approved: carries the data the execute phase needs.
    Approved {
        /// Item id of the originating `ToolCall`.
        item_id: ItemId,
        /// Tool name to look up and execute.
        tool_name: String,
        /// Effective (possibly hook-mutated) input arguments.
        args: serde_json::Value,
        /// Provider tool-call id round-tripped onto the finalized item.
        tool_use_id: String,
        /// Whether a hook already requested loop termination.
        stop_loop: bool,
    },
}

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
/// ## Steps (per iteration, up to the engine's configured iteration cap)
///
/// 1. Build `CallOptions` from the thread history.
/// 2. Call `provider.do_stream(call_options)`.  On outer `Err` → `TurnFailed`.
/// 3. Stream loop: `tokio::select!` on `cancel.cancelled()` vs next part.
///    On each part: fold → push items → emit `ItemAppended`.
///    On inner stream error: `TurnFailed` + finish.
/// 4. After the stream ends: if tool-call items were produced, dispatch them in
///    three phases (serial resolve → parallel execute → serial commit). If a
///    hook requested `stop_loop`, finish early. If no tool calls, the turn is
///    complete.
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

    // Effective per-turn iteration cap (from the engine's `TurnLimits`).
    // Read once so every comparison below uses a single stable value.
    let max_iterations = inner.max_turn_iterations();

    'outer: for iteration in 0..max_iterations {
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
        let call_options =
            build_call_options(&handle, inner.tools(), inner.system_prompt(), &scope).await;

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

        // Emit this provider call's token usage for observability (D-014).
        // Runs once per loop iteration, so a multi-call (tool-using) turn logs
        // each call's usage. Field names follow OTel GenAI semantic conventions
        // so an OTLP exporter surfaces them without a rename. `total` is `None`
        // when the provider reported no usage (e.g. the scripted test model).
        if let Some(usage) = fold.usage() {
            // `input_tokens` / `output_tokens` map to the OTel GenAI semconv
            // attributes `gen_ai.usage.input_tokens` / `output_tokens`; the
            // dotted names are kept flat here only to satisfy the `tracing`
            // macro's field-name grammar.
            let input_tokens = usage.input_tokens.total.unwrap_or(0);
            let output_tokens = usage.output_tokens.total.unwrap_or(0);
            tracing::info!(
                name: "zhive.turn.usage",
                input_tokens,
                output_tokens,
                "token usage: {{input_tokens}} in / {{output_tokens}} out"
            );
            // Broadcast a wire-visible Usage event so clients and observers
            // can track per-turn token consumption without parsing tracing
            // events. Also stashes the latest input_tokens in the engine
            // inner for token-based auto-compaction decisions.
            let _ = inner.events_tx().send(EngineEvent::Usage {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                input_tokens,
                output_tokens,
            });
            inner.set_last_input_tokens(input_tokens);
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
            if iteration + 1 >= max_iterations {
                let notice_id = ItemId(Arc::from(format!("item:{}/max-iter-fu", turn_id.0)));
                let notice = Item::SystemNotice {
                    id: notice_id,
                    level: NoticeLevel::Warn,
                    message: format!("max turn iterations reached ({max_iterations}); turn ended"),
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

        // Tool dispatch runs in three phases so that several tool calls in one
        // model turn execute CONCURRENTLY without firing multiple interactive
        // permission prompts at once:
        //   PHASE 1 (serial)   — resolve permission for each call in emit order.
        //   PHASE 2 (parallel) — execute every approved call concurrently.
        //   PHASE 3 (serial)   — push the finalized items in emit order.

        // ── PHASE 1: serial permission resolution (original emit order) ──────
        let mut resolved: Vec<Resolved> = Vec::with_capacity(new_tool_call_items.len());
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

            let resolution = resolve_tool_permission(
                inner,
                &hook_host,
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

            // A cancel observed during the (serial) permission wait means the
            // turn is being torn down. Finish without pushing any items.
            if cancel.is_cancelled() {
                inner.finish_turn(&handle, thread_id, turn_id, false).await;
                return None;
            }

            match resolution {
                ToolResolution::Blocked(outcome) => {
                    if outcome.stop_loop() {
                        stop_requested = true;
                    }
                    resolved.push(Resolved::Blocked(outcome));
                }
                ToolResolution::Approved { args, stop_loop } => {
                    if stop_loop {
                        stop_requested = true;
                    }
                    resolved.push(Resolved::Approved {
                        item_id: item_id.clone(),
                        tool_name: tool_name.clone(),
                        args,
                        tool_use_id,
                        stop_loop,
                    });
                }
            }
        }

        // ── PHASE 2: parallel execution of every approved call ───────────────
        //
        // Build one execute future per resolution (Blocked entries resolve
        // immediately to their already-final outcome), then await them all
        // concurrently via `join_all` over an ORDERED Vec so PHASE 3 can
        // reassemble results in the original model-emit order.
        let exec_futures = resolved.into_iter().map(|r| {
            let tools = &tools;
            let hook_host = &hook_host;
            let cancel = &cancel;
            let turn_id = &turn_id;
            // Borrow (do not move) the parent thread id so `thread_id_str`,
            // which borrows `thread_id.0`, stays valid for the other closures.
            let parent_thread_id = &thread_id;
            async move {
                match r {
                    Resolved::Blocked(outcome) => outcome,
                    Resolved::Approved {
                        item_id,
                        tool_name,
                        args,
                        tool_use_id,
                        stop_loop,
                    } => {
                        // Build a fresh spawner handle per approved call so the
                        // `agent` tool can delegate to a child agent. Cloning
                        // the engine handle is cheap (shared-ownership Arc). The
                        // child cannot recurse: `prepare_child_scope` rejects a
                        // spawn whose parent is itself a subagent.
                        let spawner: Option<Arc<dyn crate::tools::SubagentSpawner>> =
                            Some(Arc::new(EngineSubagentSpawner::new(
                                Arc::clone(inner),
                                parent_thread_id.clone(),
                            )));
                        execute_resolved_tool(
                            tools,
                            hook_host,
                            thread_id_str,
                            turn_id,
                            item_id,
                            &tool_name,
                            args,
                            &tool_use_id,
                            cancel,
                            stop_loop,
                            spawner,
                        )
                        .await
                    }
                }
            }
        });
        let outcomes: Vec<DispatchOutcome> = futures::future::join_all(exec_futures).await;

        // If the turn was cancelled while the tools were executing (a tool body
        // or PostToolUse lost its select! race), every outcome is an abandoned
        // result. Do NOT push or broadcast any of them — finish and exit.
        if cancel.is_cancelled() {
            inner.finish_turn(&handle, thread_id, turn_id, false).await;
            return None;
        }

        // ── PHASE 3: serial item commit (original emit order) ────────────────
        for outcome in outcomes {
            if outcome.stop_loop() {
                stop_requested = true;
            }

            // The finalized item already carries `provider_tool_call_id`, so
            // pushing it to the tail is all prompt reconstruction needs.
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
        if iteration + 1 >= max_iterations {
            let notice_id = ItemId(Arc::from(format!("item:{}/max-iter", turn_id.0)));
            let notice = Item::SystemNotice {
                id: notice_id,
                level: NoticeLevel::Warn,
                message: format!("max tool iterations reached ({max_iterations}); turn ended"),
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
    if handle.parent_thread_id.is_none() {
        let item_count = handle.items_tail.read().await.len();
        let last_input = inner.last_input_tokens();
        let token_threshold_hit = inner
            .compact_token_threshold()
            .is_some_and(|t| last_input >= t);
        let item_threshold_hit = item_count >= super::compaction::AUTO_COMPACT_ITEM_THRESHOLD;
        if token_threshold_hit || item_threshold_hit {
            let _ = inner
                .run_compaction(&handle, thread_id, CompactTrigger::Auto)
                .await;
        }
    }
    None
}

// Rust guideline compliant 2026-02-21

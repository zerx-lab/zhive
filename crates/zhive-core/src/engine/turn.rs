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

    // ── Persist the turn's INPUT items (the data-loss fix) ───────────────────
    //
    // `start_turn` / `start_child_turn` push the input items (user message,
    // next-turn seeds, subagent prompt) into the history buffer and broadcast
    // `ItemAppended`, but they do NOT enqueue a `StorageWriteOp::ItemAppended`.
    // Every other item the engine produces (agent messages, tool calls) is
    // persisted from this function, so the input items were the ONLY items
    // never written to the rollout / state.db — they silently vanished on
    // resume and crash recovery.
    //
    // Centralising the input-item persistence here (rather than in each
    // `start_*` site) keeps a single seq origin: these input items take seq
    // `0..N-1` and every subsequent agent item continues from `item_seq == N`,
    // so the `idx_items_turn (turn_id, seq)` ordering never collides.
    //
    // Ordering guarantee: `start_turn` enqueues `ThreadUpserted` (rollout
    // header) and `TurnStarted` synchronously BEFORE spawning the task that
    // runs this function, and the writer drains a single MPSC channel in send
    // order, so these input-item ops always land after the header/turn-start
    // ops. The same holds for the subagent path (`start_child_turn` enqueues
    // both ops before `run_child_turn_and_deliver` is spawned).
    //
    // Snapshot ONLY the active turn's items: at this point (before any provider
    // call) the active turn holds exactly the input items, while completed
    // turns hold prior turns that were already persisted and must not be
    // re-enqueued.
    for item in handle.active_turn_items().await {
        inner.enqueue_storage_op(StorageWriteOp::ItemAppended {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            seq: item_seq,
            item: Box::new(item),
        });
        item_seq += 1;
    }

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

        // Save point #1 (prepare_next_turn semantics): flush any session
        // writes that were buffered before this LLM request. Best-effort — a
        // flush failure is swallowed inside `flush_pending_session_writes`
        // because persistence must never abort the turn loop.
        let _ = inner.flush_pending_session_writes(&handle);

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
                inner.finish_turn(&handle, thread_id, turn_id, None).await;
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
                // Pass the failure to finish_turn so its TurnEnded op records the
                // error on the `turns` row; also return it for subagent callers.
                inner
                    .finish_turn(&handle, thread_id, turn_id, Some(turn_error.clone()))
                    .await;
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

        // Save point #4 (stream finally): flush deferred session writes once
        // the provider stream has drained, before deciding the iteration's
        // failure / cancel outcome.
        //
        // Skip the flush on the cancel path: cancellation is an abort, and the
        // abort path must NOT flush buffered session writes (they survive to the
        // next Idle save point). This matches `cancel_turn`, which also leaves
        // the pending buffer intact. A *failed* (non-cancel) turn still flushes
        // here so its buffered writes are not lost.
        if !cancel.is_cancelled() {
            let _ = inner.flush_pending_session_writes(&handle);
        }

        if failure.is_some() || cancel.is_cancelled() {
            // finish_turn handles the Turn→Idle rollback; passing the captured
            // failure (Some only on a stream error) suppresses TurnCompleted and
            // records the error on the persisted TurnEnded op. On cancellation
            // `failure` is None, so a cancelled turn is reported as non-failure.
            inner
                .finish_turn(&handle, thread_id, turn_id, failure.clone())
                .await;
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
                // A child (subagent) turn carries a `subagent_decision_tx`; its
                // tool calls route every non-deny decision to the parent
                // spawner for a second fold. A top-level turn has `None` here
                // and runs its own `Ask` / `Defer` reverse-RPC directly.
                handle.subagent_decision_tx.as_ref(),
            )
            .await;

            // A cancel observed during the (serial) permission wait means the
            // turn is being torn down. Finish without pushing any items.
            if cancel.is_cancelled() {
                inner.finish_turn(&handle, thread_id, turn_id, None).await;
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
            inner.finish_turn(&handle, thread_id, turn_id, None).await;
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
    //    emission. Pass `None` (normal, non-failing completion).
    inner
        .finish_turn(&handle, thread_id.clone(), turn_id, None)
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
        let item_count = handle.item_count().await;
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

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use llmsdk::language_model::StreamPart;
    use tokio::sync::broadcast;
    use zhive_proto::domain::{Item, ItemContent, ItemId, ThreadId, TurnId};

    use crate::engine::event::EngineEvent;
    use crate::engine::inner::EngineInner;
    use crate::persistence::Storage;
    use crate::persistence::writer::{PersistenceWriter, StorageWriteOp};
    use crate::provider::{DynLanguageModel, ScriptedModel};

    fn tid(s: &str) -> ThreadId {
        ThreadId(Arc::from(s))
    }

    /// A scripted model that emits a single text block ("ok") so a driven turn
    /// produces exactly one `AgentMessage` item after the input items.
    fn text_provider() -> DynLanguageModel {
        ScriptedModel::new(
            "scripted",
            "m",
            vec![
                StreamPart::TextStart {
                    id: "b0".into(),
                    provider_metadata: None,
                },
                StreamPart::TextDelta {
                    id: "b0".into(),
                    delta: "ok".into(),
                    provider_metadata: None,
                },
                StreamPart::TextEnd {
                    id: "b0".into(),
                    provider_metadata: None,
                },
            ],
        )
        .into_dyn()
    }

    /// Builds an engine inner backed by a real on-disk `Storage` and the given
    /// provider. Mirrors the scaffolding in `engine::fork` / `engine::resume`.
    async fn inner_with_storage(
        provider: DynLanguageModel,
    ) -> (Arc<EngineInner>, tempfile::TempDir, Arc<Storage>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Arc::new(Storage::open(dir.path()).await.expect("open storage"));
        let (tx, handle) = PersistenceWriter::spawn(Arc::clone(&storage));
        let (events_tx, _) = broadcast::channel::<EngineEvent>(64);
        let inner = Arc::new(EngineInner::new_with_hooks_tools_storage(
            events_tx,
            provider,
            Arc::new(crate::hooks::HookHost::new()),
            Arc::new(crate::tools::ToolRegistry::new()),
            crate::engine::TurnLimits::default(),
            None,
            Some(tx),
            Some(handle),
            None,
            Some(Arc::clone(&storage)),
            std::path::PathBuf::from("/turn/cwd"),
        ));
        (inner, dir, storage)
    }

    fn user_message(id: &str, text: &str) -> Item {
        Item::UserMessage {
            id: ItemId(Arc::from(id)),
            content: vec![ItemContent::Text {
                text: text.to_owned(),
                annotations: None,
            }],
        }
    }

    /// Drains the persistence writer deterministically: enqueue an ack'd
    /// `Flush` on `thread_id` and await it. Because the writer applies ops in
    /// order, the ack only fires once every op enqueued before it (including
    /// the user-input `ItemAppended` and the `TurnEnded` fsync) has been
    /// applied to live storage.
    async fn drain_writer(inner: &EngineInner, thread_id: &ThreadId) {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        inner.enqueue_storage_op(StorageWriteOp::Flush {
            thread_id: thread_id.clone(),
            ack: Some(ack_tx),
        });
        tokio::time::timeout(Duration::from_secs(5), ack_rx)
            .await
            .expect("flush ack must arrive")
            .expect("flush ack channel must not drop");
    }

    /// Waits for the engine bus to report `TurnCompleted` for `turn_id`.
    async fn await_turn_completed(rx: &mut broadcast::Receiver<EngineEvent>, turn_id: &TurnId) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let ev = tokio::time::timeout_at(deadline, rx.recv())
                .await
                .expect("turn must complete within timeout")
                .expect("event bus must not lag/close");
            if let EngineEvent::TurnCompleted { turn_id: t, .. } = ev
                && &t == turn_id
            {
                return;
            }
        }
    }

    /// Regression for the input-item data-loss bug: a top-level turn that
    /// carries a `UserMessage` must persist that item to BOTH the rollout
    /// (`read_all`) and the SQL index (`get_turn_items`), with `seq` continuous
    /// with the agent item that follows (user = 0, agent = 1).
    #[tokio::test]
    async fn user_message_persists_to_rollout_and_state_db() {
        use crate::persistence::{RolloutEntry, read_all};

        let (inner, _dir, storage) = inner_with_storage(text_provider()).await;
        let thread_id = tid("thread:native/user-persist");
        let mut events_rx = inner.events_tx().subscribe();

        let reply = inner
            .start_turn(
                thread_id.clone(),
                vec![user_message("item:user/0", "hello world")],
                None,
            )
            .await
            .expect("start_turn must accept the submission");
        let turn_id = reply.turn_id;

        await_turn_completed(&mut events_rx, &turn_id).await;
        drain_writer(&inner, &thread_id).await;

        // 1. SQL index: get_turn_items returns the user message FIRST, then the
        //    agent message, proving the seq is continuous (user=0, agent=1).
        let items = storage
            .state
            .get_turn_items(&turn_id)
            .await
            .expect("get_turn_items");
        assert_eq!(
            items.len(),
            2,
            "turn must persist the user message AND the agent reply, got {items:?}"
        );
        assert!(
            matches!(&items[0], Item::UserMessage { .. }),
            "the FIRST persisted item must be the user message (seq 0), got {:?}",
            items[0]
        );
        assert!(
            matches!(&items[1], Item::AgentMessage { .. }),
            "the SECOND persisted item must be the agent reply (seq 1), got {:?}",
            items[1]
        );

        // 2. Rollout JSONL (source of truth) also contains the user message.
        let rollout = storage.rollout_path(&thread_id.0);
        let entries = read_all(&rollout).await.expect("read_all rollout");
        let has_user = entries.iter().any(|e| {
            matches!(
                e,
                RolloutEntry::Item { item, .. } if matches!(item.as_ref(), Item::UserMessage { .. })
            )
        });
        assert!(
            has_user,
            "rollout must contain the UserMessage item entry; entries = {entries:?}"
        );

        // 3. Resume path: get_items(None) replays the FULL thread history from
        //    the rollout (source of truth), so a resumed session sees the user
        //    message exactly as a fresh client would on reconnect.
        let replayed = inner
            .get_items(thread_id.clone(), None, None, None)
            .await
            .expect("get_items full-history read");
        let replayed_user = replayed.iter().find_map(|i| match i {
            Item::UserMessage { content, .. } => content.iter().find_map(|c| match c {
                ItemContent::Text { text, .. } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        });
        assert_eq!(
            replayed_user.as_deref(),
            Some("hello world"),
            "resume (get_items) must return the persisted user message; got {replayed:?}"
        );
    }

    /// A subagent child turn's spawn prompt (`UserMessage`) must also be
    /// persisted to the child rollout — it was pushed in `start_child_turn`
    /// and is now picked up by `run_turn`'s input-item persistence.
    #[tokio::test]
    async fn subagent_prompt_persists_to_child_rollout() {
        use crate::persistence::{RolloutEntry, read_all};

        let (inner, _dir, storage) = inner_with_storage(text_provider()).await;
        let parent_id = tid("thread:native/sub-parent");
        let _parent = inner.threads().get_or_init(&parent_id).await;

        let definition: zhive_proto::permission::SubagentDefinition =
            serde_json::from_value(serde_json::json!({
                "name": "scout",
                "description": "probe",
                "prompt": "investigate the repo",
            }))
            .expect("definition fixture");

        let (child_id, mut final_rx, _decision_rx) = inner
            .spawn_subagent_awaitable(parent_id, definition)
            .await
            .expect("spawn must succeed");

        // Await the child's final event so the child turn (and its finish_turn
        // fsync) has run before we drain the writer.
        tokio::time::timeout(Duration::from_secs(5), final_rx.recv())
            .await
            .expect("child final event must arrive")
            .expect("child final channel must not close empty");
        drain_writer(&inner, &child_id).await;

        // The child rollout contains the prompt as a UserMessage item.
        let rollout = storage.rollout_path(&child_id.0);
        let entries = read_all(&rollout).await.expect("read_all child rollout");
        let prompt_text = entries.iter().find_map(|e| match e {
            RolloutEntry::Item { item, .. } => match item.as_ref() {
                Item::UserMessage { content, .. } => content.iter().find_map(|c| match c {
                    ItemContent::Text { text, .. } => Some(text.clone()),
                    _ => None,
                }),
                _ => None,
            },
            _ => None,
        });
        assert_eq!(
            prompt_text.as_deref(),
            Some("investigate the repo"),
            "child rollout must persist the subagent prompt UserMessage; entries = {entries:?}"
        );
    }
}

// Rust guideline compliant 2026-02-21

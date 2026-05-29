//! Turn execution logic for the engine actor.
//!
//! This module contains the streaming provider loop that runs inside the
//! dedicated `tokio::spawn` task created by
//! [`super::inner::EngineInner::start_turn`].  It is intentionally kept
//! separate from the actor-loop code in [`super::inner`] so that the two
//! concerns — actor dispatch / state management vs. per-turn LLM I/O —
//! each remain well under the 600-line soft limit.
//!
//! ## Inner tool-call loop
//!
//! `run_turn` is a **bounded loop** (cap: [`MAX_TURN_ITERATIONS`]):
//!
//! 1. Build `CallOptions` by reconstructing the prompt from the thread tail.
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
//!
//! ## Prompt mapping (Phase 1, documented)
//!
//! [`build_call_options`] reconstructs the full `llmsdk::Prompt` from the
//! thread's `items_tail` (the single source of truth) in item order:
//! - `Item::UserMessage` → `Message::User { content: [UserPart::Text(…)] }`
//! - `Item::AgentMessage` → `Message::Assistant { content: [AssistantPart::Text(…)] }`
//! - `Item::ToolCall { status: Completed | Failed, … }` → a
//!   `Message::Assistant { content: [AssistantPart::ToolCall(…)] }` carrying
//!   the original `provider_tool_call_id` (falling back to the item id), tool
//!   name and `raw_input`, **immediately followed** by a
//!   `Message::Tool { content: [ToolMessagePart::ToolResult(…)] }` with the
//!   *same* `tool_call_id` and the tool output. Pending / `InProgress`
//!   tool-call items (which never produced a result) are skipped.
//! - All other item kinds are skipped.
//!
//! Reconstructing from `items_tail` (rather than a side accumulator) keeps a
//! single source of truth and guarantees the invariant a real provider relies
//! on: every historical tool call appears as a matching `tool_use` /
//! `tool_result` id pair, in order, for **every** iteration — not just the
//! last one. The bounded tail (default 256 items) comfortably covers a turn's
//! 32-iteration cap, so intra-turn eviction is not a concern.
//!
//! If the resulting prompt is empty (no convertible history), the call is
//! still issued — the provider may generate a greeting or refuse; both
//! outcomes are valid.

use std::sync::Arc;

use futures::StreamExt as _;
use tokio_util::sync::CancellationToken;
use zhive_proto::domain::{Item, ItemId, NoticeLevel, ThreadId, TurnError, TurnId};
use zhive_proto::permission::PermissionScope;

use crate::provider::{ProviderError, StreamFold};
use crate::queues::QueueTarget;
use crate::state::ThreadHandle;

use super::event::EngineEvent;
use super::inner::EngineInner;
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
#[expect(
    clippy::too_many_lines,
    reason = "run_turn is the top-level turn state machine; splitting into smaller functions \
              would require passing many args through shared context structs, adding more \
              complexity than it removes. All arms are clear and sequential."
)]
pub(super) async fn run_turn(
    inner: &Arc<EngineInner>,
    handle: Arc<ThreadHandle>,
    thread_id: ThreadId,
    turn_id: TurnId,
    cancel: CancellationToken,
) {
    // Scope used for permission evaluation when none is supplied externally.
    // Phase 1: turns do not yet carry an explicit scope from the client.
    let scope = PermissionScope::default_turn_scope();

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
                    item: Box::new(item),
                });
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
                return;
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
                    error: turn_error,
                });
                inner.finish_turn(&handle, thread_id, turn_id, true).await;
                return;
            }
        };

        // 3. Stream loop with per-turn cancellation.
        let mut fold = StreamFold::new(&turn_id);
        let mut stream_failed = false;
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
                                        item: Box::new(item),
                                    });
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
                                error: turn_error,
                            });
                            stream_failed = true;
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
                    item: Box::new(item),
                });
            }
        }

        if stream_failed || cancel.is_cancelled() {
            // finish_turn handles the Turn→Idle rollback; pass failed=true
            // when the stream errored so TurnCompleted is not emitted.
            inner
                .finish_turn(&handle, thread_id, turn_id, stream_failed)
                .await;
            return;
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
                    item: Box::new(item),
                });
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
                    item: Box::new(notice),
                });
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
                return;
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
                item: Box::new(final_item),
            });
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
                item: Box::new(notice),
            });
            break 'outer;
        }
    }

    // 5. finish_turn handles the Turn→Idle rollback and TurnCompleted
    //    emission. Pass failed=false (normal completion).
    inner.finish_turn(&handle, thread_id, turn_id, false).await;
}

// ============================================================
// Prompt mapping
// ============================================================

/// Reconstructs a [`llmsdk::language_model::CallOptions`] from the thread tail.
///
/// Walks the thread's `items_tail` in order — the single source of truth —
/// and maps each item to provider messages:
///
/// - `Item::UserMessage` → `Message::User { [UserPart::Text] }`
/// - `Item::AgentMessage` → `Message::Assistant { [AssistantPart::Text] }`
/// - `Item::ToolCall { status: Completed | Failed, .. }` → a
///   `Message::Assistant { [AssistantPart::ToolCall] }` (the request the model
///   made) **immediately followed by** a `Message::Tool { [ToolResult] }` (the
///   output it received). Both parts share the same `tool_call_id`, taken from
///   the item's `provider_tool_call_id` (falling back to the item id when the
///   provider omitted one). Tool-call items still `Pending` / `InProgress`
///   never produced a result and are skipped.
/// - All other item kinds are skipped.
///
/// Because every completed tool call carries its `provider_tool_call_id`, the
/// reconstruction emits matching `tool_use` / `tool_result` id pairs, in
/// order, for **every** prior iteration — exactly what Anthropic/OpenAI
/// message-pair validation requires when replaying a multi-turn tool
/// conversation.
///
/// The returned `CallOptions` has all optional fields set to `None`
/// (no tool list, no `max_output_tokens`, no temperature override, …).
/// Provider defaults apply.
pub(super) async fn build_call_options(
    handle: &ThreadHandle,
) -> llmsdk::language_model::CallOptions {
    use llmsdk::language_model::{
        AssistantPart, Message, TextPart, ToolCallPart, ToolMessagePart, ToolResultOutput,
        ToolResultPart, UserPart,
    };
    use zhive_proto::domain::ToolCallStatus;

    let tail = handle.items_tail.read().await;
    let mut prompt: Vec<Message> = Vec::with_capacity(tail.len());

    for item in tail.iter() {
        match item {
            Item::UserMessage { content, .. } => {
                let text: String = content
                    .iter()
                    .filter_map(|c| match c {
                        zhive_proto::domain::ItemContent::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                prompt.push(Message::User {
                    content: vec![UserPart::Text(TextPart {
                        text,
                        provider_options: None,
                    })],
                    provider_options: None,
                });
            }
            Item::AgentMessage { text, .. } => {
                prompt.push(Message::Assistant {
                    content: vec![AssistantPart::Text(TextPart {
                        text: text.clone(),
                        provider_options: None,
                    })],
                    provider_options: None,
                });
            }
            // A finished tool call reconstructs into the required message pair:
            //   Message::Assistant { ToolCall }  — what the model asked for
            //   Message::Tool      { ToolResult } — what it got back
            // Both share one tool_call_id so a real provider can correlate
            // them. Pending / InProgress items have no result and are skipped.
            Item::ToolCall {
                id,
                name,
                status: ToolCallStatus::Completed | ToolCallStatus::Failed,
                content,
                raw_input,
                raw_output,
                provider_tool_call_id,
                ..
            } => {
                // Prefer the provider's original id; fall back to the item id
                // (which is itself derived from the provider block id in the
                // common case) so the pair always shares a non-empty id.
                let tool_call_id = provider_tool_call_id
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| id.0.to_string());

                prompt.push(Message::Assistant {
                    content: vec![AssistantPart::ToolCall(ToolCallPart {
                        tool_call_id: tool_call_id.clone(),
                        tool_name: name.clone(),
                        input: raw_input.clone().unwrap_or(serde_json::Value::Null),
                        provider_executed: None,
                        dynamic: None,
                        provider_options: None,
                    })],
                    provider_options: None,
                });

                prompt.push(Message::Tool {
                    content: vec![ToolMessagePart::ToolResult(ToolResultPart {
                        tool_call_id,
                        tool_name: name.clone(),
                        output: ToolResultOutput::Text {
                            value: tool_result_text(content, raw_output.as_ref()),
                            provider_options: None,
                        },
                        provider_options: None,
                    })],
                    provider_options: None,
                });
            }
            _ => {}
        }
    }
    drop(tail);

    llmsdk::language_model::CallOptions {
        prompt,
        ..Default::default()
    }
}

/// Extracts the tool-result text from a finalized `ToolCall` item.
///
/// Prefers the joined text of the item's `content` blocks (the human-readable
/// result the dispatch loop stored). Falls back to the JSON `raw_output` when
/// no text content is present, and finally to an empty string so the provider
/// always receives a non-`null` tool result.
fn tool_result_text(
    content: &[zhive_proto::domain::ItemToolCallContent],
    raw_output: Option<&serde_json::Value>,
) -> String {
    use zhive_proto::domain::{ItemContent, ItemToolCallContent};

    let text: String = content
        .iter()
        .filter_map(|c| match c {
            ItemToolCallContent::Content {
                content: ItemContent::Text { text, .. },
            } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    if !text.is_empty() {
        return text;
    }

    match raw_output {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use llmsdk::language_model::{AssistantPart, Message, ToolMessagePart, ToolResultOutput};
    use zhive_proto::domain::{
        Item, ItemContent, ItemId, ItemToolCallContent, ThreadId, ToolCallStatus, ToolKind,
    };

    use super::build_call_options;
    use crate::state::ThreadHandle;

    fn user_msg(id: &str, text: &str) -> Item {
        Item::UserMessage {
            id: ItemId(Arc::from(id)),
            content: vec![ItemContent::Text {
                text: text.to_owned(),
                annotations: None,
            }],
        }
    }

    fn completed_tool_call(id: &str, provider_id: &str, name: &str, result: &str) -> Item {
        Item::ToolCall {
            id: ItemId(Arc::from(id)),
            name: name.to_owned(),
            kind: ToolKind::Other,
            status: ToolCallStatus::Completed,
            content: vec![ItemToolCallContent::Content {
                content: ItemContent::Text {
                    text: result.to_owned(),
                    annotations: None,
                },
            }],
            locations: vec![],
            raw_input: Some(serde_json::json!({ "n": result })),
            raw_output: Some(serde_json::Value::String(result.to_owned())),
            provider_tool_call_id: Some(provider_id.to_owned()),
        }
    }

    /// A 3-iteration tool conversation reconstructs into matching
    /// `tool_use` / `tool_result` id pairs for **every** iteration, in order,
    /// using each item's `provider_tool_call_id`.
    #[tokio::test]
    async fn build_call_options_emits_id_pairs_for_every_iteration() {
        let handle = ThreadHandle::new_idle(ThreadId(Arc::from("thread:native/multi")));

        // Simulate three tool iterations interleaved with an assistant text
        // turn, mirroring what run_turn pushes to the tail across iterations.
        handle.push_item(user_msg("item:u0", "go")).await;
        handle
            .push_item(completed_tool_call("item:0", "toolu_A", "echo", "rA"))
            .await;
        handle
            .push_item(completed_tool_call("item:1", "toolu_B", "echo", "rB"))
            .await;
        handle
            .push_item(completed_tool_call("item:2", "toolu_C", "echo", "rC"))
            .await;

        let opts = build_call_options(&handle).await;

        // Collect (assistant tool_call_id, tool result tool_call_id) for each
        // pair in order. Every assistant ToolCall must be immediately followed
        // by a Tool ToolResult carrying the SAME id.
        let mut pairs: Vec<(String, String, String)> = Vec::new();
        let msgs = &opts.prompt;
        let mut i = 0;
        while i < msgs.len() {
            if let Message::Assistant { content, .. } = &msgs[i]
                && let Some(AssistantPart::ToolCall(tc)) = content.first()
            {
                // Next message must be the matching Tool result.
                let Some(Message::Tool {
                    content: tool_content,
                    ..
                }) = msgs.get(i + 1)
                else {
                    panic!("Assistant ToolCall not followed by a Tool message");
                };
                let ToolMessagePart::ToolResult(tr) =
                    tool_content.first().expect("tool result part")
                else {
                    panic!("expected ToolResult part");
                };
                let result_text = match &tr.output {
                    ToolResultOutput::Text { value, .. } => value.clone(),
                    other => panic!("expected Text output, got {other:?}"),
                };
                pairs.push((
                    tc.tool_call_id.clone(),
                    tr.tool_call_id.clone(),
                    result_text,
                ));
                i += 2;
                continue;
            }
            i += 1;
        }

        assert_eq!(pairs.len(), 3, "one matched pair per iteration");
        // The ids must come from provider_tool_call_id, in order, and match
        // within each pair.
        let expected = [("toolu_A", "rA"), ("toolu_B", "rB"), ("toolu_C", "rC")];
        for (idx, ((use_id, result_id, result_text), (exp_id, exp_text))) in
            pairs.iter().zip(expected.iter()).enumerate()
        {
            assert_eq!(use_id, exp_id, "iteration {idx}: tool_use id");
            assert_eq!(
                use_id, result_id,
                "iteration {idx}: tool_use and tool_result ids must match"
            );
            assert_eq!(result_text, exp_text, "iteration {idx}: result text");
        }
    }

    /// When `provider_tool_call_id` is absent, reconstruction falls back to the
    /// item id, and the assistant/tool pair still shares one id.
    #[tokio::test]
    async fn build_call_options_falls_back_to_item_id_when_provider_id_absent() {
        let handle = ThreadHandle::new_idle(ThreadId(Arc::from("thread:native/fallback")));

        handle
            .push_item(Item::ToolCall {
                id: ItemId(Arc::from("item:fallback-0")),
                name: "echo".to_owned(),
                kind: ToolKind::Other,
                status: ToolCallStatus::Completed,
                content: vec![ItemToolCallContent::Content {
                    content: ItemContent::Text {
                        text: "result".to_owned(),
                        annotations: None,
                    },
                }],
                locations: vec![],
                raw_input: Some(serde_json::json!({})),
                raw_output: None,
                provider_tool_call_id: None,
            })
            .await;

        let opts = build_call_options(&handle).await;
        assert_eq!(opts.prompt.len(), 2, "one Assistant + one Tool message");

        let use_id = match &opts.prompt[0] {
            Message::Assistant { content, .. } => match content.first() {
                Some(AssistantPart::ToolCall(tc)) => tc.tool_call_id.clone(),
                other => panic!("expected ToolCall, got {other:?}"),
            },
            other => panic!("expected Assistant, got {other:?}"),
        };
        let result_id = match &opts.prompt[1] {
            Message::Tool { content, .. } => match content.first() {
                Some(ToolMessagePart::ToolResult(tr)) => tr.tool_call_id.clone(),
                other => panic!("expected ToolResult, got {other:?}"),
            },
            other => panic!("expected Tool, got {other:?}"),
        };

        assert_eq!(use_id, "item:fallback-0", "falls back to item id");
        assert_eq!(use_id, result_id, "pair shares the fallback id");
    }

    /// A `Failed` (blocked/denied) tool call is still replayed as a matching
    /// pair so the provider sees a tool result for every request it made.
    #[tokio::test]
    async fn build_call_options_replays_failed_tool_call() {
        let handle = ThreadHandle::new_idle(ThreadId(Arc::from("thread:native/failed")));

        handle
            .push_item(Item::ToolCall {
                id: ItemId(Arc::from("item:denied-0")),
                name: "echo".to_owned(),
                kind: ToolKind::Other,
                status: ToolCallStatus::Failed,
                content: vec![ItemToolCallContent::Content {
                    content: ItemContent::Text {
                        text: "permission denied".to_owned(),
                        annotations: None,
                    },
                }],
                locations: vec![],
                raw_input: Some(serde_json::json!({})),
                raw_output: None,
                provider_tool_call_id: Some("toolu_denied".to_owned()),
            })
            .await;

        let opts = build_call_options(&handle).await;
        assert_eq!(opts.prompt.len(), 2, "failed call still yields a pair");
        match (&opts.prompt[0], &opts.prompt[1]) {
            (Message::Assistant { .. }, Message::Tool { content, .. }) => {
                let ToolMessagePart::ToolResult(tr) = content.first().expect("result part") else {
                    panic!("expected ToolResult");
                };
                match &tr.output {
                    ToolResultOutput::Text { value, .. } => {
                        assert_eq!(value, "permission denied");
                    }
                    other => panic!("expected Text output, got {other:?}"),
                }
            }
            other => panic!("expected (Assistant, Tool) pair, got {other:?}"),
        }
    }

    /// `Pending` / `InProgress` tool-call items (no result yet) are skipped so
    /// the prompt never contains an orphan `tool_use` without a matching result.
    #[tokio::test]
    async fn build_call_options_skips_in_progress_tool_calls() {
        let handle = ThreadHandle::new_idle(ThreadId(Arc::from("thread:native/inprog")));

        handle.push_item(user_msg("item:u0", "hi")).await;
        handle
            .push_item(Item::ToolCall {
                id: ItemId(Arc::from("item:inprog-0")),
                name: "echo".to_owned(),
                kind: ToolKind::Other,
                status: ToolCallStatus::InProgress,
                content: vec![],
                locations: vec![],
                raw_input: Some(serde_json::json!({})),
                raw_output: None,
                provider_tool_call_id: Some("toolu_inprog".to_owned()),
            })
            .await;

        let opts = build_call_options(&handle).await;
        // Only the user message survives; the in-progress tool call is skipped.
        assert_eq!(opts.prompt.len(), 1);
        assert!(matches!(opts.prompt[0], Message::User { .. }));
    }
}

// Rust guideline compliant 2026-02-21

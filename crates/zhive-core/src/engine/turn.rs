//! Turn execution logic for the engine actor.
//!
//! This module contains the streaming provider loop that runs inside the
//! dedicated `tokio::spawn` task created by
//! [`super::inner::EngineInner::start_turn`].  It is intentionally kept
//! separate from the actor-loop code in [`super::inner`] so that the two
//! concerns — actor dispatch / state management vs. per-turn LLM I/O —
//! each remain well under the 600-line soft limit.
//!
//! ## Prompt mapping (Phase 1, documented deviations)
//!
//! [`build_call_options`] maps the thread's `items_tail` to
//! `llmsdk::Prompt` with the following Phase-1 rules:
//! - `Item::UserMessage` → `Message::User { content: [UserPart::Text(…)] }`
//! - `Item::AgentMessage` → `Message::Assistant { content: [AssistantPart::Text(…)] }`
//! - All other item kinds are skipped (no tool results, no context items, …).
//!
//! If the resulting prompt is empty (no convertible history), the call is
//! still issued — the provider may generate a greeting or refuse; both
//! outcomes are valid.

use std::sync::Arc;

use futures::StreamExt as _;
use tokio_util::sync::CancellationToken;
use zhive_proto::domain::{Item, ThreadId, TurnError, TurnId};

use crate::provider::{ProviderError, StreamFold};
use crate::state::ThreadHandle;

use super::event::EngineEvent;
use super::inner::EngineInner;

// ============================================================
// Turn execution
// ============================================================

/// Drives the provider streaming loop for one turn.
///
/// Runs in a dedicated `tokio::spawn` task; holds no lock across await
/// points so `cancel_turn` can take the `active_turn` slot at any time.
///
/// ## Steps
///
/// 1. Build `CallOptions` from the thread history (Phase-1 prompt mapping).
/// 2. Call `provider.do_stream(call_options)`.  On outer `Err` → `TurnFailed`.
/// 3. Stream loop: `tokio::select!` on `cancel.cancelled()` vs next part.
///    On each part: fold → push items → emit `ItemAppended`.
///    On inner stream error: `TurnFailed` + finish.
/// 4. After the stream ends or on cancel: call `fold.finish()` then
///    `finish_turn`.  Cancel does *not* emit `TurnCompleted` — that is
///    handled by `cancel_turn` (which emits `SessionAborted`).
pub(super) async fn run_turn(
    inner: &Arc<EngineInner>,
    handle: Arc<ThreadHandle>,
    thread_id: ThreadId,
    turn_id: TurnId,
    cancel: CancellationToken,
) {
    // 1. Build the prompt from the thread's item tail (Phase-1 mapping).
    let call_options = build_call_options(&handle).await;

    // 2. Call the provider.
    let stream_result = inner.provider().do_stream(call_options).await;
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
            // Pass failed=true so finish_turn does NOT emit TurnCompleted
            // after TurnFailed (protocol: a turn has exactly one terminal
            // event — either TurnCompleted or TurnFailed, never both).
            inner.finish_turn(&handle, thread_id, turn_id, true).await;
            return;
        }
    };

    // 3. Stream loop with per-turn cancellation.
    let mut fold = StreamFold::new(&turn_id);
    // Tracks whether the stream ended with an in-stream error so the
    // finish path can call finish_turn(failed=true) and avoid emitting
    // TurnCompleted after TurnFailed.
    let mut stream_failed = false;

    loop {
        tokio::select! {
            // Cancel token wins: stop consuming the stream. The
            // finish path below will call finish_turn, which is
            // gated by `we_owned_the_turn`. Because cancel_turn
            // takes the active_turn slot and emits SessionAborted,
            // finish_turn will see `we_owned_the_turn = false` and
            // skip the TurnCompleted emission.
            () = cancel.cancelled() => {
                break;
            }
            maybe_part = stream.next() => {
                match maybe_part {
                    None => break, // stream exhausted normally
                    Some(Ok(part)) => {
                        for item in fold.fold(part) {
                            handle.push_item(item.clone()).await;
                            let _ = inner.events_tx().send(EngineEvent::ItemAppended {
                                thread_id: thread_id.clone(),
                                turn_id: turn_id.clone(),
                                item: Box::new(item),
                            });
                        }
                    }
                    Some(Err(err)) => {
                        // In-stream provider error: emit TurnFailed, then
                        // break to the finish path (which calls fold.finish
                        // and then finish_turn(failed=true)).
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

    // Always drain any open fold buffers — items buffered before a
    // cancel or mid-stream error fired must not be silently dropped.
    // The spec's only carve-out for cancel is that TurnCompleted must
    // not be emitted; partial items that the provider already sent are
    // still pushed to the thread store and broadcast as ItemAppended.
    for item in fold.finish() {
        handle.push_item(item.clone()).await;
        let _ = inner.events_tx().send(EngineEvent::ItemAppended {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            item: Box::new(item),
        });
    }

    // 4. finish_turn handles the Turn→Idle rollback and TurnCompleted
    //    emission. Pass failed=true when the stream errored so that
    //    TurnCompleted is not emitted after TurnFailed.
    //    When cancelled, finish_turn sees we_owned_the_turn=false
    //    (cancel_turn already cleared the slot) and skips both rollback
    //    and TurnCompleted automatically — failed flag is redundant but
    //    harmless there.
    inner
        .finish_turn(&handle, thread_id, turn_id, stream_failed)
        .await;
}

// ============================================================
// Prompt mapping
// ============================================================

/// Builds a minimal [`llmsdk::language_model::CallOptions`] from the
/// thread's current `items_tail`.
///
/// ## Phase-1 mapping (documented)
///
/// - `Item::UserMessage { text, .. }` → `Message::User { content: [UserPart::Text(TextPart { text, provider_options: None })] }`
/// - `Item::AgentMessage { text, .. }` → `Message::Assistant { content: [AssistantPart::Text(TextPart { text, provider_options: None })] }`
/// - All other item kinds are skipped. Tool results, context items,
///   reasoning, and notices are not sent to the provider in Phase 1.
///
/// The returned `CallOptions` has all optional fields set to `None`
/// (no tool list, no `max_output_tokens`, no temperature override, …).
/// Provider defaults apply.
pub(super) async fn build_call_options(
    handle: &ThreadHandle,
) -> llmsdk::language_model::CallOptions {
    use llmsdk::language_model::{AssistantPart, Message, TextPart, UserPart};

    let tail = handle.items_tail.read().await;
    let prompt: Vec<Message> = tail
        .iter()
        .filter_map(|item| match item {
            Item::UserMessage { content, .. } => {
                // Phase-1 mapping: concatenate all text parts into one
                // llmsdk TextPart. Non-text content (images, audio, …)
                // is skipped in Phase 1.
                let text: String = content
                    .iter()
                    .filter_map(|c| match c {
                        zhive_proto::domain::ItemContent::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                Some(Message::User {
                    content: vec![UserPart::Text(TextPart {
                        text,
                        provider_options: None,
                    })],
                    provider_options: None,
                })
            }
            Item::AgentMessage { text, .. } => Some(Message::Assistant {
                content: vec![AssistantPart::Text(TextPart {
                    text: text.clone(),
                    provider_options: None,
                })],
                provider_options: None,
            }),
            _ => None,
        })
        .collect();
    drop(tail);

    llmsdk::language_model::CallOptions {
        prompt,
        ..Default::default()
    }
}

// Rust guideline compliant 2026-02-21

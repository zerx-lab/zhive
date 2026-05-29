//! Prompt construction for the LLM provider.
//!
//! Reconstructs a [`llmsdk::language_model::CallOptions`] from the thread's
//! `items_tail` (the single source of truth) in item order.  Extracted from
//! [`super::turn`] to keep both files under the 600-line soft limit.
//!
//! ## Mapping rules
//!
//! - `Item::UserMessage` → `Message::User { content: [UserPart::Text(…)] }`
//! - `Item::AgentMessage` → `Message::Assistant { content: [AssistantPart::Text(…)] }`
//! - `Item::ToolCall { status: Completed | Failed, … }` → a
//!   `Message::Assistant { [AssistantPart::ToolCall] }` **immediately followed by**
//!   `Message::Tool { [ToolResult] }`.  Both parts share the same `tool_call_id`,
//!   taken from the item's `provider_tool_call_id` (falling back to the item id).
//!   Tool-call items still `Pending` / `InProgress` are skipped.
//! - All other item kinds are skipped.
//!
//! Because every completed tool call carries its `provider_tool_call_id`, the
//! reconstruction emits matching `tool_use` / `tool_result` id pairs, in order,
//! for every prior iteration — exactly what Anthropic/OpenAI message-pair
//! validation requires.

use zhive_proto::domain::Item;

use crate::state::ThreadHandle;

// ============================================================
// Public API
// ============================================================

/// Reconstructs a [`llmsdk::language_model::CallOptions`] from the thread tail.
///
/// Walks the thread's `items_tail` in order — the single source of truth —
/// and maps each item to provider messages using the rules documented in the
/// module header.
///
/// The returned `CallOptions` has all optional fields set to `None`
/// (no tool list, no `max_output_tokens`, no temperature override).
/// Provider defaults apply.
///
/// This function is crate-internal (`pub(in crate::engine)`).
/// See the `#[cfg(test)]` block in this file for usage examples.
pub(in crate::engine) async fn build_call_options(
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

// ============================================================
// Private helpers
// ============================================================

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

        let mut pairs: Vec<(String, String, String)> = Vec::new();
        let msgs = &opts.prompt;
        let mut i = 0;
        while i < msgs.len() {
            if let Message::Assistant { content, .. } = &msgs[i]
                && let Some(AssistantPart::ToolCall(tc)) = content.first()
            {
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
        assert_eq!(opts.prompt.len(), 1);
        assert!(matches!(opts.prompt[0], Message::User { .. }));
    }
}

// Rust guideline compliant 2026-02-21

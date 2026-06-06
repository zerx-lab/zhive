//! Prompt construction for the LLM provider.
//!
//! Reconstructs a [`llmsdk::language_model::CallOptions`] from the thread's
//! resident transcript (the single source of truth) in item order.  Extracted
//! from [`super::turn`] to keep both files under the 600-line soft limit.
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
//!
//! ## Tool advertisement
//!
//! When the engine's [`crate::tools::ToolRegistry`] is non-empty, the built
//! `CallOptions` also carries `tools` (one `Tool::Function` per registered
//! tool) and `tool_choice = ToolChoice::Auto`, so the provider knows which
//! tools the model may call. An empty registry leaves both fields `None`.

use zhive_proto::domain::Item;
use zhive_proto::permission::PermissionScope;

use crate::state::ThreadHandle;

// ============================================================
// Public API
// ============================================================

/// Reconstructs a [`llmsdk::language_model::CallOptions`] from the thread tail.
///
/// Walks the thread's resident transcript in order — the single source of
/// truth — and maps each item to provider messages using the rules documented
/// in the module header.
///
/// When `tools` is non-empty, the returned `CallOptions` advertises each
/// registered tool via `tools = Some(vec![Tool::Function(..)])` and sets
/// `tool_choice = Some(ToolChoice::Auto)` so the model may pick a tool. A
/// tool whose `input_schema` cannot be converted to a provider `JsonSchema`
/// is logged and skipped rather than failing the whole turn. An empty
/// registry leaves both `tools` and `tool_choice` as `None`. All other
/// optional fields (`max_output_tokens`, `temperature`, …) stay `None`, so
/// provider defaults apply.
///
/// When `system_prompt` is `Some` and non-empty, it is prepended as the
/// leading [`llmsdk::language_model::Message::System`] so the provider applies
/// it to the whole conversation; an empty or `None` system prompt is omitted.
///
/// Only tools the turn's `scope` permits ([`PermissionScope::permits`]) are
/// advertised, so a subagent (or a scope with `disallowed_tools` /
/// `allowed_tools`) does not see tools it cannot call. This mirrors — but does
/// not replace — the authoritative dispatch-side scope gate.
///
/// This function is crate-internal (`pub(in crate::engine)`).
/// See the `#[cfg(test)]` block in this file for usage examples.
#[expect(
    clippy::too_many_lines,
    reason = "single-pass message assembly; refactoring into smaller fns would scatter the protocol logic"
)]
pub(in crate::engine) async fn build_call_options(
    handle: &ThreadHandle,
    tools: &crate::tools::ToolRegistry,
    system_prompt: Option<&str>,
    scope: &PermissionScope,
) -> llmsdk::language_model::CallOptions {
    use llmsdk::language_model::{
        AssistantPart, Message, TextPart, ToolCallPart, ToolMessagePart, ToolResultOutput,
        ToolResultPart, UserPart,
    };
    use zhive_proto::domain::ToolCallStatus;

    // Flat snapshot of every resident item across completed + active turns
    // (the single source of truth; see `ThreadHandle::items_snapshot`).
    let tail = handle.items_snapshot().await;
    // Reserve room for the optional leading system message plus one entry per
    // item (tool-call items expand to two, but this is only a hint).
    let mut prompt: Vec<Message> = Vec::with_capacity(tail.len() + 1);

    // The system instruction, when configured, must be the first message so a
    // provider applies it to the whole conversation.
    if let Some(system) = system_prompt.filter(|s| !s.is_empty()) {
        prompt.push(Message::System {
            content: system.to_owned(),
            provider_options: None,
        });
    }

    for item in &tail {
        match item {
            Item::UserMessage { content, .. } => {
                use llmsdk::language_model::{FilePart, UserPart as UP};
                use llmsdk::shared::{FileBytes, FileData};
                use zhive_proto::domain::ItemContent;

                let mut parts: Vec<UserPart> = Vec::new();
                for block in content {
                    match block {
                        ItemContent::Text { text, .. } if !text.is_empty() => {
                            parts.push(UP::Text(TextPart {
                                text: text.clone(),
                                provider_options: None,
                            }));
                        }
                        ItemContent::Image {
                            data, mime_type, ..
                        } => {
                            parts.push(UP::File(FilePart {
                                filename: None,
                                data: FileData::Data {
                                    data: FileBytes::Base64(data.clone()),
                                },
                                media_type: mime_type.clone(),
                                provider_options: None,
                            }));
                        }
                        _ => {}
                    }
                }
                if parts.is_empty() {
                    // Preserve the existing behaviour of sending an empty text
                    // block rather than dropping the message entirely, so a
                    // pure-attachment turn that arrives with no text still
                    // produces a well-formed message pair.
                    parts.push(UP::Text(TextPart {
                        text: String::new(),
                        provider_options: None,
                    }));
                }
                prompt.push(Message::User {
                    content: parts,
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
                status: status @ (ToolCallStatus::Completed | ToolCallStatus::Failed),
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
                        // `tool_use.input` MUST be a JSON object: the Anthropic
                        // Messages API rejects a string/null with
                        // "tool_use.input: Input should be an object". An
                        // argument-less or malformed call reconstructs as `{}`.
                        input: match raw_input {
                            Some(value @ serde_json::Value::Object(_)) => value.clone(),
                            _ => serde_json::json!({}),
                        },
                        provider_executed: None,
                        dynamic: None,
                        provider_options: None,
                    })],
                    provider_options: None,
                });

                // A failed tool call is sent back as `ErrorText` so providers
                // that distinguish error results (Anthropic's `is_error: true`)
                // see the failure instead of treating it as a normal result.
                let result_value = tool_result_text(content, raw_output.as_ref());
                let output = if matches!(status, ToolCallStatus::Failed) {
                    ToolResultOutput::ErrorText {
                        value: result_value,
                        provider_options: None,
                    }
                } else {
                    ToolResultOutput::Text {
                        value: result_value,
                        provider_options: None,
                    }
                };
                prompt.push(Message::Tool {
                    content: vec![ToolMessagePart::ToolResult(ToolResultPart {
                        tool_call_id,
                        tool_name: name.clone(),
                        output,
                        provider_options: None,
                    })],
                    provider_options: None,
                });
            }
            _ => {}
        }
    }
    drop(tail);

    // Advertise registered tools (if any). An empty result leaves both fields
    // `None` so an empty registry is a no-op (no behavior change).
    let advertised = build_tool_advertisements(tools, scope);
    let (tools_opt, tool_choice) = if advertised.is_empty() {
        (None, None)
    } else {
        (
            Some(advertised),
            Some(llmsdk::language_model::ToolChoice::Auto),
        )
    };

    llmsdk::language_model::CallOptions {
        prompt,
        tools: tools_opt,
        tool_choice,
        ..Default::default()
    }
}

// ============================================================
// Private helpers
// ============================================================

/// Builds the provider `Tool::Function` list from the registry's specs.
///
/// Tools the `scope` forbids ([`PermissionScope::permits`]) are skipped so the
/// model is not offered tools it cannot call. Each remaining spec's
/// `input_schema` is converted to a provider `JsonSchema`; a tool whose schema
/// is not a valid JSON schema is logged (`zhive.prompt.tool_schema_invalid`)
/// and skipped so one bad tool never aborts the turn. Returns an empty vec when
/// the registry is empty (or every tool is filtered / invalid).
fn build_tool_advertisements(
    tools: &crate::tools::ToolRegistry,
    scope: &PermissionScope,
) -> Vec<llmsdk::language_model::Tool> {
    let mut advertised: Vec<llmsdk::language_model::Tool> = Vec::new();
    for spec in tools.specs() {
        // Defense-in-depth + UX: do not advertise a tool the scope forbids
        // (the dispatch gate is the authoritative block).
        if !scope.permits(&spec.name) {
            continue;
        }
        match serde_json::from_value::<llmsdk::json::JsonSchema>(spec.input_schema.clone()) {
            Ok(input_schema) => {
                advertised.push(llmsdk::language_model::Tool::Function(
                    llmsdk::language_model::FunctionTool {
                        name: spec.name,
                        description: spec.description,
                        input_schema,
                        input_examples: None,
                        strict: None,
                        provider_options: None,
                    },
                ));
            }
            Err(err) => {
                tracing::warn!(
                    name: "zhive.prompt.tool_schema_invalid",
                    tool = %spec.name,
                    error = %err,
                    "tool input schema is not a valid JSON schema; skipping tool advertisement"
                );
            }
        }
    }
    advertised
}

// ============================================================
// Tool-result truncation constants
// ============================================================

/// Maximum UTF-8 bytes of a single tool result embedded into the provider
/// prompt.
///
/// 16 KiB keeps even a dozen large tool results well inside a 32k-token
/// budget while preserving enough head + tail context for the model to reason
/// about the output.  Changing this constant only affects what is sent to the
/// provider; the canonical tool output in the rollout / SQL index is never
/// truncated (see §B3 of the Wave 3 spec: prompt is a derived view, not the
/// source of truth).
const MAX_TOOL_RESULT_BYTES: usize = 16 * 1024; // 16 384 bytes

/// UTF-8 bytes of the tool result head (oldest output) kept on truncation.
///
/// Set to half the maximum so the elision marker fits comfortably in the gap
/// between head and tail.  Must satisfy `HEAD + TAIL < MAX`.
const TOOL_RESULT_HEAD_BYTES: usize = 8 * 1024; // 8 192 bytes

/// UTF-8 bytes of the tool result tail (newest output) kept on truncation.
///
/// Smaller than the head because newer lines tend to be more diagnostic and
/// are already visible via `TOOL_RESULT_HEAD_BYTES` overlap avoidance.
/// Must satisfy `HEAD + TAIL < MAX` (8 192 + 4 096 = 12 288 < 16 384 ✓).
const TOOL_RESULT_TAIL_BYTES: usize = 4 * 1024; // 4 096 bytes

/// Extracts the tool-result text from a finalized `ToolCall` item.
///
/// Prefers the joined text of the item's `content` blocks (the human-readable
/// result the dispatch loop stored). Falls back to the JSON `raw_output` when
/// no text content is present, and finally to an empty string so the provider
/// always receives a non-`null` tool result.
///
/// The returned text is passed through [`truncate_tool_result`] so that very
/// large outputs (bash on a big file, read of a multi-MB log) are capped at
/// [`MAX_TOOL_RESULT_BYTES`] before reaching the provider.
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

    let full = if text.is_empty() {
        match raw_output {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        }
    } else {
        text
    };
    truncate_tool_result(full)
}

/// Truncates an over-long tool result for inclusion in the provider prompt.
///
/// Returns `s` unchanged when `s.len() <= MAX_TOOL_RESULT_BYTES`.  Otherwise
/// keeps [`TOOL_RESULT_HEAD_BYTES`] from the start and [`TOOL_RESULT_TAIL_BYTES`]
/// from the end, with an elision marker in between that states the exact byte
/// counts so the model can gauge how much was omitted.
///
/// All splits are clamped to UTF-8 character boundaries via
/// [`utf8_floor_boundary`] so the returned `String` is always valid Unicode.
/// `tool_call_id` pairing is unaffected because the id lives in the
/// `ToolResultPart` wrapper, not in the text content.
fn truncate_tool_result(s: String) -> String {
    if s.len() <= MAX_TOOL_RESULT_BYTES {
        return s;
    }
    // Find safe UTF-8 cut points: floor (round down) for head, ceil (round up)
    // for tail so we never include a partial multi-byte char at either boundary.
    let head_end = utf8_floor_boundary(&s, TOOL_RESULT_HEAD_BYTES);
    let tail_raw = s.len().saturating_sub(TOOL_RESULT_TAIL_BYTES);
    // tail_start must be >= head_end to avoid an inverted range.
    let tail_start = utf8_ceil_boundary(&s, tail_raw.max(head_end));

    let omitted = tail_start.saturating_sub(head_end);
    format!(
        "{}\n\n[... truncated {omitted} bytes; showing first {head_end} + last {} of {} bytes ...]\n\n{}",
        &s[..head_end],
        s.len() - tail_start,
        s.len(),
        &s[tail_start..]
    )
}

/// Returns the largest byte index `<= pos` that falls on a UTF-8 char boundary.
///
/// Clamps to `[0, s.len()]`; always returns a valid slice index.
/// `str::is_char_boundary` is available on all Rust versions.
fn utf8_floor_boundary(s: &str, pos: usize) -> usize {
    let mut b = pos.min(s.len());
    while b > 0 && !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}

/// Returns the smallest byte index `>= pos` that falls on a UTF-8 char boundary.
///
/// Clamps to `[0, s.len()]`; `s.len()` is always a valid boundary (end of string).
fn utf8_ceil_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut b = pos;
    while b < s.len() && !s.is_char_boundary(b) {
        b += 1;
    }
    b
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

    use zhive_proto::permission::PermissionScope;

    use super::build_call_options;
    use crate::state::ThreadHandle;

    /// Builds an idle handle with one active turn so `push_item` has a turn to
    /// append to (the engine seeds the turn before pushing in production).
    async fn seeded_handle(id: &str) -> ThreadHandle {
        let handle = ThreadHandle::new_idle(ThreadId(Arc::from(id)));
        handle
            .start_turn_buffer(zhive_proto::domain::TurnId(Arc::from("turn:test/0")), 0)
            .await;
        handle
    }

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
            title: None,
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
        let handle = seeded_handle("thread:native/multi").await;

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

        let opts = build_call_options(
            &handle,
            &crate::tools::ToolRegistry::new(),
            None,
            &PermissionScope::default_turn_scope(),
        )
        .await;

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
        let handle = seeded_handle("thread:native/fallback").await;

        handle
            .push_item(Item::ToolCall {
                id: ItemId(Arc::from("item:fallback-0")),
                name: "echo".to_owned(),
                title: None,
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

        let opts = build_call_options(
            &handle,
            &crate::tools::ToolRegistry::new(),
            None,
            &PermissionScope::default_turn_scope(),
        )
        .await;
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
        let handle = seeded_handle("thread:native/failed").await;

        handle
            .push_item(Item::ToolCall {
                id: ItemId(Arc::from("item:denied-0")),
                name: "echo".to_owned(),
                title: None,
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

        let opts = build_call_options(
            &handle,
            &crate::tools::ToolRegistry::new(),
            None,
            &PermissionScope::default_turn_scope(),
        )
        .await;
        assert_eq!(opts.prompt.len(), 2, "failed call still yields a pair");
        match (&opts.prompt[0], &opts.prompt[1]) {
            (Message::Assistant { .. }, Message::Tool { content, .. }) => {
                let ToolMessagePart::ToolResult(tr) = content.first().expect("result part") else {
                    panic!("expected ToolResult");
                };
                // A Failed tool call must serialize as ErrorText (is_error).
                match &tr.output {
                    ToolResultOutput::ErrorText { value, .. } => {
                        assert_eq!(value, "permission denied");
                    }
                    other => panic!("expected ErrorText output, got {other:?}"),
                }
            }
            other => panic!("expected (Assistant, Tool) pair, got {other:?}"),
        }
    }

    /// `Pending` / `InProgress` tool-call items (no result yet) are skipped so
    /// the prompt never contains an orphan `tool_use` without a matching result.
    #[tokio::test]
    async fn build_call_options_skips_in_progress_tool_calls() {
        let handle = seeded_handle("thread:native/inprog").await;

        handle.push_item(user_msg("item:u0", "hi")).await;
        handle
            .push_item(Item::ToolCall {
                id: ItemId(Arc::from("item:inprog-0")),
                name: "echo".to_owned(),
                title: None,
                kind: ToolKind::Other,
                status: ToolCallStatus::InProgress,
                content: vec![],
                locations: vec![],
                raw_input: Some(serde_json::json!({})),
                raw_output: None,
                provider_tool_call_id: Some("toolu_inprog".to_owned()),
            })
            .await;

        let opts = build_call_options(
            &handle,
            &crate::tools::ToolRegistry::new(),
            None,
            &PermissionScope::default_turn_scope(),
        )
        .await;
        assert_eq!(opts.prompt.len(), 1);
        assert!(matches!(opts.prompt[0], Message::User { .. }));
    }

    /// A non-empty `system_prompt` becomes the leading `Message::System`,
    /// ahead of the reconstructed conversation; `None`/empty adds nothing.
    #[tokio::test]
    async fn build_call_options_prepends_system_prompt() {
        let handle = seeded_handle("thread:native/sys").await;
        handle.push_item(user_msg("item:u0", "hi")).await;

        // With a system prompt: it is the first message, before the user turn.
        let with_sys = build_call_options(
            &handle,
            &crate::tools::ToolRegistry::new(),
            Some("You are zhive."),
            &PermissionScope::default_turn_scope(),
        )
        .await;
        assert!(
            matches!(&with_sys.prompt[0], Message::System { content, .. } if content == "You are zhive."),
            "system prompt must be the leading message"
        );
        assert!(matches!(with_sys.prompt[1], Message::User { .. }));

        // Without a system prompt (None): no System message is emitted.
        let no_sys = build_call_options(
            &handle,
            &crate::tools::ToolRegistry::new(),
            None,
            &PermissionScope::default_turn_scope(),
        )
        .await;
        assert!(
            !no_sys
                .prompt
                .iter()
                .any(|m| matches!(m, Message::System { .. })),
            "None system prompt must emit no System message"
        );

        // An empty system prompt is treated like None.
        let empty_sys = build_call_options(
            &handle,
            &crate::tools::ToolRegistry::new(),
            Some(""),
            &PermissionScope::default_turn_scope(),
        )
        .await;
        assert!(
            !empty_sys
                .prompt
                .iter()
                .any(|m| matches!(m, Message::System { .. })),
            "empty system prompt must emit no System message"
        );
    }

    /// A non-empty registry advertises each tool as a `Tool::Function` and
    /// sets `tool_choice = Auto`, so the provider learns the callable surface.
    #[tokio::test]
    async fn build_call_options_advertises_registered_tools() {
        use llmsdk::language_model::{Tool, ToolChoice};

        use crate::tools::{EchoTool, ToolRegistry};

        let handle = seeded_handle("thread:native/advertise").await;
        handle.push_item(user_msg("item:u0", "go")).await;

        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));

        let opts =
            build_call_options(&handle, &reg, None, &PermissionScope::default_turn_scope()).await;

        let tools = opts.tools.expect("registered tools must be advertised");
        assert_eq!(tools.len(), 1, "exactly one tool advertised");
        match &tools[0] {
            Tool::Function(f) => assert_eq!(f.name, "echo", "echo tool advertised by name"),
            Tool::Provider(p) => panic!("expected Tool::Function, got provider tool {p:?}"),
        }
        assert!(
            matches!(opts.tool_choice, Some(ToolChoice::Auto)),
            "tool_choice must be Auto when tools are advertised"
        );
    }

    /// A tool the scope forbids is omitted from the advertised set (defense in
    /// depth + UX; the dispatch-side scope gate is the authoritative block).
    #[tokio::test]
    async fn build_call_options_omits_scope_disallowed_tools() {
        use zhive_proto::permission::ToolName;

        use crate::tools::{EchoTool, ToolRegistry};

        let handle = seeded_handle("thread:native/scope").await;
        handle.push_item(user_msg("item:u0", "go")).await;

        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));

        let mut scope = PermissionScope::default_turn_scope();
        scope.disallowed_tools.push(ToolName(Arc::from("echo")));

        let opts = build_call_options(&handle, &reg, None, &scope).await;
        assert!(
            opts.tools.is_none(),
            "the only registered tool is disallowed → nothing advertised"
        );
    }

    // ============================================================
    // truncate_tool_result unit tests
    // ============================================================

    use super::{
        MAX_TOOL_RESULT_BYTES, TOOL_RESULT_HEAD_BYTES, TOOL_RESULT_TAIL_BYTES, truncate_tool_result,
    };

    /// A result at or below the limit passes through unchanged.
    #[test]
    fn truncate_tool_result_short_passthrough() {
        let short = "hello, world!".to_owned();
        assert_eq!(truncate_tool_result(short.clone()), short);
    }

    /// An empty string also passes through unchanged.
    #[test]
    fn truncate_tool_result_empty_passthrough() {
        assert_eq!(truncate_tool_result(String::new()), "");
    }

    /// A result exactly at the limit passes through unchanged.
    #[test]
    fn truncate_tool_result_at_limit_passthrough() {
        let exact = "x".repeat(MAX_TOOL_RESULT_BYTES);
        let result = truncate_tool_result(exact.clone());
        assert_eq!(result, exact, "result at MAX should not be truncated");
    }

    /// A result one byte over the limit is truncated.
    #[test]
    fn truncate_tool_result_one_over_limit() {
        let over = "x".repeat(MAX_TOOL_RESULT_BYTES + 1);
        let result = truncate_tool_result(over.clone());
        // Must be shorter than the original.
        assert!(
            result.len() < over.len(),
            "over-limit result must be shorter after truncation"
        );
        assert!(
            result.contains("[... truncated"),
            "truncated result must contain elision marker"
        );
    }

    /// Over-long output: head and tail are preserved, marker contains counts.
    #[test]
    fn truncate_tool_result_preserves_head_and_tail_with_marker() {
        // Build a string big enough to trigger truncation with distinct head/tail.
        let head_content = "H".repeat(TOOL_RESULT_HEAD_BYTES);
        let middle_filler = "M".repeat(MAX_TOOL_RESULT_BYTES * 2);
        let tail_content = "T".repeat(TOOL_RESULT_TAIL_BYTES);
        let big = format!("{head_content}{middle_filler}{tail_content}");
        assert!(
            big.len() > MAX_TOOL_RESULT_BYTES,
            "test input must exceed MAX"
        );

        let result = truncate_tool_result(big.clone());
        assert!(
            result.len() <= MAX_TOOL_RESULT_BYTES + 256,
            "result must be near the cap (marker overhead < 256 bytes)"
        );

        // Head is preserved.
        assert!(
            result.starts_with(&head_content),
            "result must start with the head content"
        );
        // Tail is preserved.
        assert!(
            result.ends_with(&tail_content),
            "result must end with the tail content"
        );
        // Elision marker present with total size.
        assert!(
            result.contains(&format!("{} bytes", big.len())),
            "elision marker must contain original byte count"
        );
        assert!(
            result.contains("[... truncated"),
            "elision marker must use the expected format"
        );
    }

    /// UTF-8 multi-byte character boundaries are not broken by truncation.
    #[test]
    fn truncate_tool_result_utf8_boundary_safe() {
        // Each '中' is 3 UTF-8 bytes; build a string large enough to truncate.
        let cjk_char = '中';
        let cjk_bytes = cjk_char.len_utf8(); // 3
        // Make it long enough that truncation fires (> 16 KiB).
        let repeat_count = (MAX_TOOL_RESULT_BYTES / cjk_bytes) * 2;
        let long_cjk = cjk_char.to_string().repeat(repeat_count);
        assert!(long_cjk.len() > MAX_TOOL_RESULT_BYTES);

        // Must not panic, and must be valid UTF-8.
        let result = truncate_tool_result(long_cjk);
        assert!(
            std::str::from_utf8(result.as_bytes()).is_ok(),
            "truncated CJK string must be valid UTF-8"
        );
    }

    /// Over-long tool result flowing through `build_call_options` is reflected
    /// in the emitted `ToolResultOutput::Text` value.
    #[tokio::test]
    async fn build_call_options_truncates_oversized_tool_result() {
        let handle = seeded_handle("thread:native/trunc").await;

        // A tool result much larger than MAX_TOOL_RESULT_BYTES.
        let big_result = "R".repeat(MAX_TOOL_RESULT_BYTES * 3);

        handle
            .push_item(Item::ToolCall {
                id: ItemId(Arc::from("item:trunc-0")),
                name: "read_file".to_owned(),
                title: None,
                kind: ToolKind::Other,
                status: ToolCallStatus::Completed,
                content: vec![ItemToolCallContent::Content {
                    content: ItemContent::Text {
                        text: big_result.clone(),
                        annotations: None,
                    },
                }],
                locations: vec![],
                raw_input: Some(serde_json::json!({ "path": "/large" })),
                raw_output: None,
                provider_tool_call_id: Some("toolu_trunc".to_owned()),
            })
            .await;

        let opts = build_call_options(
            &handle,
            &crate::tools::ToolRegistry::new(),
            None,
            &PermissionScope::default_turn_scope(),
        )
        .await;

        // Find the Tool message and check its result text.
        let tool_msg = opts
            .prompt
            .iter()
            .find(|m| matches!(m, Message::Tool { .. }))
            .expect("a Tool message must be emitted");
        let tool_result_part = match tool_msg {
            Message::Tool { content, .. } => content.first().expect("tool result part"),
            _ => unreachable!(),
        };
        let ToolMessagePart::ToolResult(tr) = tool_result_part else {
            panic!("expected ToolResult part");
        };
        let result_text = match &tr.output {
            ToolResultOutput::Text { value, .. } => value,
            other => panic!("expected Text output, got {other:?}"),
        };
        assert!(
            result_text.len() <= MAX_TOOL_RESULT_BYTES + 256,
            "prompt result must be truncated to near MAX; got {} bytes",
            result_text.len()
        );
        assert!(
            result_text.contains("[... truncated"),
            "elision marker must appear in the prompt result"
        );
        // tool_call_id pairing must survive truncation.
        assert_eq!(
            tr.tool_call_id, "toolu_trunc",
            "tool_call_id must be preserved"
        );
    }
}

// Rust guideline compliant 2026-02-21

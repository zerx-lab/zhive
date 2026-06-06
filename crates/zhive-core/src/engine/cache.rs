//! Layers provider-specific prompt caching onto a freshly-built request.
//!
//! The engine assembles a provider [`CallOptions`] in [`super::prompt`] and
//! then calls [`apply_cache_control`] to add prompt caching, mirroring the way
//! [`super::reasoning::apply_reasoning`] layers thinking depth. Two provider
//! families are handled, following opencode's `cache-policy` design (the
//! `@opencode-ai/llm` package, itself an ai-sdk port like the bundled
//! `llmsdk`):
//!
//! * **Anthropic** uses explicit per-block `cache_control` breakpoints. We mark
//!   the static prefix (the last advertised tool, then the system prompt) and
//!   the conversation: a stable anchor on the latest user message plus a rolling
//!   breakpoint on the very last message. Anthropic caches the whole prefix up to
//!   each breakpoint and, on read, auto-matches the longest already-cached
//!   prefix, so the rolling tail makes every assistant/tool round-trip in an
//!   agentic loop land in cache (the previous turn's tail is read-hit, only the
//!   new suffix is written). This is the `auto` policy: at most four
//!   breakpoints — exactly Anthropic's limit.
//! * **OpenAI** caches prefixes automatically server-side; the only useful
//!   client signal is a stable `prompt_cache_key`, set here to the session id so
//!   repeated turns route to the same cache shard. That single key is read by
//!   both the Chat Completions and Responses paths in `llmsdk-openai`.
//!
//! Other providers (Google, xAI, Mistral, …) cache implicitly and ignore inline
//! markers, so [`apply_cache_control`] is a no-op for them — the same gate as
//! opencode's `RESPECTS_INLINE_HINTS`.
//!
//! Each Anthropic breakpoint is written as
//! `provider_options["anthropic"]["cacheControl"]` on the chosen
//! message/part/tool; `llmsdk-anthropic` reads it (and enforces the
//! 4-breakpoint cap) when lowering to the wire. Writing the key merges into any
//! existing provider-options bucket, so it never clobbers a `thinking` /
//! `effort` block [`super::reasoning`] may have written.
// Rust guideline compliant 2026-02-21

use llmsdk::language_model::{CallOptions, Message, Tool, UserPart};
use llmsdk::shared::ProviderOptions;

/// Applies provider-specific prompt caching to a freshly-built [`CallOptions`].
///
/// `provider` and `session_id` come from the active provider and the current
/// thread. Dispatches on `provider`: Anthropic gets explicit `cache_control`
/// breakpoints, OpenAI gets a stable `prompt_cache_key`, and every other
/// provider is left untouched (they cache implicitly server-side). Call this
/// after [`super::reasoning::apply_reasoning`] so the two passes share — rather
/// than overwrite — the per-request provider-options bucket.
///
/// # Examples
///
/// ```ignore
/// // (crate-internal) cache an Anthropic request's prefix + tail:
/// let mut opts = CallOptions::default();
/// // ... opts.prompt / opts.tools populated by build_call_options ...
/// apply_cache_control(&mut opts, "anthropic", "session-123");
/// ```
pub(in crate::engine) fn apply_cache_control(
    opts: &mut CallOptions,
    provider: &str,
    session_id: &str,
) {
    match provider {
        // Bedrock would also take inline markers (a `cachePoint` block); add a
        // branch here when a Bedrock backend is wired up.
        "anthropic" => apply_anthropic_breakpoints(opts),
        "openai" => apply_openai_cache_key(opts, session_id),
        // Google / xAI / Mistral / … cache implicitly; inline markers are
        // ignored (or rejected by strict gateways), so do nothing.
        _ => {}
    }
}

/// Marks the Anthropic `auto` cache breakpoints on `opts`.
///
/// Places up to four breakpoints — the last tool, the system prompt, the latest
/// user message (a stable anchor), and the very last message (a rolling tail) —
/// covering the static prefix plus the conversation. The rolling tail advances
/// every turn, so an agentic tool loop emitting many assistant/tool round-trips
/// keeps writing only the new suffix to cache while read-hitting everything
/// before it. A fresh turn (where the last message *is* the latest user message)
/// collapses the two conversation breakpoints into one and uses only three.
fn apply_anthropic_breakpoints(opts: &mut CallOptions) {
    let hint = ephemeral_breakpoint();

    // (1) Tool definitions: the last function tool caches the whole tool block
    //     (and, server-side, the system prefix preceding it). Cache stability
    //     depends on the tool order being identical across requests; the engine
    //     guarantees this by building `opts.tools` from `ToolRegistry::specs`,
    //     which sorts by name (see `super::prompt::build_tool_advertisements`).
    //     A caller assembling `opts.tools` in a different order would shift this
    //     breakpoint between turns and bust the tool-block cache.
    if let Some(tools) = opts.tools.as_mut()
        && let Some(Tool::Function(tool)) = tools
            .iter_mut()
            .rev()
            .find(|t| matches!(t, Tool::Function(_)))
    {
        set_anthropic_cache_control(&mut tool.provider_options, &hint);
    }

    // (2) System prompt: cache the large, stable instruction prefix. Marks the
    //     LAST system message (opencode's `markLastSystem`); today the engine
    //     emits at most one, but `rev()` keeps the breakpoint on the final, most
    //     stable boundary if that ever changes.
    if let Some(Message::System {
        provider_options, ..
    }) = opts
        .prompt
        .iter_mut()
        .rev()
        .find(|m| matches!(m, Message::System { .. }))
    {
        set_anthropic_cache_control(provider_options, &hint);
    }

    // (3) Stable anchor: the latest user message's trailing part. Pins the
    //     user-turn boundary in cache (opencode's `auto` policy, where
    //     `messages = latest-user-message`), so repeated intra-turn tool
    //     iterations all read-hit the prefix up to the user message.
    let latest_user = opts
        .prompt
        .iter()
        .rposition(|m| matches!(m, Message::User { .. }));
    if let Some(index) = latest_user
        && let Message::User { content, .. } = &mut opts.prompt[index]
    {
        mark_last_user_part(content, &hint);
    }

    // (4) Rolling tail: the very last message, whatever its role. Anthropic
    //     writes the cache up to this breakpoint and, on read, auto-matches the
    //     longest cached prefix, so marking the trailing message every turn
    //     incrementally caches each assistant/tool round-trip — the previous
    //     turn's tail is read-hit and only the new suffix is written. This is the
    //     decisive win for an agentic coding loop with large tool outputs; it
    //     extends opencode's `auto` with a `{tail:1}`-style breakpoint while
    //     staying within Anthropic's 4-breakpoint budget (tool + system + user +
    //     tail). Skipped when the last message *is* the latest user message
    //     (already marked in step 3), so a fresh turn still uses only three.
    let last = opts.prompt.len().checked_sub(1);
    if let Some(index) = last
        && Some(index) != latest_user
    {
        mark_message_tail(&mut opts.prompt[index], &hint);
    }
}

/// Marks the cache breakpoint on a user message's trailing cacheable part.
///
/// Prefers the last text part and falls back to the last part of any kind,
/// mirroring opencode's `markMessageAt` (which marks the final content part when
/// there is no text part, e.g. an image-only turn). User parts carry their own
/// `provider_options`; the Anthropic converter does not read a user message's
/// message-level options, so the breakpoint must sit on a part.
fn mark_last_user_part(content: &mut [UserPart], hint: &serde_json::Value) {
    let index = content
        .iter()
        .rposition(|p| matches!(p, UserPart::Text(_)))
        .or_else(|| content.len().checked_sub(1));
    let Some(index) = index else {
        return;
    };
    let slot = match &mut content[index] {
        UserPart::Text(part) => &mut part.provider_options,
        UserPart::File(part) => &mut part.provider_options,
    };
    set_anthropic_cache_control(slot, hint);
}

/// Marks the cache breakpoint on a message's trailing cacheable part.
///
/// User messages take a per-part breakpoint (see [`mark_last_user_part`]).
/// Assistant and tool messages take a message-level breakpoint, which
/// `llmsdk-anthropic` applies to the message's last cacheable part. A `System`
/// message is never the conversation tail but is handled for completeness.
///
/// The load-bearing invariant is that the conversation tail is always a
/// cacheable part (text / tool-call / tool-result): `super::prompt` never
/// replays reasoning parts into the history it rebuilds, so the trailing
/// assistant/tool message has no thinking block at its end. (If reasoning replay
/// were ever added, `llmsdk-anthropic` would consume a breakpoint slot on the
/// trailing thinking block without emitting one on the wire — so the breakpoint
/// placement here would need to walk back to the last cacheable part.)
fn mark_message_tail(message: &mut Message, hint: &serde_json::Value) {
    match message {
        Message::User { content, .. } => mark_last_user_part(content, hint),
        Message::Assistant {
            provider_options, ..
        }
        | Message::Tool {
            provider_options, ..
        }
        | Message::System {
            provider_options, ..
        } => set_anthropic_cache_control(provider_options, hint),
    }
}

/// Returns the default ephemeral cache breakpoint (`{"type":"ephemeral"}`).
///
/// The `ttl` field is omitted, which Anthropic treats as the default 5-minute
/// ephemeral TTL. This mirrors opencode's `auto` policy: a 5-minute cache write
/// costs 1.25x base input tokens and a read costs 0.1x, so a single reuse inside
/// the window already pays for the write. A longer 1-hour TTL would require an
/// explicit `"ttl":"1h"` and is left for a future per-request opt-in.
fn ephemeral_breakpoint() -> serde_json::Value {
    serde_json::json!({ "type": "ephemeral" })
}

/// Sets a stable `prompt_cache_key` (the session id) for OpenAI.
///
/// OpenAI caches request prefixes automatically once they exceed ~1024 tokens;
/// the key only routes repeated calls to the same cache shard for a higher hit
/// rate. It is written to the request-level `provider_options["openai"]`, which
/// both the Chat Completions and Responses paths in `llmsdk-openai` read. An
/// empty session id is a no-op so a useless key is never sent.
fn apply_openai_cache_key(opts: &mut CallOptions, session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    let mut options = opts.provider_options.take().unwrap_or_default();
    let mut openai = options.remove("openai").unwrap_or_default();
    openai.insert(
        "promptCacheKey".to_owned(),
        serde_json::Value::String(session_id.to_owned()),
    );
    options.insert("openai".to_owned(), openai);
    opts.provider_options = Some(options);
}

/// Merges a `cache_control` breakpoint into a part's `provider_options`.
///
/// Only the `anthropic.cacheControl` key is (re)set; any sibling keys (e.g. a
/// `thinking` / `effort` block) are preserved so caching composes with
/// [`super::reasoning`].
fn set_anthropic_cache_control(slot: &mut Option<ProviderOptions>, hint: &serde_json::Value) {
    let mut options = slot.take().unwrap_or_default();
    let mut anthropic = options.remove("anthropic").unwrap_or_default();
    anthropic.insert("cacheControl".to_owned(), hint.clone());
    options.insert("anthropic".to_owned(), anthropic);
    *slot = Some(options);
}

#[cfg(test)]
mod tests {
    use super::*;
    use llmsdk::language_model::{
        AssistantPart, FunctionTool, TextPart, ToolCallPart, ToolMessagePart, ToolResultOutput,
        ToolResultPart,
    };

    /// Builds a `Message::System` with no provider options.
    fn system(text: &str) -> Message {
        Message::System {
            content: text.to_owned(),
            provider_options: None,
        }
    }

    /// Builds a single-text `Message::User` with no provider options.
    fn user(text: &str) -> Message {
        Message::User {
            content: vec![UserPart::Text(TextPart {
                text: text.to_owned(),
                provider_options: None,
            })],
            provider_options: None,
        }
    }

    /// Builds a `Tool::Function` named `name` with a trivial object schema.
    fn function_tool(name: &str) -> Tool {
        Tool::Function(FunctionTool {
            name: name.to_owned(),
            description: None,
            input_schema: serde_json::from_value(serde_json::json!({ "type": "object" }))
                .expect("trivial object schema is valid"),
            input_examples: None,
            strict: None,
            provider_options: None,
        })
    }

    /// Reads `provider_options["anthropic"]["cacheControl"]`, if present.
    fn cache_control(slot: Option<&ProviderOptions>) -> Option<&serde_json::Value> {
        slot?.get("anthropic")?.get("cacheControl")
    }

    /// Reads a `Message::User`'s first text part's provider options.
    fn user_text_options(message: &Message) -> Option<&ProviderOptions> {
        let Message::User { content, .. } = message else {
            panic!("expected a user message");
        };
        let UserPart::Text(part) = &content[0] else {
            panic!("expected a text part");
        };
        part.provider_options.as_ref()
    }

    /// Reads a `Message::System`'s provider options.
    fn system_options(message: &Message) -> Option<&ProviderOptions> {
        let Message::System {
            provider_options, ..
        } = message
        else {
            panic!("expected a system message");
        };
        provider_options.as_ref()
    }

    /// Reads a `Tool::Function`'s provider options.
    fn tool_options(tool: &Tool) -> Option<&ProviderOptions> {
        let Tool::Function(function) = tool else {
            panic!("expected a function tool");
        };
        function.provider_options.as_ref()
    }

    /// Builds an assistant message that only requests a tool call.
    fn assistant_tool_call(id: &str, name: &str) -> Message {
        Message::Assistant {
            content: vec![AssistantPart::ToolCall(ToolCallPart {
                tool_call_id: id.to_owned(),
                tool_name: name.to_owned(),
                input: serde_json::json!({}),
                provider_executed: None,
                dynamic: None,
                provider_options: None,
            })],
            provider_options: None,
        }
    }

    /// Builds a tool message carrying a single text result.
    fn tool_result(id: &str, name: &str, text: &str) -> Message {
        Message::Tool {
            content: vec![ToolMessagePart::ToolResult(ToolResultPart {
                tool_call_id: id.to_owned(),
                tool_name: name.to_owned(),
                output: ToolResultOutput::Text {
                    value: text.to_owned(),
                    provider_options: None,
                },
                provider_options: None,
            })],
            provider_options: None,
        }
    }

    /// Reads a message's message-level provider options.
    fn message_options(message: &Message) -> Option<&ProviderOptions> {
        match message {
            Message::Assistant {
                provider_options, ..
            }
            | Message::Tool {
                provider_options, ..
            }
            | Message::System {
                provider_options, ..
            } => provider_options.as_ref(),
            Message::User { .. } => panic!("expected a non-user message"),
        }
    }

    /// Anthropic gets breakpoints on the last tool, the system prompt, and the
    /// latest user message — and nowhere else.
    #[test]
    fn anthropic_marks_prefix_and_latest_user() {
        let mut opts = CallOptions {
            prompt: vec![system("rules"), user("first"), user("second")],
            tools: Some(vec![function_tool("a"), function_tool("b")]),
            ..CallOptions::default()
        };

        apply_cache_control(&mut opts, "anthropic", "session-1");

        let expected = serde_json::json!({ "type": "ephemeral" });
        // System prompt cached.
        assert_eq!(
            cache_control(system_options(&opts.prompt[0])),
            Some(&expected)
        );
        // Earlier user message NOT cached; latest one IS.
        assert!(cache_control(user_text_options(&opts.prompt[1])).is_none());
        assert_eq!(
            cache_control(user_text_options(&opts.prompt[2])),
            Some(&expected)
        );
        // Only the last tool is cached.
        let tools = opts.tools.as_ref().expect("tools present");
        assert!(cache_control(tool_options(&tools[0])).is_none());
        assert_eq!(cache_control(tool_options(&tools[1])), Some(&expected));
    }

    /// The `auto` policy never exceeds Anthropic's 4-breakpoint limit.
    #[test]
    fn anthropic_stays_within_breakpoint_budget() {
        let mut opts = CallOptions {
            prompt: vec![system("rules"), user("a"), user("b"), user("c")],
            tools: Some(vec![function_tool("t1"), function_tool("t2")]),
            ..CallOptions::default()
        };

        apply_cache_control(&mut opts, "anthropic", "session-1");

        let mut count = 0;
        if cache_control(system_options(&opts.prompt[0])).is_some() {
            count += 1;
        }
        for message in &opts.prompt {
            if let Message::User { content, .. } = message
                && let UserPart::Text(part) = &content[0]
                && cache_control(part.provider_options.as_ref()).is_some()
            {
                count += 1;
            }
        }
        for tool in opts.tools.as_ref().expect("tools present") {
            if cache_control(tool_options(tool)).is_some() {
                count += 1;
            }
        }
        assert!(count <= 4, "auto policy placed {count} breakpoints (max 4)");
    }

    /// In an agentic tool loop (tail is a tool result, not the user message),
    /// the rolling breakpoint lands on the trailing tool message while the latest
    /// user message still keeps its stable anchor — and the budget holds at four.
    #[test]
    fn anthropic_rolling_tail_marks_trailing_tool() {
        let mut opts = CallOptions {
            prompt: vec![
                system("rules"),
                user("solve it"),
                assistant_tool_call("c1", "grep"),
                tool_result("c1", "grep", "match at line 42"),
            ],
            tools: Some(vec![function_tool("grep")]),
            ..CallOptions::default()
        };

        apply_cache_control(&mut opts, "anthropic", "session-1");

        let expected = serde_json::json!({ "type": "ephemeral" });
        // Static prefix: system + last tool.
        assert_eq!(
            cache_control(system_options(&opts.prompt[0])),
            Some(&expected)
        );
        let tools = opts.tools.as_ref().expect("tools present");
        assert_eq!(cache_control(tool_options(&tools[0])), Some(&expected));
        // Stable anchor: the latest user message.
        assert_eq!(
            cache_control(user_text_options(&opts.prompt[1])),
            Some(&expected)
        );
        // Rolling tail: the trailing tool message (message level), not the
        // assistant in between.
        assert!(cache_control(message_options(&opts.prompt[2])).is_none());
        assert_eq!(
            cache_control(message_options(&opts.prompt[3])),
            Some(&expected)
        );

        // This is the maximal case (system + tool + user + tail); it must use
        // exactly four breakpoints, never more.
        let mut count = 0;
        if cache_control(system_options(&opts.prompt[0])).is_some() {
            count += 1;
        }
        if cache_control(user_text_options(&opts.prompt[1])).is_some() {
            count += 1;
        }
        if cache_control(message_options(&opts.prompt[3])).is_some() {
            count += 1;
        }
        if cache_control(tool_options(&tools[0])).is_some() {
            count += 1;
        }
        assert_eq!(count, 4, "tool-loop tail must use exactly 4 breakpoints");
    }

    /// A pre-existing `anthropic` bucket (e.g. a thinking block) survives the
    /// `cache_control` merge.
    #[test]
    fn anthropic_merge_preserves_sibling_keys() {
        let mut anthropic = serde_json::Map::new();
        anthropic.insert(
            "thinking".to_owned(),
            serde_json::json!({ "type": "adaptive" }),
        );
        let mut po = ProviderOptions::new();
        po.insert("anthropic".to_owned(), anthropic);

        let mut opts = CallOptions {
            prompt: vec![Message::System {
                content: "rules".to_owned(),
                provider_options: Some(po),
            }],
            ..CallOptions::default()
        };

        apply_cache_control(&mut opts, "anthropic", "session-1");

        let bucket = system_options(&opts.prompt[0])
            .expect("provider options present")
            .get("anthropic")
            .expect("anthropic bucket present");
        assert_eq!(bucket["thinking"]["type"], "adaptive");
        assert_eq!(bucket["cacheControl"]["type"], "ephemeral");
    }

    /// OpenAI gets a stable `promptCacheKey` and no inline `cache_control`.
    #[test]
    fn openai_sets_prompt_cache_key_only() {
        let mut opts = CallOptions {
            prompt: vec![system("rules"), user("hi")],
            tools: Some(vec![function_tool("a")]),
            ..CallOptions::default()
        };

        apply_cache_control(&mut opts, "openai", "session-9");

        let openai = opts
            .provider_options
            .as_ref()
            .expect("provider options present")
            .get("openai")
            .expect("openai bucket present");
        assert_eq!(openai["promptCacheKey"], "session-9");
        // No inline anthropic breakpoints for OpenAI.
        assert!(cache_control(system_options(&opts.prompt[0])).is_none());
        assert!(cache_control(user_text_options(&opts.prompt[1])).is_none());
    }

    /// The OpenAI cache key merges with (does not clobber) an existing bucket.
    #[test]
    fn openai_merges_with_existing_bucket() {
        let mut openai = serde_json::Map::new();
        openai.insert(
            "reasoningEffort".to_owned(),
            serde_json::Value::String("high".to_owned()),
        );
        let mut po = ProviderOptions::new();
        po.insert("openai".to_owned(), openai);

        let mut opts = CallOptions {
            provider_options: Some(po),
            ..CallOptions::default()
        };

        apply_cache_control(&mut opts, "openai", "session-9");

        let openai = opts.provider_options.as_ref().unwrap()["openai"].clone();
        assert_eq!(openai["reasoningEffort"], "high");
        assert_eq!(openai["promptCacheKey"], "session-9");
    }

    /// An empty session id leaves OpenAI requests untouched.
    #[test]
    fn openai_empty_session_is_noop() {
        let mut opts = CallOptions {
            prompt: vec![user("hi")],
            ..CallOptions::default()
        };

        apply_cache_control(&mut opts, "openai", "");

        assert!(opts.provider_options.is_none());
    }

    /// Providers with implicit caching get no breakpoints and no key.
    #[test]
    fn other_provider_is_noop() {
        let mut opts = CallOptions {
            prompt: vec![system("rules"), user("hi")],
            tools: Some(vec![function_tool("a")]),
            ..CallOptions::default()
        };

        apply_cache_control(&mut opts, "xai", "session-1");

        assert!(opts.provider_options.is_none());
        assert!(cache_control(system_options(&opts.prompt[0])).is_none());
        let tools = opts.tools.as_ref().expect("tools present");
        assert!(cache_control(tool_options(&tools[0])).is_none());
    }
}

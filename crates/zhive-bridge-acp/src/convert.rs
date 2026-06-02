//! Pure conversions between zhive domain types and ACP schema types.
//!
//! This module is the single source of truth for the bridge's data mapping. It
//! holds **no** async logic and **no** connection state, so every function here
//! is unit-testable in isolation and carries a doctest.
//!
//! The zhive [`zhive_proto::domain`] leaf types were designed "1:1 isomorphic
//! with ACP `ContentBlock` / `ToolCallContent`", so the mappings are mostly
//! structural. Two directions matter:
//!
//! * **inbound** — ACP [`ContentBlock`] from a `session/prompt` becomes a zhive
//!   [`ItemContent`] (wrapped into a single [`Item::UserMessage`]).
//! * **outbound** — zhive [`EngineEvent`] / [`Item`] become ACP
//!   [`SessionUpdate`] values streamed over `session/update`.
//!
//! Unknown / unmappable variants never panic: inbound unknown content collapses
//! to an empty text block, and outbound items with no ACP analogue yield `None`.

use agent_client_protocol::schema::{
    Content, ContentBlock, ContentChunk, Diff, ImageContent, Plan, PlanEntry, PlanEntryPriority,
    PlanEntryStatus, ResourceLink, SessionUpdate, TextContent, ToolCall, ToolCallContent,
    ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};

use zhive_proto::domain::{
    Item, ItemContent, ItemToolCallContent, PlanStep, PlanStepStatus,
    ToolCallStatus as ZToolStatus, ToolKind as ZToolKind,
};

/// Converts an ACP [`ContentBlock`] into a zhive [`ItemContent`].
///
/// Used on the inbound `session/prompt` path. `Image` carries data + mime type;
/// `Audio` drops the (rare) `uri`; `ResourceLink` keeps name/description/mime;
/// `Resource` is preserved as raw JSON (zhive defers strong typing).
///
/// # Examples
///
/// ```
/// use agent_client_protocol::schema::{ContentBlock, TextContent};
/// use zhive_bridge_acp::convert::content_block_to_item_content;
/// use zhive_proto::domain::ItemContent;
///
/// let block = ContentBlock::Text(TextContent::new("hi"));
/// let item = content_block_to_item_content(block);
/// assert!(matches!(item, ItemContent::Text { text, .. } if text == "hi"));
/// ```
#[must_use]
pub fn content_block_to_item_content(block: ContentBlock) -> ItemContent {
    match block {
        ContentBlock::Text(t) => ItemContent::Text {
            text: t.text,
            annotations: None,
        },
        ContentBlock::Image(img) => ItemContent::Image {
            data: img.data,
            mime_type: img.mime_type,
            uri: img.uri,
        },
        ContentBlock::Audio(a) => ItemContent::Audio {
            data: a.data,
            mime_type: a.mime_type,
        },
        ContentBlock::ResourceLink(link) => ItemContent::ResourceLink {
            uri: link.uri,
            name: Some(link.name),
            description: link.description,
            mime_type: link.mime_type,
        },
        ContentBlock::Resource(res) => ItemContent::Resource {
            // The ACP embedded resource is preserved verbatim as JSON; a
            // serialization failure is impossible for a value the SDK just
            // deserialized, so fall back to JSON null defensively.
            resource: serde_json::to_value(&res.resource).unwrap_or(serde_json::Value::Null),
        },
        // `ContentBlock` is `#[non_exhaustive]`; an unknown future variant maps
        // to empty text rather than panicking (domain.rs downgrade contract).
        _ => ItemContent::Text {
            text: String::new(),
            annotations: None,
        },
    }
}

/// Converts a zhive [`ItemContent`] back into an ACP [`ContentBlock`].
///
/// Inverse of [`content_block_to_item_content`] for the common variants. Used
/// when streaming agent message / tool content back to the client.
///
/// # Examples
///
/// ```
/// use agent_client_protocol::schema::ContentBlock;
/// use zhive_bridge_acp::convert::item_content_to_block;
/// use zhive_proto::domain::ItemContent;
///
/// let content = ItemContent::Text { text: "yo".into(), annotations: None };
/// let block = item_content_to_block(content);
/// assert!(matches!(block, ContentBlock::Text(t) if t.text == "yo"));
/// ```
#[must_use]
pub fn item_content_to_block(content: ItemContent) -> ContentBlock {
    match content {
        ItemContent::Text { text, .. } => ContentBlock::Text(TextContent::new(text)),
        ItemContent::Image {
            data,
            mime_type,
            uri,
        } => {
            let mut img = ImageContent::new(data, mime_type);
            img.uri = uri;
            ContentBlock::Image(img)
        }
        ItemContent::Audio { data, mime_type } => ContentBlock::Audio(
            agent_client_protocol::schema::AudioContent::new(data, mime_type),
        ),
        ItemContent::ResourceLink {
            uri,
            name,
            description,
            mime_type,
        } => {
            let mut link = ResourceLink::new(name.unwrap_or_default(), uri);
            link.description = description;
            link.mime_type = mime_type;
            ContentBlock::ResourceLink(link)
        }
        ItemContent::Resource { resource } => {
            // Best-effort: a typed embedded resource is reconstructed when the
            // JSON matches, otherwise the payload is surfaced as text so no
            // content is silently dropped.
            match serde_json::from_value(resource.clone()) {
                Ok(embedded) => ContentBlock::Resource(
                    agent_client_protocol::schema::EmbeddedResource::new(embedded),
                ),
                Err(_) => ContentBlock::Text(TextContent::new(resource.to_string())),
            }
        }
        // `ItemContent` is `#[non_exhaustive]`: unknown variant -> empty text.
        _ => ContentBlock::Text(TextContent::new(String::new())),
    }
}

/// Wraps ACP prompt blocks into a single zhive [`Item::UserMessage`].
///
/// The engine consumes a `Vec<Item>` as user input; an ACP prompt is a single
/// user message composed of multiple content blocks, so all blocks fold into
/// one item.
///
/// # Examples
///
/// ```
/// use agent_client_protocol::schema::{ContentBlock, TextContent};
/// use zhive_bridge_acp::convert::prompt_blocks_to_user_item;
/// use zhive_proto::domain::Item;
///
/// let blocks = vec![ContentBlock::Text(TextContent::new("a"))];
/// let item = prompt_blocks_to_user_item(blocks, "item:turn/0");
/// assert!(matches!(item, Item::UserMessage { content, .. } if content.len() == 1));
/// ```
#[must_use]
pub fn prompt_blocks_to_user_item(blocks: Vec<ContentBlock>, item_id: impl Into<String>) -> Item {
    Item::UserMessage {
        id: zhive_proto::domain::ItemId(std::sync::Arc::from(item_id.into())),
        content: blocks
            .into_iter()
            .map(content_block_to_item_content)
            .collect(),
    }
}

/// Maps a zhive [`ZToolKind`] to the ACP [`ToolKind`].
///
/// # Examples
///
/// ```
/// use agent_client_protocol::schema::ToolKind;
/// use zhive_bridge_acp::convert::tool_kind_to_acp;
/// use zhive_proto::domain::ToolKind as ZToolKind;
///
/// assert_eq!(tool_kind_to_acp(ZToolKind::Read), ToolKind::Read);
/// assert_eq!(tool_kind_to_acp(ZToolKind::Other), ToolKind::Other);
/// ```
#[must_use]
pub fn tool_kind_to_acp(kind: ZToolKind) -> ToolKind {
    match kind {
        ZToolKind::Read => ToolKind::Read,
        ZToolKind::Edit => ToolKind::Edit,
        ZToolKind::Delete => ToolKind::Delete,
        ZToolKind::Move => ToolKind::Move,
        ZToolKind::Search => ToolKind::Search,
        ZToolKind::Execute => ToolKind::Execute,
        ZToolKind::Think => ToolKind::Think,
        ZToolKind::Fetch => ToolKind::Fetch,
        ZToolKind::SwitchMode => ToolKind::SwitchMode,
        _ => ToolKind::Other,
    }
}

/// Maps a zhive [`ZToolStatus`] to the ACP [`ToolCallStatus`].
///
/// # Examples
///
/// ```
/// use agent_client_protocol::schema::ToolCallStatus;
/// use zhive_bridge_acp::convert::tool_status_to_acp;
/// use zhive_proto::domain::ToolCallStatus as ZToolStatus;
///
/// assert_eq!(tool_status_to_acp(ZToolStatus::Completed), ToolCallStatus::Completed);
/// ```
#[must_use]
pub fn tool_status_to_acp(status: ZToolStatus) -> ToolCallStatus {
    match status {
        ZToolStatus::Pending => ToolCallStatus::Pending,
        ZToolStatus::InProgress => ToolCallStatus::InProgress,
        ZToolStatus::Completed => ToolCallStatus::Completed,
        _ => ToolCallStatus::Failed,
    }
}

/// Maps a zhive [`ItemToolCallContent`] to an ACP [`ToolCallContent`].
///
/// `Diff` carries straight through (the highest-value mapping for editor diff
/// UIs); generic `Content` wraps an inner block; `Terminal` references a handle.
///
/// # Examples
///
/// ```
/// use agent_client_protocol::schema::ToolCallContent;
/// use zhive_bridge_acp::convert::tool_call_content_to_acp;
/// use zhive_proto::domain::ItemToolCallContent;
///
/// let diff = ItemToolCallContent::Diff {
///     path: "/tmp/a.rs".into(),
///     old_text: None,
///     new_text: "x".into(),
/// };
/// assert!(matches!(tool_call_content_to_acp(diff), ToolCallContent::Diff(_)));
/// ```
#[must_use]
pub fn tool_call_content_to_acp(content: ItemToolCallContent) -> ToolCallContent {
    match content {
        ItemToolCallContent::Content { content } => {
            ToolCallContent::Content(Content::new(item_content_to_block(content)))
        }
        ItemToolCallContent::Diff {
            path,
            old_text,
            new_text,
        } => {
            let mut diff = Diff::new(path, new_text);
            diff.old_text = old_text;
            ToolCallContent::Diff(diff)
        }
        ItemToolCallContent::Terminal { terminal_id } => {
            ToolCallContent::Terminal(agent_client_protocol::schema::Terminal::new(terminal_id))
        }
        // Unknown future variant -> empty text content rather than panic.
        _ => ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(
            String::new(),
        )))),
    }
}

/// Maps zhive [`PlanStep`]s to an ACP [`Plan`] (full-replace semantics).
///
/// All ACP plan entries default to [`PlanEntryPriority::Medium`]; zhive has no
/// per-step priority concept.
///
/// # Examples
///
/// ```
/// use zhive_bridge_acp::convert::plan_steps_to_acp;
/// use zhive_proto::domain::{PlanStep, PlanStepStatus};
///
/// let steps = vec![PlanStep { step: "do".into(), status: PlanStepStatus::Pending }];
/// let plan = plan_steps_to_acp(steps);
/// assert_eq!(plan.entries.len(), 1);
/// ```
#[must_use]
pub fn plan_steps_to_acp(steps: Vec<PlanStep>) -> Plan {
    let entries = steps
        .into_iter()
        .map(|s| {
            PlanEntry::new(
                s.step,
                PlanEntryPriority::Medium,
                plan_step_status_to_acp(s.status),
            )
        })
        .collect();
    Plan::new(entries)
}

fn plan_step_status_to_acp(status: PlanStepStatus) -> PlanEntryStatus {
    match status {
        PlanStepStatus::Pending => PlanEntryStatus::Pending,
        PlanStepStatus::InProgress => PlanEntryStatus::InProgress,
        _ => PlanEntryStatus::Completed,
    }
}

/// Returns the stable ACP [`ToolCallId`] for a zhive `ToolCall` item.
///
/// Prefers the provider-assigned id (preserved end-to-end through the engine)
/// and falls back to the per-turn item id so correlation across
/// `ToolCall` → `ToolCallUpdate` → permission stays 1:1.
fn tool_call_id_for(item: &Item) -> ToolCallId {
    match item {
        Item::ToolCall {
            id,
            provider_tool_call_id,
            ..
        } => {
            let raw = provider_tool_call_id
                .as_deref()
                .unwrap_or_else(|| id.0.as_ref());
            ToolCallId::new(std::sync::Arc::<str>::from(raw))
        }
        other => ToolCallId::new(std::sync::Arc::<str>::from(other.id().0.as_ref())),
    }
}

/// Builds an ACP [`ToolCall`] (first sighting) from a zhive `ToolCall` item.
///
/// Returns `None` for non-`ToolCall` items. Maps name → title, kind, status,
/// content (incl. diffs) and raw input/output.
///
/// # Examples
///
/// ```
/// use zhive_bridge_acp::convert::tool_call_to_acp;
/// use zhive_proto::domain::{Item, ItemId, ToolKind, ToolCallStatus};
///
/// let item = Item::ToolCall {
///     id: ItemId("item:t/0".into()),
///     name: "read_file".into(),
///     kind: ToolKind::Read,
///     status: ToolCallStatus::InProgress,
///     content: vec![],
///     locations: vec![],
///     raw_input: None,
///     raw_output: None,
///     provider_tool_call_id: Some("toolu_1".into()),
/// };
/// let acp = tool_call_to_acp(&item).unwrap();
/// assert_eq!(acp.title, "read_file");
/// assert_eq!(acp.tool_call_id.0.as_ref(), "toolu_1");
/// ```
#[must_use]
pub fn tool_call_to_acp(item: &Item) -> Option<ToolCall> {
    let Item::ToolCall {
        name,
        kind,
        status,
        content,
        raw_input,
        raw_output,
        ..
    } = item
    else {
        return None;
    };
    let tool_call_id = tool_call_id_for(item);
    let acp_content: Vec<ToolCallContent> = content
        .iter()
        .cloned()
        .map(tool_call_content_to_acp)
        .collect();
    let mut call = ToolCall::new(tool_call_id, name.clone())
        .kind(tool_kind_to_acp(*kind))
        .status(tool_status_to_acp(*status))
        .content(acp_content);
    call.raw_input.clone_from(raw_input);
    call.raw_output.clone_from(raw_output);
    Some(call)
}

/// Builds an ACP [`ToolCallUpdate`] from a zhive `ToolCall` item.
///
/// Returns `None` for non-`ToolCall` items. Used for status / content changes
/// after the first `ToolCall` notification.
///
/// # Examples
///
/// ```
/// use zhive_bridge_acp::convert::tool_call_update_to_acp;
/// use zhive_proto::domain::{Item, ItemId, ToolKind, ToolCallStatus};
///
/// let item = Item::ToolCall {
///     id: ItemId("item:t/0".into()),
///     name: "read_file".into(),
///     kind: ToolKind::Read,
///     status: ToolCallStatus::Completed,
///     content: vec![],
///     locations: vec![],
///     raw_input: None,
///     raw_output: None,
///     provider_tool_call_id: Some("toolu_1".into()),
/// };
/// let upd = tool_call_update_to_acp(&item).unwrap();
/// assert_eq!(upd.tool_call_id.0.as_ref(), "toolu_1");
/// ```
#[must_use]
pub fn tool_call_update_to_acp(item: &Item) -> Option<ToolCallUpdate> {
    let Item::ToolCall {
        name,
        kind,
        status,
        content,
        raw_input,
        raw_output,
        ..
    } = item
    else {
        return None;
    };
    let tool_call_id = tool_call_id_for(item);
    let acp_content: Vec<ToolCallContent> = content
        .iter()
        .cloned()
        .map(tool_call_content_to_acp)
        .collect();
    let fields = ToolCallUpdateFields::new()
        .title(name.clone())
        .kind(tool_kind_to_acp(*kind))
        .status(tool_status_to_acp(*status))
        .content(acp_content)
        .raw_input(raw_input.clone())
        .raw_output(raw_output.clone());
    Some(ToolCallUpdate::new(tool_call_id, fields))
}

/// Maps a zhive [`Item`] (from `ItemAppended`) to a single ACP [`SessionUpdate`].
///
/// Returns `None` for items with no ACP `session/update` analogue (e.g.
/// `ContextCompaction`, `Terminal`, `ModeChange`, the `UserMessage` echo) and,
/// crucially, for [`Item::AgentMessage`].
///
/// `AgentMessage` is **deliberately suppressed** here. The engine emits the
/// agent's text twice: once live as a stream of [`EngineEvent::ItemDelta`]
/// fragments and again as a finalising `ItemAppended` carrying the complete
/// `Item::AgentMessage` (see `engine/event.rs`). The bridge streams the deltas
/// (the point of streaming), so re-emitting the finalised whole block would make
/// the client render the agent's text a second time. `AgentMessageChunk` is
/// therefore produced only on the delta path ([`delta_to_session_update`]).
///
/// Thought/reasoning text, by contrast, has *no* delta path — it finalises as a
/// single item — so `AgentThought` / `Reasoning` are forwarded here unchanged.
///
/// [`EngineEvent::ItemDelta`]: zhive_core::engine::event::EngineEvent::ItemDelta
///
/// * `AgentMessage` → `None` (already streamed live as deltas)
/// * `AgentThought` / `Reasoning` → `AgentThoughtChunk`
/// * `ToolCall` → `ToolCall`
/// * `Plan` → `Plan`
///
/// # Examples
///
/// ```
/// use zhive_bridge_acp::convert::item_to_session_update;
/// use zhive_proto::domain::{Item, ItemId};
///
/// // The full agent block is suppressed; only the live deltas reach the client.
/// let item = Item::AgentMessage { id: ItemId("item:t/0".into()), text: "done".into() };
/// assert!(item_to_session_update(&item).is_none());
/// ```
#[must_use]
pub fn item_to_session_update(item: &Item) -> Option<SessionUpdate> {
    match item {
        // Suppressed: the agent text already streamed live via ItemDelta /
        // `delta_to_session_update`. Re-emitting the finalised whole block would
        // duplicate the message in the client. See the function docs above. This
        // arm is kept explicit (rather than folded into the wildcard) so the
        // deliberate dedup is documented at the point of suppression.
        #[expect(
            clippy::match_same_arms,
            reason = "explicit arm documents the deliberate AgentMessage dedup"
        )]
        Item::AgentMessage { .. } => None,
        Item::AgentThought { text, .. } => Some(SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(text.clone()))),
        )),
        Item::Reasoning { summary, .. } => Some(SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(summary.join("\n")))),
        )),
        Item::ToolCall { .. } => tool_call_to_acp(item).map(SessionUpdate::ToolCall),
        Item::Plan { steps, .. } => Some(SessionUpdate::Plan(plan_steps_to_acp(steps.clone()))),
        Item::SystemNotice { message, .. } => Some(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(message.clone()))),
        )),
        // UserMessage echo, Terminal, ModeChange, ContextCompaction,
        // CommandExecution, FileEdit, Diff, AvailableCommands: no direct ACP
        // session/update analogue in v1 -> skip.
        _ => None,
    }
}

/// Maps a streamed text delta to an `AgentMessageChunk` `session/update`.
///
/// # Examples
///
/// ```
/// use agent_client_protocol::schema::SessionUpdate;
/// use zhive_bridge_acp::convert::delta_to_session_update;
///
/// assert!(matches!(delta_to_session_update("hi"), SessionUpdate::AgentMessageChunk(_)));
/// ```
#[must_use]
pub fn delta_to_session_update(delta: impl Into<String>) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        delta.into(),
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips() {
        let original = ItemContent::Text {
            text: "hello".into(),
            annotations: None,
        };
        let block = item_content_to_block(original);
        let back = content_block_to_item_content(block);
        assert!(matches!(back, ItemContent::Text { text, .. } if text == "hello"));
    }

    #[test]
    fn image_round_trips() {
        let original = ItemContent::Image {
            data: "AAAA".into(),
            mime_type: "image/png".into(),
            uri: Some("file:///x.png".into()),
        };
        let block = item_content_to_block(original);
        let back = content_block_to_item_content(block);
        match back {
            ItemContent::Image {
                data,
                mime_type,
                uri,
            } => {
                assert_eq!(data, "AAAA");
                assert_eq!(mime_type, "image/png");
                assert_eq!(uri.as_deref(), Some("file:///x.png"));
            }
            other => panic!("expected image, got {other:?}"),
        }
    }

    #[test]
    fn audio_round_trips() {
        let original = ItemContent::Audio {
            data: "BBBB".into(),
            mime_type: "audio/wav".into(),
        };
        let block = item_content_to_block(original);
        let back = content_block_to_item_content(block);
        assert!(matches!(back, ItemContent::Audio { mime_type, .. } if mime_type == "audio/wav"));
    }

    #[test]
    fn resource_link_round_trips() {
        let original = ItemContent::ResourceLink {
            uri: "https://x".into(),
            name: Some("doc".into()),
            description: Some("a doc".into()),
            mime_type: Some("text/plain".into()),
        };
        let block = item_content_to_block(original);
        let back = content_block_to_item_content(block);
        match back {
            ItemContent::ResourceLink {
                uri,
                name,
                description,
                ..
            } => {
                assert_eq!(uri, "https://x");
                assert_eq!(name.as_deref(), Some("doc"));
                assert_eq!(description.as_deref(), Some("a doc"));
            }
            other => panic!("expected resource link, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_uses_provider_id_when_present() {
        let item = Item::ToolCall {
            id: zhive_proto::domain::ItemId("item:t/0".into()),
            name: "bash".into(),
            kind: ZToolKind::Execute,
            status: ZToolStatus::InProgress,
            content: vec![],
            locations: vec![],
            raw_input: None,
            raw_output: None,
            provider_tool_call_id: Some("toolu_abc".into()),
        };
        let id_a = tool_call_to_acp(&item).unwrap().tool_call_id;
        let id_b = tool_call_update_to_acp(&item).unwrap().tool_call_id;
        assert_eq!(id_a.0.as_ref(), "toolu_abc");
        assert_eq!(id_a, id_b, "ToolCall and ToolCallUpdate must share one id");
    }

    #[test]
    fn tool_call_falls_back_to_item_id() {
        let item = Item::ToolCall {
            id: zhive_proto::domain::ItemId("item:t/9".into()),
            name: "bash".into(),
            kind: ZToolKind::Execute,
            status: ZToolStatus::Completed,
            content: vec![],
            locations: vec![],
            raw_input: None,
            raw_output: None,
            provider_tool_call_id: None,
        };
        let id = tool_call_to_acp(&item).unwrap().tool_call_id;
        assert_eq!(id.0.as_ref(), "item:t/9");
    }

    #[test]
    fn diff_content_maps_to_acp_diff() {
        let content = ItemToolCallContent::Diff {
            path: "/tmp/a.rs".into(),
            old_text: Some("old".into()),
            new_text: "new".into(),
        };
        match tool_call_content_to_acp(content) {
            ToolCallContent::Diff(d) => {
                assert_eq!(d.new_text, "new");
                assert_eq!(d.old_text.as_deref(), Some("old"));
            }
            other => panic!("expected diff, got {other:?}"),
        }
    }

    #[test]
    fn non_tool_call_item_yields_no_tool_call() {
        let item = Item::AgentMessage {
            id: zhive_proto::domain::ItemId("item:t/0".into()),
            text: "x".into(),
        };
        assert!(tool_call_to_acp(&item).is_none());
        assert!(tool_call_update_to_acp(&item).is_none());
    }

    #[test]
    fn unmapped_item_yields_no_update() {
        let item = Item::ContextCompaction {
            id: zhive_proto::domain::ItemId("item:t/0".into()),
        };
        assert!(item_to_session_update(&item).is_none());
    }

    #[test]
    fn agent_message_item_is_suppressed() {
        // The finalised whole-block AgentMessage must NOT become a session
        // update: its text already streamed live via ItemDelta, so emitting it
        // here would render the agent's reply twice in the client.
        let item = Item::AgentMessage {
            id: zhive_proto::domain::ItemId("item:t/0".into()),
            text: "hello world".into(),
        };
        assert!(
            item_to_session_update(&item).is_none(),
            "AgentMessage must be suppressed to avoid duplicate rendering"
        );
    }

    #[test]
    fn agent_thought_item_still_maps() {
        // Thought/reasoning text has no delta path, so it must still surface as
        // an AgentThoughtChunk (no duplication risk).
        let item = Item::AgentThought {
            id: zhive_proto::domain::ItemId("item:t/0".into()),
            text: "thinking".into(),
        };
        assert!(matches!(
            item_to_session_update(&item),
            Some(SessionUpdate::AgentThoughtChunk(_))
        ));
    }
}

// Rust guideline compliant 2026-02-21

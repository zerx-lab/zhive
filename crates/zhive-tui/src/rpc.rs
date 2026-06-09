//! Typed engine actions layered over the native client's generic `call`.
//!
//! The conversation view is reconstructed purely from engine events
//! ([`crate::conversation`]), so these helpers are fire-and-observe commands:
//! `start_turn` submits input and returns once the engine accepts it; the
//! resulting transcript arrives asynchronously as `events/item_appended`
//! notifications. Engine-side failures surface as
//! [`crate::error::TuiError::Client`] via the `?` operator.

use serde::Deserialize;
use serde_json::json;
use zhive_client_native::Client;
use zhive_proto::domain::{Checkpoint, Item, ItemContent, ThreadId, TurnId};
use zhive_proto::permission::PermissionOutcome;

use crate::error::Result;
use crate::id::new_user_item_id;

/// Outcome of a manual `engine/compact` request.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompactOutcome {
    /// `"compacted"` or `"nothing_to_compact"`.
    pub status: String,
    /// Number of transcript items folded into the summary.
    pub entries_compacted: u32,
}

/// A persisted thread as shown in the `/session` resume list.
///
/// A distilled projection of the engine's `Thread` index entry — only the
/// fields the session-list overlay renders, so the TUI never depends on the
/// full domain `Thread` shape evolving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEntry {
    /// Stable thread id, used to resume the session.
    pub id: ThreadId,
    /// User-supplied label, if any; falls back to the preview when absent.
    pub title: Option<String>,
    /// First-message excerpt for the list row.
    pub preview: String,
    /// Unix timestamp (seconds) of the last update, for relative-time display.
    pub updated_at: i64,
    /// Parent thread id when this session is a spawned subagent child.
    ///
    /// `None` for top-level (user) threads. Used by resume to reattach a
    /// thread's historical subagent children as nested summaries; the picker
    /// itself lists all threads regardless of this field.
    pub subagent_parent: Option<ThreadId>,
}

/// The `thread/list` reply: the persisted thread index, newest first.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ListThreadsReply {
    threads: Vec<zhive_proto::domain::Thread>,
}

/// The `thread/get_items` reply: history items in conversation order.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct GetItemsReply {
    items: Vec<Item>,
}

/// Builds a multi-part user-message item by parsing `[Image #N]` placeholders
/// in `text` and interleaving the corresponding attachment bytes.
///
/// The resulting content preserves the order the user composed: text runs
/// before the first image, between images, and after the last image are each
/// emitted as separate `Text` blocks; each `[Image #N]` becomes an `Image`
/// block with base64-encoded bytes.  References to out-of-range N are dropped.
/// At least one block is always present (empty `Text` fallback).
fn user_message(
    thread: &ThreadId,
    text: &str,
    attachments: &[crate::app::ImageAttachment],
) -> Item {
    let content = parse_multipart(text, attachments);
    Item::UserMessage {
        id: new_user_item_id(thread),
        content,
    }
}

/// Splits `text` on `[Image #N]` tokens into an ordered `Vec<ItemContent>`.
fn parse_multipart(text: &str, attachments: &[crate::app::ImageAttachment]) -> Vec<ItemContent> {
    const TAG: &str = "[Image #";
    let mut content: Vec<ItemContent> = Vec::new();
    let mut rest = text;
    loop {
        if let Some(start) = rest.find(TAG) {
            let chunk = &rest[..start];
            if !chunk.is_empty() {
                content.push(ItemContent::Text {
                    text: chunk.to_owned(),
                    annotations: None,
                });
            }
            let after = &rest[start + TAG.len()..];
            if let Some(end) = after.find(']') {
                let n: usize = after[..end].trim().parse().unwrap_or(0);
                if n >= 1 && n <= attachments.len() {
                    let att = &attachments[n - 1];
                    content.push(ItemContent::Image {
                        data: crate::clipboard::base64_encode(&att.bytes),
                        mime_type: att.mime.to_owned(),
                        uri: None,
                    });
                }
                rest = &after[end + 1..];
            } else {
                // Unterminated token — emit remainder as plain text.
                content.push(ItemContent::Text {
                    text: rest[start..].to_owned(),
                    annotations: None,
                });
                break;
            }
        } else {
            if !rest.is_empty() {
                content.push(ItemContent::Text {
                    text: rest.to_owned(),
                    annotations: None,
                });
            }
            break;
        }
    }
    if content.is_empty() {
        content.push(ItemContent::Text {
            text: String::new(),
            annotations: None,
        });
    }
    content
}

/// Submits `text` (and any `attachments`) as a new turn on `thread`.
///
/// Returns once the engine has accepted the turn. The user message and all
/// agent output arrive afterwards as `events/item_appended` notifications, so
/// the caller does not need the allocated turn id.
///
/// # Errors
///
/// Returns [`crate::error::TuiError::Client`] if the engine rejects the turn
/// (e.g. busy) or the transport fails.
pub async fn start_turn(
    client: &Client,
    thread: &ThreadId,
    text: &str,
    attachments: &[crate::app::ImageAttachment],
    reasoning: zhive_proto::domain::ThinkingEffort,
) -> Result<()> {
    let item = user_message(thread, text, attachments);
    // `reasoning` is always sent explicitly (including `Off`) so the turn's
    // depth is exactly what the UI shows rather than an engine default.
    let params = json!({
        "threadId": thread,
        "userInput": [item],
        "scope": null,
        "reasoning": reasoning,
    });
    client.call("engine/start_turn", Some(params)).await?;
    Ok(())
}

/// Cancels the active turn on `thread`, if any.
///
/// Resolves to `Some(turn_id)` when a turn was interrupted, `None` otherwise.
///
/// # Errors
///
/// Returns [`crate::error::TuiError::Client`] on transport or engine failure.
pub async fn cancel_turn(client: &Client, thread: &ThreadId) -> Result<Option<TurnId>> {
    Ok(client.cancel_turn(thread).await?)
}

/// Resolves a pending permission request via `engine/resume_permission`.
///
/// # Errors
///
/// Returns [`crate::error::TuiError::Client`] on transport or engine failure.
pub async fn resume_permission(
    client: &Client,
    request_id: &str,
    outcome: PermissionOutcome,
) -> Result<()> {
    let params = json!({ "requestId": request_id, "outcome": outcome });
    client
        .call("engine/resume_permission", Some(params))
        .await?;
    Ok(())
}

/// Requests a manual compaction of `thread`'s transcript.
///
/// The `trigger` is omitted so the engine applies its `Manual` default.
///
/// # Errors
///
/// Returns [`crate::error::TuiError::Client`] if the engine is busy, the thread
/// is unknown, or summarization fails.
pub async fn compact(client: &Client, thread: &ThreadId) -> Result<CompactOutcome> {
    let params = json!({ "threadId": thread });
    let result = client.call("engine/compact", Some(params)).await?;
    Ok(serde_json::from_value(result).unwrap_or_default())
}

/// Outcome of an `engine/restore` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    /// New branch thread the conversation was forked into.
    pub new_thread_id: ThreadId,
    /// Number of files whose content was restored.
    pub reverted: u32,
    /// Number of files deleted (created after the checkpoint).
    pub deleted: u32,
}

/// Lists a thread's revertable checkpoints (oldest first) for the rewind picker.
///
/// # Errors
///
/// [`crate::error::TuiError::Client`] when the engine call fails.
pub async fn list_checkpoints(client: &Client, thread: &ThreadId) -> Result<Vec<Checkpoint>> {
    let params = json!({ "threadId": thread });
    let result = client
        .call(zhive_proto::methods::METHOD_LIST_CHECKPOINTS, Some(params))
        .await?;
    let reply: zhive_proto::rpc::ListCheckpointsResult = serde_json::from_value(result)
        .map_err(|e| zhive_client_native::ClientError::Decode(e.to_string()))?;
    Ok(reply.checkpoints)
}

/// Reverts the workspace and conversation to a checkpoint.
///
/// # Errors
///
/// [`crate::error::TuiError::Client`] when the engine call fails.
pub async fn restore(
    client: &Client,
    thread: &ThreadId,
    target_turn: &TurnId,
) -> Result<RestoreOutcome> {
    let params = json!({ "threadId": thread, "targetTurnId": target_turn });
    let result = client
        .call(zhive_proto::methods::METHOD_RESTORE, Some(params))
        .await?;
    let reply: zhive_proto::rpc::RestoreResult = serde_json::from_value(result)
        .map_err(|e| zhive_client_native::ClientError::Decode(e.to_string()))?;
    Ok(RestoreOutcome {
        new_thread_id: reply.new_thread_id,
        reverted: reply.reverted,
        deleted: reply.deleted,
    })
}

/// Asks the engine to shut down (best-effort; ignores the reply).
///
/// # Errors
///
/// Returns [`crate::error::TuiError::Client`] on transport failure.
pub async fn shutdown(client: &Client) -> Result<()> {
    client.call("engine/shutdown", None).await?;
    Ok(())
}

/// Lists the models the active provider exposes, for the `/models` picker.
///
/// The engine forwards to its host model catalogue; the active model is flagged
/// in the returned [`ModelDescriptor`]s. An empty list comes back when no
/// catalogue is configured for the provider kind.
///
/// [`ModelDescriptor`]: zhive_proto::rpc::ModelDescriptor
///
/// # Errors
///
/// Returns [`crate::error::TuiError::Client`] on transport or engine failure
/// (e.g. the provider's `/models` endpoint is unreachable).
pub async fn list_models(client: &Client) -> Result<Vec<zhive_proto::rpc::ModelDescriptor>> {
    let result = client.call("models/list", None).await?;
    let reply: zhive_proto::rpc::ListModelsResult =
        serde_json::from_value(result).unwrap_or_default();
    Ok(reply.models)
}

/// Hot-swaps the engine's active model, returning the resolved context window.
///
/// `context_window` is the value the picker already knows for the model, sent as
/// a hint so the engine need not re-fetch; the engine prefers a host override
/// when one is configured.
///
/// # Errors
///
/// Returns [`crate::error::TuiError::Client`] when the switch fails (unknown
/// model id, provider build error, or transport failure).
pub async fn set_model(
    client: &Client,
    model_id: &str,
    context_window: Option<u64>,
) -> Result<zhive_proto::rpc::SetModelResult> {
    let params = json!({ "modelId": model_id, "contextWindow": context_window });
    let result = client.call("engine/set_model", Some(params)).await?;
    Ok(serde_json::from_value(result).unwrap_or_default())
}

/// Lists persisted threads for the `/session` resume picker, newest first.
///
/// When `cwd` is `Some(path)`, the engine scopes the listing to threads created
/// under that working directory (codex-style per-project view); `None` lists
/// every persisted thread. Each [`SessionEntry`] is the distilled projection the
/// overlay renders; the full domain `Thread` (with its empty `turns` field) is
/// discarded.
///
/// # Errors
///
/// Returns [`crate::error::TuiError::Client`] on transport or engine failure.
pub async fn list_threads(client: &Client, cwd: Option<&str>) -> Result<Vec<SessionEntry>> {
    // Omit the params entirely when unfiltered so the engine takes its
    // "list every thread" path; a `{ cwd }` body scopes the listing.
    let params = cwd.map(|c| json!({ "cwd": c }));
    let result = client.call("thread/list", params).await?;
    let reply: ListThreadsReply = serde_json::from_value(result).unwrap_or_default();
    Ok(reply
        .threads
        .into_iter()
        .map(|t| SessionEntry {
            id: t.id,
            title: t.name,
            preview: t.preview,
            updated_at: t.updated_at,
            subagent_parent: t.subagent_parent,
        })
        .collect())
}

/// Restores `thread`'s history into the engine so it can accept new turns.
///
/// Resolving this before submitting a turn ensures the engine replays the
/// persisted rollout, so the continued conversation carries its prior context.
/// The reply (counts of restored items/turns) is not surfaced to the caller.
///
/// # Errors
///
/// Returns [`crate::error::TuiError::Client`] if storage is unavailable, the
/// thread is unknown, or the transport fails.
pub async fn resume_thread(client: &Client, thread: &ThreadId) -> Result<()> {
    let params = json!({ "threadId": thread });
    client.call("engine/resume_thread", Some(params)).await?;
    Ok(())
}

/// Fetches `thread`'s full history items for replay into the resumed view.
///
/// Returns items in conversation order; the caller folds them into a fresh
/// [`crate::conversation::Conversation`] via `load_history`.
///
/// # Errors
///
/// Returns [`crate::error::TuiError::Client`] if the thread is unknown or the
/// transport fails.
pub async fn get_thread_items(client: &Client, thread: &ThreadId) -> Result<Vec<Item>> {
    let params = json!({ "threadId": thread });
    let result = client.call("thread/get_items", Some(params)).await?;
    let reply: GetItemsReply = serde_json::from_value(result).unwrap_or_default();
    Ok(reply.items)
}

/// One restored subagent child of a resumed thread.
///
/// Carries just enough to rebuild a nested-subagent summary via
/// [`crate::conversation::Conversation::restore_subagents`]: the child thread
/// id, optional agent type / description, and the child's restored history.
#[derive(Debug, Clone)]
pub struct SubagentRestore {
    /// The child thread the subagent ran in.
    pub child_thread_id: ThreadId,
    /// The subagent definition name, if recorded.
    pub agent_type: Option<String>,
    /// The task description, if recorded.
    pub description: Option<String>,
    /// The child's restored history items, in conversation order.
    pub items: Vec<Item>,
}

/// Loads the historical subagent children of `parent` for nested-summary replay.
///
/// Lists every persisted thread (unfiltered, so children created in a different
/// `cwd` than the current one are still found), keeps those whose
/// `subagent_parent` equals `parent`, and fetches each child's full history. A
/// child whose history cannot be read is skipped (best-effort: a missing child
/// never blocks resuming the parent). Returns an empty `Vec` when the parent has
/// no subagent children.
///
/// The `agent_type` / `description` are not persisted in the thread index, so
/// they come back `None`; the child transcript itself is the restored content.
///
/// # Errors
///
/// Returns [`crate::error::TuiError::Client`] if the thread list cannot be read.
pub async fn resume_subagent_children(
    client: &Client,
    parent: &ThreadId,
) -> Result<Vec<SubagentRestore>> {
    // Unfiltered list so children are found regardless of the cwd filter the
    // picker was last in; we re-decode the raw threads to read subagent_parent.
    let result = client.call("thread/list", None).await?;
    let reply: ListThreadsReply = serde_json::from_value(result).unwrap_or_default();

    // Keep matching children with their timestamps so we can restore them in
    // spawn order. `thread/list` returns newest-first, but the parent's inline
    // summaries are consumed in transcript (chronological) order, so we sort
    // ascending by `updated_at` to align child N with the Nth `agent` tool call.
    let mut children: Vec<(i64, ThreadId)> = reply
        .threads
        .into_iter()
        .filter(|t| t.subagent_parent.as_ref() == Some(parent))
        .map(|t| (t.updated_at, t.id))
        .collect();
    children.sort_by_key(|(updated_at, _)| *updated_at);

    let mut restored = Vec::new();
    for (_updated_at, child_id) in children {
        // Best-effort: a child whose items fail to load is skipped rather than
        // failing the whole resume.
        let items = get_thread_items(client, &child_id)
            .await
            .unwrap_or_default();
        restored.push(SubagentRestore {
            child_thread_id: child_id,
            agent_type: None,
            description: None,
            items,
        });
    }
    Ok(restored)
}

// Rust guideline compliant 2026-02-21

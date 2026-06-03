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
use zhive_proto::domain::{Item, ItemContent, ThreadId, TurnId};
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

/// Builds a single-text user-message item addressed to `thread`.
fn user_message(thread: &ThreadId, text: &str) -> Item {
    Item::UserMessage {
        id: new_user_item_id(thread),
        content: vec![ItemContent::Text {
            text: text.to_owned(),
            annotations: None,
        }],
    }
}

/// Submits `text` as a new turn on `thread`.
///
/// Returns once the engine has accepted the turn. The user message and all
/// agent output arrive afterwards as `events/item_appended` notifications, so
/// the caller does not need the allocated turn id.
///
/// # Errors
///
/// Returns [`crate::error::TuiError::Client`] if the engine rejects the turn
/// (e.g. busy) or the transport fails.
pub async fn start_turn(client: &Client, thread: &ThreadId, text: &str) -> Result<()> {
    let item = user_message(thread, text);
    let params = json!({ "threadId": thread, "userInput": [item], "scope": null });
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

/// Asks the engine to shut down (best-effort; ignores the reply).
///
/// # Errors
///
/// Returns [`crate::error::TuiError::Client`] on transport failure.
pub async fn shutdown(client: &Client) -> Result<()> {
    client.call("engine/shutdown", None).await?;
    Ok(())
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

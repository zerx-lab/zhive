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

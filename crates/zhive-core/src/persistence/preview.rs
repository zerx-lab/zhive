//! Deterministic (no-LLM) thread-preview derivation, shared between the live
//! turn path and the historical backfill.
//!
//! A thread's preview is its opening user prompt rather than a model-generated
//! summary (mirrors codex's `set_preview_if_empty`). The same derivation runs in
//! two places, which is why it lives here rather than inside the engine:
//!
//! * [`crate::engine`]'s `start_turn` derives the preview from a turn's input
//!   items the moment a turn begins (see `engine::lifecycle`, which delegates
//!   here).
//! * [`super::Storage::backfill_thread_metadata`] derives it from a persisted
//!   rollout for threads recorded before the live path filled the column.
//!
//! Keeping one implementation guarantees a backfilled preview is byte-identical
//! to one the engine would have produced live.

use zhive_proto::domain::{Item, ItemContent};

/// Maximum number of characters retained in a derived thread preview.
///
/// Long enough to make a list entry recognisable, short enough to keep the
/// `threads.preview` column and any list-view rendering compact. Truncation is
/// applied on a character boundary (not a byte boundary) so multi-byte input is
/// never split mid-codepoint.
pub const PREVIEW_MAX_CHARS: usize = 80;

/// Derives a thread preview from the first user message in `items`.
///
/// Scans `items` for the first [`Item::UserMessage`], concatenates its
/// [`ItemContent::Text`] parts with a single space, trims surrounding
/// whitespace, and truncates the result to [`PREVIEW_MAX_CHARS`] characters.
/// Returns an empty string when no user message with text content is present.
///
/// This is the deterministic (no-LLM) title/preview derivation, shared by the
/// live turn path and the historical backfill so both agree byte-for-byte.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_core::persistence::preview::derive_preview_from_items;
/// use zhive_proto::domain::{Item, ItemContent, ItemId};
///
/// let items = vec![Item::UserMessage {
///     id: ItemId(Arc::from("item:0")),
///     content: vec![ItemContent::Text { text: "  hello world  ".into(), annotations: None }],
/// }];
/// assert_eq!(derive_preview_from_items(&items), "hello world");
/// assert!(derive_preview_from_items(&[]).is_empty());
/// ```
#[must_use]
pub fn derive_preview_from_items(items: &[Item]) -> String {
    let Some(text) = items.iter().find_map(|item| match item {
        Item::UserMessage { content, .. } => {
            let joined = content
                .iter()
                .filter_map(|part| match part {
                    ItemContent::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            Some(joined)
        }
        _ => None,
    }) else {
        return String::new();
    };

    truncate_preview(&text)
}

/// Trims and truncates `text` to [`PREVIEW_MAX_CHARS`] characters.
///
/// Trims surrounding whitespace, then truncates on a character boundary so
/// multi-byte input is never split mid-codepoint.
///
/// # Examples
///
/// ```
/// use zhive_core::persistence::preview::{truncate_preview, PREVIEW_MAX_CHARS};
///
/// assert_eq!(truncate_preview("  spaced  "), "spaced");
/// let long: String = "é".repeat(200);
/// assert_eq!(truncate_preview(&long).chars().count(), PREVIEW_MAX_CHARS);
/// ```
#[must_use]
pub fn truncate_preview(text: &str) -> String {
    text.trim().chars().take(PREVIEW_MAX_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zhive_proto::domain::ItemId;

    use super::*;

    fn user_msg(parts: &[&str]) -> Item {
        Item::UserMessage {
            id: ItemId(Arc::from("item:test/0")),
            content: parts
                .iter()
                .map(|t| ItemContent::Text {
                    text: (*t).to_owned(),
                    annotations: None,
                })
                .collect(),
        }
    }

    #[test]
    fn joins_first_user_message_text() {
        let items = vec![user_msg(&["  hello", "world  "])];
        assert_eq!(derive_preview_from_items(&items), "hello world");
    }

    #[test]
    fn empty_without_user_message() {
        let items = vec![Item::AgentMessage {
            id: ItemId(Arc::from("item:test/0")),
            text: "agent only".into(),
        }];
        assert!(derive_preview_from_items(&items).is_empty());
    }

    #[test]
    fn uses_first_user_message() {
        let items = vec![user_msg(&["first"]), user_msg(&["second"])];
        assert_eq!(derive_preview_from_items(&items), "first");
    }

    #[test]
    fn truncate_caps_chars_on_boundary() {
        let long: String = "é".repeat(200);
        let truncated = truncate_preview(&long);
        assert_eq!(truncated.chars().count(), PREVIEW_MAX_CHARS);
        assert!(truncated.chars().all(|c| c == 'é'));
    }

    #[test]
    fn truncate_trims() {
        assert_eq!(truncate_preview("  spaced  "), "spaced");
    }
}

// Rust guideline compliant 2026-06-03

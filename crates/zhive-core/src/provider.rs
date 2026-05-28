//! Provider boundary (D-010 + B10).
//!
//! The engine reaches LLMs through this module, never through
//! `llmsdk` directly. Phase 1 ships:
//!
//! * [`ProviderAdapter`] — a thin trait that re-exports `llmsdk`'s
//!   streaming API under a zhive-controlled surface, so future bridge
//!   crates can swap implementations without churning every call site.
//! * [`fold_text`] — convenience that aggregates `text-delta` chunks
//!   into a finalised [`zhive_proto::domain::Item::AgentMessage`].
//!
//! The complete `StreamPart → Item` projection (tool calls, reasoning,
//! tool-call output) lands in a follow-up once the engine actor wires
//! into the provider; the fold helper here covers the minimum needed
//! to exercise the boundary end-to-end.

use thiserror::Error;
use zhive_proto::domain::{Item, ItemId};

/// Reasons a provider call surfaces back to the engine.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// The provider returned an unrecoverable error.
    #[error("provider error: {0}")]
    Other(String),
}

/// Minimal façade over `llmsdk::LanguageModel`.
///
/// Implementations are expected to be `Send + Sync + Clone` so the
/// engine actor can fan them out to subagents without per-clone
/// initialisation cost.
pub trait ProviderAdapter: Send + Sync {
    /// Returns a stable provider identifier (e.g. `"openai:gpt-4o"`).
    fn name(&self) -> &str;
}

/// Aggregates a sequence of text deltas into one
/// [`Item::AgentMessage`].
///
/// `item_id` is the per-turn id assigned by the engine. `chunks` is
/// the ordered list of `text-delta` payloads produced by the provider
/// stream; an empty list still produces a (blank) message.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_proto::domain::{Item, ItemId};
/// use zhive_core::provider::fold_text;
///
/// let id = ItemId(Arc::from("item:t/0"));
/// let item = fold_text(id.clone(), ["Hello", ", ", "world"]);
/// match item {
///     Item::AgentMessage { id: got_id, text } => {
///         assert_eq!(got_id, id);
///         assert_eq!(text, "Hello, world");
///     }
///     _ => panic!("expected AgentMessage"),
/// }
/// ```
#[must_use]
pub fn fold_text<I>(item_id: ItemId, chunks: I) -> Item
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let mut text = String::new();
    for chunk in chunks {
        text.push_str(chunk.as_ref());
    }
    Item::AgentMessage { id: item_id, text }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn fold_text_concatenates_in_order() {
        let id = ItemId(Arc::from("item:t/0"));
        let item = fold_text(id.clone(), ["a", "b", "c"]);
        match item {
            Item::AgentMessage { id: got, text } => {
                assert_eq!(got, id);
                assert_eq!(text, "abc");
            }
            other => panic!("expected AgentMessage, got {other:?}"),
        }
    }

    #[test]
    fn fold_text_empty_iterator_yields_blank_message() {
        let id = ItemId(Arc::from("item:t/0"));
        let item = fold_text(id, std::iter::empty::<&str>());
        match item {
            Item::AgentMessage { text, .. } => assert!(text.is_empty()),
            other => panic!("expected AgentMessage, got {other:?}"),
        }
    }
}

// Rust guideline compliant 2026-02-21

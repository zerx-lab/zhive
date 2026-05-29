//! LLM provider boundary for zhive (B10 decision: reuse llmsdk directly).
//!
//! Per B10 §2 (decision finalized 2026-05-28) zhive does **not** invent its
//! own `ProviderAdapter` trait. The engine holds a [`DynLanguageModel`], which
//! is llmsdk's `Arc<dyn LanguageModel>` newtype — cheap to clone and already
//! `Send + Sync`.
//!
//! # Sub-modules / top-level items
//!
//! * [`DynLanguageModel`] — re-exported provider handle; the sole zhive
//!   provider boundary type.
//! * [`ProviderError`] — engine-visible error surfaced when a provider call
//!   fails at the call-site level (outer `Err`). Carries a conversion from
//!   [`llmsdk::ProviderError`].
//! * [`StreamFold`] — pure (no async, no I/O) fold of a [`StreamPart`]
//!   sequence into zhive [`Item`]s. The engine owns the channel; `StreamFold`
//!   only does the mapping.
//! * [`ScriptedModel`] — in-memory deterministic model for unit tests and
//!   doctests; always compiled, no new dependencies.
//! * [`fold_text`] — convenience aggregating text delta chunks into a single
//!   [`Item::AgentMessage`]; used by callers that already hold a flat
//!   `Vec<&str>` and don't need the full fold machinery.
//!
//! # Design deviations from B10 §3 sketch
//!
//! **Deviation 1 — `Vec<Item>` instead of `mpsc::Sender<Item>`.**
//! B10 §3 shows `StreamFold::fold` pushing items through an `mpsc::Sender`.
//! This increment returns `Vec<Item>` instead, so callers can unit-test
//! without a tokio runtime. The engine forwards returned items to its own
//! channel; the observable semantics are identical.
//!
//! **Finalize-on-boundary emission model (inc1b).**
//! [`StreamFold`] emits **exactly one finalized [`Item`] per provider block**,
//! on that block's terminal boundary (`TextEnd`, `ReasoningEnd`,
//! `ToolInputEnd`), or on [`StreamFold::finish`] for a truncated stream.
//! The [`ItemId`] is minted when the block opens (`*Start`) and used for that
//! single emission — so the same id is never emitted twice and never collides
//! with the persistence primary key (`items.id PRIMARY KEY` in increment 5).
//!
//! Live token-by-token streaming, if needed by the UI, is a separate
//! chunk-notification concern layered by the engine, not part of the persisted
//! [`Item`] stream.

pub mod scripted;
pub mod stream_fold;

pub use scripted::ScriptedModel;
pub use stream_fold::StreamFold;

use thiserror::Error;

use zhive_proto::domain::{Item, ItemId};

/// The zhive provider boundary — cheap-to-clone `Send + Sync` language model handle.
///
/// Re-exported from [`llmsdk::provider::DynLanguageModel`] (a newtype over
/// `Arc<dyn LanguageModel>`). The engine holds one instance per thread and
/// calls `do_stream(CallOptions) -> StreamResult` to obtain the streaming
/// response.
///
/// This re-export is the single import point callers need; they do not have
/// to reach into `llmsdk` directly.
#[doc(inline)]
pub use llmsdk::provider::DynLanguageModel;

// ============================================================
// ProviderError
// ============================================================

/// Error surfaced to the engine when a provider call fails at the call level.
///
/// This is distinct from in-stream provider errors, which arrive as
/// [`StreamPart`] and are folded to [`Item::SystemNotice`] by
/// [`StreamFold`].
///
/// # Errors
///
/// [`ProviderError::Other`] carries the `Display` string of the underlying
/// [`llmsdk::ProviderError`]. Use [`ProviderError::from_llmsdk`] to convert.
///
/// [`StreamPart`]: llmsdk::language_model::StreamPart
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// The provider returned an unrecoverable error.
    #[error("provider error: {0}")]
    Other(String),
}

impl ProviderError {
    /// Convert an [`llmsdk::ProviderError`] to a zhive [`ProviderError`].
    ///
    /// Maps to [`ProviderError::Other`] carrying the error's `Display`
    /// string. This is intentionally lossy: the engine only needs to surface
    /// a human-readable message; structured provider error fields are not part
    /// of the zhive protocol in Phase 1.
    #[must_use]
    pub fn from_llmsdk(err: &llmsdk::ProviderError) -> Self {
        Self::Other(err.to_string())
    }
}

impl From<llmsdk::ProviderError> for ProviderError {
    fn from(err: llmsdk::ProviderError) -> Self {
        Self::from_llmsdk(&err)
    }
}

// ============================================================
// fold_text (retained convenience)
// ============================================================

/// Aggregates a sequence of text delta chunks into one [`Item::AgentMessage`].
///
/// `item_id` is the per-turn id assigned by the engine. `chunks` is the
/// ordered list of `text-delta` payloads produced by the provider stream;
/// an empty list still produces a (blank) message.
///
/// Use this when you already hold a flat iterator of delta strings and do not
/// need the full [`StreamFold`] machinery.
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

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zhive_proto::domain::{Item, ItemId};

    use super::{ProviderError, fold_text};

    fn item_id(s: &str) -> ItemId {
        ItemId(Arc::from(s))
    }

    // ---- fold_text ----

    #[test]
    fn fold_text_concatenates_in_order() {
        let id = item_id("item:t/0");
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
        let id = item_id("item:t/0");
        let item = fold_text(id, std::iter::empty::<&str>());
        match item {
            Item::AgentMessage { text, .. } => assert!(text.is_empty()),
            other => panic!("expected AgentMessage, got {other:?}"),
        }
    }

    // ---- ProviderError conversion ----

    #[test]
    fn provider_error_from_llmsdk_carries_display() {
        let sdk_err = llmsdk::ProviderError::no_such_model("gpt-0", "languageModel");
        let err = ProviderError::from(sdk_err);
        match &err {
            ProviderError::Other(msg) => assert!(msg.contains("gpt-0")),
        }
    }
}

// Rust guideline compliant 2026-02-21

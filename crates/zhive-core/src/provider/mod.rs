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

use std::collections::HashMap;
use std::time::Duration;

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
/// [`StreamPart`] and are folded to [`Item::SystemNotice`] by [`StreamFold`].
///
/// Variants are classified so the engine can decide whether to retry:
/// - [`ProviderError::RateLimit`] and [`ProviderError::Transient`] are
///   retryable (the call should be re-issued after a backoff).
/// - [`ProviderError::Other`] is fatal (the turn should be failed immediately).
///
/// Use [`ProviderError::from_llmsdk`] to map from an [`llmsdk::ProviderError`].
/// Use [`ProviderError::is_retryable`] to branch on whether a retry is warranted.
///
/// [`StreamPart`]: llmsdk::language_model::StreamPart
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ProviderError {
    /// HTTP 429 rate limit.
    ///
    /// `retry_after` carries the server-advised delay parsed from the
    /// `Retry-After` response header (integer-seconds form) when the provider
    /// sent one; otherwise `None`, and the engine falls back to its computed
    /// exponential back-off.
    #[error("provider rate limited{}", .retry_after.map(|d| format!(" (retry after {d:?})")).unwrap_or_default())]
    RateLimit {
        /// Advised back-off delay from the `Retry-After` response header, if
        /// the provider sent one and it parsed as integer seconds.
        retry_after: Option<std::time::Duration>,
    },
    /// Transient HTTP failure (408 / 409 / 5xx) flagged retryable by llmsdk.
    #[error("transient provider error: {0}")]
    Transient(String),
    /// Unrecoverable provider error (other 4xx, auth failure, no-such-model, …).
    #[error("provider error: {0}")]
    Other(String),
}

impl ProviderError {
    /// Convert an [`llmsdk::ProviderError`] to a classified zhive [`ProviderError`].
    ///
    /// Classification uses llmsdk's public inspection helpers only (never
    /// matching private `ErrorKind` variants):
    ///
    /// - HTTP 429 → [`ProviderError::RateLimit`]
    /// - `err.is_retryable()` true (408 / 409 / 5xx) → [`ProviderError::Transient`]
    /// - everything else → [`ProviderError::Other`]
    ///
    /// On a 429, `retry_after` is populated from the `Retry-After` response
    /// header (integer-seconds form) via
    /// [`llmsdk::ProviderError::response_headers`] when present; absent or
    /// non-integer (HTTP-date) values yield `None`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::provider::ProviderError;
    ///
    /// let sdk_err = llmsdk::ProviderError::no_such_model("gpt-0", "languageModel");
    /// let err = ProviderError::from_llmsdk(&sdk_err);
    /// assert!(!err.is_retryable());
    /// ```
    #[must_use]
    pub fn from_llmsdk(err: &llmsdk::ProviderError) -> Self {
        if err.status_code() == Some(429) {
            // Honor the server-advised Retry-After when present; falls back to
            // the engine's computed back-off when absent or non-integer.
            let retry_after = err.response_headers().and_then(parse_retry_after);
            Self::RateLimit { retry_after }
        } else if err.is_retryable() {
            // is_retryable() covers HTTP 408, 409, and 5xx per llmsdk defaults.
            Self::Transient(err.to_string())
        } else {
            Self::Other(err.to_string())
        }
    }

    /// Returns `true` when this error is safe to retry.
    ///
    /// Both [`ProviderError::RateLimit`] and [`ProviderError::Transient`]
    /// are retryable; [`ProviderError::Other`] is not.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::provider::ProviderError;
    ///
    /// assert!(!ProviderError::Other("fatal".into()).is_retryable());
    /// ```
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimit { .. } | Self::Transient(_))
    }
}

impl From<llmsdk::ProviderError> for ProviderError {
    fn from(err: llmsdk::ProviderError) -> Self {
        Self::from_llmsdk(&err)
    }
}

/// Parses a `Retry-After` HTTP header value into a back-off [`Duration`].
///
/// The header lookup is case-insensitive because header maps preserve each
/// transport's original key casing. Only the integer-seconds form is
/// recognised — the form Anthropic and `OpenAI` send. The HTTP-date form is
/// intentionally not parsed (it would require a date-parsing dependency this
/// crate avoids), so such values yield `None` and the caller keeps its own
/// computed back-off.
fn parse_retry_after(headers: &HashMap<String, String>) -> Option<Duration> {
    let raw = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .map(|(_, value)| value.trim())?;
    raw.parse::<u64>().ok().map(Duration::from_secs)
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
            other => panic!("expected Other, got {other:?}"),
        }
    }

    #[test]
    fn provider_error_429_maps_to_rate_limit() {
        let sdk_err = llmsdk::ProviderError::api_call_builder("https://api.test", "rate limited")
            .status_code(429)
            .build();
        let err = ProviderError::from(sdk_err);
        assert!(
            matches!(err, ProviderError::RateLimit { .. }),
            "expected RateLimit, got {err:?}"
        );
        assert!(err.is_retryable());
    }

    #[test]
    fn provider_error_429_with_retry_after_header_parses_seconds() {
        use std::collections::HashMap;
        use std::time::Duration;

        let mut headers = HashMap::new();
        // Mixed casing exercises the case-insensitive header lookup.
        headers.insert("Retry-After".to_string(), "30".to_string());
        let sdk_err = llmsdk::ProviderError::api_call_builder("https://api.test", "rate limited")
            .status_code(429)
            .response_headers(headers)
            .build();
        match ProviderError::from(sdk_err) {
            ProviderError::RateLimit { retry_after } => {
                assert_eq!(retry_after, Some(Duration::from_secs(30)));
            }
            other => panic!("expected RateLimit, got {other:?}"),
        }
    }

    #[test]
    fn provider_error_429_without_header_has_no_hint() {
        let sdk_err = llmsdk::ProviderError::api_call_builder("https://api.test", "rate limited")
            .status_code(429)
            .build();
        match ProviderError::from(sdk_err) {
            ProviderError::RateLimit { retry_after } => assert!(retry_after.is_none()),
            other => panic!("expected RateLimit, got {other:?}"),
        }
    }

    #[test]
    fn provider_error_429_non_integer_retry_after_is_ignored() {
        use std::collections::HashMap;

        let mut headers = HashMap::new();
        // The HTTP-date form is intentionally unsupported and must fall back to None.
        headers.insert(
            "retry-after".to_string(),
            "Wed, 21 Oct 2026 07:28:00 GMT".to_string(),
        );
        let sdk_err = llmsdk::ProviderError::api_call_builder("https://api.test", "rate limited")
            .status_code(429)
            .response_headers(headers)
            .build();
        match ProviderError::from(sdk_err) {
            ProviderError::RateLimit { retry_after } => assert!(retry_after.is_none()),
            other => panic!("expected RateLimit, got {other:?}"),
        }
    }

    #[test]
    fn provider_error_503_maps_to_transient() {
        let sdk_err =
            llmsdk::ProviderError::api_call_builder("https://api.test", "service unavailable")
                .status_code(503)
                .build();
        let err = ProviderError::from(sdk_err);
        assert!(
            matches!(err, ProviderError::Transient(_)),
            "expected Transient, got {err:?}"
        );
        assert!(err.is_retryable());
    }

    #[test]
    fn provider_error_fatal_is_not_retryable() {
        let err = ProviderError::Other("fatal auth failure".into());
        assert!(!err.is_retryable());
    }
}

// Rust guideline compliant 2026-02-21

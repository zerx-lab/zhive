//! Deterministic in-memory language model for tests and examples.
//!
//! [`ScriptedModel`] is always compiled (no feature gate) so it is available
//! in integration tests, doctests, and increment-2 turn tests without any
//! additional setup.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream;

use llmsdk::LanguageModel;
use llmsdk::language_model::{BoxStream, CallOptions, GenerateResult, StreamPart, StreamResult};

use super::DynLanguageModel;

// ============================================================
// ScriptedModel
// ============================================================

/// Deterministic in-memory model for tests and examples.
///
/// Constructed from a [`Vec<StreamPart>`]; its `do_stream` returns a
/// [`StreamResult`] that yields exactly those parts. `do_generate` is a
/// minimal stub that returns an empty [`GenerateResult`].
///
/// This model is always compiled (no feature gate) so it is available in
/// integration tests, doctests, and increment-2 turn tests without any
/// additional setup.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use llmsdk::language_model::{StreamPart, CallOptions};
/// use llmsdk::LanguageModel;
/// use zhive_core::provider::ScriptedModel;
///
/// # let rt = tokio::runtime::Builder::new_current_thread()
/// #     .enable_all()
/// #     .build()
/// #     .unwrap();
/// # rt.block_on(async {
/// let model = ScriptedModel::new(
///     "test-provider",
///     "test-model",
///     vec![
///         StreamPart::TextStart { id: "b0".into(), provider_metadata: None },
///         StreamPart::TextDelta { id: "b0".into(), delta: "hi".into(), provider_metadata: None },
///         StreamPart::TextEnd   { id: "b0".into(), provider_metadata: None },
///     ],
/// );
///
/// use futures::StreamExt;
/// let mut result = model.do_stream(CallOptions::default()).await.unwrap();
/// let mut parts = Vec::new();
/// while let Some(Ok(p)) = result.stream.next().await {
///     parts.push(p);
/// }
/// assert_eq!(parts.len(), 3);
/// # });
/// ```
#[derive(Debug, Clone)]
pub struct ScriptedModel {
    provider_id: Arc<str>,
    model_id: Arc<str>,
    parts: Arc<Vec<StreamPart>>,
}

impl ScriptedModel {
    /// Build a scripted model from a list of [`StreamPart`]s.
    pub fn new(
        provider_id: impl Into<Arc<str>>,
        model_id: impl Into<Arc<str>>,
        parts: Vec<StreamPart>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            parts: Arc::new(parts),
        }
    }

    /// Wrap `self` in a [`DynLanguageModel`] for engine injection.
    #[must_use]
    pub fn into_dyn(self) -> DynLanguageModel {
        DynLanguageModel::new(self)
    }
}

#[async_trait]
impl LanguageModel for ScriptedModel {
    fn provider(&self) -> &str {
        &self.provider_id
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn do_generate(&self, _options: CallOptions) -> llmsdk::error::Result<GenerateResult> {
        use llmsdk::language_model::{FinishReason, FinishReasonKind};

        Ok(GenerateResult {
            content: vec![],
            finish_reason: FinishReason::new(FinishReasonKind::Stop),
            usage: llmsdk::language_model::Usage::default(),
            provider_metadata: None,
            request: None,
            response: None,
            warnings: vec![],
        })
    }

    async fn do_stream(&self, _options: CallOptions) -> llmsdk::error::Result<StreamResult> {
        // Clone the Arc so the captured vec lives for `'static`.
        let parts_vec: Vec<StreamPart> = (*self.parts).clone();
        let iter = parts_vec
            .into_iter()
            .map(Ok::<StreamPart, llmsdk::ProviderError>);
        let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::iter(iter));
        Ok(StreamResult {
            stream: s,
            request: None,
            response: None,
        })
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scripted_model_stream_yields_parts() {
        use futures::StreamExt;
        let parts = vec![
            StreamPart::TextStart {
                id: "b0".into(),
                provider_metadata: None,
            },
            StreamPart::TextEnd {
                id: "b0".into(),
                provider_metadata: None,
            },
        ];
        let model = ScriptedModel::new("test", "m", parts.clone());
        let mut result = model.do_stream(CallOptions::default()).await.unwrap();
        let mut collected = Vec::new();
        while let Some(Ok(p)) = result.stream.next().await {
            collected.push(p);
        }
        assert_eq!(collected, parts);
    }

    #[tokio::test]
    async fn scripted_model_into_dyn_works() {
        use futures::StreamExt;
        let model = ScriptedModel::new(
            "test",
            "m2",
            vec![StreamPart::TextStart {
                id: "x".into(),
                provider_metadata: None,
            }],
        );
        let dyn_model = model.into_dyn();
        let mut result = dyn_model.do_stream(CallOptions::default()).await.unwrap();
        let first = result.stream.next().await;
        assert!(first.is_some());
    }
}

// Rust guideline compliant 2026-02-21

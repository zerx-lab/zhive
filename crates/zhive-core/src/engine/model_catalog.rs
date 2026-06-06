//! Host-injected catalogue for listing and hot-swapping provider models.
//!
//! The engine binds one provider at construction and cannot, on its own,
//! enumerate the models a remote endpoint offers or build a provider for a
//! different model id — both need provider credentials and a `/models`
//! endpoint the engine deliberately does not hold (D-002). [`ModelCatalog`]
//! closes that gap: the host implements it over its provider config and
//! injects it onto the [`Engine`] handle via
//! [`Engine::with_model_catalog`].
//!
//! When a catalogue is present the `models/list` and `engine/set_model` RPCs
//! are served; when absent they report that model management is unavailable,
//! so an engine built without a host catalogue keeps its prior behaviour.
//!
//! [`Engine`]: super::Engine
//! [`Engine::with_model_catalog`]: super::Engine::with_model_catalog

use async_trait::async_trait;
use thiserror::Error;
use zhive_proto::rpc::ModelDescriptor;

use crate::provider::DynLanguageModel;

/// A rebuilt provider plus the context window resolved for its model.
///
/// Returned by [`ModelCatalog::switch`]; the engine swaps the provider into
/// the running turn loop and applies `context_window` to its auto-compaction
/// budget.
#[derive(Debug, Clone)]
pub struct SwitchedModel {
    /// Provider bound to the requested model.
    pub provider: DynLanguageModel,
    /// Context window (maximum input tokens) the host resolved, if known.
    pub context_window: Option<u64>,
}

impl SwitchedModel {
    /// Constructs a switch outcome from a provider and resolved window.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::engine::SwitchedModel;
    /// use zhive_core::provider::ScriptedModel;
    /// let s = SwitchedModel::new(ScriptedModel::new("p", "m", vec![]).into_dyn(), Some(200_000));
    /// assert_eq!(s.context_window, Some(200_000));
    /// ```
    #[must_use]
    pub fn new(provider: DynLanguageModel, context_window: Option<u64>) -> Self {
        Self {
            provider,
            context_window,
        }
    }
}

/// Failure listing or switching the active provider's models.
///
/// A list failure carries the upstream reason as text (the endpoint was
/// unreachable or its body did not parse); a switch failure additionally
/// names the offending model id.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ModelCatalogError {
    /// The provider's `/models` endpoint was unreachable or unparseable.
    #[error("listing models failed: {0}")]
    List(String),

    /// Building a provider for the requested model id failed.
    #[error("switching to model {model_id:?} failed: {reason}")]
    Switch {
        /// The model id that could not be built.
        model_id: String,
        /// The upstream reason, rendered as text.
        reason: String,
    },

    /// The requested model id is not one the provider advertises.
    #[error("unknown model id: {0:?}")]
    UnknownModel(String),
}

/// Lists and hot-swaps the models a provider exposes.
///
/// Implemented by the host and injected onto the [`Engine`] handle. The engine
/// calls [`list`](ModelCatalog::list) for the `models/list` RPC and
/// [`switch`](ModelCatalog::switch) for `engine/set_model`. Implementations
/// must be cheap to share (`Send + Sync`) and resilient — a transport or parse
/// failure is returned as a [`ModelCatalogError`], never a panic.
///
/// [`Engine`]: super::Engine
#[async_trait]
pub trait ModelCatalog: Send + Sync + std::fmt::Debug {
    /// Returns the models the active provider advertises, in endpoint order.
    ///
    /// # Errors
    ///
    /// Returns [`ModelCatalogError::List`] when the endpoint cannot be reached
    /// or its response cannot be parsed.
    async fn list(&self) -> Result<Vec<ModelDescriptor>, ModelCatalogError>;

    /// Builds a provider for `model_id`, resolving its context window.
    ///
    /// `context_window_hint` is a value the caller already knows from a prior
    /// listing; an implementation may instead prefer a host-configured
    /// override. The returned [`SwitchedModel`] carries the effective window.
    ///
    /// # Errors
    ///
    /// Returns [`ModelCatalogError::Switch`] when the provider cannot be built,
    /// or [`ModelCatalogError::UnknownModel`] when `model_id` is not advertised.
    fn switch(
        &self,
        model_id: &str,
        context_window_hint: Option<u64>,
    ) -> Result<SwitchedModel, ModelCatalogError>;
}

// Rust guideline compliant 2026-02-21

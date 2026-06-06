//! Provider `/models` discovery and the host model catalogue.
//!
//! Implements [`zhive_core::engine::ModelCatalog`] over the active provider's
//! HTTP config so the engine can serve `models/list` and `engine/set_model`.
//! [`build_catalog`] constructs one for the kinds that expose an `OpenAI`- or
//! `Anthropic`-style `/models` endpoint (anthropic, openai, xai, mistral); other
//! kinds get no catalogue and the model-management RPCs report unavailable.
//!
//! The endpoint shape is parsed defensively: every field beyond `id` is
//! optional, so a stock endpoint returning only ids still yields usable rows,
//! while a richer one (per-model `max_input_tokens` and `capabilities.effort`)
//! flows through to context-window sizing and reasoning-depth control.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use zhive_core::engine::{ModelCatalog, ModelCatalogError, SwitchedModel};
use zhive_proto::domain::ThinkingEffort;
use zhive_proto::rpc::ModelDescriptor;

use crate::config::{Config, ProviderEntry};

/// `anthropic-version` header sent with Anthropic `/models` requests.
///
/// Anthropic requires this header on every request; `2023-06-01` is the stable
/// version the SDK defaults to. The value is inert for the models listing but
/// the API rejects requests that omit it.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Timeout for the boot-time and picker `/models` fetches.
///
/// Generous for a local proxy yet short enough that an unreachable remote does
/// not stall startup — a timed-out fetch degrades to "no models" rather than
/// hanging the TUI.
const MODELS_HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// Authentication header convention for a provider's `/models` endpoint.
#[derive(Debug, Clone, Copy)]
enum AuthStyle {
    /// `x-api-key` + `anthropic-version` (Anthropic Messages API).
    Anthropic,
    /// `Authorization: Bearer <key>` (OpenAI-compatible APIs).
    Bearer,
}

impl AuthStyle {
    /// Returns the auth convention for `kind`, or `None` for kinds without a
    /// standard listable `/models` endpoint.
    fn for_kind(kind: &str) -> Option<Self> {
        match kind {
            "anthropic" => Some(Self::Anthropic),
            "openai" | "xai" | "mistral" => Some(Self::Bearer),
            _ => None,
        }
    }

    /// Applies the auth headers to `req`, sending the key only when present.
    fn apply(self, req: reqwest::RequestBuilder, key: Option<&str>) -> reqwest::RequestBuilder {
        match self {
            Self::Anthropic => {
                let req = req.header("anthropic-version", ANTHROPIC_VERSION);
                match key {
                    Some(k) => req.header("x-api-key", k),
                    None => req,
                }
            }
            Self::Bearer => match key {
                Some(k) => req.bearer_auth(k),
                None => req,
            },
        }
    }
}

/// Default base URL for kinds whose `/models` endpoint is well-known.
fn default_base_url(kind: &str) -> Option<String> {
    let url = match kind {
        "anthropic" => "https://api.anthropic.com/v1",
        "openai" => "https://api.openai.com/v1",
        "xai" => "https://api.x.ai/v1",
        "mistral" => "https://api.mistral.ai/v1",
        _ => return None,
    };
    Some(url.to_owned())
}

/// Resolves the API key from the inline value or the named/default env var.
///
/// Returns `None` (rather than an error) when no key is configured: a local
/// proxy may accept unauthenticated `/models` requests, so the catalogue tries
/// without a key instead of refusing to list.
fn resolve_key(entry: &ProviderEntry) -> Option<String> {
    if let Some(key) = &entry.api_key
        && !key.is_empty()
    {
        return Some(key.clone());
    }
    let default_env = match entry.kind.as_str() {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "xai" => "XAI_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        _ => return None,
    };
    let env_name = entry.api_key_env.as_deref().unwrap_or(default_env);
    std::env::var(env_name).ok().filter(|s| !s.is_empty())
}

// ─── Wire shapes (defensive) ──────────────────────────────────────────────────

/// Top-level `/models` response body (`OpenAI` + `Anthropic` share `data`).
#[derive(Debug, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<RawModel>,
}

/// One `/models` row; every field beyond `id` is optional.
#[derive(Debug, Deserialize)]
struct RawModel {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    max_input_tokens: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u64>,
    #[serde(default)]
    capabilities: Option<RawCapabilities>,
}

/// The `capabilities` sub-object (only the fields zhive consumes).
#[derive(Debug, Default, Deserialize)]
struct RawCapabilities {
    #[serde(default)]
    effort: Option<RawEffort>,
    #[serde(default)]
    thinking: Option<RawSupported>,
}

/// The `capabilities.effort` matrix of per-level support flags.
#[derive(Debug, Default, Deserialize)]
struct RawEffort {
    #[serde(default)]
    supported: bool,
    #[serde(default)]
    low: Option<RawSupported>,
    #[serde(default)]
    medium: Option<RawSupported>,
    #[serde(default)]
    high: Option<RawSupported>,
    #[serde(default)]
    xhigh: Option<RawSupported>,
    #[serde(default)]
    max: Option<RawSupported>,
}

/// A `{ "supported": bool }` leaf, used throughout the capabilities tree.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
struct RawSupported {
    #[serde(default)]
    supported: bool,
}

impl RawSupported {
    /// Whether this capability leaf is present and supported.
    fn is(opt: Option<Self>) -> bool {
        opt.is_some_and(|s| s.supported)
    }
}

/// Maps a raw `/models` row to the neutral [`ModelDescriptor`] wire type.
fn to_descriptor(raw: RawModel) -> ModelDescriptor {
    // Build the Off-first reasoning-depth cycle from the per-level flags. Off is
    // always available; only levels the endpoint reports as supported follow, so
    // the cycle the UI offers is exactly what the model accepts.
    let mut efforts = vec![ThinkingEffort::Off];
    let mut thinking_supported = false;
    if let Some(caps) = &raw.capabilities {
        if let Some(effort) = &caps.effort
            && effort.supported
        {
            if RawSupported::is(effort.low) {
                efforts.push(ThinkingEffort::Low);
            }
            if RawSupported::is(effort.medium) {
                efforts.push(ThinkingEffort::Medium);
            }
            if RawSupported::is(effort.high) {
                efforts.push(ThinkingEffort::High);
            }
            if RawSupported::is(effort.xhigh) {
                efforts.push(ThinkingEffort::Xhigh);
            }
            if RawSupported::is(effort.max) {
                efforts.push(ThinkingEffort::Max);
            }
        }
        thinking_supported = RawSupported::is(caps.thinking);
    }
    ModelDescriptor::new(raw.id)
        .with_display_name(raw.display_name)
        .with_context_window(raw.max_input_tokens)
        .with_max_output_tokens(raw.max_tokens)
        .with_supported_efforts(efforts)
        .with_thinking_supported(thinking_supported)
}

// ─── Catalogue ────────────────────────────────────────────────────────────────

/// Host catalogue backed by a provider's HTTP `/models` endpoint.
pub(crate) struct HttpModelCatalog {
    /// Active provider entry, cloned so `switch` can rebuild for a new model.
    entry: ProviderEntry,
    /// Fully-resolved `{base}/models` URL.
    models_url: String,
    /// Auth header convention for this provider kind.
    auth: AuthStyle,
    /// Resolved API key, if any (absent for unauthenticated local proxies).
    api_key: Option<String>,
    /// Shared HTTP client with a bounded timeout.
    http: reqwest::Client,
}

impl std::fmt::Debug for HttpModelCatalog {
    /// Renders the catalogue without leaking the API key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpModelCatalog")
            .field("models_url", &self.models_url)
            .field("kind", &self.entry.kind)
            .field("auth", &self.auth)
            .field("has_api_key", &self.api_key.is_some())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl ModelCatalog for HttpModelCatalog {
    async fn list(&self) -> Result<Vec<ModelDescriptor>, ModelCatalogError> {
        let req = self.http.get(&self.models_url);
        let req = self.auth.apply(req, self.api_key.as_deref());
        let resp = req
            .send()
            .await
            .map_err(|e| ModelCatalogError::List(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ModelCatalogError::List(format!(
                "GET {} returned HTTP {status}",
                self.models_url
            )));
        }
        let body: ModelsResponse = resp
            .json()
            .await
            .map_err(|e| ModelCatalogError::List(e.to_string()))?;
        Ok(body.data.into_iter().map(to_descriptor).collect())
    }

    fn switch(
        &self,
        model_id: &str,
        context_window_hint: Option<u64>,
    ) -> Result<SwitchedModel, ModelCatalogError> {
        let mut entry = self.entry.clone();
        model_id.clone_into(&mut entry.model);
        let provider = crate::provider::builtin_registry()
            .build(&entry.kind, &entry)
            .map_err(|e| ModelCatalogError::Switch {
                model_id: model_id.to_owned(),
                reason: e.to_string(),
            })?;
        // Resolution priority: the host's manual override, then the caller's
        // hint from a prior listing.
        let context_window = self.entry.context_window.or(context_window_hint);
        Ok(SwitchedModel::new(provider, context_window))
    }
}

/// Builds a model catalogue for the active provider, if its kind supports one.
///
/// Returns `None` for kinds without a standard listable `/models` endpoint
/// (cloud `SigV4` backends, `scripted`, etc.) or when the HTTP client cannot be
/// built; the engine then reports model management as unavailable.
#[must_use]
pub(crate) fn build_catalog(cfg: &Config) -> Option<Arc<dyn ModelCatalog>> {
    let entry = cfg.provider.providers.get(&cfg.provider.default)?;
    let auth = AuthStyle::for_kind(&entry.kind)?;
    let base = entry
        .base_url
        .clone()
        .or_else(|| default_base_url(&entry.kind))?;
    let models_url = format!("{}/models", base.trim_end_matches('/'));
    let http = reqwest::Client::builder()
        .timeout(MODELS_HTTP_TIMEOUT)
        .build()
        .ok()?;
    Some(Arc::new(HttpModelCatalog {
        api_key: resolve_key(entry),
        entry: entry.clone(),
        models_url,
        auth,
        http,
    }))
}

/// Resolves the active model's context window to seed the compaction budget.
///
/// Prefers the config override (no network), otherwise asks the catalogue for
/// the active model's `max_input_tokens`. Best-effort: any fetch failure yields
/// `None`, leaving the engine on its conservative default budget.
pub(crate) async fn resolve_initial_context_window(
    cfg: &Config,
    catalog: Option<&Arc<dyn ModelCatalog>>,
) -> Option<u64> {
    if let Some(window) = cfg.active_context_window() {
        return Some(window);
    }
    let catalog = catalog?;
    let active = cfg.active_model();
    let models = catalog.list().await.ok()?;
    models
        .into_iter()
        .find(|m| m.id == active)
        .and_then(|m| m.context_window)
}

/// Boot-time `/models` snapshot for the active model: window plus depth cycle.
///
/// Lets the TUI seed both the compaction budget and the reasoning-depth cycle at
/// launch from a single fetch, so Ctrl+T reflects the model's live capabilities
/// without first opening the `/model` picker.
#[derive(Debug, Default, Clone)]
pub(crate) struct ActiveModelInfo {
    /// Context window (max input tokens) for the compaction budget, if known.
    pub context_window: Option<u64>,
    /// The model's Off-first reasoning-depth cycle from the live endpoint.
    ///
    /// `None` when no catalogue exists, the fetch failed, or the active model is
    /// absent from the listing — the TUI then falls back to its static table.
    pub supported_efforts: Option<Vec<ThinkingEffort>>,
}

/// Fetches the active model's window and depth cycle in one `/models` call.
///
/// The context window still honors a manual config override (which wins over the
/// endpoint), but the depth cycle always comes from the live listing. Any
/// failure degrades to the manual window (if any) and no cycle, never an error.
pub(crate) async fn resolve_active_model_info(
    cfg: &Config,
    catalog: Option<&Arc<dyn ModelCatalog>>,
) -> ActiveModelInfo {
    let override_window = cfg.active_context_window();
    let Some(catalog) = catalog else {
        return ActiveModelInfo {
            context_window: override_window,
            supported_efforts: None,
        };
    };
    let active = cfg.active_model();
    let Ok(models) = catalog.list().await else {
        return ActiveModelInfo {
            context_window: override_window,
            supported_efforts: None,
        };
    };
    let descriptor = models.into_iter().find(|m| m.id == active);
    let supported_efforts = descriptor.as_ref().map(|d| d.supported_efforts.clone());
    let endpoint_window = descriptor.and_then(|d| d.context_window);
    ActiveModelInfo {
        // The manual override still wins; otherwise use the endpoint's value.
        context_window: override_window.or(endpoint_window),
        supported_efforts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_style_only_for_listable_kinds() {
        assert!(matches!(
            AuthStyle::for_kind("anthropic"),
            Some(AuthStyle::Anthropic)
        ));
        assert!(matches!(
            AuthStyle::for_kind("openai"),
            Some(AuthStyle::Bearer)
        ));
        assert!(AuthStyle::for_kind("scripted").is_none());
        assert!(AuthStyle::for_kind("amazon-bedrock").is_none());
    }

    #[test]
    fn descriptor_maps_rich_anthropic_row() {
        // Mirrors a row from the enriched proxy: 1 M window, effort up to xhigh.
        let raw: RawModel = serde_json::from_value(serde_json::json!({
            "id": "claude-opus-4-8",
            "display_name": "Claude Opus 4.8",
            "max_input_tokens": 1_000_000,
            "max_tokens": 128_000,
            "capabilities": {
                "effort": {
                    "supported": true,
                    "low": { "supported": true },
                    "medium": { "supported": true },
                    "high": { "supported": true },
                    "xhigh": { "supported": true },
                    "max": { "supported": false }
                },
                "thinking": { "supported": true }
            }
        }))
        .unwrap();
        let d = to_descriptor(raw);
        assert_eq!(d.id, "claude-opus-4-8");
        assert_eq!(d.display_name.as_deref(), Some("Claude Opus 4.8"));
        assert_eq!(d.context_window, Some(1_000_000));
        assert_eq!(d.max_output_tokens, Some(128_000));
        assert!(d.thinking_supported);
        assert_eq!(
            d.supported_efforts,
            vec![
                ThinkingEffort::Off,
                ThinkingEffort::Low,
                ThinkingEffort::Medium,
                ThinkingEffort::High,
                ThinkingEffort::Xhigh,
            ],
        );
    }

    #[test]
    fn descriptor_maps_bare_openai_row() {
        // A stock OpenAI row: only id/object/created — no metadata.
        let raw: RawModel = serde_json::from_value(serde_json::json!({
            "id": "gpt-4o",
            "object": "model",
            "created": 1_700_000_000,
            "owned_by": "openai"
        }))
        .unwrap();
        let d = to_descriptor(raw);
        assert_eq!(d.id, "gpt-4o");
        assert!(d.context_window.is_none());
        assert!(!d.thinking_supported);
        // No effort metadata → only Off (no depth control).
        assert_eq!(d.supported_efforts, vec![ThinkingEffort::Off]);
    }

    #[test]
    fn parses_full_response_envelope() {
        // Both the Anthropic (first_id/has_more) and OpenAI (object) envelopes
        // round-trip through the lenient `data`-only shape.
        let body: ModelsResponse = serde_json::from_value(serde_json::json!({
            "data": [{ "id": "a" }, { "id": "b" }],
            "first_id": "a",
            "has_more": false,
            "last_id": "b",
            "object": "list"
        }))
        .unwrap();
        assert_eq!(body.data.len(), 2);
    }

    /// A catalogue returning a fixed listing, for offline boot-info tests.
    #[derive(Debug)]
    struct StubCatalog(Vec<ModelDescriptor>);

    #[async_trait]
    impl ModelCatalog for StubCatalog {
        async fn list(&self) -> Result<Vec<ModelDescriptor>, ModelCatalogError> {
            Ok(self.0.clone())
        }
        fn switch(
            &self,
            model_id: &str,
            _context_window_hint: Option<u64>,
        ) -> Result<SwitchedModel, ModelCatalogError> {
            Err(ModelCatalogError::Switch {
                model_id: model_id.to_owned(),
                reason: "stub".to_owned(),
            })
        }
    }

    #[tokio::test]
    async fn active_model_info_carries_window_and_efforts() {
        let catalog: Arc<dyn ModelCatalog> = Arc::new(StubCatalog(vec![
            ModelDescriptor::new("claude-opus-4-8".to_owned())
                .with_context_window(Some(1_000_000))
                .with_supported_efforts(vec![
                    ThinkingEffort::Off,
                    ThinkingEffort::Low,
                    ThinkingEffort::High,
                ]),
        ]));
        let mut cfg = Config::default();
        cfg.set_active_selection("claude-opus-4-8".to_owned(), None);

        let info = resolve_active_model_info(&cfg, Some(&catalog)).await;
        assert_eq!(info.context_window, Some(1_000_000));
        assert_eq!(
            info.supported_efforts,
            Some(vec![
                ThinkingEffort::Off,
                ThinkingEffort::Low,
                ThinkingEffort::High,
            ]),
        );
    }

    #[tokio::test]
    async fn active_model_info_override_window_wins_over_endpoint() {
        let catalog: Arc<dyn ModelCatalog> = Arc::new(StubCatalog(vec![
            ModelDescriptor::new("claude-opus-4-8".to_owned())
                .with_context_window(Some(1_000_000))
                .with_supported_efforts(vec![ThinkingEffort::Off]),
        ]));
        let mut cfg = Config::default();
        cfg.set_active_selection("claude-opus-4-8".to_owned(), None);
        if let Some(entry) = cfg.provider.providers.get_mut("anthropic") {
            entry.context_window = Some(123_456);
        }

        let info = resolve_active_model_info(&cfg, Some(&catalog)).await;
        // Manual override wins for the window; the cycle still comes live.
        assert_eq!(info.context_window, Some(123_456));
        assert_eq!(info.supported_efforts, Some(vec![ThinkingEffort::Off]));
    }

    #[tokio::test]
    async fn active_model_info_without_catalog_has_no_efforts() {
        let cfg = Config::default();
        let info = resolve_active_model_info(&cfg, None).await;
        assert!(info.supported_efforts.is_none());
    }

    /// Live end-to-end against a running `/models` proxy. Opt-in: set
    /// `ZHIVE_LIVE_MODELS_BASE_URL` (e.g. `http://127.0.0.1:8765/v1`) and run
    /// with `--run-ignored`. Verifies the real HTTP fetch, parse, and the
    /// switch-time window resolution. Skips silently when the env var is unset.
    #[tokio::test]
    #[ignore = "requires a live /models endpoint; set ZHIVE_LIVE_MODELS_BASE_URL"]
    async fn live_fetch_and_switch() {
        let Ok(base_url) = std::env::var("ZHIVE_LIVE_MODELS_BASE_URL") else {
            return;
        };
        let mut cfg = Config::default();
        cfg.provider.default = "anthropic".to_owned();
        if let Some(entry) = cfg.provider.providers.get_mut("anthropic") {
            entry.base_url = Some(base_url);
            // A key is needed to build a provider on switch; the no-auth proxy
            // ignores it for the listing.
            entry.api_key = Some(
                std::env::var("ZHIVE_LIVE_MODELS_KEY").unwrap_or_else(|_| "live-test".to_owned()),
            );
        }
        let catalog = build_catalog(&cfg).expect("catalogue builds for anthropic kind");
        let models = catalog.list().await.expect("live /models fetch + parse");
        assert!(!models.is_empty(), "live endpoint returned models");
        assert!(
            models.iter().any(|m| m.context_window.is_some()),
            "at least one model reports a context window",
        );
        // Switching to the first model rebuilds a provider and resolves a window.
        let first = &models[0];
        let switched = catalog
            .switch(&first.id, first.context_window)
            .expect("switch rebuilds the provider");
        assert_eq!(switched.context_window, first.context_window);
    }
}

// Rust guideline compliant 2026-02-21

//! Data-driven provider registry: maps `kind` strings to backend factories.
//!
//! [`ProviderRegistry`] holds one `Fn(&ProviderEntry) -> DynLanguageModel`
//! per backend kind. [`builtin_registry`] pre-populates every backend that
//! `llmsdk` exposes. The top-level [`build`] function resolves the active
//! entry from config and dispatches through the registry.
//!
//! Cloud backends that require extra configuration (Azure resource name, AWS
//! region, GCP project, …) are registered unconditionally; if a required
//! field is absent from the [`ProviderEntry`], the factory returns a
//! descriptive [`anyhow::Error`] instead of silently omitting the kind.

use std::collections::HashMap;

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use futures::stream;
use llmsdk::DynLanguageModel;
use llmsdk::LanguageModel;
use llmsdk::language_model::{
    BoxStream, CallOptions, GenerateResult, Message, StreamPart, StreamResult, UserPart,
};

use crate::config::{Config, ProviderEntry};

/// Type alias for the factory function stored in [`ProviderRegistry`].
///
/// Each factory takes a [`ProviderEntry`] and returns a built model or an error.
type ProviderFactory =
    Box<dyn Fn(&ProviderEntry) -> anyhow::Result<DynLanguageModel> + Send + Sync>;

// ─── Registry ────────────────────────────────────────────────────────────────

/// A pluggable map of backend-kind names to model factories.
///
/// # Examples
///
/// ```no_run
/// use zhive_cli::provider::{ProviderRegistry, EchoModel};
/// use zhive_cli::config::ProviderEntry;
///
/// let mut reg = ProviderRegistry::new();
/// reg.register("scripted", |_entry| {
///     Ok(llmsdk::DynLanguageModel::new(EchoModel))
/// });
/// let entry = ProviderEntry { kind: "scripted".into(), ..Default::default() };
/// assert!(reg.build("scripted", &entry).is_ok());
/// ```
pub struct ProviderRegistry {
    factories: HashMap<String, ProviderFactory>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut known: Vec<&str> = self.factories.keys().map(String::as_str).collect();
        known.sort_unstable();
        f.debug_struct("ProviderRegistry")
            .field("kinds", &known)
            .finish()
    }
}

impl ProviderRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Registers a factory under `kind`.
    ///
    /// Replaces any previously registered factory for the same kind.
    pub fn register<F>(&mut self, kind: impl Into<String>, factory: F)
    where
        F: Fn(&ProviderEntry) -> anyhow::Result<DynLanguageModel> + Send + Sync + 'static,
    {
        self.factories.insert(kind.into(), Box::new(factory));
    }

    /// Builds a [`DynLanguageModel`] for `kind` using `entry`.
    ///
    /// # Errors
    ///
    /// Returns an error when the kind is unknown, listing all registered
    /// kinds, or when the factory itself fails (e.g. missing API key).
    pub fn build(&self, kind: &str, entry: &ProviderEntry) -> anyhow::Result<DynLanguageModel> {
        let factory = self.factories.get(kind).ok_or_else(|| {
            let mut known: Vec<&str> = self.factories.keys().map(String::as_str).collect();
            known.sort_unstable();
            anyhow!(
                "unknown provider kind {:?} — known kinds: {}",
                kind,
                known.join(", ")
            )
        })?;
        factory(entry)
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Builtin registry ────────────────────────────────────────────────────────

/// Builds a [`ProviderRegistry`] pre-populated with all supported backends.
///
/// Simple backends (anthropic, openai, xai, mistral, cohere, google) need only
/// `api_key` / `api_key_env` and optionally `base_url`.
///
/// Cloud backends (azure, amazon-bedrock, google-vertex, anthropic-aws) need
/// additional fields documented in [`ProviderEntry`]; if a required field is
/// absent the factory returns a descriptive error.
///
/// # Examples
///
/// ```no_run
/// use zhive_cli::provider::builtin_registry;
/// use zhive_cli::config::ProviderEntry;
///
/// let reg = builtin_registry();
/// let entry = ProviderEntry {
///     kind: "scripted".into(),
///     ..Default::default()
/// };
/// assert!(reg.build("scripted", &entry).is_ok());
/// ```
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "one registration block per backend — splitting into helpers would \
              obscure the per-backend configuration logic without reducing total lines"
)]
pub fn builtin_registry() -> ProviderRegistry {
    let mut reg = ProviderRegistry::new();

    // ── anthropic ────────────────────────────────────────────────────────────
    reg.register("anthropic", |entry| {
        let key = resolve_key(entry, "ANTHROPIC_API_KEY")?;
        let mut builder = llmsdk::anthropic::Anthropic::builder().api_key(key);
        if let Some(url) = &entry.base_url {
            builder = builder.base_url(url.clone());
        }
        let provider = builder.build().context("building Anthropic provider")?;
        Ok(DynLanguageModel::new(provider.messages(&entry.model)))
    });

    // ── openai ───────────────────────────────────────────────────────────────
    reg.register("openai", |entry| {
        let key = resolve_key(entry, "OPENAI_API_KEY")?;
        let mut builder = llmsdk::openai::OpenAi::builder().api_key(key);
        if let Some(url) = &entry.base_url {
            builder = builder.base_url(url.clone());
        }
        let provider = builder.build().context("building OpenAI provider")?;
        Ok(DynLanguageModel::new(provider.chat(&entry.model)))
    });

    // ── xai ──────────────────────────────────────────────────────────────────
    reg.register("xai", |entry| {
        let key = resolve_key(entry, "XAI_API_KEY")?;
        let mut builder = llmsdk::xai::Xai::builder().api_key(key);
        if let Some(url) = &entry.base_url {
            builder = builder.base_url(url.clone());
        }
        let provider = builder.build().context("building xAI provider")?;
        Ok(DynLanguageModel::new(provider.chat(&entry.model)))
    });

    // ── mistral ───────────────────────────────────────────────────────────────
    reg.register("mistral", |entry| {
        let key = resolve_key(entry, "MISTRAL_API_KEY")?;
        let mut builder = llmsdk::mistral::Mistral::builder().api_key(key);
        if let Some(url) = &entry.base_url {
            builder = builder.base_url(url.clone());
        }
        let provider = builder.build().context("building Mistral provider")?;
        Ok(DynLanguageModel::new(provider.chat(&entry.model)))
    });

    // ── cohere ────────────────────────────────────────────────────────────────
    reg.register("cohere", |entry| {
        let key = resolve_key(entry, "COHERE_API_KEY")?;
        let mut builder = llmsdk::cohere::Cohere::builder().api_key(key);
        if let Some(url) = &entry.base_url {
            builder = builder.base_url(url.clone());
        }
        let provider = builder.build().context("building Cohere provider")?;
        Ok(DynLanguageModel::new(provider.chat(&entry.model)))
    });

    // ── google ────────────────────────────────────────────────────────────────
    reg.register("google", |entry| {
        let key = resolve_key(entry, "GOOGLE_GENERATIVE_AI_API_KEY")?;
        let mut builder = llmsdk::google::Google::builder().api_key(key);
        if let Some(url) = &entry.base_url {
            builder = builder.base_url(url.clone());
        }
        let provider = builder.build().context("building Google provider")?;
        Ok(DynLanguageModel::new(provider.language_model(&entry.model)))
    });

    // ── azure ─────────────────────────────────────────────────────────────────
    // Requires `resource_name` (or AZURE_RESOURCE_NAME env var).
    // API key resolved from entry or AZURE_API_KEY.
    reg.register("azure", |entry| {
        let key = resolve_key(entry, "AZURE_API_KEY")?;
        let resource = entry
            .resource_name
            .clone()
            .or_else(|| std::env::var("AZURE_RESOURCE_NAME").ok())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!("azure provider requires `resource_name` in config or $AZURE_RESOURCE_NAME")
            })?;
        let mut builder = llmsdk::azure::AzureOpenAi::builder()
            .api_key(key)
            .resource_name(resource);
        if let Some(ver) = &entry.api_version {
            builder = builder.api_version(ver.clone());
        }
        let provider = builder.build().context("building Azure OpenAI provider")?;
        // `deployment` if set, otherwise fall back to `model` as the deployment id.
        let deployment = entry
            .deployment
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&entry.model);
        Ok(DynLanguageModel::new(provider.chat(deployment)))
    });

    // ── amazon-bedrock ────────────────────────────────────────────────────────
    // Requires `region` (or AWS_REGION env var).
    // Credentials resolved from entry fields or standard AWS env vars.
    reg.register("amazon-bedrock", |entry| {
        let region = entry
            .region
            .clone()
            .or_else(|| std::env::var("AWS_REGION").ok())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!("amazon-bedrock provider requires `region` in config or $AWS_REGION")
            })?;
        let mut builder = llmsdk::amazon_bedrock::AmazonBedrock::builder().region(region);
        // Prefer an explicit bearer token (api_key) over SigV4 credentials.
        if let Some(token) = &entry.api_key {
            builder = builder.api_key(token.clone());
        }
        let provider = builder
            .build()
            .context("building Amazon Bedrock provider")?;
        Ok(DynLanguageModel::new(provider.language_model(&entry.model)))
    });

    // ── anthropic-aws ─────────────────────────────────────────────────────────
    // Requires `region` (or AWS_REGION) and `workspace_id`
    // (or ANTHROPIC_AWS_WORKSPACE_ID).
    reg.register("anthropic-aws", |entry| {
        let region = entry
            .region
            .clone()
            .or_else(|| std::env::var("AWS_REGION").ok())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "anthropic-aws provider requires `region` in config or $AWS_REGION"
                )
            })?;
        let workspace_id = entry
            .workspace_id
            .clone()
            .or_else(|| std::env::var("ANTHROPIC_AWS_WORKSPACE_ID").ok())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "anthropic-aws provider requires `workspace_id` in config or $ANTHROPIC_AWS_WORKSPACE_ID"
                )
            })?;
        let mut builder = llmsdk::anthropic_aws::AnthropicAws::builder()
            .region(region)
            .workspace_id(workspace_id);
        if let Some(key) = &entry.api_key {
            builder = builder.api_key(key.clone());
        }
        let provider = builder.build().context("building Anthropic-on-AWS provider")?;
        Ok(DynLanguageModel::new(provider.language_model(&entry.model)))
    });

    // ── google-vertex ─────────────────────────────────────────────────────────
    // Express mode: set `api_key` / api_key_env → $GOOGLE_VERTEX_API_KEY.
    // OAuth mode: requires `project` (or GOOGLE_VERTEX_PROJECT) and
    // optionally `location` (or GOOGLE_VERTEX_LOCATION; defaults to us-central1).
    // build() is async; we block the calling thread via a new single-thread
    // runtime (provider.rs factories are invoked synchronously from build()).
    reg.register("google-vertex", |entry| {
        let api_key_val = entry
            .api_key
            .clone()
            .or_else(|| {
                let env_name = entry
                    .api_key_env
                    .as_deref()
                    .unwrap_or("GOOGLE_VERTEX_API_KEY");
                std::env::var(env_name).ok()
            })
            .filter(|s| !s.is_empty());

        let mut builder = llmsdk::google_vertex::GoogleVertex::builder();
        if let Some(key) = api_key_val {
            builder = builder.api_key(key);
        } else {
            // OAuth mode: project required, location optional.
            let project = entry
                .project
                .clone()
                .or_else(|| std::env::var("GOOGLE_VERTEX_PROJECT").ok())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow!(
                        "google-vertex OAuth mode requires `project` in config or $GOOGLE_VERTEX_PROJECT; \
                         or set `api_key` / api_key_env for Express mode"
                    )
                })?;
            builder = builder.project(project);
            if let Some(loc) = entry
                .location
                .clone()
                .or_else(|| std::env::var("GOOGLE_VERTEX_LOCATION").ok())
                .filter(|s| !s.is_empty())
            {
                builder = builder.location(loc);
            }
        }

        // GoogleVertex::build() is async; spin up a one-shot runtime.
        let provider = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("building tokio runtime for google-vertex")?
            .block_on(builder.build())
            .context("building Google Vertex provider")?;
        Ok(DynLanguageModel::new(provider.language_model(&entry.model)))
    });

    // ── scripted (offline EchoModel) ──────────────────────────────────────────
    reg.register("scripted", |_entry| Ok(DynLanguageModel::new(EchoModel)));

    reg
}

// ─── Public build entry-point ─────────────────────────────────────────────────

/// Builds the [`DynLanguageModel`] for the active provider in `cfg`.
///
/// Looks up `cfg.provider.default` in the providers map, then dispatches
/// through [`builtin_registry`].
///
/// # Errors
///
/// Returns an error if the active provider name is not found in the map, the
/// kind is unregistered, or the backend factory fails.
pub fn build(cfg: &Config) -> anyhow::Result<DynLanguageModel> {
    let name = &cfg.provider.default;
    let entry = cfg.provider.providers.get(name).ok_or_else(|| {
        let known: Vec<&str> = cfg.provider.providers.keys().map(String::as_str).collect();
        anyhow!(
            "active provider {:?} not found in config; defined entries: {}",
            name,
            if known.is_empty() {
                "(none)".to_owned()
            } else {
                known.join(", ")
            }
        )
    })?;
    builtin_registry().build(&entry.kind, entry)
}

// ─── resolve_key helper ───────────────────────────────────────────────────────

/// Resolves an API key from the inline value or the named environment variable.
///
/// `default_env` is the fallback env-var name when no `api_key_env` is set.
fn resolve_key(entry: &ProviderEntry, default_env: &str) -> anyhow::Result<String> {
    if let Some(key) = &entry.api_key {
        return Ok(key.clone());
    }
    let env_name = entry.api_key_env.as_deref().unwrap_or(default_env);
    std::env::var(env_name).map_err(|source| {
        anyhow!(
            "missing API key: set ${env_name} or set `api_key` / `api_key_env` in config.toml ({source})"
        )
    })
}

// ─── EchoModel ────────────────────────────────────────────────────────────────

/// An offline model that streams the user's last message back.
///
/// Used by the `scripted` kind. Requires no network or API key, so the TUI
/// runs and renders end-to-end offline.
#[derive(Debug, Clone, Copy)]
pub struct EchoModel;

/// Extracts the most recent user-message text from a prompt.
fn last_user_text(prompt: &[Message]) -> String {
    for message in prompt.iter().rev() {
        if let Message::User { content, .. } = message {
            return content
                .iter()
                .filter_map(|part| {
                    if let UserPart::Text(text) = part {
                        Some(text.text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("");
        }
    }
    String::new()
}

#[async_trait]
impl LanguageModel for EchoModel {
    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the LanguageModel trait fixes this return type to &str"
    )]
    fn provider(&self) -> &str {
        "scripted"
    }

    #[expect(
        clippy::unnecessary_literal_bound,
        reason = "the LanguageModel trait fixes this return type to &str"
    )]
    fn model_id(&self) -> &str {
        "echo"
    }

    async fn do_generate(&self, _options: CallOptions) -> llmsdk::error::Result<GenerateResult> {
        use llmsdk::language_model::{FinishReason, FinishReasonKind, Usage};
        Ok(GenerateResult {
            content: vec![],
            finish_reason: FinishReason::new(FinishReasonKind::Stop),
            usage: Usage::default(),
            provider_metadata: None,
            request: None,
            response: None,
            warnings: vec![],
        })
    }

    async fn do_stream(&self, options: CallOptions) -> llmsdk::error::Result<StreamResult> {
        let said = last_user_text(&options.prompt);
        let reply = format!(
            "**echo** · offline demo\n\n> {}\n\nThis is the `scripted` provider — no model was called. \
Set a real `provider` and API key in `config.toml` to chat for real.",
            if said.is_empty() { "(empty)" } else { &said }
        );
        let parts = vec![
            StreamPart::TextStart {
                id: "b0".into(),
                provider_metadata: None,
            },
            StreamPart::TextDelta {
                id: "b0".into(),
                delta: reply,
                provider_metadata: None,
            },
            StreamPart::TextEnd {
                id: "b0".into(),
                provider_metadata: None,
            },
        ];
        let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::iter(
            parts.into_iter().map(Ok::<_, llmsdk::ProviderError>),
        ));
        Ok(StreamResult {
            stream: s,
            request: None,
            response: None,
        })
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_provider_builds_without_key() {
        let cfg = Config::default();
        // The default config has "scripted" in the providers map; switch to it.
        let mut cfg = cfg;
        cfg.provider.default = "scripted".to_owned();
        assert!(build(&cfg).is_ok());
    }

    #[test]
    fn anthropic_without_key_errors() {
        let mut cfg = Config::default();
        cfg.provider.default = "anthropic".to_owned();
        let entry = cfg.provider.providers.get_mut("anthropic").unwrap();
        entry.api_key = None;
        entry.api_key_env = Some("ZHIVE_TEST_DEFINITELY_ABSENT_KEY".to_owned());
        assert!(build(&cfg).is_err());
    }

    #[test]
    fn unknown_provider_name_errors() {
        let mut cfg = Config::default();
        cfg.provider.default = "does-not-exist".to_owned();
        let err = build(&cfg).unwrap_err();
        assert!(
            err.to_string().contains("not found in config"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn unknown_kind_errors_with_list() {
        let reg = builtin_registry();
        let entry = ProviderEntry {
            kind: "nonexistent-backend".to_owned(),
            ..Default::default()
        };
        let err = reg.build("nonexistent-backend", &entry).unwrap_err();
        assert!(err.to_string().contains("known kinds"), "unexpected: {err}");
    }

    #[test]
    fn registry_contains_all_builtin_kinds() {
        let reg = builtin_registry();
        for kind in &[
            "anthropic",
            "openai",
            "xai",
            "mistral",
            "cohere",
            "google",
            "azure",
            "amazon-bedrock",
            "anthropic-aws",
            "google-vertex",
            "scripted",
        ] {
            assert!(reg.factories.contains_key(*kind), "missing kind: {kind}");
        }
    }

    #[test]
    fn azure_without_resource_name_errors() {
        let reg = builtin_registry();
        // Use a placeholder key so key resolution does not fail first.
        let entry = ProviderEntry {
            kind: "azure".to_owned(),
            model: "gpt-4o-mini".to_owned(),
            api_key: Some("placeholder".to_owned()),
            resource_name: None,
            ..Default::default()
        };
        // Only run this test when AZURE_RESOURCE_NAME is not set in the env.
        if std::env::var("AZURE_RESOURCE_NAME").is_err() {
            let err = reg.build("azure", &entry).unwrap_err();
            assert!(
                err.to_string().contains("resource_name"),
                "unexpected error: {err}"
            );
        }
    }

    #[test]
    fn bedrock_without_region_errors() {
        let reg = builtin_registry();
        let entry = ProviderEntry {
            kind: "amazon-bedrock".to_owned(),
            model: "anthropic.claude-3-5-sonnet-20241022-v2:0".to_owned(),
            region: None,
            ..Default::default()
        };
        if std::env::var("AWS_REGION").is_err() {
            let err = reg.build("amazon-bedrock", &entry).unwrap_err();
            assert!(
                err.to_string().contains("region"),
                "unexpected error: {err}"
            );
        }
    }

    #[tokio::test]
    async fn echo_model_streams_user_text() {
        use futures::StreamExt;
        use llmsdk::language_model::{Message, TextPart, UserPart};

        let options = CallOptions {
            prompt: vec![Message::User {
                content: vec![UserPart::Text(TextPart {
                    text: "ping".to_owned(),
                    provider_options: None,
                })],
                provider_options: None,
            }],
            ..Default::default()
        };
        let mut result = EchoModel.do_stream(options).await.unwrap();
        let mut text = String::new();
        while let Some(Ok(part)) = result.stream.next().await {
            if let StreamPart::TextDelta { delta, .. } = part {
                text.push_str(&delta);
            }
        }
        assert!(text.contains("ping"));
    }
}

// Rust guideline compliant 2026-02-21

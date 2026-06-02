//! Builds a [`DynLanguageModel`] for the engine from the resolved [`Config`].
//!
//! Anthropic and `OpenAI` are constructed via the `llmsdk` builders with the API
//! key resolved from config or the environment. The `scripted` provider is an
//! in-process [`EchoModel`] that needs no network or key, so `zhive tui` runs
//! and renders end-to-end offline.

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use futures::stream;
use llmsdk::DynLanguageModel;
use llmsdk::LanguageModel;
use llmsdk::language_model::{
    BoxStream, CallOptions, GenerateResult, Message, StreamPart, StreamResult, UserPart,
};

use crate::config::{Config, ProviderEntry, ProviderKind};

/// Builds the language model selected by `cfg`.
///
/// # Errors
///
/// Returns an error if a real provider's API key is missing or its client
/// fails to construct.
pub fn build(cfg: &Config) -> anyhow::Result<DynLanguageModel> {
    match cfg.provider.default {
        ProviderKind::Anthropic => build_anthropic(&cfg.provider.anthropic),
        ProviderKind::Openai => build_openai(&cfg.provider.openai),
        ProviderKind::Scripted => Ok(DynLanguageModel::new(EchoModel)),
    }
}

/// Builds an Anthropic Messages model.
fn build_anthropic(entry: &ProviderEntry) -> anyhow::Result<DynLanguageModel> {
    let key = resolve_key(entry, "ANTHROPIC_API_KEY")?;
    let mut builder = llmsdk::anthropic::Anthropic::builder().api_key(key);
    if let Some(url) = &entry.base_url {
        builder = builder.base_url(url.clone());
    }
    let provider = builder.build().context("building Anthropic provider")?;
    Ok(DynLanguageModel::new(provider.messages(&entry.model)))
}

/// Builds an `OpenAI` Chat Completions model.
fn build_openai(entry: &ProviderEntry) -> anyhow::Result<DynLanguageModel> {
    let key = resolve_key(entry, "OPENAI_API_KEY")?;
    let mut builder = llmsdk::openai::OpenAi::builder().api_key(key);
    if let Some(url) = &entry.base_url {
        builder = builder.base_url(url.clone());
    }
    let provider = builder.build().context("building OpenAI provider")?;
    Ok(DynLanguageModel::new(provider.chat(&entry.model)))
}

/// Resolves an API key from the inline value or the named environment variable.
fn resolve_key(entry: &ProviderEntry, default_env: &str) -> anyhow::Result<String> {
    if let Some(key) = &entry.api_key {
        return Ok(key.clone());
    }
    let env_name = entry.api_key_env.as_deref().unwrap_or(default_env);
    std::env::var(env_name).map_err(|source| {
        anyhow!(
            "missing API key: set ${env_name} or provider.<provider>.api_key in config.toml ({source})"
        )
    })
}

/// An offline model that streams the user's last message back, for demos/tests.
#[derive(Debug, Clone, Copy)]
struct EchoModel;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_provider_builds_without_key() {
        let mut cfg = Config::default();
        cfg.provider.default = ProviderKind::Scripted;
        assert!(build(&cfg).is_ok());
    }

    #[test]
    fn anthropic_without_key_errors() {
        // Point at an env var that is guaranteed absent so no key resolves.
        let mut cfg = Config::default();
        cfg.provider.default = ProviderKind::Anthropic;
        cfg.provider.anthropic.api_key = None;
        cfg.provider.anthropic.api_key_env = Some("ZHIVE_TEST_DEFINITELY_ABSENT_KEY".to_owned());
        assert!(build(&cfg).is_err());
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

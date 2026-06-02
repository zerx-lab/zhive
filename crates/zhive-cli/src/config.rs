//! Layered TOML configuration for the `zhive` binary.
//!
//! The user requested config-file-driven provider/model selection, so this is
//! the single place process concerns live: which provider to dial, the model
//! and credentials, and the TUI palette. The TUI itself never reads this — the
//! `tui` command distills it into a `zhive_tui::TuiConfig` (D-002).
//!
//! Resolution order for the config path: an explicit `--config`, then
//! `$ZHIVE_CONFIG`, then `$XDG_CONFIG_HOME/zhive/config.toml`, then
//! `~/.config/zhive/config.toml`. A missing file yields [`Config::default`].

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Top-level configuration document.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Provider selection and per-provider settings.
    pub provider: ProviderSection,
    /// TUI presentation preferences.
    pub ui: UiSection,
}

/// Which built-in provider to use, by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// Anthropic Messages API (the default).
    #[default]
    Anthropic,
    /// `OpenAI` Chat Completions API.
    Openai,
    /// Offline echo model — no network or API key, for trying the UI.
    Scripted,
}

/// The `[provider]` section: a default plus per-provider entries.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderSection {
    /// Which provider the `tui` / `serve` commands use by default.
    pub default: ProviderKind,
    /// Anthropic settings.
    pub anthropic: ProviderEntry,
    /// `OpenAI` settings.
    pub openai: ProviderEntry,
}

impl Default for ProviderSection {
    fn default() -> Self {
        Self {
            default: ProviderKind::Anthropic,
            anthropic: ProviderEntry {
                model: "claude-sonnet-4-6".to_owned(),
                api_key: None,
                api_key_env: Some("ANTHROPIC_API_KEY".to_owned()),
                base_url: None,
            },
            openai: ProviderEntry {
                model: "gpt-4o".to_owned(),
                api_key: None,
                api_key_env: Some("OPENAI_API_KEY".to_owned()),
                base_url: None,
            },
        }
    }
}

/// Settings for one provider.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderEntry {
    /// Model id passed to the provider (e.g. `claude-sonnet-4-6`, `gpt-4o`).
    pub model: String,
    /// Inline API key (discouraged; prefer `api_key_env`).
    pub api_key: Option<String>,
    /// Name of the environment variable holding the API key.
    pub api_key_env: Option<String>,
    /// Optional base-URL override (proxies, gateways, compatible servers).
    pub base_url: Option<String>,
}

/// The `[ui]` section: palette selection passed through to the TUI.
///
/// Stored as strings so this crate's config layer does not depend on
/// `zhive-tui` (which only the `tui` feature pulls in); the `tui` command
/// parses these into `zhive_tui` enums, ignoring unknown values.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiSection {
    /// Base theme: `dark`, `light`, or `mono`.
    pub theme: String,
    /// Accent: `cyan`, `amber`, `lime`, or `magenta`.
    pub accent: String,
    /// Panel density: `lean`, `default`, or `airy`.
    pub density: String,
}

impl Default for UiSection {
    fn default() -> Self {
        Self {
            theme: "dark".to_owned(),
            accent: "cyan".to_owned(),
            density: "default".to_owned(),
        }
    }
}

impl Config {
    /// Loads config from `explicit` or the standard search path.
    ///
    /// Returns the parsed config and the path it came from (`None` when no file
    /// existed and defaults were used).
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load(explicit: Option<&Path>) -> anyhow::Result<(Self, Option<PathBuf>)> {
        let path = match explicit {
            Some(p) => Some(p.to_path_buf()),
            None => default_config_path(),
        };
        let Some(path) = path else {
            return Ok((Self::default(), None));
        };
        if !path.exists() {
            return Ok((Self::default(), None));
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let config: Self = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        Ok((config, Some(path)))
    }

    /// The active provider's model id, for display in the top bar.
    #[must_use]
    pub fn active_model(&self) -> &str {
        match self.provider.default {
            ProviderKind::Anthropic => &self.provider.anthropic.model,
            ProviderKind::Openai => &self.provider.openai.model,
            ProviderKind::Scripted => "demo",
        }
    }

    /// The active provider's lowercase name, for display in the top bar.
    #[must_use]
    pub fn active_provider_label(&self) -> &'static str {
        match self.provider.default {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Openai => "openai",
            ProviderKind::Scripted => "scripted",
        }
    }
}

/// Computes the default config path from the XDG / HOME environment.
#[must_use]
pub fn default_config_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("ZHIVE_CONFIG") {
        return Some(PathBuf::from(explicit));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("zhive").join("config.toml"));
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("zhive")
            .join("config.toml"),
    )
}

/// A commented sample config emitted by `zhive config init`.
pub const SAMPLE_CONFIG: &str = "\
# zhive configuration.
#
# Provider/model selection and the TUI palette. The active provider's API key
# is read from the environment variable named by `api_key_env` (or set inline
# with `api_key`, which is discouraged).

[provider]
# anthropic | openai | scripted (scripted is an offline echo model)
default = \"anthropic\"

[provider.anthropic]
model = \"claude-sonnet-4-6\"
api_key_env = \"ANTHROPIC_API_KEY\"
# base_url = \"https://api.anthropic.com/v1\"

[provider.openai]
model = \"gpt-4o\"
api_key_env = \"OPENAI_API_KEY\"
# base_url = \"https://api.openai.com/v1\"

[ui]
theme = \"dark\"      # dark | light | mono
accent = \"cyan\"     # cyan | amber | lime | magenta
density = \"default\" # lean | default | airy
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_anthropic_sonnet() {
        let cfg = Config::default();
        assert_eq!(cfg.provider.default, ProviderKind::Anthropic);
        assert_eq!(cfg.active_model(), "claude-sonnet-4-6");
        assert_eq!(cfg.active_provider_label(), "anthropic");
    }

    #[test]
    fn parses_a_full_document() {
        let cfg: Config = toml::from_str(SAMPLE_CONFIG).expect("sample parses");
        assert_eq!(cfg.provider.anthropic.model, "claude-sonnet-4-6");
        assert_eq!(cfg.ui.theme, "dark");
        assert_eq!(cfg.ui.accent, "cyan");
    }

    #[test]
    fn parses_provider_override() {
        let cfg: Config = toml::from_str(
            "[provider]\ndefault = \"openai\"\n[provider.openai]\nmodel = \"gpt-4o-mini\"\n",
        )
        .expect("parses");
        assert_eq!(cfg.provider.default, ProviderKind::Openai);
        assert_eq!(cfg.active_model(), "gpt-4o-mini");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<Config>("[provider]\nbogus = 1\n");
        assert!(err.is_err(), "deny_unknown_fields should reject typos");
    }
}

// Rust guideline compliant 2026-02-21

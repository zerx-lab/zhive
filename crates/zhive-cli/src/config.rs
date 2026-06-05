//! Layered TOML configuration for the `zhive` binary.
//!
//! Provider/model selection is now **data-driven**: any backend kind
//! recognised by [`crate::provider::ProviderRegistry`] can appear under
//! `[provider.<name>]`, and `[provider].default` selects which one is active.
//! This removes the closed `ProviderKind` enum and fixed `anthropic` /
//! `openai` fields — new backends require only a `kind = "…"` entry.
//!
//! # Resolution order for the config path
//!
//! An explicit `--config`, then `$ZHIVE_CONFIG`, then
//! `$XDG_CONFIG_HOME/zhive/config.toml`, then `~/.config/zhive/config.toml`.
//! A missing file yields [`Config::default`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Top-level configuration document.
///
/// # Examples
///
/// ```
/// # use zhive_cli::config::Config;
/// let cfg = Config::default();
/// assert_eq!(cfg.provider.default, "anthropic");
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Provider selection and per-provider settings.
    pub provider: ProviderSection,
    /// TUI presentation preferences.
    pub ui: UiSection,
    /// MCP servers whose tools are registered with the engine.
    pub mcp: McpSection,
    /// On-disk Agent-Skills discovery settings.
    pub skills: SkillsSection,
    /// Per-turn engine limits (tool-call iteration cap).
    pub engine: EngineSection,
}

/// The `[provider]` table: a `default` name plus an open map of named entries.
///
/// # `#[serde(flatten)]` and `deny_unknown_fields`
///
/// `#[serde(flatten)]` is fundamentally incompatible with
/// `deny_unknown_fields` (serde issue #1547): flattening merges the outer and
/// inner key sets before field rejection runs, causing every flattened key to
/// be treated as unknown. `deny_unknown_fields` is therefore omitted here.
/// Individual [`ProviderEntry`] deserialization still uses it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderSection {
    /// Name of the active provider entry (must be a key in `providers`).
    pub default: String,
    /// Named provider entries; keyed by arbitrary user-chosen name.
    #[serde(flatten)]
    pub providers: BTreeMap<String, ProviderEntry>,
}

impl Default for ProviderSection {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(
            "anthropic".to_owned(),
            ProviderEntry {
                kind: "anthropic".to_owned(),
                model: "claude-sonnet-4-6".to_owned(),
                api_key: None,
                api_key_env: Some("ANTHROPIC_API_KEY".to_owned()),
                base_url: None,
                region: None,
                project: None,
                location: None,
                resource_name: None,
                api_version: None,
                deployment: None,
                workspace_id: None,
            },
        );
        providers.insert(
            "openai".to_owned(),
            ProviderEntry {
                kind: "openai".to_owned(),
                model: "gpt-4o".to_owned(),
                api_key: None,
                api_key_env: Some("OPENAI_API_KEY".to_owned()),
                base_url: None,
                region: None,
                project: None,
                location: None,
                resource_name: None,
                api_version: None,
                deployment: None,
                workspace_id: None,
            },
        );
        providers.insert(
            "scripted".to_owned(),
            ProviderEntry {
                kind: "scripted".to_owned(),
                model: String::new(),
                api_key: None,
                api_key_env: None,
                base_url: None,
                region: None,
                project: None,
                location: None,
                resource_name: None,
                api_version: None,
                deployment: None,
                workspace_id: None,
            },
        );
        Self {
            default: "anthropic".to_owned(),
            providers,
        }
    }
}

/// Settings for one named provider entry.
///
/// `kind` selects the backend factory in [`crate::provider::ProviderRegistry`].
/// The remaining fields are forwarded to that factory as needed.
///
/// # Examples
///
/// ```
/// # use zhive_cli::config::ProviderEntry;
/// let entry: ProviderEntry = toml::from_str(
///     r#"kind = "openai"
///        model = "gpt-4o"
///        api_key_env = "OPENAI_API_KEY"
///     "#
/// ).unwrap();
/// assert_eq!(entry.kind, "openai");
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderEntry {
    /// Backend kind: `anthropic`, `openai`, `xai`, `mistral`, `azure`,
    /// `cohere`, `google`, `google-vertex`, `amazon-bedrock`,
    /// `anthropic-aws`, or `scripted`.
    pub kind: String,
    /// Model id passed to the provider (e.g. `claude-sonnet-4-6`, `gpt-4o`).
    pub model: String,
    /// Inline API key (discouraged; prefer `api_key_env`).
    pub api_key: Option<String>,
    /// Name of the environment variable that holds the API key.
    pub api_key_env: Option<String>,
    /// Optional base-URL override (proxies, gateways, compatible servers).
    pub base_url: Option<String>,
    /// AWS/Vertex region (e.g. `us-east-1`, `us-central1`).
    pub region: Option<String>,
    /// GCP project id for `google-vertex` OAuth mode.
    pub project: Option<String>,
    /// GCP Vertex location (e.g. `us-central1`).
    pub location: Option<String>,
    /// Azure resource name (`<resource>.openai.azure.com`).
    pub resource_name: Option<String>,
    /// Azure API version query parameter (default: `"v1"`).
    pub api_version: Option<String>,
    /// Azure deployment id (for legacy deployment-based URLs).
    pub deployment: Option<String>,
    /// Anthropic-on-AWS workspace id (`anthropic-workspace-id` header).
    pub workspace_id: Option<String>,
}

/// The `[ui]` section: palette and layout preferences for the TUI.
///
/// Stored as strings so this crate does not depend on `zhive-tui`; the `tui`
/// command parses these into `zhive_tui` enums, ignoring unknown values.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

/// The `[mcp]` section: MCP servers whose tools the engine should expose.
///
/// Each entry under `[mcp.servers.<name>]` describes one server. The map key
/// is the server name and becomes the `mcp__<name>__<tool>` prefix on every
/// tool it exposes. An empty map (the default) registers no MCP tools.
///
/// # Examples
///
/// ```
/// # use zhive_cli::config::McpSection;
/// let section: McpSection = toml::from_str(
///     r#"
///     [servers.filesystem]
///     transport = "stdio"
///     command = "npx"
///     args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
///     "#,
/// )
/// .unwrap();
/// assert!(section.servers.contains_key("filesystem"));
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpSection {
    /// Named MCP servers, keyed by the server name (the tool prefix).
    pub servers: BTreeMap<String, McpServerDef>,
}

/// One MCP server definition, tagged by its `transport`.
///
/// `transport = "stdio"` spawns a child process; `transport = "http"` connects
/// to a Streamable-HTTP endpoint. Fields not relevant to the chosen transport
/// are rejected by `deny_unknown_fields`.
///
/// # Examples
///
/// ```
/// # use zhive_cli::config::McpServerDef;
/// let def: McpServerDef = toml::from_str(
///     r#"transport = "http"
///        url = "http://localhost:8000/mcp"
///     "#,
/// )
/// .unwrap();
/// assert!(matches!(def, McpServerDef::Http { .. }));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase", deny_unknown_fields)]
pub enum McpServerDef {
    /// Spawn a child process and speak JSON-RPC over its stdio.
    Stdio {
        /// Executable to run (resolved against `PATH`).
        command: String,
        /// Arguments passed to the executable.
        #[serde(default)]
        args: Vec<String>,
        /// Extra environment variables for the child process.
        #[serde(default)]
        env: BTreeMap<String, String>,
        /// Working directory for the child; inherits the parent's when absent.
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Connect to a Streamable-HTTP MCP endpoint.
    Http {
        /// Endpoint URL, e.g. `http://localhost:8000/mcp`.
        url: String,
        /// Custom headers sent with every request.
        #[serde(default)]
        headers: BTreeMap<String, String>,
        /// Inline bearer token (discouraged; prefer `auth_token_env`).
        #[serde(default)]
        auth_token: Option<String>,
        /// Name of an environment variable holding the bearer token.
        #[serde(default)]
        auth_token_env: Option<String>,
    },
}

#[cfg(feature = "mcp")]
impl McpSection {
    /// Maps the configured servers into neutral [`zhive_mcp::McpServerConfig`]s.
    ///
    /// For HTTP servers, an `auth_token` set inline wins; otherwise
    /// `auth_token_env` is resolved via [`std::env::var`] (a missing or empty
    /// variable yields no token rather than an error).
    ///
    /// # Examples
    ///
    /// ```
    /// # use zhive_cli::config::McpSection;
    /// let section: McpSection = toml::from_str(
    ///     r#"
    ///     [servers.fs]
    ///     transport = "stdio"
    ///     command = "echo"
    ///     "#,
    /// )
    /// .unwrap();
    /// let configs = section.to_mcp_configs();
    /// assert_eq!(configs.len(), 1);
    /// assert_eq!(configs[0].name, "fs");
    /// ```
    #[must_use]
    pub fn to_mcp_configs(&self) -> Vec<zhive_mcp::McpServerConfig> {
        self.servers
            .iter()
            .map(|(name, def)| {
                let transport = match def {
                    McpServerDef::Stdio {
                        command,
                        args,
                        env,
                        cwd,
                    } => zhive_mcp::McpTransport::Stdio {
                        command: command.clone(),
                        args: args.clone(),
                        env: env.clone(),
                        cwd: cwd.clone(),
                    },
                    McpServerDef::Http {
                        url,
                        headers,
                        auth_token,
                        auth_token_env,
                    } => {
                        let auth_token = auth_token.clone().or_else(|| {
                            auth_token_env
                                .as_deref()
                                .and_then(|var| std::env::var(var).ok())
                                .filter(|tok| !tok.is_empty())
                        });
                        zhive_mcp::McpTransport::Http {
                            url: url.clone(),
                            headers: headers.clone(),
                            auth_token,
                        }
                    }
                };
                zhive_mcp::McpServerConfig {
                    name: name.clone(),
                    transport,
                }
            })
            .collect()
    }
}

/// The `[skills]` section: on-disk Agent-Skills discovery settings.
///
/// When `enabled` (the default), the engine discovers `SKILL.md` files under a
/// layered set of roots — the cross-tool external roots (`~/.claude/skills`,
/// `~/.agents/skills`, and their per-project `.claude`/`.agents` counterparts
/// along the repo ancestor chain), the zhive roots (`~/.config/zhive/skills`,
/// `./.zhive/skills`), and any `extra_roots` — then folds the model-invocable
/// ones into the system prompt as a read-on-demand `<available_skills>`
/// catalogue (they are not registered as tools).
///
/// # Examples
///
/// ```
/// # use zhive_cli::config::SkillsSection;
/// let section = SkillsSection::default();
/// assert!(section.enabled);
/// assert!(section.extra_roots.is_empty());
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsSection {
    /// Whether on-disk skill discovery runs at boot.
    pub enabled: bool,
    /// Additional skill root directories searched after the built-in roots.
    pub extra_roots: Vec<PathBuf>,
}

impl Default for SkillsSection {
    fn default() -> Self {
        Self {
            enabled: true,
            extra_roots: Vec::new(),
        }
    }
}

/// The `[engine]` section: per-turn execution limits.
///
/// Mirrors the codex-cli style of a configurable cap rather than a hardcoded
/// constant. `max_turn_iterations` bounds how many provider call iterations a
/// single turn may run (each iteration can issue tool calls):
///
/// * absent — use the engine default (a generous bounded cap).
/// * `0` — unbounded except for the engine's hard safety ceiling.
/// * `N` — cap at `N` iterations.
///
/// # Examples
///
/// ```
/// # use zhive_cli::config::EngineSection;
/// let section = EngineSection::default();
/// assert!(section.max_turn_iterations.is_none());
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineSection {
    /// Maximum provider-call iterations per turn. `None` keeps the engine
    /// default; `Some(0)` means unbounded (up to the hard safety ceiling);
    /// `Some(n)` caps at `n`.
    pub max_turn_iterations: Option<u32>,
}

impl Config {
    /// Loads config from `explicit` or the standard search path.
    ///
    /// Returns the parsed config and the path it came from (`None` when no
    /// file existed and defaults were used).
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

    /// The active provider entry's model id, for display in the top bar.
    ///
    /// Falls back to `"demo"` when the default name is not found (e.g. for
    /// `kind = "scripted"` or a misconfigured entry).
    #[must_use]
    pub fn active_model(&self) -> &str {
        self.provider
            .providers
            .get(&self.provider.default)
            .map_or("demo", |e| {
                if e.model.is_empty() {
                    "demo"
                } else {
                    e.model.as_str()
                }
            })
    }

    /// The active provider's `default` name string, for display in the top bar.
    #[must_use]
    pub fn active_provider_label(&self) -> &str {
        self.provider.default.as_str()
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
# Provider/model selection and TUI palette. Add any number of named provider
# entries under [provider.<name>]. Set `default` to the one you want active.
# The API key is read from the env var named by `api_key_env`, or set inline
# with `api_key` (discouraged).

[provider]
# Active provider name — must be a key in this section.
default = \"anthropic\"

[provider.anthropic]
kind = \"anthropic\"
model = \"claude-sonnet-4-6\"
api_key_env = \"ANTHROPIC_API_KEY\"
# base_url = \"https://api.anthropic.com/v1\"

[provider.openai]
kind = \"openai\"
model = \"gpt-4o\"
api_key_env = \"OPENAI_API_KEY\"
# base_url = \"https://api.openai.com/v1\"

# Other supported kinds: xai, mistral, azure, cohere, google,
# google-vertex, amazon-bedrock, anthropic-aws, scripted.

# Example: a local OpenAI-compatible proxy named \"lm-studio\"
# [provider.lm-studio]
# kind = \"openai\"
# model = \"llama-3.3-70b-instruct\"
# base_url = \"http://localhost:1234/v1\"
# api_key = \"lm-studio\"

# Cloud providers take extra fields. Azure OpenAI:
# [provider.azure]
# kind = \"azure\"
# model = \"gpt-4o\"
# resource_name = \"my-resource\"  # <resource>.openai.azure.com
# api_version = \"v1\"             # Azure API version query parameter
# deployment = \"my-deployment\"   # legacy deployment-based URLs only

# Google Vertex AI (project + location; region is shared with AWS backends):
# [provider.vertex]
# kind = \"google-vertex\"
# model = \"gemini-2.5-pro\"
# project = \"my-gcp-project\"
# location = \"us-central1\"
# region = \"us-central1\"

# Anthropic on AWS Bedrock (region + workspace_id):
# [provider.bedrock]
# kind = \"anthropic-aws\"
# model = \"claude-sonnet-4-6\"
# region = \"us-east-1\"
# workspace_id = \"my-workspace\"  # anthropic-workspace-id header

[ui]
theme = \"dark\"      # dark | light | mono
accent = \"cyan\"     # cyan | amber | lime | magenta
density = \"default\" # lean | default | airy

# MCP servers. Each [mcp.servers.<name>] entry exposes that server's tools to
# the model as `mcp__<name>__<tool>`. Two transports are supported.
#
# stdio (spawns a child process):
# [mcp.servers.filesystem]
# transport = \"stdio\"
# command = \"npx\"
# args = [\"-y\", \"@modelcontextprotocol/server-filesystem\", \"/tmp\"]
# # env = { FOO = \"bar\" }
# # cwd = \"/some/dir\"
#
# http (Streamable-HTTP endpoint):
# [mcp.servers.remote]
# transport = \"http\"
# url = \"http://localhost:8000/mcp\"
# # headers = { \"X-Trace\" = \"1\" }
# # auth_token = \"...\"            # inline (discouraged)
# # auth_token_env = \"MCP_TOKEN\"  # or read from an env var

# On-disk Agent-Skills (SKILL.md) discovery. Built-in roots include the
# cross-tool ~/.claude/skills and ~/.agents/skills (plus per-project .claude/
# .agents along the repo ancestor chain) and the zhive ~/.config/zhive/skills
# and ./.zhive/skills. Model-invocable skills are folded into the system prompt
# and read on demand (not registered as tools).
[skills]
enabled = true
# extra_roots = [\"/opt/zhive/skills\"]

# Per-turn engine limits. max_turn_iterations caps how many provider-call
# iterations one turn may run (each can issue tool calls). Omit for the engine
# default; 0 means unbounded (up to a hard safety ceiling); N caps at N.
[engine]
# max_turn_iterations = 80
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_anthropic_sonnet() {
        let cfg = Config::default();
        assert_eq!(cfg.provider.default, "anthropic");
        assert_eq!(cfg.active_model(), "claude-sonnet-4-6");
        assert_eq!(cfg.active_provider_label(), "anthropic");
    }

    #[test]
    fn engine_section_defaults_and_parses() {
        // Absent -> None (engine default applies).
        assert!(Config::default().engine.max_turn_iterations.is_none());
        // Explicit cap parses.
        let cfg: Config =
            toml::from_str("[engine]\nmax_turn_iterations = 12\n").expect("engine section parses");
        assert_eq!(cfg.engine.max_turn_iterations, Some(12));
        // 0 means unbounded (mapped to TurnLimits { max_iterations: None } in boot).
        let cfg: Config =
            toml::from_str("[engine]\nmax_turn_iterations = 0\n").expect("zero parses");
        assert_eq!(cfg.engine.max_turn_iterations, Some(0));
    }

    #[test]
    fn parses_a_full_document() {
        let cfg: Config = toml::from_str(SAMPLE_CONFIG).expect("sample must parse");
        assert_eq!(
            cfg.provider
                .providers
                .get("anthropic")
                .map(|e| e.model.as_str()),
            Some("claude-sonnet-4-6"),
        );
        assert_eq!(cfg.ui.theme, "dark");
        assert_eq!(cfg.ui.accent, "cyan");
    }

    #[test]
    fn parses_provider_override() {
        let toml = "[provider]\ndefault = \"openai\"\n[provider.openai]\nkind = \"openai\"\nmodel = \"gpt-4o-mini\"\n";
        let cfg: Config = toml::from_str(toml).expect("parses");
        assert_eq!(cfg.provider.default, "openai");
        assert_eq!(cfg.active_model(), "gpt-4o-mini");
    }

    #[test]
    fn custom_provider_with_openai_kind_parses() {
        let toml = r#"
[provider]
default = "my-proxy"

[provider.my-proxy]
kind = "openai"
model = "llama-3.3-70b-instruct"
base_url = "http://localhost:1234/v1"
api_key = "lm-studio"
"#;
        let cfg: Config = toml::from_str(toml).expect("custom provider parses");
        assert_eq!(cfg.provider.default, "my-proxy");
        let entry = cfg.provider.providers.get("my-proxy").unwrap();
        assert_eq!(entry.kind, "openai");
        assert_eq!(entry.model, "llama-3.3-70b-instruct");
        assert_eq!(entry.base_url.as_deref(), Some("http://localhost:1234/v1"));
    }

    #[test]
    fn active_model_falls_back_when_missing() {
        let mut cfg = Config::default();
        cfg.provider.default = "nonexistent".to_owned();
        assert_eq!(cfg.active_model(), "demo");
    }

    #[test]
    fn active_model_scripted_is_demo() {
        let mut cfg = Config::default();
        cfg.provider.default = "scripted".to_owned();
        assert_eq!(cfg.active_model(), "demo");
    }

    #[test]
    fn skills_default_enabled_with_no_roots() {
        let cfg = Config::default();
        assert!(cfg.skills.enabled);
        assert!(cfg.skills.extra_roots.is_empty());
        assert!(cfg.mcp.servers.is_empty());
    }

    #[test]
    fn parses_mcp_and_skills_sections() {
        let toml = r#"
[provider]
default = "scripted"

[provider.scripted]
kind = "scripted"
model = ""

[mcp.servers.filesystem]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]

[mcp.servers.remote]
transport = "http"
url = "http://localhost:8000/mcp"
auth_token_env = "MCP_TOKEN"

[skills]
enabled = false
extra_roots = ["/opt/zhive/skills"]
"#;
        let cfg: Config = toml::from_str(toml).expect("mcp/skills parse");
        assert_eq!(cfg.mcp.servers.len(), 2);
        assert!(matches!(
            cfg.mcp.servers.get("filesystem"),
            Some(McpServerDef::Stdio { .. })
        ));
        assert!(matches!(
            cfg.mcp.servers.get("remote"),
            Some(McpServerDef::Http { .. })
        ));
        assert!(!cfg.skills.enabled);
        assert_eq!(cfg.skills.extra_roots.len(), 1);
    }

    #[test]
    fn mcp_server_def_rejects_unknown_field() {
        let toml = r#"
transport = "stdio"
command = "echo"
bogus = "nope"
"#;
        let result: Result<McpServerDef, _> = toml::from_str(toml);
        assert!(result.is_err(), "unknown field must be rejected");
    }

    /// Drift guard: every key reachable from a fully-populated [`Config`] must
    /// be documented in [`SAMPLE_CONFIG`].
    ///
    /// `zhive config init` emits the hand-written, comment-rich `SAMPLE_CONFIG`
    /// (not a serializer dump), so it silently drifts out of sync whenever a new
    /// field is added to a config struct. This test serializes a maximal config
    /// (every optional field `Some`, both `McpServerDef` variants present) and
    /// asserts each emitted scalar key appears in `SAMPLE_CONFIG` as a documented
    /// key — commented (`# key = ...`) or live (`key = ...`). A new struct field
    /// nobody added to the sample fails this test, never the user's config file.
    ///
    /// The maximal config is built from full struct literals (no `..default()`),
    /// so a new field on any config struct also fails to *compile* here until the
    /// author handles it — the runtime substring check then forces it into the
    /// sample. Limitation: map-valued fields (`env`, `headers`) serialize as TOML
    /// sub-tables rather than `key = value` lines, so their field *names* are not
    /// individually guarded (both are already documented and structurally
    /// stable); their scalar siblings still are. A brand-new enum *variant* is
    /// only covered if it is added to the maximal config below.
    #[test]
    fn sample_config_documents_every_field() {
        // Every struct is built with a full field literal (no `..default()`), so
        // adding a field to ANY config struct fails to compile here until the
        // author handles it — extending the guard's reach to the optional fields
        // a `default()`-plus-mutation setup would silently omit. env/headers are
        // left empty on purpose: a map serializes as a TOML sub-table, and
        // keeping values before tables keeps the serialized output valid.
        let cfg = Config {
            provider: ProviderSection {
                default: "anthropic".to_owned(),
                providers: BTreeMap::from([(
                    "anthropic".to_owned(),
                    ProviderEntry {
                        kind: "azure".to_owned(),
                        model: "gpt-4o".to_owned(),
                        api_key: Some("inline".to_owned()),
                        api_key_env: Some("OPENAI_API_KEY".to_owned()),
                        base_url: Some("https://example.invalid".to_owned()),
                        region: Some("us-east-1".to_owned()),
                        project: Some("proj".to_owned()),
                        location: Some("us-central1".to_owned()),
                        resource_name: Some("res".to_owned()),
                        api_version: Some("v1".to_owned()),
                        deployment: Some("dep".to_owned()),
                        workspace_id: Some("ws".to_owned()),
                    },
                )]),
            },
            ui: UiSection {
                theme: "dark".to_owned(),
                accent: "cyan".to_owned(),
                density: "default".to_owned(),
            },
            mcp: McpSection {
                servers: BTreeMap::from([
                    (
                        "filesystem".to_owned(),
                        McpServerDef::Stdio {
                            command: "npx".to_owned(),
                            args: vec!["-y".to_owned()],
                            env: BTreeMap::new(),
                            cwd: Some("/tmp".to_owned()),
                        },
                    ),
                    (
                        "remote".to_owned(),
                        McpServerDef::Http {
                            url: "http://localhost:8000/mcp".to_owned(),
                            headers: BTreeMap::new(),
                            auth_token: Some("tok".to_owned()),
                            auth_token_env: Some("MCP_TOKEN".to_owned()),
                        },
                    ),
                ]),
            },
            skills: SkillsSection {
                enabled: true,
                extra_roots: vec![PathBuf::from("/opt/zhive/skills")],
            },
            engine: EngineSection {
                max_turn_iterations: Some(80),
            },
        };

        let serialized = toml::to_string(&cfg).expect("maximal config serializes");

        let mut missing = Vec::new();
        for line in serialized.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('[') {
                continue;
            }
            let Some((key, _)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if !key.is_empty() && !sample_documents_key(key) {
                missing.push(key.to_owned());
            }
        }
        missing.sort();
        missing.dedup();

        assert!(
            missing.is_empty(),
            "SAMPLE_CONFIG is missing documentation for config key(s): {missing:?}. \
             Add each (a commented `# key = ...` example is enough) so `zhive config init` \
             stays in sync with the config schema.",
        );
    }

    /// True when `key` appears in [`SAMPLE_CONFIG`] as a documented TOML key,
    /// either live (`key = ...`) or commented (`# key = ...`).
    ///
    /// Anchored on the `key =` token rather than a bare substring so a short key
    /// such as `env` is not spuriously matched by prose like "environment".
    fn sample_documents_key(key: &str) -> bool {
        SAMPLE_CONFIG.lines().any(|line| {
            // Strip any nesting of comment markers and whitespace (`# `, `# # `)
            // so commented-out examples count as documentation.
            let line = line.trim_start_matches(|c: char| c == '#' || c.is_whitespace());
            line.strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn to_mcp_configs_maps_stdio_and_http() {
        let toml = r#"
[servers.fs]
transport = "stdio"
command = "echo"
args = ["hi"]

[servers.remote]
transport = "http"
url = "http://localhost:9000/mcp"
auth_token = "inline-token"
"#;
        let section: McpSection = toml::from_str(toml).expect("parse");
        let configs = section.to_mcp_configs();
        assert_eq!(configs.len(), 2);
        // BTreeMap orders keys: "fs" then "remote".
        assert_eq!(configs[0].name, "fs");
        assert!(matches!(
            configs[0].transport,
            zhive_mcp::McpTransport::Stdio { .. }
        ));
        assert_eq!(configs[1].name, "remote");
        match &configs[1].transport {
            zhive_mcp::McpTransport::Http { auth_token, .. } => {
                assert_eq!(auth_token.as_deref(), Some("inline-token"));
            }
            zhive_mcp::McpTransport::Stdio { .. } => panic!("expected http transport"),
        }
    }
}

// Rust guideline compliant 2026-02-21

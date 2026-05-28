//! Extension / prompt / skill manifest wire schema (D-013 revised).
//!
//! The manifest namespace was renamed from `skill | slash_command | hook`
//! (D-013 first draft) to `extension | prompt | skill` to match Pi
//! verbatim and let an `extension` aggregate the smaller surfaces
//! (tools, hooks, slash commands, shortcuts, flags) under one root.
//! See `plans/phase1-core-native-research/decision-diffs.md` §1.11 for
//! the rationale.
//!
//! # Phase 1 boundaries
//!
//! * [`ExtensionManifest::entrypoint`] only accepts `"builtin"`; third
//!   party entrypoints (wasm, subprocess) land in Phase 2.
//! * Hook registration is **manifest-only**: settings-level loose
//!   registration is rejected (red line 10 alignment).
//! * [`ResourcesDiscoverEvent`] is reserved as a Phase 2 surface; this
//!   module declares the schema slot but the host does not dispatch it
//!   yet.
//!
//! # Field decisions
//!
//! The mapping from Pi `ToolDefinition` 12 fields to zhive 11 manifest
//! fields is captured in
//! `plans/phase1-core-native-research/deliverables/A5-extension-manifest.md`
//! §2; the table is also reproduced in the [`ToolDefinition`] docstring.
//!
//! [`ResourcesDiscoverEvent`]: ResourcesDiscoverEvent

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(feature = "schema")]
use schemars::JsonSchema;

// ============================================================
// ExtensionRef re-export from hook
// ============================================================

pub use crate::hook::{ExtensionRef, ExtensionSource};

// ============================================================
// Top-level manifest
// ============================================================

/// Discriminator value for [`Manifest::kind`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ManifestKind {
    /// Carries tools, hooks, slash commands, shortcuts and flags.
    Extension,
    /// Markdown prompt with front-matter.
    Prompt,
    /// Skill bundle with a `SKILL.md` front-matter.
    Skill,
}

/// Top-level manifest document.
///
/// Common metadata sits at the root; the variant-specific body lives in
/// [`spec`]. The wire encoding uses an external discriminator (`kind`)
/// plus a flattened body, which keeps TOML and JSON inputs identical.
///
/// [`spec`]: Self::spec
///
/// # Examples
///
/// ```
/// use zhive_proto::manifest::{Manifest, ManifestKind};
/// let payload = r#"{
///     "kind": "extension",
///     "schemaVersion": "1",
///     "name": "git-helper",
///     "displayName": "Git Helper",
///     "version": "0.1.0",
///     "entrypoint": "builtin"
/// }"#;
/// let m: Manifest = serde_json::from_str(payload).unwrap();
/// assert_eq!(m.name, "git-helper");
/// assert!(matches!(m.kind, ManifestKind::Extension));
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Manifest {
    /// Which manifest family this document carries.
    pub kind: ManifestKind,
    /// Manifest schema version. Phase 1 accepts `"1"`.
    pub schema_version: String,
    /// Unique name within the manifest's namespace.
    pub name: String,
    /// Optional human-readable summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Variant-specific body.
    #[serde(flatten)]
    pub spec: ManifestSpec,
}

/// Variant-specific portion of [`Manifest`].
///
/// Serialised untagged so the body fields sit at the manifest root
/// alongside `kind`, `name`, …
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(untagged)]
#[non_exhaustive]
pub enum ManifestSpec {
    /// `kind = "extension"` body. Boxed to keep the enum size moderate.
    Extension(Box<ExtensionManifest>),
    /// `kind = "prompt"` body.
    Prompt(PromptManifest),
    /// `kind = "skill"` body.
    Skill(SkillManifest),
}

// ============================================================
// ExtensionManifest
// ============================================================

/// Body for `kind: extension` manifests.
///
/// Aggregates tools, hooks, slash commands, shortcuts and flags under
/// one extension root. Hook registration **must** come through this
/// manifest (red line 10) — loose settings-level hooks are rejected by
/// the host.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ExtensionManifest {
    /// UI display name.
    pub display_name: String,
    /// Semver string.
    pub version: String,
    /// Optional list of authors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authors: Option<Vec<String>>,
    /// SPDX identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Phase 1 accepts only `"builtin"`.
    pub entrypoint: String,
    /// What this extension contributes.
    #[serde(default)]
    pub capabilities: ExtensionCapabilities,
    /// `[[tools]]` sub-tables.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    /// `[[hooks]]` sub-tables.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<HookDefinition>,
    /// `[[slash_commands]]` sub-tables.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slash_commands: Vec<SlashCommandDefinition>,
    /// `[[shortcuts]]` sub-tables.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shortcuts: Vec<ShortcutDefinition>,
    /// `[[flags]]` sub-tables.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<FlagDefinition>,
    /// `[[resource_contributions]]` sub-tables (Phase 2 surface,
    /// schema-only in Phase 1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_contributions: Vec<ResourceContribution>,
}

impl ExtensionManifest {
    /// Returns a minimally populated extension manifest for tests.
    ///
    /// The defaults are intentionally inert: built-in entrypoint, no
    /// declared capabilities, empty contribution lists.
    #[must_use]
    pub fn default_for(display_name: String) -> Self {
        Self {
            display_name,
            version: "0.0.0".into(),
            authors: None,
            license: None,
            entrypoint: "builtin".into(),
            capabilities: ExtensionCapabilities::default(),
            tools: Vec::new(),
            hooks: Vec::new(),
            slash_commands: Vec::new(),
            shortcuts: Vec::new(),
            flags: Vec::new(),
            resource_contributions: Vec::new(),
        }
    }
}

/// What an extension claims to contribute.
///
/// Mirrors the sub-table list on [`ExtensionManifest`]; the host can
/// short-circuit dispatch by checking the flag before searching the
/// list.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
#[expect(
    clippy::struct_excessive_bools,
    reason = "capability digest mirrors the sub-table list 1:1; collapsing into a bitset would hide the names from JSON Schema"
)]
pub struct ExtensionCapabilities {
    /// `true` when the extension registers any hooks.
    #[serde(default)]
    pub hooks: bool,
    /// `true` when the extension contributes tools.
    #[serde(default)]
    pub tools: bool,
    /// `true` when the extension contributes slash commands.
    #[serde(default)]
    pub slash_commands: bool,
    /// `true` when the extension contributes shortcuts.
    #[serde(default)]
    pub shortcuts: bool,
    /// `true` when the extension exposes flags.
    #[serde(default)]
    pub flags: bool,
}

// ============================================================
// ToolDefinition
// ============================================================

/// One tool exposed by an extension.
///
/// Mapping from Pi's 12-field `ToolDefinition` to zhive's 11 manifest
/// fields:
///
/// | Pi field | zhive | note |
/// |---|---|---|
/// | `name` | [`name`] | Tool identifier (manifest key). |
/// | `label` | [`label`] | UI label. |
/// | `description` | [`description`] | Limited to 1024 characters. |
/// | `promptSnippet` | [`prompt_snippet`] | Optional Available-Tools snippet. |
/// | `promptGuidelines` | [`prompt_guidelines`] | Optional usage hints. |
/// | `parameters` | [`parameters_schema`] | JSON Schema serialised as a string. |
/// | `renderShell` | [`render_shell`] | `"default"` or `"self"`. |
/// | `prepareArguments` | (rejected) | Replaced by [`allow_loose_inputs`]. |
/// | `executionMode` | [`execution_mode`] | `"sequential"` or `"parallel"`. |
/// | `execute` | (rejected) | Phase 1 entrypoint is `"builtin"`. |
/// | `renderCall` | [`render_call`] | JSON descriptor. |
/// | `renderResult` | [`render_result`] | JSON descriptor. |
///
/// [`name`]: Self::name
/// [`label`]: Self::label
/// [`description`]: Self::description
/// [`prompt_snippet`]: Self::prompt_snippet
/// [`prompt_guidelines`]: Self::prompt_guidelines
/// [`parameters_schema`]: Self::parameters_schema
/// [`render_shell`]: Self::render_shell
/// [`execution_mode`]: Self::execution_mode
/// [`render_call`]: Self::render_call
/// [`render_result`]: Self::render_result
/// [`allow_loose_inputs`]: Self::allow_loose_inputs
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ToolDefinition {
    /// Tool name surfaced to the model.
    pub name: String,
    /// UI label.
    pub label: String,
    /// Free-form description shown to the model; the host rejects
    /// values longer than 1024 characters.
    pub description: String,
    /// Optional snippet inserted into the Available-Tools system prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_snippet: Option<String>,
    /// Optional usage guidelines (bullet list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_guidelines: Option<Vec<String>>,
    /// JSON Schema (encoded as a JSON string) for the tool arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters_schema: Option<String>,
    /// `"default"` to let zhive render the call/result frame, `"self"`
    /// to delegate to the extension's own renderer descriptor.
    #[serde(default = "default_render_shell")]
    pub render_shell: String,
    /// `"sequential"` (default) or `"parallel"`.
    #[serde(default = "default_execution_mode")]
    pub execution_mode: String,
    /// Optional renderer descriptor for the in-flight call display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_call: Option<RenderDescriptor>,
    /// Optional renderer descriptor for the finished result display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_result: Option<RenderDescriptor>,
    /// Allow the host to coerce best-effort arguments before validation.
    #[serde(default)]
    pub allow_loose_inputs: bool,
    /// Hide the tool when a subagent is the caller.
    #[serde(default)]
    pub disable_in_subagent: bool,
}

fn default_render_shell() -> String {
    "default".to_string()
}

fn default_execution_mode() -> String {
    "sequential".to_string()
}

// ============================================================
// RenderDescriptor (preset + composite)
// ============================================================

/// Renderer descriptor for [`ToolDefinition::render_call`] and
/// [`ToolDefinition::render_result`].
///
/// Two flavours: a named preset (covers the common 80%) or a composite
/// list of rows for bespoke layouts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "kind", rename_all = "lowercase")]
#[non_exhaustive]
pub enum RenderDescriptor {
    /// Pick a built-in preset by name.
    Preset {
        /// Preset identifier (see [`RenderPreset`]).
        preset: RenderPreset,
    },
    /// Compose the view from row primitives.
    Composite {
        /// Ordered row list.
        rows: Vec<RenderRow>,
    },
}

/// Built-in render presets.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RenderPreset {
    /// Shell command + transcript layout.
    CommandLine,
    /// Old/new file diff layout.
    Diff,
    /// File tree layout (ls / find output).
    FileTree,
    /// Plain text block.
    TextBlock,
    /// Key/value table.
    KeyValue,
}

/// One row inside a composite renderer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RenderRow {
    /// Row primitive: `"title"`, `"text"`, `"diff"`, `"key_value"`,
    /// `"file_path"`, `"spinner"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Optional templated text body (supports `${args.path}` etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Field name to read the "old" side of a diff from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_field: Option<String>,
    /// Field name to read the "new" side of a diff from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_field: Option<String>,
}

// ============================================================
// HookDefinition / SlashCommandDefinition / Shortcut / Flag
// ============================================================

/// Hook subscription registered through an extension manifest.
///
/// `registered_by` is **not** part of the on-disk schema — the host
/// injects it from the owning manifest before dispatch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct HookDefinition {
    /// Hook event name, e.g. `"PreToolUse"`.
    pub event: String,
    /// Optional tool filter; empty / absent means *every tool*.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_filter: Option<Vec<String>>,
    /// Higher values run first within the same source rank.
    #[serde(default)]
    pub priority: i32,
    /// Hide the hook from subagent threads.
    #[serde(default)]
    pub disable_in_subagent: bool,
}

/// Slash command exposed by an extension.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SlashCommandDefinition {
    /// Command name (globally unique across `prompt` and `skill` too).
    pub name: String,
    /// Free-form description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Dispatch target, e.g. `"tool:git_blame"` or
    /// `"prompt:templates/blame.md"`.
    pub target: String,
}

/// Keybinding shortcut.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ShortcutDefinition {
    /// Key combination, e.g. `"ctrl+g b"`.
    pub key: String,
    /// Slash command to invoke.
    pub command: String,
}

/// Custom feature flag.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct FlagDefinition {
    /// Flag name.
    pub name: String,
    /// Type tag: `"boolean"`, `"string"`, `"number"`, …
    #[serde(rename = "type")]
    pub flag_type: String,
    /// Optional default value matching `flag_type`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Resource contribution declaration (Phase 2 surface).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ResourceContribution {
    /// Resource kind: `"skill"`, `"prompt"`, `"theme"`.
    pub kind: String,
    /// Paths relative to the manifest root.
    pub paths: Vec<String>,
    /// Optional priority offset (Phase 2; ignored in Phase 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_offset: Option<i32>,
}

// ============================================================
// PromptManifest / SkillManifest
// ============================================================

/// Body for `kind: prompt` manifests.
///
/// Prompts default to **slash-only invocation**; set [`model_invocable`]
/// to true to let the model auto-pick the prompt by keyword.
///
/// [`model_invocable`]: Self::model_invocable
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PromptManifest {
    /// Whether the model can invoke this prompt without an explicit
    /// slash command. Defaults to `false`.
    #[serde(default)]
    pub model_invocable: bool,
    /// Optional tool allowlist (intersected with the parent scope).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    /// Hide the prompt from subagent threads.
    #[serde(default)]
    pub disable_in_subagent: bool,
}

/// Body for `kind: skill` manifests.
///
/// Skills default to **model-invocable**; the model auto-picks one when
/// a keyword in [`auto_invoke_keywords`] appears in the conversation.
///
/// [`auto_invoke_keywords`]: Self::auto_invoke_keywords
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SkillManifest {
    /// Whether the model can invoke this skill without an explicit
    /// slash command. Defaults to `true`.
    #[serde(default = "default_true")]
    pub model_invocable: bool,
    /// Optional tool allowlist (intersected with the parent scope).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    /// Keywords that trigger auto-invocation by the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_invoke_keywords: Option<Vec<String>>,
    /// Hide the skill from subagent threads.
    #[serde(default)]
    pub disable_in_subagent: bool,
}

const fn default_true() -> bool {
    true
}

// ============================================================
// Resource discovery (Phase 2 surface, schema-only in Phase 1)
// ============================================================

/// Request payload for the Phase 2 `extension/resources/discover` RPC.
///
/// Phase 1 keeps the schema slot but the host does not dispatch the
/// event yet; the struct is intentionally empty (future fields go in
/// non-breaking additions).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ResourcesDiscoverEvent;

/// Response payload returned by an extension after a resource discovery
/// round-trip.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ResourcesDiscoverResult {
    /// Skill manifest paths contributed by the extension.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_paths: Vec<String>,
    /// Prompt manifest paths contributed by the extension.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_paths: Vec<String>,
    /// Theme asset paths contributed by the extension.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub theme_paths: Vec<String>,
}

// ============================================================
// Errors
// ============================================================

/// Errors returned by the manifest loader.
///
/// The on-disk parsers (TOML, JSON, YAML) live in `zhive-core` and
/// wrap their own errors; the variants here cover the schema-level
/// constraints declared by D-013 and red lines 10/11.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, thiserror::Error)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase", tag = "kind")]
#[non_exhaustive]
pub enum ManifestError {
    /// `kind` field carried an unexpected value.
    #[error("invalid manifest kind: {value}")]
    InvalidKind {
        /// Value seen on the wire.
        value: String,
    },
    /// `schema_version` field carried a version the loader does not
    /// understand.
    #[error("unsupported manifest schema version: {value}")]
    UnsupportedSchemaVersion {
        /// Value seen on the wire.
        value: String,
    },
    /// A required field was missing.
    #[error("missing required field: {field}")]
    MissingField {
        /// Field path (dot-separated).
        field: String,
    },
    /// `ToolDefinition::description` exceeded the 1024 character limit.
    #[error("tool {tool} description exceeds 1024 characters")]
    ToolDescriptionTooLong {
        /// Offending tool name.
        tool: String,
    },
    /// A JSON Schema string failed to parse.
    #[error("invalid JSON schema in {field}: {reason}")]
    InvalidSchema {
        /// Field path that hosted the schema.
        field: String,
        /// Parser diagnostic.
        reason: String,
    },
    /// Two manifests collided on a resource name without explicit
    /// priority ordering.
    #[error("duplicate resource: {name} from {sources:?}")]
    DuplicateResource {
        /// Conflicting name.
        name: String,
        /// Source paths that registered it.
        sources: Vec<String>,
    },
    /// Hook registered at a settings-level scope instead of a manifest
    /// scope. Red line 10 rejects this.
    #[error(
        "settings-level hook registration forbidden; declare hooks under an extension manifest"
    )]
    TopLevelHookForbidden,
    /// `RenderDescriptor::Preset` named a non-existent preset.
    #[error("unknown render preset: {value}")]
    InvalidRenderPreset {
        /// Preset value seen on the wire.
        value: String,
    },
    /// `entrypoint` was something other than `"builtin"` in Phase 1.
    #[error("invalid entrypoint (Phase 1 only accepts 'builtin'): {value}")]
    InvalidEntrypoint {
        /// Entrypoint value seen on the wire.
        value: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_kind_snake_case_wire() {
        assert_eq!(
            serde_json::to_string(&ManifestKind::Extension).unwrap(),
            "\"extension\""
        );
        assert_eq!(
            serde_json::to_string(&ManifestKind::Skill).unwrap(),
            "\"skill\""
        );
    }

    #[test]
    fn extension_manifest_default_round_trip() {
        let m = ExtensionManifest::default_for("git-helper".into());
        let s = serde_json::to_string(&m).unwrap();
        let back: ExtensionManifest = serde_json::from_str(&s).unwrap();
        assert_eq!(m, back);
        assert_eq!(m.entrypoint, "builtin");
    }

    #[test]
    fn manifest_flatten_extension_body() {
        let m = Manifest {
            kind: ManifestKind::Extension,
            schema_version: "1".into(),
            name: "git-helper".into(),
            description: None,
            spec: ManifestSpec::Extension(Box::new(ExtensionManifest::default_for(
                "git-helper".into(),
            ))),
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["kind"], "extension");
        // ExtensionManifest fields land at the manifest root via flatten:
        assert_eq!(v["displayName"], "git-helper");
        assert_eq!(v["entrypoint"], "builtin");
    }

    #[test]
    fn skill_manifest_defaults_to_model_invocable() {
        let s: SkillManifest = serde_json::from_str("{}").unwrap();
        assert!(s.model_invocable);
    }

    #[test]
    fn prompt_manifest_defaults_to_slash_only() {
        let p: PromptManifest = serde_json::from_str("{}").unwrap();
        assert!(!p.model_invocable);
    }

    #[test]
    fn render_descriptor_preset_round_trip() {
        let d = RenderDescriptor::Preset {
            preset: RenderPreset::Diff,
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["kind"], "preset");
        assert_eq!(v["preset"], "diff");
        let back: RenderDescriptor = serde_json::from_value(v).unwrap();
        assert_eq!(d, back);
    }

    #[test]
    fn manifest_error_display_with_data() {
        let e = ManifestError::ToolDescriptionTooLong {
            tool: "read_file".into(),
        };
        let msg = format!("{e}");
        assert!(msg.contains("read_file"));
        assert!(msg.contains("1024"));
    }

    #[test]
    fn flag_definition_renames_type() {
        let f = FlagDefinition {
            name: "debug".into(),
            flag_type: "boolean".into(),
            default: Some(Value::Bool(false)),
            description: None,
        };
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["type"], "boolean");
        assert!(v.get("flagType").is_none());
    }
}

// Rust guideline compliant 2026-02-21

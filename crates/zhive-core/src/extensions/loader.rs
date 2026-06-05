//! `manifest.json` parser built on kind-dispatch, not untagged deserialize.
//!
//! [`parse_manifest`] is the SOLE correct way to turn a manifest document into
//! a typed [`Manifest`] in zhive. It reads the root `kind` discriminator and
//! deserializes the matching concrete body, instead of deserializing
//! [`Manifest`] directly.
//!
//! # Why kind-dispatch is mandatory
//!
//! [`zhive_proto::manifest::ManifestSpec`] is `#[serde(untagged)]` and is
//! `#[serde(flatten)]`-ed into [`Manifest`]. A direct
//! `serde_json::from_str::<Manifest>` therefore IGNORES the root `kind` field
//! for variant selection: serde tries the variants in declaration order, and
//! [`PromptManifest`](zhive_proto::manifest::PromptManifest) — whose fields are
//! all optional with no `deny_unknown_fields` — matches first. A `skill` or
//! `extension` body silently deserializes as a `Prompt`, dropping its real
//! fields (e.g. `autoInvokeKeywords`), and the `Skill` variant is structurally
//! unreachable. This loader is the single chokepoint that avoids that defect;
//! no other code should call `Manifest`/`ManifestSpec` deserialize directly.

use std::path::Path;

use serde::de::DeserializeOwned;
use serde_json::Value;
use zhive_proto::manifest::{
    ExtensionManifest, ManifestError, ManifestKind, PromptManifest, SkillManifest,
};

use super::error::ExtensionError;
use super::{ManifestBody, ParsedManifest};

/// The only `schemaVersion` accepted in Phase 1.
const SCHEMA_VERSION_1: &str = "1";

/// Maximum tool `description` length in characters (D-013).
const MAX_TOOL_DESCRIPTION_CHARS: usize = 1024;

/// Parses a manifest document by dispatching on its `kind` discriminator.
///
/// This never deserializes [`Manifest`] or [`ManifestSpec`] directly — see the
/// module docs for why an untagged deserialize corrupts the body. It reads
/// `kind`, validates the shared root metadata, then deserializes the concrete
/// body type for that kind.
///
/// # Errors
///
/// Returns [`ManifestError`] when `kind` is missing or unknown, `schemaVersion`
/// is unsupported, a required body field is absent, or (for `kind: extension`)
/// the entrypoint is not `"builtin"` in Phase 1.
///
/// # Examples
///
/// ```
/// use serde_json::json;
/// use zhive_core::extensions::{parse_manifest, ManifestBody};
/// use zhive_proto::manifest::ManifestKind;
///
/// let value = json!({
///     "kind": "skill",
///     "schemaVersion": "1",
///     "name": "deployer",
///     "modelInvocable": true,
///     "autoInvokeKeywords": ["deploy", "release"]
/// });
/// let manifest = parse_manifest(&value).expect("valid skill manifest");
/// assert_eq!(manifest.kind, ManifestKind::Skill);
/// match manifest.body {
///     // kind-dispatch preserves skill-only fields an untagged deserialize drops.
///     ManifestBody::Skill(skill) => {
///         assert_eq!(skill.auto_invoke_keywords.unwrap(), ["deploy", "release"]);
///     }
///     other => panic!("expected Skill, got {other:?}"),
/// }
/// ```
pub fn parse_manifest(value: &Value) -> Result<ParsedManifest, ManifestError> {
    let kind = parse_kind(value)?;

    let schema_version = required_str(value, "schemaVersion")?;
    if schema_version != SCHEMA_VERSION_1 {
        return Err(ManifestError::UnsupportedSchemaVersion {
            value: schema_version,
        });
    }
    let name = required_str(value, "name")?;
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned);

    // Deserialize the CONCRETE body for this kind from the same object. The
    // body structs ignore the extra root keys (kind/schemaVersion/name/…) and
    // surface a real serde error when a required body field is missing.
    let body = match kind {
        ManifestKind::Extension => {
            let ext: ExtensionManifest = deserialize_body(value, kind)?;
            validate_extension(&ext)?;
            ManifestBody::Extension(Box::new(ext))
        }
        ManifestKind::Prompt => {
            ManifestBody::Prompt(deserialize_body::<PromptManifest>(value, kind)?)
        }
        ManifestKind::Skill => ManifestBody::Skill(deserialize_body::<SkillManifest>(value, kind)?),
        // `ManifestKind` is #[non_exhaustive]; a future kind this loader does
        // not yet construct a body for is rejected, never silently mishandled.
        _ => {
            return Err(ManifestError::InvalidKind {
                value: format!("{kind:?}"),
            });
        }
    };

    Ok(ParsedManifest {
        kind,
        schema_version,
        name,
        description,
        body,
    })
}

/// Reads and parses a `manifest.json` file from disk.
///
/// Delegates the schema-level parse to [`parse_manifest`] (kind-dispatch).
///
/// # Errors
///
/// Returns [`ExtensionError::Io`] when the file cannot be read,
/// [`ExtensionError::Json`] when it is not valid JSON, or
/// [`ExtensionError::Manifest`] when a schema constraint is violated.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use zhive_core::extensions::load_manifest_file;
///
/// let manifest = load_manifest_file(Path::new("/proj/.zhive/extensions/git/manifest.json"))?;
/// assert_eq!(manifest.schema_version, "1");
/// # Ok::<(), zhive_core::extensions::ExtensionError>(())
/// ```
pub fn load_manifest_file(path: &Path) -> Result<ParsedManifest, ExtensionError> {
    let raw = std::fs::read_to_string(path).map_err(|e| ExtensionError::Io {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    let value: Value = serde_json::from_str(&raw).map_err(|e| ExtensionError::Json {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    parse_manifest(&value).map_err(|source| ExtensionError::Manifest {
        path: path.to_path_buf(),
        source,
    })
}

// ============================================================
// Internal helpers
// ============================================================

/// Reads and deserializes the `kind` discriminator.
fn parse_kind(value: &Value) -> Result<ManifestKind, ManifestError> {
    let raw = value
        .get("kind")
        .ok_or_else(|| ManifestError::MissingField {
            field: "kind".to_string(),
        })?;
    serde_json::from_value::<ManifestKind>(raw.clone()).map_err(|_err| ManifestError::InvalidKind {
        value: scalar_to_string(raw),
    })
}

/// Reads a required string field, or [`ManifestError::MissingField`].
fn required_str(value: &Value, field: &str) -> Result<String, ManifestError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ManifestError::MissingField {
            field: field.to_string(),
        })
}

/// Deserializes a concrete body type from the manifest object.
fn deserialize_body<T: DeserializeOwned>(
    value: &Value,
    kind: ManifestKind,
) -> Result<T, ManifestError> {
    serde_json::from_value::<T>(value.clone()).map_err(|err| body_error(kind, &err))
}

/// Maps a body deserialize failure onto a [`ManifestError`].
///
/// A missing required field is reported as [`ManifestError::MissingField`] with
/// the field name parsed from serde's stable missing-field message; anything
/// else becomes [`ManifestError::InvalidSchema`].
fn body_error(kind: ManifestKind, err: &serde_json::Error) -> ManifestError {
    if let Some(field) = missing_field(err) {
        ManifestError::MissingField { field }
    } else {
        ManifestError::InvalidSchema {
            field: format!("{kind:?} body"),
            reason: err.to_string(),
        }
    }
}

/// Extracts the field name from serde's missing-field diagnostic.
fn missing_field(err: &serde_json::Error) -> Option<String> {
    const MARKER: &str = "missing field `";
    let message = err.to_string();
    let start = message.find(MARKER)? + MARKER.len();
    let rest = &message[start..];
    let end = rest.find('`')?;
    Some(rest[..end].to_string())
}

/// Validates Phase-1 constraints on an extension body.
fn validate_extension(body: &ExtensionManifest) -> Result<(), ManifestError> {
    if body.entrypoint != "builtin" {
        return Err(ManifestError::InvalidEntrypoint {
            value: body.entrypoint.clone(),
        });
    }
    for tool in &body.tools {
        if tool.description.chars().count() > MAX_TOOL_DESCRIPTION_CHARS {
            return Err(ManifestError::ToolDescriptionTooLong {
                tool: tool.name.clone(),
            });
        }
    }
    Ok(())
}

/// Renders a JSON scalar as a short string for error messages.
fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    // The corruption-documenting test deserializes the proto `Manifest`
    // directly to prove why the loader must not.
    use zhive_proto::manifest::{Manifest, ManifestSpec};

    #[test]
    fn skill_kind_preserves_auto_invoke_keywords() {
        // The exact corruption kind-dispatch exists to prevent: an untagged
        // deserialize would route this skill body to Prompt and drop the
        // skill-only `autoInvokeKeywords`.
        let value = json!({
            "kind": "skill",
            "schemaVersion": "1",
            "name": "deployer",
            "autoInvokeKeywords": ["deploy", "release"]
        });
        let manifest = parse_manifest(&value).expect("valid skill manifest");
        assert_eq!(manifest.kind, ManifestKind::Skill);
        match manifest.body {
            ManifestBody::Skill(skill) => {
                assert_eq!(
                    skill.auto_invoke_keywords.as_deref(),
                    Some(&["deploy".to_string(), "release".to_string()][..]),
                );
                assert!(skill.model_invocable, "skills default to model-invocable");
            }
            other => panic!("expected Skill, got {other:?}"),
        }
    }

    #[test]
    fn naive_untagged_deserialize_corrupts_skill_into_prompt() {
        // Documents WHY parse_manifest must dispatch on kind: deserializing
        // Manifest directly misroutes a skill body to Prompt and drops fields.
        let raw = r#"{
            "kind": "skill",
            "schemaVersion": "1",
            "name": "deployer",
            "autoInvokeKeywords": ["deploy"]
        }"#;
        let corrupted: Manifest = serde_json::from_str(raw).expect("untagged still parses");
        assert!(
            matches!(corrupted.spec, ManifestSpec::Prompt(_)),
            "untagged deserialize is expected to misroute skill -> Prompt; \
             this is the defect parse_manifest avoids",
        );
    }

    #[test]
    fn extension_kind_round_trips() {
        let value = json!({
            "kind": "extension",
            "schemaVersion": "1",
            "name": "git-helper",
            "displayName": "Git Helper",
            "version": "0.1.0",
            "entrypoint": "builtin"
        });
        let manifest = parse_manifest(&value).expect("valid extension manifest");
        assert_eq!(manifest.kind, ManifestKind::Extension);
        match manifest.body {
            ManifestBody::Extension(ext) => {
                assert_eq!(ext.display_name, "Git Helper");
                assert_eq!(ext.entrypoint, "builtin");
            }
            other => panic!("expected Extension, got {other:?}"),
        }
    }

    #[test]
    fn prompt_kind_round_trips() {
        let value = json!({
            "kind": "prompt",
            "schemaVersion": "1",
            "name": "blame",
            "modelInvocable": true
        });
        let manifest = parse_manifest(&value).expect("valid prompt manifest");
        match manifest.body {
            ManifestBody::Prompt(prompt) => assert!(prompt.model_invocable),
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn extension_missing_entrypoint_errors_not_prompt_fallback() {
        // Required `entrypoint` is absent: kind-dispatch surfaces a real
        // MissingField error instead of silently degrading to a Prompt.
        let value = json!({
            "kind": "extension",
            "schemaVersion": "1",
            "name": "broken",
            "displayName": "Broken",
            "version": "0.1.0"
        });
        let err = parse_manifest(&value).expect_err("missing entrypoint must error");
        assert_eq!(
            err,
            ManifestError::MissingField {
                field: "entrypoint".to_string()
            }
        );
    }

    #[test]
    fn non_builtin_entrypoint_is_rejected() {
        let value = json!({
            "kind": "extension",
            "schemaVersion": "1",
            "name": "wasm-ext",
            "displayName": "Wasm",
            "version": "0.1.0",
            "entrypoint": "wasm"
        });
        let err = parse_manifest(&value).expect_err("wasm entrypoint is Phase 2");
        assert_eq!(
            err,
            ManifestError::InvalidEntrypoint {
                value: "wasm".to_string()
            }
        );
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let value = json!({ "kind": "widget", "schemaVersion": "1", "name": "x" });
        let err = parse_manifest(&value).expect_err("unknown kind");
        assert_eq!(
            err,
            ManifestError::InvalidKind {
                value: "widget".to_string()
            }
        );
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let value = json!({ "kind": "prompt", "schemaVersion": "2", "name": "x" });
        let err = parse_manifest(&value).expect_err("schema v2 unsupported");
        assert_eq!(
            err,
            ManifestError::UnsupportedSchemaVersion {
                value: "2".to_string()
            }
        );
    }

    #[test]
    fn missing_kind_is_rejected() {
        let value = json!({ "schemaVersion": "1", "name": "x" });
        let err = parse_manifest(&value).expect_err("missing kind");
        assert_eq!(
            err,
            ManifestError::MissingField {
                field: "kind".to_string()
            }
        );
    }
}

// Rust guideline compliant 2026-02-21

//! SKILL.md front-matter parser and manifest constructor.
//!
//! A `SKILL.md` file uses YAML front-matter (delimited by `---` fences)
//! followed by a Markdown body.  [`load`] reads the file, splits the fences,
//! deserializes the YAML into a loader-local [`SkillFrontmatter`] DTO, and
//! constructs a fully validated [`SkillManifest`] from it.
//!
//! The distinction between the on-disk DTO and the proto types is intentional:
//! `SKILL.md` uses kebab-case keys and an inverted `disable-model-invocation`
//! flag that cannot map to the proto wire schema without a translation layer.
//!
//! # Frontmatter rules
//!
//! * The file **must** begin with a line that is exactly `---` (after
//!   stripping a trailing `\r`); missing fences produce
//!   [`SkillError::MissingFrontmatter`].
//! * `name` must be non-empty and match `^[a-z0-9-]{1,64}$`.
//! * `description` must not exceed 1024 **characters** (Unicode scalar values).
//! * `allowed-tools` accepts either a YAML sequence or a comma-separated
//!   string (e.g. `Read, Grep`).
//! * `disable-model-invocation: true` maps to `model_invocable = false`;
//!   absent or `false` maps to `model_invocable = true`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Deserializer};
use zhive_proto::manifest::{ManifestError, SkillManifest};

use crate::skills::error::SkillError;

// ============================================================
// LoadedSkill
// ============================================================

/// A fully parsed and validated skill ready for use.
///
/// Returned by [`load`]; held inside [`super::SkillSet`].
///
/// The fields exposed here are the authoritative, correctly-typed values
/// sourced directly from the parsed frontmatter — they do not go through
/// a `Manifest` JSON round-trip, which would corrupt the `spec` variant
/// due to `ManifestSpec` being `#[serde(untagged)]` with
/// `PromptManifest` matching first (all-default fields).
#[derive(Debug, Clone)]
pub struct LoadedSkill {
    /// Unique skill identifier; matches the frontmatter `name` field.
    pub name: String,
    /// Optional model-facing description from the frontmatter.
    pub description: Option<String>,
    /// Validated skill-specific manifest fields.
    pub skill: SkillManifest,
    /// The Markdown body of the `SKILL.md` file (after the closing `---`).
    pub body: Arc<str>,
    /// The directory that contains the `SKILL.md` file.
    ///
    /// Used to resolve relative paths to bundled resource files.
    pub root: PathBuf,
}

// ============================================================
// Frontmatter DTO
// ============================================================

/// Loader-local DTO for SKILL.md YAML front-matter.
///
/// Uses kebab-case serde aliases and accepts the `allowed-tools`
/// field in both string and sequence form.  After deserialization the
/// loader translates this into a [`SkillManifest`].
#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    /// Required skill identifier.
    #[serde(default)]
    name: String,

    /// Optional model-facing description.
    #[serde(default)]
    description: Option<String>,

    /// Optional allowlist; accepted as a comma-string or YAML sequence.
    #[serde(
        default,
        rename = "allowed-tools",
        deserialize_with = "deserialize_allowed_tools"
    )]
    allowed_tools: Option<Vec<String>>,

    /// `true` means slash-only (`model_invocable` = false).
    ///
    /// Absent defaults to `false` which means `model_invocable = true`.
    #[serde(default, rename = "disable-model-invocation")]
    disable_model_invocation: bool,

    /// Keywords that hint the model to invoke this skill automatically.
    #[serde(default, rename = "auto-invoke-keywords")]
    auto_invoke_keywords: Option<Vec<String>>,

    /// Hide this skill inside subagent threads.
    #[serde(default, rename = "disable-in-subagent")]
    disable_in_subagent: bool,
}

// ============================================================
// Custom deserializer: comma-string OR sequence
// ============================================================

/// Deserializes `allowed-tools` as either `"Read, Grep"` or `["Read","Grep"]`.
fn deserialize_allowed_tools<'de, D>(d: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }

    let v: Option<OneOrMany> = Option::deserialize(d)?;
    Ok(v.map(|inner| match inner {
        OneOrMany::Many(list) => list,
        OneOrMany::One(s) => s
            .split(',')
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .collect(),
    }))
}

// ============================================================
// Name validation regex
// ============================================================

/// Pattern a valid skill `name` must match: `^[a-z0-9-]{1,64}$`.
///
/// Length and allowed characters are the same constraints Claude Code
/// applies; enforced here so the registry key is safe for downstream use.
fn is_valid_skill_name(name: &str) -> bool {
    // 1–64 chars, lowercase ASCII letters, digits, and hyphens only.
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

// ============================================================
// Public loader
// ============================================================

/// Reads, parses, and validates a `SKILL.md` file into a [`LoadedSkill`].
///
/// # Errors
///
/// * [`SkillError::Io`] — file could not be read.
/// * [`SkillError::MissingFrontmatter`] — file does not start with a `---` fence.
/// * [`SkillError::Yaml`] — YAML inside the fences failed to parse.
/// * [`SkillError::Manifest`] — schema-level constraint violated
///   (missing / invalid `name`, `description` too long, etc.).
///
/// # Examples
///
/// ```no_run
/// use zhive_core::skills::loader::load;
/// use std::path::Path;
///
/// # fn try_load() -> Result<(), Box<dyn std::error::Error>> {
/// let skill = load(Path::new("/home/user/.config/zhive/skills/my-skill/SKILL.md"))?;
/// assert_eq!(skill.name, "my-skill");
/// # Ok(())
/// # }
/// ```
pub fn load(path: &Path) -> Result<LoadedSkill, SkillError> {
    let source = std::fs::read_to_string(path).map_err(|e| SkillError::Io {
        path: path.to_owned(),
        reason: e.to_string(),
    })?;

    let (yaml_block, body) = split_frontmatter(&source, path)?;

    let fm: SkillFrontmatter =
        serde_norway::from_str(yaml_block).map_err(|e| SkillError::Yaml {
            path: path.to_owned(),
            reason: e.to_string(),
        })?;

    validate_and_build(&fm, body, path)
}

// ============================================================
// Frontmatter split
// ============================================================

/// Splits a `SKILL.md` source string into `(yaml_block, markdown_body)`.
///
/// Scans line by line so that a `----` line, a `--- comment` line, or a
/// `---` appearing mid-line does not accidentally match the fence.  Both
/// LF (`\n`) and CRLF (`\r\n`) line endings are handled: each line is
/// trimmed of a trailing `\r` before comparison.
///
/// The opening fence is the **first** line whose trimmed content is
/// exactly `---`.  The closing fence is the **first subsequent** line
/// with the same trimmed content.  The returned body does not contain
/// either fence line.
fn split_frontmatter<'a>(source: &'a str, path: &Path) -> Result<(&'a str, &'a str), SkillError> {
    // Split into individual lines while tracking byte offsets so we can
    // return sub-slices of the original string (avoiding allocations).
    let mut lines = source.splitn(usize::MAX, '\n').peekable();
    let mut byte_offset: usize = 0;

    // --- Locate opening fence ---
    let first = lines.next().ok_or_else(|| SkillError::MissingFrontmatter {
        path: path.to_owned(),
    })?;

    // Advance byte offset past the first line plus its '\n'.
    byte_offset += first.len() + 1; // +1 for the '\n' separator

    // The opening fence line must be exactly `---` (after stripping `\r`).
    if first.trim_end_matches('\r') != "---" {
        return Err(SkillError::MissingFrontmatter {
            path: path.to_owned(),
        });
    }

    // Record where the YAML block starts (right after the opening fence line).
    let yaml_start = byte_offset;

    // --- Locate closing fence ---
    for line in &mut lines {
        if line.trim_end_matches('\r') == "---" {
            // `byte_offset` now points to the start of this closing-fence line.
            // The YAML block is everything between yaml_start and here.
            let yaml_block = &source[yaml_start..byte_offset];

            // The body starts right after the closing fence line and its '\n'.
            let body_start = byte_offset + line.len() + 1;
            let body = if body_start <= source.len() {
                &source[body_start..]
            } else {
                ""
            };

            return Ok((yaml_block, body));
        }
        byte_offset += line.len() + 1; // +1 for the '\n' separator
    }

    // Closing fence was never found.
    Err(SkillError::MissingFrontmatter {
        path: path.to_owned(),
    })
}

// ============================================================
// Validation + manifest construction
// ============================================================

fn validate_and_build(
    fm: &SkillFrontmatter,
    body: &str,
    path: &Path,
) -> Result<LoadedSkill, SkillError> {
    // `name` is required and must match the allowed pattern.
    if fm.name.is_empty() {
        return Err(SkillError::Manifest {
            path: path.to_owned(),
            source: ManifestError::MissingField {
                field: "name".into(),
            },
        });
    }
    if !is_valid_skill_name(&fm.name) {
        return Err(SkillError::Manifest {
            path: path.to_owned(),
            source: ManifestError::InvalidKind {
                // Reuse InvalidKind as the closest match for a bad name; the
                // error message carries the offending value.
                value: fm.name.clone(),
            },
        });
    }

    // `description` must not exceed 1024 **characters** (Unicode scalar values),
    // not bytes.  Multibyte characters (CJK, emoji) are counted as one each.
    if let Some(ref desc) = fm.description
        && desc.chars().count() > 1024
    {
        return Err(SkillError::Manifest {
            path: path.to_owned(),
            source: ManifestError::ToolDescriptionTooLong {
                tool: fm.name.clone(),
            },
        });
    }

    // `SkillManifest` is `#[non_exhaustive]`, so it cannot be constructed via
    // struct literal syntax from outside `zhive-proto`.  We use a serde_json
    // round-trip to construct it: the schema is trivial and this is a cold,
    // one-per-skill path.
    let skill_manifest: SkillManifest = serde_json::from_value(serde_json::json!({
        "modelInvocable": !fm.disable_model_invocation,
        "allowedTools": fm.allowed_tools,
        "autoInvokeKeywords": fm.auto_invoke_keywords,
        "disableInSubagent": fm.disable_in_subagent,
    }))
    .map_err(|e| SkillError::Manifest {
        path: path.to_owned(),
        source: ManifestError::InvalidSchema {
            field: "skill_manifest".into(),
            reason: e.to_string(),
        },
    })?;

    let root = path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_owned);

    Ok(LoadedSkill {
        name: fm.name.clone(),
        description: fm.description.clone(),
        skill: skill_manifest,
        body: Arc::from(body),
        root,
    })
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_skill(dir: &Path, content: &str) -> PathBuf {
        let skill_dir = dir.join("test-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn basic_frontmatter_parsed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content = "---\nname: my-skill\ndescription: Does things\n---\n\n# Body text\n";
        let path = write_skill(tmp.path(), content);

        let loaded = load(&path).expect("load should succeed");
        // Finding 1: use new flat fields (not loaded.manifest.name)
        assert_eq!(loaded.name, "my-skill");
        assert_eq!(loaded.description.as_deref(), Some("Does things"));
    }

    #[test]
    fn body_is_everything_after_closing_fence() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content = "---\nname: body-skill\ndescription: x\n---\n\nHello world\n";
        let path = write_skill(tmp.path(), content);

        let loaded = load(&path).unwrap();
        assert!(
            loaded.body.contains("Hello world"),
            "body: {:?}",
            loaded.body
        );
        assert!(!loaded.body.contains("---"));
    }

    #[test]
    fn allowed_tools_comma_string() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content =
            "---\nname: tools-skill\ndescription: x\nallowed-tools: Read, Grep\n---\n\nbody\n";
        let path = write_skill(tmp.path(), content);

        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.skill.allowed_tools,
            Some(vec!["Read".to_owned(), "Grep".to_owned()])
        );
    }

    #[test]
    fn allowed_tools_yaml_list() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content = "---\nname: list-skill\ndescription: x\nallowed-tools:\n  - Read\n  - Write\n---\n\nbody\n";
        let path = write_skill(tmp.path(), content);

        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.skill.allowed_tools,
            Some(vec!["Read".to_owned(), "Write".to_owned()])
        );
    }

    #[test]
    fn disable_model_invocation_true_sets_not_invocable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content =
            "---\nname: slash-skill\ndescription: x\ndisable-model-invocation: true\n---\n\nbody\n";
        let path = write_skill(tmp.path(), content);

        let loaded = load(&path).unwrap();
        assert!(!loaded.skill.model_invocable);
    }

    #[test]
    fn disable_model_invocation_absent_means_invocable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content = "---\nname: model-skill\ndescription: x\n---\n\nbody\n";
        let path = write_skill(tmp.path(), content);

        let loaded = load(&path).unwrap();
        assert!(loaded.skill.model_invocable);
    }

    #[test]
    fn missing_frontmatter_returns_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        // No leading `---`.
        let content = "Just some markdown without frontmatter.\n";
        let path = write_skill(tmp.path(), content);

        let err = load(&path).unwrap_err();
        assert!(
            matches!(err, SkillError::MissingFrontmatter { .. }),
            "expected MissingFrontmatter, got {err:?}"
        );
    }

    #[test]
    fn description_over_1024_chars_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let long_desc = "x".repeat(1025);
        let content = format!("---\nname: long-skill\ndescription: {long_desc}\n---\n\nbody\n");
        let path = write_skill(tmp.path(), &content);

        let err = load(&path).unwrap_err();
        assert!(
            matches!(
                err,
                SkillError::Manifest {
                    source: ManifestError::ToolDescriptionTooLong { .. },
                    ..
                }
            ),
            "expected ToolDescriptionTooLong, got {err:?}"
        );
    }

    #[test]
    fn missing_name_field_returns_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content = "---\ndescription: no name here\n---\n\nbody\n";
        let path = write_skill(tmp.path(), content);

        let err = load(&path).unwrap_err();
        assert!(
            matches!(
                err,
                SkillError::Manifest {
                    source: ManifestError::MissingField { .. },
                    ..
                }
            ),
            "expected MissingField, got {err:?}"
        );
    }

    #[test]
    fn skill_manifest_fields_are_correct() {
        // Regression for finding 1: ensure the loaded skill data is
        // sourced directly from the frontmatter, not from a round-tripped
        // `Manifest` JSON (which would misidentify the spec as `Prompt`
        // due to `ManifestSpec` being `#[serde(untagged)]`).
        let tmp = tempfile::TempDir::new().unwrap();
        let content = "---\nname: kw-skill\ndescription: Has keywords\nauto-invoke-keywords:\n  - deploy\n  - release\n---\n\nbody\n";
        let path = write_skill(tmp.path(), content);

        let loaded = load(&path).unwrap();
        // These come from `loaded.skill` which is a `SkillManifest`, not a
        // round-tripped `Manifest.spec` that would silently drop the keywords.
        assert_eq!(loaded.name, "kw-skill");
        assert_eq!(loaded.description.as_deref(), Some("Has keywords"));
        assert_eq!(
            loaded.skill.auto_invoke_keywords,
            Some(vec!["deploy".to_owned(), "release".to_owned()]),
            "auto_invoke_keywords must be preserved on LoadedSkill.skill"
        );
        assert!(
            loaded.skill.model_invocable,
            "skill should be model-invocable by default"
        );
    }

    /// Finding 2 + 3 regression: a body line that is `----` (four dashes)
    /// must NOT be treated as the closing fence; only an exact `---` line
    /// (after trimming a trailing `\r`) qualifies.
    #[test]
    fn fence_not_matched_by_four_dash_line() {
        let tmp = tempfile::TempDir::new().unwrap();
        // The body contains `----` which the old substring search `find("\n---")`
        // would match as the closing fence, corrupting the YAML block.
        let content = "---\nname: dash-skill\ndescription: x\n---\n\n----\n\nReal body\n";
        let path = write_skill(tmp.path(), content);

        let loaded = load(&path).expect("should load successfully");
        assert_eq!(loaded.name, "dash-skill");
        assert!(
            loaded.body.contains("----"),
            "four-dash line must appear in the body; got: {:?}",
            loaded.body
        );
        assert!(
            loaded.body.contains("Real body"),
            "body should contain content after the four-dash line; got: {:?}",
            loaded.body
        );
    }

    /// Finding 2 regression: a `--- comment` line in the body must NOT
    /// be treated as the closing fence.
    #[test]
    fn fence_not_matched_by_dash_comment_line() {
        let tmp = tempfile::TempDir::new().unwrap();
        let content =
            "---\nname: comment-skill\ndescription: x\n---\n\n--- not a fence\n\nBody here\n";
        let path = write_skill(tmp.path(), content);

        let loaded = load(&path).expect("should load successfully");
        assert_eq!(loaded.name, "comment-skill");
        assert!(
            loaded.body.contains("--- not a fence"),
            "body should contain the non-fence dash-comment line; got: {:?}",
            loaded.body
        );
    }

    /// Finding 3 regression: a `SKILL.md` with CRLF line endings must
    /// parse correctly.  The body should not contain stray `\r` artifacts.
    #[test]
    fn crlf_skill_md_parses_correctly() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Manually construct CRLF content.
        let content = "---\r\nname: crlf-skill\r\ndescription: Windows line endings\r\n---\r\n\r\nBody content here\r\n";
        let path = write_skill(tmp.path(), content);

        let loaded = load(&path).expect("CRLF SKILL.md should parse without error");
        assert_eq!(loaded.name, "crlf-skill");
        assert_eq!(
            loaded.description.as_deref(),
            Some("Windows line endings"),
            "description must be parsed correctly from CRLF file"
        );
        assert!(
            loaded.body.contains("Body content here"),
            "body should contain body text; got: {:?}",
            loaded.body
        );
    }

    /// Finding 5 regression: a description that is exactly 1024 **characters**
    /// but more than 1024 **bytes** (due to multibyte CJK characters) must
    /// be accepted.
    #[test]
    fn multibyte_description_under_1024_chars_is_accepted() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Each '中' is 3 UTF-8 bytes; 1024 of them = 3072 bytes > 1024,
        // but 1024 chars == 1024 which should be allowed.
        let desc_1024_chars = "中".repeat(1024);
        assert_eq!(desc_1024_chars.chars().count(), 1024);
        assert!(
            desc_1024_chars.len() > 1024,
            "sanity: byte count exceeds 1024"
        );

        let content =
            format!("---\nname: cjk-skill\ndescription: \"{desc_1024_chars}\"\n---\n\nbody\n");
        let path = write_skill(tmp.path(), &content);

        let loaded = load(&path).expect("1024-char multibyte description must be accepted");
        assert_eq!(loaded.name, "cjk-skill");
        assert_eq!(
            loaded.description.as_ref().map(|d| d.chars().count()),
            Some(1024)
        );
    }

    /// Finding 5 boundary: a description with 1025 multibyte characters must
    /// be rejected.
    #[test]
    fn multibyte_description_over_1024_chars_is_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let long_desc = "中".repeat(1025);
        let content = format!("---\nname: cjk-long\ndescription: \"{long_desc}\"\n---\n\nbody\n");
        let path = write_skill(tmp.path(), &content);

        let err = load(&path).unwrap_err();
        assert!(
            matches!(
                err,
                SkillError::Manifest {
                    source: ManifestError::ToolDescriptionTooLong { .. },
                    ..
                }
            ),
            "expected ToolDescriptionTooLong for 1025-char description, got {err:?}"
        );
    }
}

// Rust guideline compliant 2026-02-21

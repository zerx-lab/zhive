//! [`SkillTool`]: a [`Tool`] adapter that delivers a skill's instructions.
//!
//! When the model invokes a skill tool, [`SkillTool::execute`] returns the
//! skill's full Markdown body as [`ToolOutput`] text.  This implements the
//! *progressive disclosure* pattern: the model sees only the skill's name
//! and description in the advertised tool list; the full instructions arrive
//! only when the model calls the tool.
//!
//! Bundled resource files are returned as relative path *pointers*, not
//! inlined content, so large assets do not bloat every invocation.
//!
//! # Lifetime of the name
//!
//! [`Tool::name`] returns `&str` borrowed from `&self`.  `SkillTool` stores
//! the skill name as a plain `String` field and returns a reference to it,
//! avoiding any heap leaking.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::{Tool, ToolContext, ToolError, ToolKind, ToolOutput};

use super::loader::LoadedSkill;

// ============================================================
// SkillTool
// ============================================================

/// A [`Tool`] that delivers a skill's Markdown instructions on invocation.
///
/// Constructed via [`SkillTool::from_loaded`]; registered into a
/// [`crate::tools::ToolRegistry`] for model-invocable skills only.
///
/// # Examples
///
/// ```
/// use std::fs;
/// use zhive_core::skills::{loader::load, tool::SkillTool};
/// use zhive_core::tools::Tool;
///
/// let tmp = tempfile::TempDir::new().unwrap();
/// let skill_dir = tmp.path().join("my-skill");
/// fs::create_dir_all(&skill_dir).unwrap();
/// fs::write(skill_dir.join("SKILL.md"),
///     "---\nname: my-skill\ndescription: Does things\n---\n\n## Body\n"
/// ).unwrap();
/// let loaded = load(&skill_dir.join("SKILL.md")).unwrap();
/// let tool = SkillTool::from_loaded(loaded);
/// assert_eq!(tool.name(), "my-skill");
/// ```
#[derive(Debug, Clone)]
pub struct SkillTool {
    /// Stable tool name; matches the manifest `name` field.
    name: String,
    /// Natural-language description advertised to the model.
    description: Option<String>,
    /// The full Markdown body returned on invocation.
    body: Arc<str>,
    /// Relative paths of bundled resource files under [`root`](SkillTool).
    resource_pointers: Vec<String>,
    /// Whether this skill can be invoked by the model (always `true` for a
    /// registered `SkillTool`; stored for informational metadata).
    model_invocable: bool,
    /// Optional tool allowlist surfaced in execution metadata.
    allowed_tools: Option<Vec<String>>,
    /// Whether to hide this skill from subagent threads.
    disable_in_subagent: bool,
}

impl SkillTool {
    /// Constructs a [`SkillTool`] from a [`LoadedSkill`].
    ///
    /// Bundled resources are discovered by listing the skill root directory
    /// for files other than `SKILL.md`; they are returned as pointers (relative
    /// paths) in the execution output rather than being inlined.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::fs;
    /// use zhive_core::skills::{loader::load, tool::SkillTool};
    /// use zhive_core::tools::Tool;
    ///
    /// let tmp = tempfile::TempDir::new().unwrap();
    /// let skill_dir = tmp.path().join("demo-skill");
    /// fs::create_dir_all(&skill_dir).unwrap();
    /// fs::write(skill_dir.join("SKILL.md"),
    ///     "---\nname: demo-skill\ndescription: x\n---\n\nbody text"
    /// ).unwrap();
    /// let loaded = load(&skill_dir.join("SKILL.md")).unwrap();
    /// let tool = SkillTool::from_loaded(loaded);
    /// assert_eq!(tool.name(), "demo-skill");
    /// ```
    #[must_use]
    pub fn from_loaded(loaded: LoadedSkill) -> Self {
        let resource_pointers = collect_resource_pointers(&loaded.root);

        Self {
            name: loaded.name,
            description: loaded.description,
            body: loaded.body,
            resource_pointers,
            model_invocable: loaded.skill.model_invocable,
            allowed_tools: loaded.skill.allowed_tools,
            disable_in_subagent: loaded.skill.disable_in_subagent,
        }
    }

    /// Whether this skill can be invoked by the model.
    #[must_use]
    pub fn model_invocable(&self) -> bool {
        self.model_invocable
    }

    /// Whether this skill should be hidden from subagent threads.
    #[must_use]
    pub fn disable_in_subagent(&self) -> bool {
        self.disable_in_subagent
    }

    /// The optional tool allowlist declared in the skill manifest.
    #[must_use]
    pub fn allowed_tools(&self) -> Option<&[String]> {
        self.allowed_tools.as_deref()
    }
}

// ============================================================
// Tool impl
// ============================================================

#[async_trait]
impl Tool for SkillTool {
    /// Returns the skill's name (e.g. `"my-skill"`).
    fn name(&self) -> &str {
        &self.name
    }

    /// Returns `false` when the manifest sets `disable_in_subagent`.
    ///
    /// When `false`, the engine excludes this skill from the child scope's
    /// disallowed-tool override list, so subagent threads cannot invoke it.
    fn available_in_subagent(&self) -> bool {
        !self.disable_in_subagent
    }

    /// Always [`ToolKind::Other`]; skills are not file-system operations.
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn description(&self) -> Option<String> {
        self.description.clone()
    }

    /// Permissive object schema with an optional free-form `input` field.
    ///
    /// The `input` field lets the model pass a context hint that is included
    /// in the structured execution metadata returned alongside the body.
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "Optional context or argument for the skill."
                }
            },
            "additionalProperties": false
        })
    }

    /// Returns the skill's full Markdown body as [`ToolOutput`] text.
    ///
    /// Bundled resource files are appended as a Markdown list of relative
    /// paths so the model can request them via a file-read tool.  The
    /// structured `value` carries invocation metadata for richer clients.
    ///
    /// # Errors
    ///
    /// This implementation is infallible; it always returns `Ok`.
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let mut text = self.body.to_string();

        if !self.resource_pointers.is_empty() {
            use std::fmt::Write as _;
            text.push_str("\n\n## Bundled resources (read on demand)\n");
            for ptr in &self.resource_pointers {
                // Infallible: writing to a `String` never returns `Err`.
                let _ = writeln!(text, "- {ptr}");
            }
        }

        let value = serde_json::json!({
            "skill": self.name,
            "allowed_tools": self.allowed_tools,
            "args": args,
        });

        Ok(ToolOutput::with_value(text, value))
    }
}

// ============================================================
// Resource pointer discovery
// ============================================================

/// Lists every bundled file under `root` (except the top-level `SKILL.md`) as a
/// relative-path string, recursing into subdirectories.
///
/// Nested resources (e.g. `scripts/helper.py`) are reported with their
/// subdirectory prefix so the model can reference them directly instead of
/// guessing the layout. Returns an empty `Vec` when the directory cannot be
/// read, so the absence of bundled resources is never a hard error.
///
/// Symlinks are skipped (never followed) and recursion is depth-bounded, so a
/// cyclic or adversarial tree cannot cause unbounded work.
fn collect_resource_pointers(root: &std::path::Path) -> Vec<String> {
    /// Maximum directory depth walked for bundled skill resources.
    ///
    /// Skill bundles are shallow by convention; this bounds work on a malformed
    /// or adversarial tree without truncating any realistic layout.
    const MAX_DEPTH: usize = 8;
    /// Maximum number of resource pointers collected per skill.
    ///
    /// Caps the prompt budget a single skill can consume; bundles larger than
    /// this are unusual and the overflow is silently dropped.
    const MAX_POINTERS: usize = 500;

    let mut pointers: Vec<String> = Vec::new();
    // Iterative DFS so a deep tree cannot blow the stack.
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if pointers.len() >= MAX_POINTERS {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // `file_type()` does not follow symlinks, so a symlinked directory
            // reports neither `is_dir()` nor `is_file()` and is skipped — this
            // is what prevents symlink cycles from causing unbounded recursion.
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                if depth + 1 < MAX_DEPTH {
                    stack.push((path, depth + 1));
                }
            } else if ft.is_file() {
                let Ok(rel) = path.strip_prefix(root) else {
                    continue;
                };
                // Skip the skill's own manifest at the bundle root.
                if depth == 0 && rel.as_os_str() == "SKILL.md" {
                    continue;
                }
                pointers.push(rel.to_string_lossy().into_owned());
                if pointers.len() >= MAX_POINTERS {
                    break;
                }
            }
        }
    }

    pointers.sort();
    pointers
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tokio_util::sync::CancellationToken;
    use zhive_proto::domain::{ThreadId, TurnId};

    fn test_ctx() -> ToolContext {
        ToolContext {
            thread_id: ThreadId(Arc::from("thread:native/test")),
            turn_id: TurnId(Arc::from("turn:thread:native/test/0")),
            cancel: CancellationToken::new(),
            spawner: None,
            workspace_root: None,
        }
    }

    /// Writes a `SKILL.md` into a temp dir and loads it via the production
    /// loader so tests do not need to construct `#[non_exhaustive]` proto types.
    fn make_loaded(
        name: &str,
        body: &str,
        model_invocable: bool,
        root: &std::path::Path,
    ) -> LoadedSkill {
        let disable_flag = if model_invocable {
            String::new()
        } else {
            "disable-model-invocation: true\n".to_owned()
        };
        let content = format!(
            "---\nname: {name}\ndescription: Description of {name}\n{disable_flag}---\n\n{body}"
        );
        let skill_dir = root.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        fs::write(&skill_md, content).unwrap();
        super::super::loader::load(&skill_md).expect("test skill should load")
    }

    #[tokio::test]
    async fn execute_returns_skill_body() {
        let tmp = tempfile::TempDir::new().unwrap();
        let loaded = make_loaded(
            "my-skill",
            "## Instructions\n\nDo the thing.",
            true,
            tmp.path(),
        );
        let tool = SkillTool::from_loaded(loaded);

        let out = tool
            .execute(serde_json::json!({}), &test_ctx())
            .await
            .unwrap();

        assert!(
            out.text.contains("Do the thing."),
            "expected body in output; got: {:?}",
            out.text
        );
        assert_eq!(tool.name(), "my-skill");
    }

    #[test]
    fn collect_resource_pointers_recurses_subdirs_and_skips_manifest() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("SKILL.md"), "manifest").unwrap();
        fs::write(root.join("notes.md"), "top-level resource").unwrap();
        fs::create_dir_all(root.join("scripts")).unwrap();
        fs::write(root.join("scripts").join("helper.py"), "print('hi')").unwrap();

        let ptrs = collect_resource_pointers(root);

        assert!(ptrs.contains(&"notes.md".to_owned()), "got {ptrs:?}");
        assert!(
            ptrs.contains(&format!("scripts{}helper.py", std::path::MAIN_SEPARATOR)),
            "nested resource must keep its subdir prefix; got {ptrs:?}"
        );
        assert!(
            !ptrs.iter().any(|p| p.ends_with("SKILL.md")),
            "the manifest must be excluded; got {ptrs:?}"
        );
    }

    #[test]
    fn name_returns_skill_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let loaded = make_loaded("demo-skill", "body", true, tmp.path());
        let tool = SkillTool::from_loaded(loaded);
        assert_eq!(tool.name(), "demo-skill");
    }

    #[test]
    fn description_forwarded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let loaded = make_loaded("desc-skill", "body", true, tmp.path());
        let tool = SkillTool::from_loaded(loaded);
        assert!(tool.description().is_some());
    }

    #[test]
    fn input_schema_is_object() {
        let tmp = tempfile::TempDir::new().unwrap();
        let loaded = make_loaded("schema-skill", "body", true, tmp.path());
        let tool = SkillTool::from_loaded(loaded);
        assert_eq!(tool.input_schema()["type"], "object");
    }

    /// A skill loaded without `disable_in_subagent: true` is available in
    /// subagents; one with the flag set is excluded.
    #[test]
    fn available_in_subagent_reflects_manifest_flag() {
        use crate::tools::Tool as _;

        let tmp = tempfile::TempDir::new().unwrap();

        // Without the flag → available_in_subagent() == true.
        let loaded = make_loaded("normal-skill", "body", true, tmp.path());
        let tool = SkillTool::from_loaded(loaded);
        assert!(
            tool.available_in_subagent(),
            "skill without disable_in_subagent must be available in subagents"
        );

        // With the flag → available_in_subagent() == false.
        // Write the SKILL.md manually so we can set `disable_in_subagent: true`.
        let skill_dir = tmp.path().join("guarded-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: guarded-skill\ndescription: x\ndisable-in-subagent: true\n---\n\nbody",
        )
        .unwrap();
        let loaded2 = super::super::loader::load(&skill_dir.join("SKILL.md")).expect("should load");
        let tool2 = SkillTool::from_loaded(loaded2);
        assert!(
            !tool2.available_in_subagent(),
            "skill with disable_in_subagent must NOT be available in subagents"
        );
    }

    #[tokio::test]
    async fn bundled_resources_appear_in_output() {
        let tmp = tempfile::TempDir::new().unwrap();
        // `make_loaded` creates `tmp/<name>/SKILL.md`; the bundled resource
        // must sit alongside SKILL.md in that same directory.
        let skill_dir = tmp.path().join("resource-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("template.txt"), "a template").unwrap();

        let loaded = make_loaded("resource-skill", "## Skill body", true, tmp.path());
        let tool = SkillTool::from_loaded(loaded);

        let out = tool
            .execute(serde_json::json!({}), &test_ctx())
            .await
            .unwrap();

        assert!(
            out.text.contains("template.txt"),
            "expected resource pointer in output; got: {:?}",
            out.text
        );
    }

    #[tokio::test]
    async fn execute_value_contains_skill_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let loaded = make_loaded("meta-skill", "body", true, tmp.path());
        let tool = SkillTool::from_loaded(loaded);

        let out = tool
            .execute(serde_json::json!({"input": "hint"}), &test_ctx())
            .await
            .unwrap();

        let val = out.value.expect("value should be present");
        assert_eq!(val["skill"], "meta-skill");
        assert_eq!(val["args"]["input"], "hint");
    }
}

// Rust guideline compliant 2026-02-21

//! Renders host-side prompts from `.j2` (Jinja2) templates via minijinja.
//!
//! The engine ([`zhive_core`]) is prompt-agnostic: it consumes whatever opaque
//! text the host hands it. This module owns the *rendering* of that text from
//! editable Jinja2 templates instead of hardcoded Rust string constants, so a
//! prompt can be tweaked, extended, or swapped per provider without touching
//! Rust. The assembled prompt text itself is shaped by [`crate::system_prompt`].
//!
//! # Template resolution
//!
//! Templates are addressed by name (the relative path under the templates root
//! with the `.j2` suffix removed, e.g. `system/base`, `system/persona`). Each
//! name resolves through two layers, with later layers overriding earlier ones:
//!
//! 1. **Embedded default** — compiled into the binary via [`include_str!`], so a
//!    default is always present and a render can never fail for "file missing".
//! 2. **On-disk override** — a `<name>.j2` file under a project-local root
//!    (`./.zhive/templates`) or the user config root
//!    (`$XDG_CONFIG_HOME/zhive/templates`, falling back to
//!    `~/.config/zhive/templates`). The project root wins over the user root,
//!    and either wins over the embedded default.
//!
//! A read error or a syntactically broken override degrades gracefully: the
//! failure is logged and the caller retries with overrides disabled (see
//! [`crate::system_prompt::assemble`]), so a bad template never aborts a turn.
//!
//! # Context contract
//!
//! [`PromptContext`] is the data exposed to a template. Fields are serialized
//! via serde and become top-level Jinja variables (`{{ cwd }}`, `{{ os }}`, …).
//! [`render_system`] additionally injects `persona_template`, the resolved name
//! of the persona partial chosen for the active provider.

use std::path::PathBuf;

use serde::Serialize;

/// Default `system/base` template, embedded at compile time.
const SYSTEM_BASE: &str = include_str!("../templates/system/base.j2");

/// Default `system/persona` template, embedded at compile time.
const SYSTEM_PERSONA: &str = include_str!("../templates/system/persona.j2");

/// Default `compaction/summary` template, embedded at compile time.
const COMPACTION_SUMMARY: &str = include_str!("../templates/compaction/summary.j2");

/// Persona text used as a last-resort fallback when rendering fails entirely.
///
/// Equal to the embedded default persona; returned by
/// [`crate::system_prompt::assemble`] only if both the override and the
/// embedded render error out, so the engine always receives a usable prompt.
pub(crate) const FALLBACK_PERSONA: &str = SYSTEM_PERSONA;

/// File extension used for on-disk template overrides.
const TEMPLATE_EXTENSION: &str = "j2";

/// Errors that occur while loading or rendering a prompt template.
///
/// `Display` renders a one-line summary; the underlying minijinja error is
/// exposed via [`std::error::Error::source`]. Implemented by hand rather than
/// via `thiserror` because `zhive-cli` is an application crate (it carries no
/// `thiserror` dependency).
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum TemplateError {
    /// A template could not be found, read, or parsed.
    Load {
        /// Logical template name (e.g. `system/base`).
        name: String,
        /// Underlying minijinja error.
        source: minijinja::Error,
    },
    /// A template failed to render with the provided context.
    Render {
        /// Logical template name (e.g. `system/base`).
        name: String,
        /// Underlying minijinja error.
        source: minijinja::Error,
    },
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Load { name, .. } => write!(f, "failed to load prompt template `{name}`"),
            Self::Render { name, .. } => write!(f, "failed to render prompt template `{name}`"),
        }
    }
}

impl std::error::Error for TemplateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Load { source, .. } | Self::Render { source, .. } => Some(source),
        }
    }
}

/// Data exposed to a system-prompt template as top-level Jinja variables.
///
/// Each field serializes to a variable of the same name. `provider_name` is the
/// user-chosen entry name (e.g. `my-proxy`) and `provider_kind` is the backend
/// type (e.g. `openai`); the latter is more stable for per-provider branching.
#[derive(Debug, Serialize)]
pub(crate) struct PromptContext {
    /// Working directory, as displayed (`{{ cwd }}`).
    pub cwd: String,
    /// Host operating system (`{{ os }}`), from `std::env::consts::OS`.
    pub os: String,
    /// Active provider entry name (`{{ provider_name }}`).
    pub provider_name: String,
    /// Active provider backend kind (`{{ provider_kind }}`).
    pub provider_kind: String,
    /// Active model id, if known (`{{ model }}`).
    pub model: Option<String>,
    /// Nearest project instruction file, if any (`{{ project_instructions }}`).
    pub project_instructions: Option<ProjectInstructions>,
    /// Rendered `<available_skills>` catalogue for discovered skills, if any
    /// (`{{ skills }}`). `None` omits the skills section entirely.
    pub skills: Option<String>,
}

/// The nearest project instruction file folded into the system prompt.
#[derive(Debug, Serialize)]
pub(crate) struct ProjectInstructions {
    /// Source path the instructions were read from.
    pub source: String,
    /// Instruction body, already truncated to the host byte budget.
    pub body: String,
    /// Whether `body` was truncated (informational; the marker is in `body`).
    pub truncated: bool,
}

/// Renders the `system/base` template into the final system prompt text.
///
/// Builds a fresh minijinja environment (cheap; called at most twice per
/// session), resolves the persona partial for the active provider, and renders
/// `system/base` with `ctx` plus the injected `persona_template` variable. When
/// `use_overrides` is `false`, only embedded defaults are consulted, which makes
/// the result hermetic and independent of any on-disk template.
///
/// # Errors
///
/// Returns [`TemplateError::Load`] if `system/base` cannot be loaded or parsed
/// (e.g. a broken on-disk override) and [`TemplateError::Render`] if rendering
/// fails. The embedded defaults are exercised by this module's tests, so the
/// `use_overrides = false` path is not expected to fail in practice.
pub(crate) fn render_system(
    use_overrides: bool,
    ctx: &PromptContext,
) -> Result<String, TemplateError> {
    let env = build_environment(use_overrides);
    let persona_template = resolve_persona_template(&env, &ctx.provider_name, &ctx.provider_kind);
    let merged = minijinja::context! {
        persona_template => persona_template,
        ..minijinja::Value::from_serialize(ctx)
    };
    let template = env
        .get_template("system/base")
        .map_err(|source| TemplateError::Load {
            name: "system/base".to_owned(),
            source,
        })?;
    template
        .render(&merged)
        .map_err(|source| TemplateError::Render {
            name: "system/base".to_owned(),
            source,
        })
}

/// Renders the `compaction/summary` template into the summarization instruction.
///
/// The default template carries no variables, but `ctx` is still exposed so an
/// override can reference `{{ provider_kind }}`, `{{ cwd }}`, and the rest. When
/// `use_overrides` is `false`, only the embedded default is consulted.
///
/// # Errors
///
/// Returns [`TemplateError::Load`] if `compaction/summary` cannot be loaded or
/// parsed and [`TemplateError::Render`] if rendering fails.
pub(crate) fn render_compaction(
    use_overrides: bool,
    ctx: &PromptContext,
) -> Result<String, TemplateError> {
    let env = build_environment(use_overrides);
    let template =
        env.get_template("compaction/summary")
            .map_err(|source| TemplateError::Load {
                name: "compaction/summary".to_owned(),
                source,
            })?;
    template
        .render(ctx)
        .map_err(|source| TemplateError::Render {
            name: "compaction/summary".to_owned(),
            source,
        })
}

/// Builds a minijinja environment whose loader resolves the two template layers.
///
/// Auto-escaping is disabled unconditionally: prompts are plain text, never
/// HTML, so values like file paths must pass through verbatim. When
/// `use_overrides` is `true`, the loader consults the on-disk roots before the
/// embedded default; otherwise it serves embedded defaults only.
fn build_environment(use_overrides: bool) -> minijinja::Environment<'static> {
    let mut env = minijinja::Environment::new();
    env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
    // Render templates verbatim: do not strip a trailing newline (minijinja's
    // Jinja2-compatible default), so a template's final newline survives — the
    // compaction prompt ends in `\n` and must keep it byte-for-byte.
    env.set_keep_trailing_newline(true);
    env.set_loader(move |name| {
        // Normalize CRLF -> LF for every template source so renders are
        // byte-for-byte identical across platforms. Files committed with LF can
        // still be checked out as CRLF on Windows (git `core.autocrlf`), and a
        // user-authored override may use either; prompts are LF-only plain text
        // fed to the model, so CRLF carries no meaning and only wastes tokens.
        if use_overrides && let Some(content) = load_override(name) {
            return Ok(Some(normalize_newlines(content)));
        }
        Ok(embedded(name).map(|s| normalize_newlines(s.to_owned())))
    });
    env
}

/// Rewrites Windows `\r\n` line endings to `\n`, leaving lone `\n` untouched.
///
/// Allocation-free on the common case (no `\r\n` present): [`str::replace`]
/// returns a fresh `String` only when a match is found, and the input is
/// already owned, so this is a cheap pass-through for LF-only sources.
fn normalize_newlines(content: String) -> String {
    if content.contains('\r') {
        content.replace("\r\n", "\n")
    } else {
        content
    }
}

/// Resolves the persona partial name for the active provider.
///
/// Tries `system/persona.<provider_name>`, then `system/persona.<provider_kind>`
/// (skipping empty suffixes), and falls back to the default `system/persona`.
/// Existence is probed via `get_template`, so both on-disk and embedded
/// candidates are honored.
fn resolve_persona_template(
    env: &minijinja::Environment<'_>,
    provider_name: &str,
    provider_kind: &str,
) -> String {
    for suffix in [provider_name, provider_kind] {
        if suffix.is_empty() {
            continue;
        }
        let candidate = format!("system/persona.{suffix}");
        if env.get_template(&candidate).is_ok() {
            return candidate;
        }
    }
    "system/persona".to_owned()
}

/// Returns the embedded default template source for `name`, if one exists.
fn embedded(name: &str) -> Option<&'static str> {
    match name {
        "system/base" => Some(SYSTEM_BASE),
        "system/persona" => Some(SYSTEM_PERSONA),
        "compaction/summary" => Some(COMPACTION_SUMMARY),
        _ => None,
    }
}

/// Reads an on-disk override for `name` from the project then user roots.
fn load_override(name: &str) -> Option<String> {
    load_override_from(&override_roots(), name)
}

/// Reads an on-disk override for `name` from `roots`, in order.
///
/// Returns the first readable `<root>/<name>.j2`. A `NotFound` miss is silent; a
/// real read error is logged and treated as a miss so the embedded default takes
/// over rather than failing the render. The file name is built with an explicit
/// `.j2` suffix (not `Path::with_extension`, which would mangle a dotted name
/// like `system/persona.openai` into `system/persona.j2`).
fn load_override_from(roots: &[PathBuf], name: &str) -> Option<String> {
    for root in roots {
        let path = root.join(format!("{name}.{TEMPLATE_EXTENSION}"));
        match std::fs::read_to_string(&path) {
            Ok(content) => return Some(content),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(
                    name: "zhive.template.override_read_failed",
                    template = name,
                    path = %path.display(),
                    error = %err,
                    "failed to read prompt template override; using embedded default",
                );
            }
        }
    }
    None
}

/// Returns the on-disk override roots in precedence order (project, then user).
fn override_roots() -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(2);
    // L2 project-local override, relative to the process working directory
    // (mirrors the skills loader's `./.zhive/skills` convention).
    roots.push(PathBuf::from(".zhive").join("templates"));
    // L1 user-global override, under the same base dir as `config.toml`.
    if let Some(dir) = config_templates_dir() {
        roots.push(dir);
    }
    roots
}

/// Resolves the user-global templates directory from the environment.
///
/// Mirrors the `config.toml` base directory derivation in [`crate::config`]:
/// `$XDG_CONFIG_HOME/zhive/templates`, falling back to
/// `~/.config/zhive/templates`. Returns `None` when neither is resolvable.
fn config_templates_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("zhive").join("templates"));
    }
    let home = std::env::home_dir()?;
    Some(home.join(".config").join("zhive").join("templates"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Oracle copy of the pre-migration `PERSONA` constant.
    ///
    /// Used to assert the templatized prompt is byte-for-byte identical to the
    /// hardcoded layout it replaced.
    const ORACLE_PERSONA: &str = "You are zhive, an AI coding assistant operating in a terminal. You help with software-engineering tasks by reading and editing files, running commands, and searching the codebase using the tools provided to you. Be concise and precise. Prefer the provided tools over guessing, and briefly explain non-trivial actions before taking them.";

    fn context(cwd: &str, os: &str, project: Option<ProjectInstructions>) -> PromptContext {
        PromptContext {
            cwd: cwd.to_owned(),
            os: os.to_owned(),
            provider_name: "anthropic".to_owned(),
            provider_kind: "anthropic".to_owned(),
            model: None,
            project_instructions: project,
            skills: None,
        }
    }

    #[test]
    fn embedded_render_matches_legacy_layout_without_instructions() {
        let rendered =
            render_system(false, &context("/work/dir", "linux", None)).expect("embedded render");
        let expected = format!(
            "{ORACLE_PERSONA}\n\n# Environment\n- Working directory: /work/dir\n- Operating system: linux\n"
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn embedded_render_matches_legacy_layout_with_instructions() {
        let project = ProjectInstructions {
            source: "/work/AGENTS.md".to_owned(),
            body: "Follow the house style.".to_owned(),
            truncated: false,
        };
        let rendered =
            render_system(false, &context("/work", "macos", Some(project))).expect("render");
        let expected = format!(
            "{ORACLE_PERSONA}\n\n# Environment\n- Working directory: /work\n- Operating system: macos\n\n# Project instructions\nSource: /work/AGENTS.md\n\nFollow the house style."
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn embedded_render_includes_skills_section_when_present() {
        let mut ctx = context("/work", "linux", None);
        ctx.skills = Some(
            "# Skills\n\n<available_skills>\n  <skill>\n    <name>demo</name>\n  </skill>\n</available_skills>"
                .to_owned(),
        );
        let rendered = render_system(false, &ctx).expect("render with skills");
        assert!(rendered.contains("<available_skills>"));
        assert!(rendered.contains("<name>demo</name>"));
        // The skills section follows the environment block.
        let env_pos = rendered.find("# Environment").expect("environment present");
        let skills_pos = rendered.find("<available_skills>").expect("skills present");
        assert!(skills_pos > env_pos, "skills must come after environment");
    }

    #[test]
    fn embedded_render_omits_skills_section_when_absent() {
        // A None `skills` must render byte-for-byte as before (no stray markers).
        let rendered =
            render_system(false, &context("/work", "linux", None)).expect("render without skills");
        assert!(!rendered.contains("available_skills"));
    }

    #[test]
    fn persona_resolution_falls_back_to_default_when_no_provider_partial() {
        let env = build_environment(false);
        // No `system/persona.anthropic` embedded default exists, so resolution
        // falls through to the plain `system/persona`.
        let resolved = resolve_persona_template(&env, "anthropic", "anthropic");
        assert_eq!(resolved, "system/persona");
    }

    #[test]
    fn persona_resolution_prefers_provider_kind_partial() {
        let mut env = minijinja::Environment::new();
        env.add_template("system/persona", SYSTEM_PERSONA)
            .expect("base persona");
        env.add_template("system/persona.openai", "OpenAI persona.")
            .expect("provider persona");
        // No exact-name partial, but a kind partial exists -> kind wins.
        let resolved = resolve_persona_template(&env, "scripted", "openai");
        assert_eq!(resolved, "system/persona.openai");
    }

    #[test]
    fn persona_resolution_prefers_exact_name_over_kind() {
        let mut env = minijinja::Environment::new();
        env.add_template("system/persona", SYSTEM_PERSONA)
            .expect("base persona");
        env.add_template("system/persona.openai", "kind persona.")
            .expect("kind persona");
        env.add_template("system/persona.my-proxy", "name persona.")
            .expect("name persona");
        // Both an exact-name and a kind partial exist -> the name wins.
        let resolved = resolve_persona_template(&env, "my-proxy", "openai");
        assert_eq!(resolved, "system/persona.my-proxy");
    }

    #[test]
    fn persona_resolution_skips_empty_provider_fields() {
        let env = build_environment(false);
        // Empty name and kind must never form a `system/persona.` candidate.
        let resolved = resolve_persona_template(&env, "", "");
        assert_eq!(resolved, "system/persona");
    }

    /// Oracle copy of `zhive_core`'s built-in `SUMMARY_INSTRUCTION` constant.
    ///
    /// The embedded `compaction/summary` template must render byte-for-byte to
    /// this string so injecting it is equivalent to the engine's own default.
    const ORACLE_SUMMARY_INSTRUCTION: &str = "You are performing a CONTEXT CHECKPOINT COMPACTION. Write a concise handoff summary for another assistant that will resume this task. Include: current progress and key decisions, important context / constraints / user preferences, what remains to be done, and any critical data or references needed to continue. Respond with the summary only.\n\n--- TRANSCRIPT ---\n";

    #[test]
    fn embedded_compaction_matches_engine_default() {
        let rendered = render_compaction(false, &context("/work", "linux", None))
            .expect("embedded compaction render");
        assert_eq!(rendered, ORACLE_SUMMARY_INSTRUCTION);
    }

    #[test]
    fn normalize_newlines_rewrites_crlf() {
        assert_eq!(normalize_newlines("a\r\nb\r\n".to_owned()), "a\nb\n");
    }

    #[test]
    fn normalize_newlines_leaves_lf_untouched() {
        assert_eq!(normalize_newlines("a\nb\n".to_owned()), "a\nb\n");
    }

    #[test]
    fn normalize_newlines_handles_mixed_endings() {
        // A lone `\r` (no following `\n`) is left as-is; only CRLF is rewritten.
        assert_eq!(normalize_newlines("a\r\nb\nc\rd".to_owned()), "a\nb\nc\rd");
    }

    #[test]
    fn load_override_reads_dotted_provider_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sys = dir.path().join("system");
        std::fs::create_dir_all(&sys).expect("mkdir");
        std::fs::write(sys.join("persona.openai.j2"), "OpenAI override.").expect("write");
        let roots = [dir.path().to_path_buf()];

        // A dotted name must map to `persona.openai.j2`, not `persona.j2`.
        assert_eq!(
            load_override_from(&roots, "system/persona.openai").as_deref(),
            Some("OpenAI override.")
        );
        // The plain name must not accidentally resolve to the dotted file.
        assert_eq!(load_override_from(&roots, "system/persona"), None);
    }

    #[test]
    fn load_override_prefers_earlier_root() {
        let project = tempfile::tempdir().expect("project");
        let user = tempfile::tempdir().expect("user");
        for (root, body) in [(project.path(), "project base"), (user.path(), "user base")] {
            let sys = root.join("system");
            std::fs::create_dir_all(&sys).expect("mkdir");
            std::fs::write(sys.join("base.j2"), body).expect("write");
        }
        let roots = [project.path().to_path_buf(), user.path().to_path_buf()];
        // The earlier (project) root wins over the later (user) root.
        assert_eq!(
            load_override_from(&roots, "system/base").as_deref(),
            Some("project base")
        );
    }

    #[test]
    fn load_override_none_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let roots = [dir.path().to_path_buf()];
        assert!(load_override_from(&roots, "system/base").is_none());
    }

    #[test]
    fn broken_override_is_rejected_so_fallback_engages() {
        // A syntactically broken override surfaces as a minijinja error, which
        // drives `assemble`'s retry-with-embedded-defaults fallback.
        let mut env = minijinja::Environment::new();
        let err = env
            .add_template_owned("system/base".to_owned(), "{% if %}".to_owned())
            .expect_err("broken template must be rejected");
        assert_eq!(err.kind(), minijinja::ErrorKind::SyntaxError);
    }
}

// Rust guideline compliant 2026-02-21

//! Host-side assembly of the engine system prompt from `.j2` templates.
//!
//! The engine ([`zhive_core`]) is prompt-agnostic: it prepends whatever opaque
//! text the host supplies via `EngineConfig::system_prompt`. This module owns
//! the *policy* of what that text says, but the prose now lives in editable
//! Jinja2 templates rather than hardcoded string constants. [`assemble`] gathers
//! the live context — working directory, host OS, active provider, and the
//! nearest project instruction file — and hands it to [`crate::prompt_template`]
//! to render the `system/base` template.
//!
//! The persona is selected per provider (see [`crate::prompt_template`]), so a
//! deployment can ship different instructions for different backends without a
//! code change. A byte budget ([`MAX_INSTRUCTION_BYTES`]) still caps the project
//! instruction file so a large document cannot dominate the context window.
//!
//! The model learns the *callable* tool surface from the function-calling
//! `tools` advertisement (see `zhive_core::engine::prompt`), so the prompt
//! deliberately does not enumerate tools — that would only drift out of sync
//! with the live registry.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::prompt_template::{ProjectInstructions, PromptContext};

/// Maximum bytes of a project instruction file folded into the prompt.
///
/// A generous cap that fits a substantial `AGENTS.md` while preventing a
/// pathological file from dominating the context window — the failure mode
/// where an oversized instruction file forces an immediate first-turn
/// compaction.
const MAX_INSTRUCTION_BYTES: usize = 32 * 1024;

/// Candidate instruction file names, in per-directory preference order.
///
/// `AGENTS.md` is the canonical name; `CLAUDE.md` is accepted as a legacy
/// fallback because most existing repositories already carry one.
const INSTRUCTION_FILES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Marker appended when a project instruction file is truncated.
const TRUNCATION_MARKER: &str = "\n\n[… instruction file truncated by zhive]";

/// Assembles the engine system prompt for a session rooted at `cwd`.
///
/// Builds the template [`PromptContext`] from the live environment — the
/// working directory, host OS, the active `provider_name` / `provider_kind` /
/// `model`, and the nearest project instruction file (see [`find_instructions`])
/// — then renders the `system/base` template. The text is returned as an
/// [`Arc<str>`] so it can be cloned cheaply into `EngineConfig::system_prompt`.
///
/// `provider_kind` (the backend type, e.g. `openai`) is more stable than
/// `provider_name` (a user-chosen label) and is the preferred key for selecting
/// a per-provider persona.
///
/// Rendering degrades rather than fails: if an on-disk template override is
/// broken, it retries with embedded defaults; if even that fails, it falls back
/// to the bare persona text. The function therefore always yields a usable
/// prompt and never panics.
pub(crate) fn assemble(
    cwd: &Path,
    provider_name: &str,
    provider_kind: &str,
    model: Option<&str>,
) -> Arc<str> {
    let ctx = build_prompt_context(cwd, provider_name, provider_kind, model);

    match crate::prompt_template::render_system(true, &ctx) {
        Ok(text) => Arc::from(text),
        Err(override_err) => {
            tracing::warn!(
                name: "zhive.system_prompt.override_render_failed",
                error = %override_err,
                "system prompt override failed to render; retrying with embedded defaults",
            );
            match crate::prompt_template::render_system(false, &ctx) {
                Ok(text) => Arc::from(text),
                Err(embedded_err) => {
                    tracing::error!(
                        name: "zhive.system_prompt.render_failed",
                        error = %embedded_err,
                        "embedded system prompt failed to render; using bare persona",
                    );
                    Arc::from(crate::prompt_template::FALLBACK_PERSONA)
                }
            }
        }
    }
}

/// Assembles the compaction summarization instruction for a session.
///
/// Renders the `compaction/summary` template with the live [`PromptContext`].
/// Returns `Some` with the rendered instruction for `EngineConfig::compaction_prompt`,
/// or `None` if rendering fails even with embedded defaults — in which case the
/// engine falls back to its own built-in instruction, so compaction still works.
///
/// With no on-disk override, the embedded template renders byte-for-byte to the
/// engine's built-in instruction, so injecting it changes nothing.
pub(crate) fn assemble_compaction(
    cwd: &Path,
    provider_name: &str,
    provider_kind: &str,
    model: Option<&str>,
) -> Option<Arc<str>> {
    let ctx = build_prompt_context(cwd, provider_name, provider_kind, model);

    match crate::prompt_template::render_compaction(true, &ctx) {
        Ok(text) => Some(Arc::from(text)),
        Err(override_err) => {
            tracing::warn!(
                name: "zhive.compaction_prompt.override_render_failed",
                error = %override_err,
                "compaction prompt override failed to render; retrying with embedded defaults",
            );
            match crate::prompt_template::render_compaction(false, &ctx) {
                Ok(text) => Some(Arc::from(text)),
                Err(embedded_err) => {
                    tracing::error!(
                        name: "zhive.compaction_prompt.render_failed",
                        error = %embedded_err,
                        "embedded compaction prompt failed to render; using engine default",
                    );
                    None
                }
            }
        }
    }
}

/// Builds the [`PromptContext`] handed to the system-prompt template.
///
/// Reads the live working directory, host OS, and active provider identity, and
/// folds in the nearest project instruction file when one exists.
fn build_prompt_context(
    cwd: &Path,
    provider_name: &str,
    provider_kind: &str,
    model: Option<&str>,
) -> PromptContext {
    let project_instructions =
        find_instructions(cwd).map(|(path, body, truncated)| ProjectInstructions {
            source: path.display().to_string(),
            body,
            truncated,
        });

    PromptContext {
        cwd: cwd.display().to_string(),
        os: std::env::consts::OS.to_owned(),
        provider_name: provider_name.to_owned(),
        provider_kind: provider_kind.to_owned(),
        model: model.map(str::to_owned),
        project_instructions,
    }
}

/// Finds the nearest project instruction file at or above `cwd`.
///
/// Walks from `cwd` to the filesystem root; within each directory, prefers
/// `AGENTS.md` over `CLAUDE.md`. Returns the first readable file's path, its
/// contents truncated to [`MAX_INSTRUCTION_BYTES`], and whether truncation
/// occurred, or `None` when none exists.
fn find_instructions(cwd: &Path) -> Option<(PathBuf, String, bool)> {
    for dir in cwd.ancestors() {
        for name in INSTRUCTION_FILES {
            let path = dir.join(name);
            if let Ok(content) = std::fs::read_to_string(&path) {
                let truncated = content.len() > MAX_INSTRUCTION_BYTES;
                let body = truncate_on_boundary(content, MAX_INSTRUCTION_BYTES);
                return Some((path, body, truncated));
            }
        }
    }
    None
}

/// Truncates `s` to at most `max_bytes`, snapping down to a UTF-8 char boundary.
///
/// When truncation occurs, [`TRUNCATION_MARKER`] is appended so the model knows
/// the instructions were clipped.
fn truncate_on_boundary(mut s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str(TRUNCATION_MARKER);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_instructions_returns_none_when_absent() {
        // A synthetic path whose ancestors carry no instruction file.
        let found = find_instructions(Path::new("/tmp/zhive-nonexistent-xyz"));
        assert!(found.is_none());
    }

    #[test]
    fn find_instructions_folds_nearest() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("AGENTS.md"), "Follow the house style.").expect("write");
        let nested = dir.path().join("sub/deep");
        std::fs::create_dir_all(&nested).expect("mkdir");

        let (path, body, truncated) = find_instructions(&nested).expect("must find");
        assert!(path.ends_with("AGENTS.md"));
        assert_eq!(body, "Follow the house style.");
        assert!(!truncated);
    }

    #[test]
    fn find_instructions_prefers_agents_over_claude() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("AGENTS.md"), "agents wins").expect("write");
        std::fs::write(dir.path().join("CLAUDE.md"), "claude loses").expect("write");

        let (_, body, _) = find_instructions(dir.path()).expect("must find");
        assert_eq!(body, "agents wins");
    }

    #[test]
    fn find_instructions_falls_back_to_claude() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("CLAUDE.md"), "legacy instructions").expect("write");

        let (path, body, _) = find_instructions(dir.path()).expect("must find");
        assert!(path.ends_with("CLAUDE.md"));
        assert_eq!(body, "legacy instructions");
    }

    #[test]
    fn build_prompt_context_populates_fields() {
        let ctx = build_prompt_context(
            Path::new("/tmp/zhive-nonexistent-xyz"),
            "my-proxy",
            "openai",
            Some("gpt-5"),
        );
        assert_eq!(ctx.cwd, "/tmp/zhive-nonexistent-xyz");
        assert_eq!(ctx.os, std::env::consts::OS);
        assert_eq!(ctx.provider_name, "my-proxy");
        assert_eq!(ctx.provider_kind, "openai");
        assert_eq!(ctx.model.as_deref(), Some("gpt-5"));
        assert!(ctx.project_instructions.is_none());
    }

    #[test]
    fn assemble_returns_nonempty_prompt() {
        // Smoke test: assembling never panics and yields usable text. (Byte
        // equivalence is asserted hermetically in `crate::prompt_template`.)
        let prompt = assemble(
            Path::new("/tmp/zhive-nonexistent-xyz"),
            "anthropic",
            "anthropic",
            Some("claude-opus-4-8"),
        );
        assert!(!prompt.is_empty());
    }

    #[test]
    fn truncate_on_boundary_caps_and_marks() {
        let s = "a".repeat(100);
        let out = truncate_on_boundary(s, 10);
        assert!(out.starts_with(&"a".repeat(10)));
        assert!(out.contains("truncated"));
    }

    #[test]
    fn truncate_on_boundary_respects_char_boundary() {
        // "é" is two bytes; a byte budget landing mid-char must snap down.
        let s = "é".repeat(10); // 20 bytes
        let out = truncate_on_boundary(s, 5); // 5 is mid-char (odd)
        let body = out.strip_suffix(TRUNCATION_MARKER).unwrap_or(&out);
        assert_eq!(body, "éé"); // 4 bytes ≤ 5, snapped down from 5
    }

    #[test]
    fn truncate_on_boundary_noop_when_within_budget() {
        let s = "short".to_owned();
        let out = truncate_on_boundary(s, 100);
        assert_eq!(out, "short");
    }
}

// Rust guideline compliant 2026-02-21

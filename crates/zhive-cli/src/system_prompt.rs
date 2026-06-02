//! Host-side assembly of the engine system prompt.
//!
//! The engine ([`zhive_core`]) is prompt-agnostic: it prepends whatever opaque
//! text the host supplies via `EngineConfig::system_prompt`. This module owns
//! the *policy* of what that text says — a fixed persona, a live environment
//! block, and the project's instruction file (`AGENTS.md`, or `CLAUDE.md` as a
//! legacy fallback) discovered by walking up from the working directory.
//!
//! A byte budget ([`MAX_INSTRUCTION_BYTES`]) caps the instruction file so a
//! large document cannot dominate the context window or trigger an immediate
//! compaction on the very first turn.
//!
//! The model still learns the *callable* tool surface from the function-calling
//! `tools` advertisement (see `zhive_core::engine::prompt`), so this prompt
//! deliberately does not enumerate tools — that would only drift out of sync
//! with the live registry.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Persona prepended to every assembled system prompt.
const PERSONA: &str = "\
You are zhive, an AI coding assistant operating in a terminal. You help with \
software-engineering tasks by reading and editing files, running commands, and \
searching the codebase using the tools provided to you. Be concise and precise. \
Prefer the provided tools over guessing, and briefly explain non-trivial \
actions before taking them.";

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
/// The result always begins with the [`PERSONA`] and an environment block, and
/// folds in the nearest project instruction file (see [`find_instructions`])
/// when one exists. The text is returned as an [`Arc<str>`] so it can be cloned
/// cheaply into `EngineConfig::system_prompt`.
pub(crate) fn assemble(cwd: &Path) -> Arc<str> {
    let mut prompt = String::with_capacity(PERSONA.len() + 256);
    prompt.push_str(PERSONA);

    prompt.push_str("\n\n# Environment\n");
    // Writing to a `String` is infallible, so the `Result` is discarded.
    let _ = writeln!(prompt, "- Working directory: {}", cwd.display());
    let _ = writeln!(prompt, "- Operating system: {}", std::env::consts::OS);

    if let Some((path, body)) = find_instructions(cwd) {
        prompt.push_str("\n# Project instructions\n");
        let _ = writeln!(prompt, "Source: {}\n", path.display());
        prompt.push_str(&body);
    }

    Arc::from(prompt)
}

/// Finds the nearest project instruction file at or above `cwd`.
///
/// Walks from `cwd` to the filesystem root; within each directory, prefers
/// `AGENTS.md` over `CLAUDE.md`. Returns the first readable file's path and its
/// contents truncated to [`MAX_INSTRUCTION_BYTES`], or `None` when none exists.
fn find_instructions(cwd: &Path) -> Option<(PathBuf, String)> {
    for dir in cwd.ancestors() {
        for name in INSTRUCTION_FILES {
            let path = dir.join(name);
            if let Ok(content) = std::fs::read_to_string(&path) {
                return Some((path, truncate_on_boundary(content, MAX_INSTRUCTION_BYTES)));
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
    fn assemble_includes_persona_and_environment() {
        let prompt = assemble(Path::new("/tmp/zhive-nonexistent-xyz"));
        assert!(prompt.contains("You are zhive"));
        assert!(prompt.contains("# Environment"));
        assert!(prompt.contains("Working directory: /tmp/zhive-nonexistent-xyz"));
        // No instruction file in that synthetic dir → no project section.
        assert!(!prompt.contains("# Project instructions"));
    }

    #[test]
    fn assemble_folds_in_nearest_instruction_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("AGENTS.md"), "Follow the house style.").expect("write");
        let nested = dir.path().join("sub/deep");
        std::fs::create_dir_all(&nested).expect("mkdir");

        let prompt = assemble(&nested);
        assert!(prompt.contains("# Project instructions"));
        assert!(prompt.contains("Follow the house style."));
        assert!(prompt.contains("AGENTS.md"));
    }

    #[test]
    fn agents_md_wins_over_claude_md_in_same_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("AGENTS.md"), "agents wins").expect("write");
        std::fs::write(dir.path().join("CLAUDE.md"), "claude loses").expect("write");

        let prompt = assemble(dir.path());
        assert!(prompt.contains("agents wins"));
        assert!(!prompt.contains("claude loses"));
    }

    #[test]
    fn claude_md_used_as_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("CLAUDE.md"), "legacy instructions").expect("write");

        let prompt = assemble(dir.path());
        assert!(prompt.contains("legacy instructions"));
        assert!(prompt.contains("CLAUDE.md"));
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
        // Output up to the marker must be valid UTF-8 (guaranteed by String)
        // and contain only whole "é"s before the marker.
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

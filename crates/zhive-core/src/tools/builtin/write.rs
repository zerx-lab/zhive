//! [`WriteFileTool`] and [`EditFileTool`]: atomic file write and in-place edit.

use std::path::Path;

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::builtin::resolve_path;
use crate::tools::{FileDiff, Tool, ToolContext, ToolError, ToolKind, ToolOutput};

/// Per-side byte ceiling above which a captured diff is dropped.
///
/// Both `edit` and `write` attach whole-file before/after text so clients can
/// render a diff. This matches the TUI renderer's own per-side ceiling
/// (`MAX_DIFF_BYTES` in `zhive-tui/src/diff.rs`): past this size the renderer
/// suppresses the diff, so capturing it would only bloat the persisted rollout
/// without any rendering benefit. The plain-text result is always kept.
const MAX_DIFF_CAPTURE_BYTES: usize = 200_000;

// ============================================================
// WriteFileTool
// ============================================================

/// Atomically writes `content` to `path`, creating parent directories.
///
/// The write is performed by writing to a sibling temporary file in the same
/// directory as `path` and then renaming it into place. This prevents partial
/// writes from corrupting the destination when the process is interrupted.
///
/// Arguments:
///
/// ```json
/// { "path": "/abs/or/rel/file.txt", "content": "new file content\n" }
/// ```
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::WriteFileTool;
/// use zhive_core::tools::Tool;
/// assert_eq!(WriteFileTool.name(), "write");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn description(&self) -> Option<String> {
        Some(
            "Atomically write content to a file, creating parent directories as \
             needed. Overwrites the entire file when it exists — prefer `edit` for \
             targeted changes so unrelated content is never lost. The write is \
             staged to a temp file and renamed, so a failure never leaves a \
             partially written target."
                .to_owned(),
        )
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "Absolute or relative path to the file." },
                "content": { "type": "string", "description": "New file content." }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    /// Executes the write operation.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] if the parent directory cannot be
    /// created, the temp file cannot be written, or the rename fails.
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::Execution("`path` must be a string".to_owned()))?;
        let content = args["content"]
            .as_str()
            .ok_or_else(|| ToolError::Execution("`content` must be a string".to_owned()))?;

        let dest = resolve_path(path_str);

        // Build the diff before overwriting so the tool call can surface it.
        // Computed up front to distinguish a brand-new file (creation diff) from
        // an existing file we cannot faithfully diff.
        let pending_diff = build_write_diff(&dest, content).await;

        // Ensure parent directory exists.
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                ToolError::Execution(format!(
                    "cannot create directories for `{}`: {e}",
                    dest.display()
                ))
            })?;
        }

        // Write atomically via a sibling temp file.
        let dir = dest.parent().unwrap_or_else(|| std::path::Path::new("."));
        let tmp = dir.join(tmp_file_name("write"));

        tokio::fs::write(&tmp, content.as_bytes())
            .await
            .map_err(|e| {
                ToolError::Execution(format!("cannot write temp file `{}`: {e}", tmp.display()))
            })?;

        tokio::fs::rename(&tmp, &dest).await.map_err(|e| {
            // Best-effort cleanup on failure; ignore secondary errors.
            let _ = std::fs::remove_file(&tmp);
            ToolError::Execution(format!(
                "cannot rename `{}` -> `{}`: {e}",
                tmp.display(),
                dest.display()
            ))
        })?;

        let out = ToolOutput::text(format!(
            "wrote {} bytes to `{}`",
            content.len(),
            dest.display()
        ));
        Ok(match pending_diff {
            Some(diff) => out.with_diffs(vec![diff]),
            None => out,
        })
    }
}

// ============================================================
// EditFileTool
// ============================================================

/// Replaces a substring inside a file and writes it back atomically.
///
/// Replacement strategy:
///
/// 1. **Exact match** — `old_string` is searched verbatim in the file
///    content. When `replace_all` is `false` (the default), exactly one match
///    is required; more than one match returns an error asking the caller to
///    add more context or pass `replace_all`. When `replace_all` is `true`,
///    all occurrences are replaced.
///
/// 2. **Line-level trim fallback** — if exact matching finds zero occurrences,
///    the tool retries by trimming trailing whitespace from every line in both
///    `old_string` and the file. The fallback applies only when it finds a
///    *unique* match position; otherwise the error `"old_string not found"` is
///    returned.
///
/// Arguments:
///
/// ```json
/// {
///   "path":        "/abs/or/rel/file.txt",
///   "old_string":  "text to replace",
///   "new_string":  "replacement text",
///   "replace_all": false
/// }
/// ```
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::EditFileTool;
/// use zhive_core::tools::Tool;
/// assert_eq!(EditFileTool.name(), "edit");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }

    fn description(&self) -> Option<String> {
        Some(
            "Replace an exact substring inside an existing file, written back \
             atomically. `old_string` must match a unique span — include \
             surrounding context to disambiguate, or pass `replace_all` to change \
             every occurrence. Use this for surgical edits; use `write` to create \
             or fully replace a file."
                .to_owned(),
        )
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":        { "type": "string",  "description": "Absolute or relative path to the file." },
                "old_string":  { "type": "string",  "description": "Text to replace." },
                "new_string":  { "type": "string",  "description": "Replacement text." },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)." }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }

    /// Executes the edit operation.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] when `old_string == new_string`,
    /// when the unique-match constraint is violated, or when I/O fails.
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::Execution("`path` must be a string".to_owned()))?;
        let old_string = args["old_string"]
            .as_str()
            .ok_or_else(|| ToolError::Execution("`old_string` must be a string".to_owned()))?;
        let new_string = args["new_string"]
            .as_str()
            .ok_or_else(|| ToolError::Execution("`new_string` must be a string".to_owned()))?;
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);

        if old_string == new_string {
            return Err(ToolError::Execution(
                "`old_string` and `new_string` are identical; nothing to do".to_owned(),
            ));
        }

        let dest = resolve_path(path_str);

        let raw = tokio::fs::read(&dest)
            .await
            .map_err(|e| ToolError::Execution(format!("cannot read `{}`: {e}", dest.display())))?;

        let content = std::str::from_utf8(&raw).map_err(|_utf8_err| {
            ToolError::Execution(format!(
                "binary file (non-UTF-8 content): `{}`",
                dest.display()
            ))
        })?;

        // --- Strategy 1: exact match ---
        let count = content.matches(old_string).count();

        let (new_content, replacements) = if count > 0 {
            if replace_all {
                let replaced = content.replace(old_string, new_string);
                (replaced, count)
            } else if count == 1 {
                let replaced = content.replacen(old_string, new_string, 1);
                (replaced, 1)
            } else {
                return Err(ToolError::Execution(format!(
                    "`old_string` is not unique ({count} occurrences); \
                     pass `replace_all: true` or add more context to make it unique"
                )));
            }
        } else {
            // --- Strategy 2: line-level trim fallback ---
            let trimmed_old = trim_line_endings(old_string);
            let (byte_start, byte_end) = find_unique_trim_match(content, &trimmed_old)?;
            let replaced = format!(
                "{}{}{}",
                &content[..byte_start],
                new_string,
                &content[byte_end..]
            );
            (replaced, 1)
        };

        // Atomic write back.
        atomic_write(&dest, new_content.as_bytes()).await?;

        let out = ToolOutput::text(format!(
            "replaced {replacements} occurrence(s) in `{}`",
            dest.display()
        ));
        // `content` is the whole file before the edit; `new_content` after. Both
        // are already in scope and UTF-8, so the diff is free of extra I/O.
        Ok(attach_diff(out, &dest, Some(content), &new_content))
    }
}

// ============================================================
// Helpers
// ============================================================

/// Attaches a whole-file diff to `out` when both sides fit the size cap.
///
/// Oversized diffs are dropped (see [`MAX_DIFF_CAPTURE_BYTES`]); the returned
/// output is otherwise `out` enriched with a single [`FileDiff`]. `old_text` is
/// `None` for a freshly created file.
fn attach_diff(out: ToolOutput, path: &Path, old_text: Option<&str>, new_text: &str) -> ToolOutput {
    let old_len = old_text.map_or(0, str::len);
    if old_len > MAX_DIFF_CAPTURE_BYTES || new_text.len() > MAX_DIFF_CAPTURE_BYTES {
        return out;
    }
    out.with_diffs(vec![FileDiff {
        path: path.to_path_buf(),
        old_text: old_text.map(str::to_owned),
        new_text: new_text.to_owned(),
    }])
}

/// Builds the diff to attach to a `write`, or `None` to attach none.
///
/// Returns a creation diff (`old_text == None`) when `dest` does not exist, an
/// update diff carrying the prior content when it can be read, and `None` when
/// either side is too large or the existing file is non-UTF-8 (binary). In the
/// last case a partial diff would misrepresent the change — e.g. rendering an
/// overwrite of large or binary content as a pure creation — so none is shown.
///
/// `new_content` duplicates the tool's `raw_input.content` in the persisted
/// item; the duplication is bounded by [`MAX_DIFF_CAPTURE_BYTES`] and accepted
/// as the cost of an ACP-renderable diff block.
async fn build_write_diff(dest: &Path, new_content: &str) -> Option<FileDiff> {
    if new_content.len() > MAX_DIFF_CAPTURE_BYTES {
        return None;
    }
    let old_text = match tokio::fs::read(dest).await {
        // Missing file → genuine creation diff (all additions).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Ok(bytes) if bytes.len() <= MAX_DIFF_CAPTURE_BYTES => match String::from_utf8(bytes) {
            Ok(text) => Some(text),
            // Binary prior content: cannot diff faithfully → show nothing.
            Err(_) => return None,
        },
        // Existing file too large or otherwise unreadable → show no diff rather
        // than pass off the overwrite as a creation.
        _ => return None,
    };
    Some(FileDiff {
        path: dest.to_path_buf(),
        old_text,
        new_text: new_content.to_owned(),
    })
}

/// Trims trailing whitespace from every line of `s`.
fn trim_line_endings(s: &str) -> String {
    s.lines().map(str::trim_end).collect::<Vec<_>>().join("\n")
}

/// Finds the unique position in `content` where `trimmed_old` matches
/// using a line-trimmed comparison. Returns `(byte_start, byte_end)` of
/// the matched region in the *original* `content`.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] when the pattern is not found or is not
/// unique.
fn find_unique_trim_match(content: &str, trimmed_old: &str) -> Result<(usize, usize), ToolError> {
    // Split the file into lines with their byte positions.
    // We look for a contiguous block of lines that, after trimming, matches
    // `trimmed_old`.
    let old_lines: Vec<&str> = trimmed_old.lines().collect();
    if old_lines.is_empty() {
        return Err(ToolError::Execution(
            "`old_string` not found (empty after normalization)".to_owned(),
        ));
    }

    // Collect (byte_offset_of_line_start, original_line, trimmed_line).
    let mut line_data: Vec<(usize, &str)> = Vec::new();
    let mut pos = 0usize;
    for line in content.split('\n') {
        line_data.push((pos, line));
        pos += line.len() + 1; // +1 for '\n'
    }

    let n = old_lines.len();
    let m = line_data.len();
    let mut matches: Vec<(usize, usize)> = Vec::new(); // (byte_start, byte_end)

    'outer: for window_start in 0..m.saturating_sub(n).saturating_add(1) {
        // Check whether lines [window_start .. window_start+n] trim-match old_lines.
        if window_start + n > m {
            break;
        }
        for (k, &old_line) in old_lines.iter().enumerate() {
            if line_data[window_start + k].1.trim_end() != old_line {
                continue 'outer;
            }
        }
        // Found a match.
        let byte_start = line_data[window_start].0;
        // byte_end: start of the character just after the last matched line
        // (including the '\n' that terminated it, if present).
        let last_idx = window_start + n - 1;
        let last_line_start = line_data[last_idx].0;
        let last_line_len = line_data[last_idx].1.len();
        // End at the last matched line's *content* — do NOT consume the
        // trailing '\n'. `old_string` was split with `lines()`, which strips
        // line terminators, so the matched region is line content only.
        // Preserving the following newline keeps the file's line structure
        // intact and avoids silently dropping a trailing newline on replace.
        let byte_end = last_line_start + last_line_len;
        matches.push((byte_start, byte_end));
    }

    match matches.len() {
        0 => Err(ToolError::Execution(
            "`old_string` not found (tried exact match and line-trim fallback)".to_owned(),
        )),
        1 => Ok(matches[0]),
        _ => Err(ToolError::Execution(
            "`old_string` is not unique after line-trim normalization; \
             add more context lines to make it unique"
                .to_owned(),
        )),
    }
}

/// Monotonic counter making every temp-file name unique within this process.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Builds a collision-resistant sibling temp-file name for an atomic write.
///
/// Combines the process id with a per-process monotonic counter so two writes
/// to the same directory never share a path — even when issued in the same
/// nanosecond from the same process, which a wall-clock-only name could not
/// guarantee. The leading dot keeps the temp file hidden.
#[must_use]
fn tmp_file_name(tag: &str) -> String {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(".zhive-{tag}-tmp-{}-{seq}.tmp", std::process::id())
}

/// Atomically writes `bytes` to `dest` via a sibling temp file + rename.
///
/// # Errors
///
/// Returns [`ToolError::Execution`] on I/O failure.
async fn atomic_write(dest: &std::path::Path, bytes: &[u8]) -> Result<(), ToolError> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            ToolError::Execution(format!(
                "cannot create directories for `{}`: {e}",
                dest.display()
            ))
        })?;
    }
    let dir = dest.parent().unwrap_or_else(|| std::path::Path::new("."));
    let tmp = dir.join(tmp_file_name("edit"));
    tokio::fs::write(&tmp, bytes).await.map_err(|e| {
        ToolError::Execution(format!("cannot write temp file `{}`: {e}", tmp.display()))
    })?;
    tokio::fs::rename(&tmp, dest).await.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        ToolError::Execution(format!(
            "cannot rename `{}` -> `{}`: {e}",
            tmp.display(),
            dest.display()
        ))
    })?;
    Ok(())
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use std::io::Write as IoWrite;
    use std::sync::Arc;

    use tempfile::{NamedTempFile, TempDir};
    use tokio_util::sync::CancellationToken;
    use zhive_proto::domain::{ThreadId, TurnId};

    use super::*;

    fn ctx() -> ToolContext {
        ToolContext {
            thread_id: ThreadId(Arc::from("thread:native/test")),
            turn_id: TurnId(Arc::from("turn:0")),
            cancel: CancellationToken::new(),
            spawner: None,
        }
    }

    // ---- temp-name helper ----

    #[test]
    fn tmp_file_name_is_unique_and_tagged() {
        let names: std::collections::HashSet<String> =
            (0..1000).map(|_| tmp_file_name("write")).collect();
        assert_eq!(names.len(), 1000, "every temp name must be distinct");
        assert!(tmp_file_name("edit").starts_with(".zhive-edit-tmp-"));
    }

    // ---- WriteFileTool tests ----

    #[tokio::test]
    async fn write_creates_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hello.txt");
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "content": "hello world\n"
        });
        let out = WriteFileTool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("bytes"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello world\n");
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a/b/c/file.txt");
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "content": "deep"
        });
        WriteFileTool.execute(args, &ctx()).await.unwrap();
        assert!(path.exists());
    }

    #[tokio::test]
    async fn write_overwrites_existing_file() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "old content").unwrap();
        let args = serde_json::json!({
            "path": f.path().to_str().unwrap(),
            "content": "new content"
        });
        WriteFileTool.execute(args, &ctx()).await.unwrap();
        let content = std::fs::read_to_string(f.path()).unwrap();
        assert_eq!(content, "new content");
    }

    // ---- EditFileTool tests ----

    #[tokio::test]
    async fn edit_exact_match_single_occurrence() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "hello world").unwrap();
        let args = serde_json::json!({
            "path": f.path().to_str().unwrap(),
            "old_string": "world",
            "new_string": "Rust"
        });
        let out = EditFileTool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("1 occurrence"));
        let content = std::fs::read_to_string(f.path()).unwrap();
        assert!(content.contains("Rust"));
        assert!(!content.contains("world"));
    }

    #[tokio::test]
    async fn edit_multiple_occurrences_require_replace_all() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "foo foo foo").unwrap();
        let args = serde_json::json!({
            "path": f.path().to_str().unwrap(),
            "old_string": "foo",
            "new_string": "bar"
        });
        let err = EditFileTool.execute(args, &ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(ref m) if m.contains("not unique")));
    }

    #[tokio::test]
    async fn edit_replace_all_replaces_all() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "foo foo foo").unwrap();
        let args = serde_json::json!({
            "path": f.path().to_str().unwrap(),
            "old_string": "foo",
            "new_string": "bar",
            "replace_all": true
        });
        EditFileTool.execute(args, &ctx()).await.unwrap();
        let content = std::fs::read_to_string(f.path()).unwrap();
        assert!(!content.contains("foo"));
        assert_eq!(content.matches("bar").count(), 3);
    }

    #[tokio::test]
    async fn edit_not_found_returns_error() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "hello world").unwrap();
        let args = serde_json::json!({
            "path": f.path().to_str().unwrap(),
            "old_string": "DOES_NOT_EXIST",
            "new_string": "x"
        });
        let err = EditFileTool.execute(args, &ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(ref m) if m.contains("not found")));
    }

    #[tokio::test]
    async fn edit_identical_strings_returns_error() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "hello").unwrap();
        let args = serde_json::json!({
            "path": f.path().to_str().unwrap(),
            "old_string": "hello",
            "new_string": "hello"
        });
        let err = EditFileTool.execute(args, &ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(ref m) if m.contains("identical")));
    }

    #[tokio::test]
    async fn edit_trim_fallback_matches_trailing_space() {
        // File has trailing spaces on a line; old_string does not.
        let f = NamedTempFile::new().unwrap();
        // Write a file where the matching line has a trailing space.
        std::fs::write(f.path(), "fn foo() {   \n    let x = 1;\n}\n").unwrap();
        let args = serde_json::json!({
            "path": f.path().to_str().unwrap(),
            "old_string": "fn foo() {\n    let x = 1;\n}",
            "new_string": "fn foo() {\n    let x = 2;\n}"
        });
        let out = EditFileTool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("1 occurrence"));
        let content = std::fs::read_to_string(f.path()).unwrap();
        assert!(content.contains("let x = 2"));
        // The trailing newline of the matched region must be preserved.
        assert!(
            content.ends_with("}\n"),
            "trailing newline must survive edit"
        );
    }

    #[tokio::test]
    async fn edit_trim_fallback_preserves_final_newline_at_eof() {
        // The first line carries trailing spaces so exact matching fails and the
        // line-trim fallback runs; the matched block reaches the file's last
        // line, whose trailing newline must be preserved.
        let f = NamedTempFile::new().unwrap();
        std::fs::write(f.path(), "a   \nb\n").unwrap();
        let args = serde_json::json!({
            "path": f.path().to_str().unwrap(),
            "old_string": "a\nb",
            "new_string": "A\nB"
        });
        EditFileTool.execute(args, &ctx()).await.unwrap();
        let content = std::fs::read_to_string(f.path()).unwrap();
        // Before the fix this dropped the trailing '\n' (became "A\nB").
        assert_eq!(
            content, "A\nB\n",
            "final newline must survive trim fallback"
        );
    }

    // ---- diff capture tests ----

    #[tokio::test]
    async fn edit_emits_diff_with_old_and_new() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "hello world").unwrap();
        let args = serde_json::json!({
            "path": f.path().to_str().unwrap(),
            "old_string": "world",
            "new_string": "Rust"
        });
        let out = EditFileTool.execute(args, &ctx()).await.unwrap();
        assert_eq!(out.diffs.len(), 1, "edit must attach exactly one diff");
        let diff = &out.diffs[0];
        assert_eq!(diff.old_text.as_deref(), Some("hello world\n"));
        assert_eq!(diff.new_text, "hello Rust\n");
        assert_eq!(diff.path.as_path(), f.path());
    }

    #[tokio::test]
    async fn write_emits_diff_for_overwrite() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "old content").unwrap();
        let args = serde_json::json!({
            "path": f.path().to_str().unwrap(),
            "content": "new content"
        });
        let out = WriteFileTool.execute(args, &ctx()).await.unwrap();
        assert_eq!(out.diffs.len(), 1);
        assert_eq!(out.diffs[0].old_text.as_deref(), Some("old content\n"));
        assert_eq!(out.diffs[0].new_text, "new content");
    }

    #[tokio::test]
    async fn write_emits_create_diff_for_new_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("fresh.txt");
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "content": "brand new\n"
        });
        let out = WriteFileTool.execute(args, &ctx()).await.unwrap();
        assert_eq!(out.diffs.len(), 1);
        assert!(
            out.diffs[0].old_text.is_none(),
            "a freshly created file has no old text"
        );
        assert_eq!(out.diffs[0].new_text, "brand new\n");
    }

    #[tokio::test]
    async fn write_skips_diff_when_overwriting_binary_file() {
        // Overwriting a non-UTF-8 (binary) file must NOT render as a creation
        // diff (which would hide that prior content was destroyed); show none.
        let f = NamedTempFile::new().unwrap();
        std::fs::write(f.path(), [0x00u8, 0x9f, 0x92, 0x96]).unwrap();
        let args = serde_json::json!({
            "path": f.path().to_str().unwrap(),
            "content": "now plain text\n"
        });
        let out = WriteFileTool.execute(args, &ctx()).await.unwrap();
        assert!(
            out.diffs.is_empty(),
            "binary overwrite shows no (misleading) diff"
        );
        assert!(out.text.contains("wrote"), "text result still present");
    }

    #[test]
    fn attach_diff_drops_oversized() {
        let big = "x".repeat(MAX_DIFF_CAPTURE_BYTES + 1);
        let out = attach_diff(ToolOutput::text("ok"), Path::new("/tmp/a"), Some("a"), &big);
        assert!(out.diffs.is_empty(), "oversized diff must be dropped");
        assert_eq!(
            out.text, "ok",
            "text result is preserved when diff is dropped"
        );
    }

    #[test]
    fn attach_diff_keeps_small() {
        let out = attach_diff(
            ToolOutput::text("ok"),
            Path::new("/tmp/a"),
            Some("a\n"),
            "b\n",
        );
        assert_eq!(out.diffs.len(), 1);
        assert_eq!(out.diffs[0].old_text.as_deref(), Some("a\n"));
    }
}

// Rust guideline compliant 2026-02-21

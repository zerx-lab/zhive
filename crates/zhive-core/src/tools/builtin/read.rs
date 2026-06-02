//! [`ReadFileTool`]: read a file with optional line offset and limit.

use std::fmt::Write as FmtWrite;

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::builtin::{
    DEFAULT_READ_LINE_LIMIT, MAX_LINE_BYTES, MAX_TOOL_OUTPUT_BYTES, clamp_output, resolve_path,
    truncate_utf8,
};
use crate::tools::{Tool, ToolContext, ToolError, ToolKind, ToolOutput};

// ============================================================
// ReadFileTool
// ============================================================

/// Reads a file and returns its contents with `cat -n`-style line numbers.
///
/// Arguments (`path` is required; `offset` and `limit` are optional):
///
/// ```json
/// { "path": "/abs/or/rel/file.txt", "offset": 10, "limit": 50 }
/// ```
///
/// `offset` is 1-based (the first line is line 1). When omitted the file is
/// read from the beginning. `limit` caps the number of lines returned;
/// defaults to [`DEFAULT_READ_LINE_LIMIT`]. Binary files (detected via NUL
/// bytes or non-UTF-8 content) are rejected with an [`ToolError::Execution`].
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::ReadFileTool;
/// use zhive_core::tools::Tool;
/// assert_eq!(ReadFileTool.name(), "read");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn description(&self) -> Option<String> {
        Some(
            "Read a file and return its contents with line numbers. \
             Supports offset and limit for large files."
                .to_owned(),
        )
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path":   { "type": "string",  "description": "Absolute or relative path to the file." },
                "offset": { "type": "integer", "minimum": 1,  "description": "1-based start line (default: 1)." },
                "limit":  { "type": "integer", "minimum": 1,  "description": "Maximum number of lines to return." }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    /// Executes the read operation.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] if the file is missing, unreadable,
    /// or contains binary data.
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path_str = args["path"]
            .as_str()
            .ok_or_else(|| ToolError::Execution("`path` must be a string".to_owned()))?;

        let offset = args["offset"]
            .as_u64()
            .map_or(1usize, |v| usize::try_from(v).unwrap_or(usize::MAX))
            .max(1);
        let limit = args["limit"].as_u64().map_or(DEFAULT_READ_LINE_LIMIT, |v| {
            usize::try_from(v).unwrap_or(usize::MAX)
        });

        let abs_path = resolve_path(path_str);

        let raw = tokio::fs::read(&abs_path).await.map_err(|e| {
            ToolError::Execution(format!("cannot read `{}`: {e}", abs_path.display()))
        })?;

        // Reject binary content (NUL bytes or non-UTF-8 sequences).
        if raw.contains(&0u8) {
            return Err(ToolError::Execution(format!(
                "binary file (NUL byte detected): `{}`",
                abs_path.display()
            )));
        }
        let text = std::str::from_utf8(&raw).map_err(|_utf8_err| {
            ToolError::Execution(format!(
                "binary file (non-UTF-8 content): `{}`",
                abs_path.display()
            ))
        })?;

        // Collect the requested slice of lines.
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        // Convert 1-based offset to 0-based index.
        let start = offset.saturating_sub(1).min(total);
        let end = (start + limit).min(total);

        let mut out = String::new();
        let width = if total == 0 {
            1
        } else {
            // Width of the largest line number we will print.
            let max_lineno = end; // end is already 1-based at most.
            format!("{max_lineno}").len()
        };

        for (i, line) in lines[start..end].iter().enumerate() {
            let lineno = start + i + 1; // 1-based
            let (truncated_line, was_cut) = truncate_utf8(line, MAX_LINE_BYTES);
            if was_cut {
                let _ = writeln!(out, "{lineno:>width$}\t{truncated_line}<line truncated>");
            } else {
                let _ = writeln!(out, "{lineno:>width$}\t{truncated_line}");
            }
            // Bail early if we have already exceeded the output byte cap.
            if out.len() > MAX_TOOL_OUTPUT_BYTES {
                break;
            }
        }

        if end < total {
            let _ = writeln!(
                out,
                "(showing lines {}-{} of {total}; use `offset`/`limit` to read more)",
                start + 1,
                end
            );
        }

        Ok(ToolOutput::text(clamp_output(out)))
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use std::io::Write as IoWrite;
    use std::sync::Arc;

    use tempfile::NamedTempFile;
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

    #[tokio::test]
    async fn read_simple_file() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "alpha").unwrap();
        writeln!(f, "beta").unwrap();
        writeln!(f, "gamma").unwrap();

        let args = serde_json::json!({ "path": f.path().to_str().unwrap() });
        let out = ReadFileTool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("alpha"));
        assert!(out.text.contains("beta"));
        assert!(out.text.contains("gamma"));
        // Line numbers must be present.
        assert!(out.text.contains('\t'));
    }

    #[tokio::test]
    async fn read_with_offset_and_limit() {
        let mut f = NamedTempFile::new().unwrap();
        for i in 1..=10u32 {
            writeln!(f, "line {i}").unwrap();
        }
        let args = serde_json::json!({
            "path": f.path().to_str().unwrap(),
            "offset": 3,
            "limit": 2
        });
        let out = ReadFileTool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("line 3"));
        assert!(out.text.contains("line 4"));
        assert!(!out.text.contains("line 1"));
        assert!(!out.text.contains("line 5"));
    }

    #[tokio::test]
    async fn read_rejects_binary_nul() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"hello\x00world").unwrap();
        let args = serde_json::json!({ "path": f.path().to_str().unwrap() });
        let err = ReadFileTool.execute(args, &ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(ref m) if m.contains("binary")));
    }

    #[tokio::test]
    async fn read_rejects_non_utf8() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"\xff\xfe invalid").unwrap();
        let args = serde_json::json!({ "path": f.path().to_str().unwrap() });
        let err = ReadFileTool.execute(args, &ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(ref m) if m.contains("binary")));
    }

    #[tokio::test]
    async fn read_missing_file_returns_error() {
        let args = serde_json::json!({ "path": "/tmp/__zhive_nonexistent_abc123.txt" });
        let err = ReadFileTool.execute(args, &ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[tokio::test]
    async fn read_long_line_truncated() {
        let long_line = "x".repeat(MAX_LINE_BYTES + 100);
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "{long_line}").unwrap();
        let args = serde_json::json!({ "path": f.path().to_str().unwrap() });
        let out = ReadFileTool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("<line truncated>"));
    }

    #[tokio::test]
    async fn read_shows_continuation_hint_when_limited() {
        let mut f = NamedTempFile::new().unwrap();
        for i in 1..=5u32 {
            writeln!(f, "line {i}").unwrap();
        }
        let args = serde_json::json!({
            "path": f.path().to_str().unwrap(),
            "limit": 2
        });
        let out = ReadFileTool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("showing lines"));
    }
}

// Rust guideline compliant 2026-02-21

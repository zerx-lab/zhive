//! [`GrepTool`] and [`GlobTool`]: directory-search tools.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::builtin::{
    DEFAULT_GLOB_MAX_RESULTS, DEFAULT_GREP_MAX_RESULTS, MAX_LINE_BYTES, clamp_output, resolve_path,
    truncate_utf8,
};
use crate::tools::{Tool, ToolContext, ToolError, ToolKind, ToolOutput};

// ============================================================
// GrepTool
// ============================================================

/// Searches file contents with a regex, respecting `.gitignore`.
///
/// Arguments:
///
/// ```json
/// {
///   "pattern":     "fn execute",
///   "path":        "/opt/project",
///   "glob":        "*.rs",
///   "ignore_case": false,
///   "max_results": 200
/// }
/// ```
///
/// `path` defaults to `std::env::current_dir()`. Binary files and entries
/// that trigger I/O errors are silently skipped. Results are in the form
/// `relative_path:line_no:line_content`, one per line.
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::GrepTool;
/// use zhive_core::tools::Tool;
/// assert_eq!(GrepTool.name(), "grep");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn description(&self) -> Option<String> {
        Some(
            "Search file contents with a regular expression across a directory \
             tree. Honors .gitignore and skips binary files; results are \
             `path:line:text`, capped by `max_results`. Use `glob` to match by \
             file name or path instead, and `read` to view a specific file."
                .to_owned(),
        )
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern":     { "type": "string",  "description": "Regular expression to search for." },
                "path":        { "type": "string",  "description": "Root directory (default: cwd)." },
                "glob":        { "type": "string",  "description": "Glob filter on the file name or path relative to the search root (e.g. '*.rs' or 'src/**/*.rs')." },
                "ignore_case": { "type": "boolean", "description": "Case-insensitive matching." },
                "max_results": { "type": "integer", "minimum": 1, "description": "Result cap." }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    /// Executes the grep search.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] when `pattern` is not valid regex.
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| ToolError::Execution("`pattern` must be a string".to_owned()))?;
        let ignore_case = args["ignore_case"].as_bool().unwrap_or(false);
        let max_results = args["max_results"]
            .as_u64()
            .map_or(DEFAULT_GREP_MAX_RESULTS, |v| {
                usize::try_from(v).unwrap_or(usize::MAX)
            });
        let base = args["path"].as_str().map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            resolve_path,
        );
        let glob_filter: Option<glob::Pattern> = match args["glob"].as_str() {
            Some(g) => Some(
                glob::Pattern::new(g)
                    .map_err(|e| ToolError::Execution(format!("invalid glob `{g}`: {e}")))?,
            ),
            None => None,
        };

        let re = regex::RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
            .map_err(|e| ToolError::Execution(format!("invalid regex `{pattern}`: {e}")))?;

        // Run the directory walk on a blocking thread to avoid stalling the
        // async executor with synchronous I/O.
        let results = tokio::task::spawn_blocking(move || {
            grep_walk(&base, &re, glob_filter.as_ref(), max_results)
        })
        .await
        .map_err(|e| ToolError::Execution(format!("grep task panicked: {e}")))?;

        let (lines, truncated) = results;
        let mut out = lines.join("\n");
        if truncated {
            out.push_str("\n(truncated: result limit reached)");
        }

        Ok(ToolOutput::text(clamp_output(out)))
    }
}

/// Synchronously walks `base` and collects grep matches.
///
/// Returns `(lines, truncated)`.
fn grep_walk(
    base: &std::path::Path,
    re: &regex::Regex,
    glob_filter: Option<&glob::Pattern>,
    max_results: usize,
) -> (Vec<String>, bool) {
    let mut results: Vec<String> = Vec::new();
    let mut truncated = false;

    let walker = ignore::WalkBuilder::new(base)
        .hidden(false) // include dotfiles (respect .gitignore still applies)
        .git_ignore(true)
        .build();

    'walk: for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();

        // Apply the glob filter against the basename OR the path relative to
        // the search root, so both `*.rs` (basename) and `src/**/*.rs` (path)
        // patterns match. `*` never crosses `/`, so a path pattern would
        // otherwise silently never match a basename-only test.
        if let Some(pattern) = glob_filter {
            let rel = path.strip_prefix(base).unwrap_or(path);
            let name_match = path
                .file_name()
                .is_some_and(|n| pattern.matches(&n.to_string_lossy()));
            if !name_match && !pattern.matches_path(rel) {
                continue;
            }
        }

        // Read file; skip on I/O error.
        let Ok(raw) = std::fs::read(path) else {
            continue;
        };
        // Skip binary files.
        if raw.contains(&0u8) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&raw) else {
            continue;
        };

        let rel_path = path.strip_prefix(base).unwrap_or(path);

        for (lineno, line) in text.lines().enumerate() {
            if re.is_match(line) {
                let (line_text, _) = truncate_utf8(line, MAX_LINE_BYTES);
                results.push(format!("{}:{}:{line_text}", rel_path.display(), lineno + 1));
                if results.len() >= max_results {
                    truncated = true;
                    break 'walk;
                }
            }
        }
    }

    (results, truncated)
}

// ============================================================
// GlobTool
// ============================================================

/// Expands a glob pattern and returns sorted matching paths.
///
/// Arguments:
///
/// ```json
/// { "pattern": "src/**/*.rs", "path": "/opt/project" }
/// ```
///
/// `path` is used as the base directory for relative patterns. Returns up to
/// [`DEFAULT_GLOB_MAX_RESULTS`] sorted paths.
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::GlobTool;
/// use zhive_core::tools::Tool;
/// assert_eq!(GlobTool.name(), "glob");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn description(&self) -> Option<String> {
        Some(
            "Find files by a glob pattern (e.g. `src/**/*.rs`) and return sorted \
             paths. Use this to locate files by name or layout; use `grep` to \
             search inside file contents. Results are capped to a bounded number \
             of paths."
                .to_owned(),
        )
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern (e.g. 'src/**/*.rs')." },
                "path":    { "type": "string", "description": "Base directory (default: cwd)." }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    /// Executes the glob expansion.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] when `pattern` is not a valid glob.
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let pattern = args["pattern"]
            .as_str()
            .ok_or_else(|| ToolError::Execution("`pattern` must be a string".to_owned()))?
            .to_owned();
        let base = args["path"].as_str().map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            resolve_path,
        );

        let paths = tokio::task::spawn_blocking(move || glob_expand(&base, &pattern))
            .await
            .map_err(|e| ToolError::Execution(format!("glob task panicked: {e}")))?
            .map_err(ToolError::Execution)?;

        let text = paths.join("\n");
        Ok(ToolOutput::text(clamp_output(text)))
    }
}

/// Expands `pattern` relative to `base` and returns sorted paths.
///
/// # Errors
///
/// Returns an error string when the glob pattern is invalid.
fn glob_expand(base: &std::path::Path, pattern: &str) -> Result<Vec<String>, String> {
    let full_pattern = base.join(pattern);
    let full_str = full_pattern.to_string_lossy();

    let entries = glob::glob(&full_str).map_err(|e| format!("invalid glob `{pattern}`: {e}"))?;

    let mut paths: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        if paths.len() >= DEFAULT_GLOB_MAX_RESULTS {
            break;
        }
        paths.push(entry.to_string_lossy().into_owned());
    }
    paths.sort();
    Ok(paths)
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;
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

    // ---- GrepTool tests ----

    #[tokio::test]
    async fn grep_finds_pattern() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world\nfoo bar\n").unwrap();
        let args = serde_json::json!({
            "pattern": "hello",
            "path": dir.path().to_str().unwrap()
        });
        let out = GrepTool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("hello"));
        assert!(out.text.contains("a.txt"));
    }

    #[tokio::test]
    async fn grep_ignore_case() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("b.txt"), "HELLO WORLD\n").unwrap();
        let args = serde_json::json!({
            "pattern": "hello",
            "path": dir.path().to_str().unwrap(),
            "ignore_case": true
        });
        let out = GrepTool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("HELLO"));
    }

    #[tokio::test]
    async fn grep_invalid_regex_returns_error() {
        let dir = TempDir::new().unwrap();
        let args = serde_json::json!({
            "pattern": "[invalid",
            "path": dir.path().to_str().unwrap()
        });
        let err = GrepTool.execute(args, &ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(ref m) if m.contains("invalid regex")));
    }

    #[tokio::test]
    async fn grep_max_results_truncates() {
        let dir = TempDir::new().unwrap();
        let many_lines: String = (0..20u32).fold(String::new(), |mut s, i| {
            use std::fmt::Write as _;
            let _ = writeln!(s, "match line {i}");
            s
        });
        std::fs::write(dir.path().join("c.txt"), many_lines).unwrap();
        let args = serde_json::json!({
            "pattern": "match",
            "path": dir.path().to_str().unwrap(),
            "max_results": 5
        });
        let out = GrepTool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("truncated"));
    }

    #[tokio::test]
    async fn grep_glob_filter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "fn main() {}\n").unwrap();
        let args = serde_json::json!({
            "pattern": "fn main",
            "path": dir.path().to_str().unwrap(),
            "glob": "*.rs"
        });
        let out = GrepTool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("a.rs"));
        assert!(!out.text.contains("b.txt"));
    }

    #[tokio::test]
    async fn grep_glob_filter_matches_path_pattern() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src").join("deep")).unwrap();
        std::fs::create_dir_all(dir.path().join("other")).unwrap();
        std::fs::write(
            dir.path().join("src").join("deep").join("x.rs"),
            "fn target() {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("other").join("y.rs"), "fn target() {}\n").unwrap();

        // A path-scoped glob matches only the file beneath src/.
        let args = serde_json::json!({
            "pattern": "fn target",
            "path": dir.path().to_str().unwrap(),
            "glob": "src/**/*.rs"
        });
        let out = GrepTool.execute(args, &ctx()).await.unwrap();
        assert!(
            out.text.contains("x.rs"),
            "src match expected: {}",
            out.text
        );
        assert!(
            !out.text.contains("y.rs"),
            "other/ must be excluded: {}",
            out.text
        );

        // A basename glob still matches nested files (basename OR path).
        let args = serde_json::json!({
            "pattern": "fn target",
            "path": dir.path().to_str().unwrap(),
            "glob": "*.rs"
        });
        let out = GrepTool.execute(args, &ctx()).await.unwrap();
        assert!(
            out.text.contains("x.rs") && out.text.contains("y.rs"),
            "basename glob matches both nested files: {}",
            out.text
        );
    }

    // ---- GlobTool tests ----

    #[tokio::test]
    async fn glob_finds_files() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::write(dir.path().join("c.txt"), "").unwrap();
        let args = serde_json::json!({
            "pattern": "*.rs",
            "path": dir.path().to_str().unwrap()
        });
        let out = GlobTool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("a.rs"));
        assert!(out.text.contains("b.rs"));
        assert!(!out.text.contains("c.txt"));
    }

    #[tokio::test]
    async fn glob_results_are_sorted() {
        let dir = TempDir::new().unwrap();
        // Create files in reverse alphabetical order.
        std::fs::write(dir.path().join("z.rs"), "").unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("m.rs"), "").unwrap();
        let args = serde_json::json!({
            "pattern": "*.rs",
            "path": dir.path().to_str().unwrap()
        });
        let out = GlobTool.execute(args, &ctx()).await.unwrap();
        let lines: Vec<&str> = out.text.lines().collect();
        assert!(
            lines[0].ends_with("a.rs"),
            "first should be a.rs, got {}",
            lines[0]
        );
    }
}

// Rust guideline compliant 2026-02-21

//! [`GrepTool`], [`GlobTool`], and [`ListDirTool`]: directory-search tools.

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
        Some("Search file contents using a regex pattern across a directory tree.".to_owned())
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern":     { "type": "string",  "description": "Regular expression to search for." },
                "path":        { "type": "string",  "description": "Root directory (default: cwd)." },
                "glob":        { "type": "string",  "description": "Filename glob filter (e.g. '*.rs')." },
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

        // Apply glob filter on file name only.
        if let Some(pattern) = glob_filter {
            let file_name = path.file_name().map(|n| n.to_string_lossy());
            if !file_name.is_some_and(|n| pattern.matches(&n)) {
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
        Some("Expand a glob pattern and return sorted matching file paths.".to_owned())
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
// ListDirTool
// ============================================================

/// Lists directory entries with type, size, and name.
///
/// Arguments:
///
/// ```json
/// { "path": "/opt/project" }
/// ```
///
/// `path` defaults to `cwd`. Each line is `<type> <size_bytes> <name>` where
/// type is `d` (directory), `f` (file), or `l` (symlink). Entries are sorted
/// by name.
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::ListDirTool;
/// use zhive_core::tools::Tool;
/// assert_eq!(ListDirTool.name(), "ls");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &'static str {
        "ls"
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn description(&self) -> Option<String> {
        Some("List directory entries with type, size, and name.".to_owned())
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list (default: cwd)." }
            },
            "additionalProperties": false
        })
    }

    /// Executes the directory listing.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] when `path` is not a readable
    /// directory.
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let dir = args["path"].as_str().map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            resolve_path,
        );

        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .map_err(|e| ToolError::Execution(format!("cannot list `{}`: {e}", dir.display())))?;

        let mut rows: Vec<(String, String, u64)> = Vec::new(); // (type, name, size)

        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| ToolError::Execution(format!("error reading directory entry: {e}")))?
        {
            let meta = entry.metadata().await;
            let (type_char, size) = match meta {
                Ok(m) if m.is_dir() => ("d", 0u64),
                Ok(m) if m.is_symlink() => ("l", m.len()),
                Ok(m) => ("f", m.len()),
                Err(_) => ("?", 0u64),
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            rows.push((type_char.to_owned(), name, size));
        }

        rows.sort_by(|a, b| a.1.cmp(&b.1));

        let text = rows.iter().fold(String::new(), |mut acc, (t, name, sz)| {
            use std::fmt::Write as _;
            let _ = writeln!(acc, "{t} {sz:>10} {name}");
            acc
        });

        Ok(ToolOutput::text(clamp_output(text)))
    }
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

    // ---- ListDirTool tests ----

    #[tokio::test]
    async fn ls_lists_entries() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("file.txt"), "content").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let args = serde_json::json!({ "path": dir.path().to_str().unwrap() });
        let out = ListDirTool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("file.txt"));
        assert!(out.text.contains("subdir"));
        assert!(out.text.contains('f'));
        assert!(out.text.contains('d'));
    }

    #[tokio::test]
    async fn ls_nonexistent_dir_returns_error() {
        let args = serde_json::json!({ "path": "/tmp/__zhive_nonexistent_dir_abc" });
        let err = ListDirTool.execute(args, &ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(_)));
    }
}

// Rust guideline compliant 2026-02-21

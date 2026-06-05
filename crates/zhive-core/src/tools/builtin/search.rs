//! [`GrepTool`] and [`GlobTool`]: directory-search tools.

use std::ffi::OsStr;
use std::path::Path;

use async_trait::async_trait;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks::UTF8};
use serde_json::Value;

use crate::tools::builtin::{
    DEFAULT_GLOB_MAX_RESULTS, DEFAULT_GREP_MAX_RESULTS, MAX_LINE_BYTES, clamp_output, resolve_path,
    truncate_utf8,
};
use crate::tools::{Tool, ToolContext, ToolError, ToolKind, ToolOutput};

/// Builds the shared directory walker for `grep` and `glob`.
///
/// A single helper keeps the two tools' traversal semantics identical: the
/// `respect_gitignore` flag toggles every gitignore-style source uniformly, and
/// a `.git` directory is **always** excluded via `filter_entry` — independent of
/// `respect_gitignore` — so repository internals never leak into results. The
/// `.hidden(false)` setting is constant so other dotfiles stay searchable.
fn search_walker(base: &Path, respect_gitignore: bool) -> ignore::Walk {
    let r = respect_gitignore;
    ignore::WalkBuilder::new(base)
        .hidden(false)
        .git_ignore(r)
        .git_global(r)
        .git_exclude(r)
        .ignore(r)
        .parents(r)
        .filter_entry(|e| e.file_name() != OsStr::new(".git"))
        .build()
}

// ============================================================
// GrepTool
// ============================================================

/// Searches file contents with a regex over a directory tree.
///
/// The search engine is ripgrep's [`grep_searcher`], which applies a SIMD
/// pre-filter before the regex and detects binary data. The traversal uses the
/// shared [`search_walker`]: a `.git` directory is **always** excluded, and the
/// `respect_gitignore` field (passed to [`GrepTool::new`]) toggles whether
/// `.gitignore`, `.ignore`, global git excludes, and parent ignores are honored.
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
/// assert_eq!(GrepTool::new(true).name(), "grep");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct GrepTool {
    /// When `true`, gitignore-style exclusions are honored during traversal.
    respect_gitignore: bool,
}

impl GrepTool {
    /// Creates a `GrepTool` with the given gitignore-respecting behavior.
    ///
    /// When `respect_gitignore` is `true`, `.gitignore`/`.ignore` files, global
    /// git excludes, and parent ignores are honored; when `false`, only the
    /// always-on `.git` directory exclusion applies.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::tools::builtin::GrepTool;
    /// use zhive_core::tools::Tool;
    /// assert_eq!(GrepTool::new(true).name(), "grep");
    /// ```
    #[must_use]
    pub fn new(respect_gitignore: bool) -> Self {
        Self { respect_gitignore }
    }
}

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
             tree, powered by ripgrep's grep-searcher (SIMD pre-filtered). The \
             .git directory is always excluded; .gitignore is honored by \
             default (configurable). Binary files are skipped; results are \
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

        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(ignore_case)
            .build(pattern)
            .map_err(|e| ToolError::Execution(format!("invalid regex `{pattern}`: {e}")))?;

        let respect = self.respect_gitignore;
        // Run the directory walk on a blocking thread to avoid stalling the
        // async executor with synchronous I/O.
        let results = tokio::task::spawn_blocking(move || {
            grep_walk(&base, &matcher, glob_filter.as_ref(), max_results, respect)
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

/// Synchronously walks `base` and collects grep matches via [`grep_searcher`].
///
/// Returns `(lines, truncated)`. Each line is `relative_path:line_no:text`.
/// `matcher` is built once in [`GrepTool::execute`] and reused across every
/// file. Files that fail to open or read are silently skipped, and binary data
/// is dropped by [`BinaryDetection::quit`]. `respect_gitignore` is forwarded to
/// [`search_walker`].
fn grep_walk(
    base: &Path,
    matcher: &RegexMatcher,
    glob_filter: Option<&glob::Pattern>,
    max_results: usize,
    respect_gitignore: bool,
) -> (Vec<String>, bool) {
    let mut results: Vec<String> = Vec::new();
    let mut truncated = false;

    // Build the searcher once and reuse it for every file. `BinaryDetection`
    // must be set explicitly (default is `none`, which would emit binary
    // garbage); `line_number(true)` is required for the UTF8 sink to receive
    // line numbers. The default `MmapChoice` is `never` (buffered I/O), so no
    // `unsafe` memory map is engaged.
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .build();

    for entry in search_walker(base, respect_gitignore).flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();

        // Apply the glob filter against the basename OR the path relative to
        // the search root, so both `*.rs` (basename) and `src/**/*.rs` (path)
        // patterns match. `*` never crosses `/`, so a path pattern would
        // otherwise silently never match a basename-only test.
        if let Some(pattern) = glob_filter
            && !glob_path_matches(pattern, path, base)
        {
            continue;
        }

        let rel_path = path.strip_prefix(base).unwrap_or(path);

        // `lnum` is 1-based, so it is used directly with no `+1` adjustment.
        // The sink line includes the trailing newline; trim only newline
        // characters (not significant trailing spaces/tabs). An I/O error on a
        // single file is ignored so the walk continues, matching the old
        // `fs::read`-failure skip behavior.
        let _ = searcher.search_path(
            matcher,
            path,
            UTF8(|lnum, line| {
                let trimmed = line.trim_end_matches(['\n', '\r']);
                let (line_text, _) = truncate_utf8(trimmed, MAX_LINE_BYTES);
                results.push(format!("{}:{lnum}:{line_text}", rel_path.display()));
                if results.len() >= max_results {
                    truncated = true;
                    return Ok(false);
                }
                Ok(true)
            }),
        );

        if truncated {
            break;
        }
    }

    (results, truncated)
}

// ============================================================
// GlobTool
// ============================================================

/// Finds files matching a glob pattern over a directory tree.
///
/// Uses the shared [`search_walker`] (same configuration as `grep`) so that the
/// `.git` directory is **always** excluded and, when `respect_gitignore` (passed
/// to [`GlobTool::new`]) is `true`, `target/`, `node_modules/`, and any path
/// listed in a `.gitignore` are excluded too. A plain basename pattern such as
/// `*.rs` matches files at **all** directory depths (not just the root), because
/// the pattern is tested against both the file's basename and its path relative
/// to the search root. A path-scoped pattern like `src/**/*.rs` restricts
/// matches to that subtree. Results are absolute paths, sorted alphabetically,
/// capped at [`DEFAULT_GLOB_MAX_RESULTS`] with a truncation notice when the
/// limit is hit.
///
/// Arguments:
///
/// ```json
/// { "pattern": "src/**/*.rs", "path": "/opt/project", "max_results": 1000 }
/// ```
///
/// `path` defaults to `std::env::current_dir()`; `max_results` defaults to
/// [`DEFAULT_GLOB_MAX_RESULTS`].
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::GlobTool;
/// use zhive_core::tools::Tool;
/// assert_eq!(GlobTool::new(true).name(), "glob");
/// ```
#[derive(Debug, Clone, Copy)]
pub struct GlobTool {
    /// When `true`, gitignore-style exclusions are honored during traversal.
    respect_gitignore: bool,
}

impl GlobTool {
    /// Creates a `GlobTool` with the given gitignore-respecting behavior.
    ///
    /// When `respect_gitignore` is `true`, `.gitignore`/`.ignore` files, global
    /// git excludes, and parent ignores are honored; when `false`, only the
    /// always-on `.git` directory exclusion applies.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::tools::builtin::GlobTool;
    /// use zhive_core::tools::Tool;
    /// assert_eq!(GlobTool::new(true).name(), "glob");
    /// ```
    #[must_use]
    pub fn new(respect_gitignore: bool) -> Self {
        Self { respect_gitignore }
    }
}

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
            "Find files by glob pattern; returns sorted absolute paths. The \
             .git directory is always excluded; .gitignore is honored by \
             default (configurable). A plain pattern like `*.rs` matches all \
             depths (basename check). A path pattern like `src/**/*.rs` is \
             matched against the relative path. Results are capped by \
             `max_results`; a truncation notice is appended when the limit is \
             reached. Use `grep` to search inside file contents."
                .to_owned(),
        )
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern":     { "type": "string",  "description": "Glob pattern (e.g. '*.rs' or 'src/**/*.rs')." },
                "path":        { "type": "string",  "description": "Base directory (default: cwd)." },
                "max_results": { "type": "integer", "minimum": 1, "description": "Result cap (default: 1000)." }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    /// Executes the glob search.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] when `pattern` is not a valid glob or
    /// the blocking walk task panics.
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let pattern_str = args["pattern"]
            .as_str()
            .ok_or_else(|| ToolError::Execution("`pattern` must be a string".to_owned()))?;
        let compiled = glob::Pattern::new(pattern_str)
            .map_err(|e| ToolError::Execution(format!("invalid glob `{pattern_str}`: {e}")))?;
        let base = args["path"].as_str().map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            resolve_path,
        );
        let max_results = args["max_results"]
            .as_u64()
            .map_or(DEFAULT_GLOB_MAX_RESULTS, |v| {
                usize::try_from(v).unwrap_or(usize::MAX)
            });

        let respect = self.respect_gitignore;
        let (paths, truncated) =
            tokio::task::spawn_blocking(move || glob_walk(&base, &compiled, max_results, respect))
                .await
                .map_err(|e| ToolError::Execution(format!("glob task panicked: {e}")))?;

        let mut text = paths.join("\n");
        if truncated {
            text.push_str("\n(truncated: result limit reached)");
        }

        Ok(ToolOutput::text(clamp_output(text)))
    }
}

/// Synchronously walks `base` and collects paths matching `pattern`.
///
/// Uses the shared [`search_walker`] (same configuration as [`grep_walk`]):
/// `.hidden(false)` (dotfiles visible), a `.git` directory always excluded, and
/// `respect_gitignore` toggling whether `.gitignore`-style files are honored.
/// Returns `(sorted_absolute_paths, truncated)`.
fn glob_walk(
    base: &Path,
    pattern: &glob::Pattern,
    max_results: usize,
    respect_gitignore: bool,
) -> (Vec<String>, bool) {
    let mut paths: Vec<String> = Vec::new();
    let mut truncated = false;

    for entry in search_walker(base, respect_gitignore).flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();

        if !glob_path_matches(pattern, path, base) {
            continue;
        }

        paths.push(path.to_string_lossy().into_owned());
        if paths.len() >= max_results {
            truncated = true;
            break;
        }
    }

    paths.sort();
    (paths, truncated)
}

/// Returns `true` when `path` matches `pattern` by basename or relative path.
///
/// Tests the glob against both the file's basename (enabling `*.rs` to match
/// files at any depth) and the path relative to `base` (enabling scoped
/// patterns like `src/**/*.rs`). Used by both `grep_walk` and `glob_walk` to
/// guarantee consistent match semantics across the two tools.
fn glob_path_matches(
    pattern: &glob::Pattern,
    path: &std::path::Path,
    base: &std::path::Path,
) -> bool {
    let rel = path.strip_prefix(base).unwrap_or(path);
    let name_match = path
        .file_name()
        .is_some_and(|n| pattern.matches(&n.to_string_lossy()));
    name_match || pattern.matches_path(rel)
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
        let out = GrepTool::new(true).execute(args, &ctx()).await.unwrap();
        // Pin the exact `relative_path:line_no:text` shape: line 1 (1-based, no
        // off-by-one) and the line text trimmed of its trailing newline. A
        // stray `+1` on the line number or a missing trim would break this.
        assert!(
            out.text.contains("a.txt:1:hello world"),
            "expected exact `a.txt:1:hello world`, got: {}",
            out.text
        );
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
        let out = GrepTool::new(true).execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("HELLO"));
    }

    #[tokio::test]
    async fn grep_invalid_regex_returns_error() {
        let dir = TempDir::new().unwrap();
        let args = serde_json::json!({
            "pattern": "[invalid",
            "path": dir.path().to_str().unwrap()
        });
        let err = GrepTool::new(true).execute(args, &ctx()).await.unwrap_err();
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
        let out = GrepTool::new(true).execute(args, &ctx()).await.unwrap();
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
        let out = GrepTool::new(true).execute(args, &ctx()).await.unwrap();
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
        let out = GrepTool::new(true).execute(args, &ctx()).await.unwrap();
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
        let out = GrepTool::new(true).execute(args, &ctx()).await.unwrap();
        assert!(
            out.text.contains("x.rs") && out.text.contains("y.rs"),
            "basename glob matches both nested files: {}",
            out.text
        );
    }

    /// `.gitignore` exclusions are honored by `GrepTool` when a `.git` directory is present.
    ///
    /// `ignore::WalkBuilder` only activates gitignore processing when it detects
    /// a `.git` directory. The test creates one explicitly to trigger the check.
    /// This mirrors `glob_respects_gitignore` to ensure the shared `WalkBuilder`
    /// configuration is not accidentally broken on the grep path.
    #[tokio::test]
    async fn grep_respects_gitignore() {
        let dir = TempDir::new().unwrap();
        // A real .git dir is needed; WalkBuilder requires_git defaults to true.
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("kept.rs"), "hello\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "hello\n").unwrap();

        let args = serde_json::json!({
            "pattern": "hello",
            "path": dir.path().to_str().unwrap()
        });
        let out = GrepTool::new(true).execute(args, &ctx()).await.unwrap();
        assert!(
            out.text.contains("kept.rs"),
            "kept.rs should appear in grep results: {}",
            out.text
        );
        assert!(
            !out.text.contains("ignored.rs"),
            "ignored.rs must be excluded by .gitignore: {}",
            out.text
        );
    }

    /// Binary files are dropped by `BinaryDetection::quit`, not searched.
    ///
    /// The NUL is placed as the **first** byte so the outcome is deterministic
    /// regardless of buffer scanning. With binary detection omitted (default
    /// `none`), the NUL is valid UTF-8 and the sink would emit the binary line,
    /// so `bin.dat` would appear — this test fails loudly in that case.
    #[tokio::test]
    async fn grep_skips_binary_files() {
        let dir = TempDir::new().unwrap();
        // Binary file: NUL first, then a pattern that also lives in the text file.
        std::fs::write(dir.path().join("bin.dat"), b"\x00needle here").unwrap();
        std::fs::write(dir.path().join("txt.txt"), "needle here\n").unwrap();

        let args = serde_json::json!({
            "pattern": "needle",
            "path": dir.path().to_str().unwrap()
        });
        let out = GrepTool::new(true).execute(args, &ctx()).await.unwrap();
        assert!(
            out.text.contains("txt.txt"),
            "the text file's match must appear: {}",
            out.text
        );
        assert!(
            !out.text.contains("bin.dat"),
            "binary file content must not appear: {}",
            out.text
        );
        assert!(
            !out.text.contains('\u{0}'),
            "no NUL byte may leak into output: {:?}",
            out.text
        );
    }

    /// The `.git` directory is never searched, even with real files inside.
    ///
    /// `filter_entry` excludes `.git` unconditionally (independent of
    /// `respect_gitignore`). An empty `.git` dir would pass even if the filter
    /// were broken, so a real file is placed inside to make the test meaningful.
    #[tokio::test]
    async fn grep_git_dir_not_leaked() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("config"), "hello\n").unwrap();
        std::fs::write(dir.path().join("kept.rs"), "hello\n").unwrap();

        let args = serde_json::json!({
            "pattern": "hello",
            "path": dir.path().to_str().unwrap()
        });
        let out = GrepTool::new(true).execute(args, &ctx()).await.unwrap();
        assert!(
            out.text.contains("kept.rs"),
            "kept.rs should appear: {}",
            out.text
        );
        assert!(
            !out.text.contains(".git"),
            ".git internals must never leak: {}",
            out.text
        );
    }

    /// With `respect_gitignore = false`, `.gitignore` is ignored but `.git` is not.
    ///
    /// Drives `GrepTool::new(false)` so the field-to-walker wiring is exercised.
    /// `ignored.rs` must now appear (gitignore not honored), while `.git`
    /// internals stay excluded — the `filter_entry` guard is independent of the
    /// `respect` flag.
    #[tokio::test]
    async fn grep_respect_false_ignores_gitignore_but_skips_git() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("config"), "hello\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("kept.rs"), "hello\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "hello\n").unwrap();

        let args = serde_json::json!({
            "pattern": "hello",
            "path": dir.path().to_str().unwrap()
        });
        let out = GrepTool::new(false).execute(args, &ctx()).await.unwrap();
        assert!(
            out.text.contains("kept.rs"),
            "kept.rs should appear: {}",
            out.text
        );
        assert!(
            out.text.contains("ignored.rs"),
            "ignored.rs must appear when gitignore is not respected: {}",
            out.text
        );
        assert!(
            !out.text.contains(".git"),
            ".git internals must never leak regardless of respect flag: {}",
            out.text
        );
    }

    // ---- GlobTool tests ----

    #[tokio::test]
    async fn glob_invalid_pattern_returns_error() {
        let dir = TempDir::new().unwrap();
        let args = serde_json::json!({
            "pattern": "[invalid",
            "path": dir.path().to_str().unwrap()
        });
        let err = GlobTool::new(true).execute(args, &ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(ref m) if m.contains("invalid glob")));
    }

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
        let out = GlobTool::new(true).execute(args, &ctx()).await.unwrap();
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
        let out = GlobTool::new(true).execute(args, &ctx()).await.unwrap();
        let lines: Vec<&str> = out.text.lines().collect();
        assert_eq!(lines.len(), 3, "expected exactly 3 results: {lines:?}");
        assert!(
            lines[0].ends_with("a.rs"),
            "pos 0 should be a.rs: {lines:?}"
        );
        assert!(
            lines[1].ends_with("m.rs"),
            "pos 1 should be m.rs: {lines:?}"
        );
        assert!(
            lines[2].ends_with("z.rs"),
            "pos 2 should be z.rs: {lines:?}"
        );
    }

    /// `*.rs` now matches `.rs` files at any depth, not just the root level.
    ///
    /// This is the intentional semantic change introduced when `GlobTool` was
    /// switched from `glob::glob(base/pattern)` to `ignore::WalkBuilder` +
    /// basename matching (consistent with `GrepTool`'s glob filter). The old
    /// implementation only matched top-level files because `glob::glob`'s `*`
    /// does not cross `/`. The new implementation tests the pattern against
    /// each file's basename, so `*.rs` recursively finds all `.rs` files.
    #[tokio::test]
    async fn glob_basename_pattern_matches_nested_files() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sub").join("deep")).unwrap();
        std::fs::write(dir.path().join("root.rs"), "").unwrap();
        std::fs::write(dir.path().join("sub").join("mid.rs"), "").unwrap();
        std::fs::write(dir.path().join("sub").join("deep").join("leaf.rs"), "").unwrap();
        std::fs::write(dir.path().join("sub").join("other.txt"), "").unwrap();

        let args = serde_json::json!({
            "pattern": "*.rs",
            "path": dir.path().to_str().unwrap()
        });
        let out = GlobTool::new(true).execute(args, &ctx()).await.unwrap();
        assert!(
            out.text.contains("root.rs"),
            "root.rs should match: {}",
            out.text
        );
        assert!(
            out.text.contains("mid.rs"),
            "mid.rs (1 level deep) should match: {}",
            out.text
        );
        assert!(
            out.text.contains("leaf.rs"),
            "leaf.rs (2 levels deep) should match: {}",
            out.text
        );
        assert!(
            !out.text.contains("other.txt"),
            "other.txt must not match: {}",
            out.text
        );
    }

    /// A path-scoped pattern like `sub/**/*.rs` restricts to that subtree only.
    #[tokio::test]
    async fn glob_path_scoped_pattern_restricts_subtree() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sub").join("deep")).unwrap();
        std::fs::create_dir_all(dir.path().join("other")).unwrap();
        std::fs::write(dir.path().join("sub").join("in_sub.rs"), "").unwrap();
        std::fs::write(dir.path().join("sub").join("deep").join("nested.rs"), "").unwrap();
        std::fs::write(dir.path().join("other").join("not_sub.rs"), "").unwrap();

        let args = serde_json::json!({
            "pattern": "sub/**/*.rs",
            "path": dir.path().to_str().unwrap()
        });
        let out = GlobTool::new(true).execute(args, &ctx()).await.unwrap();
        assert!(
            out.text.contains("in_sub.rs"),
            "in_sub.rs should match: {}",
            out.text
        );
        assert!(
            out.text.contains("nested.rs"),
            "sub/deep/nested.rs should match (** spans multiple segments): {}",
            out.text
        );
        assert!(
            !out.text.contains("not_sub.rs"),
            "not_sub.rs must be excluded: {}",
            out.text
        );
    }

    /// `.gitignore` exclusions are honored when a `.git` directory is present.
    ///
    /// `ignore::WalkBuilder` only activates gitignore processing when it detects
    /// a `.git` directory. The test creates one explicitly to trigger the check.
    #[tokio::test]
    async fn glob_respects_gitignore() {
        let dir = TempDir::new().unwrap();
        // A real .git dir is needed; WalkBuilder requires_git defaults to true.
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("kept.rs"), "").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "").unwrap();

        let args = serde_json::json!({
            "pattern": "*.rs",
            "path": dir.path().to_str().unwrap()
        });
        let out = GlobTool::new(true).execute(args, &ctx()).await.unwrap();
        assert!(
            out.text.contains("kept.rs"),
            "kept.rs should appear: {}",
            out.text
        );
        assert!(
            !out.text.contains("ignored.rs"),
            "ignored.rs must be excluded by .gitignore: {}",
            out.text
        );
    }

    /// `GlobTool` never lists files inside the `.git` directory.
    ///
    /// `filter_entry` excludes `.git` unconditionally. A real file is placed
    /// inside so the test would fail if the filter were removed (an empty `.git`
    /// would pass vacuously).
    #[tokio::test]
    async fn glob_git_dir_not_leaked() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("config"), "hello").unwrap();
        std::fs::write(dir.path().join("kept.rs"), "").unwrap();

        let args = serde_json::json!({
            "pattern": "*",
            "path": dir.path().to_str().unwrap()
        });
        let out = GlobTool::new(true).execute(args, &ctx()).await.unwrap();
        assert!(
            out.text.contains("kept.rs"),
            "kept.rs should appear: {}",
            out.text
        );
        assert!(
            !out.text.contains(".git"),
            ".git paths must never leak: {}",
            out.text
        );
    }

    /// With `respect_gitignore = false`, glob ignores `.gitignore` but not `.git`.
    ///
    /// Drives `GlobTool::new(false)` so the field-to-walker wiring is exercised.
    /// `ignored.rs` must appear (gitignore not honored), and `.git` internals
    /// stay excluded — the `filter_entry` guard is independent of `respect`.
    #[tokio::test]
    async fn glob_respect_false_ignores_gitignore_but_skips_git() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git").join("config"), "x").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("kept.rs"), "").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "").unwrap();

        let args = serde_json::json!({
            "pattern": "*.rs",
            "path": dir.path().to_str().unwrap()
        });
        let out = GlobTool::new(false).execute(args, &ctx()).await.unwrap();
        assert!(
            out.text.contains("kept.rs"),
            "kept.rs should appear: {}",
            out.text
        );
        assert!(
            out.text.contains("ignored.rs"),
            "ignored.rs must appear when gitignore is not respected: {}",
            out.text
        );
        assert!(
            !out.text.contains(".git"),
            ".git internals must never leak regardless of respect flag: {}",
            out.text
        );
    }

    /// Truncation through the public `execute()` path appends the notice.
    ///
    /// `GlobTool` accepts `max_results`, so the cap is driven through the real
    /// `execute()` entry point (mirroring `grep_max_results_truncates`). This
    /// exercises the actual output-assembly code — not a copy of it — so that
    /// removing the truncation logic in `execute()` would fail this test.
    #[tokio::test]
    async fn glob_truncation_at_cap() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::write(dir.path().join("c.rs"), "").unwrap();

        let args = serde_json::json!({
            "pattern": "*.rs",
            "path": dir.path().to_str().unwrap(),
            "max_results": 2
        });
        let out = GlobTool::new(true).execute(args, &ctx()).await.unwrap();
        assert!(
            out.text.contains("(truncated: result limit reached)"),
            "execute() output must contain truncation notice: {}",
            out.text
        );
        // Exactly 2 path lines + 1 truncation notice line.
        assert_eq!(
            out.text.lines().count(),
            3,
            "expected 2 paths + 1 notice line: {}",
            out.text
        );
    }

    /// Glob output paths are absolute.
    #[tokio::test]
    async fn glob_output_paths_are_absolute() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("file.rs"), "").unwrap();
        let args = serde_json::json!({
            "pattern": "*.rs",
            "path": dir.path().to_str().unwrap()
        });
        let out = GlobTool::new(true).execute(args, &ctx()).await.unwrap();
        for line in out.text.lines() {
            // Skip the truncation sentinel (only present when the cap is hit);
            // it is a notice, not a path.
            if line.is_empty() || line.starts_with('(') {
                continue;
            }
            assert!(
                std::path::Path::new(line).is_absolute(),
                "expected absolute path, got: {line}"
            );
        }
    }
}

// Rust guideline compliant 2026-02-21

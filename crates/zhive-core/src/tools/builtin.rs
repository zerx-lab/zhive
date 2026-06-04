//! Built-in coding tool suite for the zhive agent engine.
//!
//! This module provides the standard set of filesystem, search, and shell tools
//! that power the agent's coding workflow. All tools are self-contained within
//! this module tree and do not touch the dispatch layer or CLI.
//!
//! ## Tools provided
//!
//! | Name | Kind  | Description                              |
//! |------|-------|------------------------------------------|
//! | `read`  | Read    | Read a file with optional offset/limit |
//! | `write` | Edit    | Atomically write a file                |
//! | `edit`  | Edit    | Replace a substring inside a file      |
//! | `grep`  | Read    | Regex search across a directory tree   |
//! | `glob`  | Read    | Expand a glob pattern to file paths    |
//! | `bash`  | Execute | Run a shell command with a timeout     |
//! | `agent` | Other   | Delegate a sub-task to a child agent   |
//!
//! ## Sandbox seam
//!
//! [`BashTool`] accepts any [`Sandbox`] implementation via
//! [`BashTool::with_sandbox`]. The bundled [`DefaultSandbox`] is a no-op that
//! runs commands with the host's full privileges. OS-native isolation backends
//! (Landlock on Linux, Seatbelt on macOS, Job Objects on Windows) are planned
//! for a future phase; today [`DefaultSandbox`] is the only shipped variant.
//!
//! ## Output limits
//!
//! All tools clamp their text output to [`MAX_TOOL_OUTPUT_BYTES`]. Tools that
//! read files additionally cap per-line length at [`MAX_LINE_BYTES`] and
//! default to reading at most [`DEFAULT_READ_LINE_LIMIT`] lines.

use std::path::PathBuf;
use std::sync::Arc;

use crate::tools::ToolRegistry;

pub mod agent;
pub mod bash;
pub mod read;
pub mod search;
pub mod write;

#[doc(inline)]
pub use agent::AgentTool;
#[doc(inline)]
pub use bash::BashTool;
#[doc(inline)]
pub use read::ReadFileTool;
#[doc(inline)]
pub use search::{GlobTool, GrepTool};
#[doc(inline)]
pub use write::{EditFileTool, WriteFileTool};

// ============================================================
// Shared output-limit constants (M-DOCUMENTED-MAGIC)
// ============================================================

/// Maximum bytes returned in a single tool output.
///
/// Chosen to keep model context window pressure low while still returning
/// enough content for a multi-hundred-line file. Callers that need more
/// content can use `offset` / `limit` on [`ReadFileTool`].
pub const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024; // 64 KiB

/// Maximum bytes kept from a single source line.
///
/// Very long minified or machine-generated lines are truncated to this length
/// before they can inflate the output. The value matches the default read-line
/// limit in the Claude Code host for cross-tool consistency.
pub const MAX_LINE_BYTES: usize = 2_000;

/// Default maximum number of lines returned by [`ReadFileTool`].
///
/// Matches the Claude Code host default so that agents that share prompts
/// across hosts see consistent truncation behaviour.
pub const DEFAULT_READ_LINE_LIMIT: usize = 2_000;

/// Default maximum results returned by [`GrepTool`].
///
/// Large codebases can contain thousands of matches; this cap prevents a
/// single grep call from exhausting the model's context window. Callers can
/// override via the `max_results` argument.
pub const DEFAULT_GREP_MAX_RESULTS: usize = 200;

/// Default maximum paths returned by [`GlobTool`].
///
/// A glob over an unfiltered tree can match millions of files. The cap keeps
/// output bounded; increase it only when the caller knows the tree is small.
pub const DEFAULT_GLOB_MAX_RESULTS: usize = 1_000;

/// Default shell command timeout.
///
/// 120 seconds is long enough for typical compilation steps that an agent
/// issues interactively while still ensuring the turn loop eventually makes
/// progress if a command hangs. Callers can shorten it via `timeout_ms`.
pub const DEFAULT_BASH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Hard ceiling on the caller-supplied shell command timeout.
///
/// Prevents a misbehaving or adversarial prompt from locking a thread for an
/// arbitrarily long time. Ten minutes exceeds any reasonable interactive shell
/// operation.
pub const MAX_BASH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

// ============================================================
// Sandbox seam
// ============================================================

/// Execution-isolation hook for command-running tools.
///
/// Implement this trait to wrap [`BashTool`] commands in an OS sandbox such
/// as Linux Landlock, macOS Seatbelt, or Windows Job Objects. The current
/// production default is [`DefaultSandbox`], which applies no restrictions.
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::{DefaultSandbox, Sandbox};
/// let s = DefaultSandbox;
/// // DefaultSandbox is a no-op; just verify it implements the trait.
/// let _: &dyn Sandbox = &s;
/// ```
pub trait Sandbox: Send + Sync + std::fmt::Debug {
    /// Applies isolation settings to `command` before it is spawned.
    fn prepare(&self, command: &mut tokio::process::Command);
}

/// No-op sandbox: runs commands with the host's full privileges.
///
/// This is the default used by [`BashTool`] when no sandbox is configured.
/// OS-native isolation backends (Landlock on Linux, Seatbelt on macOS) are
/// planned for a future phase.
///
/// [`BashTool`] applies process-group isolation and environment tightening
/// independently of the sandbox seam (see [`apply_minimal_env`] and the
/// security-boundary note in `bash.rs`). The sandbox seam is orthogonal: a
/// future `LandlockSandbox` would add filesystem confinement on top.
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::DefaultSandbox;
/// let s = DefaultSandbox;
/// assert_eq!(format!("{s:?}"), "DefaultSandbox");
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultSandbox;

impl Sandbox for DefaultSandbox {
    fn prepare(&self, _command: &mut tokio::process::Command) {}
}

// ============================================================
// Minimal environment helper (env tightening for BashTool)
// ============================================================

/// Applies the minimal environment whitelist to `cmd`.
///
/// Clears all inherited variables with [`tokio::process::Command::env_clear`],
/// then injects only:
///
/// | Variable | Source |
/// |----------|--------|
/// | `PATH`   | Parent `PATH`, or `/usr/local/bin:/usr/bin:/bin` if absent |
/// | `HOME`   | Parent `HOME`, if set |
/// | `TERM`   | Hard-coded `"dumb"` to avoid interactive-mode probing |
///
/// This prevents secrets or tool-specific variables present in the agent
/// process's environment from leaking into executed shell commands.
///
/// # Examples
///
/// ```
/// use tokio::process::Command;
/// use zhive_core::tools::builtin::apply_minimal_env;
/// let mut cmd = Command::new("sh");
/// apply_minimal_env(&mut cmd);
/// // cmd will now run with env_clear + PATH/HOME/TERM whitelist only.
/// ```
pub fn apply_minimal_env(cmd: &mut tokio::process::Command) {
    cmd.env_clear();

    let path = std::env::var_os("PATH")
        .unwrap_or_else(|| std::ffi::OsString::from("/usr/local/bin:/usr/bin:/bin"));
    cmd.env("PATH", path);

    if let Some(home) = std::env::var_os("HOME") {
        cmd.env("HOME", home);
    }

    // `TERM=dumb` prevents interactive programs (e.g. less, man) from
    // switching to full-screen TUI mode when they detect a terminal.
    cmd.env("TERM", "dumb");
}

// ============================================================
// BuiltinToolsConfig
// ============================================================

/// Configuration for which built-in tools to register.
///
/// Pass this to [`register_builtins`] after building your [`ToolRegistry`].
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::BuiltinToolsConfig;
/// let cfg = BuiltinToolsConfig::default();
/// assert!(cfg.enable_bash);
/// ```
#[derive(Debug, Clone)]
pub struct BuiltinToolsConfig {
    /// When `true`, the `bash` tool is registered. Disable for sandboxed
    /// environments that do not permit arbitrary shell execution.
    pub enable_bash: bool,
    /// Sandbox applied to every [`BashTool`] invocation.
    pub sandbox: Arc<dyn Sandbox>,
}

impl Default for BuiltinToolsConfig {
    fn default() -> Self {
        Self {
            enable_bash: true,
            sandbox: Arc::new(DefaultSandbox),
        }
    }
}

// ============================================================
// register_builtins
// ============================================================

/// Registers the built-in tools into `registry` according to `config`.
///
/// Read, write, edit, grep, glob, and agent are always registered. The
/// `bash` tool is registered only when [`BuiltinToolsConfig::enable_bash`] is
/// `true`. The `agent` tool is registered unconditionally: when no subagent
/// spawner is wired into the [`crate::tools::ToolContext`] (e.g. outside a real
/// engine turn) it returns a graceful error rather than spawning.
///
/// # Examples
///
/// ```
/// use zhive_core::tools::{ToolRegistry};
/// use zhive_core::tools::builtin::{BuiltinToolsConfig, register_builtins};
///
/// let mut reg = ToolRegistry::new();
/// register_builtins(&mut reg, &BuiltinToolsConfig::default());
/// assert!(reg.get("read").is_some());
/// assert!(reg.get("write").is_some());
/// assert!(reg.get("edit").is_some());
/// assert!(reg.get("grep").is_some());
/// assert!(reg.get("glob").is_some());
/// assert!(reg.get("bash").is_some());
/// assert!(reg.get("agent").is_some());
/// ```
pub fn register_builtins(registry: &mut ToolRegistry, config: &BuiltinToolsConfig) {
    registry.register(Arc::new(ReadFileTool));
    registry.register(Arc::new(WriteFileTool));
    registry.register(Arc::new(EditFileTool));
    registry.register(Arc::new(GrepTool));
    registry.register(Arc::new(GlobTool));
    registry.register(Arc::new(AgentTool));
    if config.enable_bash {
        registry.register(Arc::new(BashTool::with_sandbox(Arc::clone(
            &config.sandbox,
        ))));
    }
}

// ============================================================
// Shared helpers
// ============================================================

/// Resolves `p` to an absolute path, using `current_dir` for relative inputs.
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::resolve_path;
/// let abs = resolve_path("/tmp/foo.txt");
/// assert!(abs.is_absolute());
/// ```
#[must_use]
pub fn resolve_path(p: impl AsRef<str>) -> PathBuf {
    let s = p.as_ref();
    let path = PathBuf::from(s);
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    }
}

/// Truncates `s` to at most `max_bytes`, snapping to a valid UTF-8 boundary.
///
/// Returns `(truncated_string, was_truncated)`.
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::truncate_utf8;
/// let (s, cut) = truncate_utf8("hello world", 5);
/// assert_eq!(s, "hello");
/// assert!(cut);
/// let (s2, cut2) = truncate_utf8("hi", 100);
/// assert_eq!(s2, "hi");
/// assert!(!cut2);
/// ```
#[must_use]
pub fn truncate_utf8(s: &str, max_bytes: usize) -> (String, bool) {
    if s.len() <= max_bytes {
        return (s.to_owned(), false);
    }
    // Walk backward from max_bytes to find a valid char boundary.
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_owned(), true)
}

/// Clamps `text` to [`MAX_TOOL_OUTPUT_BYTES`], appending a truncation notice.
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::{clamp_output, MAX_TOOL_OUTPUT_BYTES};
/// let big = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 1);
/// let out = clamp_output(big);
/// assert!(out.len() <= MAX_TOOL_OUTPUT_BYTES + 60);
/// assert!(out.contains("(output truncated"));
/// ```
#[must_use]
pub fn clamp_output(text: String) -> String {
    if text.len() <= MAX_TOOL_OUTPUT_BYTES {
        return text;
    }
    let mut end = MAX_TOOL_OUTPUT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n(output truncated: {} bytes omitted)",
        &text[..end],
        text.len() - end
    )
}

// Rust guideline compliant 2026-02-21

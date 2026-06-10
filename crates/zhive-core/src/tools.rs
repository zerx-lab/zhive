//! Tool abstraction, registry, and built-in tools.
//!
//! This module defines the extension point that lets callers inject callable
//! tools into the engine's inner tool-dispatch loop.  Each tool is an
//! `async fn(args, ctx)` behind the [`Tool`] trait; the [`ToolRegistry`]
//! maps names to `Arc<dyn Tool>`.
//!
//! ## Built-in tools
//!
//! [`EchoTool`] is always available as a zero-cost regression target: it
//! returns its JSON args as its output text, making it ideal for unit tests
//! that need an observable, side-effect-free round-trip.
//!
//! ## Phase-1 limitations
//!
//! Tool schemas are not registered here; schema registration happens at
//! manifest load time through
//! [`crate::hooks::validator::SchemaCache::register`].
//!
//! Red line 11: when a `PreToolUse` hook returns `updated_input`, the host
//! re-validates it against the tool input schema; if the tool has no
//! registered schema, revalidation fails (`UnknownTool`) and the call is
//! blocked, because mutating input for a schema-less tool is the exact
//! failure mode red line 11 guards against. When no `updated_input` is
//! returned, no revalidation runs and the call proceeds.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use zhive_proto::domain::{ThreadId, TurnId};

// ============================================================
// ToolKind
// ============================================================

/// Coarse classification of a registered tool.
///
/// Mirrors [`zhive_proto::domain::ToolKind`] but lives here so the
/// [`Tool`] trait does not depend on the proto crate at the trait-object
/// boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ToolKind {
    /// Read-only file or resource access.
    Read,
    /// Modify a file or resource in place.
    Edit,
    /// Execute a shell command or script.
    Execute,
    /// Anything else.
    #[default]
    Other,
}

impl From<ToolKind> for zhive_proto::domain::ToolKind {
    fn from(k: ToolKind) -> Self {
        match k {
            ToolKind::Read => zhive_proto::domain::ToolKind::Read,
            ToolKind::Edit => zhive_proto::domain::ToolKind::Edit,
            ToolKind::Execute => zhive_proto::domain::ToolKind::Execute,
            ToolKind::Other => zhive_proto::domain::ToolKind::Other,
        }
    }
}

// ============================================================
// ToolOutput / ToolError
// ============================================================

/// Whole-file before/after text for a diff produced by a tool call.
///
/// A tool that edits a file (e.g. `edit`, `write`) attaches one `FileDiff` per
/// touched file so clients can render a diff view. The engine promotes each
/// `FileDiff` into an [`zhive_proto::domain::ItemToolCallContent::Diff`] block
/// on the tool call's content, where the TUI and ACP bridge already know how to
/// surface it. The provider-facing tool result is left untouched — only the
/// human-readable [`ToolOutput::text`] reaches the model.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
/// use zhive_core::tools::FileDiff;
/// let diff = FileDiff {
///     path: PathBuf::from("src/lib.rs"),
///     old_text: Some("a\n".to_owned()),
///     new_text: "b\n".to_owned(),
/// };
/// assert_eq!(diff.old_text.as_deref(), Some("a\n"));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct FileDiff {
    /// Affected file path.
    pub path: PathBuf,
    /// Pre-edit text; `None` for a newly created file.
    pub old_text: Option<String>,
    /// Post-edit text.
    pub new_text: String,
}

/// Successful output of a tool execution.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    /// Human-readable text to feed back to the model.
    pub text: String,
    /// Optional structured JSON value for richer clients.
    pub value: Option<serde_json::Value>,
    /// File diffs to surface as diff content blocks on the tool call.
    ///
    /// Empty for tools that touch no files. The engine maps each entry to an
    /// [`zhive_proto::domain::ItemToolCallContent::Diff`] block; it never
    /// affects the provider-facing result text.
    pub diffs: Vec<FileDiff>,
}

impl ToolOutput {
    /// Builds a text-only output.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::tools::ToolOutput;
    /// let out = ToolOutput::text("hello");
    /// assert_eq!(out.text, "hello");
    /// assert!(out.value.is_none());
    /// assert!(out.diffs.is_empty());
    /// ```
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            value: None,
            diffs: Vec::new(),
        }
    }

    /// Builds an output with both text and a structured value.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::tools::ToolOutput;
    /// let out = ToolOutput::with_value("ok", serde_json::json!({"status": 0}));
    /// assert_eq!(out.text, "ok");
    /// assert!(out.value.is_some());
    /// ```
    #[must_use]
    pub fn with_value(text: impl Into<String>, value: serde_json::Value) -> Self {
        Self {
            text: text.into(),
            value: Some(value),
            diffs: Vec::new(),
        }
    }

    /// Attaches file diffs, returning the updated output.
    ///
    /// Each diff becomes a diff content block on the resulting tool call. The
    /// model still sees only [`ToolOutput::text`], so attaching a diff never
    /// enlarges the provider prompt.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::PathBuf;
    /// use zhive_core::tools::{FileDiff, ToolOutput};
    /// let out = ToolOutput::text("replaced 1 occurrence(s)").with_diffs(vec![FileDiff {
    ///     path: PathBuf::from("src/lib.rs"),
    ///     old_text: Some("a\n".to_owned()),
    ///     new_text: "b\n".to_owned(),
    /// }]);
    /// assert_eq!(out.diffs.len(), 1);
    /// ```
    #[must_use]
    pub fn with_diffs(mut self, diffs: Vec<FileDiff>) -> Self {
        self.diffs = diffs;
        self
    }
}

/// Error returned by a tool's `execute` method.
///
/// # Errors
///
/// [`ToolError::Execution`] wraps a free-form error message from the tool.
/// [`ToolError::Cancelled`] is injected by the dispatch loop when the turn
/// cancel token fires during a tool's execution.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ToolError {
    /// Tool body returned an error.
    #[error("tool execution failed: {0}")]
    Execution(String),

    /// The turn was cancelled while the tool was running.
    #[error("tool execution cancelled")]
    Cancelled,
}

// ============================================================
// SubagentSpawner
// ============================================================

/// Lets a model-invocable tool delegate a sub-task to a child agent.
///
/// The engine wires a concrete implementation into [`ToolContext::spawner`]
/// for every real turn; test and non-engine contexts leave it `None`. A tool
/// (e.g. the built-in `agent` tool) calls [`SubagentSpawner::spawn_and_await`]
/// to spawn a child agent and block until the child produces its final
/// message.
///
/// This is a `dyn`-friendly trait (object-safe, `Send + Sync + Debug`) so it
/// can live behind an `Arc<dyn SubagentSpawner>` inside the `Clone` +
/// `Debug` [`ToolContext`].
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use async_trait::async_trait;
/// use zhive_core::tools::SubagentSpawner;
///
/// #[derive(Debug)]
/// struct FixedSpawner;
///
/// #[async_trait]
/// impl SubagentSpawner for FixedSpawner {
///     async fn spawn_and_await(
///         &self,
///         _name: String,
///         _description: String,
///         _prompt: String,
///     ) -> Result<String, String> {
///         Ok("child result".to_owned())
///     }
/// }
///
/// // The trait is object-safe: it can be erased behind `Arc<dyn ...>`.
/// let _erased: Arc<dyn SubagentSpawner> = Arc::new(FixedSpawner);
/// ```
#[async_trait]
pub trait SubagentSpawner: Send + Sync + std::fmt::Debug {
    /// Spawns a subagent and awaits its final message.
    ///
    /// The child inherits the parent's tool allowlist and permission mode; only
    /// `name`, `description`, and `prompt` are supplied per call. The returned
    /// `String` is the child's final message text (empty when the child
    /// produced no textual output).
    ///
    /// # Errors
    ///
    /// Returns the failure reason as a `String`: recursion is forbidden, the
    /// spawn was rejected (scope widening, missing parent), or the child turn
    /// itself errored.
    async fn spawn_and_await(
        &self,
        name: String,
        description: String,
        prompt: String,
    ) -> Result<String, String>;
}

// ============================================================
// ToolContext
// ============================================================

/// Read-only execution context passed to every tool invocation.
///
/// Carries identifiers and an abort handle so a tool can observe the
/// turn's cancellation and return early rather than blocking the loop.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Thread the tool is running inside.
    pub thread_id: ThreadId,
    /// Turn the tool belongs to.
    pub turn_id: TurnId,
    /// Fires when the turn is cancelled; a well-behaved tool should check
    /// this before performing expensive or irreversible operations.
    pub cancel: CancellationToken,
    /// Optional hook for spawning child agents from inside a tool.
    ///
    /// `Some` only during real engine turns (wired by the dispatch loop);
    /// `None` in tests and other non-engine contexts, in which case a tool
    /// that needs it must return an error rather than spawning.
    pub spawner: Option<Arc<dyn SubagentSpawner>>,
    /// Canonical session workspace root that relative tool paths resolve
    /// against.
    ///
    /// `Some` during real engine turns (the engine pins one canonical root so
    /// the shadow snapshot repo and tool writes agree on the same directory,
    /// regardless of the live process `current_dir()`). `None` in tests and
    /// non-engine contexts, where [`ToolContext::resolve`] falls back to the
    /// process working directory.
    pub workspace_root: Option<std::path::PathBuf>,
}

impl ToolContext {
    /// Resolves `path` to an absolute path against the session workspace root.
    ///
    /// An absolute input is returned unchanged. A relative input is joined onto
    /// [`ToolContext::workspace_root`] when set, falling back to the process
    /// working directory (via [`crate::tools::builtin::resolve_path`]) when not.
    ///
    /// Anchoring on the session root — rather than the live process cwd — is
    /// what keeps file writes and the shadow snapshot repository in agreement,
    /// so a revert reverts the files that were actually written.
    #[must_use]
    pub fn resolve(&self, path: impl AsRef<str>) -> std::path::PathBuf {
        let p = std::path::PathBuf::from(path.as_ref());
        if p.is_absolute() {
            return p;
        }
        match &self.workspace_root {
            Some(root) => root.join(p),
            None => std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from("/"))
                .join(p),
        }
    }
}

// ============================================================
// Tool trait
// ============================================================

/// Callable tool that can be registered with [`ToolRegistry`].
///
/// Implementors receive JSON `args` and a [`ToolContext`]; they are
/// responsible for validating their own argument types (or trusting that
/// the engine's schema re-validation step has already done so).
///
/// # Examples
///
/// See [`EchoTool`] for the canonical no-dependency implementation.
///
/// # Errors
///
/// Returning [`ToolError::Execution`] records a failed tool call and feeds
/// the error back to the model as a denial result.  Returning
/// [`ToolError::Cancelled`] is treated identically but triggers no
/// `PostToolUseFailure` hook (it is an engine-level abort, not a tool
/// fault).
#[async_trait]
pub trait Tool: Send + Sync {
    /// Stable, unique name used to look up this tool in the registry.
    ///
    /// Returns a borrow tied to `&self` (not `&'static`) so dynamically
    /// sourced tools — MCP-server tools, skills — can return a runtime
    /// `String` field they own rather than leaking a `&'static str`.
    fn name(&self) -> &str;

    /// Coarse classification used for UI grouping.
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    /// Natural-language description advertised to the model, if any.
    ///
    /// Returned to the provider as the tool's `description` so the model can
    /// decide when to call it. The default `None` advertises no description.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::tools::{EchoTool, Tool};
    /// assert!(EchoTool.description().is_some());
    /// ```
    fn description(&self) -> Option<String> {
        None
    }

    /// Human-readable, one-line title summarizing a call with these `args`.
    ///
    /// Surfaced as the ACP `ToolCall.title` so clients show a meaningful
    /// headline (e.g. `$ cargo check` for a shell call) instead of the bare
    /// tool name. The default `None` lets consumers fall back to [`Tool::name`].
    ///
    /// Implementors should keep the result short and single-line; long values
    /// are the implementor's responsibility to truncate.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::tools::{EchoTool, Tool};
    /// assert!(EchoTool.title(&serde_json::json!({})).is_none());
    /// ```
    fn title(&self, args: &serde_json::Value) -> Option<String> {
        let _ = args;
        None
    }

    /// JSON Schema (an object schema) describing this tool's input arguments.
    ///
    /// Advertised to the model so it emits well-formed arguments, and used as
    /// the red-line-11 revalidation fallback for tools that did not register a
    /// schema explicitly. The default is the permissive empty object schema
    /// `{"type": "object"}`.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::tools::{EchoTool, Tool};
    /// let schema = EchoTool.input_schema();
    /// assert_eq!(schema["type"], "object");
    /// ```
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }

    /// Whether this tool is available inside subagent threads.
    ///
    /// Returns `false` to prevent the tool from appearing in the child scope
    /// when a subagent is spawned. The default is `true` (all tools are
    /// available in subagents unless an implementor opts out).
    ///
    /// Skill tools honour the `disable_in_subagent` manifest field by
    /// overriding this method; see [`crate::skills::tool::SkillTool`].
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::tools::{EchoTool, Tool};
    /// // Built-in tools are available in subagents by default.
    /// assert!(EchoTool.available_in_subagent());
    /// ```
    fn available_in_subagent(&self) -> bool {
        true
    }

    /// Executes the tool with JSON `args` and a read-only [`ToolContext`].
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] on failure.
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError>;
}

// ============================================================
// ToolSpec
// ============================================================

/// Advertisable description of one registered tool.
///
/// Produced by [`ToolRegistry::specs`] and consumed by the engine's prompt
/// builder to advertise tools to the LLM provider. Carries only the
/// model-facing surface (name, description, input schema); the executable
/// behaviour stays behind the [`Tool`] trait object.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_core::tools::{EchoTool, ToolRegistry};
///
/// let mut reg = ToolRegistry::new();
/// reg.register(Arc::new(EchoTool));
/// let specs = reg.specs();
/// assert_eq!(specs[0].name, "echo");
/// assert!(specs[0].description.is_some());
/// assert_eq!(specs[0].input_schema["type"], "object");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    /// Stable, unique tool name (matches [`Tool::name`]).
    pub name: String,
    /// Optional natural-language description advertised to the model.
    pub description: Option<String>,
    /// JSON Schema (object) for the tool's input arguments.
    pub input_schema: serde_json::Value,
}

// ============================================================
// ToolRegistry
// ============================================================

/// Thread-safe registry mapping tool names to their implementations.
///
/// The registry is read-only after construction; tools are registered before
/// the engine starts and not modified while turns are in flight.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_core::tools::{ToolRegistry, EchoTool};
///
/// let mut reg = ToolRegistry::new();
/// reg.register(Arc::new(EchoTool));
/// assert!(reg.get("echo").is_some());
/// assert!(reg.get("unknown").is_none());
/// assert!(!reg.is_empty());
/// ```
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tool_count", &self.tools.len())
            .field("tool_names", &self.tools.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ToolRegistry {
    /// Builds an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `tool`, using [`Tool::name`] as the map key.
    ///
    /// A subsequent registration with the same name overwrites the previous
    /// entry silently.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_owned(), tool);
    }

    /// Returns the tool registered under `name`, or `None` when absent.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Returns `true` when the registry contains no tools.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Iterates over `(name, tool)` pairs for all registered tools.
    ///
    /// Iteration order is unspecified (`HashMap` order). Callers that need
    /// deterministic ordering should sort the results themselves.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_core::tools::{EchoTool, ToolRegistry};
    ///
    /// let mut reg = ToolRegistry::new();
    /// reg.register(Arc::new(EchoTool));
    /// let names: Vec<&str> = reg.iter().map(|(n, _)| n.as_str()).collect();
    /// assert_eq!(names, ["echo"]);
    /// ```
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Arc<dyn Tool>)> {
        self.tools.iter()
    }

    /// Enumerates the model-facing [`ToolSpec`] of every registered tool.
    ///
    /// The result is sorted by tool name so the generated prompt is
    /// deterministic across runs (a `HashMap` would otherwise iterate in an
    /// unspecified order, making cached prompts and snapshots flaky).
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_core::tools::{EchoTool, ToolRegistry};
    ///
    /// let mut reg = ToolRegistry::new();
    /// reg.register(Arc::new(EchoTool));
    /// let specs = reg.specs();
    /// assert_eq!(specs.len(), 1);
    /// assert_eq!(specs[0].name, "echo");
    /// ```
    #[must_use]
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .tools
            .values()
            .map(|tool| ToolSpec {
                name: tool.name().to_owned(),
                description: tool.description(),
                input_schema: tool.input_schema(),
            })
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }
}

// ============================================================
// EchoTool
// ============================================================

/// Built-in tool that returns its JSON arguments as its output text.
///
/// Useful for unit tests and examples that need an observable,
/// side-effect-free tool round-trip.
///
/// # Examples
///
/// ```
/// # let rt = tokio::runtime::Runtime::new().unwrap();
/// # rt.block_on(async {
/// use std::sync::Arc;
/// use tokio_util::sync::CancellationToken;
/// use zhive_core::tools::{EchoTool, Tool, ToolContext};
/// use zhive_proto::domain::{ThreadId, TurnId};
///
/// let ctx = ToolContext {
///     thread_id: ThreadId(Arc::from("thread:native/test")),
///     turn_id:   TurnId(Arc::from("turn:thread:native/test/0")),
///     cancel:    CancellationToken::new(),
///     spawner:   None,
///     workspace_root: None,
/// };
/// let out = EchoTool.execute(serde_json::json!({"msg": "hello"}), &ctx).await.unwrap();
/// assert!(out.text.contains("hello"));
/// # });
/// ```
#[derive(Debug, Clone, Copy)]
pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn description(&self) -> Option<String> {
        Some("Echoes its JSON arguments back as text; used for testing.".to_owned())
    }

    fn input_schema(&self) -> serde_json::Value {
        // `additionalProperties: true` keeps the tool free-form (any JSON
        // object round-trips) while still advertising the conventional `msg`
        // field so the model has a hint about the expected shape.
        serde_json::json!({
            "type": "object",
            "properties": {
                "msg": {
                    "type": "string",
                    "description": "Free-form text to echo back."
                }
            },
            "additionalProperties": true
        })
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let text = args.to_string();
        Ok(ToolOutput::with_value(text.clone(), args))
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        ToolContext {
            thread_id: ThreadId(Arc::from("thread:native/t")),
            turn_id: TurnId(Arc::from("turn:thread:native/t/0")),
            cancel: CancellationToken::new(),
            spawner: None,
            workspace_root: None,
        }
    }

    #[tokio::test]
    async fn echo_tool_returns_args_as_text() {
        let out = EchoTool
            .execute(serde_json::json!({"key": "val"}), &ctx())
            .await
            .unwrap();
        assert!(out.text.contains("val"));
        assert!(out.value.is_some());
    }

    #[test]
    fn tool_registry_register_and_get() {
        let mut reg = ToolRegistry::new();
        assert!(reg.is_empty());
        reg.register(Arc::new(EchoTool));
        assert!(!reg.is_empty());
        assert!(reg.get("echo").is_some());
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn tool_output_text_helper() {
        let out = ToolOutput::text("hello");
        assert_eq!(out.text, "hello");
        assert!(out.value.is_none());
    }

    #[test]
    fn tool_output_with_value_helper() {
        let v = serde_json::json!(42);
        let out = ToolOutput::with_value("done", v.clone());
        assert_eq!(out.value, Some(v));
    }

    #[test]
    fn tool_kind_converts_to_proto() {
        let k: zhive_proto::domain::ToolKind = ToolKind::Read.into();
        assert_eq!(k, zhive_proto::domain::ToolKind::Read);
    }

    #[test]
    fn echo_tool_advertises_description_and_object_schema() {
        assert!(EchoTool.description().is_some());
        let schema = EchoTool.input_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["msg"].is_object());
    }

    #[test]
    fn echo_tool_available_in_subagent_default_is_true() {
        // The default implementation must return `true` so existing tools are
        // not inadvertently excluded when spawning child agents.
        assert!(EchoTool.available_in_subagent());
    }

    #[test]
    fn registry_specs_are_sorted_by_name() {
        struct AlphaTool;
        #[async_trait]
        impl Tool for AlphaTool {
            fn name(&self) -> &'static str {
                "alpha"
            }
            async fn execute(
                &self,
                _args: serde_json::Value,
                _ctx: &ToolContext,
            ) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput::text("a"))
            }
        }

        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        reg.register(Arc::new(AlphaTool));
        let specs = reg.specs();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "alpha");
        assert_eq!(specs[1].name, "echo");
        // The default schema fallback applies to AlphaTool.
        assert_eq!(
            specs[0].input_schema,
            serde_json::json!({ "type": "object" })
        );
    }
}

/// Built-in coding tools (read / write / edit / grep / glob / bash).
///
/// Gated behind the `tools` feature so consumers that inject their own tools
/// do not pull in `ignore` / `regex` / `glob`.
#[cfg(feature = "tools")]
pub mod builtin;

// Rust guideline compliant 2026-02-21

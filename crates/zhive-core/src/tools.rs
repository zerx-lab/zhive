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

/// Successful output of a tool execution.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    /// Human-readable text to feed back to the model.
    pub text: String,
    /// Optional structured JSON value for richer clients.
    pub value: Option<serde_json::Value>,
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
    /// ```
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            value: None,
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
        }
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
    fn name(&self) -> &'static str;

    /// Coarse classification used for UI grouping.
    fn kind(&self) -> ToolKind {
        ToolKind::Other
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
}

// Rust guideline compliant 2026-02-21

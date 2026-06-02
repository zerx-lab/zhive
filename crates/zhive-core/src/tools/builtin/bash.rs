//! [`BashTool`]: run a shell command with timeout and cancellation support.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use crate::tools::builtin::{
    DEFAULT_BASH_TIMEOUT, DefaultSandbox, MAX_BASH_TIMEOUT, MAX_TOOL_OUTPUT_BYTES, Sandbox,
    clamp_output,
};
use crate::tools::{Tool, ToolContext, ToolError, ToolKind, ToolOutput};

// ============================================================
// BashTool
// ============================================================

/// Runs a shell command and returns combined stdout + stderr.
///
/// The command is launched via `sh -c <command>` with stdin connected to
/// `/dev/null` so a command that reads stdin sees EOF instead of hanging until
/// the timeout. `kill_on_drop(true)` is set so dropping the child handle (on
/// timeout or cancellation) terminates the spawned child process. Compound
/// commands may leave grandchildren behind; full process-group cleanup is a
/// future improvement tracked with the sandbox work.
///
/// Arguments:
///
/// ```json
/// {
///   "command":    "cargo check",
///   "timeout_ms": 30000,
///   "cwd":        "/opt/project"
/// }
/// ```
///
/// `timeout_ms` defaults to [`DEFAULT_BASH_TIMEOUT`] (120 s) and is clamped
/// to [`MAX_BASH_TIMEOUT`] (600 s). The [`ToolContext::cancel`] token is
/// respected: if it fires the child is killed and [`ToolError::Cancelled`] is
/// returned.
///
/// The `value` field of the output contains a structured JSON object:
/// `{ "exit_code": int, "stdout": str, "stderr": str, "timed_out": bool }`.
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::BashTool;
/// use zhive_core::tools::Tool;
/// let tool = BashTool::new();
/// assert_eq!(tool.name(), "bash");
/// ```
#[derive(Debug, Clone)]
pub struct BashTool {
    sandbox: Arc<dyn Sandbox>,
}

impl BashTool {
    /// Builds a [`BashTool`] with the default no-op [`DefaultSandbox`].
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::tools::builtin::BashTool;
    /// use zhive_core::tools::Tool;
    /// let tool = BashTool::new();
    /// assert_eq!(tool.name(), "bash");
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            sandbox: Arc::new(DefaultSandbox),
        }
    }

    /// Builds a [`BashTool`] with a custom [`Sandbox`].
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_core::tools::builtin::{BashTool, DefaultSandbox};
    /// use zhive_core::tools::Tool;
    /// let tool = BashTool::with_sandbox(Arc::new(DefaultSandbox));
    /// assert_eq!(tool.name(), "bash");
    /// ```
    #[must_use]
    pub fn with_sandbox(sandbox: Arc<dyn Sandbox>) -> Self {
        Self { sandbox }
    }
}

impl Default for BashTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }

    fn description(&self) -> Option<String> {
        Some("Run a shell command and return its combined stdout/stderr output.".to_owned())
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command":    { "type": "string",  "description": "Shell command to execute via sh -c." },
                "timeout_ms": { "type": "integer", "minimum": 1,  "description": "Timeout in milliseconds." },
                "cwd":        { "type": "string",  "description": "Working directory for the command." }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    /// Executes the shell command.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Cancelled`] when the turn's cancel token fires.
    /// Returns [`ToolError::Execution`] on timeout or spawn failure.
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let command = args["command"]
            .as_str()
            .ok_or_else(|| ToolError::Execution("`command` must be a string".to_owned()))?
            .to_owned();

        let timeout = args["timeout_ms"]
            .as_u64()
            .map_or(DEFAULT_BASH_TIMEOUT, |ms| {
                Duration::from_millis(ms).min(MAX_BASH_TIMEOUT)
            });

        let cwd: Option<std::path::PathBuf> = args["cwd"].as_str().map(|s| {
            let p = std::path::PathBuf::from(s);
            if p.is_absolute() {
                p
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| std::path::PathBuf::from("/"))
                    .join(p)
            }
        });

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&command);
        cmd.kill_on_drop(true);
        // Connect stdin to /dev/null so a command that reads stdin (e.g. a bare
        // `cat`) gets EOF immediately instead of blocking until the timeout.
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        if let Some(ref dir) = cwd {
            cmd.current_dir(dir);
        }

        // Let the sandbox apply any isolation settings.
        self.sandbox.prepare(&mut cmd);

        let child = cmd
            .spawn()
            .map_err(|e| ToolError::Execution(format!("failed to spawn command: {e}")))?;

        let cancel = ctx.cancel.clone();

        // `wait_with_output` takes ownership of `Child`, so we pin it as a
        // future before entering `select!` to avoid the "moved value" issue.
        let wait_fut = child.wait_with_output();
        tokio::pin!(wait_fut);

        tokio::select! {
            // Branch 1: command completes normally.
            result = &mut wait_fut => {
                let output = result.map_err(|e| {
                    ToolError::Execution(format!("command wait failed: {e}"))
                })?;
                Ok(build_tool_output(&output, false))
            }

            // Branch 2: timeout. The pinned future is dropped automatically
            // when this arm returns; `kill_on_drop(true)` on the child ensures
            // the process is killed when the `Child` inside the future drops.
            () = tokio::time::sleep(timeout) => {
                Err(ToolError::Execution(format!(
                    "timed out after {:.0}s",
                    timeout.as_secs_f64()
                )))
            }

            // Branch 3: cancellation.
            () = cancel.cancelled() => {
                Err(ToolError::Cancelled)
            }
        }
    }
}

// ============================================================
// Helpers
// ============================================================

/// Converts a completed process `output` into a [`ToolOutput`].
fn build_tool_output(output: &std::process::Output, timed_out: bool) -> ToolOutput {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_code = output.status.code().unwrap_or(-1);

    // Combined view for the human-readable text field.
    let combined = clamp_output(format!("{stdout}{stderr}"));
    let text = format!("Exit code: {exit_code}\n{combined}");

    // Individual streams for the structured value (also clamped).
    let stdout_clamped = {
        let (s, _) = crate::tools::builtin::truncate_utf8(&stdout, MAX_TOOL_OUTPUT_BYTES);
        s
    };
    let stderr_clamped = {
        let (s, _) = crate::tools::builtin::truncate_utf8(&stderr, MAX_TOOL_OUTPUT_BYTES);
        s
    };

    let value = serde_json::json!({
        "exit_code": exit_code,
        "stdout":    stdout_clamped,
        "stderr":    stderr_clamped,
        "timed_out": timed_out
    });

    ToolOutput::with_value(text, value)
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
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

    fn ctx_with_token(cancel: CancellationToken) -> ToolContext {
        ToolContext {
            thread_id: ThreadId(Arc::from("thread:native/test")),
            turn_id: TurnId(Arc::from("turn:0")),
            cancel,
            spawner: None,
        }
    }

    #[tokio::test]
    async fn bash_echo_returns_output() {
        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "echo hello_world" });
        let out = tool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("hello_world"));
    }

    #[tokio::test]
    async fn bash_exit_code_captured() {
        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "exit 42" });
        let out = tool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("42"));
        let val = out.value.unwrap();
        assert_eq!(val["exit_code"], 42);
    }

    #[tokio::test]
    async fn bash_stderr_captured() {
        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "echo err_msg >&2" });
        let out = tool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("err_msg"));
        let val = out.value.unwrap();
        assert!(val["stderr"].as_str().unwrap().contains("err_msg"));
    }

    #[tokio::test]
    async fn bash_timeout_returns_error() {
        let tool = BashTool::new();
        let args = serde_json::json!({
            "command": "sleep 60",
            "timeout_ms": 100
        });
        let err = tool.execute(args, &ctx()).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(ref m) if m.contains("timed out")));
    }

    #[tokio::test]
    async fn bash_cancel_returns_cancelled() {
        let token = CancellationToken::new();
        let tool = BashTool::new();
        let ctx = ctx_with_token(token.clone());
        let args = serde_json::json!({
            "command": "sleep 60"
        });
        // Cancel after a brief delay so the process has time to start.
        let token_clone = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            token_clone.cancel();
        });
        let err = tool.execute(args, &ctx).await.unwrap_err();
        assert_eq!(err, ToolError::Cancelled);
    }

    #[tokio::test]
    async fn bash_cwd_respected() {
        let dir = tempfile::TempDir::new().unwrap();
        let tool = BashTool::new();
        let args = serde_json::json!({
            "command": "pwd",
            "cwd": dir.path().to_str().unwrap()
        });
        let out = tool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains(dir.path().to_str().unwrap()));
    }

    #[tokio::test]
    async fn bash_stdin_is_null_no_hang() {
        // A command that reads stdin must not hang: stdin is /dev/null so it
        // gets EOF and exits well within the short timeout.
        let tool = BashTool::new();
        let args = serde_json::json!({
            "command": "cat",
            "timeout_ms": 2000
        });
        let out = tool.execute(args, &ctx()).await.unwrap();
        // `cat` on empty stdin exits 0 with no output.
        let val = out.value.unwrap();
        assert_eq!(val["exit_code"], 0);
    }

    #[tokio::test]
    async fn bash_max_timeout_clamped() {
        // Supply a timeout larger than MAX_BASH_TIMEOUT; the command should
        // still work (we verify clamping does not error, not that it waited).
        let tool = BashTool::new();
        let args = serde_json::json!({
            "command": "echo hi",
            "timeout_ms": 99_999_999u64
        });
        let out = tool.execute(args, &ctx()).await.unwrap();
        assert!(out.text.contains("hi"));
    }
}

// Rust guideline compliant 2026-02-21

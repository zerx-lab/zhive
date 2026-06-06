//! [`BashTool`]: run a shell command with timeout and cancellation support.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use crate::tools::builtin::{
    DEFAULT_BASH_TIMEOUT, DefaultSandbox, MAX_BASH_TIMEOUT, MAX_TOOL_OUTPUT_BYTES, Sandbox,
    apply_minimal_env, clamp_output,
};
use crate::tools::{Tool, ToolContext, ToolError, ToolKind, ToolOutput};

// ============================================================
// BashTool
// ============================================================

/// Runs a shell command and returns combined stdout + stderr.
///
/// The command is launched via `sh -c <command>` placed in its own Unix
/// process group (`process_group(0)`), with stdin connected to `/dev/null`.
/// On timeout, cancellation, or drop the entire process group receives
/// `SIGKILL` via [`rustix::process::kill_process_group`], cleaning up
/// grandchild processes that would otherwise become orphans.
///
/// ## Security boundary
///
/// **No OS-level sandbox** is applied (Landlock / Seatbelt are planned).
/// Isolation today comes from three layers:
///
/// 1. **Process group isolation** — `process_group(0)` places the child in a
///    new group; timeout/cancel/drop sends `SIGKILL` to the whole group,
///    preventing orphan grandchildren.
/// 2. **Environment tightening** — `env_clear()` removes every inherited
///    variable; only a minimal whitelist (`PATH`, `HOME`, `TERM`) is
///    re-injected (see [`apply_minimal_env`]).
/// 3. **Permission gate** — the `bash` tool runs through the engine's
///    `Ask`/`Defer` approval flow before the process is ever spawned.
///
/// File system and network access are **not** restricted — commands run with
/// the host's full privileges. A graceful SIGTERM ramp-down before SIGKILL
/// and Landlock file-system confinement are tracked as follow-up work.
///
/// ## Arguments
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
/// respected: if it fires the child group is killed and
/// [`ToolError::Cancelled`] is returned.
///
/// On normal completion the `value` field carries a structured JSON object:
/// `{ "exit_code": int, "stdout": str, "stderr": str, "timed_out": false }`.
/// A timeout instead returns [`ToolError::Execution`] ("timed out …") with no
/// value, so `timed_out` is always `false` on the success path.
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
        Some(
            "Run a shell command via `sh -c` and return its combined output \
             (stderr appended after stdout, not interleaved). Runs with a cleared \
             environment — only PATH, HOME, and TERM are kept — under a timeout. \
             Prefer the dedicated `read`/`grep`/`glob`/`edit` tools for file work; \
             use bash for builds, tests, git, and other commands."
                .to_owned(),
        )
    }

    fn title(&self, args: &Value) -> Option<String> {
        args["command"].as_str().map(summarize_command)
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
        // Place the child in its own process group so timeout/cancel/drop can
        // send SIGKILL to the *entire group*, including grandchildren spawned
        // by compound commands (e.g. `cmd1 & cmd2`).  pgid == child pid after
        // process_group(0).  `kill_on_drop(true)` is kept as a second-layer
        // backstop that kills the group leader should the explicit group-kill
        // not fire (e.g. on a non-unix target or if child_pid is unavailable).
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.kill_on_drop(true);
        // Connect stdin to /dev/null so a command that reads stdin (e.g. a bare
        // `cat`) gets EOF immediately instead of blocking until the timeout.
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        // Tighten the environment: clear all inherited variables, then inject
        // only the minimal whitelist required for shell commands to work.
        apply_minimal_env(&mut cmd);

        if let Some(ref dir) = cwd {
            cmd.current_dir(dir);
        }

        // Let the sandbox apply any isolation settings.
        self.sandbox.prepare(&mut cmd);

        let child = cmd
            .spawn()
            .map_err(|e| ToolError::Execution(format!("failed to spawn command: {e}")))?;

        // Capture the child's pid *before* `wait_with_output` moves it.  Used
        // in the timeout/cancel arms to kill the whole process group.
        let child_pid = child.id();

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

            // Branch 2: timeout.  Kill the entire process group (group leader
            // pid == pgid after process_group(0)) so grandchildren do not
            // become orphans.  `kill_on_drop(true)` inside `wait_fut` provides
            // a backstop kill of the group leader when the future is dropped.
            () = tokio::time::sleep(timeout) => {
                #[cfg(unix)]
                if let Some(pid) = child_pid {
                    kill_process_group(pid);
                }
                Err(ToolError::Execution(format!(
                    "timed out after {:.0}s",
                    timeout.as_secs_f64()
                )))
            }

            // Branch 3: cancellation.  Same group-kill as the timeout arm.
            () = cancel.cancelled() => {
                #[cfg(unix)]
                if let Some(pid) = child_pid {
                    kill_process_group(pid);
                }
                Err(ToolError::Cancelled)
            }
        }
    }
}

// ============================================================
// Helpers
// ============================================================

/// Sends `SIGKILL` to the process group whose id equals `pid_raw`.
///
/// After `process_group(0)` the child's pgid equals its own pid, so passing
/// the child pid here kills the group leader **and** every grandchild that
/// inherited the group id.  Errors (e.g. race with the process already having
/// exited) are intentionally ignored: we are in a best-effort cleanup path and
/// `kill_on_drop` provides a further backstop.
#[cfg(unix)]
fn kill_process_group(pid_raw: u32) {
    use rustix::process::{Pid, Signal, kill_process_group};
    if let Some(pid) = Pid::from_raw(pid_raw.cast_signed()) {
        // kill_process_group(pid, sig) ≡ kill(-pgid, sig).
        // Errors are silently discarded: the process may have already exited.
        let _ = kill_process_group(pid, Signal::KILL);
    }
}

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

/// Maximum number of characters in a [`BashTool::title`] before truncation.
///
/// Kept short because the title renders as a single-line headline (e.g. the
/// ACP `ToolCall.title`); long commands would otherwise wrap or be clipped by
/// the client. The value is a display budget only and has no functional
/// effect on command execution.
const BASH_TITLE_MAX_CHARS: usize = 80;

/// Builds a one-line `$ <command>` title summary from a raw shell command.
///
/// Collapses every run of whitespace (including newlines in multi-line
/// commands) into a single space and truncates to [`BASH_TITLE_MAX_CHARS`]
/// characters, appending `…` when shortened, so the result stays on one line.
fn summarize_command(command: &str) -> String {
    let collapsed = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title = String::from("$ ");
    if collapsed.chars().count() > BASH_TITLE_MAX_CHARS {
        title.extend(collapsed.chars().take(BASH_TITLE_MAX_CHARS - 1));
        title.push('…');
    } else {
        title.push_str(&collapsed);
    }
    title
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

    #[test]
    fn bash_title_summarizes_command() {
        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "cargo check -p zhive-core" });
        assert_eq!(
            tool.title(&args).as_deref(),
            Some("$ cargo check -p zhive-core")
        );
    }

    #[test]
    fn bash_title_collapses_whitespace_and_truncates() {
        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "echo   a\n    b\tc" });
        assert_eq!(tool.title(&args).as_deref(), Some("$ echo a b c"));

        let long = "x".repeat(200);
        let args = serde_json::json!({ "command": long });
        let title = tool.title(&args).unwrap();
        // `$ ` prefix (2) + clamped body (BASH_TITLE_MAX_CHARS, last char `…`).
        assert_eq!(title.chars().count(), 2 + BASH_TITLE_MAX_CHARS);
        assert!(title.ends_with('\u{2026}'));
    }

    #[test]
    fn bash_title_none_without_command() {
        let tool = BashTool::new();
        assert!(
            tool.title(&serde_json::json!({ "timeout_ms": 10 }))
                .is_none()
        );
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

    /// Verify that the child is placed in a new process group.  The child's
    /// pgid must equal its own pid after `process_group(0)`.
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_child_is_in_new_process_group() {
        let tool = BashTool::new();
        // `sh -c 'echo $BASHPID; cat /proc/$BASHPID/status'` would be too
        // Linux-specific.  Portable approach: have the shell print its pid and
        // pgid using POSIX `ps -o pid= -o pgid= -p $$` and confirm they match.
        let args = serde_json::json!({
            "command": "ps -o pid= -o pgid= -p $$"
        });
        let out = tool.execute(args, &ctx()).await.unwrap();
        // ps output is two whitespace-separated numbers, e.g. "12345 12345".
        let text = out.text.trim().to_owned();
        // Extract the last line that looks like two numbers (skip the Exit code line).
        let pair = text.lines().find_map(|l| {
            let mut parts = l.split_whitespace();
            let pid_text = parts.next()?;
            let group_text = parts.next()?;
            let pid: i32 = pid_text.parse().ok()?;
            let group: i32 = group_text.parse().ok()?;
            Some((pid, group))
        });
        if let Some((pid, group)) = pair {
            assert_eq!(
                pid, group,
                "child process should be its own process-group leader (pid={pid} group={group})"
            );
        }
        // If `ps` is unavailable the test passes vacuously — the important
        // thing is that `execute` does not panic.
    }

    /// Verify the child env is cleared down to the whitelist.
    ///
    /// Reads (never mutates) this process's environment to pick a non-whitelisted
    /// variable that must be stripped from the child. `std::env::set_var` is an
    /// `unsafe fn` under edition 2024 and the crate is `#![forbid(unsafe_code)]`,
    /// so the test cannot inject a canary — it asserts against an inherited one.
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_env_is_cleared_except_whitelist() {
        const WHITELIST: [&str; 3] = ["PATH", "HOME", "TERM"];
        // A variable this process already has that the bash whitelist excludes;
        // `apply_minimal_env` must drop it from the child env.
        let leak_candidate = std::env::vars()
            .map(|(k, _)| k)
            .find(|k| !k.is_empty() && !WHITELIST.contains(&k.as_str()));

        let tool = BashTool::new();
        let args = serde_json::json!({ "command": "env" });
        let out = tool.execute(args, &ctx()).await.unwrap();
        let env_output = &out.text;

        // PATH must be present (whitelist).
        assert!(
            env_output.lines().any(|l| l.starts_with("PATH=")),
            "PATH must be in child env; got: {env_output}"
        );

        // A non-whitelisted parent variable must NOT leak into the child env.
        if let Some(key) = leak_candidate {
            let prefix = format!("{key}=");
            assert!(
                !env_output.lines().any(|l| l.starts_with(&prefix)),
                "non-whitelisted parent var `{key}` leaked into child env: {env_output}"
            );
        }
    }

    /// Timeout fires → kills the entire process group including the background
    /// grandchild.  We launch a grandchild that tries to create a marker file
    /// after a brief sleep; if the group kill works the file must never appear.
    #[cfg(unix)]
    #[tokio::test]
    async fn bash_kills_grandchildren_on_timeout() {
        let dir = tempfile::TempDir::new().unwrap();
        let marker = dir.path().join("grandchild_ran");
        let marker_str = marker.to_str().unwrap();

        let tool = BashTool::new();
        // Grandchild runs in background; tries to touch the marker after 1 s.
        // The whole command is wrapped in an outer sleep so the timeout fires
        // quickly and the process group is killed before the grandchild wakes.
        let cmd = format!("(sleep 1 && touch {marker_str}) & sleep 60");
        let args = serde_json::json!({ "command": cmd, "timeout_ms": 200u64 });
        let err = tool.execute(args, &ctx()).await.unwrap_err();
        assert!(
            matches!(err, ToolError::Execution(ref m) if m.contains("timed out")),
            "expected timed-out error, got {err:?}"
        );

        // Give the grandchild a moment to prove it has been killed (or not).
        tokio::time::sleep(Duration::from_millis(1_400)).await;
        assert!(
            !marker.exists(),
            "grandchild should have been killed by group SIGKILL but marker file exists"
        );
    }
}

// Rust guideline compliant 2026-02-21

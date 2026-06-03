//! Subprocess hook execution: spawn a child, feed JSON stdin, read JSON stdout.
//!
//! This module backs the [`HookExecutor::Subprocess`] arm of the hook
//! host. A subprocess hook is an external program that follows the Claude
//! Code command-hook protocol:
//!
//! 1. The host serialises the [`HookEvent`] to JSON and writes it to the
//!    child's stdin, then closes stdin so a reader sees EOF.
//! 2. The child runs to completion (or is killed on timeout / cancel).
//! 3. The exit code decides the outcome:
//!    * exit `0` — stdout is parsed as a [`HookOutput`] (empty stdout means
//!      *no opinion*; non-JSON stdout is ignored).
//!    * exit `2` — a *blocking* signal. For events that may block, the
//!      child's stderr becomes the block reason; for non-blocking events
//!      (`PostToolUse` / `PostToolUseFailure`) it is reported but does not
//!      block, matching Claude Code.
//!    * any other exit code, or termination by signal — a non-blocking
//!      error: the host logs it and the hook contributes no decision.
//!
//! Process isolation is layered: a spawn failure, an I/O failure, or a
//! timeout each degrade to *skip this hook, keep dispatching*. Only a
//! fired [`CancellationToken`] short-circuits the whole dispatch. Because
//! every subprocess hook is a separate OS process, a crash in the child
//! can never corrupt the host runtime — stronger isolation than the
//! in-process `catch_unwind` path.
//!
//! [`HookExecutor::Subprocess`]: super::HookExecutor

use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use zhive_proto::hook::HookEvent;
use zhive_proto::permission::{HookOutput, HookSpecificOutput, PermissionDecision};

/// Default subprocess timeout when a registration leaves it unset.
///
/// Mirrors the Claude Code `command` hook default of 600 seconds: large
/// enough that a legitimate hook (e.g. running a formatter over a repo)
/// can finish, while still bounding a runaway child. The host clamps
/// nothing on top of this; callers pick a tighter value per registration.
pub const DEFAULT_SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(600);

/// Specification for a hook implemented as an external program.
///
/// Carries everything needed to spawn the child: the program (or shell
/// snippet when [`shell`](Self::shell) is set), its arguments, a list of
/// environment variables to inject, and an optional working directory.
///
/// # Examples
///
/// ```
/// use zhive_core::hooks::SubprocessSpec;
/// let spec = SubprocessSpec {
///     program: "echo '{}'".into(),
///     args: Vec::new(),
///     env: vec![("FOO".into(), "bar".into())],
///     cwd: None,
///     shell: true,
/// };
/// assert!(spec.shell);
/// assert_eq!(spec.env[0].0, "FOO");
/// ```
///
/// This is a plain configuration record: every field is public and a
/// struct literal is the intended way to build one (it is intentionally
/// **not** `#[non_exhaustive]`). [`SubprocessSpec::new`] is a convenience
/// for the common "run a program directly" case.
#[derive(Debug, Clone)]
pub struct SubprocessSpec {
    /// Program to run, or the shell snippet when `shell` is `true`.
    pub program: String,
    /// Arguments passed verbatim (ignored when `shell` is `true`).
    pub args: Vec<String>,
    /// Environment variables injected into the child.
    pub env: Vec<(String, String)>,
    /// Working directory; falls back to the event `cwd` when `None`.
    pub cwd: Option<std::path::PathBuf>,
    /// Run `program` via `sh -c` instead of executing it directly.
    pub shell: bool,
}

impl SubprocessSpec {
    /// Builds a spec that runs `program` directly with no arguments.
    ///
    /// `shell` is `false`, `env` is empty and `cwd` falls back to the
    /// event working directory at dispatch time.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::hooks::SubprocessSpec;
    /// let spec = SubprocessSpec::new("/usr/bin/true");
    /// assert!(!spec.shell);
    /// assert!(spec.args.is_empty());
    /// ```
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            shell: false,
        }
    }
}

/// Errors that abort a subprocess hook dispatch (rather than degrading it).
///
/// Spawn / I/O / timeout failures do **not** surface here: they degrade to
/// `Ok(None)` so dispatch keeps going. Only two conditions reach the
/// caller: a fired cancellation token (which short-circuits the whole
/// dispatch chain) and a failure to serialise the event for stdin (a
/// programming error in the wire types, surfaced rather than hidden).
///
/// # Examples
///
/// ```
/// use zhive_core::hooks::SubprocessHookError;
/// // Only the two aborting variants exist: Cancelled and Serialize.
/// // Construct a Cancelled to verify the Display message:
/// let err = SubprocessHookError::Cancelled;
/// assert!(matches!(err, SubprocessHookError::Cancelled));
/// assert_eq!(err.to_string(), "hook subprocess dispatch cancelled");
/// ```
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SubprocessHookError {
    /// The dispatch cancellation token fired before the child finished.
    #[error("hook subprocess dispatch cancelled")]
    Cancelled,

    /// The hook event could not be serialised to JSON for stdin.
    #[error("failed to serialize hook event for subprocess stdin: {0}")]
    Serialize(#[from] serde_json::Error),
}

/// Runs one subprocess hook and returns its decision, if any.
///
/// Spawns the child described by `spec`, writes `event` as JSON to its
/// stdin, and interprets the exit code into an optional [`HookOutput`].
/// `timeout` bounds the run; `cancel` short-circuits it.
///
/// Returns `Ok(None)` for every degraded outcome (spawn failure, I/O
/// failure, timeout, non-blocking exit code, unparsable stdout) so the
/// caller can simply skip the hook and continue dispatching.
///
/// # Errors
///
/// * [`SubprocessHookError::Serialize`] when `event` cannot be encoded as
///   JSON for the child's stdin.
/// * [`SubprocessHookError::Cancelled`] when `cancel` fires before the
///   child exits; the child is killed via `kill_on_drop`.
pub(crate) async fn run_subprocess_hook(
    spec: &SubprocessSpec,
    event: &HookEvent,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<Option<HookOutput>, SubprocessHookError> {
    // Cheap pre-check: if the turn is already cancelled, do not spawn.
    if cancel.is_cancelled() {
        return Err(SubprocessHookError::Cancelled);
    }

    // stdin payload is the wire HookEvent — already the Claude Code shape.
    let input = serde_json::to_string(event)?;

    let mut cmd = build_command(spec, event);
    cmd.kill_on_drop(true);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            // First isolation layer: a hook that cannot even start must
            // not abort the engine. Skip it and keep dispatching.
            tracing::warn!(
                name: "zhive.hooks.subprocess.spawn_failed",
                program = spec.program.as_str(),
                error_message = %err,
                "hook subprocess failed to spawn; skipping"
            );
            return Ok(None);
        }
    };

    // Feed stdin then close it so a child reading stdin gets EOF instead of
    // blocking until the timeout (the single most common subprocess-hook
    // deadlock; see hooks-subprocess-FULL.md risk #1).
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(err) = stdin.write_all(input.as_bytes()).await {
            tracing::warn!(
                name: "zhive.hooks.subprocess.stdin_failed",
                program = spec.program.as_str(),
                error_message = %err,
                "failed to write hook event to subprocess stdin; killing and skipping"
            );
            // Drop the handle and force-kill; dispatch continues.
            drop(stdin);
            let _ = child.kill().await;
            return Ok(None);
        }
        // Explicit shutdown sends EOF; dropping the handle alone is enough
        // but shutdown makes the intent and ordering unambiguous.
        let _ = stdin.shutdown().await;
        drop(stdin);
    }

    // `wait_with_output` consumes the child, so pin it before `select!`.
    let wait_fut = child.wait_with_output();
    tokio::pin!(wait_fut);

    tokio::select! {
        result = &mut wait_fut => {
            match result {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    Ok(interpret_exit(
                        event,
                        output.status.code(),
                        stdout.trim(),
                        stderr.trim(),
                        spec,
                    ))
                }
                Err(err) => {
                    // Second isolation layer: an I/O error waiting on the
                    // child degrades to no-opinion.
                    tracing::warn!(
                        name: "zhive.hooks.subprocess.wait_failed",
                        program = spec.program.as_str(),
                        error_message = %err,
                        "hook subprocess wait failed; skipping"
                    );
                    Ok(None)
                }
            }
        }

        // Third isolation layer: timeout. Dropping the pinned future drops
        // the `Child`; `kill_on_drop(true)` then terminates the process.
        () = tokio::time::sleep(timeout) => {
            tracing::warn!(
                name: "zhive.hooks.subprocess.timeout",
                program = spec.program.as_str(),
                timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                "hook subprocess timed out; killing and skipping"
            );
            Ok(None)
        }

        // Cancellation short-circuits the whole dispatch chain.
        () = cancel.cancelled() => {
            tracing::warn!(
                name: "zhive.hooks.subprocess.cancelled",
                program = spec.program.as_str(),
                "hook subprocess dispatch cancelled; killing"
            );
            Err(SubprocessHookError::Cancelled)
        }
    }
}

/// Builds the [`Command`] for `spec`, applying env, cwd and shell mode.
///
/// When `spec.shell` is set the program is run via `sh -c <program>`
/// (POSIX shells only; subprocess hooks target Unix hosts). Otherwise the
/// program is executed directly with `spec.args`. The working directory
/// falls back to the event `cwd` when the spec leaves it unset.
fn build_command(spec: &SubprocessSpec, event: &HookEvent) -> Command {
    let mut cmd = if spec.shell {
        let mut c = Command::new("sh");
        c.arg("-c").arg(&spec.program);
        c
    } else {
        let mut c = Command::new(&spec.program);
        c.args(&spec.args);
        c
    };

    for (key, value) in &spec.env {
        cmd.env(key, value);
    }

    if let Some(dir) = &spec.cwd {
        cmd.current_dir(dir);
    } else {
        let cwd = event_cwd(event);
        if !cwd.is_empty() {
            cmd.current_dir(cwd);
        }
    }

    cmd
}

/// Interprets the child's exit code and streams into a [`HookOutput`].
///
/// Follows the Claude Code command-hook exit-code contract. `stdout` and
/// `stderr` are already trimmed. Returns `None` for every "no decision"
/// outcome so the caller skips the hook.
fn interpret_exit(
    event: &HookEvent,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    spec: &SubprocessSpec,
) -> Option<HookOutput> {
    /// Exit code that signals a blocking decision (Claude Code contract).
    const EXIT_BLOCK: i32 = 2;

    match exit_code {
        Some(0) => {
            if stdout.is_empty() {
                return None;
            }
            if !looks_like_json(stdout) {
                // Plain-text output is informational; not a decision.
                return None;
            }
            match serde_json::from_str::<HookOutput>(stdout) {
                Ok(output) => Some(output),
                Err(err) => {
                    tracing::warn!(
                        name: "zhive.hooks.subprocess.invalid_output",
                        program = spec.program.as_str(),
                        error_message = %err,
                        "hook subprocess emitted invalid HookOutput JSON; ignoring"
                    );
                    None
                }
            }
        }
        Some(EXIT_BLOCK) => {
            if !event_can_block(event) {
                // PostToolUse / PostToolUseFailure cannot block; surface
                // the reason but contribute no blocking decision.
                tracing::warn!(
                    name: "zhive.hooks.subprocess.block_ignored",
                    program = spec.program.as_str(),
                    reason = stderr,
                    "hook subprocess exited 2 on a non-blockable event; ignoring block"
                );
                return None;
            }
            if stderr.is_empty() {
                tracing::warn!(
                    name: "zhive.hooks.subprocess.block_no_reason",
                    program = spec.program.as_str(),
                    "hook subprocess exited 2 without a stderr reason; ignoring block"
                );
                return None;
            }
            Some(block_output(event, stderr))
        }
        Some(code) => {
            tracing::warn!(
                name: "zhive.hooks.subprocess.nonzero_exit",
                program = spec.program.as_str(),
                exit_code = code,
                "hook subprocess exited non-zero (non-blocking); ignoring"
            );
            None
        }
        None => {
            tracing::warn!(
                name: "zhive.hooks.subprocess.no_exit_code",
                program = spec.program.as_str(),
                "hook subprocess terminated without an exit code (killed by signal); ignoring"
            );
            None
        }
    }
}

/// Synthesises a blocking [`HookOutput`] carrying `reason` as the rationale.
///
/// For `PreToolUse` events the output uses the typed
/// `HookSpecificOutput::PreToolUse { Deny }` so the downstream permission
/// reducer sees the correct decision. For every other blockable event
/// (`UserPromptSubmit`, `Stop`, `SubagentStop`, `PreCompact`, …) the
/// `continue_loop = Some(false)` field signals a block without pinning a
/// `PreToolUse`-specific type that would be semantically wrong.
fn block_output(event: &HookEvent, reason: &str) -> HookOutput {
    // `HookOutput` is `#[non_exhaustive]`, so a struct literal is rejected
    // outside the proto crate; mutate a `Default` value instead.
    let mut output = HookOutput::default();
    if matches!(event, HookEvent::PreToolUse(_)) {
        // PreToolUse: use the typed permission-decision field.
        output.hook_specific_output = Some(HookSpecificOutput::PreToolUse {
            permission_decision: PermissionDecision::Deny,
            permission_decision_reason: Some(reason.to_owned()),
            updated_input: None,
        });
    } else {
        // Other blockable events: signal stop via continue_loop.
        // The stderr reason is carried as system_message so callers can log it.
        output.continue_loop = Some(false);
        output.system_message = Some(reason.to_owned());
    }
    output
}

/// Returns `true` when an exit-2 from `event`'s hook may block.
///
/// `PostToolUse` and `PostToolUseFailure` run after the tool has already
/// executed, so they cannot block (Claude Code contract); every other
/// event may. Unknown forward-compat variants are treated as blockable so
/// a future blocking event is not silently downgraded.
fn event_can_block(event: &HookEvent) -> bool {
    !matches!(
        event,
        HookEvent::PostToolUse(_) | HookEvent::PostToolUseFailure(_)
    )
}

/// Returns the event working directory, or an empty string for `Unknown`.
fn event_cwd(event: &HookEvent) -> &str {
    match event {
        HookEvent::PreToolUse(p) => &p.base.cwd,
        HookEvent::PostToolUse(p) => &p.base.cwd,
        HookEvent::PostToolUseFailure(p) => &p.base.cwd,
        HookEvent::UserPromptSubmit(p) => &p.base.cwd,
        HookEvent::PermissionRequest(p) => &p.base.cwd,
        HookEvent::ToolApprovalChange(p) => &p.base.cwd,
        HookEvent::Stop(p) => &p.base.cwd,
        HookEvent::Notification(p) => &p.base.cwd,
        HookEvent::Setup(p) => &p.base.cwd,
        HookEvent::SubagentStart(p) => &p.base.cwd,
        HookEvent::SubagentStop(p) => &p.base.cwd,
        HookEvent::PreCompact(p) => &p.base.cwd,
        // PostCompact and branch-summary variants were previously falling
        // through to the `_ => ""` arm, losing the event cwd. Fixed here.
        HookEvent::PostCompact(p) => &p.base.cwd,
        HookEvent::PreBranchSummary(p) => &p.base.cwd,
        HookEvent::PostBranchSummary(p) => &p.base.cwd,
        HookEvent::SessionStart(p) => &p.base.cwd,
        HookEvent::SessionEnd(p) => &p.base.cwd,
        HookEvent::PhaseTransition(p) => &p.base.cwd,
        // `HookEvent` is `#[non_exhaustive]`; forward-compat fallback.
        _ => "",
    }
}

/// Returns `true` when `s` looks like a JSON object or array.
///
/// Used to distinguish a child that *meant* to emit JSON (and may have
/// emitted it incorrectly) from one that printed plain text. Plain text is
/// treated as informational and ignored.
fn looks_like_json(s: &str) -> bool {
    let trimmed = s.trim_start();
    trimmed.starts_with('{') || trimmed.starts_with('[')
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Builds a `PreToolUse` event with the given cwd so subprocess tests
    /// can assert the child inherits it.
    fn pre_tool_use_event(cwd: &str) -> HookEvent {
        serde_json::from_value(serde_json::json!({
            "hook_event_name": "PreToolUse",
            "sessionId": "s",
            "cwd": cwd,
            "registeredBy": {
                "id": "test",
                "version": "0.1.0",
                "source": "builtin",
            },
            "toolName": "bash",
            "toolInput": {},
            "toolUseId": "tool:0",
        }))
        .expect("static fixture must deserialise")
    }

    fn post_tool_use_event() -> HookEvent {
        serde_json::from_value(serde_json::json!({
            "hook_event_name": "PostToolUse",
            "sessionId": "s",
            "cwd": "/",
            "registeredBy": {
                "id": "test",
                "version": "0.1.0",
                "source": "builtin",
            },
            "toolName": "bash",
            "toolInput": {},
            "toolResponse": {},
            "toolUseId": "tool:0",
        }))
        .expect("static fixture must deserialise")
    }

    fn shell_spec(script: &str) -> SubprocessSpec {
        SubprocessSpec {
            program: script.to_owned(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            shell: true,
        }
    }

    #[tokio::test]
    async fn round_trip_emits_hook_output() {
        // The child reads stdin (drains it) then prints a HookOutput JSON.
        let json = r#"{"systemMessage":"from-subprocess"}"#;
        let spec = shell_spec(&format!("cat >/dev/null; printf '%s' '{json}'"));
        let event = pre_tool_use_event("/");
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect("dispatch ok")
            .expect("some output");
        assert_eq!(out.system_message.as_deref(), Some("from-subprocess"));
    }

    #[tokio::test]
    async fn stdin_receives_serialized_event() {
        // Echo a field plucked from stdin back through systemMessage so we
        // can prove the child actually received the serialized event.
        let spec = shell_spec(
            "in=$(cat); echo \"{\\\"systemMessage\\\":\\\"$(echo \"$in\" | grep -o bash | head -1)\\\"}\"",
        );
        let event = pre_tool_use_event("/");
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect("dispatch ok")
            .expect("some output");
        assert_eq!(out.system_message.as_deref(), Some("bash"));
    }

    #[tokio::test]
    async fn exit0_empty_stdout_is_no_opinion() {
        let spec = shell_spec("cat >/dev/null; exit 0");
        let event = pre_tool_use_event("/");
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect("dispatch ok");
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn exit2_with_stderr_blocks() {
        let spec = shell_spec("cat >/dev/null; echo blocked-reason >&2; exit 2");
        let event = pre_tool_use_event("/");
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect("dispatch ok")
            .expect("block output");
        assert!(matches!(
            out.hook_specific_output,
            Some(HookSpecificOutput::PreToolUse {
                permission_decision: PermissionDecision::Deny,
                permission_decision_reason: Some(ref r),
                ..
            }) if r == "blocked-reason"
        ));
    }

    #[tokio::test]
    async fn exit2_without_stderr_is_non_blocking() {
        let spec = shell_spec("cat >/dev/null; exit 2");
        let event = pre_tool_use_event("/");
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect("dispatch ok");
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn post_tool_use_exit2_cannot_block() {
        let spec = shell_spec("cat >/dev/null; echo nope >&2; exit 2");
        let event = post_tool_use_event();
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect("dispatch ok");
        assert!(out.is_none(), "PostToolUse exit 2 must not block");
    }

    #[tokio::test]
    async fn other_nonzero_exit_is_non_blocking() {
        let spec = shell_spec("cat >/dev/null; exit 1");
        let event = pre_tool_use_event("/");
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect("dispatch ok");
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn plain_text_stdout_is_ignored() {
        let spec = shell_spec("cat >/dev/null; echo not-json; exit 0");
        let event = pre_tool_use_event("/");
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect("dispatch ok");
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn malformed_json_stdout_is_ignored() {
        let spec = shell_spec("cat >/dev/null; echo '{bad'; exit 0");
        let event = pre_tool_use_event("/");
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect("dispatch ok");
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn timeout_kills_and_skips() {
        let spec = shell_spec("sleep 30");
        let event = pre_tool_use_event("/");
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_millis(50), &token)
            .await
            .expect("dispatch ok despite timeout");
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn spawn_failure_is_skipped() {
        let spec = SubprocessSpec::new("/nonexistent/zhive-hook-binary");
        let event = pre_tool_use_event("/");
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect("dispatch ok despite spawn failure");
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn pre_cancelled_token_short_circuits() {
        let spec = shell_spec("sleep 30");
        let event = pre_tool_use_event("/");
        let token = CancellationToken::new();
        token.cancel();
        let err = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect_err("pre-cancelled token must short-circuit");
        assert!(matches!(err, SubprocessHookError::Cancelled));
    }

    #[tokio::test]
    async fn cancel_during_run_kills_child() {
        let spec = shell_spec("cat >/dev/null; sleep 30");
        let event = pre_tool_use_event("/");
        let token = CancellationToken::new();
        let token_clone = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            token_clone.cancel();
        });
        let err = run_subprocess_hook(&spec, &event, Duration::from_secs(30), &token)
            .await
            .expect_err("cancel must short-circuit");
        assert!(matches!(err, SubprocessHookError::Cancelled));
    }

    #[tokio::test]
    async fn cwd_and_env_are_applied() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let canonical = dir.path().canonicalize().expect("canonicalize tempdir");
        let mut spec = shell_spec(
            // Emit pwd + $FOO into systemMessage so the test can read both.
            r#"cat >/dev/null; printf '{"systemMessage":"%s|%s"}' "$(pwd)" "$FOO""#,
        );
        spec.env = vec![("FOO".to_owned(), "bar".to_owned())];
        spec.cwd = Some(canonical.clone());
        let event = pre_tool_use_event("/");
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect("dispatch ok")
            .expect("some output");
        let msg = out.system_message.expect("system message");
        let expected = format!("{}|bar", canonical.display());
        assert_eq!(msg, expected);
    }

    #[tokio::test]
    async fn event_cwd_used_when_spec_cwd_absent() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let canonical = dir.path().canonicalize().expect("canonicalize tempdir");
        // Spec has no cwd; the child should inherit the event cwd.
        let spec = shell_spec(r#"cat >/dev/null; printf '{"systemMessage":"%s"}' "$(pwd)""#);
        let event = pre_tool_use_event(canonical.to_str().expect("utf8 path"));
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect("dispatch ok")
            .expect("some output");
        assert_eq!(
            out.system_message.as_deref(),
            Some(canonical.display().to_string().as_str())
        );
    }

    #[test]
    fn looks_like_json_detects_objects_and_arrays() {
        assert!(looks_like_json("  {\"a\":1}"));
        assert!(looks_like_json("[1,2]"));
        assert!(!looks_like_json("plain text"));
        assert!(!looks_like_json(""));
    }

    #[test]
    fn spec_new_defaults_to_direct_exec() {
        let spec = SubprocessSpec::new("/usr/bin/true");
        assert!(!spec.shell);
        assert!(spec.args.is_empty());
        assert!(spec.env.is_empty());
        assert!(spec.cwd.is_none());
        // Ensure Arc-wrapping (used by the executor) keeps the spec intact.
        let arced = Arc::new(spec);
        assert_eq!(arced.program, "/usr/bin/true");
    }

    // ------------------------------------------------------------------
    // Regression: exit_code = None (process killed by signal) must yield
    // Ok(None) — a signal-terminated child contributes no decision and
    // does not abort the dispatch chain.
    //
    // Note on the stdin.take() == None defensive path: the path where
    // `child.stdin.take()` returns `None` (meaning the child was spawned
    // without a piped stdin handle) cannot be exercised in an integration
    // test because we always set `Stdio::piped()`. The code path is
    // retained as a safety guard against future spawning changes — if
    // it ever fires in practice the child will simply see EOF immediately,
    // which is the safe fallback for a hook that tries to read stdin.
    #[tokio::test]
    async fn signal_killed_child_yields_no_opinion() {
        // On Unix, `kill -9 $$` terminates the shell without an exit code.
        // `exit_code` will be `None` in `wait_with_output`.  The hook
        // must degrade to Ok(None) so the dispatch chain keeps running.
        let spec = shell_spec("kill -9 $$");
        let event = pre_tool_use_event("/");
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect("dispatch ok despite signal-killed child");
        assert!(
            out.is_none(),
            "signal-killed subprocess must contribute no decision"
        );
    }

    /// For non-PreToolUse blockable events the exit-2 blocking output must
    /// use `continue_loop = Some(false)` rather than forcing a
    /// `HookSpecificOutput::PreToolUse { Deny }` shape (finding B-subprocess-3).
    #[tokio::test]
    async fn exit2_on_stop_event_uses_continue_loop_not_pre_tool_use() {
        let spec = shell_spec("cat >/dev/null; echo blocked-reason >&2; exit 2");
        // Build a Stop event (not PreToolUse) so the block path is exercised
        // with a non-PreToolUse event type.
        let event: HookEvent = serde_json::from_value(serde_json::json!({
            "hook_event_name": "Stop",
            "sessionId": "s",
            "cwd": "/",
            "registeredBy": {
                "id": "test",
                "version": "0.1.0",
                "source": "builtin",
            },
            "stopHookActive": false,
        }))
        .expect("static fixture");
        let token = CancellationToken::new();
        let out = run_subprocess_hook(&spec, &event, Duration::from_secs(5), &token)
            .await
            .expect("dispatch ok")
            .expect("block output");
        // Must NOT be wrapped in PreToolUse{Deny} — that type is wrong for Stop.
        assert!(
            !matches!(
                out.hook_specific_output,
                Some(HookSpecificOutput::PreToolUse { .. })
            ),
            "Stop exit-2 block must not produce PreToolUse output"
        );
        // Must carry continue_loop = Some(false) instead.
        assert_eq!(
            out.continue_loop,
            Some(false),
            "Stop exit-2 block must set continue_loop=false"
        );
    }

    /// `event_cwd` must return the cwd for `PostCompact` and `PostBranchSummary`,
    /// not the empty fallback that the `_ => ""` arm would have produced.
    #[test]
    fn event_cwd_returns_cwd_for_post_compact_and_post_branch_summary() {
        let post_compact: HookEvent = serde_json::from_value(serde_json::json!({
            "hook_event_name": "PostCompact",
            "sessionId": "s",
            "cwd": "/expected",
            "registeredBy": { "id": "t", "version": "0.1.0", "source": "builtin" },
            "trigger": "auto",
            "entriesCompacted": 5_u32,
        }))
        .expect("PostCompact fixture");
        assert_eq!(event_cwd(&post_compact), "/expected");

        let post_branch: HookEvent = serde_json::from_value(serde_json::json!({
            "hook_event_name": "PostBranchSummary",
            "sessionId": "s",
            "cwd": "/branch",
            "registeredBy": { "id": "t", "version": "0.1.0", "source": "builtin" },
            "sourceThreadId": "thread:native/src",
            "entriesSummarized": 3_u32,
        }))
        .expect("PostBranchSummary fixture");
        assert_eq!(event_cwd(&post_branch), "/branch");
    }
}

// Rust guideline compliant 2026-02-21

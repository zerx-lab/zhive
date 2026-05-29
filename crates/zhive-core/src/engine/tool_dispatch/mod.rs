//! Per-tool-call dispatch: hooks → red-line-11 → permission → execute → post-hooks.
//!
//! Each call to [`dispatch_tool_call`] handles **one** tool invocation from the
//! agent loop:
//!
//! 1. Build a `PreToolUse` [`HookEvent`] and dispatch it through the
//!    [`HookHost`].
//! 2. Fold the hook outputs into a single [`PermissionDecision`]; capture the
//!    last `updated_input` if any.
//! 3. **Red line 11**: when a `PreToolUse` hook returns `updated_input`, the
//!    host re-validates it against the tool input schema. If the tool has no
//!    registered schema, revalidation fails (`UnknownTool`) and the call is
//!    blocked, because mutating input for a schema-less tool is the exact
//!    failure mode red line 11 guards against. When no `updated_input` is
//!    returned, no revalidation runs and the call proceeds.
//! 4. Evaluate the final permission decision and execute (or block) the tool.
//! 5. Build a `PostToolUse` [`HookEvent`]; dispatch it and apply any
//!    `updated_tool_output`.
//!
//! ## Phase-1 limitation: `Defer`
//!
//! Full suspend / resume for `Defer` outcomes requires a later increment.
//! For now, `Defer` is treated as a block: a `SystemNotice` is appended
//! explaining the limitation and the tool call is marked `Failed`.
//!
//! ## Outcome model
//!
//! The function returns a [`DispatchOutcome`] so `run_turn` can decide whether
//! to continue the loop (`Executed` / `Blocked`) or terminate it early
//! (`Stop`, from a `continue_loop == false` hook output).

mod helpers;

use std::sync::{Arc, OnceLock};

use tokio_util::sync::CancellationToken;
use zhive_proto::domain::{Item, ItemContent, ItemId, ItemToolCallContent, ToolCallStatus, TurnId};
use zhive_proto::hook::{ExtensionRef, HookEvent};
use zhive_proto::permission::{
    HookOutput, HookSpecificOutput, PermissionDecision, PermissionOutcome,
};

use crate::engine::event::EngineEvent;
use crate::engine::inner::EngineInner;
use crate::hooks::HookHost;
use crate::permission::{PermissionReducer, evaluate};
use crate::tools::{ToolContext, ToolRegistry};

use helpers::{blocked_outcome, build_permission_request, cancelled_during_execution};

// ============================================================
// DispatchOutcome
// ============================================================

/// Result of dispatching a single tool call.
///
/// The result text fed back to the provider is **not** carried here: the
/// finalized [`Item`]'s `content` already holds it, and `build_call_options`
/// reconstructs the tool-result message directly from the item tail. Carrying
/// it twice would risk the two copies drifting out of sync.
#[derive(Debug)]
pub(super) enum DispatchOutcome {
    /// Tool executed (successfully or with an error result) and the loop
    /// should continue unless other conditions stop it.
    Executed {
        /// The finalized Item to store + broadcast.
        item: Item,
        /// If `true`, a hook requested loop termination (like Claude Code Stop).
        stop_loop: bool,
    },
    /// Tool was blocked (denied, schema failure, or missing).  The loop
    /// should continue unless `stop_loop` is set.
    Blocked {
        /// The finalized Item (status=Failed) to store + broadcast.
        item: Item,
        /// If `true`, a hook requested loop termination.
        stop_loop: bool,
    },
}

impl DispatchOutcome {
    /// Returns `true` when a hook requested loop termination.
    pub(super) fn stop_loop(&self) -> bool {
        match self {
            Self::Executed { stop_loop, .. } | Self::Blocked { stop_loop, .. } => *stop_loop,
        }
    }

    /// Returns the item regardless of outcome variant.
    pub(super) fn item(&self) -> &Item {
        match self {
            Self::Executed { item, .. } | Self::Blocked { item, .. } => item,
        }
    }
}

// ============================================================
// Synthetic ExtensionRef for engine-internal hook events
// ============================================================

/// Engine-internal provenance used when building hook events.
///
/// Red line 10 requires every hook event to carry a provenance ref; for
/// events emitted by the engine itself (rather than an extension) this
/// sentinel value is used.
///
/// The value is initialized once from a static JSON template and then
/// cloned on each call.  `OnceLock` ensures initialization is race-free
/// without a mutex on the hot dispatch path.
/// Returns the engine-internal [`ExtensionRef`], memoized after the first call.
///
/// Returns `None` if JSON deserialization fails.  In practice this cannot
/// happen because the engine controls every byte of the template and
/// `ExtensionSource::Builtin` is a stable known enum variant.  The function
/// returns `Option` to avoid `.expect()` in non-test code; callers treat
/// `None` as a transient failure and block the tool call.
fn engine_ext_ref() -> Option<ExtensionRef> {
    static REF: OnceLock<Option<ExtensionRef>> = OnceLock::new();
    REF.get_or_init(|| {
        // ExtensionRef is #[non_exhaustive]; construct via JSON to stay
        // future-safe.  The JSON is 100% engine-controlled.
        let version = env!("CARGO_PKG_VERSION");
        serde_json::from_value(serde_json::json!({
            "id": "zhive.engine",
            "version": version,
            "source": "builtin"
        }))
        .ok()
    })
    .clone()
}

// ============================================================
// dispatch_tool_call
// ============================================================

/// Dispatches one tool call through the full hook → permission → execute
/// → post-hook pipeline.
///
/// `tool_use_id` is the provider-side stable id for this call (the value
/// that appears in the original `ToolCall` item's `raw_input` if the
/// provider emitted it, or a synthetic id otherwise).
///
/// # Errors
///
/// This function does not return a `Result`; all failure modes are folded
/// into the returned [`DispatchOutcome`] so the caller can log and continue
/// without early-return complexity.
#[expect(
    clippy::too_many_lines,
    reason = "dispatch_tool_call spans all six hook/permission/execute steps as one conceptual unit; \
              extracting sub-functions would hurt readability without reducing complexity"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "All args are required context; no good grouping"
)]
pub(super) async fn dispatch_tool_call(
    inner: &Arc<EngineInner>,
    hook_host: &Arc<HookHost>,
    tools: &Arc<ToolRegistry>,
    reducer: &PermissionReducer,
    thread_id_str: &str,
    turn_id: &TurnId,
    item_id: ItemId,
    tool_name: &str,
    raw_args: serde_json::Value,
    tool_use_id: &str,
    scope: &zhive_proto::permission::PermissionScope,
    cancel: &CancellationToken,
) -> DispatchOutcome {
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("/"))
        .to_string_lossy()
        .into_owned();
    // Use the memoized ExtensionRef if available; fall back to a known-good
    // static string so the dispatch path never panics.
    let ext_ref = engine_ext_ref();
    let ext_id = ext_ref.as_ref().map_or("zhive.engine", |r| r.id.as_str());
    let ext_ver = ext_ref
        .as_ref()
        .map_or(env!("CARGO_PKG_VERSION"), |r| r.version.as_str());

    // ---- Step a: PreToolUse hook dispatch ----
    // HookEvent + inner payloads are #[non_exhaustive]; construct via JSON.
    // If construction fails (practically impossible — engine controls the
    // template), block the tool call rather than panicking.
    let pre_event: HookEvent = match serde_json::from_value(serde_json::json!({
        "hook_event_name": "PreToolUse",
        "sessionId": thread_id_str,
        "cwd": cwd,
        "registeredBy": {
            "id": ext_id,
            "version": ext_ver,
            "source": "builtin"
        },
        "toolName": tool_name,
        "toolInput": raw_args,
        "toolUseId": tool_use_id,
    })) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(
                name: "zhive.tool.hook_event_build_failed",
                tool = tool_name,
                error = %err,
                "failed to build PreToolUse HookEvent; blocking tool call"
            );
            return blocked_outcome(
                item_id,
                tool_name,
                raw_args,
                tool_use_id,
                format!("failed to build PreToolUse hook event: {err}"),
                false,
            );
        }
    };

    let hook_outputs: Vec<HookOutput> = match hook_host.dispatch(&pre_event).await {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                name: "zhive.tool.hook_dispatch_failed",
                tool = tool_name,
                error = %err,
                "PreToolUse hook dispatch failed; treating as Deny"
            );
            return blocked_outcome(
                item_id,
                tool_name,
                raw_args,
                tool_use_id,
                format!("PreToolUse hook error: {err}"),
                false,
            );
        }
    };

    // ---- Step b: Fold hook outputs → decisions + updated_input + stop ----
    let mut decisions: Vec<PermissionDecision> = Vec::new();
    let mut updated_input: Option<serde_json::Value> = None;
    let mut stop_loop = false;

    for output in &hook_outputs {
        if output.continue_loop == Some(false) {
            stop_loop = true;
        }
        if let Some(HookSpecificOutput::PreToolUse {
            permission_decision,
            updated_input: maybe_input,
            ..
        }) = &output.hook_specific_output
        {
            decisions.push(*permission_decision);
            if let Some(new_input) = maybe_input {
                updated_input = Some(new_input.clone());
            }
        }
    }

    // ---- Step c: Red line 11 — re-validate updated_input if present ----
    // Only runs when a PreToolUse hook actually returned `updated_input`.
    // If the tool has no registered schema, `revalidate` returns
    // `ValidatorError::UnknownTool` and the call is blocked: mutating input
    // for a schema-less tool is the exact failure mode red line 11 guards
    // against (absent schema is a bug, not an implicit pass). When no
    // `updated_input` is present, this whole block is skipped and the call
    // proceeds with the original args.
    let args = if let Some(new_input) = updated_input {
        match hook_host.schemas().revalidate(tool_name, &new_input) {
            Ok(()) => new_input,
            Err(err) => {
                tracing::warn!(
                    name: "zhive.tool.schema_revalidation_failed",
                    tool = tool_name,
                    error = %err,
                    "red line 11: updated_input failed schema re-validation; blocking tool"
                );
                return blocked_outcome(
                    item_id,
                    tool_name,
                    raw_args,
                    tool_use_id,
                    format!("schema re-validation failed: {err}"),
                    stop_loop,
                );
            }
        }
    } else {
        raw_args.clone()
    };

    // ---- Step d: Permission evaluation ----
    let decision = evaluate(scope, &decisions);
    match decision {
        PermissionDecision::Allow => {
            // Continue to execution below.
        }
        PermissionDecision::Deny => {
            return blocked_outcome(
                item_id,
                tool_name,
                raw_args,
                tool_use_id,
                "permission denied".to_owned(),
                stop_loop,
            );
        }
        PermissionDecision::Ask => {
            // Build reverse-RPC request.
            let request = match build_permission_request(thread_id_str, tool_name) {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!(
                        name: "zhive.tool.permission_request_build_failed",
                        tool = tool_name,
                        error = %err,
                        "failed to build RequestPermissionRequest; treating as Deny"
                    );
                    return blocked_outcome(
                        item_id,
                        tool_name,
                        raw_args,
                        tool_use_id,
                        format!("failed to build permission request: {err}"),
                        stop_loop,
                    );
                }
            };
            let (key, req, rx) = reducer.enroll(request);

            // Emit PermissionRequested so the client/test can answer.
            let wire_id = key.to_wire();
            let _ = inner.events_tx().send(EngineEvent::PermissionRequested {
                request_id: wire_id,
                request: Box::new(req),
            });

            // Race the permission wait against turn cancellation so that a
            // cancelled turn does not block for up to DEFAULT_PERMISSION_TIMEOUT
            // (30 s) and then emit orphaned ItemAppended events after the engine
            // has already rolled back to Idle.  The select! arm for cancellation
            // handles the case where cancel_all() already drained the pending
            // map before enroll() inserted this entry (a narrow but real race
            // window), as well as the more common case where cancel fires during
            // the wait itself.
            let outcome = tokio::select! {
                outcome = reducer.wait(rx) => outcome,
                () = cancel.cancelled() => {
                    return blocked_outcome(
                        item_id,
                        tool_name,
                        raw_args,
                        tool_use_id,
                        "permission wait cancelled".to_owned(),
                        stop_loop,
                    );
                }
            };
            match outcome {
                Ok(PermissionOutcome::Selected { option_id }) if option_id.starts_with("allow") => {
                    // Allowed by the user — proceed to execution.
                }
                Ok(PermissionOutcome::Cancelled) | Err(_) => {
                    // B6 §2.1: distinguish a silent timeout (unresponsive
                    // client) from an operator-initiated Cancelled outcome.
                    // Both map to Deny, but a timeout must be visible in
                    // production logs so operators can tune the timeout or
                    // investigate client connectivity issues.
                    if let Err(crate::permission::ReducerError::TimedOut(dur)) = outcome {
                        tracing::warn!(
                            name: "zhive.tool.permission_timeout",
                            tool = tool_name,
                            timeout_secs = dur.as_secs(),
                            "permission request timed out; treating as deny"
                        );
                    }
                    return blocked_outcome(
                        item_id,
                        tool_name,
                        raw_args,
                        tool_use_id,
                        "permission request cancelled or timed out".to_owned(),
                        stop_loop,
                    );
                }
                Ok(PermissionOutcome::Selected { option_id }) => {
                    return blocked_outcome(
                        item_id,
                        tool_name,
                        raw_args,
                        tool_use_id,
                        format!("permission denied by user (option: {option_id})"),
                        stop_loop,
                    );
                }
                // PermissionOutcome is #[non_exhaustive]; unknown variants → deny.
                Ok(_) => {
                    return blocked_outcome(
                        item_id,
                        tool_name,
                        raw_args,
                        tool_use_id,
                        "permission denied (unrecognised outcome variant)".to_owned(),
                        stop_loop,
                    );
                }
            }
        }
        PermissionDecision::Defer => {
            // Phase-1 limitation: full suspend/resume not yet implemented.
            // Treat Defer as a blocked tool call with an explanatory notice.
            tracing::info!(
                name: "zhive.tool.defer_not_implemented",
                tool = tool_name,
                "Defer permission decision received; suspend/resume not yet implemented (Phase 1)"
            );
            return blocked_outcome(
                item_id,
                tool_name,
                raw_args,
                tool_use_id,
                "permission deferred; suspend/resume not yet implemented (Phase 1 limitation)"
                    .to_owned(),
                stop_loop,
            );
        }
        // PermissionDecision is #[non_exhaustive]; future variants are treated
        // as Deny to stay on the safe side.
        _ => {
            return blocked_outcome(
                item_id,
                tool_name,
                raw_args,
                tool_use_id,
                "permission denied (unrecognised decision variant)".to_owned(),
                stop_loop,
            );
        }
    }

    // ---- Step e: Execute ----
    let Some(tool) = tools.get(tool_name) else {
        return blocked_outcome(
            item_id,
            tool_name,
            raw_args,
            tool_use_id,
            format!("unknown tool: {tool_name}"),
            stop_loop,
        );
    };

    let thread_id = zhive_proto::domain::ThreadId(Arc::from(thread_id_str));
    let ctx = ToolContext {
        thread_id,
        turn_id: turn_id.clone(),
        cancel: cancel.clone(),
    };

    // Race the tool body against turn cancellation.  Without this, a
    // long-running tool would keep executing after `cancel_turn` fired the
    // turn token, and we would later append an ItemAppended for a result the
    // engine has already abandoned (it rolled back to Idle).  On cancel we
    // skip the remaining work (post-hook, item append) and return a Blocked
    // outcome whose item carries no orphan output.
    let exec_result = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return cancelled_during_execution(item_id, tool_name, args, tool_use_id, stop_loop);
        }
        result = tool.execute(args.clone(), &ctx) => result,
    };

    // ---- Step f: PostToolUse hook dispatch ----
    let (raw_output, succeeded, mut result_text) = match exec_result {
        Ok(out) => {
            let json_out = out
                .value
                .clone()
                .unwrap_or_else(|| serde_json::Value::String(out.text.clone()));
            (json_out, true, out.text)
        }
        Err(err) => {
            let msg = err.to_string();
            (serde_json::Value::String(msg.clone()), false, msg)
        }
    };

    // Reuse the same engine ref values obtained at the top of the function.
    // If construction fails (practically impossible — engine controls the
    // template), skip post-hook dispatch rather than panicking.
    let maybe_post_event: Result<HookEvent, _> = serde_json::from_value(serde_json::json!({
        "hook_event_name": "PostToolUse",
        "sessionId": thread_id_str,
        "cwd": cwd,
        "registeredBy": {
            "id": ext_id,
            "version": ext_ver,
            "source": "builtin"
        },
        "toolName": tool_name,
        "toolInput": args,
        "toolResponse": raw_output,
        "toolUseId": tool_use_id,
    }));

    // Race the PostToolUse dispatch against cancellation as well: a hook can
    // perform arbitrary I/O, so a cancelled turn must not wait on it before
    // appending an item that will be discarded.
    let post_dispatch = match maybe_post_event {
        Ok(post_event) => {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    return cancelled_during_execution(item_id, tool_name, args, tool_use_id, stop_loop);
                }
                outputs = hook_host.dispatch(&post_event) => outputs,
            }
        }
        Err(err) => {
            tracing::warn!(
                name: "zhive.tool.post_hook_event_build_failed",
                error = %err,
                "failed to build PostToolUse HookEvent; skipping post-hook dispatch"
            );
            Ok(vec![])
        }
    };

    if let Ok(post_outputs) = post_dispatch {
        for out in &post_outputs {
            if out.continue_loop == Some(false) {
                stop_loop = true;
            }
            if let Some(HookSpecificOutput::PostToolUse {
                updated_tool_output: Some(new_out),
                ..
            }) = &out.hook_specific_output
            {
                // Replace result text with the hook's override.
                // `Value::to_string()` JSON-encodes string values (adding
                // surrounding quotes), so unwrap `Value::String` directly;
                // other JSON variants (object, array, …) fall back to their
                // JSON serialization which is intentional.
                result_text = match new_out {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
            }
        }
    }

    // ---- Build final ToolCall item ----
    let status = if succeeded {
        ToolCallStatus::Completed
    } else {
        ToolCallStatus::Failed
    };

    let content = vec![ItemToolCallContent::Content {
        content: ItemContent::Text {
            text: result_text.clone(),
            annotations: None,
        },
    }];

    let item = Item::ToolCall {
        id: item_id,
        name: tool_name.to_owned(),
        kind: zhive_proto::domain::ToolKind::Other,
        status,
        content,
        locations: vec![],
        raw_input: Some(args),
        raw_output: Some(raw_output),
        // Carry the provider's original tool_call_id so prompt
        // reconstruction (build_call_options) can emit matching
        // tool_use / tool_result id pairs for this completed call.
        provider_tool_call_id: Some(tool_use_id.to_owned()),
    };

    DispatchOutcome::Executed { item, stop_loop }
}

// Rust guideline compliant 2026-02-21

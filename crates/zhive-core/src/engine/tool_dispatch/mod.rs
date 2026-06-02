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
//! ## Two-phase split (serial resolve, parallel execute)
//!
//! The pipeline is split into [`resolve_tool_permission`] (steps a–d) and
//! [`execute_resolved_tool`] (steps e–f). `run_turn` resolves every tool call
//! in a turn **serially** (so an interactive `Ask` / `Defer` prompt is raised
//! for at most one call at a time), then executes all approved calls
//! **concurrently**. The thin [`dispatch_tool_call`] wrapper runs both phases
//! for one call and preserves the original single-call behaviour for direct
//! callers and tests.
//!
//! ## Outcome model
//!
//! Execution returns a [`DispatchOutcome`] so `run_turn` can decide whether to
//! continue the loop (`Executed` / `Blocked`) or terminate it early (a
//! `continue_loop == false` hook output sets `stop_loop`).

mod helpers;

use std::sync::{Arc, OnceLock};

use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;
use zhive_proto::domain::{Item, ItemContent, ItemId, ItemToolCallContent, ToolCallStatus, TurnId};
use zhive_proto::hook::{ExtensionRef, HookEvent};
use zhive_proto::permission::{
    HookOutput, HookSpecificOutput, PermissionDecision, PermissionOptionKind, PermissionOutcome,
};

use crate::cancel::CancellationTree;
use crate::engine::event::EngineEvent;
use crate::engine::inner::EngineInner;
use crate::hooks::HookHost;
use crate::permission::{PermissionReducer, evaluate};
use crate::tools::{SubagentSpawner, ToolContext, ToolRegistry};

use helpers::{
    blocked_outcome, build_permission_request, cancelled_during_execution, classify_option_id,
};

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
// ToolResolution
// ============================================================

/// Outcome of the serial permission phase for one tool call.
///
/// Produced by [`resolve_tool_permission`] (the steps that must run serially
/// because they may raise an interactive permission prompt). An `Approved`
/// resolution can then be executed concurrently with other approved calls via
/// [`execute_resolved_tool`].
#[derive(Debug)]
pub(super) enum ToolResolution {
    /// The call was blocked before execution (denied, schema failure, cancel,
    /// or a build error); the carried [`DispatchOutcome`] is already final.
    Blocked(DispatchOutcome),
    /// The call is permitted; execution should proceed with `args`.
    Approved {
        /// Effective input arguments (possibly hook-mutated, post red-line-11).
        args: serde_json::Value,
        /// If `true`, a hook requested loop termination.
        stop_loop: bool,
    },
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
// Permission wait result
// ============================================================

/// Outcome of the async select! inside the `Ask` permission sub-flow.
///
/// Defined at module level so it can be named before any statements inside
/// `resolve_tool_permission_inner` (clippy `items_after_statements` lint).
enum PermResult {
    /// The reducer returned an outcome (allow / deny / cancelled / timed out).
    Outcome(Result<PermissionOutcome, crate::permission::ReducerError>),
    /// The turn cancel token fired before the reducer resolved.
    Cancelled,
}

// ============================================================
// dispatch_tool_call (thin one-call wrapper)
// ============================================================

/// Dispatches one tool call through the full hook → permission → execute
/// → post-hook pipeline.
///
/// `tool_use_id` is the provider-side stable id for this call (the value
/// that appears in the original `ToolCall` item's `raw_input` if the
/// provider emitted it, or a synthetic id otherwise).
///
/// Resolves permission then, if approved, executes — the same single-call
/// behaviour as before the two-phase split. The `zhive.tool_call` span is
/// opened by each phase ([`resolve_tool_permission`] / [`execute_resolved_tool`]).
/// The permission sub-flow additionally opens a nested `zhive.permission` span
/// when the decision is `Ask` / `Defer`.
///
/// # Errors
///
/// This function does not return a `Result`; all failure modes are folded
/// into the returned [`DispatchOutcome`] so the caller can log and continue
/// without early-return complexity.
#[allow(
    clippy::too_many_arguments,
    reason = "All args are required context; no good grouping"
)]
#[expect(
    dead_code,
    reason = "thin single-call wrapper retained for direct callers/tests; \
              run_turn now uses the two-phase resolve/execute split directly"
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
    spawner: Option<Arc<dyn SubagentSpawner>>,
) -> DispatchOutcome {
    match resolve_tool_permission(
        inner,
        hook_host,
        reducer,
        thread_id_str,
        turn_id,
        item_id.clone(),
        tool_name,
        raw_args,
        tool_use_id,
        scope,
        cancel,
    )
    .await
    {
        ToolResolution::Blocked(outcome) => outcome,
        ToolResolution::Approved { args, stop_loop } => {
            execute_resolved_tool(
                tools,
                hook_host,
                thread_id_str,
                turn_id,
                item_id,
                tool_name,
                args,
                tool_use_id,
                cancel,
                stop_loop,
                spawner,
            )
            .await
        }
    }
}

// ============================================================
// resolve_tool_permission (serial phase, steps a–d)
// ============================================================

/// Span-wrapping entry to the serial permission phase (steps a–d).
///
/// Opens the `zhive.tool_call` span and delegates to
/// [`resolve_tool_permission_inner`]. `run_turn` calls this once per tool call
/// in the model-emit order so an interactive `Ask` / `Defer` prompt is raised
/// for at most one call at a time.
#[allow(
    clippy::too_many_arguments,
    reason = "All args are required context; no good grouping"
)]
pub(super) async fn resolve_tool_permission(
    inner: &Arc<EngineInner>,
    hook_host: &Arc<HookHost>,
    reducer: &PermissionReducer,
    thread_id_str: &str,
    turn_id: &TurnId,
    item_id: ItemId,
    tool_name: &str,
    raw_args: serde_json::Value,
    tool_use_id: &str,
    scope: &zhive_proto::permission::PermissionScope,
    cancel: &CancellationToken,
) -> ToolResolution {
    // Span name and field names are string literals (tracing macro
    // requirement).  The constants spans::TOOL_CALL, fields::THREAD_ID,
    // fields::TURN_ID, and fields::TOOL_NAME are the single source of
    // truth; observability tests assert the literals match.
    let span = tracing::info_span!(
        "zhive.tool_call",
        "session.id"       = thread_id_str,
        "zhive.turn.id"    = %turn_id.0,
        "gen_ai.tool.name" = tool_name,
    );
    resolve_tool_permission_inner(
        inner,
        hook_host,
        reducer,
        thread_id_str,
        item_id,
        tool_name,
        raw_args,
        tool_use_id,
        scope,
        cancel,
    )
    .instrument(span)
    .await
}

/// Inner body of [`resolve_tool_permission`], instrumented by the caller
/// with the `zhive.tool_call` span.
///
/// Runs steps a–d: `PreToolUse` hook dispatch, hook output folding, red-line-11
/// revalidation of any `updated_input`, and permission evaluation (including
/// the `Ask` / `Defer` reverse-RPC enroll → emit → await flow raced against
/// turn cancellation). Returns [`ToolResolution::Blocked`] (carrying an
/// already-final [`DispatchOutcome`]) when the call is denied, fails
/// revalidation, or is cancelled mid-wait; otherwise
/// [`ToolResolution::Approved`].
#[expect(
    clippy::too_many_lines,
    reason = "resolve_tool_permission_inner spans the hook + permission steps as one \
              conceptual unit; extracting sub-functions would hurt readability without \
              reducing complexity"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "All args are required context; no good grouping"
)]
async fn resolve_tool_permission_inner(
    inner: &Arc<EngineInner>,
    hook_host: &Arc<HookHost>,
    reducer: &PermissionReducer,
    thread_id_str: &str,
    item_id: ItemId,
    tool_name: &str,
    raw_args: serde_json::Value,
    tool_use_id: &str,
    scope: &zhive_proto::permission::PermissionScope,
    cancel: &CancellationToken,
) -> ToolResolution {
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
            return ToolResolution::Blocked(blocked_outcome(
                item_id,
                tool_name,
                raw_args,
                tool_use_id,
                format!("failed to build PreToolUse hook event: {err}"),
                false,
            ));
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
            return ToolResolution::Blocked(blocked_outcome(
                item_id,
                tool_name,
                raw_args,
                tool_use_id,
                format!("PreToolUse hook error: {err}"),
                false,
            ));
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
                return ToolResolution::Blocked(blocked_outcome(
                    item_id,
                    tool_name,
                    raw_args,
                    tool_use_id,
                    format!("schema re-validation failed: {err}"),
                    stop_loop,
                ));
            }
        }
    } else {
        raw_args.clone()
    };

    // ---- Step d: Permission evaluation ----
    let decision = evaluate(scope, &decisions);
    match decision {
        PermissionDecision::Allow => {
            // Continue to the approval below.
        }
        PermissionDecision::Deny => {
            return ToolResolution::Blocked(blocked_outcome(
                item_id,
                tool_name,
                raw_args,
                tool_use_id,
                "permission denied".to_owned(),
                stop_loop,
            ));
        }
        PermissionDecision::Ask | PermissionDecision::Defer => {
            // `Ask` and `Defer` share the same reverse-RPC flow (enroll → emit
            // PermissionRequested → await the client's decision). They differ
            // only in the wait: `Ask` is bounded by the reducer timeout (a
            // silent client → Deny), while `Defer` suspends the turn
            // indefinitely (B6 §4) until `resume_permission` arrives. The live
            // pending-permission entry IS the materialised "suspended turn";
            // `cancel_turn` drains it via `cancel_all` (→ Cancelled).
            let is_defer = matches!(decision, PermissionDecision::Defer);

            // Allow-always short-circuit (Ask only). If the user previously
            // approved this exact tool name with `AllowAlways`, downgrade the
            // `Ask` to `Allow` without raising another prompt. Scoped strictly
            // to the `Ask` path: `Defer` carries an explicit "suspend the turn"
            // semantic that a prior allow-always must not silently bypass, and
            // a folded `Deny` never reaches this arm — so allow-always can only
            // relax a call that would otherwise have prompted, never one that
            // was denied.
            if !is_defer && reducer.is_tool_allow_always(tool_name) {
                tracing::debug!(
                    name: "zhive.permission.allow_always_hit",
                    tool = tool_name,
                    decision = "allow",
                    "allow-always recorded for tool; skipping prompt"
                );
                return ToolResolution::Approved { args, stop_loop };
            }

            // Open a `zhive.permission` span for the interactive sub-flow:
            // enroll → emit PermissionRequested → await user decision.
            //
            // Span name is a literal; spans::PERMISSION / fields::TOOL_NAME are
            // the single source of truth (see observability tests).
            // The `Span` handle is `Send`; only `EnteredSpan` guards are not.
            // We use `perm_span.in_scope(...)` for sync work and
            // `.instrument(perm_span)` on the awaited future so the span
            // is correctly entered across yield points without holding a
            // non-Send guard across any `.await`.
            let perm_span = tracing::info_span!(
                "zhive.permission",
                "session.id" = thread_id_str,
                "gen_ai.tool.name" = tool_name,
            );

            // Build reverse-RPC request inside the span context (sync).
            let request = perm_span.in_scope(|| build_permission_request(thread_id_str, tool_name));
            let request = match request {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!(
                        name: "zhive.tool.permission_request_build_failed",
                        tool = tool_name,
                        error = %err,
                        "failed to build RequestPermissionRequest; treating as Deny"
                    );
                    return ToolResolution::Blocked(blocked_outcome(
                        item_id,
                        tool_name,
                        raw_args,
                        tool_use_id,
                        format!("failed to build permission request: {err}"),
                        stop_loop,
                    ));
                }
            };
            // Retain the advertised options so the selected `option_id` can be
            // classified structurally (by `PermissionOptionKind`) once the
            // outcome arrives. `enroll` consumes and returns the request, so
            // clone the small option vec up front rather than reaching into the
            // boxed event copy afterwards.
            let options = request.options.clone();
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
            //
            if is_defer {
                perm_span.in_scope(|| {
                    tracing::info!(
                        name: "zhive.permission.deferred",
                        tool = tool_name,
                        "permission deferred; suspending turn until resume_permission (unbounded wait)"
                    );
                });
            }

            // The async block is instrumented with perm_span so the span
            // context flows across every await point. `Defer` waits without a
            // timeout; `Ask` applies the reducer's bounded wait.
            let perm_result = async {
                tokio::select! {
                    outcome = async {
                        if is_defer {
                            reducer.wait_unbounded(rx).await
                        } else {
                            reducer.wait(rx).await
                        }
                    } => PermResult::Outcome(outcome),
                    () = cancel.cancelled() => PermResult::Cancelled,
                }
            }
            .instrument(perm_span.clone())
            .await;

            let outcome = match perm_result {
                PermResult::Outcome(o) => o,
                PermResult::Cancelled => {
                    return ToolResolution::Blocked(blocked_outcome(
                        item_id,
                        tool_name,
                        raw_args,
                        tool_use_id,
                        "permission wait cancelled".to_owned(),
                        stop_loop,
                    ));
                }
            };

            match outcome {
                Ok(PermissionOutcome::Selected { option_id }) => {
                    // Classify the selected option structurally by its
                    // `PermissionOptionKind` (never by string prefix): look it
                    // up in the advertised options, falling back to the
                    // well-known stable ids. Allow kinds (`AllowOnce` /
                    // `AllowAlways`) proceed; reject kinds and unrecognised ids
                    // deny. `AllowAlways` additionally records the tool name so
                    // future `Ask`s for it are auto-allowed.
                    match classify_option_id(&options, &option_id) {
                        Some(PermissionOptionKind::AllowOnce) => {
                            perm_span.in_scope(|| {
                                tracing::debug!(
                                    name: "zhive.permission.allowed",
                                    decision = "allow",
                                    "permission granted (allow once)"
                                );
                            });
                        }
                        Some(PermissionOptionKind::AllowAlways) => {
                            reducer.record_allow_always(tool_name);
                            perm_span.in_scope(|| {
                                tracing::debug!(
                                    name: "zhive.permission.allow_always_recorded",
                                    tool = tool_name,
                                    decision = "allow",
                                    "permission granted and recorded as allow-always"
                                );
                            });
                        }
                        Some(
                            PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways,
                        ) => {
                            return ToolResolution::Blocked(blocked_outcome(
                                item_id,
                                tool_name,
                                raw_args,
                                tool_use_id,
                                format!("permission denied by user (option: {option_id})"),
                                stop_loop,
                            ));
                        }
                        // Either an unrecognised option id (`None`) or a future
                        // `PermissionOptionKind` variant (`#[non_exhaustive]`).
                        // Deny conservatively: an unknown choice is never an
                        // implicit allow.
                        None | Some(_) => {
                            return ToolResolution::Blocked(blocked_outcome(
                                item_id,
                                tool_name,
                                raw_args,
                                tool_use_id,
                                format!("permission denied (unrecognised option: {option_id})"),
                                stop_loop,
                            ));
                        }
                    }
                }
                Ok(PermissionOutcome::Cancelled) | Err(_) => {
                    // B6 §2.1: distinguish a silent timeout (unresponsive
                    // client) from an operator-initiated Cancelled outcome.
                    // Both map to Deny, but a timeout must be visible in
                    // production logs so operators can tune the timeout or
                    // investigate client connectivity issues.
                    // On the Defer (unbounded) path a `TimedOut` never fires;
                    // the only error is `Abandoned` (sender dropped, e.g. engine
                    // shutdown). Surface it so a shutdown-with-suspended-Defer is
                    // observable, mirroring the timeout warning.
                    if let Err(crate::permission::ReducerError::Abandoned) = outcome {
                        perm_span.in_scope(|| {
                            tracing::warn!(
                                name: "zhive.tool.permission_abandoned",
                                tool = tool_name,
                                decision = "deny",
                                "permission waiter abandoned (sender dropped); treating as deny"
                            );
                        });
                    }
                    if let Err(crate::permission::ReducerError::TimedOut(dur)) = outcome {
                        perm_span.in_scope(|| {
                            tracing::warn!(
                                name: "zhive.tool.permission_timeout",
                                tool = tool_name,
                                timeout_secs = dur.as_secs(),
                                decision = "deny",
                                "permission request timed out; treating as deny"
                            );
                        });
                    }
                    return ToolResolution::Blocked(blocked_outcome(
                        item_id,
                        tool_name,
                        raw_args,
                        tool_use_id,
                        "permission request cancelled or timed out".to_owned(),
                        stop_loop,
                    ));
                }
                // PermissionOutcome is #[non_exhaustive]; unknown variants → deny.
                Ok(_) => {
                    return ToolResolution::Blocked(blocked_outcome(
                        item_id,
                        tool_name,
                        raw_args,
                        tool_use_id,
                        "permission denied (unrecognised decision variant)".to_owned(),
                        stop_loop,
                    ));
                }
            }
        }
        // PermissionDecision is #[non_exhaustive]; future variants are treated
        // as Deny to stay on the safe side.
        _ => {
            return ToolResolution::Blocked(blocked_outcome(
                item_id,
                tool_name,
                raw_args,
                tool_use_id,
                "permission denied (unrecognised decision variant)".to_owned(),
                stop_loop,
            ));
        }
    }

    // Permission granted; hand the effective args off to the execute phase.
    ToolResolution::Approved { args, stop_loop }
}

// ============================================================
// execute_resolved_tool (parallel-safe phase, steps e–f)
// ============================================================

/// Span-wrapping entry to the parallel-safe execute phase (steps e–f).
///
/// Opens the `zhive.tool_call` span and delegates to
/// [`execute_resolved_tool_inner`]. Safe to run concurrently for distinct tool
/// calls; `run_turn` joins many of these in its parallel phase.
#[allow(
    clippy::too_many_arguments,
    reason = "All args are required context; no good grouping"
)]
pub(super) async fn execute_resolved_tool(
    tools: &Arc<ToolRegistry>,
    hook_host: &Arc<HookHost>,
    thread_id_str: &str,
    turn_id: &TurnId,
    item_id: ItemId,
    tool_name: &str,
    args: serde_json::Value,
    tool_use_id: &str,
    cancel: &CancellationToken,
    stop_loop: bool,
    spawner: Option<Arc<dyn SubagentSpawner>>,
) -> DispatchOutcome {
    let span = tracing::info_span!(
        "zhive.tool_call",
        "session.id"       = thread_id_str,
        "zhive.turn.id"    = %turn_id.0,
        "gen_ai.tool.name" = tool_name,
    );
    execute_resolved_tool_inner(
        tools,
        hook_host,
        thread_id_str,
        turn_id,
        item_id,
        tool_name,
        args,
        tool_use_id,
        cancel,
        stop_loop,
        spawner,
    )
    .instrument(span)
    .await
}

/// Inner body of [`execute_resolved_tool`], instrumented by the caller with
/// the `zhive.tool_call` span.
///
/// Runs steps e–f: tool lookup + `execute` raced against the turn cancel token,
/// then `PostToolUse` hook dispatch (also raced against cancel), and finally
/// builds the finalized `Item::ToolCall` carrying the `provider_tool_call_id`.
/// Borrows only shared/immutable references and derives its own per-call child
/// cancel token, so multiple invocations for distinct tool calls may run
/// concurrently.
///
/// `stop_loop` is the flag computed during [`resolve_tool_permission_inner`];
/// it may be promoted to `true` by a `PostToolUse` hook returning
/// `continue_loop == false`.
#[expect(
    clippy::too_many_lines,
    reason = "execute_resolved_tool_inner spans the execute + post-hook steps as one \
              conceptual unit; extracting sub-functions would hurt readability without \
              reducing complexity"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "All args are required context; no good grouping"
)]
async fn execute_resolved_tool_inner(
    tools: &Arc<ToolRegistry>,
    hook_host: &Arc<HookHost>,
    thread_id_str: &str,
    turn_id: &TurnId,
    item_id: ItemId,
    tool_name: &str,
    args: serde_json::Value,
    tool_use_id: &str,
    cancel: &CancellationToken,
    mut stop_loop: bool,
    spawner: Option<Arc<dyn SubagentSpawner>>,
) -> DispatchOutcome {
    // Re-derive the engine provenance + cwd locally so this function owns no
    // state from the resolve phase (it must be safe to run concurrently).
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("/"))
        .to_string_lossy()
        .into_owned();
    let ext_ref = engine_ext_ref();
    let ext_id = ext_ref.as_ref().map_or("zhive.engine", |r| r.id.as_str());
    let ext_ver = ext_ref
        .as_ref()
        .map_or(env!("CARGO_PKG_VERSION"), |r| r.version.as_str());

    // ---- Step e: Execute ----
    let Some(tool) = tools.get(tool_name) else {
        return blocked_outcome(
            item_id,
            tool_name,
            args,
            tool_use_id,
            format!("unknown tool: {tool_name}"),
            stop_loop,
        );
    };

    let thread_id = zhive_proto::domain::ThreadId(Arc::from(thread_id_str));
    // Each tool call gets its own child token so the tool scope sits below
    // the turn in the hierarchy.  Firing the turn token propagates to the
    // tool child (cancelling the tool); firing only the tool child (if we
    // were to add per-tool cancellation later) does not affect the turn.
    // The existing `select!` on `cancel` (the turn token) continues to work
    // because the turn token is the child's parent.
    let tool_cancel = CancellationTree::child_for_tool(cancel);
    let ctx = ToolContext {
        thread_id,
        turn_id: turn_id.clone(),
        cancel: tool_cancel.clone(),
        spawner,
    };

    // Race the tool body against turn cancellation.  Without this, a
    // long-running tool would keep executing after `cancel_turn` fired the
    // turn token, and we would later append an ItemAppended for a result the
    // engine has already abandoned (it rolled back to Idle).  On cancel we
    // skip the remaining work (post-hook, item append) and return a Blocked
    // outcome whose item carries no orphan output.
    //
    // We race against `cancel` (the turn token) rather than `tool_cancel`
    // (the child) so that firing the turn also aborts the tool select! arm.
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
        // Propagate the tool's self-declared kind (Read / Edit / Execute) so
        // UI grouping and permission policy can distinguish, e.g., a read from
        // a shell execution. `tool` is the resolved registry entry above.
        kind: tool.kind().into(),
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

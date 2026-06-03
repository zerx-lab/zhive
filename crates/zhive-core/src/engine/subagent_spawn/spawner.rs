//! Parent-side subagent spawner: the per-tool-call permission handshake.
//!
//! [`EngineSubagentSpawner`] bridges the model-callable `agent` tool to
//! [`EngineInner`] subagent spawns. Its [`crate::tools::SubagentSpawner`] impl
//! does more than await the child's final message: it runs a `select` loop
//! over **both** in-process channels returned by
//! [`EngineInner::spawn_subagent_awaitable`]:
//!
//! 1. `final_rx` — the child's terminal [`SubagentFinalEvent`]
//!    (`Completed` / `Errored` / `Suspended`).
//! 2. `decision_rx` — per-tool-call [`SubagentDecisionRequest`]s. For each one
//!    the parent runs a **second fold** ([`EngineSubagentSpawner::parent_second_fold`])
//!    of its own `PreToolUse` hooks plus the child's decision, optionally
//!    raising its own reverse-RPC (`Ask` / `Defer`), and replies with a
//!    terminal [`ParentVerdict`].
//!
//! ## Deadlock safety (critical invariant)
//!
//! The whole select loop runs inside the **parent turn task** (the `agent`
//! tool's `execute` frame). When `parent_second_fold` raises a reverse-RPC and
//! waits on the reducer, the resolve is delivered by the engine **actor loop**
//! (`EngineInner::resume_permission`, a *different* task) — never by this
//! parked turn task. That separation is what makes two-layer suspend/resume
//! deadlock-free: the parent turn task can block on the reducer because someone
//! else discharges it. Routing resolve back through the turn task would
//! deadlock instantly; do not do that.
//!
//! Mirrors codex `codex_delegate.rs`: a child agent runs its own loop, the
//! parent intercepts every approval, decides (optionally asking the user), and
//! sends the verdict back to the parked child tool call.

use std::sync::Arc;

use zhive_proto::domain::{ThreadId, TurnId};
use zhive_proto::hook::{ExtensionRef, HookEvent};
use zhive_proto::permission::{
    HookOutput, HookSpecificOutput, PermissionDecision, PermissionOption, PermissionOptionKind,
    PermissionOutcome, PermissionScope, RequestPermissionRequest, SubagentDefinition,
};

use crate::engine::event::EngineEvent;
use crate::engine::inner::EngineInner;
use crate::permission::{RequestContext, evaluate};
use crate::subagent::{ParentVerdict, SubagentDecisionRequest, SubagentFinalEvent};

/// Stable option ids advertised by the parent's reverse-RPC for a child call.
///
/// Mirrors the ids used by the top-level dispatch path so a client UI renders
/// the same choices regardless of whether the parent is asking on its own
/// behalf or on behalf of a child.
const OPT_ALLOW_ONCE: &str = "allow-once";
const OPT_ALLOW_ALWAYS: &str = "allow-always";
const OPT_REJECT_ONCE: &str = "reject-once";
const OPT_REJECT_ALWAYS: &str = "reject-always";

/// Bridges the model-callable `agent` tool to [`EngineInner`] subagent spawns.
///
/// Holds a handle to the engine and the parent thread id so the tool can
/// delegate a sub-task to a child agent and `await` its final message while
/// supervising every child tool call. One instance is created per tool
/// execution in the dispatch loop; cloning the `Arc<EngineInner>` is cheap
/// (shared-ownership handle).
pub(crate) struct EngineSubagentSpawner {
    /// Shared engine handle used to drive the child turn.
    inner: Arc<EngineInner>,
    /// Thread that owns the tool call requesting the spawn.
    parent_thread_id: ThreadId,
}

// `EngineInner` does not implement `Debug` (it holds non-Debug runtime
// handles), so derive cannot be used. The `SubagentSpawner` trait requires
// `Debug`; render only the parent thread id, which is the useful identifier.
impl std::fmt::Debug for EngineSubagentSpawner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineSubagentSpawner")
            .field("parent_thread_id", &self.parent_thread_id)
            .finish_non_exhaustive()
    }
}

impl EngineSubagentSpawner {
    /// Builds a spawner bound to `inner` and `parent_thread_id`.
    pub(crate) fn new(inner: Arc<EngineInner>, parent_thread_id: ThreadId) -> Self {
        Self {
            inner,
            parent_thread_id,
        }
    }

    /// Reads the parent's live turn scope and turn id.
    ///
    /// Returns `(scope, Some(turn_id))` when the parent has an active turn, or
    /// the default scope and `None` when it is idle (e.g. a unit test without a
    /// live turn). The turn id is needed to attribute the parent's reverse-RPC
    /// suspend/resume notifications.
    async fn parent_turn_context(&self) -> (PermissionScope, Option<TurnId>) {
        match self.inner.threads().get(&self.parent_thread_id).await {
            Some(handle) => {
                let guard = handle.active_turn.lock().await;
                guard.as_ref().map_or_else(
                    || (PermissionScope::default_turn_scope(), None),
                    |a| (a.scope.clone(), Some(a.id.clone())),
                )
            }
            None => (PermissionScope::default_turn_scope(), None),
        }
    }

    /// Runs the parent's second permission fold for one child tool call.
    ///
    /// Dispatches the parent's own `PreToolUse` hooks (with the child's tool
    /// context), appends the child's decision, and evaluates against the
    /// parent's scope. The result is monotone: the parent can only equal or
    /// tighten the child decision, never widen it. `Ask` / `Defer` are resolved
    /// inline via the parent's reverse-RPC so the returned [`ParentVerdict`] is
    /// always terminal (`Allow` / `Deny`).
    ///
    /// Any failure to build / dispatch the parent hooks degrades to `Deny`
    /// (never widen on error).
    async fn parent_second_fold(&self, req: &SubagentDecisionRequest) -> ParentVerdict {
        let (parent_scope, parent_turn_id) = self.parent_turn_context().await;

        // Dispatch the parent's PreToolUse hooks with the child's tool context.
        let Ok(mut decisions) = self
            .dispatch_parent_pre_hooks(&req.tool_name, &req.raw_args)
            .await
        else {
            return ParentVerdict::Deny;
        };
        // Append the child's own decision as the final vote (A3 §7.4).
        decisions.push(req.child_decision);

        // `Ask` / `Defer` differ only in whether the parent reverse-RPC waits
        // unbounded (and emits a suspend); both still terminate as Allow / Deny.
        match evaluate(&parent_scope, &decisions) {
            PermissionDecision::Allow => ParentVerdict::Allow,
            PermissionDecision::Ask => {
                self.parent_reverse_rpc(&req.tool_name, parent_turn_id, false)
                    .await
            }
            PermissionDecision::Defer => {
                self.parent_reverse_rpc(&req.tool_name, parent_turn_id, true)
                    .await
            }
            // `Deny` and any future `#[non_exhaustive]` variant deny.
            _ => ParentVerdict::Deny,
        }
    }

    /// Dispatches the parent's `PreToolUse` hooks and folds them to decisions.
    ///
    /// Returns the per-hook [`PermissionDecision`] votes, or `Err(())` if the
    /// hook event cannot be built or the host dispatch fails (the caller then
    /// denies — never widen on error).
    async fn dispatch_parent_pre_hooks(
        &self,
        tool_name: &str,
        raw_args: &serde_json::Value,
    ) -> Result<Vec<PermissionDecision>, ()> {
        let Some(event) = build_pre_tool_use_event(&self.parent_thread_id.0, tool_name, raw_args)
        else {
            tracing::warn!(
                name: "zhive.subagent.parent_fold.hook_event_build_failed",
                tool = tool_name,
                "failed to build parent PreToolUse hook event; denying child call"
            );
            return Err(());
        };
        let outputs: Vec<HookOutput> = match self.inner.hook_host().dispatch(&event).await {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(
                    name: "zhive.subagent.parent_fold.hook_dispatch_failed",
                    tool = tool_name,
                    error = %err,
                    "parent PreToolUse hook dispatch failed; denying child call"
                );
                return Err(());
            }
        };
        let mut decisions = Vec::new();
        for out in &outputs {
            if let Some(HookSpecificOutput::PreToolUse {
                permission_decision,
                ..
            }) = &out.hook_specific_output
            {
                decisions.push(*permission_decision);
            }
        }
        Ok(decisions)
    }

    /// Raises the parent's reverse-RPC for a child tool call and waits.
    ///
    /// Enrolls a permission request, emits `PermissionRequested` on the parent
    /// thread (the client cannot tell the parent is asking on a child's
    /// behalf — the `request_id` is globally unique), then waits. When `defer`
    /// is set the wait is unbounded and a `TurnSuspended` is emitted first; the
    /// resolve (and the matching `TurnResumed`) is driven by the engine actor
    /// loop, never this task — the deadlock guard.
    ///
    /// The terminal outcome is mapped to a [`ParentVerdict`]; any cancel /
    /// timeout / reject / unknown option degrades to `Deny`.
    async fn parent_reverse_rpc(
        &self,
        tool_name: &str,
        parent_turn_id: Option<TurnId>,
        defer: bool,
    ) -> ParentVerdict {
        let reducer = self.inner.permission_reducer();
        let Some(request) = build_permission_request(&self.parent_thread_id.0, tool_name) else {
            tracing::warn!(
                name: "zhive.subagent.parent_fold.request_build_failed",
                tool = tool_name,
                "failed to build parent permission request; denying child call"
            );
            return ParentVerdict::Deny;
        };
        let options = request.options.clone();

        // Record turn context on the `Defer` path so `resume_permission` can
        // emit a `TurnResumed` for the parent turn when the request resolves.
        let (key, req, rx) = match (defer, parent_turn_id.clone()) {
            (true, Some(turn_id)) => reducer.enroll_with_context(
                request,
                RequestContext {
                    thread_id: self.parent_thread_id.clone(),
                    turn_id,
                },
            ),
            _ => reducer.enroll(request),
        };

        let wire_id = key.to_wire();
        // Clone `req` before moving it into the broadcast event so the Defer
        // path can persist the full request payload below (B6).
        let req_for_persist = if defer { Some(req.clone()) } else { None };
        let _ = self
            .inner
            .events_tx()
            .send(EngineEvent::PermissionRequested {
                request_id: wire_id.clone(),
                request: Box::new(req),
            });

        if defer && let Some(ref turn_id) = parent_turn_id {
            let _ = self.inner.events_tx().send(EngineEvent::TurnSuspended {
                thread_id: self.parent_thread_id.clone(),
                turn_id: turn_id.clone(),
                request_id: wire_id.clone(),
                reason: None,
            });
            // B6: persist the pending permission request (subagent Defer path).
            if let Some(persist_req) = req_for_persist {
                self.inner.enqueue_storage_op(
                    crate::persistence::writer::StorageWriteOp::PermissionSuspended {
                        thread_id: self.parent_thread_id.clone(),
                        turn_id: turn_id.clone(),
                        timestamp: crate::engine::lifecycle::unix_now_pub(),
                        request_id: wire_id.0.to_string(),
                        request: Box::new(persist_req),
                    },
                );
            }
        }

        let outcome = if defer {
            reducer.wait_unbounded(rx).await
        } else {
            reducer.wait(rx).await
        };

        // B6: on Defer path, persist a PermissionResolved entry so resume does
        // not re-surface a prompt that was already answered.
        if defer {
            self.inner.enqueue_storage_op(
                crate::persistence::writer::StorageWriteOp::PermissionResolved {
                    thread_id: self.parent_thread_id.clone(),
                    request_id: wire_id.0.to_string(),
                    timestamp: crate::engine::lifecycle::unix_now_pub(),
                },
            );
        }

        match outcome {
            Ok(PermissionOutcome::Selected { option_id }) => {
                match classify_option(&options, &option_id) {
                    Some(PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways) => {
                        ParentVerdict::Allow
                    }
                    // Reject kinds, unknown ids, and future variants all deny.
                    _ => ParentVerdict::Deny,
                }
            }
            // Cancelled / timeout / abandoned / unknown variant → deny.
            _ => ParentVerdict::Deny,
        }
    }
}

#[async_trait::async_trait]
impl crate::tools::SubagentSpawner for EngineSubagentSpawner {
    async fn spawn_and_await(
        &self,
        name: String,
        description: String,
        prompt: String,
    ) -> Result<String, String> {
        // Inherit the parent's tool allowlist and permission mode: `tools`,
        // `disallowed_tools`, and `permission_mode` are left at their defaults
        // so `prepare_child_scope` resolves them against the parent scope.
        let definition: SubagentDefinition = serde_json::from_value(serde_json::json!({
            "name": name,
            "description": description,
            "prompt": prompt,
        }))
        .map_err(|err| format!("failed to build subagent definition: {err}"))?;

        let (_child_thread_id, mut final_rx, mut decision_rx) = self
            .inner
            .spawn_subagent_awaitable(self.parent_thread_id.clone(), definition)
            .await
            .map_err(|err| err.to_string())?;

        // Drive both channels until the child reaches a terminal state. The
        // `biased` ordering polls the decision channel first so a pending
        // handshake is always serviced before a final event is consumed (the
        // child cannot emit a final while a decision is still parked, but the
        // bias keeps the intent explicit).
        loop {
            tokio::select! {
                biased;
                Some(req) = decision_rx.recv() => {
                    let verdict = self.parent_second_fold(&req).await;
                    // The child may have gone away (cancelled); ignore send err.
                    let _ = req.reply.send(verdict);
                }
                final_event = final_rx.recv() => {
                    return match final_event {
                        Some(SubagentFinalEvent::Completed { final_message, .. }) => {
                            Ok(final_message_text(final_message.as_deref()))
                        }
                        Some(SubagentFinalEvent::Errored { error, .. }) => Err(error.message),
                        Some(SubagentFinalEvent::Suspended { child_thread_id, child_request_id }) => {
                            // Defensive fallback: the current full-handshake
                            // architecture never constructs `Suspended` (a
                            // deferring child is resolved by the parent's second
                            // fold inline; see SubagentFinalEvent::Suspended). If
                            // a future path does emit it, warn so the unexpected
                            // route is visible, then forward it as a parent
                            // TurnSuspended and keep looping: the child resumes
                            // via the shared reducer (request id globally unique)
                            // and emits a final event once it continues.
                            tracing::warn!(
                                name: "zhive.subagent.spawn.unexpected_suspended",
                                child_thread_id = %child_thread_id.0,
                                child_request_id = %child_request_id,
                                "subagent emitted a Suspended event, which the current architecture does not construct; forwarding as a parent TurnSuspended"
                            );
                            self.forward_child_suspended(&child_request_id).await;
                            continue;
                        }
                        // Sender dropped only on child task panic / engine
                        // shutdown before delivering the outcome.
                        None => Err("subagent channel closed without result".to_owned()),
                    };
                }
                // Both channels closed: the child went away without a final.
                else => return Err("subagent channels closed without result".to_owned()),
            }
        }
    }
}

impl EngineSubagentSpawner {
    /// Forwards a child-thread suspension to the parent as a `TurnSuspended`.
    ///
    /// The parent turn does not itself defer, but it suspends *waiting on the
    /// suspended child*. Subscribers see a parent `TurnSuspended` carrying the
    /// child's globally unique `request_id`; resuming that id resumes the child.
    async fn forward_child_suspended(&self, child_request_id: &str) {
        let (_scope, parent_turn_id) = self.parent_turn_context().await;
        if let Some(turn_id) = parent_turn_id {
            let _ = self.inner.events_tx().send(EngineEvent::TurnSuspended {
                thread_id: self.parent_thread_id.clone(),
                turn_id,
                request_id: crate::engine::submission::PermissionRequestId(Arc::from(
                    child_request_id,
                )),
                reason: None,
            });
        }
    }
}

/// Extracts the displayable text of a subagent's final message.
///
/// The final item is the child's last [`zhive_proto::domain::Item::AgentMessage`]
/// (or `Item::SystemNotice` fallback). Returns an empty string when there is no
/// final message or it carries no text, matching the Claude Code contract that
/// an empty subagent result is delivered verbatim rather than as an error.
fn final_message_text(final_message: Option<&zhive_proto::domain::Item>) -> String {
    match final_message {
        Some(zhive_proto::domain::Item::AgentMessage { text, .. }) => text.clone(),
        Some(zhive_proto::domain::Item::SystemNotice { message, .. }) => message.clone(),
        _ => String::new(),
    }
}

/// Builds a synthetic engine-internal [`ExtensionRef`] for parent hook events.
///
/// Returns `None` only if the engine-controlled JSON template fails to
/// deserialize, which cannot happen in practice; callers then deny the call.
fn engine_ext_ref() -> Option<ExtensionRef> {
    let version = env!("CARGO_PKG_VERSION");
    serde_json::from_value(serde_json::json!({
        "id": "zhive.engine",
        "version": version,
        "source": "builtin"
    }))
    .ok()
}

/// Builds the parent's `PreToolUse` [`HookEvent`] for a child tool call.
///
/// Carries the child's `tool_name` and `raw_args` so the parent's hooks see
/// the same context they would for a native tool call. Returns `None` if the
/// engine-controlled JSON template fails to deserialize.
fn build_pre_tool_use_event(
    thread_id_str: &str,
    tool_name: &str,
    raw_args: &serde_json::Value,
) -> Option<HookEvent> {
    let ext = engine_ext_ref();
    let ext_id = ext.as_ref().map_or("zhive.engine", |r| r.id.as_str());
    let ext_ver = ext
        .as_ref()
        .map_or(env!("CARGO_PKG_VERSION"), |r| r.version.as_str());
    serde_json::from_value(serde_json::json!({
        "hook_event_name": "PreToolUse",
        "sessionId": thread_id_str,
        "cwd": ".",
        "registeredBy": { "id": ext_id, "version": ext_ver, "source": "builtin" },
        "toolName": tool_name,
        "toolInput": raw_args,
        "toolUseId": "subagent-parent-fold",
    }))
    .ok()
}

/// Builds the parent's reverse-RPC [`RequestPermissionRequest`] for a child call.
///
/// Advertises the four standard option kinds. Returns `None` if the
/// engine-controlled JSON template fails to deserialize.
fn build_permission_request(
    thread_id_str: &str,
    tool_name: &str,
) -> Option<RequestPermissionRequest> {
    serde_json::from_value(serde_json::json!({
        "threadId": thread_id_str,
        "resourceType": "tool",
        "name": tool_name,
        "reason": format!("subagent wants to call tool: {tool_name}"),
        "options": [
            { "id": OPT_ALLOW_ONCE, "kind": "AllowOnce", "description": "Allow once" },
            { "id": OPT_ALLOW_ALWAYS, "kind": "AllowAlways", "description": "Always allow" },
            { "id": OPT_REJECT_ONCE, "kind": "RejectOnce", "description": "Reject once" },
            { "id": OPT_REJECT_ALWAYS, "kind": "RejectAlways", "description": "Always reject" }
        ]
    }))
    .ok()
}

/// Classifies a selected option id into its [`PermissionOptionKind`].
///
/// Prefers the advertised option's kind, then falls back to the well-known
/// stable ids (hyphen and underscore forms). Returns `None` for an unrecognised
/// id so the caller denies conservatively.
fn classify_option(options: &[PermissionOption], option_id: &str) -> Option<PermissionOptionKind> {
    if let Some(opt) = options.iter().find(|o| o.id == option_id) {
        return Some(opt.kind);
    }
    match option_id {
        OPT_ALLOW_ONCE | "allow_once" => Some(PermissionOptionKind::AllowOnce),
        OPT_ALLOW_ALWAYS | "allow_always" => Some(PermissionOptionKind::AllowAlways),
        OPT_REJECT_ONCE | "reject_once" => Some(PermissionOptionKind::RejectOnce),
        OPT_REJECT_ALWAYS | "reject_always" => Some(PermissionOptionKind::RejectAlways),
        _ => None,
    }
}

// Rust guideline compliant 2026-02-21

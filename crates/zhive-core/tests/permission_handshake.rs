//! Integration tests for the full permission handshake (Phase 1, B6/B8-O6).
//!
//! Covers three behaviours end to end through a live [`Engine`]:
//!
//! 1. A top-level turn whose tool call defers emits `TurnSuspended`, parks on
//!    the reducer, and resumes (emitting `TurnResumed`) after
//!    `resume_permission`, then completes — the full Defer wire round trip.
//! 2. A subagent tool call reports up to the parent, and the parent's second
//!    fold can **tighten** the child's `Allow` to `Deny` (a hook scoped to the
//!    parent session) so the child tool never executes.
//! 3. Neither path deadlocks: every test body is wrapped in a `tokio::time`
//!    timeout so a parked turn task that the actor loop fails to drive shows up
//!    as a hard failure rather than a hang.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;
use llmsdk::language_model::{
    BoxStream, CallOptions, GenerateResult, StreamPart, StreamResult, ToolCallPart,
};
use llmsdk::{LanguageModel, ProviderError};
use tokio::time::timeout;

use zhive_core::engine::{Engine, EngineConfig, EngineEvent, PermissionRequestId};
use zhive_core::hooks::{HookFilter, HookFn, HookHost};
use zhive_core::provider::DynLanguageModel;
use zhive_core::tools::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use zhive_proto::domain::{Item, ItemContent, ItemId, ThreadId};
use zhive_proto::hook::{ExtensionRef, HookEvent};
use zhive_proto::permission::{
    HookOutput, PermissionDecision, PermissionOutcome, ResumeOutcome, ResumePermissionParams,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

// ============================================================
// Test fixtures
// ============================================================

/// A model that replays a different script on each successive `do_stream`.
#[derive(Debug, Clone)]
struct MultiScriptedModel {
    call_count: Arc<AtomicUsize>,
    scripts: Arc<Vec<Vec<StreamPart>>>,
}

impl MultiScriptedModel {
    fn new(scripts: Vec<Vec<StreamPart>>) -> Self {
        Self {
            call_count: Arc::new(AtomicUsize::new(0)),
            scripts: Arc::new(scripts),
        }
    }
}

#[async_trait]
impl LanguageModel for MultiScriptedModel {
    fn provider(&self) -> &'static str {
        "test"
    }

    fn model_id(&self) -> &'static str {
        "multi-scripted"
    }

    async fn do_generate(&self, _opts: CallOptions) -> llmsdk::error::Result<GenerateResult> {
        use llmsdk::language_model::{FinishReason, FinishReasonKind};
        Ok(GenerateResult {
            content: vec![],
            finish_reason: FinishReason::new(FinishReasonKind::Stop),
            usage: llmsdk::language_model::Usage::default(),
            provider_metadata: None,
            request: None,
            response: None,
            warnings: vec![],
        })
    }

    async fn do_stream(&self, _opts: CallOptions) -> llmsdk::error::Result<StreamResult> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        let parts = self.scripts.get(idx).cloned().unwrap_or_default();
        let iter = parts.into_iter().map(Ok::<_, ProviderError>);
        let s: BoxStream<llmsdk::error::Result<StreamPart>> = Box::pin(stream::iter(iter));
        Ok(StreamResult {
            stream: s,
            request: None,
            response: None,
        })
    }
}

/// One atomic tool-call stream part with the given id / name / input.
fn tool_call(id: &str, name: &str, input: serde_json::Value) -> StreamPart {
    StreamPart::ToolCall(ToolCallPart {
        tool_call_id: id.into(),
        tool_name: name.into(),
        input,
        provider_executed: None,
        dynamic: None,
        provider_options: None,
    })
}

/// A tool that records every execution into a shared counter.
#[derive(Debug)]
struct RecordingTool {
    name: &'static str,
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for RecordingTool {
    fn name(&self) -> &str {
        self.name
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("ran"))
    }
}

/// Builds engine-internal provenance for a test hook.
fn provenance(id: &str) -> ExtensionRef {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "version": "0.1.0",
        "source": "builtin",
    }))
    .expect("provenance fixture")
}

/// A `PreToolUse` hook returning a fixed decision for one tool, optionally
/// gated to a single session id (so it can target the parent's second fold
/// without also firing on the child's own fold).
struct DecidingHook {
    tool: String,
    decision: PermissionDecision,
    only_session: Option<String>,
}

#[async_trait]
impl HookFn for DecidingHook {
    async fn call(&self, event: &HookEvent) -> Option<HookOutput> {
        let HookEvent::PreToolUse(pre) = event else {
            return None;
        };
        if pre.tool_name != self.tool {
            return None;
        }
        if let Some(want) = &self.only_session
            && &pre.base.session_id != want
        {
            return None;
        }
        let decision = match self.decision {
            PermissionDecision::Allow => "allow",
            PermissionDecision::Ask => "ask",
            PermissionDecision::Defer => "defer",
            // `Deny` and any future `#[non_exhaustive]` variant: deny.
            _ => "deny",
        };
        serde_json::from_value(serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": decision,
            }
        }))
        .ok()
    }
}

/// Drains the broadcast bus into a Vec until `pred` matches, with a timeout.
async fn wait_for_event(
    rx: &mut tokio::sync::broadcast::Receiver<EngineEvent>,
    mut pred: impl FnMut(&EngineEvent) -> bool,
) -> EngineEvent {
    timeout(TEST_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if pred(&ev) {
                        return ev;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("event bus closed before predicate matched")
                }
            }
        }
    })
    .await
    .expect("timed out waiting for engine event")
}

fn tid(s: &str) -> ThreadId {
    ThreadId(Arc::from(s))
}

fn user_msg(text: &str) -> Item {
    Item::UserMessage {
        id: ItemId(Arc::from(format!("item:{text}"))),
        content: vec![ItemContent::Text {
            text: text.to_owned(),
            annotations: None,
        }],
    }
}

// ============================================================
// (1) Top-level Defer → TurnSuspended → resume → TurnResumed
// ============================================================

#[tokio::test]
async fn top_level_defer_suspend_resume_completes() {
    timeout(TEST_TIMEOUT, async {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(RecordingTool {
            name: "probe",
            runs: Arc::clone(&runs),
        }));

        // Script 0: emit a `probe` tool call. Script 1: empty (turn ends after
        // the resumed tool executes).
        let model = MultiScriptedModel::new(vec![
            vec![tool_call("tc-1", "probe", serde_json::json!({}))],
            vec![],
        ]);

        // Hook defers `probe` so the turn suspends on the reducer.
        let hooks = Arc::new(HookHost::new());
        let _scope = hooks
            .register(
                provenance("test.defer"),
                HookFilter::default(),
                0,
                Arc::new(DecidingHook {
                    tool: "probe".into(),
                    decision: PermissionDecision::Defer,
                    only_session: None,
                }),
            )
            .expect("register defer hook");

        let engine = Engine::spawn_with_config(EngineConfig {
            provider: DynLanguageModel::new(model),
            tools: Arc::new(reg),
            hook_host: hooks,
            ..EngineConfig::default()
        });
        let mut events = engine.subscribe();

        let thread = tid("thread:native/defer");
        engine
            .start_turn(thread.clone(), vec![user_msg("go")], None)
            .await
            .expect("start_turn");

        // The turn must suspend with a request id we can resume.
        let suspended = wait_for_event(&mut events, |ev| {
            matches!(ev, EngineEvent::TurnSuspended { .. })
        })
        .await;
        let EngineEvent::TurnSuspended {
            request_id,
            thread_id,
            ..
        } = suspended
        else {
            unreachable!("predicate guarantees TurnSuspended")
        };
        assert_eq!(thread_id, thread, "suspend attributed to the right thread");

        // Resume by allowing the deferred call.
        let reply = engine
            .resume_permission(
                request_id.clone(),
                PermissionOutcome::Selected {
                    option_id: "allow-once".into(),
                },
            )
            .await
            .expect("resume_permission dispatch");
        assert!(
            matches!(
                reply,
                zhive_core::engine::submission::ResumePermissionReply::Resolved
            ),
            "resume must resolve the pending request, got {reply:?}"
        );

        // A TurnResumed and then TurnCompleted must follow.
        wait_for_event(&mut events, |ev| {
            matches!(ev, EngineEvent::TurnResumed { .. })
        })
        .await;
        wait_for_event(&mut events, |ev| {
            matches!(ev, EngineEvent::TurnCompleted { .. })
        })
        .await;

        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "the deferred tool must run exactly once after resume"
        );

        engine.shutdown().await.expect("shutdown");
    })
    .await
    .expect("test deadlocked / timed out");
}

// ============================================================
// (1b) Resume id is the wire form the proto resume params accept
// ============================================================

#[tokio::test]
async fn suspended_request_id_round_trips_through_proto_resume_params() {
    // The id surfaced on TurnSuspended must be a valid `requestId` for the
    // proto `ResumePermissionParams` the server handler decodes.
    let request_id = PermissionRequestId(Arc::from("perm:7"));
    let params = ResumePermissionParams::new(request_id.0.as_ref(), ResumeOutcome::Cancelled);
    let v = serde_json::to_value(&params).expect("serialize");
    assert_eq!(v["requestId"], "perm:7");
    let back: ResumePermissionParams = serde_json::from_value(v).expect("round trip");
    assert_eq!(back.request_id, "perm:7");
}

// ============================================================
// (2) Subagent handshake: parent denies the child's Allow
// ============================================================

#[cfg(feature = "tools")]
#[tokio::test]
async fn parent_second_fold_denies_child_allow() {
    timeout(TEST_TIMEOUT, async {
        let parent_runs = Arc::new(AtomicUsize::new(0));
        let child_runs = Arc::new(AtomicUsize::new(0));

        let mut reg = ToolRegistry::new();
        // The `agent` builtin tool is required so the parent can spawn a child.
        reg.register(Arc::new(zhive_core::tools::builtin::AgentTool));
        reg.register(Arc::new(RecordingTool {
            name: "child_probe",
            runs: Arc::clone(&child_runs),
        }));
        // A parent-side recording tool only used to prove the parent's own
        // tools are unaffected (not strictly needed, kept minimal).
        let _ = &parent_runs;

        // do_stream call order across the single shared provider:
        //   0: parent turn → emit an `agent` tool call (spawns the child).
        //   1: child turn  → emit a `child_probe` tool call.
        //   2: child turn  → empty (child completes after the blocked call).
        //   3: parent turn → empty (parent completes after child returns).
        let model = MultiScriptedModel::new(vec![
            vec![tool_call(
                "tc-agent",
                "agent",
                serde_json::json!({
                    "name": "worker",
                    "description": "probe runner",
                    "prompt": "call child_probe",
                }),
            )],
            vec![tool_call("tc-child", "child_probe", serde_json::json!({}))],
            vec![],
            vec![],
        ]);

        // Hook denies `child_probe` ONLY on the parent's second-fold session
        // (the parent thread). The child's own fold (child session) sees no
        // deny, so its decision is Allow — and the parent's tighter Deny is
        // what blocks the call. This proves the parent can only tighten.
        let parent_thread = tid("thread:native/parent");
        let hooks = Arc::new(HookHost::new());
        let _scope = hooks
            .register(
                provenance("test.parent-deny"),
                HookFilter::default(),
                0,
                Arc::new(DecidingHook {
                    tool: "child_probe".into(),
                    decision: PermissionDecision::Deny,
                    only_session: Some(parent_thread.0.to_string()),
                }),
            )
            .expect("register parent-deny hook");

        let engine = Engine::spawn_with_config(EngineConfig {
            provider: DynLanguageModel::new(model),
            tools: Arc::new(reg),
            hook_host: hooks,
            ..EngineConfig::default()
        });
        let mut events = engine.subscribe();

        engine
            .start_turn(parent_thread.clone(), vec![user_msg("delegate")], None)
            .await
            .expect("start_turn");

        // Wait for the child subagent to finish (the parent received its final).
        wait_for_event(&mut events, |ev| {
            matches!(ev, EngineEvent::SubagentCompleted { .. })
        })
        .await;
        // Parent turn must then complete.
        wait_for_event(&mut events, |ev| {
            matches!(ev, EngineEvent::TurnCompleted { thread_id, .. } if *thread_id == parent_thread)
        })
        .await;

        assert_eq!(
            child_runs.load(Ordering::SeqCst),
            0,
            "the child tool must NOT execute: the parent's second fold denied it"
        );

        engine.shutdown().await.expect("shutdown");
    })
    .await
    .expect("test deadlocked / timed out");
}

// ============================================================
// (2b) Subagent handshake: parent allows the child's Allow
// ============================================================

#[cfg(feature = "tools")]
#[tokio::test]
async fn parent_second_fold_allows_child_tool() {
    timeout(TEST_TIMEOUT, async {
        let child_runs = Arc::new(AtomicUsize::new(0));

        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(zhive_core::tools::builtin::AgentTool));
        reg.register(Arc::new(RecordingTool {
            name: "child_probe",
            runs: Arc::clone(&child_runs),
        }));

        let model = MultiScriptedModel::new(vec![
            vec![tool_call(
                "tc-agent",
                "agent",
                serde_json::json!({
                    "name": "worker",
                    "description": "probe runner",
                    "prompt": "call child_probe",
                }),
            )],
            vec![tool_call("tc-child", "child_probe", serde_json::json!({}))],
            vec![],
            vec![],
        ]);

        // No hooks: both the child fold and the parent's second fold resolve to
        // Allow, so the child tool executes after the handshake round trip.
        let engine = Engine::spawn_with_config(EngineConfig {
            provider: DynLanguageModel::new(model),
            tools: Arc::new(reg),
            ..EngineConfig::default()
        });
        let mut events = engine.subscribe();

        let parent_thread = tid("thread:native/parent-allow");
        engine
            .start_turn(parent_thread.clone(), vec![user_msg("delegate")], None)
            .await
            .expect("start_turn");

        wait_for_event(&mut events, |ev| {
            matches!(ev, EngineEvent::SubagentCompleted { .. })
        })
        .await;
        wait_for_event(&mut events, |ev| {
            matches!(ev, EngineEvent::TurnCompleted { thread_id, .. } if *thread_id == parent_thread)
        })
        .await;

        assert_eq!(
            child_runs.load(Ordering::SeqCst),
            1,
            "the child tool must run once: both folds allowed it via the handshake"
        );

        engine.shutdown().await.expect("shutdown");
    })
    .await
    .expect("test deadlocked / timed out");
}

// ============================================================
// (3) Two-layer suspend/resume must not deadlock
// ============================================================

/// A child tool call that defers must drive a parent `TurnSuspended`, then
/// resume the parked child via the shared reducer — all without deadlock.
///
/// This is the headline deadlock guard: the parent turn task is parked inside
/// `spawn_and_await` waiting on its own reducer wait, while the resume is
/// delivered by the engine **actor loop** (a different task). If the resolve
/// were ever routed through the turn task this test would time out.
#[cfg(feature = "tools")]
#[tokio::test]
async fn child_defer_forwards_parent_suspend_then_resumes_without_deadlock() {
    timeout(TEST_TIMEOUT, async {
        let child_runs = Arc::new(AtomicUsize::new(0));

        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(zhive_core::tools::builtin::AgentTool));
        reg.register(Arc::new(RecordingTool {
            name: "child_probe",
            runs: Arc::clone(&child_runs),
        }));

        let model = MultiScriptedModel::new(vec![
            vec![tool_call(
                "tc-agent",
                "agent",
                serde_json::json!({
                    "name": "worker",
                    "description": "probe runner",
                    "prompt": "call child_probe",
                }),
            )],
            vec![tool_call("tc-child", "child_probe", serde_json::json!({}))],
            vec![],
            vec![],
        ]);

        // The child's OWN fold defers `child_probe` (hook gated to the child
        // session). The parent's second fold then also sees Defer and parks on
        // its reverse-RPC, emitting a parent TurnSuspended.
        let child_thread = tid("thread:subagent/thread:native/parent-defer/1");
        let _ = &child_thread; // exact id is engine-allocated; gate by tool only
        let hooks = Arc::new(HookHost::new());
        let _scope = hooks
            .register(
                provenance("test.child-defer"),
                HookFilter::default(),
                0,
                Arc::new(DecidingHook {
                    tool: "child_probe".into(),
                    decision: PermissionDecision::Defer,
                    // Defer on every session: the child folds Defer, reports up,
                    // and the parent's own fold also defers → parent suspends.
                    only_session: None,
                }),
            )
            .expect("register child-defer hook");

        let engine = Engine::spawn_with_config(EngineConfig {
            provider: DynLanguageModel::new(model),
            tools: Arc::new(reg),
            hook_host: hooks,
            ..EngineConfig::default()
        });
        let mut events = engine.subscribe();

        let parent_thread = tid("thread:native/parent-defer");
        engine
            .start_turn(parent_thread.clone(), vec![user_msg("delegate")], None)
            .await
            .expect("start_turn");

        // The parent must surface a TurnSuspended carrying the (child) request
        // id; resume it. The request id is globally unique on the shared
        // reducer regardless of which layer enrolled it.
        let suspended = wait_for_event(&mut events, |ev| {
            matches!(ev, EngineEvent::TurnSuspended { .. })
        })
        .await;
        let EngineEvent::TurnSuspended { request_id, .. } = suspended else {
            unreachable!("predicate guarantees TurnSuspended")
        };

        let reply = engine
            .resume_permission(
                request_id,
                PermissionOutcome::Selected {
                    option_id: "allow-once".into(),
                },
            )
            .await
            .expect("resume dispatch");
        assert!(
            matches!(
                reply,
                zhive_core::engine::submission::ResumePermissionReply::Resolved
            ),
            "resume must resolve, got {reply:?}"
        );

        // The child then completes and the parent turn finishes — proving the
        // actor loop drove the resolve while the turn task was parked.
        wait_for_event(&mut events, |ev| {
            matches!(ev, EngineEvent::SubagentCompleted { .. })
        })
        .await;
        wait_for_event(&mut events, |ev| {
            matches!(ev, EngineEvent::TurnCompleted { thread_id, .. } if *thread_id == parent_thread)
        })
        .await;

        assert_eq!(
            child_runs.load(Ordering::SeqCst),
            1,
            "the child tool must run once after the two-layer resume"
        );

        engine.shutdown().await.expect("shutdown");
    })
    .await
    .expect("two-layer suspend/resume deadlocked / timed out");
}

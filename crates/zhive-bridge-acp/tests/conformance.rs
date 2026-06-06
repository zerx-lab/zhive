//! End-to-end ACP conformance tests for the zhive bridge.
//!
//! Each test spins up `zhive_bridge_acp::serve_on` on one end of a
//! `tokio::io::duplex()` and a real `agent_client_protocol` **client** on the
//! other, connected by adapting the tokio byte halves to `futures` byte streams
//! via `tokio_util::compat`. The engine is driven by deterministic
//! `ScriptedModel` output so updates and stop reasons are reproducible.
//!
//! Coverage:
//! * `initialize` → `session/new` → `session/prompt` → `session/update` →
//!   `StopReason::EndTurn` (happy path with a scripted text turn).
//! * `session/cancel` mid-turn → `StopReason::Cancelled`.
//! * `session/request_permission` reverse-request round trip (client answers
//!   `Selected(allow-once)`, the tool then executes and the turn completes).

use std::sync::Arc;

use agent_client_protocol::schema::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, ProtocolVersion,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, StopReason, TextContent,
};
use agent_client_protocol::{ByteStreams, Client, ConnectionTo};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use llmsdk::language_model::StreamPart;
use zhive_core::engine::{Engine, EngineConfig, TurnLimits};
use zhive_core::hooks::{HookFilter, HookFn, HookHost};
use zhive_core::provider::ScriptedModel;
use zhive_core::tools::{EchoTool, ToolRegistry};
use zhive_proto::hook::HookEvent;
use zhive_proto::permission::{HookOutput, PermissionDecision};

/// Wires the agent (`serve_on`) and a client closure over an in-memory duplex.
///
/// The agent runs as a spawned task; `client_fn` receives the client-side
/// connection handle to drive requests. Returns the client closure's result.
async fn with_agent_and_client<F, Fut, T>(engine: Engine, client_fn: F) -> T
where
    F: FnOnce(ConnectionTo<agent_client_protocol::Agent>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, agent_client_protocol::Error>> + Send,
    T: Send + 'static,
{
    // duplex() gives two ends; each end is split into (read, write) halves.
    let (agent_side, client_side) = tokio::io::duplex(64 * 1024);
    let (agent_read, agent_write) = tokio::io::split(agent_side);
    let (client_read, client_write) = tokio::io::split(client_side);

    // Agent end: serve_on takes futures byte streams (write, read).
    let agent = tokio::spawn(async move {
        let _ = zhive_bridge_acp::serve_on(engine, agent_write.compat_write(), agent_read.compat())
            .await;
    });

    // Client end: a real ACP client over the other duplex half.
    let transport = ByteStreams::new(client_write.compat_write(), client_read.compat());
    let result = Client
        .connect_with(transport, async move |cx| client_fn(cx).await)
        .await
        .expect("client connection");

    agent.abort();
    result
}

/// A hook that returns a fixed [`PermissionDecision`] for every tool call.
struct FixedDecisionHook(PermissionDecision);

#[async_trait::async_trait]
impl HookFn for FixedDecisionHook {
    async fn call(&self, _event: &HookEvent) -> Option<HookOutput> {
        Some(
            serde_json::from_value(serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": self.0,
                }
            }))
            .expect("hook output fixture"),
        )
    }
}

/// Builds an `ExtensionRef` for hook registration (it is `#[non_exhaustive]`).
fn ext_ref(id: &str) -> zhive_proto::hook::ExtensionRef {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "version": "0.1.0",
        "source": "builtin"
    }))
    .expect("extension ref fixture")
}

/// Engine whose single turn streams "hello world" as agent text.
fn scripted_text_engine() -> Engine {
    let model = ScriptedModel::new(
        "test",
        "m",
        vec![
            StreamPart::TextStart {
                id: "b0".into(),
                provider_metadata: None,
            },
            StreamPart::TextDelta {
                id: "b0".into(),
                delta: "hello ".into(),
                provider_metadata: None,
            },
            StreamPart::TextDelta {
                id: "b0".into(),
                delta: "world".into(),
                provider_metadata: None,
            },
            StreamPart::TextEnd {
                id: "b0".into(),
                provider_metadata: None,
            },
        ],
    );
    Engine::spawn_with_provider(model.into_dyn())
}

/// Sends initialize + session/new and returns the minted session id.
async fn init_and_new_session(
    cx: &ConnectionTo<agent_client_protocol::Agent>,
) -> Result<agent_client_protocol::schema::SessionId, agent_client_protocol::Error> {
    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
        .block_task()
        .await?;
    let resp = cx
        .send_request(NewSessionRequest::new("/tmp"))
        .block_task()
        .await?;
    Ok(resp.session_id)
}

fn text_prompt(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new(text))]
}

#[tokio::test]
async fn initialize_new_session_prompt_completes() {
    let engine = scripted_text_engine();
    let stop = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;
        let resp = cx
            .send_request(PromptRequest::new(session_id, text_prompt("hi")))
            .block_task()
            .await?;
        Ok(resp.stop_reason)
    })
    .await;

    assert_eq!(
        stop,
        StopReason::EndTurn,
        "a scripted text turn must end with EndTurn"
    );
}

#[tokio::test]
async fn agent_text_is_not_duplicated() {
    // Regression guard for the agent-message-duplicated bug: the engine emits the
    // agent's text both as live `ItemDelta`s and a finalising whole-block
    // `AgentMessage`. The bridge must stream only the deltas, so the client sees
    // "hello world" exactly once, not "hello worldhello world".
    use std::sync::{Arc, Mutex};

    let engine = scripted_text_engine();
    let collected: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&collected);

    let stop = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;

        cx.add_dynamic_handler(AgentTextCollector { sink })?
            .run_indefinitely();

        let resp = cx
            .send_request(PromptRequest::new(session_id, text_prompt("hi")))
            .block_task()
            .await?;
        Ok(resp.stop_reason)
    })
    .await;

    assert_eq!(stop, StopReason::EndTurn);
    let text = collected.lock().expect("collector lock").clone();
    assert_eq!(
        text, "hello world",
        "agent text must appear exactly once (deltas only, no finalised re-emit)"
    );
}

/// Client handler that concatenates every `AgentMessageChunk`'s text.
///
/// Used to assert end-to-end that agent text is streamed exactly once.
struct AgentTextCollector {
    sink: std::sync::Arc<std::sync::Mutex<String>>,
}

impl agent_client_protocol::HandleDispatchFrom<agent_client_protocol::Agent>
    for AgentTextCollector
{
    async fn handle_dispatch_from(
        &mut self,
        message: agent_client_protocol::Dispatch,
        _cx: ConnectionTo<agent_client_protocol::Agent>,
    ) -> Result<
        agent_client_protocol::Handled<agent_client_protocol::Dispatch>,
        agent_client_protocol::Error,
    > {
        use agent_client_protocol::schema::{SessionNotification, SessionUpdate};
        match message.into_notification::<SessionNotification>()? {
            Ok(notif) => {
                if let SessionUpdate::AgentMessageChunk(chunk) = notif.update
                    && let ContentBlock::Text(text) = chunk.content
                {
                    self.sink.lock().expect("sink lock").push_str(&text.text);
                }
                Ok(agent_client_protocol::Handled::Yes)
            }
            Err(message) => Ok(agent_client_protocol::Handled::No {
                message,
                retry: false,
            }),
        }
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "AgentTextCollector"
    }
}

#[tokio::test]
async fn initialize_advertises_protocol_version() {
    let engine = Engine::spawn();
    let version = with_agent_and_client(engine, async move |cx| {
        let resp = cx
            .send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        Ok(resp.protocol_version)
    })
    .await;

    assert_eq!(version, ProtocolVersion::V1);
}

#[tokio::test]
async fn prompt_to_unknown_session_errors() {
    let engine = Engine::spawn();
    let errored = with_agent_and_client(engine, async move |cx| {
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        // Never created this session -> the agent must reject the prompt.
        let result = cx
            .send_request(PromptRequest::new("acp-does-not-exist", text_prompt("hi")))
            .block_task()
            .await;
        Ok(result.is_err())
    })
    .await;

    assert!(errored, "prompting an unknown session must return an error");
}

#[tokio::test]
async fn permission_round_trip_allows_tool() {
    // Engine: first model call emits a tool call (echo); second call returns a
    // text answer ending the turn. A PreToolUse hook returns `Ask`, so the
    // bridge must drive a session/request_permission reverse request.
    let engine = scripted_tool_call_engine(PermissionDecision::Ask);

    let stop = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;

        // The bridge will send us `session/request_permission`; spawn a handler
        // task that answers `Selected(allow-once)` so the tool executes.
        cx.add_dynamic_handler(PermissionAnswerer)?
            .run_indefinitely();

        let resp = cx
            .send_request(PromptRequest::new(session_id, text_prompt("run echo")))
            .block_task()
            .await?;
        Ok(resp.stop_reason)
    })
    .await;

    assert_eq!(
        stop,
        StopReason::EndTurn,
        "the turn must complete after the permission is granted"
    );
}

#[tokio::test]
async fn cancel_mid_turn_yields_cancelled() {
    // The turn suspends on a permission `Ask` that the client never answers;
    // instead it sends `session/cancel`, which aborts the engine turn. The
    // bridge must resolve the prompt with `StopReason::Cancelled`.
    let engine = scripted_tool_call_engine(PermissionDecision::Ask);

    let stop = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;

        // When the bridge asks for permission, cancel the session instead of
        // answering. The handler then replies `Cancelled` to settle the
        // pending reverse request, matching the ACP cancellation contract.
        cx.add_dynamic_handler(CancelOnPermission {
            session_id: session_id.clone(),
        })?
        .run_indefinitely();

        let resp = cx
            .send_request(PromptRequest::new(session_id, text_prompt("run echo")))
            .block_task()
            .await?;
        Ok(resp.stop_reason)
    })
    .await;

    assert_eq!(
        stop,
        StopReason::Cancelled,
        "a cancelled turn must report StopReason::Cancelled"
    );
}

#[tokio::test]
async fn cancel_without_answering_permission_does_not_deadlock() {
    // Adversarial client: it sends `session/cancel` but NEVER answers the
    // outstanding `session/request_permission` (the spec only says a client
    // SHOULD answer). The bridge must still settle the prompt as `Cancelled`
    // instead of blocking forever on the unanswered reverse request. The whole
    // exchange is wrapped in a timeout so a regression deadlocks the test
    // rather than hanging the suite.
    let engine = scripted_tool_call_engine(PermissionDecision::Ask);

    let run = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;

        cx.add_dynamic_handler(CancelWithoutAnswering {
            session_id: session_id.clone(),
        })?
        .run_indefinitely();

        let resp = cx
            .send_request(PromptRequest::new(session_id, text_prompt("run echo")))
            .block_task()
            .await?;
        Ok(resp.stop_reason)
    });

    let stop = tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("prompt must resolve; a deadlock would expire this timeout");

    assert_eq!(
        stop,
        StopReason::Cancelled,
        "a cancel that never answers the permission must still yield Cancelled"
    );
}

/// Client handler that cancels the session on the first permission request and
/// deliberately never answers the reverse request (drops the responder).
struct CancelWithoutAnswering {
    session_id: agent_client_protocol::schema::SessionId,
}

impl agent_client_protocol::HandleDispatchFrom<agent_client_protocol::Agent>
    for CancelWithoutAnswering
{
    async fn handle_dispatch_from(
        &mut self,
        message: agent_client_protocol::Dispatch,
        cx: ConnectionTo<agent_client_protocol::Agent>,
    ) -> Result<
        agent_client_protocol::Handled<agent_client_protocol::Dispatch>,
        agent_client_protocol::Error,
    > {
        match message.into_request::<RequestPermissionRequest>()? {
            Ok((_req, _responder)) => {
                // Cancel, then drop `_responder` WITHOUT replying. The bridge's
                // cancel race must observe the engine abort and resolve the turn.
                use agent_client_protocol::schema::CancelNotification;
                cx.send_notification(CancelNotification::new(self.session_id.clone()))?;
                Ok(agent_client_protocol::Handled::Yes)
            }
            Err(message) => Ok(agent_client_protocol::Handled::No {
                message,
                retry: false,
            }),
        }
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "CancelWithoutAnswering"
    }
}

/// Client handler that, on the first permission request, cancels the session
/// and answers the reverse request with `Cancelled`.
struct CancelOnPermission {
    session_id: agent_client_protocol::schema::SessionId,
}

impl agent_client_protocol::HandleDispatchFrom<agent_client_protocol::Agent>
    for CancelOnPermission
{
    async fn handle_dispatch_from(
        &mut self,
        message: agent_client_protocol::Dispatch,
        cx: ConnectionTo<agent_client_protocol::Agent>,
    ) -> Result<
        agent_client_protocol::Handled<agent_client_protocol::Dispatch>,
        agent_client_protocol::Error,
    > {
        match message.into_request::<RequestPermissionRequest>()? {
            Ok((_req, responder)) => {
                // Cancel the turn, then settle the in-flight permission request
                // with `Cancelled` as the spec mandates.
                use agent_client_protocol::schema::CancelNotification;
                cx.send_notification(CancelNotification::new(self.session_id.clone()))?;
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))?;
                Ok(agent_client_protocol::Handled::Yes)
            }
            Err(message) => Ok(agent_client_protocol::Handled::No {
                message,
                retry: false,
            }),
        }
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "CancelOnPermission"
    }
}

/// Dynamic client handler that answers every permission request with allow-once.
struct PermissionAnswerer;

impl agent_client_protocol::HandleDispatchFrom<agent_client_protocol::Agent>
    for PermissionAnswerer
{
    async fn handle_dispatch_from(
        &mut self,
        message: agent_client_protocol::Dispatch,
        _cx: ConnectionTo<agent_client_protocol::Agent>,
    ) -> Result<
        agent_client_protocol::Handled<agent_client_protocol::Dispatch>,
        agent_client_protocol::Error,
    > {
        match message.into_request::<RequestPermissionRequest>()? {
            Ok((_req, responder)) => {
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                        "allow-once",
                    )),
                ))?;
                Ok(agent_client_protocol::Handled::Yes)
            }
            Err(message) => Ok(agent_client_protocol::Handled::No {
                message,
                retry: false,
            }),
        }
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "PermissionAnswerer"
    }
}

#[tokio::test]
async fn turn_failure_yields_end_turn_and_surfaces_error() {
    use std::sync::{Arc, Mutex};

    // A provider whose stream errors mid-flight makes the engine broadcast
    // `TurnFailed`. The bridge must map that to `EndTurn` (NOT `Refusal`: the
    // transcript is preserved) and surface the failure text to the user.
    let engine = Engine::spawn_with_provider(FailingModel.into_dyn());
    let collected: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&collected);

    let stop = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;

        cx.add_dynamic_handler(AgentTextCollector { sink })?
            .run_indefinitely();

        let resp = cx
            .send_request(PromptRequest::new(session_id, text_prompt("hi")))
            .block_task()
            .await?;
        Ok(resp.stop_reason)
    })
    .await;

    assert_eq!(
        stop,
        StopReason::EndTurn,
        "a transient TurnFailed must end the turn, not signal a content Refusal"
    );
    let text = collected.lock().expect("collector lock").clone();
    assert!(
        text.contains("turn failed"),
        "the bridge must surface the failure text to the user, got: {text:?}"
    );
}

/// A model whose stream yields a single provider error to force `TurnFailed`.
#[derive(Debug, Clone)]
struct FailingModel;

impl FailingModel {
    fn into_dyn(self) -> zhive_core::provider::DynLanguageModel {
        zhive_core::provider::DynLanguageModel::new(self)
    }
}

#[async_trait::async_trait]
impl llmsdk::LanguageModel for FailingModel {
    fn provider(&self) -> &'static str {
        "test"
    }

    fn model_id(&self) -> &'static str {
        "failing"
    }

    async fn do_generate(
        &self,
        _opts: llmsdk::language_model::CallOptions,
    ) -> llmsdk::error::Result<llmsdk::language_model::GenerateResult> {
        Err(llmsdk::ProviderError::api_call("test://x", "boom"))
    }

    async fn do_stream(
        &self,
        _opts: llmsdk::language_model::CallOptions,
    ) -> llmsdk::error::Result<llmsdk::language_model::StreamResult> {
        use futures::stream;
        let err = llmsdk::ProviderError::api_call("test://x", "boom");
        let iter = std::iter::once(Err::<StreamPart, _>(err));
        let s: llmsdk::language_model::BoxStream<llmsdk::error::Result<StreamPart>> =
            Box::pin(stream::iter(iter));
        Ok(llmsdk::language_model::StreamResult {
            stream: s,
            request: None,
            response: None,
        })
    }
}

/// Engine that emits one `echo` tool call then a text answer, with `decision`.
fn scripted_tool_call_engine(decision: PermissionDecision) -> Engine {
    use llmsdk::ToolCallPart;

    let script0 = vec![StreamPart::ToolCall(ToolCallPart {
        tool_call_id: "tc-0".into(),
        tool_name: "echo".into(),
        input: serde_json::json!({ "msg": "hi" }),
        provider_executed: None,
        dynamic: None,
        provider_options: None,
    })];
    let script1 = vec![
        StreamPart::TextStart {
            id: "b0".into(),
            provider_metadata: None,
        },
        StreamPart::TextDelta {
            id: "b0".into(),
            delta: "done".into(),
            provider_metadata: None,
        },
        StreamPart::TextEnd {
            id: "b0".into(),
            provider_metadata: None,
        },
    ];

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));

    let hook_host = Arc::new(HookHost::new());
    // Keep the registration scope alive for the engine's lifetime by leaking it;
    // the engine outlives this builder and tests are short-lived processes.
    let scope = hook_host
        .register(
            ext_ref("perm-hook"),
            HookFilter::default(),
            0,
            Arc::new(FixedDecisionHook(decision)),
        )
        .expect("hook registration");
    std::mem::forget(scope);

    let cfg = EngineConfig {
        provider: MultiScriptedModel::new(vec![script0, script1]).into_dyn(),
        tools: Arc::new(tools),
        hook_host,
        storage: None,
        cwd: std::path::PathBuf::from("."),
        turn_limits: TurnLimits::default(),
        system_prompt: None,
        compaction_prompt: None,
        compact_token_threshold: None,
    };
    Engine::spawn_with_config(cfg)
}

/// A model returning a different `StreamPart` script on each successive call.
#[derive(Debug, Clone)]
struct MultiScriptedModel {
    call_count: Arc<std::sync::atomic::AtomicUsize>,
    scripts: Arc<Vec<Vec<StreamPart>>>,
}

impl MultiScriptedModel {
    fn new(scripts: Vec<Vec<StreamPart>>) -> Self {
        Self {
            call_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            scripts: Arc::new(scripts),
        }
    }

    fn into_dyn(self) -> zhive_core::provider::DynLanguageModel {
        zhive_core::provider::DynLanguageModel::new(self)
    }
}

#[async_trait::async_trait]
impl llmsdk::LanguageModel for MultiScriptedModel {
    fn provider(&self) -> &'static str {
        "test"
    }

    fn model_id(&self) -> &'static str {
        "multi-scripted"
    }

    async fn do_generate(
        &self,
        _opts: llmsdk::language_model::CallOptions,
    ) -> llmsdk::error::Result<llmsdk::language_model::GenerateResult> {
        use llmsdk::language_model::{FinishReason, FinishReasonKind};
        Ok(llmsdk::language_model::GenerateResult {
            content: vec![],
            finish_reason: FinishReason::new(FinishReasonKind::Stop),
            usage: llmsdk::language_model::Usage::default(),
            provider_metadata: None,
            request: None,
            response: None,
            warnings: vec![],
        })
    }

    async fn do_stream(
        &self,
        _opts: llmsdk::language_model::CallOptions,
    ) -> llmsdk::error::Result<llmsdk::language_model::StreamResult> {
        use futures::stream;
        let idx = self
            .call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let parts = self.scripts.get(idx).cloned().unwrap_or_default();
        let iter = parts.into_iter().map(Ok::<_, llmsdk::ProviderError>);
        let s: llmsdk::language_model::BoxStream<llmsdk::error::Result<StreamPart>> =
            Box::pin(stream::iter(iter));
        Ok(llmsdk::language_model::StreamResult {
            stream: s,
            request: None,
            response: None,
        })
    }
}

/// Captures a one-line summary of every `session/update` and answers any
/// `request_permission` with allow-once, so a test can assert the exact
/// notification sequence the bridge emits for a tool-call turn.
struct UpdateCapture {
    sink: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl agent_client_protocol::HandleDispatchFrom<agent_client_protocol::Agent> for UpdateCapture {
    async fn handle_dispatch_from(
        &mut self,
        message: agent_client_protocol::Dispatch,
        _cx: ConnectionTo<agent_client_protocol::Agent>,
    ) -> Result<
        agent_client_protocol::Handled<agent_client_protocol::Dispatch>,
        agent_client_protocol::Error,
    > {
        use agent_client_protocol::schema::{SessionNotification, SessionUpdate};
        let message = match message.into_notification::<SessionNotification>()? {
            Ok(notif) => {
                let summary = match &notif.update {
                    SessionUpdate::ToolCall(tc) => format!(
                        "ToolCall id={} status={:?} content_len={}",
                        tc.tool_call_id.0,
                        tc.status,
                        tc.content.len()
                    ),
                    SessionUpdate::ToolCallUpdate(tu) => format!(
                        "ToolCallUpdate id={} status={:?} content_len={}",
                        tu.tool_call_id.0,
                        tu.fields.status,
                        tu.fields.content.as_ref().map_or(0, Vec::len)
                    ),
                    SessionUpdate::AgentMessageChunk(_) => "AgentMessageChunk".to_string(),
                    other => format!("{other:?}"),
                };
                self.sink.lock().expect("sink").push(summary);
                return Ok(agent_client_protocol::Handled::Yes);
            }
            Err(message) => message,
        };
        match message.into_request::<RequestPermissionRequest>()? {
            Ok((req, responder)) => {
                self.sink.lock().expect("sink").push(format!(
                    "RequestPermission tool_call_id={}",
                    req.tool_call.tool_call_id.0
                ));
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                        "allow-once",
                    )),
                ))?;
                Ok(agent_client_protocol::Handled::Yes)
            }
            Err(message) => Ok(agent_client_protocol::Handled::No {
                message,
                retry: false,
            }),
        }
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "UpdateCapture"
    }
}

#[tokio::test]
async fn tool_call_announces_then_updates_with_matching_id() {
    use std::sync::{Arc, Mutex};

    // The bridge must follow the canonical ACP two-event tool-call lifecycle
    // (matching pi / opencode):
    //   1. `SessionUpdate::ToolCall` with status=InProgress — establishes the
    //      card in the client UI (empty content, no output yet).
    //   2. `session/request_permission` — the hook returns `Ask`, so the bridge
    //      sends the reverse request before executing the tool.
    //   3. `SessionUpdate::ToolCallUpdate` with status=Completed and
    //      content_len=1 — carries the tool output after execution.
    //
    // Both events must share the same tool_call_id ("tc-0" from the script),
    // so the client can correlate the update with the initial card.
    let engine = scripted_tool_call_engine(PermissionDecision::Ask);
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);

    with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;
        cx.add_dynamic_handler(UpdateCapture { sink })?
            .run_indefinitely();
        cx.send_request(PromptRequest::new(session_id, text_prompt("run echo")))
            .block_task()
            .await?;
        Ok(())
    })
    .await;

    let lines = captured.lock().expect("lock").clone();

    // 1. InProgress ToolCall must be sent first (establishes the card).
    let announce = lines
        .iter()
        .find(|l| l.starts_with("ToolCall ") && l.contains("InProgress"))
        .unwrap_or_else(|| panic!("expected InProgress ToolCall announcement; got: {lines:?}"));
    assert!(
        announce.contains("id=tc-0"),
        "InProgress ToolCall id must match; got: {announce}"
    );

    // 2. ToolCallUpdate with the tool output must follow.
    let update = lines
        .iter()
        .find(|l| l.starts_with("ToolCallUpdate ") && l.contains("id=tc-0"))
        .unwrap_or_else(|| panic!("expected ToolCallUpdate for tc-0; got: {lines:?}"));
    assert!(
        update.contains("content_len=1"),
        "ToolCallUpdate must carry the tool output; got: {update}"
    );
    assert!(
        update.contains("id=tc-0"),
        "ToolCallUpdate id must match announcement; got: {update}"
    );

    // 3. Permission request must use the same provider id.
    let permission = lines
        .iter()
        .find(|l| l.starts_with("RequestPermission "))
        .unwrap_or_else(|| panic!("expected RequestPermission; got: {lines:?}"));
    assert!(
        permission.contains("tool_call_id=tc-0"),
        "permission must reference the same tool call; got: {permission}"
    );
}

/// A deterministic in-memory model catalogue for the config-option tests.
#[derive(Debug)]
struct TestCatalog;

#[async_trait::async_trait]
impl zhive_core::engine::ModelCatalog for TestCatalog {
    async fn list(
        &self,
    ) -> Result<Vec<zhive_proto::rpc::ModelDescriptor>, zhive_core::engine::ModelCatalogError> {
        use zhive_proto::domain::ThinkingEffort;
        use zhive_proto::rpc::ModelDescriptor;
        Ok(vec![
            ModelDescriptor::new("model-a".to_owned())
                .with_context_window(Some(200_000))
                .with_supported_efforts(vec![ThinkingEffort::Off, ThinkingEffort::High])
                .with_active(true),
            ModelDescriptor::new("model-b".to_owned()).with_context_window(Some(1_000_000)),
        ])
    }

    fn switch(
        &self,
        model_id: &str,
        context_window_hint: Option<u64>,
    ) -> Result<zhive_core::engine::SwitchedModel, zhive_core::engine::ModelCatalogError> {
        let window = context_window_hint.or(Some(123_456));
        Ok(zhive_core::engine::SwitchedModel::new(
            ScriptedModel::new("stub", model_id, vec![]).into_dyn(),
            window,
        ))
    }
}

/// Returns the `select` current value for the config option with `id`, if any.
fn config_current_value(
    options: &[agent_client_protocol::schema::SessionConfigOption],
    id: &str,
) -> Option<String> {
    use agent_client_protocol::schema::SessionConfigKind;
    let option = options.iter().find(|o| o.id.0.as_ref() == id)?;
    match &option.kind {
        SessionConfigKind::Select(sel) => Some(sel.current_value.0.to_string()),
        _ => None,
    }
}

#[tokio::test]
async fn new_session_advertises_model_and_effort_config_options() {
    use agent_client_protocol::schema::{NewSessionRequest, ProtocolVersion};

    // With a catalogue injected, `session/new` must surface a model selector and
    // a reasoning-depth selector so the editor renders both dropdowns.
    let engine = scripted_text_engine().with_model_catalog(std::sync::Arc::new(TestCatalog));

    let options = with_agent_and_client(engine, async move |cx| {
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        let resp = cx
            .send_request(NewSessionRequest::new("/tmp"))
            .block_task()
            .await?;
        Ok(resp.config_options.unwrap_or_default())
    })
    .await;

    assert_eq!(
        config_current_value(&options, "model").as_deref(),
        Some("model-a"),
        "model selector defaults to the active model"
    );
    assert_eq!(
        config_current_value(&options, "effort").as_deref(),
        Some("off"),
        "reasoning selector defaults to the model's first depth"
    );
}

#[tokio::test]
async fn set_config_option_switches_effort_and_model() {
    use agent_client_protocol::schema::{
        NewSessionRequest, ProtocolVersion, SetSessionConfigOptionRequest,
    };

    let engine = scripted_text_engine().with_model_catalog(std::sync::Arc::new(TestCatalog));

    let (after_effort, after_model) = with_agent_and_client(engine, async move |cx| {
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await?;
        let session = cx
            .send_request(NewSessionRequest::new("/tmp"))
            .block_task()
            .await?
            .session_id;

        // Pick the High reasoning depth; the response echoes the new value.
        let effort_resp = cx
            .send_request(SetSessionConfigOptionRequest::new(
                session.clone(),
                "effort",
                "high",
            ))
            .block_task()
            .await?;

        // Switch the active model; the response reflects the new selection.
        let model_resp = cx
            .send_request(SetSessionConfigOptionRequest::new(
                session.clone(),
                "model",
                "model-b",
            ))
            .block_task()
            .await?;

        Ok((effort_resp.config_options, model_resp.config_options))
    })
    .await;

    assert_eq!(
        config_current_value(&after_effort, "effort").as_deref(),
        Some("high"),
        "the effort selection must round-trip in the response"
    );
    assert_eq!(
        config_current_value(&after_model, "model").as_deref(),
        Some("model-b"),
        "the model switch must be reflected as the new active model"
    );
}

// ── Slash-command conformance tests ─────────────────────────────────────────

/// Returns an `Arc<Mutex<String>>` collector and registers an `AgentTextCollector`
/// handler that funnels every `AgentMessageChunk` into it.
fn start_text_collector(
    cx: &ConnectionTo<agent_client_protocol::Agent>,
) -> Arc<std::sync::Mutex<String>> {
    let buf: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
    cx.add_dynamic_handler(AgentTextCollector {
        sink: Arc::clone(&buf),
    })
    .expect("add handler")
    .run_indefinitely();
    buf
}

/// `/compact` on an empty thread → "nothing to compact".
#[tokio::test]
async fn slash_compact_nothing_to_compact() {
    let engine = scripted_text_engine();
    let (stop, text) = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;
        let buf = start_text_collector(&cx);
        let resp = cx
            .send_request(PromptRequest::new(session_id, text_prompt("/compact")))
            .block_task()
            .await?;
        let text = buf.lock().expect("lock").clone();
        Ok((resp.stop_reason, text))
    })
    .await;

    assert_eq!(stop, StopReason::EndTurn, "/compact must end with EndTurn");
    assert!(
        text.contains("nothing to compact") || text.contains("compact"),
        "compact response must mention compaction; got: {text:?}"
    );
}

/// `/clear` rebinds the session and returns a confirmation.
#[tokio::test]
async fn slash_clear_returns_confirmation() {
    let engine = scripted_text_engine();
    let (stop, text) = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;
        let buf = start_text_collector(&cx);
        let resp = cx
            .send_request(PromptRequest::new(session_id, text_prompt("/clear")))
            .block_task()
            .await?;
        let text = buf.lock().expect("lock").clone();
        Ok((resp.stop_reason, text))
    })
    .await;

    assert_eq!(stop, StopReason::EndTurn, "/clear must end with EndTurn");
    assert!(
        text.to_lowercase().contains("cleared") || text.to_lowercase().contains("fresh"),
        "/clear must confirm the reset; got: {text:?}"
    );
}

/// `/new` is an alias for `/clear` and also returns a confirmation.
#[tokio::test]
async fn slash_new_returns_confirmation() {
    let engine = scripted_text_engine();
    let (stop, text) = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;
        let buf = start_text_collector(&cx);
        let resp = cx
            .send_request(PromptRequest::new(session_id, text_prompt("/new")))
            .block_task()
            .await?;
        let text = buf.lock().expect("lock").clone();
        Ok((resp.stop_reason, text))
    })
    .await;

    assert_eq!(stop, StopReason::EndTurn, "/new must end with EndTurn");
    assert!(
        !text.is_empty(),
        "/new must send a response notification; got empty text"
    );
}

/// `/help` returns a list of available commands.
#[tokio::test]
async fn slash_help_lists_commands() {
    let engine = scripted_text_engine();
    let (stop, text) = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;
        let buf = start_text_collector(&cx);
        let resp = cx
            .send_request(PromptRequest::new(session_id, text_prompt("/help")))
            .block_task()
            .await?;
        let text = buf.lock().expect("lock").clone();
        Ok((resp.stop_reason, text))
    })
    .await;

    assert_eq!(stop, StopReason::EndTurn, "/help must end with EndTurn");
    assert!(
        text.contains("/compact"),
        "/help output must mention /compact; got: {text:?}"
    );
    assert!(
        text.contains("/clear") || text.contains("/new"),
        "/help output must mention /clear or /new; got: {text:?}"
    );
}

/// `/?` is an alias for `/help`.
#[tokio::test]
async fn slash_question_mark_is_help_alias() {
    let engine = scripted_text_engine();
    let (stop, text) = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;
        let buf = start_text_collector(&cx);
        let resp = cx
            .send_request(PromptRequest::new(session_id, text_prompt("/?")))
            .block_task()
            .await?;
        let text = buf.lock().expect("lock").clone();
        Ok((resp.stop_reason, text))
    })
    .await;

    assert_eq!(stop, StopReason::EndTurn);
    assert!(
        !text.is_empty(),
        "/? must produce help text; got empty text"
    );
}

/// `/skills` returns the skill list (empty when no skills are installed).
#[tokio::test]
async fn slash_skills_returns_list() {
    let engine = scripted_text_engine();
    let (stop, text) = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;
        let buf = start_text_collector(&cx);
        let resp = cx
            .send_request(PromptRequest::new(session_id, text_prompt("/skills")))
            .block_task()
            .await?;
        let text = buf.lock().expect("lock").clone();
        Ok((resp.stop_reason, text))
    })
    .await;

    assert_eq!(stop, StopReason::EndTurn, "/skills must end with EndTurn");
    assert!(
        text.to_lowercase().contains("skill") || text.to_lowercase().contains("no skills"),
        "/skills must mention skills; got: {text:?}"
    );
}

/// An unrecognised command sends an error notification instead of hanging.
#[tokio::test]
async fn slash_unknown_command_returns_error() {
    let engine = scripted_text_engine();
    let (stop, text) = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;
        let buf = start_text_collector(&cx);
        let resp = cx
            .send_request(PromptRequest::new(
                session_id,
                text_prompt("/xyzzy_no_such_command"),
            ))
            .block_task()
            .await?;
        let text = buf.lock().expect("lock").clone();
        Ok((resp.stop_reason, text))
    })
    .await;

    assert_eq!(
        stop,
        StopReason::EndTurn,
        "unknown slash command must end with EndTurn"
    );
    assert!(
        text.contains("unknown"),
        "response must mention unknown command; got: {text:?}"
    );
}

/// After `/clear`, the next prompt starts a fresh LLM turn (no leftover history).
#[tokio::test]
async fn slash_clear_then_prompt_completes() {
    let engine = scripted_text_engine();
    let stop = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;
        // First, clear the session.
        let clear = cx
            .send_request(PromptRequest::new(
                session_id.clone(),
                text_prompt("/clear"),
            ))
            .block_task()
            .await?;
        assert_eq!(clear.stop_reason, StopReason::EndTurn);
        // Then, send a normal prompt to the fresh thread — must complete.
        let resp = cx
            .send_request(PromptRequest::new(session_id, text_prompt("hello")))
            .block_task()
            .await?;
        Ok(resp.stop_reason)
    })
    .await;

    assert_eq!(
        stop,
        StopReason::EndTurn,
        "prompt after /clear must complete normally"
    );
}

/// A slash command is still intercepted when the prompt has extra trailing blocks.
///
/// ACP clients (e.g. Zed) may attach workspace context or file contents
/// alongside the slash text. The bridge must route on the first block alone.
#[tokio::test]
async fn slash_command_with_extra_blocks_is_intercepted() {
    let engine = scripted_text_engine();
    let (stop, text) = with_agent_and_client(engine, async move |cx| {
        let session_id = init_and_new_session(&cx).await?;
        let buf = start_text_collector(&cx);
        // Two-block prompt: slash command first, then some context text.
        let resp = cx
            .send_request(PromptRequest::new(
                session_id,
                vec![
                    ContentBlock::Text(TextContent::new("/help")),
                    ContentBlock::Text(TextContent::new("some attached context")),
                ],
            ))
            .block_task()
            .await?;
        let text = buf.lock().expect("lock").clone();
        Ok((resp.stop_reason, text))
    })
    .await;

    assert_eq!(
        stop,
        StopReason::EndTurn,
        "slash command with extra blocks must end with EndTurn"
    );
    assert!(
        text.contains("compact") || text.contains("clear"),
        "response must be the /help listing, not an LLM reply; got: {text:?}"
    );
}

// Rust guideline compliant 2026-02-21

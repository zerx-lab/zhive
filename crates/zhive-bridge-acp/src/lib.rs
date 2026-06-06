//! ACP (Agent Client Protocol) agent for zhive.
//!
//! Serves zhive as a standard ACP **agent** that editors (Zed, etc.) connect
//! to. The bridge embeds a [`zhive_core::engine::Engine`] in-process and
//! translates ACP requests into engine operations, streaming engine events back
//! as `session/update` notifications. It is built on the official
//! [`agent_client_protocol`] SDK (v0.13), whose agent surface is a **builder +
//! per-message callback** API (`Agent.builder().on_receive_request(...)`), not a
//! trait to implement.
//!
//! # Architecture
//!
//! * **Embed, not proxy.** The bridge owns the engine and calls
//!   [`Engine::start_turn`] / [`Engine::cancel_turn`] /
//!   [`Engine::resume_permission`] directly. There is no UDS hop.
//! * **One session, one thread.** `session/new` mints a [`SessionId`] bound to a
//!   fresh zhive `ThreadId` ([`crate::state`]).
//! * **Streaming via a spawned task.** ACP callbacks run on a single-threaded
//!   event loop where blocking on a reverse request deadlocks. The `prompt`
//!   callback therefore offloads the turn to
//!   [`agent_client_protocol::ConnectionTo::spawn`], where it may safely `await`
//!   the permission reverse request and the engine event stream, then responds
//!   with the final [`StopReason`].
//!
//! # Conformance (v1)
//!
//! Covered: `initialize` (capability negotiation), `session/new`,
//! `session/prompt` (streaming `AgentMessageChunk` / `AgentThoughtChunk` /
//! `ToolCall` / `Plan`), `session/cancel`, and the `session/request_permission`
//! reverse request. Deferred (advertised off): `session/load`, session modes,
//! authentication, and client `fs/*` / `terminal/*` (zhive tools run
//! in-process). See [`crate::convert`] for the data mapping.
//!
//! # Examples
//!
//! ```no_run
//! use zhive_core::engine::Engine;
//!
//! # async fn run() -> Result<(), zhive_bridge_acp::AcpError> {
//! let engine = Engine::spawn();
//! // Serves ACP over stdio until the client disconnects.
//! zhive_bridge_acp::serve(engine).await
//! # }
//! ```

pub mod config_option;
pub mod convert;
pub mod error;
pub mod permission;
pub mod slash;
pub mod state;

#[doc(inline)]
pub use error::AcpError;
#[doc(inline)]
pub use slash::Skill as SlashSkill;

use std::sync::Arc;

use agent_client_protocol::schema::{
    AgentCapabilities, CancelNotification, ContentBlock, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionId,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason, ToolCallId,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo, Dispatch, Responder, Stdio};
use futures::{AsyncRead, AsyncWrite};
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;
use zhive_core::engine::Engine;
use zhive_core::engine::event::EngineEvent;
use zhive_core::engine::submission::PermissionRequestId;
use zhive_proto::domain::{ThinkingEffort, ThreadId, TurnId};
use zhive_proto::permission::{
    PermissionOutcome, RequestPermissionRequest as ZRequestPermissionRequest,
};

use slash::Skill;
use state::AgentState;

/// Serves zhive over ACP on stdio until the client disconnects.
///
/// Embeds `engine` and wires the ACP agent callbacks onto [`Stdio::new`].
/// Returns when the transport closes or a connection-fatal error occurs. The
/// caller (the `zhive acp` subcommand) is responsible for building the engine
/// (provider, tools, hooks) and for keeping stdout clean — stdout is the
/// JSON-RPC wire.
///
/// # Errors
///
/// Returns [`AcpError::Transport`] if the ACP connection fails, or
/// [`AcpError::Engine`] if the embedded engine actor is unreachable.
///
/// # Examples
///
/// ```no_run
/// use zhive_core::engine::Engine;
/// # async fn run() -> Result<(), zhive_bridge_acp::AcpError> {
/// zhive_bridge_acp::serve(Engine::spawn()).await
/// # }
/// ```
pub async fn serve(engine: Engine) -> Result<(), AcpError> {
    let skills = discover_skills();
    build_agent(&engine, &AgentState::new(), &skills)
        .connect_to(Stdio::new())
        .await
        .map_err(AcpError::from)
}

/// Serves zhive over ACP on an explicit byte transport (test seam).
///
/// Identical to [`serve`] but accepts caller-provided `write` / `read` halves
/// instead of `Stdio::new()`, so in-process tests can drive the agent with an
/// ACP client over `tokio::io::duplex()` (adapted to `futures` byte streams).
///
/// # Errors
///
/// Same as [`serve`].
///
/// # Examples
///
/// ```no_run
/// use zhive_core::engine::Engine;
/// use futures::io::{empty, sink};
/// # async fn run() -> Result<(), zhive_bridge_acp::AcpError> {
/// // A real call wires duplex halves; `empty`/`sink` just type-check here.
/// zhive_bridge_acp::serve_on(Engine::spawn(), sink(), empty()).await
/// # }
/// ```
pub async fn serve_on<W, R>(engine: Engine, write: W, read: R) -> Result<(), AcpError>
where
    W: AsyncWrite + Send + 'static,
    R: AsyncRead + Send + 'static,
{
    let skills = discover_skills();
    build_agent(&engine, &AgentState::new(), &skills)
        .connect_to(ByteStreams::new(write, read))
        .await
        .map_err(AcpError::from)
}

/// Discovers on-disk skills when the `skills` feature is enabled.
///
/// Returns an empty slice when the feature is off or no skills are installed.
fn discover_skills() -> Arc<[Skill]> {
    #[cfg(feature = "skills")]
    {
        use zhive_core::skills::{SkillDiscoveryConfig, SkillSet};
        let set = SkillSet::discover_and_load(&SkillDiscoveryConfig::new());
        set.catalogue()
            .into_iter()
            .map(|e| Skill {
                name: Arc::from(e.name.as_str()),
                invocation: Arc::from(e.invocation.as_str()),
            })
            .collect()
    }
    #[cfg(not(feature = "skills"))]
    Arc::from([])
}

/// Constructs the configured ACP agent builder with all callbacks registered.
///
/// Factored out so [`serve`] and [`serve_on`] share one wiring definition; the
/// only difference between them is the transport passed to `connect_to`.
fn build_agent(
    engine: &Engine,
    state: &AgentState,
    skills: &Arc<[Skill]>,
) -> agent_client_protocol::Builder<
    Agent,
    impl agent_client_protocol::HandleDispatchFrom<Client>,
    agent_client_protocol::NullRun,
> {
    Agent
        .builder()
        .name("zhive")
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: InitializeRequest,
                            responder: Responder<InitializeResponse>,
                            _cx| {
                    state.set_client_capabilities(req.client_capabilities.clone());
                    tracing::info!(
                        name: "zhive.acp.initialize",
                        "initialize received; advertising agent capabilities"
                    );
                    responder.respond(
                        InitializeResponse::new(req.protocol_version)
                            .agent_capabilities(advertise_capabilities()),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                let engine = engine.clone();
                async move |req: NewSessionRequest,
                            responder: Responder<NewSessionResponse>,
                            _cx| {
                    let session_id = state.new_session(req.cwd.clone());
                    tracing::info!(
                        name: "zhive.acp.session.new",
                        session = %session_id.0,
                        "session/new bound to fresh thread"
                    );
                    // Surface model + reasoning-depth pickers to the client. The
                    // model list needs a (possibly networked) catalog fetch, so
                    // session creation pays that latency once up front.
                    let options = current_config_options(&engine, &state, &session_id).await;
                    responder.respond(NewSessionResponse::new(session_id).config_options(options))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                let engine = engine.clone();
                let skills = Arc::clone(skills);
                async move |req: PromptRequest,
                            responder: Responder<PromptResponse>,
                            cx: ConnectionTo<Client>| {
                    on_prompt(&state, &engine, &skills, req, responder, &cx)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                let engine = engine.clone();
                async move |req: SetSessionConfigOptionRequest,
                            responder: Responder<SetSessionConfigOptionResponse>,
                            _cx| {
                    on_set_config_option(&state, &engine, req, responder).await
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let state = state.clone();
                let engine = engine.clone();
                async move |notif: CancelNotification, cx: ConnectionTo<Client>| {
                    on_cancel(&state, &engine, &notif, &cx)
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<Client>| {
                message.respond_with_error(
                    agent_client_protocol::util::internal_error("unsupported ACP method"),
                    cx,
                )
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
}

/// Advertises the agent capabilities zhive supports in v1.
///
/// `load_session` is off (no storage replay yet). Model and reasoning-depth
/// pickers are advertised per session via `config_options` on the `session/new`
/// response (no capability flag is required for them), and refreshed through
/// `session/set_config_option`.
///
/// `prompt_capabilities.image = true` tells the client (Zed, pi, …) that the
/// agent accepts `ContentBlock::Image` in prompts; without this flag those
/// clients silently drop image attachments before sending.
fn advertise_capabilities() -> AgentCapabilities {
    use agent_client_protocol::schema::PromptCapabilities;
    AgentCapabilities::new()
        .load_session(false)
        .prompt_capabilities(PromptCapabilities::new().image(true))
}

/// Returns the session's current model + reasoning-depth config options.
///
/// Fetches the model catalog via `engine.list_models()` (degrading to an empty
/// list, i.e. no model selector, when the catalog is unavailable) and combines
/// it with the session's chosen reasoning depth. Shared by `session/new` and
/// `session/set_config_option` so both return a consistent, current view.
async fn current_config_options(
    engine: &Engine,
    state: &AgentState,
    session: &SessionId,
) -> Vec<agent_client_protocol::schema::SessionConfigOption> {
    let models = engine.list_models().await.unwrap_or_else(|err| {
        tracing::warn!(
            name: "zhive.acp.models.list_failed",
            error = %err,
            "list_models failed; omitting the model selector"
        );
        Vec::new()
    });
    config_option::build_config_options(&models, state.session_effort(session))
}

/// Handles `session/set_config_option`: switches model or reasoning depth.
///
/// `model` swaps the engine's active model (engine-global); `effort` records the
/// per-session reasoning depth applied to subsequent prompts. Either way the
/// response carries the full, refreshed option set (a model switch can change
/// the available depths), so the client re-renders both dropdowns.
async fn on_set_config_option(
    state: &AgentState,
    engine: &Engine,
    req: SetSessionConfigOptionRequest,
    responder: Responder<SetSessionConfigOptionResponse>,
) -> Result<(), agent_client_protocol::Error> {
    if state.thread_for_session(&req.session_id).is_none() {
        return responder
            .respond_with_internal_error(format!("unknown session: {}", req.session_id.0));
    }

    let value = req.value.0.as_ref();
    match req.config_id.0.as_ref() {
        config_option::CONFIG_MODEL_ID => {
            // The catalog resolves the context window itself, so no hint is
            // passed. A model switch affects the whole engine, not just this
            // session (the engine holds one active provider).
            if let Err(err) = engine.set_model(value, None) {
                tracing::warn!(
                    name: "zhive.acp.set_model.failed",
                    model = value,
                    error = %err,
                    "set_model failed"
                );
                return responder.respond_with_internal_error(format!("set_model failed: {err}"));
            }
        }
        config_option::CONFIG_EFFORT_ID => match ThinkingEffort::from_label(value) {
            Some(effort) => state.set_session_effort(&req.session_id, effort),
            None => {
                return responder
                    .respond_with_internal_error(format!("unknown reasoning depth: {value}"));
            }
        },
        other => {
            return responder
                .respond_with_internal_error(format!("unknown config option: {other}"));
        }
    }

    let options = current_config_options(engine, state, &req.session_id).await;
    responder.respond(SetSessionConfigOptionResponse::new(options))
}

/// Handles a `session/prompt` request by spawning the turn task.
///
/// If the prompt is a slash command (single text block starting with `/`), it
/// is dispatched to the appropriate slash handler instead of the LLM. All work
/// is offloaded to [`ConnectionTo::spawn`] so the dispatch loop never blocks:
/// slash handlers that need engine I/O (e.g. `/compact`) and the permission
/// reverse request both require `await` outside a handler context.
///
/// Returns an error to the request when the session is unknown.
fn on_prompt(
    state: &AgentState,
    engine: &Engine,
    skills: &Arc<[Skill]>,
    req: PromptRequest,
    responder: Responder<PromptResponse>,
    cx: &ConnectionTo<Client>,
) -> Result<(), agent_client_protocol::Error> {
    let Some(thread_id) = state.thread_for_session(&req.session_id) else {
        return responder
            .respond_with_internal_error(format!("unknown session: {}", req.session_id.0));
    };
    let engine = engine.clone();
    let state = state.clone();
    let skills = Arc::clone(skills);
    let cx = cx.clone();
    cx.clone().spawn(async move {
        match slash::parse_prompt(&req.prompt, &skills) {
            Some(slash::SlashAction::Compact) => {
                run_compact_slash(engine, cx, req.session_id, thread_id, responder).await;
            }
            Some(slash::SlashAction::Clear) => {
                run_clear_slash(&state, &cx, &req.session_id, responder);
            }
            Some(slash::SlashAction::ListSkills) => {
                run_list_skills_slash(&skills, &cx, &req.session_id, responder);
            }
            Some(slash::SlashAction::Help) => {
                run_help_slash(&cx, &req.session_id, responder);
            }
            Some(slash::SlashAction::RunSkill { invocation }) => {
                // Substitute the skill invocation for the user's raw prompt.
                use agent_client_protocol::schema::TextContent;
                let skill_prompt = vec![ContentBlock::Text(TextContent::new(invocation))];
                run_prompt_turn(
                    state,
                    engine,
                    cx,
                    req.session_id,
                    thread_id,
                    skill_prompt,
                    responder,
                )
                .await;
            }
            Some(slash::SlashAction::Unknown { cmd }) => {
                run_unknown_slash(&cmd, &cx, &req.session_id, responder);
            }
            None => {
                // Normal LLM turn.
                run_prompt_turn(
                    state,
                    engine,
                    cx,
                    req.session_id,
                    thread_id,
                    req.prompt,
                    responder,
                )
                .await;
            }
        }
        Ok(())
    })
}

// ── Slash-command handlers ────────────────────────────────────────────────────

/// Compacts the session's thread and streams progress as agent text chunks.
///
/// Subscribes to the engine event stream before calling `compact` (so no early
/// event is missed), then either handles the synchronous reply directly or
/// watches for the async `CompactionCompleted` / `CompactionFailed` events.
async fn run_compact_slash(
    engine: Engine,
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    thread_id: ThreadId,
    responder: Responder<PromptResponse>,
) {
    use zhive_core::engine::submission::{CompactError, CompactReply};
    use zhive_proto::hook::CompactTrigger;

    let mut events = engine.subscribe();

    let compact_result = engine
        .compact(thread_id.clone(), CompactTrigger::Manual)
        .await;

    match compact_result {
        Err(err) => {
            notify_text(&cx, &session_id, &format!("compact failed: {err}"));
        }
        Ok(Err(CompactError::EngineBusy { .. })) => {
            notify_text(&cx, &session_id, "compact failed: engine is busy");
        }
        Ok(Err(err)) => {
            notify_text(&cx, &session_id, &format!("compact failed: {err}"));
        }
        Ok(Ok(CompactReply::NothingToCompact)) => {
            notify_text(&cx, &session_id, "nothing to compact");
        }
        Ok(Ok(CompactReply::Compacted { entries_compacted })) => {
            notify_text(
                &cx,
                &session_id,
                &format!("compacted {entries_compacted} entries"),
            );
        }
        Ok(Ok(CompactReply::Started)) => {
            // The summarise phase is async; stream its output via events.
            loop {
                match events.recv().await {
                    Ok(EngineEvent::CompactionDelta {
                        thread_id: ev_thread,
                        delta,
                    }) if ev_thread == thread_id => {
                        notify_text(&cx, &session_id, &delta);
                    }
                    Ok(EngineEvent::CompactionCompleted {
                        thread_id: ev_thread,
                        entries_compacted,
                    }) if ev_thread == thread_id => {
                        notify_text(
                            &cx,
                            &session_id,
                            &format!("\n\nCompacted {entries_compacted} entries."),
                        );
                        break;
                    }
                    Ok(EngineEvent::CompactionFailed {
                        thread_id: ev_thread,
                        reason,
                    }) if ev_thread == thread_id => {
                        notify_text(&cx, &session_id, &format!("compaction failed: {reason}"));
                        break;
                    }
                    Err(RecvError::Closed) => break,
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                }
            }
        }
        // `CompactReply` is `#[non_exhaustive]`; unknown future variants are treated
        // as a successful synchronous compaction with no detail to report.
        Ok(Ok(_)) => {}
    }
    respond_stop(responder, StopReason::EndTurn);
}

/// Rebinds the session to a fresh thread (clears history) and notifies the client.
fn run_clear_slash(
    state: &AgentState,
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    responder: Responder<PromptResponse>,
) {
    if state.rebind_session(session_id).is_some() {
        notify_text(
            cx,
            session_id,
            "Session cleared. Starting a fresh conversation.",
        );
        tracing::info!(
            name: "zhive.acp.slash.clear",
            session = %session_id.0,
            "slash.clear: session rebound to a fresh thread"
        );
    } else {
        notify_text(cx, session_id, "No session to clear.");
    }
    respond_stop(responder, StopReason::EndTurn);
}

/// Lists all available skills as an agent text notification.
fn run_list_skills_slash(
    skills: &[Skill],
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    responder: Responder<PromptResponse>,
) {
    let text = if skills.is_empty() {
        "No skills installed.".to_owned()
    } else {
        use std::fmt::Write as _;
        let mut out = String::from("Available skills:\n");
        for skill in skills {
            let _ = writeln!(out, "  /{}", skill.name);
        }
        out
    };
    notify_text(cx, session_id, &text);
    respond_stop(responder, StopReason::EndTurn);
}

/// Sends the list of supported slash commands as an agent text notification.
fn run_help_slash(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    responder: Responder<PromptResponse>,
) {
    let text = "Available slash commands:\n\
        /compact    — compress conversation history\n\
        /new        — start a fresh conversation\n\
        /clear      — alias for /new\n\
        /skills     — list installed skills\n\
        /help       — show this message\n\
        /<name>     — run a skill by name (e.g. /commit)\n\
        /skill:<name> — same, explicit prefix\n";
    notify_text(cx, session_id, text);
    respond_stop(responder, StopReason::EndTurn);
}

/// Notifies the client that a slash command is unknown.
fn run_unknown_slash(
    cmd: &str,
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    responder: Responder<PromptResponse>,
) {
    notify_text(cx, session_id, &format!("unknown command: /{cmd}"));
    respond_stop(responder, StopReason::EndTurn);
}

/// Handles a `session/cancel` notification by cancelling the bound thread.
///
/// Fire-and-forget: the in-flight prompt task observes the engine abort and
/// returns [`StopReason::Cancelled`]. Unknown sessions are ignored.
fn on_cancel(
    state: &AgentState,
    engine: &Engine,
    notif: &CancelNotification,
    cx: &ConnectionTo<Client>,
) -> Result<(), agent_client_protocol::Error> {
    let Some(thread_id) = state.thread_for_session(&notif.session_id) else {
        return Ok(());
    };
    let engine = engine.clone();
    cx.spawn(async move {
        if let Err(err) = engine.cancel_turn(thread_id).await {
            tracing::warn!(
                name: "zhive.acp.cancel.failed",
                error = %err,
                "cancel_turn failed"
            );
        }
        Ok(())
    })
}

/// Drives one prompt turn end-to-end inside a spawned connection task.
///
/// Subscribes to the engine **before** starting the turn (so no early event is
/// missed), starts the turn, then streams matching [`EngineEvent`]s as
/// `session/update` notifications until a terminal event, finally responding to
/// the original `session/prompt` request with the mapped [`StopReason`].
///
/// When the turn resolves with [`StopReason::Cancelled`] due to a
/// `SessionAborted` engine event, the session entry is removed from `state` so
/// the bridge does not hold a stale reference to a dead thread.
async fn run_prompt_turn(
    state: AgentState,
    engine: Engine,
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    thread_id: ThreadId,
    prompt: Vec<ContentBlock>,
    responder: Responder<PromptResponse>,
) {
    let mut events = engine.subscribe();
    let user_item = convert::prompt_blocks_to_user_item(prompt, format!("item:{}/0", thread_id.0));

    // Apply the session's chosen reasoning depth (set via
    // `session/set_config_option`); `None` keeps the engine default.
    let reasoning = state.session_effort(&session_id);
    let turn_id = match engine
        .start_turn_with_reasoning(thread_id.clone(), vec![user_item], None, reasoning)
        .await
    {
        Ok(turn_id) => turn_id,
        Err(err) => {
            // EngineBusy or an actor-level failure both surface as a refusal:
            // the turn never ran, so there is nothing for the client to render.
            tracing::warn!(
                name: "zhive.acp.prompt.start_failed",
                error = %err,
                "start_turn failed; responding Refusal"
            );
            respond_stop(responder, StopReason::Refusal);
            return;
        }
    };

    let stop = stream_turn(
        &state,
        &engine,
        &cx,
        &session_id,
        &thread_id,
        &turn_id,
        &mut events,
    )
    .await;
    respond_stop(responder, stop);
}

/// Responds to the prompt request with `stop`, logging a send failure.
fn respond_stop(responder: Responder<PromptResponse>, stop: StopReason) {
    if let Err(err) = responder.respond(PromptResponse::new(stop)) {
        tracing::debug!(
            name: "zhive.acp.prompt.respond_failed",
            error = %err,
            "failed to send prompt response"
        );
    }
}

/// Streams a single turn's events to the client and returns its [`StopReason`].
///
/// Filters the broadcast to events for `(thread_id, turn_id)`, handles the
/// permission reverse request inline, and returns on the first terminal event.
///
/// On `SessionAborted` the session is removed from `state` because the engine
/// thread is gone and the session can never receive another turn. This is the
/// primary lifecycle-event cleanup path described in [`crate::state`].
///
/// Tool-call events follow the canonical ACP two-event lifecycle (matching pi
/// and opencode): the `InProgress` broadcast becomes `SessionUpdate::ToolCall`
/// (establishes the card, empty content), and the `Completed`/`Failed` item
/// becomes `SessionUpdate::ToolCallUpdate` (carries `content` + `raw_output`).
/// Clients (Zed, pi, …) render tool output from the update's `content` field.
async fn stream_turn(
    state: &AgentState,
    engine: &Engine,
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    thread_id: &ThreadId,
    turn_id: &TurnId,
    events: &mut Receiver<EngineEvent>,
) -> StopReason {
    let mut seen_tool_calls: std::collections::HashSet<ToolCallId> =
        std::collections::HashSet::new();
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    name: "zhive.acp.events.lagged",
                    skipped,
                    "engine event stream lagged; some updates dropped"
                );
                continue;
            }
            Err(RecvError::Closed) => {
                tracing::warn!(
                    name: "zhive.acp.events.closed",
                    "engine event stream closed before turn completed"
                );
                return StopReason::EndTurn;
            }
        };

        match event {
            EngineEvent::ItemDelta {
                thread_id: ev_thread,
                turn_id: ev_turn,
                delta,
                kind,
            } if &ev_thread == thread_id && &ev_turn == turn_id => {
                notify(
                    cx,
                    session_id,
                    convert::delta_to_session_update(delta, kind),
                );
            }
            EngineEvent::ItemAppended {
                thread_id: ev_thread,
                turn_id: ev_turn,
                item,
            } if &ev_thread == thread_id && &ev_turn == turn_id => {
                // Tool calls: two-event lifecycle via seen_tool_calls.
                // First sighting (InProgress) → ToolCall (card header, empty content).
                // Second sighting (Completed/Failed) → ToolCallUpdate (content + raw_output).
                // All other items are routed through item_to_session_update.
                let update = if matches!(item.as_ref(), zhive_proto::domain::Item::ToolCall { .. })
                {
                    convert::tool_call_session_update(item.as_ref(), &mut seen_tool_calls)
                } else {
                    convert::item_to_session_update(item.as_ref())
                };
                if let Some(update) = update {
                    notify(cx, session_id, update);
                }
            }
            EngineEvent::PermissionRequested {
                request_id,
                request,
            } if request.thread_id == *thread_id => {
                if let PermissionFlow::Cancelled =
                    handle_permission(engine, cx, session_id, thread_id, request_id, &request).await
                {
                    // The session was aborted while we were waiting for the
                    // client's permission answer. The engine already released
                    // the pending permission and emitted `SessionAborted`, so
                    // settle the turn as cancelled. The `SessionAborted` arm
                    // below handles removal; if we return early here the abort
                    // event was already consumed by `handle_permission`'s inner
                    // subscriber, so remove explicitly to avoid leaving a stale
                    // entry.
                    state.remove_session(session_id);
                    tracing::info!(
                        name: "zhive.acp.session.removed",
                        session = %session_id.0,
                        "session removed after abort during permission wait"
                    );
                    return StopReason::Cancelled;
                }
            }
            EngineEvent::TurnCompleted {
                thread_id: ev_thread,
                turn_id: ev_turn,
            } if &ev_thread == thread_id && &ev_turn == turn_id => {
                return StopReason::EndTurn;
            }
            EngineEvent::TurnFailed {
                thread_id: ev_thread,
                turn_id: ev_turn,
                error,
            } if &ev_thread == thread_id && &ev_turn == turn_id => {
                // A `TurnFailed` is a transient provider / stream error, NOT a
                // content refusal: the engine keeps the transcript. Mapping it to
                // `Refusal` would tell the client to drop the user prompt from
                // history, which is wrong. Surface the failure text to the user,
                // then end the turn with the transcript preserved.
                notify(
                    cx,
                    session_id,
                    convert::delta_to_session_update(
                        format!("turn failed: {}", error.message),
                        zhive_proto::events::ItemDeltaKind::Text,
                    ),
                );
                return StopReason::EndTurn;
            }
            EngineEvent::SessionAborted(notif) if notif.thread_id == *thread_id => {
                // The engine thread is gone; the session can never accept
                // another turn. Remove it from the bridge state now so the
                // entry does not accumulate in long-running daemons.
                state.remove_session(session_id);
                tracing::info!(
                    name: "zhive.acp.session.removed",
                    session = %session_id.0,
                    "session removed on SessionAborted"
                );
                return StopReason::Cancelled;
            }
            _ => {}
        }
    }
}

/// Outcome of the permission round trip, telling [`stream_turn`] how to proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionFlow {
    /// The client answered (or the request failed) and the engine was resumed;
    /// the turn continues streaming its remaining events.
    Resumed,
    /// The session was aborted while awaiting the answer; the turn must settle
    /// as [`StopReason::Cancelled`].
    Cancelled,
}

/// Performs the permission reverse-request round trip and resumes the engine.
///
/// Sends `session/request_permission`, then races the client's answer against a
/// session abort observed on the engine event stream. This race is the safety
/// net: a client is only *required* to answer outstanding permission requests
/// when it sends `session/cancel`, but the spec merely says it *should*, so a
/// client may cancel without answering. Without the race the bridge would block
/// forever on [`block_task`](agent_client_protocol::SentRequest::block_task) and
/// the turn would deadlock.
///
/// On the answer branch the outcome is mapped and the engine resumed, returning
/// [`PermissionFlow::Resumed`]. On the abort branch the engine has already
/// released the pending permission via `cancel_all`, but the bridge still calls
/// [`Engine::resume_permission`] with [`PermissionOutcome::Cancelled`] so the
/// slot is unconditionally released (idempotent if already gone), then returns
/// [`PermissionFlow::Cancelled`]. A failure to send the request, a dropped
/// client answer, or a closed event stream all also resolve as `Cancelled`
/// outcome / flow so a never-answered request cannot hang.
///
/// The abort is watched on a *fresh* engine subscription taken before awaiting
/// the client, so the caller's event receiver position is untouched and no
/// turn event is consumed on the answer path. The `PermissionRequested` event
/// was already drained by [`stream_turn`], so any `SessionAborted` for this
/// thread necessarily arrives after this subscription and cannot be missed.
async fn handle_permission(
    engine: &Engine,
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    thread_id: &ThreadId,
    request_id: PermissionRequestId,
    request: &ZRequestPermissionRequest,
) -> PermissionFlow {
    let mut abort_events = engine.subscribe();
    let acp_request = permission::build_request(session_id, request);
    let response = cx.send_request(acp_request).block_task();
    tokio::pin!(response);

    let (outcome, flow) = tokio::select! {
        // Bias the abort arm so a cancel that arrives together with a late
        // answer is still honoured as a cancellation.
        biased;
        () = wait_for_session_abort(thread_id, &mut abort_events) => {
            tracing::info!(
                name: "zhive.acp.permission.cancelled",
                thread = %thread_id.0,
                "session aborted while awaiting permission; cancelling"
            );
            (PermissionOutcome::Cancelled, PermissionFlow::Cancelled)
        }
        result = &mut response => match result {
            Ok(response) => (permission::outcome_to_engine(response.outcome), PermissionFlow::Resumed),
            Err(err) => {
                tracing::warn!(
                    name: "zhive.acp.permission.request_failed",
                    error = %err,
                    "request_permission failed; cancelling pending permission"
                );
                (PermissionOutcome::Cancelled, PermissionFlow::Resumed)
            }
        },
    };

    if let Err(err) = engine.resume_permission(request_id, outcome).await {
        tracing::warn!(
            name: "zhive.acp.permission.resume_failed",
            error = %err,
            "resume_permission failed"
        );
    }
    flow
}

/// Resolves when a [`EngineEvent::SessionAborted`] for `thread_id` is observed.
///
/// Drains `abort_events` until the abort (or a closed stream) arrives. A closed
/// stream is treated as an abort: the engine is gone, so the only safe action
/// is to stop waiting on the permission answer. Non-matching events are skipped;
/// a lagged stream is tolerated (the abort is terminal and will still be seen,
/// or the stream closes). This is the cancellation arm of [`handle_permission`].
async fn wait_for_session_abort(thread_id: &ThreadId, abort_events: &mut Receiver<EngineEvent>) {
    loop {
        match abort_events.recv().await {
            Ok(EngineEvent::SessionAborted(notif)) if notif.thread_id == *thread_id => return,
            // A non-matching event or a lagged stream both mean "keep waiting":
            // the abort is terminal, so it will still arrive (or the stream
            // closes). A closed stream means the engine is gone — stop waiting.
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
        }
    }
}

/// Sends a `session/update` notification, logging (but not failing) on error.
///
/// A send failure means the connection is already gone; the turn loop will
/// observe the closed event stream and exit, so logging is sufficient here.
fn notify(cx: &ConnectionTo<Client>, session_id: &SessionId, update: SessionUpdate) {
    if let Err(err) = cx.send_notification(SessionNotification::new(session_id.clone(), update)) {
        tracing::debug!(
            name: "zhive.acp.update.send_failed",
            error = %err,
            "failed to send session/update"
        );
    }
}

/// Sends a plain-text agent message chunk as a `session/update` notification.
fn notify_text(cx: &ConnectionTo<Client>, session_id: &SessionId, text: &str) {
    use agent_client_protocol::schema::{ContentChunk, TextContent};
    notify(
        cx,
        session_id,
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text.to_owned(),
        )))),
    );
}

// Rust guideline compliant 2026-02-21

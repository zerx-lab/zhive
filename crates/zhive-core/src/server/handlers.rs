//! JSON-RPC method handlers that bridge the wire to the engine actor.
//!
//! Each handler in this module implements [`super::router::Handler`]
//! and dispatches a JSON-RPC request to a matching method on
//! [`crate::engine::Engine`]. Use [`register_engine_handlers`] to wire
//! the canonical Phase 1 method set into a [`super::router::Router`] in
//! one call.
//!
//! ## Method catalogue (Phase 1)
//!
//! * `engine/start_turn` — calls [`crate::engine::Engine::start_turn`]
//! * `engine/cancel_turn` — calls [`crate::engine::Engine::cancel_turn`]
//! * `engine/resume_permission` — calls
//!   [`crate::engine::Engine::resume_permission`]
//! * `engine/compact` — calls [`crate::engine::Engine::compact`]
//! * `thread/fork` — calls [`crate::engine::Engine::fork_thread`]
//! * `thread/list` — calls [`crate::engine::Engine::list_threads`]
//! * `engine/resume_thread` — calls [`crate::engine::Engine::resume_thread`]
//! * `thread/get_items` — calls [`crate::engine::Engine::get_items`]
//! * `engine/shutdown` — calls [`crate::engine::Engine::shutdown`]
//!
//! Method names are intentionally namespaced under `engine/` so the
//! router can later host hook / extension / observability methods
//! without collisions.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use zhive_proto::ErrorObject;
use zhive_proto::methods;
use zhive_proto::permission::{PermissionOutcome, ResumePermissionParams};
use zhive_proto::rpc::{
    CancelTurnParams, CancelTurnResult, CompactParams, CompactResult, CompactStatus, ForkParams,
    ForkResult, GetItemsParams, GetItemsResult, InjectionAck, InjectionParams, ListThreadsParams,
    ListThreadsResult, ResumePermissionResult, ResumePermissionStatus, ResumeThreadParams,
    ResumeThreadResult, SessionCancelParams, StartTurnParams, StartTurnResult,
};

use crate::engine::{
    Engine, EngineError, PermissionRequestId,
    submission::{
        CompactError, CompactReply, ForkError, ForkReply, GetItemsError, ResumeError,
        ResumePermissionReply, ResumeReply, Submission,
    },
};
use zhive_proto::permission::StreamingBehavior;

use super::router::{Handler, JsonRpcCode, Router};

/// JSON-RPC error code reserved for engine-level domain errors (busy,
/// unknown thread, etc.). Picked from the JSON-RPC "server error"
/// range (-32000 to -32099).
pub const ENGINE_ERROR_CODE: i64 = -32000;

/// Registers the Phase 1 engine method set into `router`.
///
/// Subsequent calls to [`Router::register`] for the same method name
/// silently overwrite, so callers that want custom dispatchers should
/// invoke this first and then register their overrides.
pub fn register_engine_handlers(router: &mut Router, engine: Engine) {
    let engine = Arc::new(engine);
    router.register(
        methods::METHOD_START_TURN,
        Arc::new(StartTurnHandler {
            engine: Arc::clone(&engine),
        }),
    );
    router.register(
        methods::METHOD_CANCEL_TURN,
        Arc::new(CancelTurnHandler {
            engine: Arc::clone(&engine),
        }),
    );
    router.register(
        methods::METHOD_RESUME_PERMISSION_LEGACY,
        Arc::new(ResumePermissionHandler {
            engine: Arc::clone(&engine),
        }),
    );
    // Canonical ACP-style alias for the same handler. The deferred-permission
    // suspend/resume wire surface names this method
    // ([`zhive_proto::permission::METHOD_RESUME_PERMISSION`]); the legacy
    // `engine/resume_permission` is kept registered so existing clients keep
    // working. Both route to the same handler.
    router.register(
        methods::METHOD_RESUME_PERMISSION,
        Arc::new(ResumePermissionHandler {
            engine: Arc::clone(&engine),
        }),
    );
    router.register(
        methods::METHOD_COMPACT,
        Arc::new(CompactHandler {
            engine: Arc::clone(&engine),
        }),
    );
    router.register(
        methods::METHOD_THREAD_FORK,
        Arc::new(ForkHandler {
            engine: Arc::clone(&engine),
        }),
    );
    // History / resume surface (recent-session list, resume, item fetch).
    router.register(
        methods::METHOD_THREAD_LIST,
        Arc::new(ListThreadsHandler {
            engine: Arc::clone(&engine),
        }),
    );
    router.register(
        methods::METHOD_RESUME_THREAD,
        Arc::new(ResumeThreadHandler {
            engine: Arc::clone(&engine),
        }),
    );
    router.register(
        methods::METHOD_THREAD_GET_ITEMS,
        Arc::new(GetItemsHandler {
            engine: Arc::clone(&engine),
        }),
    );
    // Injection-queue methods (Pi `streamingBehavior` model). These back the
    // `streaming.{steer,followUp,nextTurn}` capability advertised in
    // `server::initialize`; without them a client reading the capability would
    // get -32601 (method not found) — a silent contract violation.
    router.register(
        methods::METHOD_ENQUEUE_STEER,
        Arc::new(EnqueueSteerHandler {
            engine: Arc::clone(&engine),
        }),
    );
    router.register(
        methods::METHOD_ENQUEUE_FOLLOW_UP,
        Arc::new(EnqueueFollowUpHandler {
            engine: Arc::clone(&engine),
        }),
    );
    router.register(
        methods::METHOD_ENQUEUE_NEXT_TURN,
        Arc::new(EnqueueNextTurnHandler {
            engine: Arc::clone(&engine),
        }),
    );
    router.register(
        methods::METHOD_SHUTDOWN,
        Arc::new(ShutdownHandler {
            engine: Arc::clone(&engine),
        }),
    );
    // ACP session/cancel notification handler (B7).
    // Receives a client-sent `session/cancel` notification and maps it to
    // `engine.cancel_turn(thread_id)`.  The handler is registered here so
    // the server does not silently drop the notification once clients start
    // sending `Client::cancel_session`.  For notifications the router
    // dispatches but discards the returned value; an `Err` is only logged.
    router.register(
        methods::METHOD_SESSION_CANCEL,
        Arc::new(SessionCancelHandler { engine }),
    );
}

// ============================================================
// Handlers
// ============================================================

struct StartTurnHandler {
    engine: Arc<Engine>,
}

#[async_trait]
impl Handler for StartTurnHandler {
    async fn handle(&self, params: Option<Value>) -> Result<Value, ErrorObject> {
        let params: StartTurnParams = decode_params(params)?;
        match self
            .engine
            .start_turn(params.thread_id, params.user_input, params.scope)
            .await
        {
            Ok(turn_id) => {
                // `serde_json::to_value` cannot fail at runtime on
                // this fully-typed struct; fall back to `Null` to
                // honour CLAUDE.md's no-`expect()` rule.
                Ok(serde_json::to_value(StartTurnResult::new(turn_id)).unwrap_or(Value::Null))
            }
            Err(err) => Err(engine_error(&err)),
        }
    }
}

struct CancelTurnHandler {
    engine: Arc<Engine>,
}

#[async_trait]
impl Handler for CancelTurnHandler {
    async fn handle(&self, params: Option<Value>) -> Result<Value, ErrorObject> {
        let params: CancelTurnParams = decode_params(params)?;
        match self.engine.cancel_turn(params.thread_id).await {
            Ok(turn_id) => {
                Ok(serde_json::to_value(CancelTurnResult::new(turn_id)).unwrap_or(Value::Null))
            }
            Err(err) => Err(engine_error(&err)),
        }
    }
}

struct ResumePermissionHandler {
    engine: Arc<Engine>,
}

#[async_trait]
impl Handler for ResumePermissionHandler {
    async fn handle(&self, params: Option<Value>) -> Result<Value, ErrorObject> {
        let params: ResumePermissionParams = decode_params(params)?;
        // The proto carries the request id as a plain string and a narrower
        // `ResumeOutcome` (no `Defer`); lift both into the engine's types. The
        // `From<ResumeOutcome>` conversion is total and can never produce
        // `PermissionOutcome::Defer`, so a resumed request can never re-suspend.
        let request_id = PermissionRequestId(Arc::from(params.request_id.as_str()));
        let outcome: PermissionOutcome = params.outcome.into();
        match self.engine.resume_permission(request_id, outcome).await {
            Ok(reply) => {
                let status = match reply {
                    ResumePermissionReply::Resolved => ResumePermissionStatus::Resolved,
                    ResumePermissionReply::UnknownRequest => ResumePermissionStatus::UnknownRequest,
                    ResumePermissionReply::InvalidRequestId => {
                        ResumePermissionStatus::InvalidRequestId
                    }
                    ResumePermissionReply::Abandoned => ResumePermissionStatus::Abandoned,
                };
                // `serde_json::to_value` on a fixed struct cannot
                // fail at runtime, but CLAUDE.md forbids `.expect()`
                // in library code; fall back to a sentinel Null so
                // the call still completes if the impossible occurs.
                Ok(
                    serde_json::to_value(ResumePermissionResult::new(status))
                        .unwrap_or(Value::Null),
                )
            }
            Err(err) => Err(engine_error(&err)),
        }
    }
}

struct CompactHandler {
    engine: Arc<Engine>,
}

#[async_trait]
impl Handler for CompactHandler {
    async fn handle(&self, params: Option<Value>) -> Result<Value, ErrorObject> {
        let params: CompactParams = decode_params(params)?;
        match self.engine.compact(params.thread_id, params.trigger).await {
            Ok(Ok(reply)) => {
                let result = match reply {
                    CompactReply::Compacted { entries_compacted } => {
                        CompactResult::new(CompactStatus::Compacted, entries_compacted)
                    }
                    CompactReply::NothingToCompact => {
                        CompactResult::new(CompactStatus::NothingToCompact, 0)
                    }
                };
                // A fully-typed struct cannot fail to serialise; fall back to
                // Null rather than `.expect()` (CLAUDE.md no-expect rule).
                Ok(serde_json::to_value(result).unwrap_or(Value::Null))
            }
            Ok(Err(domain)) => Err(compact_error(&domain)),
            Err(err) => Err(engine_error(&err)),
        }
    }
}

struct ForkHandler {
    engine: Arc<Engine>,
}

#[async_trait]
impl Handler for ForkHandler {
    async fn handle(&self, params: Option<Value>) -> Result<Value, ErrorObject> {
        let params: ForkParams = decode_params(params)?;
        match self
            .engine
            .fork_thread(params.source_thread_id, params.up_to_item, params.summarize)
            .await
        {
            Ok(Ok(ForkReply::Forked {
                new_thread_id,
                items_replayed,
                summarized,
            })) => {
                let result = ForkResult::new(new_thread_id, items_replayed, summarized);
                // A fully-typed struct cannot fail to serialise; fall back to
                // Null rather than `.expect()` (CLAUDE.md no-expect rule).
                Ok(serde_json::to_value(result).unwrap_or(Value::Null))
            }
            Ok(Err(domain)) => Err(fork_error(&domain)),
            Err(err) => Err(engine_error(&err)),
        }
    }
}

struct ListThreadsHandler {
    engine: Arc<Engine>,
}

#[async_trait]
impl Handler for ListThreadsHandler {
    async fn handle(&self, params: Option<Value>) -> Result<Value, ErrorObject> {
        // `thread/list` params are optional: a missing/null body lists every
        // thread, a `{ cwd }` body scopes the listing to that directory.
        let cwd = match params {
            Some(value) => {
                let parsed: ListThreadsParams = decode_params(Some(value))?;
                parsed.cwd
            }
            None => None,
        };
        match self.engine.list_threads(cwd.as_deref()).await {
            Ok(threads) => {
                // A fully-typed struct cannot fail to serialise; fall back to
                // Null rather than `.expect()` (CLAUDE.md no-expect rule).
                Ok(serde_json::to_value(ListThreadsResult::new(threads)).unwrap_or(Value::Null))
            }
            Err(err) => Err(engine_error(&err)),
        }
    }
}

struct ResumeThreadHandler {
    engine: Arc<Engine>,
}

#[async_trait]
impl Handler for ResumeThreadHandler {
    async fn handle(&self, params: Option<Value>) -> Result<Value, ErrorObject> {
        let params: ResumeThreadParams = decode_params(params)?;
        match self.engine.resume_thread(params.thread_id).await {
            Ok(Ok(ResumeReply {
                thread_id,
                items_restored,
                turns_restored,
            })) => {
                let result = ResumeThreadResult::new(thread_id, items_restored, turns_restored);
                Ok(serde_json::to_value(result).unwrap_or(Value::Null))
            }
            Ok(Err(domain)) => Err(resume_error(&domain)),
            Err(err) => Err(engine_error(&err)),
        }
    }
}

struct GetItemsHandler {
    engine: Arc<Engine>,
}

#[async_trait]
impl Handler for GetItemsHandler {
    async fn handle(&self, params: Option<Value>) -> Result<Value, ErrorObject> {
        let params: GetItemsParams = decode_params(params)?;
        match self
            .engine
            .get_items(
                params.thread_id,
                params.turn_id,
                params.offset,
                params.limit,
            )
            .await
        {
            Ok(Ok(items)) => {
                Ok(serde_json::to_value(GetItemsResult::new(items)).unwrap_or(Value::Null))
            }
            Ok(Err(domain)) => Err(get_items_error(&domain)),
            Err(err) => Err(engine_error(&err)),
        }
    }
}

struct ShutdownHandler {
    engine: Arc<Engine>,
}

#[async_trait]
impl Handler for ShutdownHandler {
    async fn handle(&self, _params: Option<Value>) -> Result<Value, ErrorObject> {
        match self.engine.shutdown().await {
            Ok(()) => Ok(Value::Null),
            Err(err) => Err(engine_error(&err)),
        }
    }
}

/// Handler for the ACP `session/cancel` notification.
///
/// Maps an inbound `session/cancel { threadId }` notification to
/// `engine.cancel_turn(thread_id)`, mirroring the semantics of the
/// `engine/cancel_turn` RPC but on the ACP notification channel.
///
/// The handler is registered by [`register_engine_handlers`] so
/// that [`zhive_client_native::Client::cancel_session`] works end-to-end
/// without any protocol-level configuration.  For the notification dispatch
/// path the router discards the returned [`Value`]; only an `Err` is logged.
struct SessionCancelHandler {
    engine: Arc<Engine>,
}

#[async_trait]
impl Handler for SessionCancelHandler {
    async fn handle(&self, params: Option<Value>) -> Result<Value, ErrorObject> {
        let p: SessionCancelParams = decode_params(params)?;
        // `cancel_turn` returns Option<TurnId>; both Some and None are
        // success cases for a notification (no active turn is idempotent).
        match self.engine.cancel_turn(p.thread_id).await {
            Ok(_turn_id) => Ok(Value::Null),
            Err(err) => Err(engine_error(&err)),
        }
    }
}

// ============================================================
// Injection-queue handlers (Pi `streamingBehavior` model)
// ============================================================

struct EnqueueSteerHandler {
    engine: Arc<Engine>,
}

#[async_trait]
impl Handler for EnqueueSteerHandler {
    async fn handle(&self, params: Option<Value>) -> Result<Value, ErrorObject> {
        let p: InjectionParams = decode_params(params)?;
        match self
            .engine
            .submit(Submission::EnqueueInjection {
                thread_id: p.thread_id,
                behavior: StreamingBehavior::Steer,
                items: p.items,
            })
            .await
        {
            Ok(()) => Ok(injection_ack()),
            Err(err) => Err(engine_error(&err)),
        }
    }
}

struct EnqueueFollowUpHandler {
    engine: Arc<Engine>,
}

#[async_trait]
impl Handler for EnqueueFollowUpHandler {
    async fn handle(&self, params: Option<Value>) -> Result<Value, ErrorObject> {
        let p: InjectionParams = decode_params(params)?;
        match self
            .engine
            .submit(Submission::EnqueueInjection {
                thread_id: p.thread_id,
                behavior: StreamingBehavior::FollowUp,
                items: p.items,
            })
            .await
        {
            Ok(()) => Ok(injection_ack()),
            Err(err) => Err(engine_error(&err)),
        }
    }
}

struct EnqueueNextTurnHandler {
    engine: Arc<Engine>,
}

#[async_trait]
impl Handler for EnqueueNextTurnHandler {
    async fn handle(&self, params: Option<Value>) -> Result<Value, ErrorObject> {
        let p: InjectionParams = decode_params(params)?;
        match self
            .engine
            .submit(Submission::EnqueueNextTurn {
                thread_id: p.thread_id,
                items: p.items,
            })
            .await
        {
            Ok(()) => Ok(injection_ack()),
            Err(err) => Err(engine_error(&err)),
        }
    }
}

// ============================================================
// Helpers
// ============================================================

fn decode_params<T: for<'de> Deserialize<'de>>(params: Option<Value>) -> Result<T, ErrorObject> {
    let value = params.unwrap_or(Value::Null);
    serde_json::from_value(value).map_err(|e| ErrorObject {
        code: JsonRpcCode::InvalidParams.as_i64(),
        message: JsonRpcCode::InvalidParams.message().to_string(),
        data: Some(Value::String(e.to_string())),
    })
}

/// Emits the canonical `{ "accepted": true }` ack for fire-and-forget injection methods.
///
/// Uses [`InjectionAck::accepted`] so the wire shape is owned by proto and
/// the `serde` round-trip is type-safe.
fn injection_ack() -> Value {
    serde_json::to_value(InjectionAck::accepted()).unwrap_or(Value::Null)
}

fn engine_error(err: &EngineError) -> ErrorObject {
    let message = err.to_string();
    let data = match err {
        EngineError::EngineBusy { current } => Some(serde_json::json!({
            "kind": "engine_busy",
            "currentPhase": current,
        })),
        EngineError::ActorStopped => Some(serde_json::json!({ "kind": "actor_stopped" })),
        EngineError::ReplyDropped => Some(serde_json::json!({ "kind": "reply_dropped" })),
        EngineError::ReplyTimedOut(d) => Some(serde_json::json!({
            "kind": "reply_timed_out",
            "timeoutSecs": d.as_secs(),
        })),
        EngineError::SubagentSpawnFailed(reason) => Some(serde_json::json!({
            "kind": "subagent_spawn_failed",
            "reason": reason.to_string(),
        })),
    };
    ErrorObject {
        code: ENGINE_ERROR_CODE,
        message,
        data,
    }
}

/// Maps a [`CompactError`] to a wire error carrying a `kind` discriminator.
///
/// Uses the same [`ENGINE_ERROR_CODE`] and `data.kind` convention as
/// [`engine_error`] so clients fold both into one handler.
fn compact_error(err: &CompactError) -> ErrorObject {
    let message = err.to_string();
    let data = match err {
        CompactError::ThreadNotFound => Some(serde_json::json!({ "kind": "thread_not_found" })),
        CompactError::EngineBusy { current } => Some(serde_json::json!({
            "kind": "engine_busy",
            "currentPhase": current,
        })),
        CompactError::SummarizationFailed { message } => Some(serde_json::json!({
            "kind": "summarization_failed",
            "reason": message,
        })),
        CompactError::BlockedByHook { reason } => Some(serde_json::json!({
            "kind": "blocked_by_hook",
            "reason": reason,
        })),
    };
    ErrorObject {
        code: ENGINE_ERROR_CODE,
        message,
        data,
    }
}

/// Maps a [`ForkError`] to a wire error carrying a `kind` discriminator.
///
/// Uses the same [`ENGINE_ERROR_CODE`] and `data.kind` convention as
/// [`engine_error`] / [`compact_error`].
fn fork_error(err: &ForkError) -> ErrorObject {
    let message = err.to_string();
    let data = match err {
        ForkError::SourceNotFound => Some(serde_json::json!({ "kind": "source_not_found" })),
        ForkError::EngineBusy { current } => Some(serde_json::json!({
            "kind": "engine_busy",
            "currentPhase": current,
        })),
        ForkError::ReplayFailed { message } => Some(serde_json::json!({
            "kind": "replay_failed",
            "reason": message,
        })),
        ForkError::SummarizationFailed { message } => Some(serde_json::json!({
            "kind": "summarization_failed",
            "reason": message,
        })),
    };
    ErrorObject {
        code: ENGINE_ERROR_CODE,
        message,
        data,
    }
}

/// Maps a [`ResumeError`] to a wire error carrying a `kind` discriminator.
///
/// Uses the same [`ENGINE_ERROR_CODE`] and `data.kind` convention as the other
/// domain-error mappers.
fn resume_error(err: &ResumeError) -> ErrorObject {
    let message = err.to_string();
    let data = match err {
        ResumeError::StorageUnavailable => {
            Some(serde_json::json!({ "kind": "storage_unavailable" }))
        }
        ResumeError::ThreadNotFound => Some(serde_json::json!({ "kind": "thread_not_found" })),
        ResumeError::EngineBusy { current } => Some(serde_json::json!({
            "kind": "engine_busy",
            "currentPhase": current,
        })),
        ResumeError::ReplayFailed { message } => Some(serde_json::json!({
            "kind": "replay_failed",
            "reason": message,
        })),
    };
    ErrorObject {
        code: ENGINE_ERROR_CODE,
        message,
        data,
    }
}

/// Maps a [`GetItemsError`] to a wire error carrying a `kind` discriminator.
fn get_items_error(err: &GetItemsError) -> ErrorObject {
    let message = err.to_string();
    let data = match err {
        GetItemsError::StorageUnavailable => {
            Some(serde_json::json!({ "kind": "storage_unavailable" }))
        }
        GetItemsError::ReadFailed { message } => Some(serde_json::json!({
            "kind": "read_failed",
            "reason": message,
        })),
    };
    ErrorObject {
        code: ENGINE_ERROR_CODE,
        message,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn start_turn_handler_returns_turn_id() {
        let engine = Engine::spawn();
        let mut router = Router::new();
        register_engine_handlers(&mut router, engine.clone());

        let params = serde_json::json!({
            "threadId": "thread:native/handler",
            "userInput": [],
            "scope": null,
        });
        let v = router
            .dispatch("engine/start_turn", Some(params))
            .await
            .expect("dispatch ok");
        assert!(v.get("turnId").is_some());

        engine.shutdown().await.unwrap();
    }

    /// The injection-queue methods backing the advertised
    /// `streaming.{steer,followUp,nextTurn}` capability must be registered:
    /// a missing handler would surface as -32601 (a silent contract breach).
    #[tokio::test]
    async fn injection_handlers_are_registered_and_ack() {
        let engine = Engine::spawn();
        let mut router = Router::new();
        register_engine_handlers(&mut router, engine.clone());

        for method in [
            "session/enqueue_steer",
            "session/enqueue_follow_up",
            "session/enqueue_next_turn",
        ] {
            let params = serde_json::json!({
                "threadId": "thread:native/inject",
                "items": [],
            });
            let v = router
                .dispatch(method, Some(params))
                .await
                .unwrap_or_else(|e| panic!("{method} must be registered, got {e:?}"));
            assert_eq!(v["accepted"], true, "{method} must acknowledge");
        }

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_turn_handler_on_missing_thread_returns_null_turn_id() {
        let engine = Engine::spawn();
        let mut router = Router::new();
        register_engine_handlers(&mut router, engine.clone());

        let v = router
            .dispatch(
                "engine/cancel_turn",
                Some(serde_json::json!({"threadId": "thread:native/missing"})),
            )
            .await
            .expect("dispatch ok");
        assert!(v.get("turnId").is_some());
        assert!(v["turnId"].is_null());

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_params_surface_invalid_params_error() {
        let engine = Engine::spawn();
        let mut router = Router::new();
        register_engine_handlers(&mut router, engine.clone());

        let err = router
            .dispatch("engine/start_turn", Some(serde_json::json!({})))
            .await
            .expect_err("missing threadId");
        assert_eq!(err.code, JsonRpcCode::InvalidParams.as_i64());

        engine.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn resume_permission_handler_reports_unknown_request() {
        let engine = Engine::spawn();
        let mut router = Router::new();
        register_engine_handlers(&mut router, engine.clone());

        let params = serde_json::json!({
            "requestId": "perm:9999",
            "outcome": { "outcome": "cancelled" },
        });
        let v = router
            .dispatch("engine/resume_permission", Some(params))
            .await
            .expect("dispatch ok");
        assert_eq!(v["status"], "unknown_request");
        engine.shutdown().await.unwrap();
    }

    /// Both the legacy `engine/resume_permission` and the canonical
    /// `session/resume_permission` alias must route to the same handler.
    #[tokio::test]
    async fn resume_permission_alias_routes_to_same_handler() {
        let engine = Engine::spawn();
        let mut router = Router::new();
        register_engine_handlers(&mut router, engine.clone());

        for method in [
            "engine/resume_permission",
            zhive_proto::permission::METHOD_RESUME_PERMISSION,
        ] {
            let params = serde_json::json!({
                "requestId": "perm:9999",
                "outcome": { "outcome": "selected", "optionId": "allow_once" },
            });
            let v = router
                .dispatch(method, Some(params))
                .await
                .unwrap_or_else(|e| panic!("{method} must be registered, got {e:?}"));
            // No such pending request → unknown_request from both routes.
            assert_eq!(v["status"], "unknown_request", "{method} routed");
        }
        engine.shutdown().await.unwrap();
    }

    /// Verifies the `engine_error` mapping carries the `engine_busy`
    /// discriminator that clients key off.
    #[test]
    fn engine_busy_maps_to_kind_engine_busy() {
        use zhive_proto::hook::EnginePhase;
        let err = engine_error(&EngineError::EngineBusy {
            current: EnginePhase::Compaction,
        });
        assert_eq!(err.code, ENGINE_ERROR_CODE);
        assert_eq!(err.data.as_ref().unwrap()["kind"], "engine_busy");
        assert_eq!(err.data.as_ref().unwrap()["currentPhase"], "compaction");
    }

    /// Compacting a thread that never ran a turn surfaces the
    /// `thread_not_found` engine error rather than a success result.
    #[tokio::test]
    async fn compact_handler_unknown_thread_reports_thread_not_found() {
        let engine = Engine::spawn();
        let mut router = Router::new();
        register_engine_handlers(&mut router, engine.clone());

        let err = router
            .dispatch(
                "engine/compact",
                Some(serde_json::json!({"threadId": "thread:native/never"})),
            )
            .await
            .expect_err("compact on missing thread is an error");
        assert_eq!(err.code, ENGINE_ERROR_CODE);
        assert_eq!(err.data.as_ref().unwrap()["kind"], "thread_not_found");

        engine.shutdown().await.unwrap();
    }

    /// `thread/fork` is registered; on an in-memory engine (no storage) it
    /// surfaces the `source_not_found` fork error rather than -32601.
    #[tokio::test]
    async fn fork_handler_without_storage_reports_source_not_found() {
        let engine = Engine::spawn();
        let mut router = Router::new();
        register_engine_handlers(&mut router, engine.clone());

        let err = router
            .dispatch(
                "thread/fork",
                Some(serde_json::json!({"sourceThreadId": "thread:native/src"})),
            )
            .await
            .expect_err("fork on an engine without storage is an error");
        assert_eq!(err.code, ENGINE_ERROR_CODE);
        assert_eq!(err.data.as_ref().unwrap()["kind"], "source_not_found");

        engine.shutdown().await.unwrap();
    }

    /// `thread/list` is registered and returns an empty `threads` array on an
    /// in-memory engine (no persisted sessions, but never -32601).
    #[tokio::test]
    async fn list_threads_handler_returns_empty_on_in_memory_engine() {
        let engine = Engine::spawn();
        let mut router = Router::new();
        register_engine_handlers(&mut router, engine.clone());

        let v = router
            .dispatch("thread/list", None)
            .await
            .expect("thread/list must be registered");
        assert!(v["threads"].is_array());
        assert_eq!(v["threads"].as_array().unwrap().len(), 0);

        engine.shutdown().await.unwrap();
    }

    /// `engine/resume_thread` on an in-memory engine surfaces the
    /// `storage_unavailable` engine error rather than -32601.
    #[tokio::test]
    async fn resume_thread_handler_without_storage_reports_storage_unavailable() {
        let engine = Engine::spawn();
        let mut router = Router::new();
        register_engine_handlers(&mut router, engine.clone());

        let err = router
            .dispatch(
                "engine/resume_thread",
                Some(serde_json::json!({"threadId": "thread:native/x"})),
            )
            .await
            .expect_err("resume on an engine without storage is an error");
        assert_eq!(err.code, ENGINE_ERROR_CODE);
        assert_eq!(err.data.as_ref().unwrap()["kind"], "storage_unavailable");

        engine.shutdown().await.unwrap();
    }

    /// `thread/get_items` is registered; on an in-memory engine it surfaces the
    /// `storage_unavailable` error rather than -32601.
    #[tokio::test]
    async fn get_items_handler_without_storage_reports_storage_unavailable() {
        let engine = Engine::spawn();
        let mut router = Router::new();
        register_engine_handlers(&mut router, engine.clone());

        let err = router
            .dispatch(
                "thread/get_items",
                Some(serde_json::json!({"threadId": "thread:native/x"})),
            )
            .await
            .expect_err("get_items on an engine without storage is an error");
        assert_eq!(err.code, ENGINE_ERROR_CODE);
        assert_eq!(err.data.as_ref().unwrap()["kind"], "storage_unavailable");

        engine.shutdown().await.unwrap();
    }

    /// Confirms a missing handler still returns `MethodNotFound` (sanity
    /// check that `register_engine_handlers` did not blanket-register).
    #[tokio::test]
    async fn unregistered_method_returns_method_not_found() {
        let engine = Engine::spawn();
        let mut router = Router::new();
        register_engine_handlers(&mut router, engine.clone());

        let err = router
            .dispatch("engine/nonexistent", None)
            .await
            .expect_err("unknown method");
        assert_eq!(err.code, JsonRpcCode::MethodNotFound.as_i64());

        engine.shutdown().await.unwrap();
    }
}

// Rust guideline compliant 2026-02-21

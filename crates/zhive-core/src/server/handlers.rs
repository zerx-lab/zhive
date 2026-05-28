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
//! * `engine/shutdown` — calls [`crate::engine::Engine::shutdown`]
//!
//! Method names are intentionally namespaced under `engine/` so the
//! router can later host hook / extension / observability methods
//! without collisions.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zhive_proto::ErrorObject;
use zhive_proto::domain::{Item, ThreadId, TurnId};
use zhive_proto::permission::{PermissionOutcome, PermissionScope};

use crate::engine::{Engine, EngineError, PermissionRequestId, submission::ResumePermissionReply};

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
        "engine/start_turn",
        Arc::new(StartTurnHandler {
            engine: Arc::clone(&engine),
        }),
    );
    router.register(
        "engine/cancel_turn",
        Arc::new(CancelTurnHandler {
            engine: Arc::clone(&engine),
        }),
    );
    router.register(
        "engine/resume_permission",
        Arc::new(ResumePermissionHandler {
            engine: Arc::clone(&engine),
        }),
    );
    router.register("engine/shutdown", Arc::new(ShutdownHandler { engine }));
}

// ============================================================
// Wire payloads
// ============================================================

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartTurnParams {
    thread_id: ThreadId,
    #[serde(default)]
    user_input: Vec<Item>,
    #[serde(default)]
    scope: Option<PermissionScope>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartTurnResult {
    turn_id: TurnId,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelTurnParams {
    thread_id: ThreadId,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CancelTurnResult {
    /// `Some(id)` when the cancel hit an active turn, `None` otherwise.
    turn_id: Option<TurnId>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResumePermissionParams {
    request_id: PermissionRequestId,
    outcome: PermissionOutcome,
}

/// Wire-form classifier for the resume-permission outcome.
///
/// Mirrors [`ResumePermissionReply`] 1:1 so a future reducer variant
/// has to be reflected here too (the match in
/// [`ResumePermissionHandler::handle`] is exhaustive against this
/// enum, not `#[non_exhaustive]`).
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ResumePermissionStatus {
    Resolved,
    UnknownRequest,
    InvalidRequestId,
    Abandoned,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ResumePermissionResult {
    /// Wire-form copy of the reducer's typed reply.
    status: ResumePermissionStatus,
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
                Ok(serde_json::to_value(StartTurnResult { turn_id }).unwrap_or(Value::Null))
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
                Ok(serde_json::to_value(CancelTurnResult { turn_id }).unwrap_or(Value::Null))
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
        match self
            .engine
            .resume_permission(params.request_id, params.outcome)
            .await
        {
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
                Ok(serde_json::to_value(ResumePermissionResult { status }).unwrap_or(Value::Null))
            }
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

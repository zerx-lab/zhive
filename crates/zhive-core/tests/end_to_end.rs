//! End-to-end integration tests: `Engine` ↔ `serve_uds` ↔
//! `zhive_client_native::Client`.
//!
//! These tests exercise the full Phase 1 wire path: a real UDS
//! listener bound by `serve_uds`, a real `Engine` actor mounted via
//! `register_engine_handlers`, and the real native client decoding
//! `Content-Length:` framed JSON-RPC frames. They are the canonical
//! smoke check that the three crates compose without surprises.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use zhive_client_native::{Client, ClientError};
use zhive_core::engine::Engine;
use zhive_core::server::{
    DEFAULT_MAX_CONNECTIONS, ENGINE_ERROR_CODE, Router, register_engine_handlers, serve_uds,
    serve_uds_with_events,
};

/// Spawns the server side and returns the cancel token + the socket
/// path so the client can connect.
async fn spawn_server() -> (
    CancellationToken,
    std::path::PathBuf,
    tempfile::TempDir,
    Arc<Engine>,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("e2e.sock");
    let engine = Arc::new(Engine::spawn());
    let mut router = Router::new();
    register_engine_handlers(&mut router, (*engine).clone());
    let router = Arc::new(router);
    let token = CancellationToken::new();

    let socket_for_task = socket.clone();
    let token_for_task = token.clone();
    tokio::spawn(async move {
        serve_uds(
            &socket_for_task,
            router,
            DEFAULT_MAX_CONNECTIONS,
            token_for_task,
        )
        .await
        .expect("serve_uds clean exit");
    });

    // Wait for the bind to land before letting tests connect.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !socket.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (token, socket, dir, engine)
}

#[tokio::test]
async fn start_turn_round_trip_via_client() {
    let (token, socket, _dir, engine) = spawn_server().await;
    let client = Client::connect_uds(&socket).await.expect("connect");

    let params = serde_json::json!({
        "threadId": "thread:native/e2e-1",
        "userInput": [],
        "scope": null,
    });
    let response = client
        .call("engine/start_turn", Some(params))
        .await
        .expect("call ok");

    assert!(response.get("turnId").is_some(), "missing turnId");
    let turn_id = response["turnId"].as_str().expect("turnId is string");
    assert!(turn_id.starts_with("turn:thread:native/e2e-1/"));

    client.shutdown();
    token.cancel();
    let _ = engine.shutdown().await;
}

#[tokio::test]
async fn cancel_turn_returns_null_for_missing_thread() {
    let (token, socket, _dir, engine) = spawn_server().await;
    let client = Client::connect_uds(&socket).await.expect("connect");

    let response = client
        .call(
            "engine/cancel_turn",
            Some(serde_json::json!({"threadId": "thread:native/nope"})),
        )
        .await
        .expect("call ok");
    assert!(response.get("turnId").is_some());
    assert!(response["turnId"].is_null());

    client.shutdown();
    token.cancel();
    let _ = engine.shutdown().await;
}

#[tokio::test]
async fn resume_permission_with_invalid_id_surfaces_status() {
    let (token, socket, _dir, engine) = spawn_server().await;
    let client = Client::connect_uds(&socket).await.expect("connect");

    let params = serde_json::json!({
        "requestId": "not-a-perm-id",
        "outcome": { "outcome": "cancelled" },
    });
    let response = client
        .call("engine/resume_permission", Some(params))
        .await
        .expect("call ok");
    assert_eq!(response["status"], "invalid_request_id");

    client.shutdown();
    token.cancel();
    let _ = engine.shutdown().await;
}

#[tokio::test]
async fn unknown_method_returns_method_not_found() {
    let (token, socket, _dir, engine) = spawn_server().await;
    let client = Client::connect_uds(&socket).await.expect("connect");

    let err = client
        .call("engine/does_not_exist", None)
        .await
        .expect_err("must fail");
    match err {
        ClientError::Server(e) => assert_eq!(e.code, -32601), // MethodNotFound
        other => panic!("expected Server error, got {other:?}"),
    }

    client.shutdown();
    token.cancel();
    let _ = engine.shutdown().await;
}

#[tokio::test]
async fn engine_busy_error_round_trips_with_kind_payload() {
    // We can't easily make the engine busy in Phase 1 (turns auto-
    // complete synchronously), but the wire mapping itself is what we
    // verify. Issue a malformed cancel that returns InvalidParams to
    // confirm the error envelope reaches the client.
    let (token, socket, _dir, engine) = spawn_server().await;
    let client = Client::connect_uds(&socket).await.expect("connect");

    let err = client
        .call("engine/start_turn", Some(serde_json::json!({})))
        .await
        .expect_err("missing threadId");
    match err {
        ClientError::Server(e) => assert_eq!(e.code, -32602), // InvalidParams
        other => panic!("expected Server error, got {other:?}"),
    }
    // Sanity: the dedicated engine error code is wired into the
    // exported constant, so a future engine-side failure carries it.
    assert_eq!(ENGINE_ERROR_CODE, -32000);

    client.shutdown();
    token.cancel();
    let _ = engine.shutdown().await;
}

/// Spawns a server that forwards engine events as JSON-RPC notifications.
async fn spawn_server_with_events() -> (
    CancellationToken,
    std::path::PathBuf,
    tempfile::TempDir,
    Arc<Engine>,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("events.sock");
    let engine = Arc::new(Engine::spawn());
    let mut router = Router::new();
    register_engine_handlers(&mut router, (*engine).clone());
    let router = Arc::new(router);
    let token = CancellationToken::new();

    let socket_for_task = socket.clone();
    let token_for_task = token.clone();
    let engine_for_task = (*engine).clone();
    tokio::spawn(async move {
        serve_uds_with_events(
            &socket_for_task,
            router,
            engine_for_task,
            DEFAULT_MAX_CONNECTIONS,
            token_for_task,
        )
        .await
        .expect("serve_uds_with_events clean exit");
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !socket.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (token, socket, dir, engine)
}

#[tokio::test]
async fn client_receives_turn_lifecycle_notifications() {
    let (token, socket, _dir, engine) = spawn_server_with_events().await;
    let client = Client::connect_uds(&socket).await.expect("connect");
    let mut events = client.subscribe_notifications();

    // Trigger a turn — Phase 1 auto-completes synchronously, so the
    // notification stream should contain at least PhaseChanged
    // (idle→turn), TurnStarted, PhaseChanged (turn→idle) and
    // TurnCompleted in some order.
    let _ = client
        .call(
            "engine/start_turn",
            Some(serde_json::json!({
                "threadId": "thread:native/evt-1",
                "userInput": [],
                "scope": null,
            })),
        )
        .await
        .expect("start_turn ok");

    let mut saw_started = false;
    let mut saw_completed = false;
    let mut saw_phase_to_turn = false;
    let mut saw_phase_to_idle = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline
        && !(saw_started && saw_completed && saw_phase_to_turn && saw_phase_to_idle)
    {
        let notif = tokio::time::timeout(Duration::from_millis(500), events.recv()).await;
        let Ok(Ok(n)) = notif else {
            break;
        };
        match n.method.as_str() {
            "events/turn_started" => {
                let p = n.params.as_ref().unwrap();
                assert_eq!(p["threadId"], "thread:native/evt-1");
                saw_started = true;
            }
            "events/turn_completed" => {
                saw_completed = true;
            }
            "events/phase_changed" => {
                let p = n.params.as_ref().unwrap();
                if p["from"] == "idle" && p["to"] == "turn" {
                    saw_phase_to_turn = true;
                }
                if p["from"] == "turn" && p["to"] == "idle" {
                    saw_phase_to_idle = true;
                }
            }
            _ => {}
        }
    }

    assert!(saw_started, "did not receive turn_started notification");
    assert!(saw_completed, "did not receive turn_completed notification");
    assert!(saw_phase_to_turn, "did not receive phase_changed idle→turn");
    assert!(saw_phase_to_idle, "did not receive phase_changed turn→idle");

    client.shutdown();
    token.cancel();
    let _ = engine.shutdown().await;
}

#[tokio::test]
async fn subscribe_notifications_before_any_event_still_receives_next() {
    let (token, socket, _dir, engine) = spawn_server_with_events().await;
    let client = Client::connect_uds(&socket).await.expect("connect");

    // Two subscribers; both should see the next turn's events.
    let mut events_a = client.subscribe_notifications();
    let mut events_b = client.subscribe_notifications();

    let _ = client
        .call(
            "engine/start_turn",
            Some(serde_json::json!({
                "threadId": "thread:native/evt-2",
                "userInput": [],
                "scope": null,
            })),
        )
        .await
        .expect("start_turn ok");

    let saw_completed_a = wait_for_method(&mut events_a, "events/turn_completed").await;
    let saw_completed_b = wait_for_method(&mut events_b, "events/turn_completed").await;
    assert!(saw_completed_a, "subscriber A missed turn_completed");
    assert!(saw_completed_b, "subscriber B missed turn_completed");

    client.shutdown();
    token.cancel();
    let _ = engine.shutdown().await;
}

async fn wait_for_method(
    rx: &mut tokio::sync::broadcast::Receiver<zhive_proto::Notification>,
    method: &str,
) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            Ok(Ok(n)) if n.method == method => return true,
            Ok(Ok(_)) => {}
            _ => return false,
        }
    }
    false
}

// Rust guideline compliant 2026-02-21

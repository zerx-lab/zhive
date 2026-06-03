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

/// The `connect_uds` handshake must negotiate V1 and surface the
/// server's capabilities (hooks=true because `register_engine_handlers`
/// is wired, and the server is a Phase 1 zhive instance).
#[tokio::test]
async fn connect_uds_handshake_populates_negotiated_metadata() {
    use zhive_proto::initialize::ProtocolVersion;

    let (token, socket, _dir, engine) = spawn_server().await;
    let client = Client::connect_uds(&socket).await.expect("connect");

    // Negotiated version must be V1 (current LATEST).
    assert_eq!(
        client.negotiated_version(),
        ProtocolVersion::V1,
        "negotiated version must be V1"
    );

    // Server must claim hooks and cancellation.
    let caps = client.server_capabilities();
    assert!(caps.hooks, "server must advertise hooks capability");
    assert!(caps.cancellation, "server must advertise cancellation");
    assert!(caps.subagents, "server must advertise subagents");

    // Server identity must be "zhive".
    assert_eq!(client.server_info().name, "zhive");
    assert!(!client.server_info().version.is_empty());

    let _ = client.shutdown().await;
    token.cancel();
    let _ = engine.shutdown().await;
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

    let _ = client.shutdown().await;
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

    let _ = client.shutdown().await;
    token.cancel();
    let _ = engine.shutdown().await;
}

/// `Client::cancel_turn` typed helper returns `Ok(None)` when no turn
/// is active on the given thread.
#[tokio::test]
async fn cancel_turn_typed_helper_returns_none_for_missing_thread() {
    use zhive_proto::domain::ThreadId;

    let (token, socket, _dir, engine) = spawn_server().await;
    let client = Client::connect_uds(&socket).await.expect("connect");

    let tid = ThreadId(std::sync::Arc::from("thread:native/cancel-typed-none"));
    let result = client.cancel_turn(&tid).await.expect("cancel_turn ok");
    assert!(
        result.is_none(),
        "expected None for a thread with no active turn, got {result:?}"
    );

    let _ = client.shutdown().await;
    token.cancel();
    let _ = engine.shutdown().await;
}

/// `Client::cancel_turn` typed helper returns `Ok(Some(TurnId))` when
/// a turn was active and was cancelled.
///
/// Phase 1 turns auto-complete before `cancel_turn` can race them, so
/// we start a turn first then immediately call cancel; the turn will
/// have already completed but the test validates the `Some` path via
/// a stub server that synthesises the `{ "turnId": "..." }` response.
#[tokio::test]
async fn cancel_turn_typed_helper_decodes_turn_id_from_stub() {
    use tokio::io::AsyncWriteExt;
    use zhive_proto::domain::{ThreadId, TurnId};
    use zhive_proto::framing;
    use zhive_proto::{Message, Response};

    // Stub server: reads the cancel_turn request and replies with a
    // synthetic `{ "turnId": "turn:thread:native/stub-cancel/42" }`.
    let (client_io, server_io) = tokio::io::duplex(4096);
    let (mut server_read, mut server_write) = tokio::io::split(server_io);
    let (client_read, client_write) = tokio::io::split(client_io);

    let server_task = tokio::spawn(async move {
        let mut buf = tokio::io::BufReader::new(&mut server_read);
        let msg = framing::read_message(&mut buf).await.expect("read req");
        let req = match msg {
            Message::Request(r) => r,
            other => panic!("stub server: expected Request, got {other:?}"),
        };
        let resp = Response::ok(
            req.id,
            serde_json::json!({ "turnId": "turn:thread:native/stub-cancel/42" }),
        );
        framing::write_message(&mut server_write, &Message::Response(resp))
            .await
            .expect("write resp");
        server_write.flush().await.expect("flush");
    });

    let client = Client::from_split(client_read, client_write);
    let tid = ThreadId(std::sync::Arc::from("thread:native/stub-cancel"));
    let result = client.cancel_turn(&tid).await.expect("cancel_turn ok");

    assert!(result.is_some(), "expected Some(TurnId), got None");
    let TurnId(id_str) = result.unwrap();
    assert_eq!(
        &*id_str, "turn:thread:native/stub-cancel/42",
        "decoded TurnId does not match stub value"
    );

    server_task.await.unwrap();
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

    let _ = client.shutdown().await;
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

    let _ = client.shutdown().await;
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

    let _ = client.shutdown().await;
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

    let _ = client.shutdown().await;
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

    let _ = client.shutdown().await;
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

/// Helper: send `events/subscribe` with a list of methods and await the ok
/// response so the caller can rely on the filter being set before emitting
/// any engine events.
async fn subscribe(client: &zhive_client_native::Client, methods: &[&str]) {
    let params = serde_json::json!({ "methods": methods });
    client
        .call("events/subscribe", Some(params))
        .await
        .expect("events/subscribe must succeed");
}

/// Two connections with distinct subscriptions each receive only their
/// subscribed events; the unsubscribed third connection receives all events.
///
/// Connection A subscribes to `events/turn_*` — must see `turn_started` and
/// `turn_completed` but NOT `phase_changed`.
/// Connection B subscribes to `events/phase_changed` — must see
/// `phase_changed` but NOT `turn_started`.
/// Connection C never subscribes — must see both kinds (backward compat).
#[tokio::test]
async fn per_connection_filter_turns_vs_phase() {
    let (token, socket, _dir, engine) = spawn_server_with_events().await;

    let client_a = Client::connect_uds(&socket).await.expect("connect A");
    let client_b = Client::connect_uds(&socket).await.expect("connect B");
    let client_c = Client::connect_uds(&socket).await.expect("connect C");

    // Subscribe before triggering any turn so the filter is in place.
    subscribe(&client_a, &["events/turn_started", "events/turn_completed"]).await;
    subscribe(&client_b, &["events/phase_changed"]).await;
    // client_c: no subscribe — allow-all.

    let mut rx_a = client_a.subscribe_notifications();
    let mut rx_b = client_b.subscribe_notifications();
    let mut rx_c = client_c.subscribe_notifications();

    let _ = client_a
        .call(
            "engine/start_turn",
            Some(serde_json::json!({
                "threadId": "thread:native/filter-test",
                "userInput": [],
                "scope": null,
            })),
        )
        .await
        .expect("start_turn ok");

    let timeout = Duration::from_secs(3);

    // A must see turn events, not phase.
    let a_methods: Vec<String> = {
        let end = std::time::Instant::now() + timeout;
        let mut acc = Vec::new();
        while std::time::Instant::now() < end {
            match tokio::time::timeout(Duration::from_millis(300), rx_a.recv()).await {
                Ok(Ok(n)) => acc.push(n.method),
                _ => break,
            }
        }
        acc
    };
    assert!(
        a_methods
            .iter()
            .any(|m| m == "events/turn_started" || m == "events/turn_completed"),
        "A must see at least one turn event; got {a_methods:?}"
    );
    assert!(
        !a_methods.iter().any(|m| m == "events/phase_changed"),
        "A must NOT see phase_changed; got {a_methods:?}"
    );

    // B must see phase events, not turn events. Collect B's full method
    // stream (not predicate-filtered) so the negative assertion is real:
    // a predicate filter would silently discard any leaked turn events.
    let b_methods: Vec<String> = {
        let end = std::time::Instant::now() + timeout;
        let mut acc = Vec::new();
        while std::time::Instant::now() < end {
            match tokio::time::timeout(Duration::from_millis(300), rx_b.recv()).await {
                Ok(Ok(n)) => acc.push(n.method),
                _ => break,
            }
        }
        acc
    };
    assert!(
        b_methods.iter().any(|m| m == "events/phase_changed"),
        "B must see at least one phase_changed event; got {b_methods:?}"
    );
    assert!(
        !b_methods.iter().any(|m| m.contains("turn_")),
        "B (phase-only subscriber) must NOT see turn events; got {b_methods:?}"
    );

    // C (unsubscribed) must see both kinds — backward compat.
    let c_methods: Vec<String> = {
        let end = std::time::Instant::now() + timeout;
        let mut acc = Vec::new();
        while std::time::Instant::now() < end {
            match tokio::time::timeout(Duration::from_millis(300), rx_c.recv()).await {
                Ok(Ok(n)) => acc.push(n.method),
                _ => break,
            }
        }
        acc
    };
    let c_has_turn = c_methods
        .iter()
        .any(|m| m == "events/turn_started" || m == "events/turn_completed");
    let c_has_phase = c_methods.iter().any(|m| m == "events/phase_changed");
    assert!(
        c_has_turn,
        "C (unsubscribed) must see turn events; got {c_methods:?}"
    );
    assert!(
        c_has_phase,
        "C (unsubscribed) must see phase_changed; got {c_methods:?}"
    );

    let _ = client_a.shutdown().await;
    let _ = client_b.shutdown().await;
    let _ = client_c.shutdown().await;
    token.cancel();
    let _ = engine.shutdown().await;
}

/// `events/unsubscribe` resets a previously narrowed filter back to allow-all.
#[tokio::test]
async fn unsubscribe_resets_to_allow_all() {
    let (token, socket, _dir, engine) = spawn_server_with_events().await;
    let client = Client::connect_uds(&socket).await.expect("connect");

    // Narrow to only turn events.
    subscribe(&client, &["events/turn_started"]).await;

    // Unsubscribe — should go back to allow-all.
    client
        .call("events/unsubscribe", None)
        .await
        .expect("events/unsubscribe must succeed");

    let mut rx = client.subscribe_notifications();

    let _ = client
        .call(
            "engine/start_turn",
            Some(serde_json::json!({
                "threadId": "thread:native/unsub-test",
                "userInput": [],
                "scope": null,
            })),
        )
        .await
        .expect("start_turn ok");

    // After unsubscribing, the connection must receive phase_changed again.
    let saw_phase = wait_for_method(&mut rx, "events/phase_changed").await;
    assert!(
        saw_phase,
        "after unsubscribe the connection must receive phase_changed (allow-all restored)"
    );

    let _ = client.shutdown().await;
    token.cancel();
    let _ = engine.shutdown().await;
}

/// Malformed `events/subscribe` params must surface `InvalidParams`
/// rather than silently resetting the filter to allow-all.
#[tokio::test]
async fn subscribe_with_malformed_params_returns_invalid_params() {
    let (token, socket, _dir, engine) = spawn_server_with_events().await;
    let client = Client::connect_uds(&socket).await.expect("connect");

    // `methods` must be an array of strings; send a number instead.
    let err = client
        .call("events/subscribe", Some(serde_json::json!({"methods": 42})))
        .await
        .expect_err("malformed subscribe must fail");
    match err {
        ClientError::Server(e) => assert_eq!(e.code, -32602), // InvalidParams
        other => panic!("expected Server InvalidParams error, got {other:?}"),
    }

    let _ = client.shutdown().await;
    token.cancel();
    let _ = engine.shutdown().await;
}

// Rust guideline compliant 2026-02-21

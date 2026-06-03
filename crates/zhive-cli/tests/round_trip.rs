//! End-to-end offline verification of the TUI's engine round-trip.
//!
//! Drives the *exact* path `zhive tui` uses minus the terminal: spawn the engine
//! with a deterministic [`ScriptedModel`], serve it over UDS, connect the native
//! client, submit a turn, then fold the resulting `events/*` notifications
//! through `zhive_tui::protocol::decode` and `zhive_tui::conversation` — the same
//! decode + reduce the running UI performs. No API key or network is needed.

#![cfg(all(feature = "tui", feature = "serve"))]

use std::sync::Arc;
use std::time::Duration;

use llmsdk::language_model::StreamPart;
use tokio_util::sync::CancellationToken;
use zhive_client_native::{Client, ClientEvent};
use zhive_core::engine::Engine;
use zhive_core::provider::ScriptedModel;
use zhive_core::server::{
    DEFAULT_MAX_CONNECTIONS, Router, register_engine_handlers, serve_uds_with_events,
};
use zhive_proto::domain::{Item, ItemContent};
use zhive_tui::conversation::Conversation;
use zhive_tui::protocol::{EngineNotification, decode};

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "a linear end-to-end scenario reads best as one function"
)]
async fn user_turn_round_trips_into_conversation() {
    // A scripted model that streams one fixed agent message.
    let parts = vec![
        StreamPart::TextStart {
            id: "b0".into(),
            provider_metadata: None,
        },
        StreamPart::TextDelta {
            id: "b0".into(),
            delta: "hello from zap".into(),
            provider_metadata: None,
        },
        StreamPart::TextEnd {
            id: "b0".into(),
            provider_metadata: None,
        },
    ];
    let model = ScriptedModel::new("scripted", "test", parts).into_dyn();
    let engine = Engine::spawn_with_provider(model);

    let mut router = Router::new();
    register_engine_handlers(&mut router, engine.clone());
    let router = Arc::new(router);

    let token = CancellationToken::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("round_trip.sock");

    let serve_socket = socket.clone();
    let serve_engine = engine.clone();
    let serve_token = token.clone();
    tokio::spawn(async move {
        let _ = serve_uds_with_events(
            &serve_socket,
            router,
            serve_engine,
            DEFAULT_MAX_CONNECTIONS,
            serve_token,
        )
        .await;
    });

    // Wait for the socket to appear before connecting.
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let client = Client::connect_uds(&socket).await.expect("connect");
    let mut events = client.subscribe_events();

    let thread = zhive_tui::id::new_thread_id();
    // Drive the real send path so `Item::UserMessage` serialization is covered.
    zhive_tui::rpc::start_turn(&client, &thread, "hi")
        .await
        .expect("start_turn accepted");

    // Fold notifications into a conversation until the turn finishes.
    let mut conv = Conversation::new(thread.clone());
    let mut saw_delta = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let next = tokio::time::timeout_at(deadline, events.next_event())
            .await
            .expect("an event before the deadline");
        match next {
            Some(ClientEvent::Notification(n)) => {
                let decoded = decode(&n.method, n.params);
                if matches!(decoded, EngineNotification::ItemDelta { .. }) {
                    saw_delta = true;
                }
                let done = matches!(
                    decoded,
                    EngineNotification::TurnCompleted { .. }
                        | EngineNotification::TurnFailed { .. }
                );
                conv.apply(&decoded);
                if done {
                    break;
                }
            }
            Some(ClientEvent::Disconnected { .. }) | None => break,
            _ => {}
        }
    }
    assert!(
        saw_delta,
        "the engine should stream at least one token delta"
    );

    let texts: Vec<String> = conv
        .turns
        .iter()
        .flat_map(|t| t.items.iter())
        .filter_map(|item| match item {
            Item::AgentMessage { text, .. } => Some(text.clone()),
            Item::UserMessage { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|c| match c {
                        ItemContent::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect();

    assert!(
        texts.iter().any(|t| t.contains("hi")),
        "user message should round-trip: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("hello from zap")),
        "scripted agent reply should arrive: {texts:?}"
    );
    assert!(!conv.busy, "a completed turn clears the busy flag");

    let _ = client.shutdown().await;
    token.cancel();
    let _ = engine.shutdown().await;
}

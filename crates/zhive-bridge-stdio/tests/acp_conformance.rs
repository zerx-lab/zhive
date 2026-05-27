//! ACP-style minimum conformance harness for `zhive-bridge-stdio`.
//!
//! Per D-010 (R3+R4 终版), Phase 1 must demonstrate that the byte-pump
//! bridge faithfully transports an ACP-shape handshake without breaking
//! frame boundaries:
//!
//! * `initialize` handshake
//! * `session/new`
//! * `session/prompt`
//! * 3 `session/update` notifications (user / agent / `tool_call` payloads)
//! * `session/cancel`
//!
//! The test does NOT speak the real ACP semantics -- per D-005 the bridge
//! crate has no schema dependency on `agent-client-protocol`. It only
//! verifies that bytes in == bytes out, in order, with `Content-Length`
//! boundaries preserved across both directions.

use std::io::Cursor;

use serde_json::json;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use zhive_proto::{Id, Message, Notification, Request, Response, framing};

/// Six-message ACP-shape conversation that exercises every shape the host
/// is expected to handle in Phase 1.
fn build_host_requests() -> Vec<Message> {
    vec![
        Message::Request(Request::new(
            Id::Number(1),
            "initialize",
            Some(json!({ "protocolVersion": 1 })),
        )),
        Message::Request(Request::new(
            Id::Number(2),
            "session/new",
            Some(json!({ "cwd": "/tmp/work" })),
        )),
        Message::Request(Request::new(
            Id::Number(3),
            "session/prompt",
            Some(json!({ "prompt": "hello" })),
        )),
        // The host normally only sends requests; these are here to verify
        // the bridge does not care about the message *kind* it ferries.
        Message::Notification(Notification::new(
            "session/cancel",
            Some(json!({ "sessionId": "s-1" })),
        )),
        Message::Request(Request::new(
            Id::Number(4),
            "session/prompt",
            Some(json!({ "prompt": "follow-up after cancel" })),
        )),
        Message::Notification(Notification::new(
            "$/log",
            Some(json!({ "level": "debug", "text": "tail" })),
        )),
    ]
}

/// Six-message reply set the fake engine sends back: an initialise ack,
/// session id, three `session/update` events (user / agent / `tool_call`),
/// and a final prompt result.
fn build_engine_replies() -> Vec<Message> {
    vec![
        Message::Response(Response::ok(
            Id::Number(1),
            json!({ "protocolVersion": 1, "agentName": "zhive-test" }),
        )),
        Message::Response(Response::ok(Id::Number(2), json!({ "sessionId": "s-1" }))),
        Message::Notification(Notification::new(
            "session/update",
            Some(json!({
                "sessionId": "s-1",
                "update": { "kind": "user_message_chunk", "content": "hello" }
            })),
        )),
        Message::Notification(Notification::new(
            "session/update",
            Some(json!({
                "sessionId": "s-1",
                "update": { "kind": "agent_message_chunk", "content": "hi there" }
            })),
        )),
        Message::Notification(Notification::new(
            "session/update",
            Some(json!({
                "sessionId": "s-1",
                "update": {
                    "kind": "tool_call",
                    "toolCallId": "tc-1",
                    "title": "ls",
                    "kindHint": "execute"
                }
            })),
        )),
        Message::Response(Response::ok(
            Id::Number(3),
            json!({ "stopReason": "end_turn" }),
        )),
    ]
}

#[tokio::test]
async fn byte_pump_preserves_acp_shape_round_trip() {
    // Step 1 / 4: bring up a fake engine on a temp UDS path.
    let tmp = tempfile::TempDir::new().expect("tmp dir");
    let sock = tmp.path().join("zhive.sock");
    let listener = UnixListener::bind(&sock).expect("bind uds");

    let engine = tokio::spawn(async move {
        let (conn, _) = listener.accept().await.expect("accept");
        let (read_half, mut write_half) = conn.into_split();
        let mut reader = BufReader::new(read_half);

        // Read all six host messages in order, verify methods.
        let host_msgs = build_host_requests();
        for expected in &host_msgs {
            let got = framing::read_message(&mut reader)
                .await
                .expect("engine reads host frame");
            assert_eq!(&got, expected, "host frame mismatch");
        }

        // Reply with six engine messages.
        for reply in build_engine_replies() {
            framing::write_message(&mut write_half, &reply)
                .await
                .expect("engine writes reply");
        }

        // Half-close so the bridge's downstream half can finish.
        write_half.shutdown().await.expect("engine shutdown");
    });

    // Step 2 / 4: pre-serialise the host's six outbound frames into a
    // byte buffer that doubles as the bridge's "stdin".
    let mut host_stdin: Vec<u8> = Vec::new();
    for msg in build_host_requests() {
        framing::write_message(&mut host_stdin, &msg)
            .await
            .expect("serialise host frame");
    }

    // Step 3 / 4: drive the bridge.
    let mut host_stdout: Vec<u8> = Vec::new();
    zhive_bridge_stdio::run(&sock, Cursor::new(host_stdin), &mut host_stdout)
        .await
        .expect("bridge run");
    engine.await.expect("engine task");

    // Step 4 / 4: parse the bridge's downstream output and assert that
    // every engine reply landed in order with intact framing.
    let mut reader = BufReader::new(&host_stdout[..]);
    for expected in build_engine_replies() {
        let got = framing::read_message(&mut reader)
            .await
            .expect("host reads engine frame");
        assert_eq!(got, expected, "engine frame mismatch");
    }
}

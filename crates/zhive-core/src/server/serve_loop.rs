//! Per-connection message loop and handshake gate.
//!
//! This module contains the three public entry points that drive a single
//! connected client ([`serve_loop`], [`serve_loop_with_reverse`],
//! [`serve_loop_with_outbound`]) together with their private helpers
//! ([`dispatch_message`], [`build_initialize_response`]).
//!
//! ## Handshake gate (D-007)
//!
//! The first request on every connection **must** be an `initialize`
//! request. The gate is tracked per-connection (`initialized` flag) and
//! enforced inside [`dispatch_message`]:
//!
//! * `initialize` request → version-negotiate, respond, set flag.
//! * `initialized` notification → accepted as a no-op (D-007 §1.4).
//! * any other request while `!initialized` → `-32002 ServerNotInitialized`.
//! * any other message while `initialized` → forwarded to the [`super::Router`].

use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zhive_proto::initialize::InitializeRequest;
use zhive_proto::{Message, Response};

use super::ServerError;
use super::events::SharedEventFilter;
use super::initialize::{InitResult, handle_initialize, not_initialized_error, response_to_value};
use super::reverse_rpc::{ResolveOutcome, ReverseRpcTracker};
use super::router::{JsonRpcCode, Router};
use super::transport::Transport;

/// Parameters accepted by the `events/subscribe` request.
///
/// A `None` value for `methods` (or an empty list) is treated as "subscribe
/// to all" — equivalent to calling `events/unsubscribe`.
///
/// # Examples
///
/// ```
/// use zhive_core::server::serve_loop::SubscribeParams;
///
/// let p: SubscribeParams = serde_json::from_value(
///     serde_json::json!({ "methods": ["events/turn_started"] })
/// ).unwrap();
/// assert_eq!(p.methods.as_deref(), Some(&["events/turn_started".to_string()][..]));
/// ```
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SubscribeParams {
    /// Allowed method names; `None` or empty means allow-all.
    pub methods: Option<Vec<String>>,
}

/// Runs the server loop on `transport` until the peer closes the stream
/// or `shutdown` fires.
///
/// Every inbound request is dispatched to `router`. Notifications go to
/// the router too but no response is sent. Inbound [`Response`]s are
/// routed to `reverse_rpc` when supplied; a stray response (no pending
/// entry) is logged and discarded.
///
/// The shutdown token is checked between messages and against the
/// blocking read on `transport.next_message`. An in-flight dispatch is
/// allowed to complete before the loop returns so the matching response
/// is not lost mid-flight.
///
/// # Errors
///
/// Returns [`ServerError::Transport`] for unrecoverable read / write
/// failures.
pub async fn serve_loop<T>(
    transport: &mut T,
    router: Arc<Router>,
    shutdown: CancellationToken,
) -> Result<(), ServerError>
where
    T: Transport + ?Sized,
{
    serve_loop_with_reverse(transport, router, None, shutdown).await
}

/// Variant of [`serve_loop`] that also routes responses to a
/// [`ReverseRpcTracker`].
///
/// Use this entry point when the engine drives server-initiated requests
/// (typically `permission/request`); the tracker is shared with the engine
/// actor so a matching reply discharges the awaiting `oneshot`.
///
/// # Errors
///
/// Same surface as [`serve_loop`].
pub async fn serve_loop_with_reverse<T>(
    transport: &mut T,
    router: Arc<Router>,
    reverse_rpc: Option<Arc<ReverseRpcTracker>>,
    shutdown: CancellationToken,
) -> Result<(), ServerError>
where
    T: Transport + ?Sized,
{
    serve_loop_with_filter(transport, router, reverse_rpc, None, None, shutdown).await
}

/// Variant of [`serve_loop`] that drains an outbound message queue.
///
/// `outbound_rx` is `Some` when the caller has wired a side channel for
/// engine-driven traffic (events, reverse RPC) — every value it receives
/// is shipped through the transport's `send` half along with inbound
/// request responses.
///
/// When `outbound_rx` is `None` the function behaves exactly like
/// [`serve_loop_with_reverse`].
///
/// ## Handshake gate
///
/// Per D-007 the first request on every connection **must** be an
/// `initialize` request. The gate is intrinsic: this function tracks
/// per-connection state (`initialized`) and:
///
/// * `initialize` request → version negotiate, respond, set flag.
/// * `initialized` notification → accept (no-op / trace).
/// * any other request while `!initialized` → `-32002 ServerNotInitialized`.
/// * any other message while `initialized` → forwarded to `router`.
///
/// # Errors
///
/// Same surface as [`serve_loop`].
pub async fn serve_loop_with_outbound<T>(
    transport: &mut T,
    router: Arc<Router>,
    reverse_rpc: Option<Arc<ReverseRpcTracker>>,
    outbound_rx: Option<mpsc::Receiver<Message>>,
    shutdown: CancellationToken,
) -> Result<(), ServerError>
where
    T: Transport + ?Sized,
{
    serve_loop_with_filter(transport, router, reverse_rpc, outbound_rx, None, shutdown).await
}

/// Internal variant of [`serve_loop_with_outbound`] that also accepts a
/// per-connection [`SharedEventFilter`].
///
/// `event_filter` is `Some` when per-connection event filtering is active.
/// The filter is written by `events/subscribe` and `events/unsubscribe`
/// control messages intercepted inside this function. When `None`, those
/// method names fall through to the router like any other request.
///
/// When both `outbound_rx` and `event_filter` are `None`, this function
/// behaves exactly like [`serve_loop_with_reverse`].
///
/// ## Handshake gate
///
/// Same as [`serve_loop_with_outbound`].
///
/// ## Subscribe / unsubscribe gate
///
/// When `event_filter` is `Some`, the following requests are handled before
/// reaching the router:
///
/// * `events/subscribe { methods?: [...] }` → update filter; respond `{}`.
/// * `events/unsubscribe` → reset filter to allow-all; respond `{}`.
///
/// # Errors
///
/// Same surface as [`serve_loop`].
pub(crate) async fn serve_loop_with_filter<T>(
    transport: &mut T,
    router: Arc<Router>,
    reverse_rpc: Option<Arc<ReverseRpcTracker>>,
    mut outbound_rx: Option<mpsc::Receiver<Message>>,
    event_filter: Option<SharedEventFilter>,
    shutdown: CancellationToken,
) -> Result<(), ServerError>
where
    T: Transport + ?Sized,
{
    // Per D-007: connections start uninitialized.
    let mut initialized = false;

    loop {
        // The select! arms cover (in priority order):
        //   1. shutdown signal — leave the loop without losing data,
        //   2. outbound queue ready — drain server-pushed messages
        //      (events, reverse RPC) before reading more inbound,
        //   3. transport read — pull the next inbound message.
        //
        // The outbound arm is gated on `outbound_rx.is_some()` so the
        // `serve_loop` entry point that supplied `None` keeps its old
        // behaviour exactly.
        enum Branch {
            Inbound(Option<Message>),
            Outbound(Message),
            Shutdown,
            OutboundClosed,
        }
        let branch = tokio::select! {
            biased;
            () = shutdown.cancelled() => Branch::Shutdown,
            out = async {
                match outbound_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending::<Option<Message>>().await,
                }
            } => match out {
                Some(m) => Branch::Outbound(m),
                None => Branch::OutboundClosed,
            },
            res = transport.next_message() => Branch::Inbound(res?),
        };
        let msg = match branch {
            // Both Shutdown and graceful Inbound EOF close the loop;
            // the explicit fall-through keeps the intent obvious to a
            // future reader.
            Branch::Shutdown | Branch::Inbound(None) => return Ok(()),
            Branch::OutboundClosed => {
                // Outbound producer hung up: stop trying to read it but
                // keep serving inbound traffic.
                outbound_rx = None;
                continue;
            }
            Branch::Outbound(m) => {
                transport.send(&m).await?;
                continue;
            }
            Branch::Inbound(Some(m)) => m,
        };

        dispatch_message(
            transport,
            &router,
            reverse_rpc.as_deref(),
            event_filter.as_ref(),
            msg,
            &mut initialized,
        )
        .await?;
    }
}

/// Handles one inbound [`Request`] through the handshake gate, subscribe gate,
/// and router dispatch.
///
/// Extracted from [`dispatch_message`] to keep that function within the
/// 100-line limit while preserving all gate ordering.
async fn dispatch_request<T>(
    transport: &mut T,
    router: &Router,
    event_filter: Option<&SharedEventFilter>,
    req: zhive_proto::Request,
    initialized: &mut bool,
) -> Result<(), ServerError>
where
    T: Transport + ?Sized,
{
    let id = req.id.clone();
    // ── Handshake gate ──────────────────────────────────────────────────
    if req.method == "initialize" {
        let parsed: Result<InitializeRequest, _> =
            serde_json::from_value(req.params.unwrap_or(serde_json::Value::Null));
        let response = build_initialize_response(id, parsed, initialized);
        transport.send(&Message::Response(response)).await?;
        return Ok(());
    }
    // ── Not-initialized guard ───────────────────────────────────────────
    if !*initialized {
        let method = req.method.clone();
        tracing::debug!(
            name: "zhive.server.handshake.not_initialized",
            rpc_method = %method,
            "blocking request: connection not yet initialized"
        );
        transport
            .send(&Message::Response(Response::err(
                id,
                not_initialized_error(),
            )))
            .await?;
        return Ok(());
    }
    // ── Subscribe / unsubscribe gate ────────────────────────────────────
    if let Some(filter) = event_filter {
        if req.method == "events/subscribe" {
            // Parse eagerly: malformed params must surface InvalidParams
            // rather than silently falling back to allow-all (which would
            // leave the client believing it narrowed its subscription).
            let params: SubscribeParams = match req.params {
                None => SubscribeParams::default(),
                Some(v) => match serde_json::from_value(v) {
                    Ok(p) => p,
                    Err(e) => {
                        transport
                            .send(&Message::Response(Response::err(
                                id,
                                zhive_proto::ErrorObject {
                                    code: JsonRpcCode::InvalidParams.as_i64(),
                                    message: JsonRpcCode::InvalidParams.message().to_string(),
                                    data: Some(serde_json::Value::String(e.to_string())),
                                },
                            )))
                            .await?;
                        return Ok(());
                    }
                },
            };
            filter
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_methods(params.methods.unwrap_or_default());
            tracing::debug!(
                name: "zhive.events.subscribe",
                "events/subscribe updated per-connection filter"
            );
            transport
                .send(&Message::Response(Response::ok(
                    id,
                    serde_json::Value::Object(serde_json::Map::new()),
                )))
                .await?;
            return Ok(());
        }
        if req.method == "events/unsubscribe" {
            filter
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .reset();
            tracing::debug!(
                name: "zhive.events.unsubscribe",
                "events/unsubscribe reset per-connection filter to allow-all"
            );
            transport
                .send(&Message::Response(Response::ok(
                    id,
                    serde_json::Value::Object(serde_json::Map::new()),
                )))
                .await?;
            return Ok(());
        }
    }
    // ── Normal dispatch ─────────────────────────────────────────────────
    let outcome = router.dispatch(&req.method, req.params).await;
    let response = match outcome {
        Ok(value) => Response::ok(id, value),
        Err(err) => Response::err(id, err),
    };
    transport.send(&Message::Response(response)).await?;
    Ok(())
}

/// Dispatches one inbound [`Message`] to the correct handler.
///
/// Extracted from [`serve_loop_with_outbound`] so that function stays
/// under clippy's 100-line limit. All handshake-gate logic lives here.
///
/// `initialized` is modified in place when a valid `initialize` request
/// completes the handshake. `event_filter` is updated in place when an
/// `events/subscribe` or `events/unsubscribe` request is received.
async fn dispatch_message<T>(
    transport: &mut T,
    router: &Router,
    reverse_rpc: Option<&ReverseRpcTracker>,
    event_filter: Option<&SharedEventFilter>,
    msg: Message,
    initialized: &mut bool,
) -> Result<(), ServerError>
where
    T: Transport + ?Sized,
{
    match msg {
        Message::Request(req) => {
            dispatch_request(transport, router, event_filter, req, initialized).await?;
        }
        Message::Notification(n) => {
            // `initialized` notification: client's handshake-complete
            // signal (D-007 §1.4). Always accepted.
            if n.method == "initialized" {
                tracing::debug!(
                    name: "zhive.server.handshake.initialized_notification",
                    "received 'initialized' notification from client"
                );
                return Ok(());
            }
            if !*initialized {
                let method = n.method.clone();
                tracing::debug!(
                    name: "zhive.server.handshake.notification_before_init",
                    rpc_method = %method,
                    "dropping notification: connection not yet initialized"
                );
                return Ok(());
            }
            let method = n.method.clone();
            if let Err(err) = router.dispatch(&n.method, n.params).await {
                let code = err.code;
                tracing::debug!(
                    name: "zhive.rpc.notification.dispatch_failed",
                    rpc_method = %method,
                    rpc_jsonrpc_error_code = code,
                    "notification dispatch returned error (no response sent)"
                );
            }
        }
        Message::Response(resp) => {
            if let Some(tracker) = reverse_rpc {
                let response_id = format!("{:?}", resp.id);
                match tracker.resolve(resp) {
                    ResolveOutcome::Delivered => {}
                    ResolveOutcome::AwaiterDropped => {
                        tracing::debug!(
                            name: "zhive.rpc.response.awaiter_dropped",
                            response_id = %response_id,
                            "reverse-RPC awaiter was dropped before response arrived"
                        );
                    }
                    ResolveOutcome::NoMatch => {
                        tracing::warn!(
                            name: "zhive.rpc.response.no_match",
                            response_id = %response_id,
                            "response did not match any pending reverse-RPC id"
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// Builds the [`Response`] for an inbound `initialize` request and
/// updates the `initialized` flag on success.
fn build_initialize_response(
    id: zhive_proto::Id,
    parsed: Result<InitializeRequest, serde_json::Error>,
    initialized: &mut bool,
) -> Response {
    let init_req = match parsed {
        Err(e) => {
            return Response::err(
                id,
                zhive_proto::ErrorObject {
                    code: JsonRpcCode::InvalidParams.as_i64(),
                    message: JsonRpcCode::InvalidParams.message().to_string(),
                    data: Some(serde_json::Value::String(e.to_string())),
                },
            );
        }
        Ok(r) => r,
    };
    match handle_initialize(&init_req, *initialized) {
        InitResult::Accept(resp) => {
            *initialized = true;
            tracing::debug!(
                name: "zhive.server.handshake.completed",
                rpc_negotiated_version = resp.protocol_version.0,
                "initialize handshake completed"
            );
            Response::ok(id, response_to_value(&resp))
        }
        InitResult::Reject(err) => {
            tracing::warn!(
                name: "zhive.server.handshake.version_rejected",
                rpc_jsonrpc_error_code = err.code,
                "initialize request rejected: unsupported protocol version"
            );
            Response::err(id, err)
        }
        InitResult::AlreadyInitialized(err) => {
            tracing::debug!(
                name: "zhive.server.handshake.already_initialized",
                "received initialize on already-initialized connection"
            );
            Response::err(id, err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::transport::TransportError;
    use super::*;
    use async_trait::async_trait;

    /// A pre-handshake request must be rejected with -32002.
    #[cfg(unix)]
    #[tokio::test]
    async fn request_before_handshake_returns_not_initialized() {
        use super::super::UdsTransport;
        use zhive_proto::{Id, Request};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("noinit.sock");
        let router = Arc::new(Router::new());

        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let router_for_server = Arc::clone(&router);
        let _server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut t = UdsTransport::new(stream);
            let _ = serve_loop(&mut t, router_for_server, CancellationToken::new()).await;
        });

        let mut client = UdsTransport::connect(&socket).await.unwrap();
        // Send a non-initialize request without handshaking.
        let req = Request::new(Id::Number(1), "ping", None);
        client.send(&Message::Request(req)).await.unwrap();
        let reply = client.next_message().await.unwrap().unwrap();
        match reply {
            Message::Response(resp) => match resp.outcome {
                zhive_proto::ResponseOutcome::Error(e) => {
                    assert_eq!(e.code, -32002, "expected ServerNotInitialized");
                }
                zhive_proto::ResponseOutcome::Result(v) => {
                    panic!("expected error, got result {v:?}")
                }
            },
            other => panic!("expected Response, got {other:?}"),
        }
    }

    /// An `initialize` request with a version above LATEST must be
    /// rejected with -32001.
    #[cfg(unix)]
    #[tokio::test]
    async fn initialize_with_unsupported_version_returns_error() {
        use super::super::UdsTransport;
        use zhive_proto::{Id, Request};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("badver.sock");
        let router = Arc::new(Router::new());

        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let router_for_server = Arc::clone(&router);
        let _server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut t = UdsTransport::new(stream);
            let _ = serve_loop(&mut t, router_for_server, CancellationToken::new()).await;
        });

        let mut client = UdsTransport::connect(&socket).await.unwrap();
        // Use raw JSON to bypass `#[non_exhaustive]` struct literals.
        let params = serde_json::json!({
            "protocolVersion": 99,
            "clientInfo": {
                "name": "bad-client",
                "version": "0.0.0",
            },
        });
        let req = Request::new(Id::Number(1), "initialize", Some(params));
        client.send(&Message::Request(req)).await.unwrap();
        let reply = client.next_message().await.unwrap().unwrap();
        match reply {
            Message::Response(resp) => match resp.outcome {
                zhive_proto::ResponseOutcome::Error(e) => {
                    assert_eq!(e.code, -32001, "expected ProtocolVersionUnsupported");
                    let data = e.data.unwrap();
                    assert_eq!(data["requested"], 99);
                    assert!(data["supported"].is_array());
                }
                zhive_proto::ResponseOutcome::Result(v) => {
                    panic!("expected error, got result {v:?}")
                }
            },
            other => panic!("expected Response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn serve_loop_with_reverse_routes_response_to_tracker() {
        use zhive_proto::{Id, Response};

        // Channel-backed transport: serve_loop reads Messages from
        // `incoming` and writes them to `outgoing`. We push a Response
        // and verify the tracker resolves the matching awaiter.
        struct ChannelTransport {
            incoming: tokio::sync::mpsc::Receiver<Message>,
            outgoing: tokio::sync::mpsc::UnboundedSender<Message>,
        }
        #[async_trait]
        impl Transport for ChannelTransport {
            async fn next_message(&mut self) -> Result<Option<Message>, TransportError> {
                Ok(self.incoming.recv().await)
            }
            async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
                let _ = self.outgoing.send(msg.clone());
                Ok(())
            }
        }

        let (in_tx, in_rx) = tokio::sync::mpsc::channel::<Message>(4);
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let mut transport = ChannelTransport {
            incoming: in_rx,
            outgoing: out_tx,
        };

        let router = Arc::new(Router::new());
        let tracker = Arc::new(ReverseRpcTracker::new());
        let (req, rx) = tracker.issue("permission/request", None);
        let id_for_response = req.id.clone();
        let token = CancellationToken::new();

        let tracker_for_loop = Arc::clone(&tracker);
        let cancel = token.clone();
        let loop_handle = tokio::spawn(async move {
            serve_loop_with_reverse(&mut transport, router, Some(tracker_for_loop), cancel)
                .await
                .unwrap();
        });

        // Feed the response that matches our issued request.
        in_tx
            .send(Message::Response(Response::ok(
                id_for_response,
                serde_json::json!({"outcome": "selected"}),
            )))
            .await
            .unwrap();

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("tracker must resolve quickly")
            .expect("oneshot")
            .expect("ok value");
        assert_eq!(outcome, serde_json::json!({"outcome": "selected"}));

        // Sending an extra Response with an unknown id should be a
        // `NoMatch` (logged at warn but not crashing the loop).
        in_tx
            .send(Message::Response(Response::ok(
                Id::String("rev:999".into()),
                serde_json::Value::Null,
            )))
            .await
            .unwrap();

        token.cancel();
        drop(in_tx);
        loop_handle.await.unwrap();
    }
}

// Rust guideline compliant 2026-02-21

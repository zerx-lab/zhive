//! Reader / writer task glue for [`crate::Client`].
//!
//! Each connector ([`crate::Client::from_split`] etc.) supplies its own
//! `AsyncRead + AsyncWrite` halves and the tasks below pump framed
//! [`Message`] values through them.
//!
//! ## Lifecycle
//!
//! Each task observes a [`CancellationToken`]; when the token fires
//! (the last [`crate::Client`] clone went out of scope or
//! [`crate::Client::shutdown`] was called) the task drains its work
//! and exits.
//!
//! ## Teardown ordering
//!
//! On disconnect the reader task:
//! 1. Drains `PendingRequests` — in-flight `call` futures resolve to
//!    `Err(ClientError::Disconnected)`.
//! 2. Aborts all pending reverse-RPC join handles.
//! 3. Broadcasts `ClientEvent::Disconnected` — subscribers observing
//!    the event stream see the terminal event.
//! 4. Drops the broadcast sender, closing the channel.
//!
//! This ordering guarantees that a caller awaiting both a `call` and
//! `next_event` sees the request error *before* the disconnect event.

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use zhive_proto::framing::{self, FramingError};
use zhive_proto::{ErrorObject, Id, Message, Notification, Response};

use futures::FutureExt as _;
use std::panic::AssertUnwindSafe;

use crate::PendingRequests;
use crate::events::ClientEvent;
use crate::reverse::{HandlerSlot, PendingReverse};

/// JSON-RPC error code for an unregistered reverse-RPC method.
const METHOD_NOT_FOUND: i64 = -32601;

/// JSON-RPC error code for an unexpected internal error (e.g. handler panic).
///
/// Maps to the standard JSON-RPC 2.0 "Internal error" reserved code.
const INTERNAL_ERROR: i64 = -32603;

/// Arguments passed to the reader task at spawn time.
pub(crate) struct ReaderArgs<R> {
    pub(crate) pending: Arc<PendingRequests>,
    pub(crate) read: R,
    pub(crate) shutdown: CancellationToken,
    pub(crate) outbound_tx: mpsc::Sender<Message>,
    /// Broadcast sender for the unified `ClientEvent` stream.
    pub(crate) events_tx: broadcast::Sender<ClientEvent>,
    /// Broadcast sender for the legacy `Notification`-only stream.
    pub(crate) notifications_tx: broadcast::Sender<Notification>,
    /// Shared handler slot (may be empty).
    pub(crate) handler_slot: Arc<HandlerSlot>,
    /// Pending reverse-RPC join handles shared with the `Client`.
    pub(crate) pending_reverse: Arc<PendingReverse>,
}

/// Spawns the reader task that decodes inbound frames and routes
/// messages to their respective sinks.
///
/// Routing rules:
/// - `Response` → resolve the matching pending `call` future.
/// - `Request` → fire [`ClientEvent::ServerRequest`], then dispatch to
///   the registered `ReverseHandler` or reply with `MethodNotFound`.
/// - `Notification` → fire [`ClientEvent::Notification`] and also
///   broadcast on the legacy `notifications_tx` channel.
///
/// On EOF / cancel / framing error the task performs the ordered
/// teardown described in the module doc before exiting.
pub(crate) fn spawn_reader<R>(args: ReaderArgs<R>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let ReaderArgs {
        pending,
        read,
        shutdown,
        outbound_tx,
        events_tx,
        notifications_tx,
        handler_slot,
        pending_reverse,
    } = args;

    tokio::spawn(async move {
        let mut reader = BufReader::new(read);
        let disconnect_reason = loop {
            let next = tokio::select! {
                biased;
                () = shutdown.cancelled() => break "shutdown cancelled".to_owned(),
                res = framing::read_message(&mut reader) => res,
            };
            match next {
                Ok(Message::Response(resp)) => {
                    crate::resolve_response(&pending, resp);
                }
                Ok(Message::Request(req)) => {
                    handle_server_request(
                        req,
                        &outbound_tx,
                        &events_tx,
                        &handler_slot,
                        &pending_reverse,
                    )
                    .await;
                }
                Ok(Message::Notification(n)) => {
                    // Broadcast on both channels; errors are benign (no
                    // receivers is not an error, lagged receivers handle
                    // their own gap).
                    let _ = notifications_tx.send(n.clone());
                    let _ = events_tx.send(ClientEvent::Notification(n));
                }
                Err(FramingError::UnexpectedEof) => {
                    break "unexpected EOF".to_owned();
                }
                Err(FramingError::Io(ref e))
                    if matches!(e.kind(), std::io::ErrorKind::UnexpectedEof) =>
                {
                    break "unexpected EOF".to_owned();
                }
                Err(other) => {
                    let msg = other.to_string();
                    tracing::warn!(
                        name: "zhive.client.reader.framing_error",
                        error_message = %msg,
                        "framing error on inbound stream; closing reader"
                    );
                    break msg;
                }
            }
        };

        // ── Ordered teardown ──────────────────────────────────────────
        // Step 1: reject all pending call futures.
        pending.drain_with_disconnected(&disconnect_reason);

        // Step 2: abort in-flight reverse-RPC handler tasks.
        pending_reverse.abort_all();

        // Step 3: broadcast the terminal disconnect event.
        let _ = events_tx.send(ClientEvent::Disconnected {
            reason: disconnect_reason,
        });
        // Step 4: drop `events_tx` — closing the broadcast channel so
        // subscribers see `None` after the Disconnected event.
    });
}

/// Spawns the writer task that drains the outbound queue and writes
/// frames to the peer.
///
/// On cancel / channel-close / write error the writer also calls
/// [`PendingRequests::drain_with_disconnected`] so callers whose
/// request never made it to the wire surface
/// [`crate::ClientError::Disconnected`] instead of blocking on a
/// response that will never arrive.
pub(crate) fn spawn_writer<W>(
    write: W,
    mut outbound_rx: mpsc::Receiver<Message>,
    shutdown: CancellationToken,
    pending: Arc<PendingRequests>,
) where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut writer = write;
        loop {
            let next = tokio::select! {
                biased;
                () = shutdown.cancelled() => None,
                msg = outbound_rx.recv() => msg,
            };
            let Some(msg) = next else {
                break;
            };
            if let Err(e) = framing::write_message(&mut writer, &msg).await {
                let message = e.to_string();
                tracing::warn!(
                    name: "zhive.client.writer.framing_error",
                    error_message = %message,
                    "outbound write failed; closing writer"
                );
                break;
            }
        }
        // Cause concurrent waiters to surface Disconnected (rather than
        // hanging forever) when the writer exited due to an I/O failure.
        pending.drain_with_disconnected("writer task exited");
    });
}

/// Dispatches a server-initiated request to the registered handler
/// or replies with `MethodNotFound`.
///
/// The `ClientEvent::ServerRequest` variant is always emitted so
/// observability subscribers can see the request regardless of whether
/// a handler is registered.
async fn handle_server_request(
    req: zhive_proto::Request,
    outbound_tx: &mpsc::Sender<Message>,
    events_tx: &broadcast::Sender<ClientEvent>,
    handler_slot: &HandlerSlot,
    pending_reverse: &Arc<PendingReverse>,
) {
    let req_id = req.id.clone();
    let method = req.method.clone();
    let params = req.params.clone();

    // Emit observation event regardless of dispatch path.
    let _ = events_tx.send(ClientEvent::ServerRequest {
        id: req_id.clone(),
        method: method.clone(),
        params: params.clone(),
    });

    let handler = handler_slot.get();
    let registered = handler
        .as_ref()
        .is_some_and(|h| h.methods().contains(&method.as_str()));

    if !registered {
        tracing::warn!(
            name: "zhive.client.reverse_request.no_handler",
            rpc_method = %method,
            "server-initiated request has no handler; replying MethodNotFound"
        );
        let resp = synthetic_method_not_found(req_id, &method);
        if outbound_tx.send(Message::Response(resp)).await.is_err() {
            tracing::warn!(
                name: "zhive.client.reverse_request.reply_dropped",
                "could not send MethodNotFound reply: writer exited"
            );
        }
        return;
    }

    // The early-return above guarantees `registered == true` implies
    // `handler.is_some()`, but we avoid `.expect()` in library code.
    let Some(handler) = handler else {
        return;
    };
    let outbound_tx = outbound_tx.clone();
    let pending_reverse_arc = Arc::clone(pending_reverse);
    let req_id_for_cleanup = req.id.clone();

    // Spawn handler in an independent task so the reader loop is not
    // blocked while the handler awaits UI or I/O.
    //
    // We wrap the `handle` call in `catch_unwind` so that a panicking
    // handler never leaves the server hanging on an unanswered request.
    // See C4 §3.2 for the required dispatch invariant:
    //   Ok(Ok(v))   → Response::ok
    //   Ok(Err(e))  → Response::err
    //   Err(_panic) → Response::err with INTERNAL_ERROR (-32603)
    let join = tokio::spawn(async move {
        let catch_result = AssertUnwindSafe(handler.handle(&method, params))
            .catch_unwind()
            .await;
        let resp = match catch_result {
            Ok(Ok(value)) => Response::ok(req_id, value),
            Ok(Err(err_obj)) => Response::err(req_id, err_obj),
            Err(_panic) => {
                tracing::error!(
                    name: "zhive.client.reverse_handler.panic",
                    rpc_method = %method,
                    "reverse handler panicked; sending InternalError to server"
                );
                Response::err(
                    req_id,
                    ErrorObject {
                        code: INTERNAL_ERROR,
                        message: "reverse handler panicked".to_owned(),
                        data: None,
                    },
                )
            }
        };
        if outbound_tx.send(Message::Response(resp)).await.is_err() {
            tracing::warn!(
                name: "zhive.client.reverse_handler.reply_dropped",
                "reverse handler reply could not be sent: writer exited"
            );
        }
        // Clean up our own entry so the map does not grow unboundedly.
        // This always runs regardless of which branch above was taken.
        pending_reverse_arc.remove(&req_id_for_cleanup);
    });

    pending_reverse.insert(req.id, join);
}

/// Builds a synthetic JSON-RPC `-32601` error response for a
/// server request that has no registered handler.
fn synthetic_method_not_found(id: Id, method: &str) -> Response {
    Response::err(
        id,
        ErrorObject {
            code: METHOD_NOT_FOUND,
            message: format!("client has no handler for reverse-RPC method {method:?}"),
            data: None,
        },
    )
}

// Rust guideline compliant 2026-02-21

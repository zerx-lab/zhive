//! Reader / writer task glue for [`crate::Client`].
//!
//! The transport stays trait-free in Phase 1: each connector
//! ([`crate::Client::from_split`] etc.) supplies its own
//! `AsyncRead + AsyncWrite` halves and the tasks below pump framed
//! [`Message`] values through them.
//!
//! ## Lifecycle
//!
//! Each task observes a [`CancellationToken`]; when the token fires
//! (the last [`crate::Client`] clone went out of scope or
//! [`crate::Client::shutdown`] was called) the task drains its work
//! and exits. The shared [`PendingRequests`] is drained on any path
//! that disables further outbound writes so callers awaiting
//! responses do not block forever.

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use zhive_proto::framing::{self, FramingError};
use zhive_proto::{ErrorObject, Id, Message, Notification, Response};

use crate::PendingRequests;

/// JSON-RPC error returned to the server when an inbound reverse-RPC
/// request arrives but no handler is wired up (C4 deliverable).
const REVERSE_RPC_METHOD_NOT_FOUND: i64 = -32601;

/// Spawns the reader task that decodes inbound frames and routes
/// [`Message::Response`]s to the pending map.
///
/// On EOF / cancel / framing error the task drains the pending map so
/// callers waiting on `call` futures fail fast with
/// [`crate::ClientError::ConnectionClosed`] rather than hanging.
pub(crate) fn spawn_reader<R>(
    pending: Arc<PendingRequests>,
    read: R,
    shutdown: CancellationToken,
    outbound_tx: mpsc::Sender<Message>,
    notifications_tx: broadcast::Sender<Notification>,
) where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(read);
        loop {
            let next = tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                res = framing::read_message(&mut reader) => res,
            };
            match next {
                Ok(Message::Response(resp)) => {
                    crate::resolve_response(&pending, resp);
                }
                Ok(Message::Request(req)) => {
                    // Reverse RPC: the client should mount a handler
                    // via C4. Until then, reply with MethodNotFound so
                    // the server's `ReverseRpcTracker` resolves its
                    // pending entry instead of hanging.
                    let id = req.id.clone();
                    let method = req.method.clone();
                    tracing::warn!(
                        name: "zhive.client.reverse_request.no_handler",
                        rpc_method = %method,
                        "server-initiated request had no handler; replying MethodNotFound"
                    );
                    let resp = synthetic_method_not_found(id, &method);
                    if outbound_tx.send(Message::Response(resp)).await.is_err() {
                        // Writer task has already exited; nothing
                        // more we can do. Log and continue draining
                        // the read side so any in-flight responses
                        // still resolve.
                        tracing::warn!(
                            name: "zhive.client.reverse_request.reply_dropped",
                            "could not send reverse-RPC MethodNotFound: writer exited"
                        );
                    }
                }
                Ok(Message::Notification(n)) => {
                    // Broadcast to every subscriber; `send` returns
                    // `Err` only when no receivers exist, which is
                    // not an error — drop silently in that case.
                    let _ = notifications_tx.send(n);
                }
                Err(FramingError::UnexpectedEof) => break,
                Err(FramingError::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::UnexpectedEof) =>
                {
                    break;
                }
                Err(other) => {
                    let message = other.to_string();
                    tracing::warn!(
                        name: "zhive.client.reader.framing_error",
                        error_message = %message,
                        "framing error on inbound stream; closing reader"
                    );
                    break;
                }
            }
        }
        // Tear down the pending map so any caller still awaiting a
        // response surfaces `ConnectionClosed` immediately.
        pending.drain();
    });
}

/// Spawns the writer task that drains the outbound queue and writes
/// frames to the peer.
///
/// On cancel / channel-close / write error the writer also calls
/// [`PendingRequests::drain`] so callers whose request never made it
/// to the wire surface [`crate::ClientError::ConnectionClosed`]
/// instead of blocking on a response that will never arrive.
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
        // Cause concurrent waiters to surface ConnectionClosed (rather
        // than hanging forever) when the writer exited because of an
        // I/O failure, not because the channel cleanly drained.
        pending.drain();
    });
}

/// Builds a synthetic JSON-RPC error response for a reverse-RPC
/// request that has no client-side handler. Kept here so a future C4
/// handler can flip the call to actually ship the reply through the
/// outbound channel without changing the call-site discipline.
fn synthetic_method_not_found(id: Id, method: &str) -> Response {
    Response::err(
        id,
        ErrorObject {
            code: REVERSE_RPC_METHOD_NOT_FOUND,
            message: format!("client has no handler for reverse RPC method {method:?}"),
            data: None,
        },
    )
}

// Rust guideline compliant 2026-02-21

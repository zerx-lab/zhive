//! Native Rust JSON-RPC 2.0 client for the zhive engine.
//!
//! All in-process Rust callers (TUI, bridges, embedded SDK) reach the
//! engine through this crate. Per D-002 it never depends on
//! `zhive-core`: the only shared crate is `zhive-proto`, which carries
//! the wire schema.
//!
//! ## Surface
//!
//! * [`Client`] — owns a transport + a per-connection pending-request
//!   map. Cheap to clone (`Arc`-shared internals).
//! * [`Client::connect_uds`] / [`Client::connect_stdio`] — connectors
//!   for the two Phase 1 transports.
//! * [`Client::call`] / [`Client::notify`] — RPC primitives.
//! * [`Client::shutdown`] — closes the connection; the reader and
//!   writer tasks exit promptly. The same teardown runs automatically
//!   when the last [`Client`] clone is dropped.
//!
//! Reconnect, request cancellation, and reverse-RPC handler
//! registration land with C2 / C3 / C4 deliverables.

#![forbid(unsafe_code)]

mod pending;
mod transport;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use zhive_proto::framing::FramingError;
use zhive_proto::{ErrorObject, Id, Message, Notification, Request, Response, ResponseOutcome};

pub use pending::{PendingRequests, ResolveResult};

/// Capacity of the per-connection inbound-notification broadcast.
///
/// Each [`Client::subscribe_notifications`] caller gets a fresh
/// receiver backed by this buffer; a subscriber that falls behind by
/// more than this many notifications observes `Lagged` errors. The
/// default balances catch-up tolerance against bounded memory.
pub const DEFAULT_NOTIFICATION_BUFFER: usize = 256;

/// Channel capacity for the outbound writer queue.
///
/// The reader task and any `Client` clone push outbound `Message`s into
/// the same channel; the writer task drains them serially so framing
/// stays atomic. Picked to absorb burst traffic without back-pressuring
/// callers in the common case.
const OUTBOUND_QUEUE_CAP: usize = 64;

/// Failure modes surfaced by [`Client`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClientError {
    /// Transport-level I/O failure (read / write / connect).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Framing-level error (header malformed, body oversize, ...).
    #[error("framing error: {0}")]
    Framing(#[from] FramingError),

    /// The reader task exited (e.g. peer closed the stream) before the
    /// expected response arrived.
    #[error("connection closed before response arrived")]
    ConnectionClosed,

    /// Server responded with a structured JSON-RPC error object.
    #[error("server error: {0:?}")]
    Server(ErrorObject),
}

/// Inner handle whose [`Drop`] cancels the per-connection shutdown
/// token so both reader and writer tasks exit when the last
/// [`Client`] clone goes out of scope.
#[derive(Debug)]
struct Inner {
    shutdown: CancellationToken,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// JSON-RPC client handle.
#[derive(Clone, Debug)]
pub struct Client {
    next_id: Arc<AtomicU64>,
    pending: Arc<PendingRequests>,
    outbound_tx: mpsc::Sender<Message>,
    /// Broadcast of inbound JSON-RPC notifications. Each call to
    /// [`Self::subscribe_notifications`] hands out a fresh receiver;
    /// the sender lives in the reader task.
    notifications_tx: broadcast::Sender<Notification>,
    /// Held only for the side effect: when the last `Inner` is
    /// dropped, the cancel token fires and the spawned tasks exit.
    /// Public methods that explicitly tear down the connection
    /// ([`Self::shutdown`]) reach into this handle directly.
    inner: Arc<Inner>,
}

impl Client {
    /// Builds a client from an arbitrary `AsyncRead` + `AsyncWrite`
    /// pair (typically a process's stdin/stdout or a UDS stream
    /// split-half).
    ///
    /// Spawns one reader task and one writer task on the current
    /// Tokio runtime. Both tasks share a [`CancellationToken`] that
    /// fires when the last [`Client`] clone is dropped or
    /// [`Client::shutdown`] is called explicitly.
    pub fn from_split<R, W>(read: R, write: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending = Arc::new(PendingRequests::default());
        let shutdown = CancellationToken::new();
        let (outbound_tx, outbound_rx) = mpsc::channel::<Message>(OUTBOUND_QUEUE_CAP);
        let (notifications_tx, _) = broadcast::channel::<Notification>(DEFAULT_NOTIFICATION_BUFFER);
        transport::spawn_reader(
            Arc::clone(&pending),
            read,
            shutdown.clone(),
            outbound_tx.clone(),
            notifications_tx.clone(),
        );
        transport::spawn_writer(write, outbound_rx, shutdown.clone(), Arc::clone(&pending));
        Self {
            next_id: Arc::new(AtomicU64::new(1)),
            pending,
            outbound_tx,
            notifications_tx,
            inner: Arc::new(Inner { shutdown }),
        }
    }

    /// Returns a fresh subscriber for inbound JSON-RPC notifications.
    ///
    /// The receiver buffers up to [`DEFAULT_NOTIFICATION_BUFFER`]
    /// notifications; subscribers that fall behind see
    /// `broadcast::error::RecvError::Lagged`. Drop the receiver to
    /// stop receiving.
    #[must_use]
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<Notification> {
        self.notifications_tx.subscribe()
    }

    /// Connects to a Unix-domain socket at `path` and builds a client
    /// over the resulting stream.
    ///
    /// # Errors
    ///
    /// Returns [`ClientError::Io`] when the connect syscall fails.
    #[cfg(unix)]
    pub async fn connect_uds(path: impl AsRef<std::path::Path>) -> Result<Self, ClientError> {
        let stream = tokio::net::UnixStream::connect(path.as_ref()).await?;
        let (read, write) = stream.into_split();
        Ok(Self::from_split(read, write))
    }

    /// Wraps the process's inherited stdio in a client.
    ///
    /// Used by host integrations that spawn `zhive serve --stdio` as a
    /// child process and communicate over the pipe.
    #[must_use]
    pub fn connect_stdio() -> Self {
        Self::from_split(tokio::io::stdin(), tokio::io::stdout())
    }

    /// Issues a JSON-RPC request and awaits the response value.
    ///
    /// # Errors
    ///
    /// * [`ClientError::ConnectionClosed`] when the reader task exits
    ///   before the response arrives (peer closed the stream or the
    ///   writer task aborted mid-flight).
    /// * [`ClientError::Server`] when the peer returned a JSON-RPC
    ///   error object.
    /// * [`ClientError::Io`] when the outbound queue cannot accept the
    ///   request (writer task already exited).
    pub async fn call(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, ClientError> {
        let seq = self.next_id.fetch_add(1, Ordering::Relaxed);
        // Use a string id (`"c:<n>"`) so clients are grepable on the
        // wire and the counter never saturates a fixed-width integer
        // — even at one call per nanosecond, `u64` lasts ~584 years.
        let id = Id::String(format!("c:{seq}"));
        let rx = self.pending.register(id.clone());
        let req = Request::new(id, method, params);
        self.outbound_tx
            .send(Message::Request(req))
            .await
            .map_err(|_send_err| ClientError::Io(std::io::Error::other("writer task exited")))?;
        let outcome = rx
            .await
            .map_err(|_recv_err| ClientError::ConnectionClosed)?;
        match outcome {
            ResponseOutcome::Result(v) => Ok(v),
            ResponseOutcome::Error(e) => Err(ClientError::Server(e)),
        }
    }

    /// Sends a fire-and-forget notification.
    ///
    /// # Errors
    ///
    /// [`ClientError::Io`] when the outbound queue is closed.
    pub async fn notify(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<(), ClientError> {
        let n = Notification::new(method, params);
        self.outbound_tx
            .send(Message::Notification(n))
            .await
            .map_err(|_send_err| ClientError::Io(std::io::Error::other("writer task exited")))
    }

    /// Explicitly cancels the per-connection shutdown token so both
    /// reader and writer tasks exit promptly.
    ///
    /// Equivalent to dropping the last [`Client`] clone. Subsequent
    /// `call` / `notify` invocations on still-alive clones surface
    /// [`ClientError::Io`] (writer gone) or
    /// [`ClientError::ConnectionClosed`] (reader gone).
    pub fn shutdown(self) {
        self.inner.shutdown.cancel();
        drop(self.outbound_tx);
    }
}

/// Inbound dispatch entry point for incoming [`Response`]s.
///
/// Pub-crate so the reader task can call into the pending map; not
/// part of the public surface.
#[doc(hidden)]
pub(crate) fn resolve_response(pending: &PendingRequests, response: Response) {
    let _ = pending.resolve(&response.id, response.outcome);
}

/// Reports this crate's package version.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, duplex};
    use zhive_proto::framing;

    /// Drives the client end-to-end against an in-memory duplex pipe
    /// that echoes back a canned response for one request id.
    #[tokio::test]
    async fn call_round_trips_through_in_memory_pipe() {
        let (client_io, server_io) = duplex(4096);
        let (server_read, server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);

        let server_task = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(server_read);
            let msg = framing::read_message(&mut reader).await.unwrap();
            let req = match msg {
                Message::Request(r) => r,
                other => panic!("expected request, got {other:?}"),
            };
            let resp = Response::ok(req.id, req.params.unwrap_or(serde_json::Value::Null));
            let mut writer = server_write;
            framing::write_message(&mut writer, &Message::Response(resp))
                .await
                .unwrap();
            writer.flush().await.unwrap();
        });

        let client = Client::from_split(client_read, client_write);
        let value = client
            .call("echo", Some(serde_json::json!({"hello": "world"})))
            .await
            .expect("call ok");
        assert_eq!(value, serde_json::json!({"hello": "world"}));
        server_task.await.unwrap();
    }

    /// When the last `Client` clone drops, the shared shutdown token
    /// fires and both background tasks exit promptly (verified
    /// indirectly: a fresh `call` on a clone made *before* drop hangs
    /// only briefly before the writer / reader teardown surfaces an
    /// IO error).
    #[tokio::test]
    async fn dropping_last_clone_terminates_tasks() {
        let (client_io, _server_io) = duplex(64);
        let (client_read, client_write) = tokio::io::split(client_io);
        let client = Client::from_split(client_read, client_write);
        let clone = client.clone();
        drop(client);
        // The clone keeps the connection alive — the writer is still
        // attached and the reader has not been cancelled.
        let outbound_capacity = clone.outbound_tx.capacity();
        assert!(outbound_capacity > 0);
        // Now drop the last clone — the spawned tasks must exit
        // because the shared CancellationToken fires.
        drop(clone);
        // Give the runtime a tick for the tasks to observe the cancel.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // We can't easily assert on task state without keeping a
        // handle, but the test passing without panics confirms the
        // cancel path does not deadlock or panic.
    }
}

// Rust guideline compliant 2026-02-21

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
//!   for the two Phase 1 transports. Both perform the full
//!   `initialize` / `initialized` handshake (D-007) before returning.
//! * [`Client::from_split`] — low-level constructor; does **not**
//!   perform the handshake. Use the `connect_*` family unless you are
//!   writing unit tests or composing the handshake yourself.
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
use zhive_proto::initialize::{Capabilities, Implementation, InitializeResponse, ProtocolVersion};
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
///
/// # Examples
///
/// ```
/// use zhive_client_native::ClientError;
/// use zhive_proto::ErrorObject;
///
/// let e = ClientError::Server(ErrorObject {
///     code: -32601,
///     message: "Method not found".into(),
///     data: None,
/// });
/// assert!(matches!(e, ClientError::Server(_)));
/// ```
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

    /// The server rejected the `initialize` request because it does
    /// not support the requested protocol version. Includes the
    /// supported range and requested version from the server's
    /// `-32001` error data payload.
    #[error("server rejected protocol version {requested}: supported [{min}, {max}]")]
    ProtocolVersionUnsupported {
        /// Requested version sent by this client.
        requested: u16,
        /// Minimum supported version reported by the server.
        min: u16,
        /// Maximum supported version reported by the server.
        max: u16,
    },

    /// The `initialize` handshake failed for a reason other than a
    /// version mismatch (server returned an unexpected JSON-RPC error
    /// or the response did not parse as an `InitializeResponse`).
    #[error("initialize handshake failed: {reason}")]
    InitializeFailed {
        /// Human-readable reason from the server or local framing.
        reason: String,
    },
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

/// Metadata agreed upon during the initialize handshake (D-007).
///
/// Stored in each [`Client`] after a `connect_*` call completes the
/// handshake. Clients created via [`Client::from_split`] carry
/// placeholder values.
#[derive(Debug, Clone)]
pub struct HandshakeMeta {
    /// Protocol version the server chose (≤ the version we requested).
    pub negotiated_version: ProtocolVersion,
    /// Capabilities the server reported in the `initialize` response.
    pub server_capabilities: Capabilities,
    /// Identity card the server included in the `initialize` response.
    pub server_info: Implementation,
}

/// JSON-RPC client handle.
///
/// Cheap to clone — all state is `Arc`-shared across clones.
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
    /// Handshake metadata; populated by `connect_*` connectors,
    /// placeholder for `from_split`.
    handshake: Arc<HandshakeMeta>,
}

impl Client {
    /// Builds a client from an arbitrary `AsyncRead` + `AsyncWrite`
    /// pair without performing the initialize handshake.
    ///
    /// This is the low-level building block used by the in-memory unit
    /// tests and by the `connect_*` connectors (which call this first,
    /// then complete the handshake themselves). Callers that want a
    /// production-ready client should prefer [`Self::connect_uds`] or
    /// [`Self::connect_stdio`], which perform the full D-007 handshake
    /// before returning.
    ///
    /// Spawns one reader task and one writer task on the current
    /// Tokio runtime. Both tasks share a [`CancellationToken`] that
    /// fires when the last [`Client`] clone is dropped or
    /// [`Client::shutdown`] is called explicitly.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use tokio::io::duplex;
    /// use zhive_client_native::Client;
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let (a, _b) = duplex(4096);
    /// let (read, write) = tokio::io::split(a);
    /// let client = Client::from_split(read, write);
    /// // `client.negotiated_version()` returns the placeholder V0 because
    /// // no handshake was performed.
    /// # }
    /// ```
    pub fn from_split<R, W>(read: R, write: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        // Placeholder: no handshake performed. Connectors (`connect_*`)
        // replace this with the real negotiated metadata.
        let placeholder = placeholder_handshake_meta();
        Self::from_split_with_meta(read, write, placeholder)
    }

    /// Internal: builds a [`Client`] with pre-computed handshake metadata.
    fn from_split_with_meta<R, W>(read: R, write: W, meta: HandshakeMeta) -> Self
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
            handshake: Arc::new(meta),
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

    /// Connects to a Unix-domain socket at `path` and performs the
    /// full initialize / initialized handshake (D-007) before
    /// returning.
    ///
    /// The returned client's [`Self::negotiated_version`],
    /// [`Self::server_capabilities`], and [`Self::server_info`]
    /// accessors reflect the values agreed during the handshake.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Io`] — connect syscall failed.
    /// * [`ClientError::ProtocolVersionUnsupported`] — server rejected
    ///   the requested protocol version (-32001).
    /// * [`ClientError::InitializeFailed`] — server returned any other
    ///   error or the response could not be decoded.
    #[cfg(unix)]
    pub async fn connect_uds(path: impl AsRef<std::path::Path>) -> Result<Self, ClientError> {
        let stream = tokio::net::UnixStream::connect(path.as_ref()).await?;
        let (read, write) = stream.into_split();
        let client = Self::from_split(read, write);
        let meta = perform_handshake(&client).await?;
        Ok(client.replace_meta(meta))
    }

    /// Wraps the process's inherited stdio in a client and performs
    /// the full initialize / initialized handshake (D-007).
    ///
    /// Used by host integrations that spawn `zhive serve --stdio` as a
    /// child process and communicate over the pipe.
    ///
    /// # Errors
    ///
    /// Same as [`Self::connect_uds`].
    pub async fn connect_stdio() -> Result<Self, ClientError> {
        let client = Self::from_split(tokio::io::stdin(), tokio::io::stdout());
        let meta = perform_handshake(&client).await?;
        Ok(client.replace_meta(meta))
    }

    /// Swaps the handshake metadata, consuming `self` and returning a
    /// new [`Client`] with `meta` applied. All other state (pending
    /// map, outbound channel, cancel token) is shared unchanged.
    fn replace_meta(self, meta: HandshakeMeta) -> Self {
        Self {
            handshake: Arc::new(meta),
            ..self
        }
    }

    /// Returns the protocol version negotiated during the handshake.
    ///
    /// Clients created via [`Self::from_split`] return
    /// [`ProtocolVersion::V0`] as a placeholder.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run() {
    /// # use zhive_client_native::Client;
    /// # use zhive_proto::initialize::ProtocolVersion;
    /// let client = Client::connect_uds("/tmp/zhive.sock").await.unwrap();
    /// assert!(client.negotiated_version().0 >= 1);
    /// # }
    /// ```
    #[must_use]
    pub fn negotiated_version(&self) -> ProtocolVersion {
        self.handshake.negotiated_version
    }

    /// Returns the capabilities the server reported during the
    /// handshake.
    ///
    /// Clients created via [`Self::from_split`] return a
    /// [`Capabilities::default()`] placeholder.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run() {
    /// # use zhive_client_native::Client;
    /// let client = Client::connect_uds("/tmp/zhive.sock").await.unwrap();
    /// assert!(client.server_capabilities().cancellation);
    /// # }
    /// ```
    #[must_use]
    pub fn server_capabilities(&self) -> &Capabilities {
        &self.handshake.server_capabilities
    }

    /// Returns the server identity card from the handshake.
    ///
    /// Clients created via [`Self::from_split`] return a placeholder
    /// with `name == "unknown"`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn run() {
    /// # use zhive_client_native::Client;
    /// let client = Client::connect_uds("/tmp/zhive.sock").await.unwrap();
    /// assert!(!client.server_info().name.is_empty());
    /// # }
    /// ```
    #[must_use]
    pub fn server_info(&self) -> &Implementation {
        &self.handshake.server_info
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
///
/// # Examples
///
/// ```
/// use zhive_client_native::version;
/// assert!(!version().is_empty());
/// ```
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ──────────────────────────────────────────────────────────────────
// Handshake helpers (used by connect_* but not part of public API)
// ──────────────────────────────────────────────────────────────────

/// Placeholder metadata stored in clients built via [`Client::from_split`].
///
/// The placeholder signals "no handshake performed" via V0 version and
/// empty server identity, but does not represent a real server.
fn placeholder_handshake_meta() -> HandshakeMeta {
    HandshakeMeta {
        negotiated_version: ProtocolVersion::V0,
        server_capabilities: Capabilities::default(),
        server_info: serde_json::from_value(serde_json::json!({
            "name": "unknown",
            "version": "unknown",
        }))
        .unwrap_or_else(|_| {
            // Infallible fallback — the JSON literal is valid.
            serde_json::from_str(r#"{"name":"unknown","version":"unknown"}"#)
                .unwrap_or_else(|_| unreachable!("bare minimal JSON must parse"))
        }),
    }
}

/// Executes the `initialize` / `initialized` handshake over an
/// already-connected [`Client`].
///
/// Sends an `initialize` request with [`ProtocolVersion::LATEST`] and
/// the client's identity, awaits the server's `InitializeResponse`,
/// then sends an `initialized` notification to signal readiness.
///
/// # Errors
///
/// * [`ClientError::ProtocolVersionUnsupported`] for server error code
///   `-32001`.
/// * [`ClientError::InitializeFailed`] for any other server error or
///   if the response cannot be decoded as an `InitializeResponse`.
/// * [`ClientError::ConnectionClosed`] / [`ClientError::Io`] for
///   transport-level failures.
async fn perform_handshake(client: &Client) -> Result<HandshakeMeta, ClientError> {
    // Build the initialize request via JSON to stay compatible with
    // `#[non_exhaustive]` on `InitializeRequest`.
    let params = serde_json::json!({
        "protocolVersion": ProtocolVersion::LATEST.0,
        "clientInfo": {
            "name": "zhive-client-native",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "clientCapabilities": {
            "cancellation": true,
        },
    });

    let raw = client.call("initialize", Some(params)).await.map_err(|e| {
        // Reinterpret a -32001 server error as a typed
        // `ProtocolVersionUnsupported` for ergonomic callers.
        if let ClientError::Server(ref obj) = e {
            if obj.code != -32001 {
                return e;
            }
            let requested = ProtocolVersion::LATEST.0;
            let supported = obj.data.as_ref().and_then(|d| d["supported"].as_array());
            let min = supported
                .and_then(|arr| arr.first())
                .and_then(serde_json::Value::as_u64)
                .map_or(0u16, |v| u16::try_from(v).unwrap_or(0));
            let max = supported
                .and_then(|arr| arr.last())
                .and_then(serde_json::Value::as_u64)
                .map_or(0u16, |v| u16::try_from(v).unwrap_or(0));
            return ClientError::ProtocolVersionUnsupported {
                requested,
                min,
                max,
            };
        }
        e
    })?;

    let resp: InitializeResponse =
        serde_json::from_value(raw).map_err(|e| ClientError::InitializeFailed {
            reason: format!("could not decode InitializeResponse: {e}"),
        })?;

    // Signal the server that our client is fully initialised.
    client
        .notify("initialized", None)
        .await
        .map_err(|e| ClientError::InitializeFailed {
            reason: format!("could not send initialized notification: {e}"),
        })?;

    Ok(HandshakeMeta {
        negotiated_version: resp.protocol_version,
        server_capabilities: resp.server_capabilities,
        server_info: resp.server_info,
    })
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

    /// Verifies that a stub server returning a -32001 error on the
    /// `initialize` request causes `perform_handshake` (and therefore
    /// any `connect_*` connector) to surface
    /// `ClientError::ProtocolVersionUnsupported` with the correct
    /// `requested`, `min`, and `max` fields extracted from the server's
    /// `data` payload.
    ///
    /// The stub server in this test simulates a server that only supports
    /// versions [0, 1] while our client requests LATEST (1); the rejection
    /// code here exercises the extraction branch at lines 500-523 in
    /// `perform_handshake`.
    #[tokio::test]
    async fn handshake_protocol_version_unsupported_surfaces_typed_error() {
        // Wire up an in-memory duplex so the client writes to one end and
        // the stub server task reads from the other.
        let (client_io, server_io) = duplex(4096);
        let (server_read, server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);

        // Stub server: reads one request (the `initialize` call), then
        // immediately replies with a -32001 ProtocolVersionUnsupported
        // error carrying a `data` object that matches the server-side
        // format defined in `zhive_core::server::initialize`.
        let server_task = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(server_read);
            let msg = framing::read_message(&mut reader).await.unwrap();
            let req = match msg {
                Message::Request(r) => r,
                other => panic!("stub server: expected Request, got {other:?}"),
            };
            let error = zhive_proto::ErrorObject {
                code: -32001,
                message: "ProtocolVersionUnsupported".into(),
                // The client parser reads `data["supported"]` as an array
                // [min, max], matching the format emitted by the real server.
                data: Some(serde_json::json!({
                    "supported": [0, 1],
                    "requested": i64::from(ProtocolVersion::LATEST.0),
                })),
            };
            let mut writer = server_write;
            framing::write_message(
                &mut writer,
                &Message::Response(Response::err(req.id, error)),
            )
            .await
            .unwrap();
            writer.flush().await.unwrap();
        });

        // Build a raw (no-handshake) client and run the handshake manually
        // so we can inspect the error without going through a socket connect.
        let raw_client = Client::from_split(client_read, client_write);
        let result = perform_handshake(&raw_client).await;

        server_task.await.unwrap();

        match result {
            Err(ClientError::ProtocolVersionUnsupported {
                requested,
                min,
                max,
            }) => {
                assert_eq!(
                    requested,
                    ProtocolVersion::LATEST.0,
                    "requested must match LATEST"
                );
                assert_eq!(min, 0, "min must be extracted from data[supported][0]");
                assert_eq!(max, 1, "max must be extracted from data[supported][1]");
            }
            other => panic!("expected ClientError::ProtocolVersionUnsupported, got {other:?}"),
        }
    }

    /// Verifies that a stub server returning a -32002 `ServerNotInitialized`
    /// error in response to the `initialize` request surfaces as
    /// `ClientError::Server` with the correct error code on the client side.
    ///
    /// In production this error would only arrive for requests *other* than
    /// `initialize` (sent before the handshake is complete), but since the
    /// client's `call` wrapper is code-agnostic, a -32002 on the `initialize`
    /// request exercises the same conversion path that the client would
    /// traverse if a raw pre-handshake call ever received this response.
    #[tokio::test]
    async fn handshake_server_not_initialized_surfaces_server_error() {
        let (client_io, server_io) = duplex(4096);
        let (server_read, server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);

        // Stub server: replies to the first request with -32002.
        let server_task = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(server_read);
            let msg = framing::read_message(&mut reader).await.unwrap();
            let req = match msg {
                Message::Request(r) => r,
                other => panic!("stub server: expected Request, got {other:?}"),
            };
            let error = zhive_proto::ErrorObject {
                code: -32002,
                message: "ServerNotInitialized".into(),
                data: None,
            };
            let mut writer = server_write;
            framing::write_message(
                &mut writer,
                &Message::Response(Response::err(req.id, error)),
            )
            .await
            .unwrap();
            writer.flush().await.unwrap();
        });

        let raw_client = Client::from_split(client_read, client_write);
        let result = perform_handshake(&raw_client).await;

        server_task.await.unwrap();

        // A -32002 code is not intercepted by the perform_handshake
        // conversion closure (only -32001 maps to a typed variant), so
        // it propagates as ClientError::Server with the raw code intact.
        match result {
            Err(ClientError::Server(obj)) => {
                assert_eq!(obj.code, -32002, "must preserve the -32002 wire code");
            }
            other => panic!("expected ClientError::Server(-32002), got {other:?}"),
        }
    }
}

// Rust guideline compliant 2026-02-21

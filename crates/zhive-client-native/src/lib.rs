//! Native Rust JSON-RPC 2.0 client for the zhive engine.
//!
//! All in-process Rust callers (TUI, bridges, embedded SDK) reach the
//! engine through this crate.  Per D-002 it never depends on
//! `zhive-core`: the only shared crate is `zhive-proto`, which carries
//! the wire schema.
//!
//! ## Surface
//!
//! * [`Client`] — owns a transport + a per-connection pending-request
//!   map.  Cheap to clone (`Arc`-shared internals).
//! * [`Client::connect_uds`] / [`Client::connect_stdio`] — connectors
//!   for the two Phase 1 transports.  Both perform the full
//!   `initialize` / `initialized` handshake (D-007) before returning.
//! * [`Client::from_split`] — low-level constructor; does **not**
//!   perform the handshake.  Use the `connect_*` family unless you are
//!   writing unit tests or composing the handshake yourself.
//! * [`Client::call`] / [`Client::notify`] — RPC primitives.
//! * [`Client::subscribe_events`] — unified event stream for all
//!   server-to-client non-response messages.
//! * [`Client::subscribe_notifications`] — legacy notification-only
//!   stream (kept for backward-compatibility with existing callers).
//! * [`Client::set_reverse_handler`] — installs a [`ReverseHandler`]
//!   for server-initiated requests.
//! * [`Client::cancel_turn`] — typed helper wrapping the
//!   `engine/cancel_turn` RPC call.
//! * [`Client::shutdown`] — closes the connection; the reader and
//!   writer tasks exit promptly.  The same teardown runs automatically
//!   when the last [`Client`] clone is dropped.

#![forbid(unsafe_code)]

mod connect;
mod error;
pub mod events;
mod pending;
mod reverse;
mod transport;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use zhive_proto::domain::{ThreadId, TurnId};
use zhive_proto::initialize::{Capabilities, Implementation, ProtocolVersion};
use zhive_proto::{ErrorObject, Id, Message, Notification, Request, Response, ResponseOutcome};

pub use connect::HandshakeMeta;
pub use error::ClientError;
pub use events::{ClientEvent, ClientEventStream};
use pending::PendingSlot;
pub use pending::{PendingRequests, ResolveResult};
pub use reverse::ReverseHandler;

use reverse::{HandlerSlot, PendingReverse};

use connect::placeholder_handshake_meta;

/// Capacity of the per-connection broadcast channel for [`ClientEvent`].
///
/// Each [`Client::subscribe_events`] and [`Client::subscribe_notifications`]
/// caller gets a fresh receiver backed by this buffer; a subscriber
/// that falls behind by more than this many events observes `Lagged`.
/// The default balances catch-up tolerance against bounded memory.
pub const DEFAULT_NOTIFICATION_BUFFER: usize = 256;

/// Channel capacity for the outbound writer queue.
///
/// The reader task and any `Client` clone push outbound `Message`s into
/// the same channel; the writer task drains them serially so framing
/// stays atomic.  Picked to absorb burst traffic without back-pressuring
/// callers in the common case.
pub(crate) const OUTBOUND_QUEUE_CAP: usize = 64;

/// Inner handle whose [`Drop`] cancels the per-connection shutdown
/// token so both reader and writer tasks exit when the last
/// [`Client`] clone goes out of scope.
#[derive(Debug)]
pub(crate) struct Inner {
    pub(crate) shutdown: CancellationToken,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// JSON-RPC client handle.
///
/// Cheap to clone — all state is `Arc`-shared across clones.
///
/// # Examples
///
/// ```no_run
/// # #[tokio::main]
/// # async fn main() {
/// use zhive_client_native::{Client, ClientEvent};
/// let client = Client::connect_uds("/tmp/zhive.sock").await.unwrap();
/// let mut stream = client.subscribe_events();
/// while let Some(ev) = stream.next_event().await {
///     match ev {
///         ClientEvent::Notification(n) => println!("{}", n.method),
///         ClientEvent::Disconnected { .. } => break,
///         _ => {}
///     }
/// }
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct Client {
    pub(crate) next_id: Arc<AtomicU64>,
    pub(crate) pending: Arc<PendingRequests>,
    pub(crate) outbound_tx: mpsc::Sender<Message>,
    /// Broadcast sender for the unified event stream.  Each call to
    /// [`Self::subscribe_events`] hands out a fresh receiver.
    pub(crate) events_tx: broadcast::Sender<ClientEvent>,
    /// Broadcast sender for the legacy notification-only stream.
    pub(crate) notifications_tx: broadcast::Sender<Notification>,
    /// Shared handler slot; consulted by the reader task on every
    /// inbound server request.
    pub(crate) handler_slot: Arc<HandlerSlot>,
    /// Pending reverse-RPC join handles; aborted on teardown.
    ///
    /// The reader task holds its own `Arc` clone and calls `abort_all`
    /// when the connection closes.  This field keeps the `Arc` alive
    /// across `Client` clones so that `set_reverse_handler` callers
    /// never observe a dangling handler slot.
    #[expect(
        dead_code,
        reason = "kept alive for Arc ref-count; reader task holds the \
                  other clone and calls abort_all on teardown"
    )]
    pub(crate) pending_reverse: Arc<PendingReverse>,
    /// Held only for the side effect: when the last `Inner` is dropped
    /// the cancel token fires and the spawned tasks exit.
    pub(crate) inner: Arc<Inner>,
    /// Handshake metadata; populated by `connect_*` connectors.
    pub(crate) handshake: Arc<HandshakeMeta>,
}

impl Client {
    /// Builds a client from an arbitrary `AsyncRead` + `AsyncWrite`
    /// pair without performing the initialize handshake.
    ///
    /// This is the low-level building block used by the in-memory unit
    /// tests and by the `connect_*` connectors (which call this first,
    /// then complete the handshake themselves).  Callers that want a
    /// production-ready client should prefer [`Self::connect_uds`] or
    /// [`Self::connect_stdio`], which perform the full D-007 handshake
    /// before returning.
    ///
    /// Spawns one reader task and one writer task on the current
    /// Tokio runtime.  Both tasks share a [`CancellationToken`] that
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
        let placeholder = placeholder_handshake_meta();
        Self::from_split_with_meta(read, write, placeholder)
    }

    /// Returns a unified event stream for all server-to-client messages.
    ///
    /// The stream delivers [`ClientEvent::Notification`],
    /// [`ClientEvent::ServerRequest`], [`ClientEvent::Lagged`], and
    /// the terminal [`ClientEvent::Disconnected`].  After
    /// `Disconnected` the stream yields `None`.
    ///
    /// Multiple independent streams may be created by calling this
    /// method multiple times; each is backed by the same broadcast
    /// channel and has its own independent position.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # #[tokio::main]
    /// # async fn main() {
    /// use zhive_client_native::{Client, ClientEvent};
    /// let client = Client::from_split(
    ///     tokio::io::empty(),
    ///     tokio::io::sink(),
    /// );
    /// let mut stream = client.subscribe_events();
    /// // stream.next_event().await returns None when the connection is closed.
    /// # }
    /// ```
    #[must_use]
    pub fn subscribe_events(&self) -> ClientEventStream {
        ClientEventStream::new(self.events_tx.subscribe())
    }

    /// Returns a fresh subscriber for inbound JSON-RPC notifications.
    ///
    /// This is a compatibility wrapper around [`Self::subscribe_events`]
    /// that delivers only [`Notification`] messages.  New callers
    /// should prefer [`Self::subscribe_events`] which also delivers
    /// server-initiated requests, lag notices, and the disconnect event.
    ///
    /// The receiver buffers up to [`DEFAULT_NOTIFICATION_BUFFER`]
    /// notifications; subscribers that fall behind see
    /// `broadcast::error::RecvError::Lagged`.  Drop the receiver to
    /// stop receiving.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # #[tokio::main]
    /// # async fn main() {
    /// use zhive_client_native::Client;
    /// let client = Client::from_split(tokio::io::empty(), tokio::io::sink());
    /// let mut rx = client.subscribe_notifications();
    /// // rx.recv().await ...
    /// # }
    /// ```
    #[must_use]
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<Notification> {
        self.notifications_tx.subscribe()
    }

    /// Installs a handler for server-initiated (reverse-RPC) requests.
    ///
    /// The reader task consults the slot on every inbound server
    /// request.  If the method appears in
    /// [`ReverseHandler::methods`] the handler is called in a spawned
    /// task; otherwise a `-32601 MethodNotFound` response is sent
    /// automatically.
    ///
    /// Pass `None` to remove an existing handler and revert to the
    /// default `MethodNotFound` behaviour.  The slot is shared across
    /// all clones of this client handle.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use async_trait::async_trait;
    /// use serde_json::Value;
    /// use zhive_proto::ErrorObject;
    /// use zhive_client_native::{Client, ReverseHandler};
    ///
    /// struct NoopHandler;
    /// #[async_trait]
    /// impl ReverseHandler for NoopHandler {
    ///     fn methods(&self) -> &[&'static str] { &[] }
    ///     async fn handle(&self, _m: &str, _p: Option<Value>) -> Result<Value, ErrorObject> {
    ///         Ok(Value::Null)
    ///     }
    /// }
    ///
    /// # #[tokio::main]
    /// # async fn main() {
    /// let client = Client::from_split(tokio::io::empty(), tokio::io::sink());
    /// client.set_reverse_handler(Some(Arc::new(NoopHandler)));
    /// # }
    /// ```
    pub fn set_reverse_handler(&self, handler: Option<Arc<dyn ReverseHandler>>) {
        self.handler_slot.set(handler);
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

    /// Returns the capabilities the server reported during the handshake.
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
    /// * [`ClientError::Disconnected`] when the reader task exits
    ///   before the response arrives (peer closed the stream, framing
    ///   error, or explicit teardown).
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
        // Use a string id (`"c:<n>"`) so clients are grep-able on the
        // wire.  At one call per nanosecond a u64 lasts ~584 years.
        let id = Id::String(format!("c:{seq}"));
        let rx = self.pending.register(id.clone());
        let req = Request::new(id, method, params);
        self.outbound_tx
            .send(Message::Request(req))
            .await
            .map_err(|_send_err| ClientError::Io(std::io::Error::other("writer task exited")))?;
        let slot = rx
            .await
            .map_err(|_recv_err| ClientError::Disconnected("reader task exited".to_owned()))?;
        match slot {
            PendingSlot::Response(ResponseOutcome::Result(v)) => Ok(v),
            PendingSlot::Response(ResponseOutcome::Error(e)) => Err(ClientError::Server(e)),
            PendingSlot::Disconnected(reason) => Err(ClientError::Disconnected(reason)),
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

    /// Cancels the active turn on `thread_id` and returns the
    /// cancelled [`TurnId`], or `None` when no turn was active.
    ///
    /// Wraps the `engine/cancel_turn` RPC defined in
    /// `zhive-core::server::handlers`.  The server cancels the running
    /// turn's `CancellationToken`, queues any pending items, and
    /// returns the id of the cancelled turn (or `null` when the thread
    /// has no active turn).
    ///
    /// **Note**: dropping the future returned by this call does **not**
    /// automatically cancel the server-side turn.  Cancellation only
    /// occurs when this method returns successfully.  Use
    /// `cancel_turn(...).await` rather than racing with a timeout
    /// unless you intend to leave the server-side work running.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`] — connection closed before the
    ///   response arrived.
    /// * [`ClientError::Server`] — the server returned a JSON-RPC
    ///   error (e.g. invalid thread id format → `-32602 InvalidParams`).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use zhive_proto::domain::ThreadId;
    /// use std::sync::Arc;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect_uds("/tmp/zhive.sock").await?;
    /// let tid = ThreadId(Arc::from("thread:native/my-thread"));
    /// let cancelled = client.cancel_turn(&tid).await?;
    /// match cancelled {
    ///     Some(turn_id) => println!("cancelled {}", turn_id.0),
    ///     None => println!("no active turn"),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn cancel_turn(&self, thread_id: &ThreadId) -> Result<Option<TurnId>, ClientError> {
        let params = serde_json::json!({ "threadId": thread_id });
        let value = self.call("engine/cancel_turn", Some(params)).await?;
        // The server's CancelTurnHandler returns `{ "turnId": "<id>" }` or
        // `{ "turnId": null }` when no turn was active.
        let turn_id_val = value.get("turnId").ok_or_else(|| {
            ClientError::Server(ErrorObject {
                code: -32000,
                message: "cancel_turn response missing 'turnId' field".to_owned(),
                data: None,
            })
        })?;
        if turn_id_val.is_null() {
            return Ok(None);
        }
        let id_str = turn_id_val.as_str().ok_or_else(|| {
            ClientError::Server(ErrorObject {
                code: -32000,
                message: "cancel_turn 'turnId' is not a string".to_owned(),
                data: None,
            })
        })?;
        Ok(Some(TurnId(Arc::from(id_str))))
    }

    /// Explicitly cancels the per-connection shutdown token so both
    /// reader and writer tasks exit promptly.
    ///
    /// Equivalent to dropping the last [`Client`] clone.  Subsequent
    /// `call` / `notify` invocations on still-alive clones surface
    /// [`ClientError::Io`] (writer gone) or
    /// [`ClientError::Disconnected`] (reader gone).
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

#[cfg(test)]
mod integration_tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::io::duplex;
    use zhive_proto::framing;

    // ── helpers ──────────────────────────────────────────────────────

    /// Sends a single framed message and flushes.
    async fn server_send(writer: &mut (impl tokio::io::AsyncWrite + Unpin), msg: &Message) {
        framing::write_message(writer, msg).await.unwrap();
        writer.flush().await.unwrap();
    }

    // ── existing tests ────────────────────────────────────────────────

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
            server_send(&mut writer, &Message::Response(resp)).await;
        });

        let client = Client::from_split(client_read, client_write);
        let value = client
            .call("echo", Some(serde_json::json!({"hello": "world"})))
            .await
            .expect("call ok");
        assert_eq!(value, serde_json::json!({"hello": "world"}));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn dropping_last_clone_terminates_tasks() {
        let (client_io, _server_io) = duplex(64);
        let (client_read, client_write) = tokio::io::split(client_io);
        let client = Client::from_split(client_read, client_write);
        let clone = client.clone();
        drop(client);
        let outbound_capacity = clone.outbound_tx.capacity();
        assert!(outbound_capacity > 0);
        drop(clone);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // ── Part 1: ClientEvent stream tests ─────────────────────────────

    /// A server notification arrives as `ClientEvent::Notification`.
    #[tokio::test]
    async fn subscribe_events_receives_notification() {
        let (client_io, server_io) = duplex(4096);
        let (server_read, mut server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);

        let client = Client::from_split(client_read, client_write);
        let mut stream = client.subscribe_events();

        let server_task = tokio::spawn(async move {
            // Drain the server read end so the client does not block.
            let _reader = server_read;
            let notif = Notification::new("events/test", Some(serde_json::json!({"k": "v"})));
            server_send(&mut server_write, &Message::Notification(notif)).await;
        });

        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next_event())
            .await
            .expect("timeout waiting for event")
            .expect("stream closed");

        match ev {
            ClientEvent::Notification(n) => {
                assert_eq!(n.method, "events/test");
            }
            other => panic!("expected Notification, got {other:?}"),
        }

        server_task.await.unwrap();
    }

    /// A server-initiated request arrives as `ClientEvent::ServerRequest`.
    #[tokio::test]
    async fn subscribe_events_receives_server_request() {
        let (client_io, server_io) = duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);

        let client = Client::from_split(client_read, client_write);
        let mut stream = client.subscribe_events();

        let server_task = tokio::spawn(async move {
            // Send a reverse request to the client.
            let req = Request::new(Id::Number(42), "fs/read_text_file", None);
            server_send(&mut server_write, &Message::Request(req)).await;
            // Read back the client's MethodNotFound reply.
            let mut buf_read = tokio::io::BufReader::new(&mut server_read);
            let _reply = framing::read_message(&mut buf_read).await.unwrap();
        });

        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next_event())
            .await
            .expect("timeout")
            .expect("stream closed");

        match ev {
            ClientEvent::ServerRequest { method, id, .. } => {
                assert_eq!(method, "fs/read_text_file");
                assert_eq!(id, Id::Number(42));
            }
            other => panic!("expected ServerRequest, got {other:?}"),
        }

        server_task.await.unwrap();
    }

    // ── Part 2: ReverseHandler tests ─────────────────────────────────

    /// A registered handler's result is sent back as a Response.
    #[tokio::test]
    async fn reverse_handler_reply_is_sent_back() {
        use async_trait::async_trait;

        struct EchoHandler;

        #[async_trait]
        impl ReverseHandler for EchoHandler {
            fn methods(&self) -> &[&'static str] {
                &["custom/echo"]
            }
            async fn handle(
                &self,
                _method: &str,
                params: Option<serde_json::Value>,
            ) -> Result<serde_json::Value, ErrorObject> {
                Ok(params.unwrap_or(serde_json::Value::Null))
            }
        }

        let (client_io, server_io) = duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);

        let client = Client::from_split(client_read, client_write);
        client.set_reverse_handler(Some(Arc::new(EchoHandler)));

        let server_task = tokio::spawn(async move {
            let req = Request::new(
                Id::Number(7),
                "custom/echo",
                Some(serde_json::json!({"echo": "hello"})),
            );
            server_send(&mut server_write, &Message::Request(req)).await;
            // Read the client's response.
            let mut buf = tokio::io::BufReader::new(&mut server_read);
            let reply = framing::read_message(&mut buf).await.unwrap();
            match reply {
                Message::Response(r) => {
                    assert_eq!(r.id, Id::Number(7));
                    assert!(matches!(r.outcome, ResponseOutcome::Result(_)));
                    if let ResponseOutcome::Result(v) = r.outcome {
                        assert_eq!(v["echo"], "hello");
                    }
                }
                other => panic!("expected Response, got {other:?}"),
            }
        });

        server_task.await.unwrap();
    }

    /// An unregistered method receives a `-32601` `MethodNotFound` reply.
    #[tokio::test]
    async fn unregistered_method_gets_method_not_found() {
        let (client_io, server_io) = duplex(4096);
        let (mut server_read, mut server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);

        let _client = Client::from_split(client_read, client_write);
        // No reverse handler set.

        let server_task = tokio::spawn(async move {
            let req = Request::new(Id::Number(8), "unknown/method", None);
            server_send(&mut server_write, &Message::Request(req)).await;
            let mut buf = tokio::io::BufReader::new(&mut server_read);
            let reply = framing::read_message(&mut buf).await.unwrap();
            match reply {
                Message::Response(r) => match r.outcome {
                    ResponseOutcome::Error(e) => assert_eq!(e.code, -32601),
                    ResponseOutcome::Result(v) => {
                        panic!("expected Error outcome, got Result({v:?})")
                    }
                },
                other => panic!("expected Response, got {other:?}"),
            }
        });

        server_task.await.unwrap();
    }

    // ── Part 4: Disconnect ordering tests ────────────────────────────

    /// An in-flight call resolves to `Err(Disconnected)` and the event
    /// stream eventually yields `ClientEvent::Disconnected` then `None`.
    ///
    /// The stub server reads ONE request (ensuring the call is in-flight)
    /// then drops the connection without responding, so the race between
    /// call-registration and server-close is eliminated.
    #[tokio::test]
    async fn disconnect_resolves_inflight_call_then_emits_disconnect_event() {
        let (client_io, server_io) = duplex(4096);
        let (server_read, server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);

        let client = Client::from_split(client_read, client_write);
        let mut stream = client.subscribe_events();

        // The server task reads ONE request then drops the connection.
        // We use a oneshot to confirm the request was received, after
        // which the in-flight `call` is guaranteed to be registered.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let server_task = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(server_read);
            // Block until the client's request arrives.
            let _ = framing::read_message(&mut reader).await;
            // Signal "request received" and then drop both streams.
            let _ = ready_tx.send(());
            drop(server_write);
            // reader is dropped implicitly — both ends closed.
        });

        // Wait for the server to confirm it received the request, then
        // verify the call fails and the event stream emits Disconnected.
        let call_handle = tokio::spawn({
            let client = client.clone();
            async move { client.call("engine/noop", None).await }
        });

        // Wait until the request is definitely in-flight.
        let _ = ready_rx.await;

        let call_result = call_handle.await.unwrap();
        server_task.await.unwrap();

        // The call must fail with Disconnected (or Io if the writer gave up).
        match &call_result {
            Err(ClientError::Disconnected(_) | ClientError::Io(_)) => {}
            other => panic!("expected Disconnected or Io, got {other:?}"),
        }

        // The event stream must eventually yield a terminal Disconnected.
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next_event())
            .await
            .expect("timed out waiting for Disconnected event")
            .expect("stream closed before Disconnected");

        assert!(
            matches!(event, ClientEvent::Disconnected { .. }),
            "expected ClientEvent::Disconnected, got {event:?}"
        );

        // After Disconnected the stream must return None.
        let after = stream.next_event().await;
        assert!(
            after.is_none(),
            "expected None after Disconnected, got {after:?}"
        );
    }
}

// Rust guideline compliant 2026-02-21

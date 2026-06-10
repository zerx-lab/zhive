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
//! * [`Client::cancel_session`] — ACP `session/cancel` notification helper
//!   (fire-and-forget, distinct from `cancel_turn`).
//! * [`Client::shutdown`] — closes the connection with a 5-second drain
//!   wait and then exits.  The same best-effort teardown runs automatically
//!   when the last [`Client`] clone is dropped.
//! * **Typed RPC helpers** — [`Client::start_turn`], [`Client::compact`],
//!   [`Client::fork_thread`], [`Client::list_threads`],
//!   [`Client::resume_thread`], [`Client::get_items`],
//!   [`Client::enqueue_steer`], [`Client::enqueue_follow_up`],
//!   [`Client::enqueue_next_turn`], [`Client::delete_thread`],
//!   [`Client::rename_thread`], [`Client::search_threads`],
//!   [`Client::tools_list`], [`Client::resume_permission`] — all take
//!   strongly-typed `Params` / `Result` from `zhive_proto::rpc` and
//!   internally call [`Client::call`].
//! * [`Client::set_permission_handler`] — convenience wrapper that installs a
//!   typed [`PermissionHandler`] for the `session/request_permission`
//!   reverse-RPC method.

#![forbid(unsafe_code)]

mod connect;
mod error;
pub mod events;
mod pending;
pub mod permission_handler;
mod reverse;
mod transport;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use zhive_proto::domain::{ThreadId, TurnId};
use zhive_proto::initialize::{Capabilities, Implementation, ProtocolVersion};
use zhive_proto::permission::ResumePermissionParams;
use zhive_proto::rpc::{
    CompactParams, CompactResult, DeleteThreadParams, DeleteThreadResult, ForkParams, ForkResult,
    GetItemsParams, GetItemsResult, InjectionAck, InjectionParams, ListCheckpointsParams,
    ListCheckpointsResult, ListThreadsParams, ListThreadsResult, RenameThreadParams,
    RenameThreadResult, RestoreParams, RestoreResult, ResumePermissionResult, ResumeThreadParams,
    ResumeThreadResult, SearchThreadsParams, SearchThreadsResult, StartTurnParams, StartTurnResult,
    ToolListResult,
};
use zhive_proto::{
    ErrorObject, Id, Message, Notification, Request, Response, ResponseOutcome, methods,
};

pub use connect::{ClientBuilder, HandshakeMeta};
pub use error::ClientError;
pub use events::{ClientEvent, ClientEventStream};
use pending::PendingSlot;
pub use pending::{PendingRequests, ResolveResult};
pub use permission_handler::{PermissionHandler, PermissionReverseAdapter};
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

/// How long [`Client::shutdown`] waits for the reader task to drain
/// before giving up and returning.
///
/// Five seconds is generous enough to absorb normal I/O latency while
/// keeping shutdown deterministic in tests.  Once the deadline passes
/// the shutdown returns anyway — the reader/writer tasks will exit on
/// their own when the cancel token fires.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Inner handle whose [`Drop`] cancels the per-connection shutdown
/// token so both reader and writer tasks exit when the last
/// [`Client`] clone goes out of scope.
#[derive(Debug)]
pub(crate) struct Inner {
    pub(crate) shutdown: CancellationToken,
    /// Notified once when the reader task exits its teardown sequence.
    ///
    /// `shutdown().await` registers a waiter on this [`Notify`] *before*
    /// cancelling the shutdown token to avoid a race where the reader
    /// fires `notify_one()` before the waiter is registered.
    pub(crate) worker_done: Arc<Notify>,
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
/// let client = Client::connect("/tmp/zhive.sock").await.unwrap();
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
    /// let client = Client::connect("/tmp/zhive.sock").await.unwrap();
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
    /// let client = Client::connect("/tmp/zhive.sock").await.unwrap();
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
    /// let client = Client::connect("/tmp/zhive.sock").await.unwrap();
    /// assert!(!client.server_info().name.is_empty());
    /// # }
    /// ```
    #[must_use]
    pub fn server_info(&self) -> &Implementation {
        &self.handshake.server_info
    }

    // ── Typed RPC helpers ─────────────────────────────────────────────────────

    /// Starts a new turn on a thread; returns the new [`TurnId`].
    ///
    /// Wraps `engine/start_turn`.  Use [`StartTurnParams`] to seed the
    /// turn with user input and an optional permission scope.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`] — connection closed before the response.
    /// * [`ClientError::Server`] — server returned a JSON-RPC error.
    /// * [`ClientError::Decode`] — response body did not match [`StartTurnResult`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::StartTurnParams;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let p = StartTurnParams::new(ThreadId(Arc::from("thread:native/x")), vec![], None);
    /// let result = client.start_turn(p).await?;
    /// println!("turn: {}", result.turn_id.0);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_turn(
        &self,
        params: StartTurnParams,
    ) -> Result<StartTurnResult, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self.call(methods::METHOD_START_TURN, Some(v)).await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Compacts the transcript on a thread; returns the compaction result.
    ///
    /// Wraps `engine/compact`.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::hook::CompactTrigger;
    /// use zhive_proto::rpc::CompactParams;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let p = CompactParams::new(ThreadId(Arc::from("thread:native/x")), CompactTrigger::Manual);
    /// let result = client.compact(p).await?;
    /// println!("compacted {} entries", result.entries_compacted);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn compact(&self, params: CompactParams) -> Result<CompactResult, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self.call(methods::METHOD_COMPACT, Some(v)).await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Forks an existing thread at an optional item boundary.
    ///
    /// Wraps `thread/fork`.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::ForkParams;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let p = ForkParams::new(ThreadId(Arc::from("thread:native/src")), None, false);
    /// let result = client.fork_thread(p).await?;
    /// println!("fork: {}", result.new_thread_id.0);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn fork_thread(&self, params: ForkParams) -> Result<ForkResult, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self.call(methods::METHOD_THREAD_FORK, Some(v)).await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Lists a thread's revertable workspace checkpoints (oldest first).
    ///
    /// Wraps `engine/list_checkpoints`. Powers the rewind picker.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::ListCheckpointsParams;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let p = ListCheckpointsParams::new(ThreadId(Arc::from("thread:native/x")));
    /// let result = client.list_checkpoints(p).await?;
    /// println!("{} checkpoints", result.checkpoints.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_checkpoints(
        &self,
        params: ListCheckpointsParams,
    ) -> Result<ListCheckpointsResult, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self.call(methods::METHOD_LIST_CHECKPOINTS, Some(v)).await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Reverts workspace files to a checkpoint and rewinds the conversation.
    ///
    /// Wraps `engine/restore`. Returns the new branch thread the conversation
    /// was forked into plus revert counts.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use std::sync::Arc;
    /// use zhive_proto::domain::{ThreadId, TurnId};
    /// use zhive_proto::rpc::RestoreParams;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let p = RestoreParams::new(
    ///     ThreadId(Arc::from("thread:native/x")),
    ///     TurnId(Arc::from("turn:thread:native/x/0")),
    /// );
    /// let result = client.restore(p).await?;
    /// println!("reverted {} files into {}", result.reverted, result.new_thread_id.0);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn restore(&self, params: RestoreParams) -> Result<RestoreResult, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self.call(methods::METHOD_RESTORE, Some(v)).await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Lists persisted threads, optionally filtered by working directory.
    ///
    /// Wraps `thread/list`.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use zhive_proto::rpc::ListThreadsParams;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let result = client.list_threads(ListThreadsParams::new(None)).await?;
    /// println!("{} thread(s)", result.threads.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_threads(
        &self,
        params: ListThreadsParams,
    ) -> Result<ListThreadsResult, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self.call(methods::METHOD_THREAD_LIST, Some(v)).await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Restores a persisted thread into engine memory.
    ///
    /// Wraps `engine/resume_thread`.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::ResumeThreadParams;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let p = ResumeThreadParams::new(ThreadId(Arc::from("thread:native/abc")));
    /// let result = client.resume_thread(p).await?;
    /// println!("restored {} items", result.items_restored);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn resume_thread(
        &self,
        params: ResumeThreadParams,
    ) -> Result<ResumeThreadResult, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self.call(methods::METHOD_RESUME_THREAD, Some(v)).await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Fetches history items for a thread or a specific turn.
    ///
    /// Wraps `thread/get_items`.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::GetItemsParams;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let p = GetItemsParams::new(ThreadId(Arc::from("thread:native/x")), None, None, None);
    /// let result = client.get_items(p).await?;
    /// println!("{} item(s)", result.items.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_items(&self, params: GetItemsParams) -> Result<GetItemsResult, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self.call(methods::METHOD_THREAD_GET_ITEMS, Some(v)).await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Enqueues items into the active turn's `steer` queue.
    ///
    /// Wraps `session/enqueue_steer`.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::InjectionParams;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let p = InjectionParams::new(ThreadId(Arc::from("thread:native/x")), vec![]);
    /// let ack = client.enqueue_steer(p).await?;
    /// assert!(ack.accepted);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn enqueue_steer(
        &self,
        params: InjectionParams,
    ) -> Result<InjectionAck, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self.call(methods::METHOD_ENQUEUE_STEER, Some(v)).await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Enqueues items into the active turn's `follow_up` queue.
    ///
    /// Wraps `session/enqueue_follow_up`.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::InjectionParams;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let p = InjectionParams::new(ThreadId(Arc::from("thread:native/x")), vec![]);
    /// let ack = client.enqueue_follow_up(p).await?;
    /// assert!(ack.accepted);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn enqueue_follow_up(
        &self,
        params: InjectionParams,
    ) -> Result<InjectionAck, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self
            .call(methods::METHOD_ENQUEUE_FOLLOW_UP, Some(v))
            .await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Enqueues items for the next turn.
    ///
    /// Wraps `session/enqueue_next_turn`.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::InjectionParams;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let p = InjectionParams::new(ThreadId(Arc::from("thread:native/x")), vec![]);
    /// let ack = client.enqueue_next_turn(p).await?;
    /// assert!(ack.accepted);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn enqueue_next_turn(
        &self,
        params: InjectionParams,
    ) -> Result<InjectionAck, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self
            .call(methods::METHOD_ENQUEUE_NEXT_TURN, Some(v))
            .await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Permanently deletes a thread and its history.
    ///
    /// Wraps `thread/delete`.  The server returns an error instead of this
    /// result when the thread has an active turn.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::DeleteThreadParams;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let p = DeleteThreadParams::new(ThreadId(Arc::from("thread:native/old")));
    /// let result = client.delete_thread(p).await?;
    /// println!("deleted: {}", result.deleted);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn delete_thread(
        &self,
        params: DeleteThreadParams,
    ) -> Result<DeleteThreadResult, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self.call(methods::METHOD_THREAD_DELETE, Some(v)).await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Renames or re-labels a thread.
    ///
    /// Wraps `thread/rename`.  An empty `name` clears the label.  `renamed:
    /// true` in the result means the rename was accepted into the async
    /// persistence queue, not necessarily flushed to disk.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_proto::rpc::RenameThreadParams;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let p = RenameThreadParams::new(
    ///     ThreadId(Arc::from("thread:native/x")),
    ///     "my feature branch".into(),
    /// );
    /// let result = client.rename_thread(p).await?;
    /// println!("renamed: {}", result.renamed);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn rename_thread(
        &self,
        params: RenameThreadParams,
    ) -> Result<RenameThreadResult, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self.call(methods::METHOD_THREAD_RENAME, Some(v)).await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Searches threads by name, preview, or working directory.
    ///
    /// Wraps `thread/search`.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use zhive_proto::rpc::SearchThreadsParams;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let p = SearchThreadsParams::new("refactor".into(), None);
    /// let result = client.search_threads(p).await?;
    /// println!("{} match(es)", result.threads.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn search_threads(
        &self,
        params: SearchThreadsParams,
    ) -> Result<SearchThreadsResult, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self.call(methods::METHOD_THREAD_SEARCH, Some(v)).await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Enumerates all tools registered with the engine.
    ///
    /// Wraps `tools/list`.  No params are required.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let result = client.tools_list().await?;
    /// for spec in &result.tools {
    ///     println!("{}: {:?}", spec.name, spec.kind);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn tools_list(&self) -> Result<ToolListResult, ClientError> {
        let resp = self.call(methods::METHOD_TOOLS_LIST, None).await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Resolves a deferred permission request on a thread.
    ///
    /// Wraps `session/resume_permission`.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Disconnected`], [`ClientError::Server`],
    ///   [`ClientError::Decode`] — see [`Self::call`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use zhive_proto::permission::{ResumeOutcome, ResumePermissionParams};
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let p = ResumePermissionParams::new("perm:1", ResumeOutcome::Cancelled);
    /// let result = client.resume_permission(p).await?;
    /// println!("status: {:?}", result.status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn resume_permission(
        &self,
        params: ResumePermissionParams,
    ) -> Result<ResumePermissionResult, ClientError> {
        let v = serde_json::to_value(&params).map_err(|e| ClientError::Decode(e.to_string()))?;
        let resp = self
            .call(methods::METHOD_RESUME_PERMISSION, Some(v))
            .await?;
        serde_json::from_value(resp).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Installs a typed permission handler for `session/request_permission`.
    ///
    /// Convenience wrapper that creates a [`PermissionReverseAdapter`] from
    /// `handler` and passes it to [`Self::set_reverse_handler`].  The adapter
    /// decodes inbound params into [`RequestPermissionRequest`] and encodes the
    /// returned [`PermissionOutcome`] back onto the wire.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use async_trait::async_trait;
    /// use zhive_proto::permission::{PermissionOutcome, RequestPermissionRequest};
    /// use zhive_client_native::{Client, PermissionHandler};
    ///
    /// struct AlwaysAllow;
    ///
    /// #[async_trait]
    /// impl PermissionHandler for AlwaysAllow {
    ///     async fn on_permission(&self, req: RequestPermissionRequest) -> PermissionOutcome {
    ///         let id = req.options.first().map(|o| o.id.clone()).unwrap_or_default();
    ///         PermissionOutcome::Selected { option_id: id }
    ///     }
    /// }
    ///
    /// # async fn run() {
    /// let client = Client::from_split(tokio::io::empty(), tokio::io::sink());
    /// client.set_permission_handler(Arc::new(AlwaysAllow));
    /// # }
    /// ```
    pub fn set_permission_handler<H: PermissionHandler + 'static>(&self, handler: Arc<H>) {
        self.set_reverse_handler(Some(Arc::new(PermissionReverseAdapter::new(handler))));
    }

    // ── Primitive call/notify ─────────────────────────────────────────────────

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
    /// let client = Client::connect("/tmp/zhive.sock").await?;
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
        let value = self.call(methods::METHOD_CANCEL_TURN, Some(params)).await?;
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

    /// Shuts the connection down, waiting up to 5 s for the reader task
    /// to drain before returning.
    ///
    /// Cancels the per-connection shutdown token, closes the outbound
    /// queue, and then awaits a signal from the reader task.  If the
    /// reader does not signal within [`SHUTDOWN_TIMEOUT`] (5 s) the
    /// method returns anyway — the tasks will eventually exit when the
    /// cancel token is observed.
    ///
    /// Subsequent `call` / `notify` invocations on still-alive clones
    /// surface [`ClientError::Io`] (writer gone) or
    /// [`ClientError::Disconnected`] (reader gone).
    ///
    /// # Errors
    ///
    /// This method never returns an `Err` variant; the return type is
    /// `Result<(), ClientError>` so that callers can use `let _ =` to
    /// silence the return value symmetrically with other client methods.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # #[tokio::main]
    /// # async fn main() {
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::from_split(tokio::io::empty(), tokio::io::sink());
    /// let _ = client.shutdown().await;
    /// # }
    /// ```
    pub async fn shutdown(self) -> Result<(), ClientError> {
        // Register the waiter *before* cancelling the token so that we
        // do not miss the notify_one() call if the reader exits very
        // quickly after the cancel.  See design doc §risk-2 for details.
        let notified = self.inner.worker_done.notified();
        self.inner.shutdown.cancel();
        // Close the outbound queue so the writer task exits promptly.
        drop(self.outbound_tx);
        // Wait for the reader to complete its ordered teardown, or
        // give up after SHUTDOWN_TIMEOUT and return anyway.
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, notified).await;
        Ok(())
    }

    /// Sends an ACP `session/cancel` notification for `thread_id`.
    ///
    /// This is a fire-and-forget notification distinct from
    /// [`Self::cancel_turn`]:
    ///
    /// * `cancel_turn` is an engine-private *request* (`engine/cancel_turn`)
    ///   that returns the cancelled [`TurnId`] synchronously.
    /// * `cancel_session` is an ACP-standard *notification*
    ///   (`session/cancel`) — the server processes it asynchronously and
    ///   may emit a `session/aborted` notification in response.
    ///
    /// The server registers a `session/cancel` handler that calls
    /// `engine.cancel_turn(thread_id)` upon receipt (landed in B7 alongside
    /// `register_engine_handlers`).  The turn is cancelled asynchronously;
    /// the engine may emit a `session/aborted` notification once the
    /// cancellation propagates.
    ///
    /// # Errors
    ///
    /// [`ClientError::Io`] when the outbound queue is closed (writer task
    /// has already exited).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example() -> Result<(), zhive_client_native::ClientError> {
    /// use std::sync::Arc;
    /// use zhive_proto::domain::ThreadId;
    /// use zhive_client_native::Client;
    ///
    /// let client = Client::connect("/tmp/zhive.sock").await?;
    /// let tid = ThreadId(Arc::from("thread:native/my-session"));
    /// client.cancel_session(&tid).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn cancel_session(&self, thread_id: &ThreadId) -> Result<(), ClientError> {
        let params = serde_json::json!({ "threadId": thread_id });
        self.notify(methods::METHOD_SESSION_CANCEL, Some(params))
            .await
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

    // ── Part 5: shutdown tests ────────────────────────────────────────

    /// `shutdown` notifies the reader and returns once the reader
    /// signals completion; the outbound queue is closed first so the
    /// writer exits promptly.
    ///
    /// We drive this deterministically: the server side keeps the read
    /// end alive (so the reader doesn't error), waits for the outbound
    /// queue to close, then drops its write end — which makes the reader
    /// observe EOF and fire `worker_done`.
    #[tokio::test]
    async fn shutdown_waits_for_reader() {
        let (client_io, server_io) = duplex(4096);
        let (server_read, server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);

        let client = Client::from_split(client_read, client_write);

        let server_task = tokio::spawn(async move {
            // Keep the read end alive until the client's outbound queue
            // is fully closed (writer exits), then close the write end.
            // When the write end closes the client reader observes EOF.
            let _read_end = server_read;
            // Wait a short grace period for the client to call shutdown
            // and close its outbound_tx.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            // Dropping server_write causes client reader to see EOF.
            drop(server_write);
        });

        // shutdown should complete without timing out.
        tokio::time::timeout(std::time::Duration::from_secs(2), client.shutdown())
            .await
            .expect("shutdown must return within deadline")
            .expect("shutdown never errors");

        server_task.await.unwrap();
    }

    /// When the reader task does not signal within `SHUTDOWN_TIMEOUT` (5 s)
    /// `shutdown` must still return — it must not block forever.
    ///
    /// We use `tokio::time::pause` + `advance` to skip the timeout
    /// instantly without real clock delay.
    #[tokio::test(start_paused = true)]
    async fn shutdown_timeout_abort() {
        let (client_io, _server_io) = duplex(64);
        let (client_read, client_write) = tokio::io::split(client_io);

        // `_server_io` is intentionally kept alive so the reader never
        // sees EOF and therefore never fires `worker_done`.
        let client = Client::from_split(client_read, client_write);

        // Drive shutdown and advance the clock past SHUTDOWN_TIMEOUT (5 s)
        // so the timeout branch fires and the method returns.
        let shutdown_fut = client.shutdown();
        tokio::pin!(shutdown_fut);

        // Poll once to let the future register the notified() waiter and
        // cancel the token, then advance time past the 5 s timeout.
        tokio::select! {
            biased;
            result = &mut shutdown_fut => {
                result.expect("shutdown must not error");
            }
            () = tokio::time::sleep(std::time::Duration::from_nanos(1)) => {
                // Advance clock far past SHUTDOWN_TIMEOUT.
                tokio::time::advance(std::time::Duration::from_secs(10)).await;
                // Now the timeout should have fired; drive the future to completion.
                shutdown_fut.await.expect("shutdown must not error after timeout");
            }
        }
    }

    // ── Part 6: cancel_session tests ─────────────────────────────────

    /// `cancel_session` sends a `session/cancel` notification with
    /// `{ "threadId": "<id>" }` in the params.
    #[tokio::test]
    async fn cancel_session_sends_notification() {
        use std::sync::Arc;
        use zhive_proto::domain::ThreadId;

        let (client_io, server_io) = duplex(4096);
        let (server_read, _server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);

        let client = Client::from_split(client_read, client_write);

        let tid = ThreadId(Arc::from("thread:native/test"));

        let server_task = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(server_read);
            let msg = framing::read_message(&mut reader)
                .await
                .expect("server must read a message");
            match msg {
                Message::Notification(n) => {
                    assert_eq!(n.method, "session/cancel");
                    let params = n.params.expect("cancel notification must have params");
                    assert_eq!(
                        params["threadId"], "thread:native/test",
                        "threadId must match"
                    );
                }
                other => panic!("expected Notification, got {other:?}"),
            }
        });

        client
            .cancel_session(&tid)
            .await
            .expect("cancel_session must not error on connected client");

        server_task.await.unwrap();
    }

    /// `cancel_session` on a client whose outbound queue is closed
    /// returns `Err(Io)`.
    ///
    /// We explicitly call `shutdown` (which cancels the token and drops
    /// this clone's `outbound_tx`) then keep a second clone alive.
    /// After the writer task observes the cancel and drains, any
    /// subsequent `notify` on the second clone fails because the
    /// `outbound_rx` has been dropped by the writer.
    #[tokio::test]
    async fn cancel_session_disconnected_returns_err() {
        use std::sync::Arc;
        use zhive_proto::domain::ThreadId;

        let (client_io, _server_io) = duplex(64);
        let (client_read, client_write) = tokio::io::split(client_io);

        let client = Client::from_split(client_read, client_write);
        // Keep a second handle alive for the cancel_session call.
        let client_for_test = client.clone();

        // `shutdown` cancels the token immediately, drops the client's
        // outbound_tx, and waits for the reader to signal done.  The
        // writer also observes the cancel token and exits shortly after.
        let _ = client.shutdown().await;

        // Yield a few times to give the writer task a chance to exit
        // and drop `outbound_rx`; once dropped, any send returns Err.
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        // Small deterministic sleep to absorb scheduling jitter.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let tid = ThreadId(Arc::from("thread:native/gone"));
        let err = client_for_test
            .cancel_session(&tid)
            .await
            .expect_err("cancel_session after shutdown must fail");

        assert!(
            matches!(err, ClientError::Io(_)),
            "expected ClientError::Io, got {err:?}"
        );
    }

    // ── Part 7: ClientBuilder custom client_info test ─────────────────

    /// A custom `client_info` set on [`ClientBuilder`] appears verbatim
    /// in the `initialize` request sent over the wire.
    #[tokio::test]
    async fn builder_custom_client_info_in_handshake() {
        use super::connect::perform_handshake_with_params;
        use zhive_proto::initialize::Implementation;

        let (client_io, server_io) = duplex(4096);
        let (server_read, server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);

        // Stub server: read the initialize request and echo back a valid
        // InitializeResponse, then discard the rest of the connection.
        let server_task = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(server_read);
            let msg = framing::read_message(&mut reader)
                .await
                .expect("server must read initialize");
            let req = match msg {
                Message::Request(r) => r,
                other => panic!("expected Request, got {other:?}"),
            };
            // Verify the clientInfo in the initialize params.
            let params = req.params.expect("initialize must have params");
            let name = params["clientInfo"]["name"]
                .as_str()
                .expect("clientInfo.name must be present");
            let version = params["clientInfo"]["version"]
                .as_str()
                .expect("clientInfo.version must be present");
            assert_eq!(name, "my-app", "clientInfo.name mismatch");
            assert_eq!(version, "2.0.0", "clientInfo.version mismatch");

            // Send back a minimal valid InitializeResponse.
            let resp = zhive_proto::Response::ok(
                req.id,
                serde_json::json!({
                    "protocolVersion": 1,
                    "serverCapabilities": { "cancellation": false },
                    "serverInfo": { "name": "stub", "version": "0.0.0" },
                }),
            );
            let mut writer = server_write;
            server_send(&mut writer, &Message::Response(resp)).await;
        });

        let raw_client = Client::from_split(client_read, client_write);
        let info: Implementation =
            serde_json::from_value(serde_json::json!({"name": "my-app", "version": "2.0.0"}))
                .unwrap();
        let builder = ClientBuilder::new().client_info(info);
        let meta = perform_handshake_with_params(&raw_client, &builder)
            .await
            .expect("handshake must succeed");

        assert_eq!(meta.server_info.name, "stub");

        server_task.await.unwrap();
    }
}

// Rust guideline compliant 2026-02-21

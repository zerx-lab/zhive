//! Unified server-to-client event stream.
//!
//! All inbound server messages that are not direct responses to a
//! `Client::call` — notifications, server-initiated requests, lag
//! notices, and the terminal disconnect event — are delivered through
//! a single [`ClientEvent`] channel.
//!
//! # Usage
//!
//! Obtain a [`ClientEventStream`] from [`crate::Client::subscribe_events`],
//! then call [`ClientEventStream::next_event`] in a loop:
//!
//! ```no_run
//! # async fn example(client: zhive_client_native::Client) {
//! use zhive_client_native::ClientEvent;
//! let mut stream = client.subscribe_events();
//! while let Some(event) = stream.next_event().await {
//!     match event {
//!         ClientEvent::Notification(n) => println!("got notification: {}", n.method),
//!         ClientEvent::ServerRequest { id, method, .. } => println!("reverse RPC: {method}"),
//!         ClientEvent::Lagged(n) => eprintln!("lagged by {n}"),
//!         ClientEvent::Disconnected { reason } => break,
//!         _ => {}
//!     }
//! }
//! # }
//! ```

use tokio::sync::broadcast;
use zhive_proto::Notification;

/// Fused event from the server, delivered via the broadcast channel.
///
/// All non-response inbound messages arrive here: notifications,
/// server-initiated (reverse-RPC) requests, back-pressure lag notices,
/// and the terminal disconnect event.
///
/// After [`ClientEvent::Disconnected`] is yielded by
/// [`ClientEventStream::next_event`] the stream will return `None` on
/// the next call — the stream tracks this locally so callers never see
/// stale events after a disconnect, even if other `Client` clones still
/// hold a broadcast sender.
///
/// # Examples
///
/// ```no_run
/// # async fn example(client: zhive_client_native::Client) {
/// use zhive_client_native::ClientEvent;
/// let mut stream = client.subscribe_events();
/// if let Some(ClientEvent::Notification(n)) = stream.next_event().await {
///     println!("method = {}", n.method);
/// }
/// # }
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ClientEvent {
    /// A JSON-RPC notification pushed by the server (e.g.
    /// `events/turn_started`, `events/permission_requested`).
    Notification(Notification),

    /// A server-initiated request that arrived on the wire.
    ///
    /// If a [`crate::ReverseHandler`] was registered and its
    /// [`crate::ReverseHandler::methods`] list includes this method,
    /// the handler was (or is being) called and the response will be
    /// sent automatically.  This variant is emitted for observability
    /// regardless of whether a handler is registered.
    ServerRequest {
        /// The JSON-RPC id of the incoming request.
        id: zhive_proto::Id,
        /// The method name, e.g. `"fs/read_text_file"`.
        method: String,
        /// Optional params from the wire, already extracted from the
        /// JSON-RPC envelope.
        params: Option<serde_json::Value>,
    },

    /// The subscriber fell behind: `n` events were dropped.
    ///
    /// Corresponds to `broadcast::error::RecvError::Lagged`.
    Lagged(u64),

    /// The connection was closed, either by the server (EOF) or by a
    /// framing error.  After this event the stream yields `None`.
    Disconnected {
        /// Human-readable reason string (e.g. `"unexpected EOF"`).
        reason: String,
    },
}

/// Wraps a [`broadcast::Receiver<ClientEvent>`] with a tidy async API.
///
/// Obtain an instance from [`crate::Client::subscribe_events`].
///
/// # Examples
///
/// ```no_run
/// # async fn example(client: zhive_client_native::Client) {
/// let mut stream = client.subscribe_events();
/// while let Some(event) = stream.next_event().await {
///     println!("{event:?}");
/// }
/// # }
/// ```
pub struct ClientEventStream {
    rx: broadcast::Receiver<ClientEvent>,
    /// Set to `true` after a [`ClientEvent::Disconnected`] has been
    /// delivered; subsequent calls to [`Self::next_event`] return `None`
    /// without polling the underlying receiver.
    disconnected: bool,
}

impl ClientEventStream {
    /// Wraps a raw broadcast receiver.
    pub(crate) fn new(rx: broadcast::Receiver<ClientEvent>) -> Self {
        Self {
            rx,
            disconnected: false,
        }
    }

    /// Returns the next event from the stream, or `None` when the
    /// connection is gone.
    ///
    /// Mapping rules:
    /// * `Ok(ev)` → `Some(ev)`.  If `ev` is
    ///   [`ClientEvent::Disconnected`] the stream is marked terminal
    ///   and subsequent calls return `None`.
    /// * `Err(RecvError::Lagged(n))` → `Some(ClientEvent::Lagged(n))`.
    /// * `Err(RecvError::Closed)` → `None`.
    /// * After [`ClientEvent::Disconnected`] has been returned → `None`
    ///   (tracked locally, does not require all senders to be dropped).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(client: zhive_client_native::Client) {
    /// use zhive_client_native::ClientEvent;
    /// let mut stream = client.subscribe_events();
    /// while let Some(ev) = stream.next_event().await {
    ///     match ev {
    ///         ClientEvent::Disconnected { .. } => break,
    ///         other => drop(other),
    ///     }
    /// }
    /// // stream.next_event().await returns None here.
    /// # }
    /// ```
    pub async fn next_event(&mut self) -> Option<ClientEvent> {
        if self.disconnected {
            return None;
        }
        match self.rx.recv().await {
            Ok(ev) => {
                if matches!(ev, ClientEvent::Disconnected { .. }) {
                    self.disconnected = true;
                }
                Some(ev)
            }
            Err(broadcast::error::RecvError::Lagged(n)) => Some(ClientEvent::Lagged(n)),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}

impl std::fmt::Debug for ClientEventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientEventStream")
            .field("disconnected", &self.disconnected)
            .finish_non_exhaustive()
    }
}

// Rust guideline compliant 2026-02-21

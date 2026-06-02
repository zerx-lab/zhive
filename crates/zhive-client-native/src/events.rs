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

// ============================================================
// Typed engine-event decode helpers
// ============================================================

/// Decoded payload of an `events/usage` wire notification.
///
/// Obtain by calling [`decode_usage`] on a raw [`Notification`].
///
/// # Examples
///
/// ```no_run
/// # async fn example(client: zhive_client_native::Client) {
/// use zhive_client_native::{ClientEvent, events::decode_usage};
/// let mut stream = client.subscribe_events();
/// while let Some(ClientEvent::Notification(n)) = stream.next_event().await {
///     if let Some(u) = decode_usage(&n) {
///         println!("input={} output={}", u.input_tokens, u.output_tokens);
///     }
/// }
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEvent {
    /// Owning thread id string.
    pub thread_id: String,
    /// Active turn id string.
    pub turn_id: String,
    /// Total input tokens consumed by this provider call.
    pub input_tokens: u64,
    /// Total output tokens produced by this provider call.
    pub output_tokens: u64,
}

/// Decodes an `events/usage` [`Notification`] into a [`UsageEvent`].
///
/// Returns `None` when `notif.method` is not `"events/usage"` or when
/// the params cannot be deserialized as [`UsageEvent`].
///
/// # Examples
///
/// ```
/// use zhive_client_native::events::{UsageEvent, decode_usage};
/// use zhive_proto::Notification;
///
/// let notif = Notification::new(
///     "events/usage",
///     Some(serde_json::json!({
///         "threadId": "thread:native/t1",
///         "turnId":   "turn:thread:native/t1/0",
///         "inputTokens":  120,
///         "outputTokens":  45,
///     })),
/// );
/// let ev = decode_usage(&notif).expect("must decode");
/// assert_eq!(ev.input_tokens, 120);
/// assert_eq!(ev.output_tokens, 45);
/// ```
#[must_use]
pub fn decode_usage(notif: &Notification) -> Option<UsageEvent> {
    if notif.method != "events/usage" {
        return None;
    }
    let params = notif.params.as_ref()?;
    // Manual field extraction avoids adding a `serde` dependency to this crate.
    let thread_id = params["threadId"].as_str()?.to_owned();
    let turn_id = params["turnId"].as_str()?.to_owned();
    let input_tokens = params["inputTokens"].as_u64()?;
    let output_tokens = params["outputTokens"].as_u64()?;
    Some(UsageEvent {
        thread_id,
        turn_id,
        input_tokens,
        output_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_usage_returns_correct_fields() {
        let notif = Notification::new(
            "events/usage",
            Some(serde_json::json!({
                "threadId":    "thread:native/t1",
                "turnId":      "turn:thread:native/t1/0",
                "inputTokens":  120u64,
                "outputTokens":  45u64,
            })),
        );
        let ev = decode_usage(&notif).expect("must decode events/usage");
        assert_eq!(ev.thread_id, "thread:native/t1");
        assert_eq!(ev.turn_id, "turn:thread:native/t1/0");
        assert_eq!(ev.input_tokens, 120);
        assert_eq!(ev.output_tokens, 45);
    }

    #[test]
    fn decode_usage_returns_none_for_other_methods() {
        let notif = Notification::new(
            "events/turn_started",
            Some(serde_json::json!({
                "threadId": "t",
                "turnId": "turn:t/0",
            })),
        );
        assert!(
            decode_usage(&notif).is_none(),
            "non-usage notification must return None"
        );
    }

    #[test]
    fn decode_usage_returns_none_for_missing_fields() {
        let notif = Notification::new("events/usage", Some(serde_json::json!({ "threadId": "t" })));
        assert!(
            decode_usage(&notif).is_none(),
            "incomplete params must return None"
        );
    }
}

// Rust guideline compliant 2026-02-21

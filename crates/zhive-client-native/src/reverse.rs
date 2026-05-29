//! Reverse-RPC handler trait and per-request dispatch tracking.
//!
//! The server may send JSON-RPC *requests* (not just notifications)
//! to the client for methods such as `fs/read_text_file` and
//! `terminal/create` that are reserved for future bridge-crate use.
//!
//! Register an implementation of [`ReverseHandler`] via
//! [`crate::Client::set_reverse_handler`].  The reader task consults
//! the slot for every inbound server request:
//!
//! - method ∈ `handler.methods()` → `tokio::spawn(handler.handle(…))`;
//!   the spawned task writes the `Result<Value, ErrorObject>` back as
//!   a JSON-RPC response via the outbound channel.
//! - method ∉ `handler.methods()`, or no handler registered →
//!   synthetic `-32601 MethodNotFound` response sent immediately.
//!
//! Pending reverse-RPC join handles are stored in a
//! [`PendingReverse`] map so they can be aborted on connection
//! teardown.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use tokio::task::JoinHandle;
use zhive_proto::{ErrorObject, Id};

/// Handles server-initiated (reverse-RPC) requests.
///
/// Implement this trait and register it with
/// [`crate::Client::set_reverse_handler`].  For every inbound server
/// request whose method appears in [`Self::methods`] the reader task
/// calls [`Self::handle`] in a freshly spawned Tokio task.
///
/// # Notes
///
/// * `methods` must return a **stable** slice — the reader task reads it
///   once on each incoming request.
/// * Methods **not** listed in `methods()` receive an automatic
///   `-32601 MethodNotFound` response; `handle` is never called for
///   them.
/// * Dropping the future returned by `handle` (e.g. via
///   `JoinHandle::abort` on teardown) is the cooperative cancellation
///   path.  The server sends explicit Cancelled outcomes for its own
///   pending-approval tracking; this client does not need to send any
///   wire message in response to an abort.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use async_trait::async_trait;
/// use serde_json::Value;
/// use zhive_proto::ErrorObject;
/// use zhive_client_native::ReverseHandler;
///
/// struct EchoHandler;
///
/// #[async_trait]
/// impl ReverseHandler for EchoHandler {
///     fn methods(&self) -> &[&'static str] {
///         &["fs/read_text_file"]
///     }
///
///     async fn handle(
///         &self,
///         _method: &str,
///         params: Option<Value>,
///     ) -> Result<Value, ErrorObject> {
///         // Echo params back as the result
///         Ok(params.unwrap_or(Value::Null))
///     }
/// }
/// ```
#[async_trait]
pub trait ReverseHandler: Send + Sync {
    /// Stable list of method names this handler can process.
    ///
    /// The reader task calls this on every inbound server request to
    /// decide whether to invoke [`Self::handle`] or immediately reply
    /// with `-32601`.
    fn methods(&self) -> &[&'static str];

    /// Process one server-initiated request.
    ///
    /// `method` is guaranteed to be present in [`Self::methods`].
    /// `params` is the raw `params` field from the JSON-RPC envelope
    /// (already unwrapped from the `Message` — may be `None` if the
    /// server omitted the field).
    ///
    /// * `Ok(value)` → the reader task sends
    ///   `{"result": <value>}` back to the server.
    /// * `Err(e)` → the reader task sends `{"error": <e>}` back.
    ///
    /// # Errors
    ///
    /// Return [`ErrorObject`] to have the reader task send a structured
    /// JSON-RPC error response to the server.
    async fn handle(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, ErrorObject>;
}

/// Shareable slot that holds an optional [`ReverseHandler`].
///
/// Written by [`crate::Client::set_reverse_handler`]; read by the
/// reader task on every inbound server request.
#[derive(Default)]
pub(crate) struct HandlerSlot {
    inner: RwLock<Option<Arc<dyn ReverseHandler>>>,
}

impl std::fmt::Debug for HandlerSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let has_handler = match self.inner.read() {
            Ok(g) => g.is_some(),
            Err(p) => p.into_inner().is_some(),
        };
        f.debug_struct("HandlerSlot")
            .field("registered", &has_handler)
            .finish()
    }
}

impl HandlerSlot {
    /// Replaces the current handler (or clears it when `None`).
    pub(crate) fn set(&self, handler: Option<Arc<dyn ReverseHandler>>) {
        match self.inner.write() {
            Ok(mut g) => *g = handler,
            Err(p) => *p.into_inner() = handler,
        }
    }

    /// Returns a clone of the current handler, if any.
    pub(crate) fn get(&self) -> Option<Arc<dyn ReverseHandler>> {
        match self.inner.read() {
            Ok(g) => g.clone(),
            Err(p) => p.into_inner().clone(),
        }
    }
}

/// Tracks in-flight reverse-RPC spawned tasks keyed by request id.
///
/// On connection teardown every handle in this map is aborted so
/// spawned handler futures do not outlive the connection.
#[derive(Debug, Default)]
pub(crate) struct PendingReverse {
    map: Mutex<HashMap<Id, JoinHandle<()>>>,
}

impl PendingReverse {
    /// Inserts a join handle for a spawned reverse-handler task.
    pub(crate) fn insert(&self, id: Id, handle: JoinHandle<()>) {
        let mut g = match self.map.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.insert(id, handle);
    }

    /// Removes and returns the handle for `id`, if any.
    pub(crate) fn remove(&self, id: &Id) -> Option<JoinHandle<()>> {
        let mut g = match self.map.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.remove(id)
    }

    /// Aborts every tracked handle and clears the map.
    ///
    /// Called on connection teardown so that spawned handler tasks
    /// do not run after the transport is gone.
    pub(crate) fn abort_all(&self) {
        let drained: Vec<_> = {
            let mut g = match self.map.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            g.drain().map(|(_, h)| h).collect()
        };
        for h in drained {
            h.abort();
        }
    }
}

// Rust guideline compliant 2026-02-21

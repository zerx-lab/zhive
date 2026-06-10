//! Typed permission handler trait and reverse-RPC adapter.
//!
//! The server initiates a `session/request_permission` reverse-RPC call when a
//! hook returns [`PermissionDecision::Ask`] and a live user decision is
//! required. This module provides a high-level [`PermissionHandler`] trait and a
//! [`PermissionReverseAdapter`] that plugs it into the low-level
//! [`ReverseHandler`] slot on a [`Client`].
//!
//! # Phase note
//!
//! As of Phase 1 the engine broadcasts an `events/permission_requested`
//! notification instead of using this reverse-RPC path directly. The constant
//! [`zhive_proto::permission::METHOD_REQUEST_PERMISSION`] (`"session/request_permission"`)
//! is already reserved; this adapter registers that method so client code is
//! ready the moment the server starts using the reverse-RPC path.
//!
//! # Examples
//!
//! ```no_run
//! use std::sync::Arc;
//! use async_trait::async_trait;
//! use zhive_proto::permission::{PermissionOutcome, RequestPermissionRequest};
//! use zhive_client_native::{Client, PermissionHandler};
//!
//! struct AlwaysAllow;
//!
//! #[async_trait]
//! impl PermissionHandler for AlwaysAllow {
//!     async fn on_permission(&self, req: RequestPermissionRequest) -> PermissionOutcome {
//!         // Pick the first option with `AllowOnce` or fall back to the first option.
//!         let id = req.options.first().map(|o| o.id.clone()).unwrap_or_default();
//!         PermissionOutcome::Selected { option_id: id }
//!     }
//! }
//!
//! # async fn run() {
//! let client = Client::connect("/tmp/zhive.sock").await.unwrap();
//! client.set_permission_handler(Arc::new(AlwaysAllow));
//! # }
//! ```

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use zhive_proto::ErrorObject;
use zhive_proto::permission::{
    METHOD_REQUEST_PERMISSION, PermissionOutcome, RequestPermissionRequest,
};

use crate::ReverseHandler;

// ── Public trait ──────────────────────────────────────────────────────────────

/// Handles server-initiated typed permission prompts.
///
/// Implement this trait and install it with
/// [`Client::set_permission_handler`] to receive strongly-typed
/// [`RequestPermissionRequest`] values instead of raw JSON.
///
/// # Cancellation
///
/// When the session is cancelled while a permission request is in flight the
/// server injects [`PermissionOutcome::Cancelled`] automatically; the client
/// handler is never called for that race.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use async_trait::async_trait;
/// use zhive_proto::permission::{PermissionOutcome, RequestPermissionRequest};
/// use zhive_client_native::PermissionHandler;
///
/// struct DenyAll;
///
/// #[async_trait]
/// impl PermissionHandler for DenyAll {
///     async fn on_permission(&self, _req: RequestPermissionRequest) -> PermissionOutcome {
///         PermissionOutcome::Cancelled
///     }
/// }
/// ```
#[async_trait]
pub trait PermissionHandler: Send + Sync {
    /// Returns the user's decision for one permission request.
    ///
    /// `Cancelled` is reserved for session-cancel races and is normally
    /// injected by the server, not returned here.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use async_trait::async_trait;
    /// use zhive_proto::permission::{PermissionOutcome, RequestPermissionRequest};
    /// use zhive_client_native::PermissionHandler;
    ///
    /// struct AutoAllow;
    ///
    /// #[async_trait]
    /// impl PermissionHandler for AutoAllow {
    ///     async fn on_permission(&self, req: RequestPermissionRequest) -> PermissionOutcome {
    ///         let id = req.options.first().map(|o| o.id.clone()).unwrap_or_default();
    ///         PermissionOutcome::Selected { option_id: id }
    ///     }
    /// }
    /// ```
    async fn on_permission(&self, req: RequestPermissionRequest) -> PermissionOutcome;
}

// ── Adapter ───────────────────────────────────────────────────────────────────

/// Adapts a [`PermissionHandler`] onto the low-level [`ReverseHandler`] slot.
///
/// Register via [`Client::set_permission_handler`]; the adapter decodes the
/// raw `params` `Value` into [`RequestPermissionRequest`], calls
/// [`PermissionHandler::on_permission`], and encodes the [`PermissionOutcome`]
/// back into a JSON `Value` for the response.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
/// use async_trait::async_trait;
/// use zhive_proto::permission::{PermissionOutcome, RequestPermissionRequest};
/// use zhive_client_native::{Client, PermissionHandler, PermissionReverseAdapter};
///
/// struct AutoAllow;
///
/// #[async_trait]
/// impl PermissionHandler for AutoAllow {
///     async fn on_permission(&self, req: RequestPermissionRequest) -> PermissionOutcome {
///         let id = req.options.first().map(|o| o.id.clone()).unwrap_or_default();
///         PermissionOutcome::Selected { option_id: id }
///     }
/// }
///
/// # async fn run() {
/// let client = Client::connect("/tmp/zhive.sock").await.unwrap();
/// let adapter = Arc::new(PermissionReverseAdapter::new(Arc::new(AutoAllow)));
/// client.set_reverse_handler(Some(adapter));
/// # }
/// ```
pub struct PermissionReverseAdapter<H: PermissionHandler> {
    inner: Arc<H>,
}

impl<H: PermissionHandler> PermissionReverseAdapter<H> {
    /// Wraps `handler` in a reverse-RPC adapter.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use async_trait::async_trait;
    /// use zhive_proto::permission::{PermissionOutcome, RequestPermissionRequest};
    /// use zhive_client_native::{PermissionHandler, PermissionReverseAdapter};
    ///
    /// struct NoOp;
    ///
    /// #[async_trait]
    /// impl PermissionHandler for NoOp {
    ///     async fn on_permission(&self, _req: RequestPermissionRequest) -> PermissionOutcome {
    ///         PermissionOutcome::Cancelled
    ///     }
    /// }
    ///
    /// let adapter = PermissionReverseAdapter::new(Arc::new(NoOp));
    /// ```
    #[must_use]
    pub fn new(handler: Arc<H>) -> Self {
        Self { inner: handler }
    }
}

// Manual Debug to avoid requiring H: Debug.
impl<H: PermissionHandler> std::fmt::Debug for PermissionReverseAdapter<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionReverseAdapter")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl<H: PermissionHandler + 'static> ReverseHandler for PermissionReverseAdapter<H> {
    fn methods(&self) -> &[&'static str] {
        // METHOD_REQUEST_PERMISSION = "session/request_permission"
        &[METHOD_REQUEST_PERMISSION]
    }

    async fn handle(&self, _method: &str, params: Option<Value>) -> Result<Value, ErrorObject> {
        let req: RequestPermissionRequest = serde_json::from_value(params.unwrap_or(Value::Null))
            .map_err(|e| ErrorObject {
            // -32602 = Invalid params (JSON-RPC spec)
            code: -32602,
            message: "invalid session/request_permission params".to_owned(),
            data: Some(Value::String(e.to_string())),
        })?;
        let outcome = self.inner.on_permission(req).await;
        serde_json::to_value(outcome).map_err(|e| ErrorObject {
            // -32603 = Internal error (JSON-RPC spec)
            code: -32603,
            message: "failed to encode PermissionOutcome".to_owned(),
            data: Some(Value::String(e.to_string())),
        })
    }
}

// Rust guideline compliant 2026-02-21

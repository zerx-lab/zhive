//! Method-name router for inbound JSON-RPC requests.
//!
//! A [`Router`] owns a map from `&'static str` method names to
//! [`Handler`] trait objects. The serve loop ([`super::serve_loop`])
//! looks up the handler for each [`zhive_proto::Request`] or
//! [`zhive_proto::Notification`] and dispatches it.
//!
//! Unknown methods receive [`JsonRpcCode::MethodNotFound`] errors;
//! notifications for unknown methods are silently dropped (per
//! JSON-RPC 2.0 § 4.1).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use zhive_proto::ErrorObject;

/// JSON-RPC 2.0 reserved error codes used by the router and the
/// per-connection handshake gate.
///
/// Spec-mandated numeric values (parse/request/method/params/internal)
/// must not change. Application-level codes in the `-32000..=-32099`
/// server-error band are documented in the zhive wire spec (D-007).
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum JsonRpcCode {
    /// Invalid JSON (-32700).
    ParseError,
    /// Well-formed JSON but invalid Request (-32600).
    InvalidRequest,
    /// Method does not exist (-32601).
    MethodNotFound,
    /// Invalid method parameters (-32602).
    InvalidParams,
    /// Internal JSON-RPC error (-32603).
    InternalError,
    /// Handshake: the requested protocol version is outside the supported
    /// range `[V0, LATEST]` (-32001). The error `data` field carries a
    /// `{ "supported": [0,1], "requested": <n> }` object.
    ProtocolVersionUnsupported,
    /// Handshake: a non-`initialize` request arrived before the
    /// handshake completed (-32002).
    ServerNotInitialized,
    /// Handshake: a second `initialize` request arrived on a connection
    /// that is already initialized (-32003).
    AlreadyInitialized,
}

impl JsonRpcCode {
    /// Numeric value as required by the JSON-RPC 2.0 spec or zhive wire
    /// spec (D-007).
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::server::router::JsonRpcCode;
    /// assert_eq!(JsonRpcCode::MethodNotFound.as_i64(), -32601);
    /// assert_eq!(JsonRpcCode::ProtocolVersionUnsupported.as_i64(), -32001);
    /// assert_eq!(JsonRpcCode::ServerNotInitialized.as_i64(), -32002);
    /// ```
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::InternalError => -32603,
            // Server-error band: zhive handshake codes (D-007).
            Self::ProtocolVersionUnsupported => -32001,
            Self::ServerNotInitialized => -32002,
            Self::AlreadyInitialized => -32003,
        }
    }

    /// Default message string for the code.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_core::server::router::JsonRpcCode;
    /// assert_eq!(JsonRpcCode::ServerNotInitialized.message(), "Server not initialized");
    /// ```
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::ParseError => "Parse error",
            Self::InvalidRequest => "Invalid Request",
            Self::MethodNotFound => "Method not found",
            Self::InvalidParams => "Invalid params",
            Self::InternalError => "Internal error",
            Self::ProtocolVersionUnsupported => "Unsupported protocol version",
            Self::ServerNotInitialized => "Server not initialized",
            Self::AlreadyInitialized => "Already initialized",
        }
    }
}

/// Builds an [`ErrorObject`] with no `data` payload.
#[must_use]
pub fn error_object(code: JsonRpcCode) -> ErrorObject {
    ErrorObject {
        code: code.as_i64(),
        message: code.message().to_string(),
        data: None,
    }
}

/// One method handler.
#[async_trait]
pub trait Handler: Send + Sync {
    /// Handles a request payload and returns either a result value or
    /// a typed error.
    ///
    /// `params` is `None` when the caller omitted the field.
    ///
    /// # Errors
    ///
    /// Implementations return [`ErrorObject`] for any non-internal
    /// failure (invalid params, business-logic errors, …). Panics
    /// inside handlers are caught by the serve loop and converted to
    /// [`JsonRpcCode::InternalError`].
    async fn handle(&self, params: Option<Value>) -> Result<Value, ErrorObject>;
}

/// Static dispatch table.
///
/// Cheap to clone (the inner map is `Arc`-shared).
#[derive(Default, Clone)]
pub struct Router {
    handlers: Arc<HashMap<&'static str, Arc<dyn Handler>>>,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("method_count", &self.handlers.len())
            .finish()
    }
}

impl Router {
    /// Builds an empty router.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `handler` under `method`. Existing handlers are
    /// overwritten.
    pub fn register(&mut self, method: &'static str, handler: Arc<dyn Handler>) {
        let map = Arc::make_mut(&mut self.handlers);
        map.insert(method, handler);
    }

    /// Dispatches `params` to the registered handler.
    ///
    /// # Errors
    ///
    /// Returns [`JsonRpcCode::MethodNotFound`] when `method` is
    /// unregistered; otherwise propagates the handler's error.
    pub async fn dispatch(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, ErrorObject> {
        let Some(handler) = self.handlers.get(method).cloned() else {
            return Err(error_object(JsonRpcCode::MethodNotFound));
        };
        handler.handle(params).await
    }

    /// Returns the number of registered methods.
    #[must_use]
    pub fn method_count(&self) -> usize {
        self.handlers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait]
    impl Handler for Echo {
        async fn handle(&self, params: Option<Value>) -> Result<Value, ErrorObject> {
            Ok(params.unwrap_or(Value::Null))
        }
    }

    #[tokio::test]
    async fn dispatch_routes_registered_handler() {
        let mut r = Router::new();
        r.register("echo", Arc::new(Echo));
        let v = r
            .dispatch("echo", Some(serde_json::json!(42)))
            .await
            .unwrap();
        assert_eq!(v, serde_json::json!(42));
    }

    #[tokio::test]
    async fn dispatch_unknown_method_returns_method_not_found() {
        let r = Router::new();
        let err = r.dispatch("missing", None).await.unwrap_err();
        assert_eq!(err.code, JsonRpcCode::MethodNotFound.as_i64());
    }

    #[test]
    fn handshake_error_codes_match_spec() {
        assert_eq!(JsonRpcCode::ProtocolVersionUnsupported.as_i64(), -32001);
        assert_eq!(JsonRpcCode::ServerNotInitialized.as_i64(), -32002);
        assert_eq!(JsonRpcCode::AlreadyInitialized.as_i64(), -32003);
    }
}

// Rust guideline compliant 2026-02-21

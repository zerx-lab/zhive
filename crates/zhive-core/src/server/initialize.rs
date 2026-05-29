//! Per-connection initialize/initialized handshake gate (D-007).
//!
//! This module implements the intrinsic handshake that every zhive
//! connection must complete before other requests are forwarded to the
//! router. The gate is embedded directly in the shared
//! [`super::serve_loop_with_outbound`] loop so it is enforced
//! uniformly regardless of which transport or caller sets up the
//! connection.
//!
//! ## State machine
//!
//! Each connection starts in the *uninitialized* state (`initialized ==
//! false`). The gate intercepts every inbound [`Message`] and applies
//! this dispatch table:
//!
//! | Message | Uninitialized | Initialized |
//! |---|---|---|
//! | `initialize` request | Negotiate version → respond + set flag | Respond `-32003 AlreadyInitialized` |
//! | `initialized` notification | Accept (no-op) | Accept (no-op / trace) |
//! | any other request | Respond `-32002 ServerNotInitialized` | Forward to router |
//! | any other notification | Discard (log at debug) | Forward to router |
//!
//! ## Version negotiation
//!
//! The supported range is `[V0, LATEST]` (currently `[0, 1]`). Any
//! request that asks for a version with `.0 > LATEST.0` is rejected
//! with `-32001 ProtocolVersionUnsupported`. Otherwise the chosen
//! version is `min(requested, LATEST)`.

use serde_json::Value;
use zhive_proto::ErrorObject;
use zhive_proto::initialize::{
    Capabilities, Implementation, InitializeRequest, InitializeResponse, ProtocolVersion,
};

use super::router::JsonRpcCode;

// ──────────────────────────────────────────────────────────────────
// Server identity helpers
// ──────────────────────────────────────────────────────────────────

/// Returns the canonical server identity card for zhive.
///
/// Used in every [`InitializeResponse`] so the client can display or
/// log which engine version it connected to.
///
/// # Examples
///
/// ```
/// use zhive_core::server::initialize::server_identity;
/// let id = server_identity();
/// assert_eq!(id.name, "zhive");
/// assert!(!id.version.is_empty());
/// ```
#[must_use]
pub fn server_identity() -> Implementation {
    // Use JSON construction to sidestep `#[non_exhaustive]` on
    // `Implementation`, which prevents struct-literal creation from
    // outside `zhive-proto`. The types are fully defined and this
    // call cannot fail at runtime.
    // Use JSON construction to sidestep `#[non_exhaustive]` on
    // `Implementation`. All fields are hard-coded constants; this
    // cannot fail at runtime.
    let json = serde_json::json!({
        "name": "zhive",
        "title": "Zhive Agent Runtime",
        "version": env!("CARGO_PKG_VERSION"),
    });
    // The `Err` arm is unreachable in practice (all fields are valid
    // string literals) but CLAUDE.md forbids `.expect()` in library
    // code, so we fall back to the sentinel version below.
    serde_json::from_value(json).unwrap_or_else(|_| {
        serde_json::from_value(serde_json::json!({
            "name": "zhive",
            "version": "unknown",
        }))
        // Second fallback is also unreachable; the literal is simpler.
        .unwrap_or_else(|_| {
            serde_json::from_str(r#"{"name":"zhive","version":"unknown"}"#)
                .unwrap_or_else(|_| unreachable!("bare minimal JSON must parse"))
        })
    })
}

/// Returns the capabilities this server implementation claims.
///
/// All flags reflect what Phase 1 actually implements; nothing is
/// advertised speculatively.
///
/// # Examples
///
/// ```
/// use zhive_core::server::initialize::server_capabilities;
/// let caps = server_capabilities();
/// assert!(caps.hooks);
/// assert!(caps.subagents);
/// assert!(caps.cancellation);
/// ```
#[must_use]
pub fn server_capabilities() -> Capabilities {
    // `Capabilities` is `#[non_exhaustive]` and is defined in
    // `zhive-proto`, so struct literals from outside the crate are
    // rejected. Deserialise from a JSON literal so every field is
    // set correctly without requiring an additional constructor in
    // `zhive-proto`.
    serde_json::from_value(serde_json::json!({
        "hooks": true,
        "subagents": true,
        "streaming": {
            "steer": true,
            "followUp": true,
            "nextTurn": true,
        },
        "cancellation": true,
        "permission": true,
        // Phase 1: extension discovery not implemented.
        "extension": false,
        "experimentalApi": false,
    }))
    .unwrap_or_default()
}

// ──────────────────────────────────────────────────────────────────
// Handshake result returned to serve_loop_with_outbound
// ──────────────────────────────────────────────────────────────────

/// Outcome of [`handle_initialize`].
#[derive(Debug)]
pub enum InitResult {
    /// Handshake completed: send this response and set `initialized = true`.
    Accept(InitializeResponse),
    /// Handshake rejected: send this error and keep `initialized = false`.
    Reject(ErrorObject),
    /// Duplicate request on an already-initialized connection.
    AlreadyInitialized(ErrorObject),
}

/// Builds the server-side handshake outcome for an `initialize` request.
///
/// The caller is responsible for actually sending the response frame;
/// this function is pure (no I/O) so it is easy to unit-test.
///
/// Negotiation rule:
/// - If `request.protocol_version.0 > ProtocolVersion::LATEST.0`, the
///   request is outside the supported range → [`InitResult::Reject`]
///   with `-32001` and a `data` object `{ "supported": [0,1],
///   "requested": <n> }`.
/// - Otherwise the chosen version is
///   `min(request.protocol_version, ProtocolVersion::LATEST)`.
///
/// # Examples
///
/// ```
/// use zhive_core::server::initialize::{handle_initialize, InitResult};
/// use zhive_proto::initialize::InitializeRequest;
///
/// let req: InitializeRequest = serde_json::from_value(serde_json::json!({
///     "protocolVersion": 1,
///     "clientInfo": { "name": "test", "version": "0.0.0" },
/// })).unwrap();
/// let result = handle_initialize(&req, false);
/// assert!(matches!(result, InitResult::Accept(_)));
/// ```
#[must_use]
pub fn handle_initialize(req: &InitializeRequest, already_initialized: bool) -> InitResult {
    if already_initialized {
        return InitResult::AlreadyInitialized(ErrorObject {
            code: JsonRpcCode::AlreadyInitialized.as_i64(),
            message: JsonRpcCode::AlreadyInitialized.message().to_string(),
            data: None,
        });
    }

    // Version is outside the supported range [V0, LATEST].
    if req.protocol_version.0 > ProtocolVersion::LATEST.0 {
        let requested = i64::from(req.protocol_version.0);
        return InitResult::Reject(ErrorObject {
            code: JsonRpcCode::ProtocolVersionUnsupported.as_i64(),
            message: JsonRpcCode::ProtocolVersionUnsupported
                .message()
                .to_string(),
            data: Some(serde_json::json!({
                "supported": [
                    i64::from(ProtocolVersion::V0.0),
                    i64::from(ProtocolVersion::LATEST.0),
                ],
                "requested": requested,
            })),
        });
    }

    // Negotiate: choose the lower of requested vs. LATEST.
    let chosen = req.protocol_version.min(ProtocolVersion::LATEST);

    // `InitializeResponse` is `#[non_exhaustive]`; use JSON
    // round-trip to construct it from outside `zhive-proto`.
    let empty_obj = serde_json::Value::Object(serde_json::Map::default());
    let resp_value = serde_json::json!({
        "protocolVersion": chosen.0,
        "serverCapabilities": serde_json::to_value(server_capabilities())
            .unwrap_or_else(|_| empty_obj.clone()),
        "serverInfo": serde_json::to_value(server_identity())
            .unwrap_or_else(|_| empty_obj.clone()),
    });
    match serde_json::from_value::<InitializeResponse>(resp_value) {
        Ok(resp) => InitResult::Accept(resp),
        Err(_) => {
            // Construction failure is a programming error; return an
            // internal error rather than panicking.
            InitResult::Reject(ErrorObject {
                code: JsonRpcCode::InternalError.as_i64(),
                message: JsonRpcCode::InternalError.message().to_string(),
                data: None,
            })
        }
    }
}

/// Builds the JSON-RPC error object for a request that arrived before
/// `initialize` completed.
///
/// # Examples
///
/// ```
/// use zhive_core::server::initialize::not_initialized_error;
/// let e = not_initialized_error();
/// assert_eq!(e.code, -32002);
/// ```
#[must_use]
pub fn not_initialized_error() -> ErrorObject {
    ErrorObject {
        code: JsonRpcCode::ServerNotInitialized.as_i64(),
        message: JsonRpcCode::ServerNotInitialized.message().to_string(),
        data: None,
    }
}

/// Converts an [`InitializeResponse`] into a [`Value`] payload for the
/// JSON-RPC result field.
///
/// # Errors
///
/// Returns a sentinel `Value::Null` if serialization fails (should
/// never happen for this fully-typed struct, but CLAUDE.md forbids
/// `.expect()` outside tests).
#[must_use]
pub fn response_to_value(resp: &InitializeResponse) -> Value {
    serde_json::to_value(resp).unwrap_or(Value::Null)
}

// ──────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a test [`InitializeRequest`] via JSON deserialization to
    /// avoid `#[non_exhaustive]` struct-literal restrictions.
    fn test_req(version: u16) -> InitializeRequest {
        serde_json::from_value(serde_json::json!({
            "protocolVersion": version,
            "clientInfo": {
                "name": "test-client",
                "version": "0.0.0",
            },
        }))
        .expect("test_req must deserialize")
    }

    #[test]
    fn accepts_v1() {
        let req = test_req(1);
        let result = handle_initialize(&req, false);
        match result {
            InitResult::Accept(resp) => {
                assert_eq!(resp.protocol_version, ProtocolVersion::V1);
                assert!(resp.server_capabilities.hooks);
                assert_eq!(resp.server_info.name, "zhive");
            }
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[test]
    fn accepts_v0_and_negotiates_down() {
        // Client asks for V0; server should accept (V0 is the floor).
        let req = test_req(0);
        let result = handle_initialize(&req, false);
        match result {
            InitResult::Accept(resp) => {
                // min(0, 1) = 0
                assert_eq!(resp.protocol_version, ProtocolVersion::V0);
            }
            other => panic!("expected Accept for V0, got {other:?}"),
        }
    }

    #[test]
    fn rejects_future_version() {
        // Version 99 is beyond LATEST; must get -32001.
        let req = test_req(99);
        let result = handle_initialize(&req, false);
        match result {
            InitResult::Reject(e) => {
                assert_eq!(e.code, -32001);
                let data = e.data.unwrap();
                assert_eq!(data["requested"], 99);
                assert!(data["supported"].is_array());
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn already_initialized_returns_error() {
        let req = test_req(1);
        let result = handle_initialize(&req, true);
        match result {
            InitResult::AlreadyInitialized(e) => {
                assert_eq!(e.code, -32003);
            }
            other => panic!("expected AlreadyInitialized, got {other:?}"),
        }
    }

    #[test]
    fn not_initialized_error_has_correct_code() {
        let e = not_initialized_error();
        assert_eq!(e.code, -32002);
    }

    #[test]
    fn server_identity_has_required_fields() {
        let id = server_identity();
        assert_eq!(id.name, "zhive");
        assert!(!id.version.is_empty());
    }

    #[test]
    fn server_capabilities_reflect_phase1_impl() {
        let caps = server_capabilities();
        assert!(caps.hooks);
        assert!(caps.subagents);
        assert!(caps.cancellation);
        assert!(caps.permission);
        assert!(caps.streaming.steer);
        assert!(caps.streaming.follow_up);
        assert!(caps.streaming.next_turn);
        assert!(!caps.extension, "extension not implemented in Phase 1");
    }
}

// Rust guideline compliant 2026-02-21

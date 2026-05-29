//! Error types for [`crate::Client`].
//!
//! All failure modes surfaced by client operations are variants of
//! [`ClientError`].

use thiserror::Error;
use zhive_proto::ErrorObject;
use zhive_proto::framing::FramingError;

/// Failure modes surfaced by [`crate::Client`].
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
///
/// let d = ClientError::Disconnected("peer closed".into());
/// assert!(matches!(d, ClientError::Disconnected(_)));
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

    /// The connection was closed before the expected response arrived.
    ///
    /// This is returned from [`crate::Client::call`] when the transport goes
    /// away (peer closed the stream, framing error, or ordered
    /// teardown) before the server sent the response.
    #[error("disconnected: {0}")]
    Disconnected(String),

    /// Server responded with a structured JSON-RPC error object.
    #[error("server error: {0:?}")]
    Server(ErrorObject),

    /// The server rejected the `initialize` request because it does
    /// not support the requested protocol version.
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
    /// version mismatch.
    #[error("initialize handshake failed: {reason}")]
    InitializeFailed {
        /// Human-readable reason.
        reason: String,
    },
}

// Rust guideline compliant 2026-02-21

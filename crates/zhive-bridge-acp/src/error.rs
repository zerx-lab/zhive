//! Error surface for the ACP bridge.
//!
//! [`AcpError`] is the public failure type returned by [`crate::serve`] and
//! [`crate::serve_on`]. It wraps the two failure domains the bridge straddles:
//! the ACP transport layer ([`agent_client_protocol::Error`]) and the embedded
//! engine ([`zhive_core::engine::EngineError`]).
//!
//! Engine errors that surface *inside* a prompt turn are not propagated as
//! [`AcpError`]; instead they are mapped to an ACP `StopReason` or a per-request
//! JSON-RPC error so a single misbehaving turn never tears down the connection.
//! [`AcpError`] is reserved for connection-fatal conditions (transport closed,
//! engine actor gone) returned by the top-level `serve` future.

use thiserror::Error;

/// Connection-fatal failure of the ACP bridge.
///
/// Returned by [`crate::serve`] / [`crate::serve_on`] when the underlying ACP
/// connection or the embedded engine fails in a way that ends the session.
///
/// # Examples
///
/// ```
/// use zhive_bridge_acp::AcpError;
/// let err = AcpError::Transport("connection closed".to_string());
/// assert!(err.to_string().contains("connection closed"));
/// ```
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AcpError {
    /// The ACP JSON-RPC transport reported a fatal error.
    #[error("acp transport error: {0}")]
    Transport(String),

    /// The embedded engine actor failed or stopped accepting submissions.
    #[error("engine error: {0}")]
    Engine(#[from] zhive_core::engine::EngineError),
}

impl From<agent_client_protocol::Error> for AcpError {
    fn from(value: agent_client_protocol::Error) -> Self {
        // The SDK error renders its message and data via `Display`; capturing
        // the rendered string keeps `AcpError` free of a public dependency on
        // the SDK error's internal shape (which is `#[non_exhaustive]`).
        Self::Transport(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_error_converts() {
        let engine_err = zhive_core::engine::EngineError::ActorStopped;
        let acp_err: AcpError = engine_err.into();
        assert!(matches!(acp_err, AcpError::Engine(_)));
        assert!(acp_err.to_string().contains("engine error"));
    }

    #[test]
    fn transport_error_renders() {
        let err = AcpError::Transport("boom".to_string());
        assert_eq!(err.to_string(), "acp transport error: boom");
    }
}

// Rust guideline compliant 2026-02-21

//! Error types for the MCP client.
//!
//! Connecting to and discovering an MCP server can fail at several distinct
//! stages — building the transport, completing the `initialize` handshake, or
//! enumerating the server's tools. [`McpError`] captures those stages so a
//! caller (or the parallel connect loop in [`crate::manager`]) can log a
//! precise reason before deciding to skip a misbehaving server.

use std::time::Duration;

use thiserror::Error;

/// Failure encountered while connecting to or querying an MCP server.
///
/// Each variant corresponds to a stage of the connect-and-discover flow.
/// The connect loop logs the variant's [`std::fmt::Display`] and skips the
/// affected server rather than aborting the whole boot sequence.
///
/// # Examples
///
/// ```
/// use zhive_mcp::McpError;
/// let err = McpError::Transport("spawn failed".to_owned());
/// assert!(err.to_string().contains("spawn failed"));
/// ```
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum McpError {
    /// The transport (child process or HTTP client) could not be built.
    #[error("MCP transport setup failed: {0}")]
    Transport(String),

    /// The MCP `initialize` handshake failed or the connection closed early.
    #[error("MCP initialize handshake failed: {0}")]
    Initialize(String),

    /// Listing tools, resources, or prompts after connecting failed.
    #[error("MCP discovery request failed: {0}")]
    Discovery(String),

    /// The per-server connect budget elapsed before the server was ready.
    #[error("MCP server did not become ready within {0:?}")]
    ConnectTimeout(Duration),
}

// Rust guideline compliant 2026-02-21

//! Error type surfaced by the zhive terminal UI.
//!
//! The TUI is an application-level crate consumed only by `zhive-cli`, but it
//! still exposes a typed error per CLAUDE.md (`?` + `thiserror`, no
//! `anyhow` leaking across the crate boundary). Transport and engine failures
//! flow in from [`zhive_client_native`]; terminal failures come from stdout
//! I/O during setup and teardown.

use thiserror::Error;

/// Errors returned while setting up, running, or tearing down the TUI.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TuiError {
    /// A terminal / stdout I/O operation failed (raw mode, alt screen, draw).
    #[error("terminal I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The native client could not reach the engine or returned an RPC error.
    #[error("client error: {0}")]
    Client(#[from] zhive_client_native::ClientError),
}

/// Convenient result alias for fallible TUI operations.
pub type Result<T, E = TuiError> = std::result::Result<T, E>;

// Rust guideline compliant 2026-02-21

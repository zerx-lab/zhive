//! Ratatui terminal UI for zhive.
//!
//! Per D-002 the TUI is a JSON-RPC client like any other (IDE, Web UI,
//! remote). It depends on `zhive-client-native` and `zhive-proto`, never
//! on `zhive-core`. The Phase 1 skeleton will render Thread / Turn / Item
//! streams produced by `zhive-client-native`.

#![forbid(unsafe_code)]

/// Reports this crate's package version.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// Rust guideline compliant 2026-02-21

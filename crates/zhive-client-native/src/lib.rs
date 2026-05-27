//! Native Rust JSON-RPC 2.0 client for the zhive engine.
//!
//! All in-process Rust callers (TUI, bridges, embedded SDK) reach the
//! engine through this crate. Per D-002 it never depends on `zhive-core`:
//! the only shared crate is `zhive-proto`, which carries the wire schema.
//!
//! Phase 1 surface is intentionally minimal: just the version helper.
//! Real `Client::connect_stdio` / `connect_uds` constructors land alongside
//! the first end-to-end turn.

#![forbid(unsafe_code)]

/// Reports this crate's package version.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// Rust guideline compliant 2026-02-21

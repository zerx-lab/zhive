//! Agent runtime engine for zhive.
//!
//! Owns the Thread / Turn / Item state machine (D-006), tool dispatch, the
//! LLM provider boundary, and the JSON-RPC server module that exposes them
//! over stdio + UDS (D-003 / D-004). Per D-002 this crate is engine-side
//! only: UI / SDK / bridges talk to `zhive-client-native` clients and never
//! to types defined here.

#![forbid(unsafe_code)]

pub mod engine;
pub mod persistence;
pub mod state;

/// Reports this crate's package version.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// Rust guideline compliant 2026-02-21

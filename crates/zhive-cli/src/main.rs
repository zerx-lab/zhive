//! Entry point for the `zhive` binary.
//!
//! Subcommand layout (target shape per D-010, R3+R4 终版):
//!   * `zhive`              -> spawn engine + attach TUI
//!   * `zhive serve`        -> JSON-RPC daemon over UDS + stdio (D-003, D-004)
//!   * `zhive tui`          -> attach TUI to a running engine
//!   * `zhive bridge-stdio` -> stdio <-> UDS pass-through (Phase 1 必含)
//!
//! Phase 1 placeholder: prints a banner; real dispatch lands once the
//! first turn end-to-end works.

fn main() {
    println!("zhive {} (skeleton)", env!("CARGO_PKG_VERSION"));
    println!("subcommand wiring lands with the v1 protocol implementation.");
}

// Rust guideline compliant 2026-02-21

//! xtask: workspace-local task runner.
//!
//! Dispatch shape:
//!   * `cargo xtask check-upstream` -- diff rmcp / acp pinned versions against latest.
//!   * `cargo xtask dist` -- build the `dist` profile artefacts (Phase 3).
//!
//! Phase 1 placeholder so the binary compiles and the dispatch contract
//! is visible. `gen-proto` from the `ConnectRPC` era has been removed
//! (D-003 R3+R4 终版): the schema is now plain `serde + schemars`.

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("check-upstream") => println!("xtask check-upstream: not yet implemented"),
        Some("dist") => println!("xtask dist: not yet implemented"),
        Some(other) => eprintln!("unknown xtask subcommand: {other}"),
        None => eprintln!("usage: cargo xtask <check-upstream|dist>"),
    }
}

// Rust guideline compliant 2026-02-21

//! xtask: workspace-local task runner.
//!
//! Dispatch shape:
//!   * `cargo xtask check-upstream` -- diff rmcp / acp pinned versions against latest.
//!   * `cargo xtask dist` -- build the `dist` profile artefacts (Phase 3).
//!   * `cargo xtask schema` -- emit JSON Schema files for all public proto wire types.
//!
//! `gen-proto` from the `ConnectRPC` era has been removed (D-003 R3+R4 终版):
//! the schema is now plain `serde + schemars`.  Run `cargo xtask schema` to
//! regenerate `proto/schema/*.json`.

mod schema;

fn main() {
    let mut args = std::env::args().skip(1);
    let result = match args.next().as_deref() {
        Some("check-upstream") => {
            println!("xtask check-upstream: not yet implemented");
            Ok(())
        }
        Some("dist") => {
            println!("xtask dist: not yet implemented");
            Ok(())
        }
        Some("schema") => schema::run(),
        Some(other) => {
            eprintln!("unknown xtask subcommand: {other}");
            Ok(())
        }
        None => {
            eprintln!("usage: cargo xtask <check-upstream|dist|schema>");
            Ok(())
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

// Rust guideline compliant 2026-02-21

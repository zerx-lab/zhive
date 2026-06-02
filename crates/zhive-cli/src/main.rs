//! The `zhive` binary: a dispatcher for the `tui`, `serve`, `bridge`, and
//! `config` subcommands.
//!
//! Process concerns (config files, provider credentials, spawning and serving
//! the engine) live here and in [`run`]; the TUI and engine crates stay free of
//! them. See [`cli`] for the command surface and [`config`] for the file format.

#![forbid(unsafe_code)]

mod cli;
mod config;
mod run;

// The provider builders, the in-process engine host, and the shared runtime
// boot path need `zhive-core` + `llmsdk`, which only the `engine` feature pulls
// in (via `tui` / `serve` / `mcp` / `acp` / `skills`).
#[cfg(feature = "engine")]
mod boot;
#[cfg(feature = "engine")]
mod engine_host;
#[cfg(feature = "engine")]
mod provider;

use clap::Parser;

use crate::cli::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    run::dispatch(cli).await
}

// Rust guideline compliant 2026-02-21

//! Command-line surface for the `zhive` binary (clap derive).
//!
//! Subcommands map to Phase 1 features: `tui` (the default) launches the UI,
//! `serve` runs the engine as a UDS daemon, `bridge` pipes stdio to a running
//! daemon for editor/ACP/MCP hosts, `exec` runs the engine headlessly (useful
//! in scripts and CI), `doctor` prints a diagnostic health summary, and
//! `config` manages the config file.
//! Argument types stay free of the optional engine/UI crates so the parser
//! compiles regardless of which features are enabled.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Top-level CLI: a global `--config` plus an optional subcommand.
#[derive(Parser, Debug)]
#[command(name = "zhive", version, about = "zhive — terminal AI copilot")]
pub struct Cli {
    /// Path to `config.toml` (overrides the default search path).
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Subcommand to run; defaults to `tui` when omitted.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The available subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Launch the interactive terminal UI (default).
    Tui(TuiArgs),
    /// Run a single prompt non-interactively and print the result to stdout.
    ///
    /// Starts the engine, submits the prompt, streams the response to stdout,
    /// and exits. Suitable for scripts and CI pipelines:
    ///
    /// ```text
    /// zhive exec -p "summarize the README"
    /// ```
    #[cfg(feature = "engine")]
    Exec(ExecArgs),
    /// Run the engine as a daemon serving JSON-RPC over a local socket
    /// (Unix-domain socket on unix, named pipe on windows).
    Serve(ServeArgs),
    /// Pipe stdio to a running engine socket (editor / ACP / MCP hosts).
    Bridge(BridgeArgs),
    /// Serve the engine over the ACP protocol on stdio (for ACP editor hosts).
    #[cfg(feature = "acp")]
    Acp(AcpArgs),
    /// Inspect or initialize the configuration file.
    Config(ConfigArgs),
    /// Print a diagnostic summary of the current configuration and capabilities.
    Doctor,
}

/// Arguments for `zhive tui` (all override `config.toml`).
#[derive(Args, Debug, Default)]
pub struct TuiArgs {
    /// Provider override: any named entry from `[provider.<name>]` in config.toml.
    ///
    /// E.g. `anthropic`, `openai`, `xai`, `scripted`, or any custom name.
    /// Must match a key already present in the providers map; unknown names are
    /// ignored with a warning.
    #[arg(long)]
    pub provider: Option<String>,
    /// Model id override.
    #[arg(long)]
    pub model: Option<String>,
    /// Theme override: `dark`, `light`, or `mono`.
    #[arg(long)]
    pub theme: Option<String>,
    /// Accent override: `cyan`, `amber`, `lime`, or `magenta`.
    #[arg(long)]
    pub accent: Option<String>,
}

/// Arguments for `zhive serve`.
#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Socket path to bind (defaults to the standard runtime path).
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

/// Arguments for `zhive bridge`.
#[derive(Args, Debug)]
pub struct BridgeArgs {
    /// Socket path of the running engine to bridge to.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

/// Arguments for `zhive acp`.
///
/// The ACP transport is stdio (stdout is the JSON-RPC wire), so the only input
/// is the global `--config`; this struct exists for clap symmetry and future
/// options.
#[cfg(feature = "acp")]
#[derive(Args, Debug, Default)]
pub struct AcpArgs;

/// Arguments for `zhive exec`.
///
/// All provider/model flags are identical to `tui` so the same config
/// overrides work in headless mode.
#[cfg(feature = "engine")]
#[derive(Args, Debug)]
pub struct ExecArgs {
    /// The prompt to send to the engine.
    ///
    /// The engine runs the prompt to completion and prints the agent's reply
    /// to stdout. Tool activity is printed to stdout as well, one line per
    /// tool call/result.
    #[arg(short, long)]
    pub prompt: String,
    /// Provider override (name from `[provider.<name>]` in config.toml).
    #[arg(long)]
    pub provider: Option<String>,
    /// Model id override.
    #[arg(long)]
    pub model: Option<String>,
}

/// Arguments for `zhive config`.
#[derive(Args, Debug)]
pub struct ConfigArgs {
    /// What to do with the config file.
    #[command(subcommand)]
    pub action: ConfigAction,
}

/// Config-management actions.
#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Print the resolved config path.
    Path,
    /// Write a commented sample config (never overwrites an existing file).
    Init,
}

// Rust guideline compliant 2026-02-21

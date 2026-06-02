//! Subcommand dispatch and implementations for the `zhive` binary.
//!
//! Each command is provided in two forms: a real implementation gated on its
//! Cargo feature, and a stub (under `cfg(not(...))`) that errors cleanly, so a
//! feature-reduced build still compiles and `dispatch` always type-checks. The
//! default build enables `tui + serve + bridge-stdio`.

use anyhow::Result;

use crate::cli::{Cli, Command, ConfigAction, ConfigArgs};

/// Routes the parsed CLI to the matching command, defaulting to `tui`.
///
/// # Errors
///
/// Propagates any command error (config load, provider build, I/O, engine).
pub async fn dispatch(cli: Cli) -> Result<()> {
    let command = cli
        .command
        .unwrap_or_else(|| Command::Tui(crate::cli::TuiArgs::default()));
    match command {
        Command::Tui(args) => run_tui(cli.config, args).await,
        Command::Serve(args) => run_serve(cli.config, args).await,
        Command::Bridge(args) => run_bridge(args).await,
        #[cfg(feature = "acp")]
        Command::Acp(_args) => run_acp(cli.config).await,
        Command::Config(args) => run_config(cli.config, args),
    }
}

// ============================================================
// tui
// ============================================================

#[cfg(feature = "tui")]
async fn run_tui(config_path: Option<std::path::PathBuf>, args: crate::cli::TuiArgs) -> Result<()> {
    let (mut cfg, _source) = crate::config::Config::load(config_path.as_deref())?;
    apply_tui_overrides(&mut cfg, &args);

    let provider = crate::provider::build(&cfg)?;
    let runtime = crate::boot::build_runtime(&cfg).await?;
    let socket = crate::engine_host::tui_socket_path();
    let host = crate::engine_host::Host::start(provider, runtime, socket).await?;

    let tui_config = build_tui_config(&cfg);
    let result = zhive_tui::run(host.client.clone(), tui_config).await;
    host.stop().await;
    Ok(result?)
}

#[cfg(not(feature = "tui"))]
async fn run_tui(_config: Option<std::path::PathBuf>, _args: crate::cli::TuiArgs) -> Result<()> {
    anyhow::bail!("this build was compiled without the `tui` feature")
}

/// Applies `--provider/--model/--theme/--accent` over the loaded config.
///
/// `--provider <name>` sets the active provider by name. If the name does not
/// exist in the providers map a warning is emitted and the current default is
/// kept. `--model <id>` overrides the model of the (possibly just-changed)
/// active entry.
#[cfg(feature = "tui")]
fn apply_tui_overrides(cfg: &mut crate::config::Config, args: &crate::cli::TuiArgs) {
    if let Some(provider_name) = &args.provider {
        if cfg.provider.providers.contains_key(provider_name.as_str()) {
            cfg.provider.default.clone_from(provider_name);
        } else {
            tracing::warn!(
                provider = %provider_name,
                "--provider value not found in config providers map; ignoring override"
            );
        }
    }
    if let (Some(model), Some(entry)) = (
        &args.model,
        cfg.provider.providers.get_mut(&cfg.provider.default),
    ) {
        entry.model.clone_from(model);
    }
    if let Some(theme) = &args.theme {
        cfg.ui.theme.clone_from(theme);
    }
    if let Some(accent) = &args.accent {
        cfg.ui.accent.clone_from(accent);
    }
}

/// Distills the config into the UI-facing `TuiConfig` (D-002).
#[cfg(feature = "tui")]
fn build_tui_config(cfg: &crate::config::Config) -> zhive_tui::TuiConfig {
    zhive_tui::TuiConfig {
        theme: parse_theme(&cfg.ui.theme),
        accent: parse_accent(&cfg.ui.accent),
        density: parse_density(&cfg.ui.density),
        provider_label: cfg.active_provider_label().to_owned(),
        model_label: cfg.active_model().to_owned(),
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        branch: detect_branch(),
        session_name: None,
    }
}

#[cfg(feature = "tui")]
fn parse_theme(s: &str) -> zhive_tui::Theme {
    match s {
        "light" => zhive_tui::Theme::Light,
        "mono" => zhive_tui::Theme::Mono,
        _ => zhive_tui::Theme::Dark,
    }
}

#[cfg(feature = "tui")]
fn parse_accent(s: &str) -> zhive_tui::Accent {
    match s {
        "amber" => zhive_tui::Accent::Amber,
        "lime" => zhive_tui::Accent::Lime,
        "magenta" => zhive_tui::Accent::Magenta,
        _ => zhive_tui::Accent::Cyan,
    }
}

#[cfg(feature = "tui")]
fn parse_density(s: &str) -> zhive_tui::Density {
    match s {
        "lean" => zhive_tui::Density::Lean,
        "airy" => zhive_tui::Density::Airy,
        _ => zhive_tui::Density::Default,
    }
}

/// Best-effort current git branch from `.git/HEAD`.
#[cfg(feature = "tui")]
fn detect_branch() -> Option<String> {
    let head = std::fs::read_to_string(".git/HEAD").ok()?;
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    Some(branch.to_owned())
}

// ============================================================
// serve
// ============================================================

#[cfg(feature = "serve")]
async fn run_serve(
    config_path: Option<std::path::PathBuf>,
    args: crate::cli::ServeArgs,
) -> Result<()> {
    use std::sync::Arc;

    use tokio_util::sync::CancellationToken;
    use zhive_core::engine::{Engine, EngineConfig};
    use zhive_core::hooks::HookHost;
    use zhive_core::server::{
        DEFAULT_MAX_CONNECTIONS, Router, register_engine_handlers, serve_uds_with_events,
    };

    init_stderr_logging();
    let (cfg, _source) = crate::config::Config::load(config_path.as_deref())?;
    let provider = crate::provider::build(&cfg)?;
    let runtime = crate::boot::build_runtime(&cfg).await?;
    let socket = args.socket.unwrap_or_else(default_socket);
    let _ = std::fs::remove_file(&socket);

    let engine = Engine::spawn_with_config(EngineConfig {
        provider,
        tools: Arc::clone(&runtime.registry),
        hook_host: Arc::new(HookHost::new()),
        storage: runtime.storage.clone(),
        turn_limits: runtime.turn_limits,
        system_prompt: Some(Arc::clone(&runtime.system_prompt)),
    });
    let mut router = Router::new();
    register_engine_handlers(&mut router, engine.clone());
    let router = Arc::new(router);
    let token = CancellationToken::new();

    let serve_socket = socket.clone();
    let serve_engine = engine.clone();
    let serve_token = token.clone();
    let mut handle = tokio::spawn(async move {
        serve_uds_with_events(
            &serve_socket,
            router,
            serve_engine,
            DEFAULT_MAX_CONNECTIONS,
            serve_token,
        )
        .await
    });

    eprintln!(
        "zhive engine serving on {} · provider={} model={}",
        socket.display(),
        cfg.active_provider_label(),
        cfg.active_model(),
    );

    // Watch the serve task and Ctrl-C together: if the bind (or serving) fails
    // the task ends on its own, and we must surface that rather than block on
    // Ctrl-C while claiming to serve on a socket that was never bound.
    let exited_early = tokio::select! {
        joined = &mut handle => Some(joined),
        signal = tokio::signal::ctrl_c() => {
            signal?;
            None
        }
    };
    eprintln!("zhive: shutting down");

    token.cancel();
    // Close MCP connections before the engine that dispatches through them.
    runtime.shutdown().await;
    let _ = engine.shutdown().await;
    let _ = std::fs::remove_file(&socket);

    if let Some(joined) = exited_early {
        joined
            .map_err(|e| anyhow::anyhow!("serve task panicked: {e}"))?
            .map_err(|e| anyhow::anyhow!("serve error: {e}"))
    } else {
        let _ = handle.await;
        Ok(())
    }
}

#[cfg(not(feature = "serve"))]
#[expect(
    clippy::unused_async,
    reason = "stub must match the async fn signature of the feature-gated real impl"
)]
async fn run_serve(
    _config: Option<std::path::PathBuf>,
    _args: crate::cli::ServeArgs,
) -> Result<()> {
    anyhow::bail!("this build was compiled without the `serve` feature")
}

/// Installs a `tracing` subscriber that writes to **stderr only**.
///
/// Both `serve` and `acp` rely on this: `acp` in particular speaks JSON-RPC on
/// stdout, so anything other than the protocol wire must go to stderr.
#[cfg(any(feature = "serve", feature = "acp"))]
fn init_stderr_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .try_init();
}

// ============================================================
// bridge
// ============================================================

#[cfg(feature = "bridge-stdio")]
async fn run_bridge(args: crate::cli::BridgeArgs) -> Result<()> {
    let socket = args.socket.unwrap_or_else(default_socket);
    zhive_bridge_stdio::run(&socket, tokio::io::stdin(), tokio::io::stdout()).await?;
    Ok(())
}

#[cfg(not(feature = "bridge-stdio"))]
#[expect(
    clippy::unused_async,
    reason = "stub must match the async fn signature of the feature-gated real impl"
)]
async fn run_bridge(_args: crate::cli::BridgeArgs) -> Result<()> {
    anyhow::bail!("this build was compiled without the `bridge-stdio` feature")
}

// ============================================================
// acp
// ============================================================

/// Serves the in-process engine over the ACP protocol on stdio.
///
/// stdout is the JSON-RPC wire, so this path is audited to emit nothing to
/// stdout: logging goes to stderr, and neither `Config::load` nor
/// `provider::build` print to stdout. The MCP manager (if any) is kept alive
/// for the engine's lifetime and shut down after `serve` returns.
#[cfg(feature = "acp")]
async fn run_acp(config_path: Option<std::path::PathBuf>) -> Result<()> {
    use std::sync::Arc;

    use zhive_core::engine::{Engine, EngineConfig};
    use zhive_core::hooks::HookHost;

    init_stderr_logging();
    let (cfg, _source) = crate::config::Config::load(config_path.as_deref())?;
    let provider = crate::provider::build(&cfg)?;
    let runtime = crate::boot::build_runtime(&cfg).await?;

    let engine = Engine::spawn_with_config(EngineConfig {
        provider,
        tools: Arc::clone(&runtime.registry),
        hook_host: Arc::new(HookHost::new()),
        storage: runtime.storage.clone(),
        turn_limits: runtime.turn_limits,
        system_prompt: Some(Arc::clone(&runtime.system_prompt)),
    });

    // `serve` owns the engine and drives it until the ACP client disconnects.
    let result = zhive_bridge_acp::serve(engine.clone()).await;

    // Close MCP connections before the engine that dispatches through them.
    runtime.shutdown().await;
    let _ = engine.shutdown().await;
    result.map_err(|e| anyhow::anyhow!("acp serve error: {e}"))
}

// No `run_acp` stub: `Command::Acp` only exists under the `acp` feature (so the
// dispatch arm is gated too). Builds without `acp` simply do not have the
// command, mirroring how the variant is omitted from the CLI surface.

// ============================================================
// config (always available)
// ============================================================

fn run_config(config_path: Option<std::path::PathBuf>, args: ConfigArgs) -> Result<()> {
    let ConfigArgs { action } = args;
    let path = config_path
        .or_else(crate::config::default_config_path)
        .ok_or_else(|| {
            anyhow::anyhow!("cannot determine config path (set $HOME or $XDG_CONFIG_HOME)")
        })?;
    match action {
        ConfigAction::Path => {
            println!("{}", path.display());
        }
        ConfigAction::Init => {
            if path.exists() {
                anyhow::bail!("config already exists at {}", path.display());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, crate::config::SAMPLE_CONFIG)?;
            println!("wrote sample config to {}", path.display());
        }
    }
    Ok(())
}

// ============================================================
// shared helpers
// ============================================================

/// Default engine socket: `$XDG_RUNTIME_DIR/zhive.sock` or the temp dir.
#[cfg(any(feature = "serve", feature = "bridge-stdio"))]
fn default_socket() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(
            || std::env::temp_dir().join("zhive.sock"),
            |dir| std::path::PathBuf::from(dir).join("zhive.sock"),
        )
}

// Rust guideline compliant 2026-02-21

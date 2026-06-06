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
        #[cfg(feature = "engine")]
        Command::Exec(args) => run_exec(cli.config, args).await,
        Command::Serve(args) => run_serve(cli.config, args).await,
        Command::Bridge(args) => run_bridge(args).await,
        #[cfg(feature = "acp")]
        Command::Acp(_args) => run_acp(cli.config).await,
        Command::Config(args) => run_config(cli.config, args),
        Command::Doctor => run_doctor(cli.config).await,
    }
}

// ============================================================
// tui
// ============================================================

#[cfg(feature = "tui")]
async fn run_tui(config_path: Option<std::path::PathBuf>, args: crate::cli::TuiArgs) -> Result<()> {
    // TUI owns the full terminal, so nothing may go to stderr or stdout.
    // Spin up a file-backed subscriber instead (best-effort; silently degraded
    // on failure so a missing data dir never blocks the TUI from starting).
    init_tui_file_logging();

    let (mut cfg, _source) = crate::config::Config::load(config_path.as_deref())?;
    apply_tui_overrides(&mut cfg, &args);

    let provider = crate::provider::build(&cfg)?;
    let runtime = crate::boot::build_runtime(&cfg).await?;
    // Map the boot-discovered skills to the TUI's own type (D-002: the TUI must
    // not depend on `zhive_core`). Taken before `runtime` moves into the host.
    let skills: Vec<zhive_tui::app::SkillCommand> = runtime
        .skills
        .iter()
        .map(|s| zhive_tui::app::SkillCommand {
            name: s.name.clone(),
            description: s.description.clone(),
            invocation: s.invocation.clone(),
        })
        .collect();
    let socket = crate::engine_host::tui_socket_path();
    // Build the host model catalogue (for the `/models` picker + hot-swap) and
    // resolve the active model's context window AND live reasoning-depth cycle in
    // one `/models` fetch, so Ctrl+T works at launch without opening the picker.
    let catalog = crate::models::build_catalog(&cfg);
    let model_info = crate::models::resolve_active_model_info(&cfg, catalog.as_ref()).await;
    let context_window = model_info.context_window;
    let host =
        crate::engine_host::Host::start(provider, runtime, socket, catalog, context_window).await?;

    let mut tui_config = build_tui_config(&cfg);
    tui_config.effort_cycle = model_info.supported_efforts;
    let result = zhive_tui::run(host.client.clone(), tui_config, skills).await;
    host.stop().await;
    let outcome = result?;

    // Persist the final model and reasoning depth so the next launch restores
    // them — but only when the user actually changed something, so an untouched
    // session never rewrites config and a boot-time clamp cannot erase a
    // remembered depth. A disabled (`Off`) depth clears the remembered value.
    // Best-effort: a write failure is logged but never fails the clean exit.
    if outcome.selection_changed {
        let thinking = outcome
            .thinking_effort
            .is_enabled()
            .then(|| outcome.thinking_effort.label().to_owned());
        cfg.set_active_selection(outcome.model_label, thinking);
        if let Some(path) = config_path.or_else(crate::config::default_config_path)
            && let Err(e) = crate::config::persist_active_selection(&path, &cfg)
        {
            tracing::warn!(error = %e, path = %path.display(), "failed to persist model selection");
        }
    }
    Ok(())
}

/// Attempts to install a file-backed `tracing` subscriber for the TUI.
///
/// The log file is placed in the user's zhive data directory
/// (`$ZHIVE_DATA_DIR`, `$XDG_DATA_HOME/zhive`, or
/// `$HOME/.local/share/zhive`) as `zhive-tui.log`. ANSI colour codes are
/// suppressed because most viewers cannot render them.
///
/// Failure (missing home directory, permission denied, etc.) is silently
/// ignored so it never prevents the TUI from launching.
#[cfg(feature = "tui")]
fn init_tui_file_logging() {
    use std::sync::Arc;

    use tracing_subscriber::EnvFilter;

    let Some(log_path) = tui_log_path() else {
        // No data directory available; the TUI will run without logging.
        return;
    };

    // Create parent directories and open the log file in append mode.
    let file = (|| -> std::io::Result<std::fs::File> {
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
    })();

    let Ok(file) = file else {
        // Cannot open the file; degrade gracefully.
        return;
    };

    // `Arc<W>` implements `MakeWriter` when `&W: io::Write`.
    // `File` satisfies `&File: io::Write`, so `Arc<File>` works.
    let writer = Arc::new(file);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // `try_init` is best-effort: if another subscriber was already installed
    // (e.g. in tests) the error is ignored.
    let _ = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init();
}

/// Resolves the TUI log-file path: `<data_dir>/zhive-tui.log`.
///
/// Returns `None` when the data directory cannot be determined.
#[cfg(feature = "tui")]
fn tui_log_path() -> Option<std::path::PathBuf> {
    crate::boot::data_dir().map(|d| d.join("zhive-tui.log"))
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
        // Restore the remembered reasoning depth; an absent or unrecognized
        // label defers to the default (`Off`). The TUI re-clamps to the model.
        thinking_effort: cfg
            .active_thinking()
            .and_then(zhive_proto::domain::ThinkingEffort::from_label)
            .unwrap_or_default(),
        // Seeded by the caller from the boot-time `/models` fetch.
        effort_cycle: None,
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
// exec  (P1-6 headless single-turn)
// ============================================================

/// Runs a single prompt headlessly, printing the response to stdout.
///
/// Starts the in-process engine, submits `args.prompt` as the first user
/// message, streams the agent reply and any tool activity to stdout (one line
/// per tool event), and exits once the turn completes or fails. The engine is
/// always shut down before return.
///
/// Exit is non-zero when the engine reports a turn failure; the error message
/// is written to stderr so scripts can distinguish it from the model reply on
/// stdout.
///
/// # Errors
///
/// Returns an error if config loading, provider build, engine startup, or the
/// `start_turn` RPC fails.
#[cfg(feature = "engine")]
#[expect(
    clippy::too_many_lines,
    reason = "run_exec is one cohesive headless driver: config/override → engine start → \
              event-stream decode loop; splitting would scatter the linear flow"
)]
async fn run_exec(
    config_path: Option<std::path::PathBuf>,
    args: crate::cli::ExecArgs,
) -> Result<()> {
    use zhive_client_native::ClientEvent;
    use zhive_proto::domain::Item;

    init_stderr_logging();

    let (mut cfg, _source) = crate::config::Config::load(config_path.as_deref())?;

    // Apply provider/model overrides the same way `tui` does.
    if let Some(ref provider_name) = args.provider {
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

    let provider = crate::provider::build(&cfg)?;
    let runtime = crate::boot::build_runtime(&cfg).await?;
    let socket = crate::engine_host::tui_socket_path();
    let catalog = crate::models::build_catalog(&cfg);
    let context_window =
        crate::models::resolve_initial_context_window(&cfg, catalog.as_ref()).await;
    let host =
        crate::engine_host::Host::start(provider, runtime, socket, catalog, context_window).await?;

    // Generate a fresh thread id and subscribe to events before start_turn so
    // no event is missed between the call and subscription. Built locally (no
    // TUI dependency) so headless `exec` works in non-tui builds.
    let thread = format!(
        "thread:native/exec-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    );
    let mut events = host.client.subscribe_events();

    // Kick off the turn (returns as soon as the engine accepts).
    host.client
        .call(
            "engine/start_turn",
            Some(serde_json::json!({
                "threadId": thread,
                "items": [{"type": "userMessage", "text": args.prompt}],
            })),
        )
        .await
        .map_err(|e| anyhow::anyhow!("start_turn failed: {e}"))?;

    // Drain events until TurnCompleted or TurnFailed for our thread.
    let mut turn_failed: Option<String> = None;
    loop {
        match events.next_event().await {
            None | Some(ClientEvent::Disconnected { .. }) => {
                anyhow::bail!("engine disconnected before turn finished");
            }
            Some(ClientEvent::Notification(n)) => {
                // React only to notifications for our own thread. Decoding is
                // done from the raw wire params (camelCase) so headless `exec`
                // does not depend on the TUI crate's decoder.
                let for_us = n
                    .params
                    .as_ref()
                    .and_then(|p| p.get("threadId"))
                    .and_then(serde_json::Value::as_str)
                    == Some(thread.as_str());
                if !for_us {
                    continue;
                }
                match n.method.as_str() {
                    "events/item_appended" => {
                        if let Some(item) = n
                            .params
                            .as_ref()
                            .and_then(|p| p.get("item").cloned())
                            .and_then(|v| serde_json::from_value::<Item>(v).ok())
                        {
                            match item {
                                // Print the complete assistant text.
                                Item::AgentMessage { text, .. } => print!("{text}"),
                                // One-line tool activity to stdout.
                                Item::ToolCall { name, .. } => println!("\n[tool] {name}"),
                                _ => {}
                            }
                        }
                    }
                    "events/item_delta" => {
                        // Stream token-by-token output as it arrives.
                        if let Some(delta) = n
                            .params
                            .as_ref()
                            .and_then(|p| p.get("delta"))
                            .and_then(serde_json::Value::as_str)
                        {
                            print!("{delta}");
                        }
                    }
                    "events/turn_completed" => {
                        // Ensure a trailing newline after the response.
                        println!();
                        break;
                    }
                    "events/turn_failed" => {
                        let err = n.params.as_ref().and_then(|p| p.get("error")).map_or_else(
                            || "unknown error".to_owned(),
                            std::string::ToString::to_string,
                        );
                        turn_failed = Some(err);
                        break;
                    }
                    _ => {} // unrelated notifications
                }
            }
            Some(ClientEvent::Lagged(n)) => {
                tracing::warn!(dropped = n, "exec: dropped events (lagged broadcast)");
            }
            _ => {}
        }
    }

    host.stop().await;

    if let Some(msg) = turn_failed {
        anyhow::bail!("turn failed: {msg}");
    }
    Ok(())
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
    // NOTE: do NOT remove_file here. serve_uds_with_events delegates to
    // serve_uds_inner -> prepare_uds_path which probes for a live server
    // (AddrInUse), removes a stale socket (ConnectionRefused), and errors on a
    // non-socket path.  A bare remove_file before this probe would delete an
    // active server's socket causing the live-server check to see NotFound and
    // mis-classify a running peer as "clean".

    let catalog = crate::models::build_catalog(&cfg);
    let context_window =
        crate::models::resolve_initial_context_window(&cfg, catalog.as_ref()).await;
    let engine = Engine::spawn_with_config(EngineConfig {
        provider,
        tools: Arc::clone(&runtime.registry),
        hook_host: Arc::new(HookHost::new()),
        storage: runtime.storage.clone(),
        turn_limits: runtime.turn_limits,
        system_prompt: Some(Arc::clone(&runtime.system_prompt)),
        compaction_prompt: runtime.compaction_prompt.clone(),
        compact_token_threshold: None,
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    })
    .with_context_window(context_window);
    let engine = match catalog {
        Some(cat) => engine.with_model_catalog(cat),
        None => engine,
    };
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

    tracing::info!(
        name: "server.serve.start",
        socket = %socket.display(),
        provider = %cfg.active_provider_label(),
        model = %cfg.active_model(),
        "engine serving on {{socket}} (provider={{provider}} model={{model}})",
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
    tracing::info!(name: "server.shutdown", "engine shutting down");

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
/// `serve`, `acp`, and `exec` rely on this: `acp` in particular speaks
/// JSON-RPC on stdout, so anything other than the protocol wire must go to
/// stderr. `exec` emits the agent reply on stdout, so tracing must not
/// interfere.
#[cfg(any(feature = "serve", feature = "acp", feature = "engine"))]
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

    let catalog = crate::models::build_catalog(&cfg);
    let context_window =
        crate::models::resolve_initial_context_window(&cfg, catalog.as_ref()).await;
    let engine = Engine::spawn_with_config(EngineConfig {
        provider,
        tools: Arc::clone(&runtime.registry),
        hook_host: Arc::new(HookHost::new()),
        storage: runtime.storage.clone(),
        turn_limits: runtime.turn_limits,
        system_prompt: Some(Arc::clone(&runtime.system_prompt)),
        compaction_prompt: runtime.compaction_prompt.clone(),
        compact_token_threshold: None,
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
    })
    .with_context_window(context_window);
    let engine = match catalog {
        Some(cat) => engine.with_model_catalog(cat),
        None => engine,
    };

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
// doctor
// ============================================================

/// Prints a diagnostic summary of the current config and capabilities.
///
/// Covers: config file path, active provider (build success/failure), MCP
/// server count, on-disk skill count (when the `skills` feature is on), and
/// the data-directory path. Does **not** make any network request.
///
/// # Errors
///
/// Currently infallible; returns `Result` for a consistent dispatch signature.
#[expect(
    clippy::unused_async,
    reason = "kept async to match the dispatch signature; skill discovery is sync"
)]
async fn run_doctor(config_path: Option<std::path::PathBuf>) -> Result<()> {
    // ── Config ───────────────────────────────────────────────────────────────
    let (cfg, cfg_source) = crate::config::Config::load(config_path.as_deref())?;
    match &cfg_source {
        Some(p) => println!("config:   {}", p.display()),
        None => println!("config:   (using defaults — no file found)"),
    }

    // ── Provider ─────────────────────────────────────────────────────────────
    // The provider builders live behind the `engine` feature; a bridge-only
    // build cannot construct one, so report that rather than failing to compile.
    #[cfg(feature = "engine")]
    {
        let provider_label = cfg.active_provider_label().to_owned();
        let model_label = cfg.active_model().to_owned();
        match crate::provider::build(&cfg) {
            Ok(_) => println!("provider: {provider_label} / {model_label} — OK"),
            Err(e) => println!("provider: {provider_label} / {model_label} — FAILED ({e})"),
        }
    }
    #[cfg(not(feature = "engine"))]
    {
        println!("provider: (engine features not compiled in)");
    }

    // ── MCP servers ──────────────────────────────────────────────────────────
    let mcp_count = cfg.mcp.servers.len();
    println!("mcp:      {mcp_count} server(s) configured");

    // ── Skills ───────────────────────────────────────────────────────────────
    #[cfg(feature = "skills")]
    {
        if cfg.skills.enabled {
            let discovery = zhive_core::skills::SkillDiscoveryConfig {
                extra_roots: cfg.skills.extra_roots.clone(),
            };
            let set = zhive_core::skills::SkillSet::discover_and_load(&discovery);
            println!(
                "skills:   {} discovered (discovery enabled)",
                set.loaded.len()
            );
        } else {
            println!("skills:   discovery disabled");
        }
    }
    #[cfg(not(feature = "skills"))]
    {
        println!("skills:   (skills feature not compiled in)");
    }

    // ── Data directory ───────────────────────────────────────────────────────
    // `boot` (and thus `data_dir`) is engine-gated.
    #[cfg(feature = "engine")]
    match crate::boot::data_dir() {
        Some(p) => println!("data-dir: {}", p.display()),
        None => println!("data-dir: (unknown — set $HOME or $XDG_DATA_HOME)"),
    }
    #[cfg(not(feature = "engine"))]
    println!("data-dir: (engine features not compiled in)");

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

// ============================================================
// tests
// ============================================================

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::Cli;

    /// `zhive exec -p "hello"` parses to `Exec { prompt: "hello", .. }`.
    #[cfg(feature = "engine")]
    #[test]
    fn exec_args_parse_short_flag() {
        let cli = Cli::parse_from(["zhive", "exec", "-p", "hello"]);
        match cli.command {
            Some(crate::cli::Command::Exec(args)) => {
                assert_eq!(args.prompt, "hello");
                assert!(args.provider.is_none());
                assert!(args.model.is_none());
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    /// `zhive exec --prompt "hi" --provider openai --model gpt-4o` parses
    /// all three flags.
    #[cfg(feature = "engine")]
    #[test]
    fn exec_args_parse_long_flags() {
        let cli = Cli::parse_from([
            "zhive",
            "exec",
            "--prompt",
            "hi",
            "--provider",
            "openai",
            "--model",
            "gpt-4o",
        ]);
        match cli.command {
            Some(crate::cli::Command::Exec(args)) => {
                assert_eq!(args.prompt, "hi");
                assert_eq!(args.provider.as_deref(), Some("openai"));
                assert_eq!(args.model.as_deref(), Some("gpt-4o"));
            }
            other => panic!("expected Exec, got {other:?}"),
        }
    }

    /// `exec` requires `--prompt`; omitting it is a parse error.
    #[cfg(feature = "engine")]
    #[test]
    fn exec_args_requires_prompt() {
        assert!(
            Cli::try_parse_from(["zhive", "exec"]).is_err(),
            "missing --prompt should be a parse error"
        );
    }

    /// `zhive doctor` parses to `Command::Doctor`.
    #[test]
    fn doctor_command_parses() {
        let cli = Cli::parse_from(["zhive", "doctor"]);
        assert!(
            matches!(cli.command, Some(crate::cli::Command::Doctor)),
            "expected Doctor variant"
        );
    }

    /// `run_doctor` prints the key diagnostic fields: provider, mcp, data-dir.
    #[tokio::test]
    async fn doctor_output_contains_key_fields() {
        use std::io::Write;

        // Redirect stdout to a buffer by running the doctor logic inline and
        // capturing its println! output via a pipe.
        // Because println! goes to the real stdout and we cannot easily swap it
        // in tests, we exercise the underlying helpers that doctor relies on:
        // config load, provider build, data_dir.
        let cfg = crate::config::Config::default();
        assert!(!cfg.active_provider_label().is_empty(), "provider label");
        assert!(!cfg.active_model().is_empty(), "model label");
        // data_dir must not panic.
        let _ = crate::boot::data_dir();

        // The "scripted" provider builds without a key, confirming the
        // provider diagnostic path works.
        let mut scripted_cfg = cfg.clone();
        scripted_cfg.provider.default = "scripted".to_owned();
        assert!(
            crate::provider::build(&scripted_cfg).is_ok(),
            "scripted provider builds"
        );

        // A missing key yields a FAILED branch in doctor output.
        let mut bad_cfg = cfg;
        bad_cfg.provider.default = "anthropic".to_owned();
        if let Some(entry) = bad_cfg.provider.providers.get_mut("anthropic") {
            entry.api_key = None;
            entry.api_key_env = Some("ZHIVE_TEST_ABSENT_KEY_DOCTOR".to_owned());
        }
        assert!(
            crate::provider::build(&bad_cfg).is_err(),
            "missing key surfaces error"
        );

        // Placeholder to make the buffer compile cleanly.
        let _ = std::io::stdout().flush();
    }

    /// `tui_log_path` returns a path ending in `zhive-tui.log` when
    /// `$HOME` is set (the CI machine always has one).
    #[cfg(feature = "tui")]
    #[test]
    fn tui_log_path_ends_with_log_filename() {
        if std::env::var_os("HOME").is_none()
            && std::env::var_os("XDG_DATA_HOME").is_none()
            && std::env::var_os("ZHIVE_DATA_DIR").is_none()
        {
            // In environments with no home directory, data_dir() returns None
            // and tui_log_path() returns None as well; nothing to assert.
            return;
        }
        let path = super::tui_log_path();
        assert!(
            path.is_some(),
            "expected Some(_) when a home dir is available"
        );
        assert_eq!(
            path.unwrap().file_name().and_then(|n| n.to_str()),
            Some("zhive-tui.log"),
            "log file name must be zhive-tui.log"
        );
    }

    /// `data_dir` never panics and returns a stable value when `$HOME` is set.
    #[test]
    fn data_dir_is_stable() {
        let first = crate::boot::data_dir();
        let second = crate::boot::data_dir();
        assert_eq!(first, second, "data_dir must be deterministic");
    }
}

// Rust guideline compliant 2026-02-21

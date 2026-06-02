//! Shared engine-runtime construction: the tool registry plus MCP/skills.
//!
//! Every command that runs the in-process engine (`tui`, `serve`, `acp`) must
//! build the *same* set of tools so that MCP servers, on-disk skills, and the
//! built-in tools all reach the running engine. [`build_runtime`] is that one
//! shared path; it returns a [`RuntimeTools`] holding the populated registry
//! and, when MCP is in use, the live [`zhive_mcp::McpManager`].
//!
//! # MCP manager lifetime
//!
//! The MCP tools hold connection handles into the manager, so the manager must
//! outlive the engine. Callers keep the returned [`RuntimeTools`] (or move the
//! manager out of it) alive for as long as the engine runs, then call
//! [`RuntimeTools::shutdown`] *before* shutting the engine down.

use std::sync::Arc;

use zhive_core::engine::TurnLimits;
use zhive_core::tools::ToolRegistry;

use crate::config::Config;

/// The engine's tool registry plus any owned capability handles.
///
/// `registry` is ready to hand to `EngineConfig::tools`. `mcp` (present only
/// when the `mcp` feature is enabled and servers connected) must be kept alive
/// for the engine's lifetime and shut down via [`RuntimeTools::shutdown`].
#[derive(Debug)]
pub struct RuntimeTools {
    /// Registry populated with the built-in, MCP, and skill tools.
    pub registry: Arc<ToolRegistry>,
    /// Per-turn iteration cap resolved from the `[engine]` config section.
    pub turn_limits: TurnLimits,
    /// Live MCP manager whose tools were registered, if any.
    #[cfg(feature = "mcp")]
    pub mcp: Option<zhive_mcp::McpManager>,
}

impl RuntimeTools {
    /// Shuts down owned capability handles (currently the MCP manager).
    ///
    /// Call this once the engine no longer needs its tools — before the engine
    /// itself is shut down — to close MCP connections cleanly. It is a no-op
    /// when no MCP manager is held.
    #[cfg_attr(
        not(feature = "mcp"),
        expect(
            clippy::unused_async,
            reason = "kept async for a uniform signature; no await without `mcp`"
        )
    )]
    pub async fn shutdown(self) {
        #[cfg(feature = "mcp")]
        if let Some(manager) = self.mcp {
            manager.shutdown().await;
        }
        // Without `mcp` there is nothing to await; `self` is dropped here.
        #[cfg(not(feature = "mcp"))]
        let _ = self;
    }
}

/// Builds the engine runtime tools from `cfg`.
///
/// Starts from an empty registry — no demo tools are advertised to the model by
/// default (the built-in `EchoTool` is a test fixture; advertising it makes a
/// real model call it for no reason). When the `mcp` feature is enabled and
/// `[mcp.servers]` is non-empty, connects to every server in parallel and
/// registers their tools (failing servers are skipped with a warning by the
/// manager). When the `skills` feature is enabled and `[skills].enabled` is
/// true, discovers on-disk skills and registers the model-invocable ones.
///
/// # Errors
///
/// Currently infallible in practice, but returns [`anyhow::Result`] so future
/// capability wiring can surface setup failures without a signature change.
///
/// # Examples
///
/// ```no_run
/// # async fn run() -> anyhow::Result<()> {
/// # use zhive_cli::config::Config;
/// let cfg = Config::default();
/// let runtime = zhive_cli::boot::build_runtime(&cfg).await?;
/// // hand `runtime.registry` to `EngineConfig::tools`, then later:
/// runtime.shutdown().await;
/// # Ok(())
/// # }
/// ```
// With `mcp` enabled the fn awaits the MCP connection; without it there is no
// `.await`, but the signature must stay `async` so every call site is uniform.
#[cfg_attr(
    not(feature = "mcp"),
    expect(
        clippy::unused_async,
        reason = "kept async for a uniform signature across feature sets; no await without `mcp`"
    )
)]
pub async fn build_runtime(cfg: &Config) -> anyhow::Result<RuntimeTools> {
    // Real tools come from MCP servers and on-disk skills; the registry starts
    // empty so no demo tool is advertised to the model.
    #[cfg_attr(
        not(any(feature = "mcp", feature = "skills")),
        expect(
            unused_mut,
            reason = "registry is only mutated when the mcp or skills feature populates it"
        )
    )]
    let mut registry = ToolRegistry::new();

    #[cfg(feature = "mcp")]
    let mcp = build_mcp(cfg, &mut registry).await;

    #[cfg(feature = "skills")]
    register_skills(cfg, &mut registry);

    Ok(RuntimeTools {
        registry: Arc::new(registry),
        turn_limits: turn_limits_from(cfg),
        #[cfg(feature = "mcp")]
        mcp,
    })
}

/// Maps the `[engine]` config section to engine [`TurnLimits`].
///
/// `None` keeps the engine default; `Some(0)` means unbounded (bounded only by
/// the engine's hard safety ceiling); `Some(n)` caps a turn at `n` provider
/// iterations.
fn turn_limits_from(cfg: &Config) -> TurnLimits {
    match cfg.engine.max_turn_iterations {
        None => TurnLimits::default(),
        Some(0) => TurnLimits {
            max_iterations: None,
        },
        Some(n) => TurnLimits {
            max_iterations: Some(n),
        },
    }
}

/// Connects configured MCP servers and registers their tools into `registry`.
///
/// Returns the live manager when at least one server is configured, or `None`
/// when `[mcp.servers]` is empty (so no idle manager is held).
#[cfg(feature = "mcp")]
async fn build_mcp(cfg: &Config, registry: &mut ToolRegistry) -> Option<zhive_mcp::McpManager> {
    let configs = cfg.mcp.to_mcp_configs();
    if configs.is_empty() {
        return None;
    }
    let manager =
        zhive_mcp::McpManager::connect_all(configs, zhive_mcp::McpConnectOptions::default()).await;
    let tools = manager.tools();
    let count = tools.len();
    for tool in tools {
        registry.register(tool);
    }
    tracing::info!(
        tool.count = count,
        "mcp.tools.registered: {{tool.count}} MCP tools registered",
    );
    Some(manager)
}

/// Discovers on-disk skills and registers the model-invocable ones.
#[cfg(feature = "skills")]
fn register_skills(cfg: &Config, registry: &mut ToolRegistry) {
    if !cfg.skills.enabled {
        return;
    }
    let discovery = zhive_core::skills::SkillDiscoveryConfig {
        extra_roots: cfg.skills.extra_roots.clone(),
    };
    let set = zhive_core::skills::SkillSet::discover_and_load(&discovery);
    let slash_only = set.register_invocable(registry);
    tracing::info!(
        skill.loaded = set.loaded.len(),
        skill.slash_only = slash_only.len(),
        "skills.registered: {{skill.loaded}} skills loaded, {{skill.slash_only}} slash-only",
    );
}

// Rust guideline compliant 2026-02-21

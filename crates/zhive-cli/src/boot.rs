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

/// A discovered skill prepared for the host UI (name, description, invocation).
///
/// A plain owned-`String` handoff type so [`RuntimeTools`] stays free of the
/// feature-gated `zhive_core::skills` types and of any TUI dependency. The
/// `invocation` is the rendered `<skill>` block injected as a user message when
/// the skill is run from a slash command or the picker.
///
/// # Examples
///
/// ```
/// # use zhive_cli::boot::RuntimeSkill;
/// let s = RuntimeSkill {
///     name: "demo".to_owned(),
///     description: "Does a thing".to_owned(),
///     invocation: "<skill name=\"demo\" location=\"/x/SKILL.md\">\n…\n</skill>".to_owned(),
/// };
/// assert_eq!(s.name, "demo");
/// ```
#[derive(Debug, Clone)]
pub struct RuntimeSkill {
    /// Skill identifier (the frontmatter `name`).
    pub name: String,
    /// One-line description for the picker; empty when none was declared.
    pub description: String,
    /// Pre-rendered `<skill>` invocation block (append `\n\n<args>` for args).
    pub invocation: String,
}

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
    /// System prompt assembled by [`crate::system_prompt`], ready to hand to
    /// `EngineConfig::system_prompt`.
    pub system_prompt: Arc<str>,
    /// Compaction summarization instruction rendered from the `compaction/summary`
    /// template, or `None` to let the engine use its built-in default. Handed to
    /// `EngineConfig::compaction_prompt`.
    pub compaction_prompt: Option<Arc<str>>,
    /// Persistent storage for the engine, or `None` when it could not be
    /// opened (the engine then runs purely in-memory). See [`open_storage`].
    pub storage: Option<Arc<zhive_core::persistence::Storage>>,
    /// All skills discovered during boot, each with a rendered `<skill>`
    /// invocation block. Drives the TUI's `/skills` picker and `/skill:<name>`
    /// slash execution. Empty when the `skills` feature is absent or
    /// `skills.enabled` is false.
    ///
    /// Consumed by `run_tui`, which maps these to the TUI's own skill type
    /// (the TUI must not depend on `zhive_core`). Unused in non-tui builds.
    #[cfg_attr(
        not(feature = "tui"),
        expect(
            dead_code,
            reason = "only consumed by run_tui for the TUI skill picker/slash; unused in non-tui builds"
        )
    )]
    pub skills: Vec<RuntimeSkill>,
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
/// Registers the built-in coding tools (read/write/edit/grep/glob/bash) as
/// the base layer so the engine is usable out of the box. When the `mcp`
/// feature is enabled and `[mcp.servers]` is non-empty, connects to every
/// server in parallel and registers their tools (failing servers are skipped
/// with a warning by the manager). When the `skills` feature is enabled and
/// `[skills].enabled` is true, discovers on-disk skills and folds the
/// model-invocable ones into the system prompt as an `<available_skills>`
/// catalogue the model reads on demand (they are no longer registered as
/// tools). MCP tools may override built-ins by name.
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
    let mut registry = ToolRegistry::new();

    // The built-in coding tools (read/write/edit/grep/glob/bash) are the
    // base layer that makes the engine usable out of the box. MCP server tools
    // and on-disk skills are layered on afterwards and may override by name.
    zhive_core::tools::builtin::register_builtins(
        &mut registry,
        &zhive_core::tools::builtin::BuiltinToolsConfig {
            respect_gitignore: cfg.tools.respect_gitignore,
            ..Default::default()
        },
    );

    #[cfg(feature = "mcp")]
    let mcp = build_mcp(cfg, &mut registry).await;

    #[cfg(feature = "skills")]
    let (skills, skills_section) = prepare_skills(cfg);
    #[cfg(not(feature = "skills"))]
    let (skills, skills_section): (Vec<RuntimeSkill>, Option<String>) = (Vec::new(), None);

    // The system prompt is a host (process) concern: it folds in the working
    // directory and the project's instruction file. A failed `current_dir`
    // (e.g. the directory was removed) degrades to ".".
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // The active provider's name (user-chosen label) and kind (backend type)
    // select the per-provider persona; kind is empty only if the configured
    // default does not match any `[provider.<name>]` entry.
    let provider_name = cfg.provider.default.as_str();
    let active_entry = cfg.provider.providers.get(&cfg.provider.default);
    let provider_kind = active_entry.map_or("", |entry| entry.kind.as_str());
    // An empty model (e.g. the scripted demo) is treated as "unknown".
    let model = active_entry
        .map(|entry| entry.model.as_str())
        .filter(|m| !m.is_empty());
    let system_prompt = crate::system_prompt::assemble(
        &cwd,
        provider_name,
        provider_kind,
        model,
        skills_section.as_deref(),
    );
    let compaction_prompt =
        crate::system_prompt::assemble_compaction(&cwd, provider_name, provider_kind, model);

    // Open persistent storage so threads, turns, and items survive restarts
    // (D-011). Best-effort: a failure degrades to an in-memory engine.
    let storage = open_storage().await;

    Ok(RuntimeTools {
        registry: Arc::new(registry),
        turn_limits: turn_limits_from(cfg),
        system_prompt,
        compaction_prompt,
        storage,
        skills,
        #[cfg(feature = "mcp")]
        mcp,
    })
}

/// Opens persistent engine storage under the user data directory.
///
/// Returns `None` (the engine then runs in-memory) when the data directory
/// cannot be resolved or the databases cannot be opened, logging a warning —
/// persistence is best-effort and must never block startup.
async fn open_storage() -> Option<Arc<zhive_core::persistence::Storage>> {
    let Some(base) = data_dir() else {
        tracing::warn!(
            name: "zhive.storage.no_data_dir",
            "no data directory (set $HOME or $XDG_DATA_HOME); running in-memory"
        );
        return None;
    };
    match zhive_core::persistence::Storage::open(&base).await {
        Ok(storage) => {
            tracing::info!(
                name: "zhive.storage.opened",
                path = %base.display(),
                "storage.opened: persistent storage at {{path}}"
            );
            // Best-effort metadata backfill for sessions recorded before the
            // live turn path filled `preview` / `cwd` (idempotent). A failure
            // here must never block startup — log and continue with the opened
            // storage regardless.
            match storage.backfill_thread_metadata().await {
                Ok(stats) if stats.scanned > 0 => {
                    // Bind the counts to locals so the structured-logging macro
                    // sees simple identifiers on the value side (a dotted field
                    // access there triggers a macro parse ambiguity).
                    let scanned = stats.scanned;
                    let previews_filled = stats.previews_filled;
                    let cwds_filled = stats.cwds_filled;
                    tracing::info!(
                        name: "zhive.storage.backfill.done",
                        scanned,
                        previews_filled,
                        cwds_filled,
                        "storage.backfill: scanned {{scanned}} thread(s), \
                         filled {{previews_filled}} preview(s) and {{cwds_filled}} cwd(s)"
                    );
                }
                Ok(_) => {
                    // Nothing to backfill (no empty-preview rows); stay quiet.
                }
                Err(err) => {
                    tracing::warn!(
                        name: "zhive.storage.backfill.failed",
                        error = %err,
                        "storage backfill failed; continuing without it"
                    );
                }
            }
            Some(Arc::new(storage))
        }
        Err(err) => {
            tracing::warn!(
                name: "zhive.storage.open_failed",
                path = %base.display(),
                error = %err,
                "storage open failed; running in-memory"
            );
            None
        }
    }
}

/// Resolves the zhive data directory from the environment.
///
/// Honours `$ZHIVE_DATA_DIR`, then `$XDG_DATA_HOME/zhive`, then
/// `$HOME/.local/share/zhive`. Returns `None` when none can be resolved.
///
/// # Examples
///
/// ```
/// // Returns None when HOME and XDG_DATA_HOME are unset; otherwise a PathBuf.
/// let _ = zhive_cli::boot::data_dir();
/// ```
pub fn data_dir() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Some(explicit) = std::env::var_os("ZHIVE_DATA_DIR") {
        return Some(PathBuf::from(explicit));
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("zhive"));
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("zhive"),
    )
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

/// Discovers on-disk skills and prepares them for the host.
///
/// Returns `(skills, available_skills)`:
/// * `skills` — every discovered skill as a [`RuntimeSkill`] (name, description,
///   rendered `<skill>` block), driving the TUI `/skills` picker and
///   `/skill:<name>` slash execution. Includes slash-only skills.
/// * `available_skills` — the `<available_skills>` catalogue folded into the
///   system prompt for model auto-discovery (excludes slash-only skills), or
///   `None` when no model-invocable skill exists.
///
/// Returns `(Vec::new(), None)` when `cfg.skills.enabled` is false. Skills are
/// **not** registered as tools: the model discovers them from the prompt list
/// and reads each `SKILL.md` on demand via the `read` tool (progressive
/// disclosure), so the tool surface stays flat regardless of skill count.
#[cfg(feature = "skills")]
fn prepare_skills(cfg: &Config) -> (Vec<RuntimeSkill>, Option<String>) {
    if !cfg.skills.enabled {
        return (Vec::new(), None);
    }
    let discovery = zhive_core::skills::SkillDiscoveryConfig {
        extra_roots: cfg.skills.extra_roots.clone(),
    };
    let set = zhive_core::skills::SkillSet::discover_and_load(&discovery);
    let available = set.render_available_skills();
    let skills: Vec<RuntimeSkill> = set
        .catalogue()
        .into_iter()
        .map(|entry| RuntimeSkill {
            name: entry.name,
            description: entry.description.unwrap_or_default(),
            invocation: entry.invocation,
        })
        .collect();
    let invocable = available.is_some();
    tracing::info!(
        skill.loaded = skills.len(),
        skill.has_invocable = invocable,
        "skills.prepared: {{skill.loaded}} skills loaded, prompt section: {{skill.has_invocable}}",
    );
    (skills, available)
}

// Rust guideline compliant 2026-02-21

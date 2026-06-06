//! In-process engine host: spawns the engine, serves it over UDS, connects.
//!
//! The `tui` command runs the engine in the same process as the UI but still
//! talks to it as a JSON-RPC client over a Unix socket (D-002/D-004) — the same
//! path an external client or the stdio bridge would take. This reuses the
//! proven `serve_uds_with_events` forwarder rather than inventing an in-memory
//! transport. The socket lives in `$XDG_RUNTIME_DIR` (or the temp dir) under a
//! per-pid name and is removed on shutdown.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use zhive_client_native::Client;
use zhive_core::engine::{Engine, EngineConfig, ModelCatalog};
use zhive_core::hooks::HookHost;
use zhive_core::provider::DynLanguageModel;
use zhive_core::server::{
    DEFAULT_MAX_CONNECTIONS, Router, register_engine_handlers, serve_uds_with_events,
};

use crate::boot::RuntimeTools;

/// A running engine plus the connected client and its lifecycle handles.
///
/// The owned [`RuntimeTools`] keeps the MCP manager (and thus its tool
/// connections) alive for as long as the engine runs; [`Host::stop`] shuts it
/// down before the engine.
#[derive(Debug)]
pub struct Host {
    /// The connected, handshaked client the TUI drives.
    pub client: Client,
    engine: Engine,
    shutdown: CancellationToken,
    socket: PathBuf,
    /// Tool registry plus capability handles (e.g. the live MCP manager).
    runtime: Option<RuntimeTools>,
}

impl Host {
    /// Spawns the engine with `provider` + `runtime`, serves it, and connects.
    ///
    /// `runtime` carries the tool registry (handed to the engine) and any live
    /// capability handles such as the MCP manager, which the `Host` keeps alive
    /// for the engine's lifetime and shuts down in [`Host::stop`].
    ///
    /// `catalog` (when present) backs the `models/list` / `engine/set_model`
    /// RPCs, `context_window` seeds the auto-compaction budget, and
    /// `max_output_tokens` seeds the per-turn request cap for the
    /// initially-bound model.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket never appears or the client cannot
    /// complete the initialize handshake.
    pub async fn start(
        provider: DynLanguageModel,
        runtime: RuntimeTools,
        socket: PathBuf,
        catalog: Option<Arc<dyn ModelCatalog>>,
        context_window: Option<u64>,
        max_output_tokens: Option<u64>,
    ) -> anyhow::Result<Self> {
        // A stale socket from a crashed run would block binding.
        let _ = std::fs::remove_file(&socket);

        // NOTE: do NOT remove_file here. serve_uds_with_events delegates to
        // serve_uds_inner -> prepare_uds_path which correctly probes for a live
        // server, removes stale sockets, and errors on non-socket paths.
        // A bare remove_file before this probe would silently delete an active
        // server's socket and cause the live-server check to mis-classify a
        // running peer as "clean" (it would observe NotFound instead of
        // AddrInUse), allowing two servers to share a socket path.
        let engine = Engine::spawn_with_config(EngineConfig {
            provider,
            tools: std::sync::Arc::clone(&runtime.registry),
            hook_host: std::sync::Arc::new(HookHost::new()),
            storage: runtime.storage.clone(),
            turn_limits: runtime.turn_limits,
            system_prompt: Some(std::sync::Arc::clone(&runtime.system_prompt)),
            compaction_prompt: runtime.compaction_prompt.clone(),
            compact_token_threshold: None,
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        })
        .with_context_window(context_window)
        .with_max_output_tokens(max_output_tokens);
        // Wire the host model catalogue when the provider kind supports one.
        let engine = match catalog {
            Some(cat) => engine.with_model_catalog(cat),
            None => engine,
        };
        let mut router = Router::new();
        register_engine_handlers(&mut router, engine.clone());
        let router = std::sync::Arc::new(router);
        let shutdown = CancellationToken::new();

        let serve_socket = socket.clone();
        let serve_engine = engine.clone();
        let serve_shutdown = shutdown.clone();
        tokio::spawn(async move {
            if let Err(err) = serve_uds_with_events(
                &serve_socket,
                router,
                serve_engine,
                DEFAULT_MAX_CONNECTIONS,
                serve_shutdown,
            )
            .await
            {
                tracing::error!(error = %err, "engine UDS server exited with error");
            }
        });

        if let Err(err) = wait_for_socket(&socket).await {
            // `Host` is not constructed here; tear the half-started engine and
            // capability handles down so MCP connections do not leak.
            shutdown.cancel();
            let _ = engine.shutdown().await;
            runtime.shutdown().await;
            let _ = std::fs::remove_file(&socket);
            return Err(err);
        }
        let client = match Client::connect_uds(&socket).await {
            Ok(client) => client,
            Err(err) => {
                // `Host` is never constructed on this path, so its `Drop` will
                // not fire; tear the half-started engine and capability handles
                // down and remove the socket here to avoid leaking them.
                shutdown.cancel();
                let _ = engine.shutdown().await;
                runtime.shutdown().await;
                let _ = std::fs::remove_file(&socket);
                return Err(err)
                    .with_context(|| format!("connecting to engine at {}", socket.display()));
            }
        };

        Ok(Self {
            client,
            engine,
            shutdown,
            socket,
            runtime: Some(runtime),
        })
    }

    /// Shuts the engine down, stops serving, and removes the socket file.
    ///
    /// Capability handles (the MCP manager) are shut down *before* the engine,
    /// since their tools may still be referenced until the engine stops.
    pub async fn stop(mut self) {
        // `Host` implements `Drop`, so fields cannot be moved out of `self`;
        // a clone of the cheap `Arc`-backed client carries the shutdown signal.
        // `shutdown` is now async and returns Result; we ignore the value here
        // since a timeout or Err at teardown is non-fatal.
        let _ = self.client.clone().shutdown().await;
        // Close MCP connections before the engine that dispatches through them.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown().await;
        }
        let _ = self.engine.shutdown().await;
        self.shutdown.cancel();
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Drop for Host {
    /// Safety net: remove the per-pid socket if `stop` never ran (e.g. a panic
    /// unwinding through the TUI). `stop` removes it too; a double remove of an
    /// already-gone file is harmless.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Per-pid socket path under `$XDG_RUNTIME_DIR` (or the temp dir).
#[must_use]
pub fn tui_socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map_or_else(std::env::temp_dir, PathBuf::from);
    dir.join(format!("zhive-tui-{}.sock", std::process::id()))
}

/// Polls until `socket` exists, up to a two-second deadline.
async fn wait_for_socket(socket: &Path) -> anyhow::Result<()> {
    // Two seconds is generous for a local bind; if it is not up by then the
    // serve task almost certainly failed and the connect would hang.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !socket.exists() {
        if Instant::now() >= deadline {
            anyhow::bail!("engine socket {} did not appear", socket.display());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

// Rust guideline compliant 2026-02-21

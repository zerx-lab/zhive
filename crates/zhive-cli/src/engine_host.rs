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
use tokio_util::sync::CancellationToken;
use zhive_client_native::Client;
use zhive_core::engine::Engine;
use zhive_core::provider::DynLanguageModel;
use zhive_core::server::{
    DEFAULT_MAX_CONNECTIONS, Router, register_engine_handlers, serve_uds_with_events,
};

/// A running engine plus the connected client and its lifecycle handles.
#[derive(Debug)]
pub struct Host {
    /// The connected, handshaked client the TUI drives.
    pub client: Client,
    engine: Engine,
    shutdown: CancellationToken,
    socket: PathBuf,
}

impl Host {
    /// Spawns the engine with `provider`, serves it on `socket`, and connects.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket never appears or the client cannot
    /// complete the initialize handshake.
    pub async fn start(provider: DynLanguageModel, socket: PathBuf) -> anyhow::Result<Self> {
        // A stale socket from a crashed run would block binding.
        let _ = std::fs::remove_file(&socket);

        let engine = Engine::spawn_with_provider(provider);
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

        wait_for_socket(&socket).await?;
        let client = match Client::connect_uds(&socket).await {
            Ok(client) => client,
            Err(err) => {
                // `Host` is never constructed on this path, so its `Drop` will
                // not fire; tear the half-started engine down and remove the
                // socket here to avoid leaking it.
                shutdown.cancel();
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
        })
    }

    /// Shuts the engine down, stops serving, and removes the socket file.
    pub async fn stop(self) {
        // `Host` implements `Drop`, so fields cannot be moved out of `self`;
        // a clone of the cheap `Arc`-backed client carries the shutdown signal.
        self.client.clone().shutdown();
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

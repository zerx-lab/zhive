//! JSON-RPC server module: stdio + Unix-domain-socket transports.
//!
//! The server runs as a tokio actor: one [`super::engine::Engine`]
//! holds the live state, and the server bridges every connected client
//! to that engine via a method-name [`Router`].
//!
//! ## Transports
//!
//! * [`serve_stdio`] — owns the process stdin / stdout pair. Used by
//!   `zhive serve --stdio` and by parent processes (Zed, Claude Desktop)
//!   that spawn zhive as a subprocess.
//! * [`serve_uds`] — listens on a Unix-domain socket. The default path
//!   is `$XDG_RUNTIME_DIR/zhive.sock` (see [`path::default_socket_path`]),
//!   with `/tmp/zhive-<uid>.sock` as a fallback. The socket file is
//!   `chmod 0600`'d so only the owning user can connect (D-004).
//!
//! ## Concurrency model
//!
//! Per connection, [`serve_loop`] dispatches requests serially: the
//! upstream engine actor enforces a single-active-turn invariant, so
//! parallel dispatch inside one connection would only let later
//! requests queue behind the same engine lock with extra spawn cost.
//! Cross-connection concurrency is real — [`serve_uds`] accepts many
//! connections concurrently and each runs its own [`serve_loop`].
//!
//! ## Graceful shutdown
//!
//! Both serve functions accept a [`tokio_util::sync::CancellationToken`].
//! When the token fires, [`serve_loop`] stops reading from the transport
//! (without aborting in-flight dispatches mid-await) and [`serve_uds`]
//! stops accepting new connections and waits for spawned per-connection
//! tasks to drain.
//!
//! ## Per-listener connection cap
//!
//! [`serve_uds`] takes a `max_connections` value and enforces it with a
//! [`tokio::sync::Semaphore`]. A misbehaving local process cannot fork
//! enough connections to exhaust the engine actor or the file descriptor
//! table.
//!
//! ## What lives where
//!
//! The router covers request / notification dispatch only. Engine-level
//! concerns (turn lifecycle, hook host, permission reducer, cancel
//! propagation) belong to [`super::engine`] and are wired into the
//! router via thin glue handlers as later Block B tasks land.

pub mod events;
pub mod handlers;
pub mod initialize;
pub mod path;
pub mod reverse_rpc;
pub mod router;
pub mod serve_loop;
pub mod transport;

#[doc(inline)]
pub use events::{
    EventFilter, SharedEventFilter, engine_event_to_notification, spawn_event_forwarder,
};
#[doc(inline)]
pub use handlers::{ENGINE_ERROR_CODE, register_engine_handlers};
#[doc(inline)]
pub use initialize::{server_capabilities, server_identity};
#[doc(inline)]
pub use reverse_rpc::{ResolveOutcome, ReverseRpcError, ReverseRpcResult, ReverseRpcTracker};
#[doc(inline)]
pub use router::{Handler, JsonRpcCode, Router, error_object};
#[doc(inline)]
pub use serve_loop::{
    SubscribeParams, serve_loop, serve_loop_with_outbound, serve_loop_with_reverse,
};
#[doc(inline)]
pub use transport::{StdioTransport, Transport, TransportError};

#[cfg(unix)]
#[doc(inline)]
pub use transport::UdsTransport;

use std::path::Path;
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use zhive_proto::Message;

/// Default capacity for the per-connection outbound queue used by
/// [`serve_loop_with_outbound`]. Sized to absorb burst notification
/// traffic (e.g. a turn that emits dozens of `ItemAppended` events in
/// quick succession) without back-pressuring the engine actor.
pub const DEFAULT_OUTBOUND_QUEUE_CAP: usize = 256;

/// Default per-listener concurrent connection cap used by
/// [`serve_uds`] when callers want a sensible value without picking
/// their own. Picked to comfortably support a small fleet of clients
/// (TUI + IDE + bridge) without ever approaching the default `RLIMIT_NOFILE`.
pub const DEFAULT_MAX_CONNECTIONS: usize = 64;

/// Classifies an [`accept`](tokio::net::UnixListener::accept) error as
/// transient (recoverable by the next call) vs fatal.
///
/// Treats fd-exhaustion (`EMFILE` / `ENFILE`), already-closed peer
/// connections (`ConnectionAborted`), and `Interrupted` (`EINTR`) as
/// transient so a single misbehaving caller cannot tear down the
/// listener for everyone else.
#[cfg(unix)]
fn accept_is_transient(err: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    if matches!(
        err.kind(),
        ErrorKind::Interrupted | ErrorKind::ConnectionAborted
    ) {
        return true;
    }
    // Compare against `rustix::io::Errno` constants rather than
    // hard-coding the numeric values; rustix is already a dependency
    // for the UDS path module (red line 2).
    err.raw_os_error().is_some_and(|code| {
        code == rustix::io::Errno::MFILE.raw_os_error()
            || code == rustix::io::Errno::NFILE.raw_os_error()
    })
}

/// Failure modes surfaced by [`serve_stdio`] / [`serve_uds`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServerError {
    /// Transport-level failure (framing or I/O).
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),

    /// Filesystem failure when binding or removing the UDS socket file.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// `max_connections` was `0`, which would deadlock the semaphore.
    #[error("max_connections must be >= 1")]
    InvalidMaxConnections,

    /// Another zhive server is already listening on the requested UDS socket path.
    ///
    /// Returned by [`serve_uds`] / [`serve_uds_with_events`] when a connection
    /// probe in [`prepare_uds_path`] succeeds, indicating a live server is
    /// already bound at the given socket path.
    #[error("zhive server already running at {path}")]
    UdsAlreadyRunning {
        /// Filesystem path of the active server's UDS socket.
        path: String,
    },
}

/// Owns the process stdin/stdout and runs [`serve_loop`] on them.
///
/// The supplied [`CancellationToken`] doubles as the soft shutdown
/// signal: when it fires the loop returns without aborting an
/// in-flight dispatch.
///
/// # Errors
///
/// Returns [`ServerError`] when the transport or the dispatch loop
/// fails.
pub async fn serve_stdio(
    router: Arc<Router>,
    shutdown: CancellationToken,
) -> Result<(), ServerError> {
    let mut transport = StdioTransport::new();
    serve_loop(&mut transport, router, shutdown).await
}

/// Binds a UDS listener at `socket_path` and spawns a serve task per
/// inbound connection.
///
/// `max_connections` caps the number of concurrent connections via a
/// [`tokio::sync::Semaphore`]; pick [`DEFAULT_MAX_CONNECTIONS`] for the
/// sensible default. `shutdown` propagates to every spawned connection
/// task and stops the accept loop; the function only returns once all
/// in-flight tasks have drained.
///
/// # Errors
///
/// * [`ServerError::Io`] when the bind, permissions, or accept syscalls
///   fail.
/// * [`ServerError::InvalidMaxConnections`] when `max_connections` is
///   `0`.
#[cfg(unix)]
pub async fn serve_uds(
    socket_path: &Path,
    router: Arc<Router>,
    max_connections: usize,
    shutdown: CancellationToken,
) -> Result<(), ServerError> {
    serve_uds_inner(socket_path, router, max_connections, None, shutdown).await
}

/// Like [`serve_uds`] but also spawns a per-connection event
/// forwarder that pushes [`crate::engine::EngineEvent`]s as JSON-RPC
/// notifications to every connected client.
///
/// Each new connection subscribes to `engine.subscribe()` and
/// receives the live event stream from that point forward (broadcast
/// catch-up is not provided; clients that need historical events
/// should query the persistence layer once it lands in B3).
///
/// # Errors
///
/// Same as [`serve_uds`].
#[cfg(unix)]
pub async fn serve_uds_with_events(
    socket_path: &Path,
    router: Arc<Router>,
    engine: crate::engine::Engine,
    max_connections: usize,
    shutdown: CancellationToken,
) -> Result<(), ServerError> {
    serve_uds_inner(socket_path, router, max_connections, Some(engine), shutdown).await
}

/// Spawns one connection-handling task into `tasks`.
///
/// Pulled out of [`serve_uds_inner`] so the body of that loop stays
/// short enough to satisfy `clippy::too_many_lines`.
#[cfg(unix)]
fn spawn_connection(
    tasks: &mut tokio::task::JoinSet<()>,
    stream: tokio::net::UnixStream,
    permit: tokio::sync::OwnedSemaphorePermit,
    router: Arc<Router>,
    shutdown: CancellationToken,
    engine_for_events: Option<crate::engine::Engine>,
) {
    use std::sync::Mutex;
    tasks.spawn(async move {
        let _permit = permit; // released when task exits
        let mut transport = UdsTransport::new(stream);
        // When events forwarding is enabled, create a per-connection
        // outbound channel and spawn a forwarder that pumps engine
        // events into it.  A shared EventFilter is also created so the
        // connection can control which events are forwarded via
        // `events/subscribe` / `events/unsubscribe`.
        let (outbound_rx, event_filter) = if let Some(engine) = engine_for_events {
            let filter = Arc::new(Mutex::new(EventFilter::new()));
            let (tx, rx) = mpsc::channel::<Message>(DEFAULT_OUTBOUND_QUEUE_CAP);
            let events_rx = engine.subscribe();
            spawn_event_forwarder(events_rx, tx, Arc::clone(&filter), shutdown.clone());
            (Some(rx), Some(filter))
        } else {
            (None, None)
        };
        if let Err(e) = serve_loop::serve_loop_with_filter(
            &mut transport,
            router,
            None,
            outbound_rx,
            event_filter,
            shutdown,
        )
        .await
        {
            let message = e.to_string();
            tracing::warn!(
                name: "zhive.server.uds.connection_error",
                error_message = %message,
                "uds connection terminated with error"
            );
        }
    });
}

/// Guard that holds an exclusive startup lock for the duration of its
/// lifetime.
///
/// Created by [`acquire_startup_lock`]. The lock is released when this
/// value is dropped (i.e. when the underlying `File` is closed). The
/// lock prevents two concurrently-starting server processes from racing
/// through the socket-path setup at the same time.
#[cfg(unix)]
pub(crate) struct ServerStartupLock {
    _file: std::fs::File,
}

/// Acquires an exclusive startup lock at `path`, blocking until any
/// competing process releases it.
///
/// The lock is an advisory `flock(LOCK_EX)` applied to the file at
/// `path`. The call is run on a `spawn_blocking` worker thread so the
/// tokio runtime thread is not blocked. Returns a [`ServerStartupLock`]
/// that releases the lock on drop.
///
/// # Errors
///
/// Returns `io::Error` if the lock file cannot be opened or the
/// `flock` call fails at the OS level.
// `std::fs::File::lock()` was stabilized in Rust 1.89.  The workspace
// `rust-version` is now 1.89, so no compatibility override is needed here.
#[cfg(unix)]
pub(crate) async fn acquire_startup_lock(
    path: std::path::PathBuf,
) -> std::io::Result<ServerStartupLock> {
    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        file.lock()?;
        Ok(ServerStartupLock { _file: file })
    })
    .await
    .map_err(std::io::Error::other)?
}

/// RAII guard that removes the UDS socket file when dropped.
///
/// Created after a successful [`tokio::net::UnixListener::bind`] so
/// that the socket file is always cleaned up when the server exits,
/// even on panic.
#[cfg(unix)]
struct UdsFileGuard(std::path::PathBuf);

#[cfg(unix)]
impl Drop for UdsFileGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                name: "zhive.server.uds.socket_cleanup_failed",
                socket_path = %self.0.display(),
                error_message = %e,
                "failed to remove UDS socket on shutdown"
            );
        }
    }
}

/// Prepares the UDS socket path for binding by probing for a live server
/// and cleaning up stale sockets.
///
/// The probe sequence is:
///
/// 1. Attempt a connection to `path` via [`tokio::net::UnixStream::connect`].
/// 2. If the connection succeeds the path hosts a live server → return
///    [`std::io::ErrorKind::AddrInUse`].
/// 3. If the connection is refused (`ConnectionRefused`) the socket file
///    exists but nothing is listening → stale; remove and return `Ok`.
/// 4. If the path does not exist (`NotFound`) → return `Ok` immediately.
/// 5. Any other error: if the path no longer exists at this point return
///    `Ok`; otherwise propagate the error.
///
/// On a successful return the caller may safely call
/// [`tokio::net::UnixListener::bind`] at `path`.
///
/// The parent directory is `chmod 0700` before the probe so both the
/// probe and the subsequent bind happen in a directory that is
/// inaccessible to other users.
///
/// # Errors
///
/// Returns `io::Error` with:
/// * `AddrInUse` when a live server is detected at `path`.
/// * `AlreadyExists` when `path` exists but is not a socket.
/// * Propagated OS errors from directory permission, metadata, or
///   `remove_file` syscalls.
#[cfg(unix)]
async fn prepare_uds_path(path: &Path) -> std::io::Result<()> {
    use std::io::ErrorKind;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    // Ensure the parent directory exists and is private (0700).
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
        let dir_perms = std::fs::Permissions::from_mode(0o700);
        // Best-effort: non-fatal if we do not own the directory.
        let _ = std::fs::set_permissions(parent, dir_perms);
    }

    // Probe: try to connect to the existing socket path.
    match tokio::net::UnixStream::connect(path).await {
        Ok(_stream) => {
            // A connection succeeded → there is a live server listening.
            return Err(std::io::Error::new(
                ErrorKind::AddrInUse,
                format!("zhive server already running at {}", path.display()),
            ));
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // Socket file does not exist at all → clean state.
            return Ok(());
        }
        Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
            // File exists but no process is listening → stale socket;
            // fall through to the remove block below.
        }
        Err(e) => {
            // Some other OS error: check if the file still exists before
            // propagating so a TOCTOU removal does not cause a spurious
            // failure.
            match path.try_exists() {
                Ok(false) => return Ok(()),
                _ => return Err(e),
            }
        }
    }

    // At this point we have a stale socket. Verify it really is a socket
    // before removing it (guard against misconfigured mounts etc.).
    let meta = tokio::fs::symlink_metadata(path).await?;
    if !meta.file_type().is_socket() {
        return Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            format!("path exists but is not a socket: {}", path.display()),
        ));
    }

    // Remove the stale socket so the caller can bind fresh.
    tokio::fs::remove_file(path).await?;
    Ok(())
}

#[cfg(unix)]
async fn serve_uds_inner(
    socket_path: &Path,
    router: Arc<Router>,
    max_connections: usize,
    engine_for_events: Option<crate::engine::Engine>,
    shutdown: CancellationToken,
) -> Result<(), ServerError> {
    use std::os::unix::fs::PermissionsExt;

    if max_connections == 0 {
        return Err(ServerError::InvalidMaxConnections);
    }

    // Acquire the startup lock first so that two concurrently-starting
    // server processes serialise here rather than racing on the bind.
    let lock_path = path::startup_lock_path();
    let startup_lock = acquire_startup_lock(lock_path).await?;

    // Probe for an active server and clean up any stale socket file.
    // `prepare_uds_path` returns `AddrInUse` when a live server is found.
    prepare_uds_path(socket_path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            ServerError::UdsAlreadyRunning {
                path: socket_path.display().to_string(),
            }
        } else {
            ServerError::Io(e)
        }
    })?;

    let listener = tokio::net::UnixListener::bind(socket_path)?;

    // Restrict to the owning user (D-004): the listener file must be
    // `chmod 0600` so that other users cannot connect.
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(socket_path, perms)?;

    // Register a guard that unlinks the socket file on exit.
    // Must be placed after `bind` so the guard owns a file that was
    // created by this process.
    let _socket_guard = UdsFileGuard(socket_path.to_path_buf());

    // The startup lock has served its purpose: this process has
    // successfully bound the socket. Release it now so a second process
    // can start immediately (e.g., after the user manually stops us).
    drop(startup_lock);

    let limiter = Arc::new(Semaphore::new(max_connections));
    let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    let outcome = loop {
        let permit = {
            let limiter = Arc::clone(&limiter);
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break Ok(()),
                acquire = limiter.acquire_owned() => match acquire {
                    Ok(p) => p,
                    Err(_closed) => break Ok(()),
                },
            }
        };

        let accepted = tokio::select! {
            biased;
            () = shutdown.cancelled() => break Ok(()),
            res = listener.accept() => res,
        };

        match accepted {
            Ok((stream, _peer)) => {
                spawn_connection(
                    &mut tasks,
                    stream,
                    permit,
                    Arc::clone(&router),
                    shutdown.clone(),
                    engine_for_events.clone(),
                );
            }
            Err(e) if accept_is_transient(&e) => {
                // EMFILE / ENFILE / ECONNABORTED and friends: log and
                // keep accepting so a single misbehaving peer cannot
                // tear down the listener. Drop the permit (`_permit`
                // not moved into a task) so the next accept can reuse
                // it once the transient condition clears.
                drop(permit);
                let message = e.to_string();
                let kind = e.kind();
                tracing::warn!(
                    name: "zhive.server.uds.accept_transient_error",
                    error_type = ?kind,
                    error_message = %message,
                    "transient accept error; continuing"
                );
                // Yield to avoid a tight busy-loop when the condition
                // does not clear immediately.
                tokio::task::yield_now().await;
            }
            Err(e) => break Err(ServerError::Io(e)),
        }
    };

    // Drain in-flight connection tasks so the caller can rely on a
    // clean shutdown: every spawned `serve_loop` has the same
    // `shutdown` token and will exit promptly.
    while let Some(join_res) = tasks.join_next().await {
        if let Err(e) = join_res {
            let message = e.to_string();
            tracing::warn!(
                name: "zhive.server.uds.task_join_error",
                error_message = %message,
                "connection task did not join cleanly"
            );
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::Value;
    use zhive_proto::ErrorObject;

    struct Pong;

    #[async_trait]
    impl Handler for Pong {
        async fn handle(&self, _params: Option<Value>) -> Result<Value, ErrorObject> {
            Ok(serde_json::json!("pong"))
        }
    }

    /// Sends the initialize handshake over a raw [`Transport`] and
    /// asserts it succeeds. Returns when the server responds with an
    /// `InitializeResponse`.
    #[cfg(unix)]
    async fn raw_handshake(client: &mut UdsTransport) {
        use zhive_proto::{Id, Request};

        // Use JSON construction to avoid `#[non_exhaustive]` struct
        // literal restrictions on `InitializeRequest`.
        let params = serde_json::json!({
            "protocolVersion": 1,
            "clientInfo": {
                "name": "test-client",
                "version": "0.0.0",
            },
        });
        let req = Request::new(Id::Number(0), "initialize", Some(params));
        client.send(&Message::Request(req)).await.unwrap();
        let reply = client.next_message().await.unwrap().unwrap();
        match reply {
            Message::Response(resp) => match resp.outcome {
                zhive_proto::ResponseOutcome::Result(_) => {}
                zhive_proto::ResponseOutcome::Error(e) => {
                    panic!("initialize handshake failed: {e:?}")
                }
            },
            other => panic!("expected Response to initialize, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn end_to_end_uds_round_trip() {
        use zhive_proto::{Id, Request};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("e2e.sock");

        let mut router = Router::new();
        router.register("ping", Arc::new(Pong));
        let router = Arc::new(router);

        // Bind on the main task so the file exists before `connect`.
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let router_for_server = Arc::clone(&router);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut t = UdsTransport::new(stream);
            serve_loop(&mut t, router_for_server, CancellationToken::new())
                .await
                .unwrap();
        });

        let mut client = UdsTransport::connect(&socket).await.unwrap();
        // Must handshake before dispatching any other method.
        raw_handshake(&mut client).await;
        let req = Request::new(Id::Number(1), "ping", None);
        client.send(&Message::Request(req)).await.unwrap();
        let reply = client.next_message().await.unwrap().unwrap();
        drop(client);
        server.await.unwrap();

        match reply {
            Message::Response(resp) => match resp.outcome {
                zhive_proto::ResponseOutcome::Result(v) => {
                    assert_eq!(v, serde_json::json!("pong"));
                }
                zhive_proto::ResponseOutcome::Error(e) => {
                    panic!("expected result, got error {e:?}")
                }
            },
            other => panic!("expected response, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn accept_is_transient_classifies_known_errnos() {
        use std::io::{Error, ErrorKind};

        // EMFILE / ENFILE: per-process / system-wide fd exhaustion.
        let per_process_fd_limit =
            Error::from_raw_os_error(rustix::io::Errno::MFILE.raw_os_error());
        let system_wide_fd_limit =
            Error::from_raw_os_error(rustix::io::Errno::NFILE.raw_os_error());
        assert!(accept_is_transient(&per_process_fd_limit));
        assert!(accept_is_transient(&system_wide_fd_limit));

        // EINTR / ECONNABORTED: explicit transient kinds.
        let eintr = Error::new(ErrorKind::Interrupted, "interrupted");
        let aborted = Error::new(ErrorKind::ConnectionAborted, "aborted");
        assert!(accept_is_transient(&eintr));
        assert!(accept_is_transient(&aborted));

        // EPERM is fatal (not transient).
        let eperm = Error::new(ErrorKind::PermissionDenied, "no perm");
        assert!(!accept_is_transient(&eperm));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serve_uds_zero_max_connections_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("zero.sock");
        let router = Arc::new(Router::new());
        let err = serve_uds(&socket, router, 0, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ServerError::InvalidMaxConnections));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serve_uds_shutdown_returns_cleanly_without_clients() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("shutdown.sock");
        let router = Arc::new(Router::new());
        let token = CancellationToken::new();
        let cancel = token.clone();
        let socket_for_task = socket.clone();
        let handle = tokio::spawn(async move {
            serve_uds(&socket_for_task, router, DEFAULT_MAX_CONNECTIONS, cancel).await
        });
        // Give the listener a chance to bind before firing cancel.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        token.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("serve_uds must return after cancel")
            .expect("task join")
            .expect("server must exit Ok on cancel");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn serve_uds_round_trip_then_shutdown() {
        use zhive_proto::{Id, Request};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("rt.sock");
        let mut router = Router::new();
        router.register("ping", Arc::new(Pong));
        let router = Arc::new(router);
        let token = CancellationToken::new();

        let socket_for_task = socket.clone();
        let token_for_task = token.clone();
        let server = tokio::spawn(async move {
            serve_uds(
                &socket_for_task,
                router,
                DEFAULT_MAX_CONNECTIONS,
                token_for_task,
            )
            .await
        });

        // Poll-connect with a short retry to absorb the listener bind race.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut client = loop {
            match UdsTransport::connect(&socket).await {
                Ok(c) => break c,
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => panic!("connect failed: {e:?}"),
            }
        };

        // Must handshake before sending any other request.
        raw_handshake(&mut client).await;
        let req = Request::new(Id::Number(7), "ping", None);
        client.send(&Message::Request(req)).await.unwrap();
        let reply = client.next_message().await.unwrap().unwrap();
        drop(client);
        token.cancel();

        let server_res = tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("server must shut down")
            .expect("server join");
        server_res.expect("serve_uds clean exit");

        match reply {
            Message::Response(resp) => match resp.outcome {
                zhive_proto::ResponseOutcome::Result(v) => {
                    assert_eq!(v, serde_json::json!("pong"));
                }
                zhive_proto::ResponseOutcome::Error(e) => {
                    panic!("expected ok result, got error {e:?}")
                }
            },
            other => panic!("expected response, got {other:?}"),
        }
    }

    // ── flock / stale-socket tests ────────────────────────────────────

    /// A stale socket (bound, then listener dropped) must be replaced
    /// transparently so that `serve_uds` can start on the same path.
    #[cfg(unix)]
    #[tokio::test]
    async fn stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("stale.sock");

        // Bind, then drop the listener immediately — leaves a socket file
        // with no process listening behind it.
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        drop(listener);
        assert!(socket.exists(), "socket file must still exist after drop");

        // `serve_uds` should detect the stale socket, remove it, and bind fresh.
        let router = Arc::new(Router::new());
        let token = CancellationToken::new();
        let cancel = token.clone();
        let socket_for_task = socket.clone();
        let handle = tokio::spawn(async move {
            serve_uds(&socket_for_task, router, DEFAULT_MAX_CONNECTIONS, cancel).await
        });

        // Give the server time to start then shut it down cleanly.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        token.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("serve_uds must return after cancel")
            .expect("task join")
            .expect("stale socket must not prevent startup");
    }

    /// A second call to `serve_uds` on a path where a live server is
    /// already listening must return `ServerError::UdsAlreadyRunning`
    /// immediately.
    #[cfg(unix)]
    #[tokio::test]
    async fn active_server_returns_already_running() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("active.sock");
        let router = Arc::new(Router::new());
        let token = CancellationToken::new();

        let router2 = Arc::clone(&router);
        let socket2 = socket.clone();
        let cancel = token.clone();
        let server = tokio::spawn(async move {
            serve_uds(&socket2, router2, DEFAULT_MAX_CONNECTIONS, cancel).await
        });

        // Wait for the first server to bind.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "first server did not bind in time"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // Second attempt must immediately fail with UdsAlreadyRunning.
        let router3 = Arc::clone(&router);
        let socket3 = socket.clone();
        let err = serve_uds(
            &socket3,
            router3,
            DEFAULT_MAX_CONNECTIONS,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ServerError::UdsAlreadyRunning { .. }),
            "expected UdsAlreadyRunning, got {err:?}"
        );

        // Shut down the first server cleanly.
        token.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("first server must shut down")
            .expect("task join")
            .expect("clean exit");
    }

    /// Two concurrent `acquire_startup_lock` calls must serialise: the
    /// second one blocks until the first releases its lock.
    #[cfg(unix)]
    #[tokio::test]
    async fn startup_lock_serializes_concurrent_starts() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("zhive-startup.lock");

        // Acquire the first lock.
        let lock1 = acquire_startup_lock(lock_path.clone())
            .await
            .expect("first acquire must succeed");

        // Attempt a second acquire in a separate task; it should block.
        let path2 = lock_path.clone();
        let second_task = tokio::spawn(async move { acquire_startup_lock(path2).await });

        // Give the task a chance to make progress (it should not yet succeed).
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(
            !second_task.is_finished(),
            "second acquire must block while first lock is held"
        );

        // Release the first lock; the second task must now complete.
        drop(lock1);
        let _lock2 = tokio::time::timeout(std::time::Duration::from_secs(2), second_task)
            .await
            .expect("second acquire must complete after first lock drop")
            .expect("task join")
            .expect("second acquire must succeed");
    }

    /// `UdsFileGuard` must remove the socket file when dropped.
    #[cfg(unix)]
    #[test]
    fn uds_file_guard_removes_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("guard.sock");

        // Create a dummy file to simulate the socket.
        std::fs::write(&socket, b"").unwrap();
        assert!(socket.exists());

        let guard = UdsFileGuard(socket.clone());
        drop(guard);

        assert!(
            !socket.exists(),
            "UdsFileGuard must remove the file on drop"
        );
    }

    /// `prepare_uds_path` must reject a path that exists but is not a socket.
    #[cfg(unix)]
    #[tokio::test]
    async fn prepare_uds_path_non_socket_file_is_rejected() {
        use std::io::ErrorKind;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not_a_socket.txt");

        // Create a regular file at the path.
        std::fs::write(&path, b"hello").unwrap();

        let err = prepare_uds_path(&path)
            .await
            .expect_err("non-socket file must be rejected");
        assert_eq!(
            err.kind(),
            ErrorKind::AlreadyExists,
            "expected AlreadyExists, got {err:?}"
        );
    }
}

// Rust guideline compliant 2026-02-21

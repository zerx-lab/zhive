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
pub mod path;
pub mod reverse_rpc;
pub mod router;
pub mod transport;

#[doc(inline)]
pub use events::{engine_event_to_notification, spawn_event_forwarder};
#[doc(inline)]
pub use handlers::{ENGINE_ERROR_CODE, register_engine_handlers};
#[doc(inline)]
pub use reverse_rpc::{ResolveOutcome, ReverseRpcError, ReverseRpcResult, ReverseRpcTracker};
#[doc(inline)]
pub use router::{Handler, JsonRpcCode, Router, error_object};
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
use zhive_proto::{Message, Response};

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
}

/// Runs the server loop on `transport` until the peer closes the
/// stream or `shutdown` fires.
///
/// Every inbound request is dispatched to `router`. Notifications go
/// to the router too but no response is sent. Inbound [`Response`]s
/// are routed to `reverse_rpc` when supplied; a stray response (no
/// pending entry) is logged and discarded.
///
/// The shutdown token is checked between messages and against the
/// blocking read on `transport.next_message`. An in-flight dispatch is
/// allowed to complete before the loop returns so the matching
/// response is not lost mid-flight.
///
/// # Errors
///
/// Returns [`ServerError::Transport`] for unrecoverable read / write
/// failures.
pub async fn serve_loop<T>(
    transport: &mut T,
    router: Arc<Router>,
    shutdown: CancellationToken,
) -> Result<(), ServerError>
where
    T: Transport + ?Sized,
{
    serve_loop_with_reverse(transport, router, None, shutdown).await
}

/// Variant of [`serve_loop`] that also routes responses to a
/// [`ReverseRpcTracker`].
///
/// Use this entry point when the engine drives server-initiated
/// requests (typically `permission/request`); the tracker is shared
/// with the engine actor so a matching reply discharges the awaiting
/// `oneshot`.
///
/// # Errors
///
/// Same surface as [`serve_loop`].
pub async fn serve_loop_with_reverse<T>(
    transport: &mut T,
    router: Arc<Router>,
    reverse_rpc: Option<Arc<ReverseRpcTracker>>,
    shutdown: CancellationToken,
) -> Result<(), ServerError>
where
    T: Transport + ?Sized,
{
    serve_loop_with_outbound(transport, router, reverse_rpc, None, shutdown).await
}

/// Variant of [`serve_loop`] that drains an outbound message queue.
///
/// `outbound_rx` is `Some` when the caller has wired a side channel
/// for engine-driven traffic (events, reverse RPC) — every value it
/// receives is shipped through the transport's `send` half along with
/// inbound request responses.
///
/// When `outbound_rx` is `None` the function behaves exactly like
/// [`serve_loop_with_reverse`].
///
/// # Errors
///
/// Same surface as [`serve_loop`].
pub async fn serve_loop_with_outbound<T>(
    transport: &mut T,
    router: Arc<Router>,
    reverse_rpc: Option<Arc<ReverseRpcTracker>>,
    mut outbound_rx: Option<mpsc::Receiver<Message>>,
    shutdown: CancellationToken,
) -> Result<(), ServerError>
where
    T: Transport + ?Sized,
{
    loop {
        // The select! arms cover (in priority order):
        //   1. shutdown signal — leave the loop without losing data,
        //   2. outbound queue ready — drain server-pushed messages
        //      (events, reverse RPC) before reading more inbound,
        //   3. transport read — pull the next inbound message.
        //
        // The outbound arm is gated on `outbound_rx.is_some()` so the
        // `serve_loop` entry point that supplied `None` keeps its old
        // behaviour exactly.
        enum Branch {
            Inbound(Option<Message>),
            Outbound(Message),
            Shutdown,
            OutboundClosed,
        }
        let branch = tokio::select! {
            biased;
            () = shutdown.cancelled() => Branch::Shutdown,
            out = async {
                match outbound_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending::<Option<Message>>().await,
                }
            } => match out {
                Some(m) => Branch::Outbound(m),
                None => Branch::OutboundClosed,
            },
            res = transport.next_message() => Branch::Inbound(res?),
        };
        let msg = match branch {
            // Both Shutdown and graceful Inbound EOF close the loop;
            // the explicit fall-through keeps the intent obvious to a
            // future reader.
            Branch::Shutdown | Branch::Inbound(None) => return Ok(()),
            Branch::OutboundClosed => {
                // Outbound producer hung up: stop trying to read it but
                // keep serving inbound traffic.
                outbound_rx = None;
                continue;
            }
            Branch::Outbound(m) => {
                transport.send(&m).await?;
                continue;
            }
            Branch::Inbound(Some(m)) => m,
        };

        match msg {
            Message::Request(req) => {
                let id = req.id.clone();
                let outcome = router.dispatch(&req.method, req.params).await;
                let response = match outcome {
                    Ok(value) => Response::ok(id, value),
                    Err(err) => Response::err(id, err),
                };
                transport.send(&Message::Response(response)).await?;
            }
            Message::Notification(n) => {
                // Notifications never produce a response per JSON-RPC
                // 2.0 § 4.1, but a dispatch failure is useful diagnostic
                // signal — log it at debug level rather than discarding
                // it outright.
                let method = n.method.clone();
                if let Err(err) = router.dispatch(&n.method, n.params).await {
                    let code = err.code;
                    tracing::debug!(
                        name: "zhive.rpc.notification.dispatch_failed",
                        rpc_method = %method,
                        rpc_jsonrpc_error_code = code,
                        "notification dispatch returned error (no response sent)"
                    );
                }
            }
            Message::Response(resp) => {
                // No tracker wired up yet in Phase 1's default
                // `serve_loop`; the stray-response branches only fire
                // when a caller supplies a tracker via
                // [`serve_loop_with_reverse`].
                if let Some(tracker) = reverse_rpc.as_ref() {
                    let response_id = format!("{:?}", resp.id);
                    match tracker.resolve(resp) {
                        ResolveOutcome::Delivered => {}
                        ResolveOutcome::AwaiterDropped => {
                            tracing::debug!(
                                name: "zhive.rpc.response.awaiter_dropped",
                                response_id = %response_id,
                                "reverse-RPC awaiter was dropped before response arrived"
                            );
                        }
                        ResolveOutcome::NoMatch => {
                            tracing::warn!(
                                name: "zhive.rpc.response.no_match",
                                response_id = %response_id,
                                "response did not match any pending reverse-RPC id"
                            );
                        }
                    }
                }
            }
        }
    }
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
    tasks.spawn(async move {
        let _permit = permit; // released when task exits
        let mut transport = UdsTransport::new(stream);
        // When events forwarding is enabled, create a per-connection
        // outbound channel and spawn a forwarder that pumps engine
        // events into it.
        let outbound_rx = if let Some(engine) = engine_for_events {
            let (tx, rx) = mpsc::channel::<Message>(DEFAULT_OUTBOUND_QUEUE_CAP);
            let events_rx = engine.subscribe();
            spawn_event_forwarder(events_rx, tx, shutdown.clone());
            Some(rx)
        } else {
            None
        };
        if let Err(e) =
            serve_loop_with_outbound(&mut transport, router, None, outbound_rx, shutdown).await
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

    // Best-effort cleanup of a stale socket file from a previous run.
    // A NotFound is the normal case; any other failure is logged so a
    // permissions or fs error does not silently swallow the bind below.
    match tokio::fs::remove_file(socket_path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            let path_display = socket_path.display().to_string();
            let kind = e.kind();
            let message = e.to_string();
            tracing::warn!(
                name: "zhive.server.stale_socket.cleanup_failed",
                socket_path = %path_display,
                error_type = ?kind,
                error_message = %message,
                "stale UDS socket cleanup failed; bind may still succeed"
            );
        }
    }

    let listener = tokio::net::UnixListener::bind(socket_path)?;

    // Restrict to the owning user (D-004): the listener file must be
    // `chmod 0600` so that other users cannot connect.
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(socket_path, perms)?;

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

    #[tokio::test]
    async fn serve_loop_with_reverse_routes_response_to_tracker() {
        use zhive_proto::{Id, Response};

        // Channel-backed transport: serve_loop reads Messages from
        // `incoming` and writes them to `outgoing`. We push a Response
        // and verify the tracker resolves the matching awaiter.
        struct ChannelTransport {
            incoming: tokio::sync::mpsc::Receiver<Message>,
            outgoing: tokio::sync::mpsc::UnboundedSender<Message>,
        }
        #[async_trait]
        impl Transport for ChannelTransport {
            async fn next_message(&mut self) -> Result<Option<Message>, TransportError> {
                Ok(self.incoming.recv().await)
            }
            async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
                let _ = self.outgoing.send(msg.clone());
                Ok(())
            }
        }

        let (in_tx, in_rx) = tokio::sync::mpsc::channel::<Message>(4);
        let (out_tx, _out_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let mut transport = ChannelTransport {
            incoming: in_rx,
            outgoing: out_tx,
        };

        let router = Arc::new(Router::new());
        let tracker = Arc::new(ReverseRpcTracker::new());
        let (req, rx) = tracker.issue("permission/request", None);
        let id_for_response = req.id.clone();
        let token = CancellationToken::new();

        let tracker_for_loop = Arc::clone(&tracker);
        let cancel = token.clone();
        let loop_handle = tokio::spawn(async move {
            serve_loop_with_reverse(&mut transport, router, Some(tracker_for_loop), cancel)
                .await
                .unwrap();
        });

        // Feed the response that matches our issued request.
        in_tx
            .send(Message::Response(Response::ok(
                id_for_response,
                serde_json::json!({"outcome": "selected"}),
            )))
            .await
            .unwrap();

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
            .await
            .expect("tracker must resolve quickly")
            .expect("oneshot")
            .expect("ok value");
        assert_eq!(outcome, serde_json::json!({"outcome": "selected"}));

        // Sending an extra Response with an unknown id should be a
        // `NoMatch` (logged at warn but not crashing the loop).
        in_tx
            .send(Message::Response(Response::ok(
                Id::String("rev:999".into()),
                serde_json::Value::Null,
            )))
            .await
            .unwrap();

        token.cancel();
        drop(in_tx);
        loop_handle.await.unwrap();
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
}

// Rust guideline compliant 2026-02-21

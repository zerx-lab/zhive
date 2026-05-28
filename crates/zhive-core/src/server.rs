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
//! ## What lives where
//!
//! The router covers request / notification dispatch only. Engine-level
//! concerns (turn lifecycle, hook host, permission reducer, cancel
//! propagation) belong to [`super::engine`] and are wired into the
//! router via thin glue handlers as later Block B tasks land.

pub mod path;
pub mod router;
pub mod transport;

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
use zhive_proto::{Message, Response};

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
}

/// Runs the server loop on `transport` until the peer closes the
/// stream.
///
/// Every inbound request is dispatched to `router`. Notifications go
/// to the router too but no response is sent. Inbound responses are
/// ignored by Phase 1; reverse-RPC bookkeeping lands in B7.
///
/// # Errors
///
/// Returns [`ServerError::Transport`] for unrecoverable read / write
/// failures.
pub async fn serve_loop<T>(transport: &mut T, router: Arc<Router>) -> Result<(), ServerError>
where
    T: Transport + ?Sized,
{
    while let Some(msg) = transport.next_message().await? {
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
                // Notifications never produce a response; dispatch
                // errors are silently swallowed per JSON-RPC 2.0 § 4.1.
                let _ = router.dispatch(&n.method, n.params).await;
            }
            Message::Response(_) => {
                // Phase 1 has no reverse-RPC tracker yet; B7 attaches
                // the pending request map and resolves matching ids.
            }
        }
    }
    Ok(())
}

/// Owns the process stdin/stdout and runs [`serve_loop`] on them.
///
/// # Errors
///
/// Returns [`ServerError`] when the transport or the dispatch loop
/// fails.
pub async fn serve_stdio(router: Arc<Router>) -> Result<(), ServerError> {
    let mut transport = StdioTransport::new();
    serve_loop(&mut transport, router).await
}

/// Binds a UDS listener at `socket_path` and spawns a serve task per
/// inbound connection.
///
/// The function returns when the listener errors out; the spawned
/// connection tasks log their own errors.
///
/// # Errors
///
/// Returns [`ServerError::Io`] when the bind or accept syscalls fail.
#[cfg(unix)]
pub async fn serve_uds(socket_path: &Path, router: Arc<Router>) -> Result<(), ServerError> {
    use std::os::unix::fs::PermissionsExt;

    // Best-effort cleanup of a stale socket file from a previous run.
    let _ = tokio::fs::remove_file(socket_path).await;

    let listener = tokio::net::UnixListener::bind(socket_path)?;

    // Restrict to the owning user (D-004): the listener file must be
    // `chmod 0600` so that other users cannot connect.
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(socket_path, perms)?;

    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => return Err(e.into()),
        };
        let router = Arc::clone(&router);
        tokio::spawn(async move {
            let mut transport = UdsTransport::new(stream);
            if let Err(e) = serve_loop(&mut transport, router).await {
                tracing::warn!(error = %e, "uds connection terminated with error");
            }
        });
    }
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
            serve_loop(&mut t, router_for_server).await.unwrap();
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
}

// Rust guideline compliant 2026-02-21

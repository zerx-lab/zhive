//! Connection establishment and initialize / initialized handshake (D-007).
//!
//! Provides the [`Client::connect_uds`] and [`Client::connect_stdio`]
//! entry points that build a transport, complete the full handshake,
//! and return a ready [`Client`].  Low-level callers can use
//! [`Client::from_split`] (defined in `lib.rs`) when they need to
//! manage the handshake themselves (e.g. unit tests).

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use zhive_proto::initialize::{Capabilities, Implementation, InitializeResponse, ProtocolVersion};

use crate::Client;
use crate::Inner;
use crate::error::ClientError;
use crate::events::ClientEvent;
use crate::reverse::{HandlerSlot, PendingReverse};
use crate::{DEFAULT_NOTIFICATION_BUFFER, OUTBOUND_QUEUE_CAP, PendingRequests, transport};

use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use zhive_proto::{Message, Notification};

/// Metadata agreed upon during the initialize handshake (D-007).
///
/// Stored in each [`Client`] after a `connect_*` call completes the
/// handshake.  Clients created via [`Client::from_split`] carry
/// placeholder values.
#[derive(Debug, Clone)]
pub struct HandshakeMeta {
    /// Protocol version the server chose (≤ the version we requested).
    pub negotiated_version: ProtocolVersion,
    /// Capabilities the server reported in the `initialize` response.
    pub server_capabilities: Capabilities,
    /// Identity card the server included in the `initialize` response.
    pub server_info: Implementation,
}

/// Builds the placeholder [`HandshakeMeta`] used by clients created
/// via [`Client::from_split`] (no real handshake is performed).
pub(crate) fn placeholder_handshake_meta() -> HandshakeMeta {
    HandshakeMeta {
        negotiated_version: ProtocolVersion::V0,
        server_capabilities: Capabilities::default(),
        server_info: serde_json::from_value(serde_json::json!({
            "name": "unknown",
            "version": "unknown",
        }))
        .unwrap_or_else(|_| {
            serde_json::from_str(r#"{"name":"unknown","version":"unknown"}"#)
                .unwrap_or_else(|_| unreachable!("bare minimal JSON must parse"))
        }),
    }
}

/// Executes the `initialize` / `initialized` handshake over an
/// already-connected [`Client`].
///
/// # Errors
///
/// * [`ClientError::ProtocolVersionUnsupported`] for error code `-32001`.
/// * [`ClientError::InitializeFailed`] for any other server error or
///   response-decode failure.
/// * [`ClientError::Disconnected`] / [`ClientError::Io`] for
///   transport-level failures.
pub(crate) async fn perform_handshake(client: &Client) -> Result<HandshakeMeta, ClientError> {
    let params = serde_json::json!({
        "protocolVersion": ProtocolVersion::LATEST.0,
        "clientInfo": {
            "name": "zhive-client-native",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "clientCapabilities": {
            "cancellation": true,
        },
    });

    let raw = client.call("initialize", Some(params)).await.map_err(|e| {
        if let ClientError::Server(ref obj) = e {
            if obj.code != -32001 {
                return e;
            }
            let requested = ProtocolVersion::LATEST.0;
            let supported = obj.data.as_ref().and_then(|d| d["supported"].as_array());
            let min = supported
                .and_then(|arr| arr.first())
                .and_then(serde_json::Value::as_u64)
                .map_or(0u16, |v| u16::try_from(v).unwrap_or(0));
            let max = supported
                .and_then(|arr| arr.last())
                .and_then(serde_json::Value::as_u64)
                .map_or(0u16, |v| u16::try_from(v).unwrap_or(0));
            return ClientError::ProtocolVersionUnsupported {
                requested,
                min,
                max,
            };
        }
        e
    })?;

    let resp: InitializeResponse =
        serde_json::from_value(raw).map_err(|e| ClientError::InitializeFailed {
            reason: format!("could not decode InitializeResponse: {e}"),
        })?;

    client
        .notify("initialized", None)
        .await
        .map_err(|e| ClientError::InitializeFailed {
            reason: format!("could not send initialized notification: {e}"),
        })?;

    Ok(HandshakeMeta {
        negotiated_version: resp.protocol_version,
        server_capabilities: resp.server_capabilities,
        server_info: resp.server_info,
    })
}

impl Client {
    /// Internal: builds a [`Client`] with pre-computed handshake metadata.
    pub(crate) fn from_split_with_meta<R, W>(read: R, write: W, meta: HandshakeMeta) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending = Arc::new(PendingRequests::default());
        let shutdown = CancellationToken::new();
        let (outbound_tx, outbound_rx) = mpsc::channel::<Message>(OUTBOUND_QUEUE_CAP);
        let (events_tx, _) = broadcast::channel::<ClientEvent>(DEFAULT_NOTIFICATION_BUFFER);
        let (notifications_tx, _) = broadcast::channel::<Notification>(DEFAULT_NOTIFICATION_BUFFER);
        let handler_slot = Arc::new(HandlerSlot::default());
        let pending_reverse = Arc::new(PendingReverse::default());

        transport::spawn_reader(transport::ReaderArgs {
            pending: Arc::clone(&pending),
            read,
            shutdown: shutdown.clone(),
            outbound_tx: outbound_tx.clone(),
            events_tx: events_tx.clone(),
            notifications_tx: notifications_tx.clone(),
            handler_slot: Arc::clone(&handler_slot),
            pending_reverse: Arc::clone(&pending_reverse),
        });
        transport::spawn_writer(write, outbound_rx, shutdown.clone(), Arc::clone(&pending));

        Self {
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            pending,
            outbound_tx,
            events_tx,
            notifications_tx,
            handler_slot,
            pending_reverse,
            inner: Arc::new(Inner { shutdown }),
            handshake: Arc::new(meta),
        }
    }

    /// Swaps the handshake metadata, consuming `self` and returning a
    /// new [`Client`] with `meta` applied.
    pub(crate) fn replace_meta(self, meta: HandshakeMeta) -> Self {
        Self {
            handshake: Arc::new(meta),
            ..self
        }
    }

    /// Connects to a Unix-domain socket at `path` and performs the
    /// full initialize / initialized handshake (D-007) before
    /// returning.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Io`] — connect syscall failed.
    /// * [`ClientError::ProtocolVersionUnsupported`] — server rejected
    ///   the requested protocol version (-32001).
    /// * [`ClientError::InitializeFailed`] — server returned any other
    ///   error or the response could not be decoded.
    #[cfg(unix)]
    pub async fn connect_uds(path: impl AsRef<std::path::Path>) -> Result<Self, ClientError> {
        let stream = tokio::net::UnixStream::connect(path.as_ref()).await?;
        let (read, write) = stream.into_split();
        let client = Self::from_split(read, write);
        let meta = perform_handshake(&client).await?;
        Ok(client.replace_meta(meta))
    }

    /// Wraps the process's inherited stdio in a client and performs
    /// the full initialize / initialized handshake (D-007).
    ///
    /// # Errors
    ///
    /// Same as [`Self::connect_uds`].
    pub async fn connect_stdio() -> Result<Self, ClientError> {
        let client = Self::from_split(tokio::io::stdin(), tokio::io::stdout());
        let meta = perform_handshake(&client).await?;
        Ok(client.replace_meta(meta))
    }
}

#[cfg(test)]
mod handshake_tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::io::duplex;
    use zhive_proto::Response;
    use zhive_proto::framing;

    /// Sends a single framed message and flushes.
    async fn server_send(writer: &mut (impl tokio::io::AsyncWrite + Unpin), msg: &Message) {
        framing::write_message(writer, msg).await.unwrap();
        writer.flush().await.unwrap();
    }

    #[tokio::test]
    async fn handshake_protocol_version_unsupported_surfaces_typed_error() {
        let (client_io, server_io) = duplex(4096);
        let (server_read, server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);

        let server_task = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(server_read);
            let msg = framing::read_message(&mut reader).await.unwrap();
            let req = match msg {
                Message::Request(r) => r,
                other => panic!("stub server: expected Request, got {other:?}"),
            };
            let error = zhive_proto::ErrorObject {
                code: -32001,
                message: "ProtocolVersionUnsupported".into(),
                data: Some(serde_json::json!({
                    "supported": [0, 1],
                    "requested": i64::from(ProtocolVersion::LATEST.0),
                })),
            };
            let mut writer = server_write;
            server_send(
                &mut writer,
                &Message::Response(Response::err(req.id, error)),
            )
            .await;
        });

        let raw_client = Client::from_split(client_read, client_write);
        let result = perform_handshake(&raw_client).await;

        server_task.await.unwrap();

        match result {
            Err(ClientError::ProtocolVersionUnsupported {
                requested,
                min,
                max,
            }) => {
                assert_eq!(requested, ProtocolVersion::LATEST.0);
                assert_eq!(min, 0);
                assert_eq!(max, 1);
            }
            other => panic!("expected ProtocolVersionUnsupported, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handshake_server_not_initialized_surfaces_server_error() {
        let (client_io, server_io) = duplex(4096);
        let (server_read, server_write) = tokio::io::split(server_io);
        let (client_read, client_write) = tokio::io::split(client_io);

        let server_task = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(server_read);
            let msg = framing::read_message(&mut reader).await.unwrap();
            let req = match msg {
                Message::Request(r) => r,
                other => panic!("stub server: expected Request, got {other:?}"),
            };
            let error = zhive_proto::ErrorObject {
                code: -32002,
                message: "ServerNotInitialized".into(),
                data: None,
            };
            let mut writer = server_write;
            server_send(
                &mut writer,
                &Message::Response(Response::err(req.id, error)),
            )
            .await;
        });

        let raw_client = Client::from_split(client_read, client_write);
        let result = perform_handshake(&raw_client).await;

        server_task.await.unwrap();

        match result {
            Err(ClientError::Server(obj)) => {
                assert_eq!(obj.code, -32002);
            }
            other => panic!("expected Server(-32002), got {other:?}"),
        }
    }
}

// Rust guideline compliant 2026-02-21

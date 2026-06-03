//! Connection establishment and initialize / initialized handshake (D-007).
//!
//! Provides the [`Client::connect_uds`] and [`Client::connect_stdio`]
//! entry points that build a transport, complete the full handshake,
//! and return a ready [`Client`].  Low-level callers can use
//! [`Client::from_split`] (defined in `lib.rs`) when they need to
//! manage the handshake themselves (e.g. unit tests).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use zhive_proto::initialize::{Capabilities, Implementation, InitializeResponse, ProtocolVersion};
use zhive_proto::{Message, Notification};

use crate::Client;
use crate::Inner;
use crate::error::ClientError;
use crate::events::ClientEvent;
use crate::reverse::{HandlerSlot, PendingReverse};
use crate::{DEFAULT_NOTIFICATION_BUFFER, OUTBOUND_QUEUE_CAP, PendingRequests, transport};

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
/// already-connected [`Client`], using the settings from a
/// [`ClientBuilder`].
///
/// This is the canonical handshake implementation that both the
/// builder's `connect_*` methods and the backward-compatible
/// [`perform_handshake`] helper delegate to.
///
/// # Errors
///
/// * [`ClientError::ProtocolVersionUnsupported`] for error code `-32001`.
/// * [`ClientError::InitializeFailed`] — any other server error, a
///   response-decode failure, or a timeout on the `initialize` call.
/// * [`ClientError::Disconnected`] / [`ClientError::Io`] for
///   transport-level failures.
pub(crate) async fn perform_handshake_with_params(
    client: &Client,
    builder: &ClientBuilder,
) -> Result<HandshakeMeta, ClientError> {
    // Resolve the effective client name/version, falling back to the
    // crate-level defaults when the caller did not set `client_info`.
    let (client_name, client_version) = builder
        .client_info
        .as_ref()
        .map_or(("zhive-client-native", env!("CARGO_PKG_VERSION")), |i| {
            (i.name.as_str(), i.version.as_str())
        });
    let cancellation = builder.capabilities.cancellation;
    let params = serde_json::json!({
        "protocolVersion": builder.protocol_version.0,
        "clientInfo": {
            "name": client_name,
            "version": client_version,
        },
        "clientCapabilities": {
            "cancellation": cancellation,
        },
    });

    let requested = builder.protocol_version.0;

    let call_future = client.call("initialize", Some(params));
    let raw = tokio::time::timeout(builder.initialize_timeout, call_future)
        .await
        .map_err(|_elapsed| ClientError::InitializeFailed {
            reason: format!(
                "initialize timed out after {}ms",
                builder.initialize_timeout.as_millis()
            ),
        })?
        .map_err(|e| {
            if let ClientError::Server(ref obj) = e
                && obj.code == -32001
            {
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

/// Executes the `initialize` / `initialized` handshake using default
/// parameters (crate name/version, default capabilities).
///
/// Used by the unit tests in this module that construct raw clients.
///
/// # Errors
///
/// Same as [`perform_handshake_with_params`].
#[cfg(test)]
pub(crate) async fn perform_handshake(client: &Client) -> Result<HandshakeMeta, ClientError> {
    perform_handshake_with_params(client, &ClientBuilder::default()).await
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
        let worker_done = Arc::new(Notify::new());

        transport::spawn_reader(transport::ReaderArgs {
            pending: Arc::clone(&pending),
            read,
            shutdown: shutdown.clone(),
            outbound_tx: outbound_tx.clone(),
            events_tx: events_tx.clone(),
            notifications_tx: notifications_tx.clone(),
            handler_slot: Arc::clone(&handler_slot),
            pending_reverse: Arc::clone(&pending_reverse),
            worker_done: Arc::clone(&worker_done),
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
            inner: Arc::new(Inner {
                shutdown,
                worker_done,
            }),
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
    /// Delegates to [`ClientBuilder::default().connect_uds(path)`][ClientBuilder::connect_uds]
    /// so existing call sites continue to work without changes.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Io`] — connect syscall failed.
    /// * [`ClientError::ProtocolVersionUnsupported`] — server rejected
    ///   the requested protocol version (-32001).
    /// * [`ClientError::InitializeFailed`] — server returned any other
    ///   error or the response could not be decoded.
    #[cfg(unix)]
    pub async fn connect_uds(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        ClientBuilder::default().connect_uds(path).await
    }

    /// Wraps the process's inherited stdio in a client and performs
    /// the full initialize / initialized handshake (D-007).
    ///
    /// Delegates to [`ClientBuilder::default().connect_stdio()`][ClientBuilder::connect_stdio]
    /// so existing call sites continue to work without changes.
    ///
    /// # Errors
    ///
    /// Same as [`Self::connect_uds`].
    pub async fn connect_stdio() -> Result<Self, ClientError> {
        ClientBuilder::default().connect_stdio().await
    }
}

// ── ClientBuilder ────────────────────────────────────────────────────────────

/// Fluent builder for [`Client`] connections.
///
/// Use this when you need to customize the identity or capabilities
/// sent during the `initialize` handshake, or to pre-wire the
/// channel capacity and timeout.  Most callers can use the convenience
/// free functions [`Client::connect_uds`] / [`Client::connect_stdio`]
/// which delegate to a [`ClientBuilder::default()`] instance.
///
/// # Examples
///
/// ```no_run
/// # #[tokio::main]
/// # async fn main() -> Result<(), zhive_client_native::ClientError> {
/// use zhive_client_native::ClientBuilder;
/// use zhive_proto::initialize::Implementation;
///
/// // `Implementation` is #[non_exhaustive]; use serde to construct it.
/// let info: Implementation = serde_json::from_value(
///     serde_json::json!({"name": "my-app", "version": "1.0.0"})
/// ).unwrap();
/// let client = ClientBuilder::new()
///     .client_info(info)
///     .connect_uds("/tmp/zhive.sock")
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ClientBuilder {
    /// Optional client identity to send in the `initialize` request.
    ///
    /// When `None` the builder falls back to the crate name and version.
    pub client_info: Option<Implementation>,
    /// Client capabilities advertised to the server.
    pub capabilities: Capabilities,
    /// Protocol version to request during the handshake.
    pub protocol_version: ProtocolVersion,
    /// Capacity of the outbound message channel.
    pub channel_capacity: usize,
    /// Maximum time to wait for the `initialize` response.
    pub initialize_timeout: Duration,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            client_info: None,
            capabilities: Capabilities::default(),
            protocol_version: ProtocolVersion::LATEST,
            channel_capacity: crate::OUTBOUND_QUEUE_CAP,
            // Two seconds is ample for a local connection while still
            // surfacing genuine hangs quickly.
            initialize_timeout: Duration::from_secs(2),
        }
    }
}

impl ClientBuilder {
    /// Creates a new builder with default settings.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_client_native::ClientBuilder;
    /// let _builder = ClientBuilder::new();
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Overrides the client identity sent in the `initialize` request.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_client_native::ClientBuilder;
    /// use zhive_proto::initialize::Implementation;
    ///
    /// // `Implementation` is #[non_exhaustive]; use serde to construct it.
    /// let info: Implementation = serde_json::from_value(
    ///     serde_json::json!({"name": "my-app", "version": "1.0.0"})
    /// ).unwrap();
    /// let builder = ClientBuilder::new().client_info(info);
    /// assert_eq!(builder.client_info.unwrap().name, "my-app");
    /// ```
    #[must_use]
    pub fn client_info(mut self, info: Implementation) -> Self {
        self.client_info = Some(info);
        self
    }

    /// Overrides the capabilities advertised during the handshake.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_client_native::ClientBuilder;
    /// use zhive_proto::initialize::Capabilities;
    ///
    /// // `Capabilities` is #[non_exhaustive]; use serde to construct it.
    /// let caps: Capabilities = serde_json::from_value(
    ///     serde_json::json!({"cancellation": true})
    /// ).unwrap();
    /// let builder = ClientBuilder::new().capabilities(caps);
    /// assert!(builder.capabilities.cancellation);
    /// ```
    #[must_use]
    pub fn capabilities(mut self, caps: Capabilities) -> Self {
        self.capabilities = caps;
        self
    }

    /// Overrides the protocol version requested during the handshake.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_client_native::ClientBuilder;
    /// use zhive_proto::initialize::ProtocolVersion;
    ///
    /// let builder = ClientBuilder::new().protocol_version(ProtocolVersion::V0);
    /// assert_eq!(builder.protocol_version, ProtocolVersion::V0);
    /// ```
    #[must_use]
    pub fn protocol_version(mut self, version: ProtocolVersion) -> Self {
        self.protocol_version = version;
        self
    }

    /// Overrides the outbound channel capacity.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_client_native::ClientBuilder;
    ///
    /// let builder = ClientBuilder::new().channel_capacity(128);
    /// assert_eq!(builder.channel_capacity, 128);
    /// ```
    #[must_use]
    pub fn channel_capacity(mut self, cap: usize) -> Self {
        self.channel_capacity = cap;
        self
    }

    /// Overrides the maximum time to wait for the `initialize` response.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_client_native::ClientBuilder;
    /// use std::time::Duration;
    ///
    /// let builder = ClientBuilder::new().initialize_timeout(Duration::from_secs(5));
    /// assert_eq!(builder.initialize_timeout, Duration::from_secs(5));
    /// ```
    #[must_use]
    pub fn initialize_timeout(mut self, d: Duration) -> Self {
        self.initialize_timeout = d;
        self
    }

    /// Connects to the Unix-domain socket at `path` and performs the
    /// full `initialize` / `initialized` handshake (D-007).
    ///
    /// The handshake uses the builder's `client_info`, `capabilities`,
    /// and `protocol_version` instead of hard-coded defaults.
    ///
    /// # Errors
    ///
    /// * [`ClientError::Io`] — connect syscall failed.
    /// * [`ClientError::ProtocolVersionUnsupported`] — server rejected
    ///   the requested protocol version (-32001).
    /// * [`ClientError::InitializeFailed`] — server error or decode
    ///   failure, including a timeout on the `initialize` response.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), zhive_client_native::ClientError> {
    /// use zhive_client_native::ClientBuilder;
    ///
    /// let client = ClientBuilder::new()
    ///     .connect_uds("/tmp/zhive.sock")
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(unix)]
    pub async fn connect_uds(self, path: impl AsRef<Path>) -> Result<Client, ClientError> {
        let stream = tokio::net::UnixStream::connect(path.as_ref()).await?;
        let (read, write) = stream.into_split();
        let client = Client::from_split(read, write);
        let meta = perform_handshake_with_params(&client, &self).await?;
        Ok(client.replace_meta(meta))
    }

    /// Wraps the process's inherited stdio in a client and performs
    /// the full `initialize` / `initialized` handshake (D-007).
    ///
    /// # Errors
    ///
    /// Same as [`Self::connect_uds`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # #[tokio::main]
    /// # async fn main() -> Result<(), zhive_client_native::ClientError> {
    /// use zhive_client_native::ClientBuilder;
    ///
    /// let client = ClientBuilder::new().connect_stdio().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn connect_stdio(self) -> Result<Client, ClientError> {
        let client = Client::from_split(tokio::io::stdin(), tokio::io::stdout());
        let meta = perform_handshake_with_params(&client, &self).await?;
        Ok(client.replace_meta(meta))
    }

    /// Placeholder for the Phase 3 remote (WebSocket / TCP) connector.
    ///
    /// This method reserves the API surface for Phase 3 remote transport
    /// support without introducing any new dependencies.  It always
    /// returns [`ClientError::NotImplemented`] with `phase: 3`.
    ///
    /// # Errors
    ///
    /// Always returns [`ClientError::NotImplemented`] until Phase 3 lands.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[tokio::main]
    /// # async fn main() {
    /// use zhive_client_native::{ClientBuilder, ClientError};
    ///
    /// let result = ClientBuilder::new()
    ///     .connect_remote("ws://localhost:9000".to_string())
    ///     .await;
    /// assert!(matches!(result, Err(ClientError::NotImplemented { phase: 3, .. })));
    /// # }
    /// ```
    #[expect(
        clippy::unused_async,
        reason = "async is part of the public API contract so callers can \
                  uniformly await all connect_* methods; Phase 3 will add \
                  actual async I/O here"
    )]
    pub async fn connect_remote(self, _url: String) -> Result<Client, ClientError> {
        Err(ClientError::NotImplemented {
            feature: "remote/websocket",
            phase: 3,
        })
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

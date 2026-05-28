//! [`Transport`] trait plus stdio and Unix-domain-socket implementations.
//!
//! The wire format is [`zhive_proto::framing`] (LSP-style
//! `Content-Length:` framing). Both transports read and write
//! [`zhive_proto::Message`] values directly; higher-level RPC semantics
//! live in [`super::router`].

use std::path::Path;

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::{AsyncBufRead, BufReader, BufStream, Stdin, Stdout};
use zhive_proto::Message;
use zhive_proto::framing::{self, FramingError};

/// Failure modes shared by every transport.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum TransportError {
    /// Framing-level error (header malformed, body oversize, ...).
    #[error("framing error: {0}")]
    Framing(#[from] FramingError),

    /// Raw I/O failure (connect, accept, ...).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// One bidirectional carrier of [`Message`] values.
#[async_trait]
pub trait Transport: Send {
    /// Reads the next message from the peer.
    ///
    /// Returns `Ok(None)` when the peer closes the stream cleanly.
    ///
    /// # Errors
    ///
    /// See [`TransportError`] for the failure modes.
    async fn next_message(&mut self) -> Result<Option<Message>, TransportError>;

    /// Sends one message and flushes the underlying writer.
    ///
    /// # Errors
    ///
    /// See [`TransportError`] for the failure modes.
    async fn send(&mut self, msg: &Message) -> Result<(), TransportError>;
}

/// Transport that reads from process `stdin` and writes to `stdout`.
///
/// Used by `zhive serve` and by `zhive-bridge-stdio` (the latter copies
/// bytes through, so it does not own one of these directly).
#[derive(Debug)]
pub struct StdioTransport {
    reader: BufReader<Stdin>,
    writer: Stdout,
}

impl StdioTransport {
    /// Builds a transport on the inherited stdio streams.
    #[must_use]
    pub fn new() -> Self {
        Self {
            reader: BufReader::new(tokio::io::stdin()),
            writer: tokio::io::stdout(),
        }
    }
}

impl Default for StdioTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn next_message(&mut self) -> Result<Option<Message>, TransportError> {
        read_with_eof(&mut self.reader).await
    }

    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        framing::write_message(&mut self.writer, msg).await?;
        Ok(())
    }
}

/// Transport that wraps a single Unix-domain socket connection.
///
/// One [`UdsTransport`] is created per accepted connection in
/// [`super::serve_uds`]; the listener itself stays in the caller.
#[cfg(unix)]
#[derive(Debug)]
pub struct UdsTransport {
    inner: BufStream<tokio::net::UnixStream>,
}

#[cfg(unix)]
impl UdsTransport {
    /// Builds a transport on an already-accepted [`tokio::net::UnixStream`].
    #[must_use]
    pub fn new(stream: tokio::net::UnixStream) -> Self {
        Self {
            inner: BufStream::new(stream),
        }
    }

    /// Convenience: connects to `path` and wraps the resulting socket.
    ///
    /// # Errors
    ///
    /// Returns [`TransportError::Io`] when the connect syscall fails.
    pub async fn connect(path: &Path) -> Result<Self, TransportError> {
        let stream = tokio::net::UnixStream::connect(path).await?;
        Ok(Self::new(stream))
    }
}

#[cfg(unix)]
#[async_trait]
impl Transport for UdsTransport {
    async fn next_message(&mut self) -> Result<Option<Message>, TransportError> {
        read_with_eof(&mut self.inner).await
    }

    async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        framing::write_message(&mut self.inner, msg).await?;
        Ok(())
    }
}

async fn read_with_eof<R>(reader: &mut R) -> Result<Option<Message>, TransportError>
where
    R: AsyncBufRead + Unpin + Send,
{
    match framing::read_message(reader).await {
        Ok(msg) => Ok(Some(msg)),
        Err(FramingError::UnexpectedEof) => Ok(None),
        Err(FramingError::Io(e)) if matches!(e.kind(), std::io::ErrorKind::UnexpectedEof) => {
            Ok(None)
        }
        Err(other) => Err(other.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zhive_proto::{Id, Request};

    #[cfg(unix)]
    #[tokio::test]
    async fn uds_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        let path2 = path.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut t = UdsTransport::new(stream);
            let msg = t.next_message().await.unwrap().unwrap();
            t.send(&msg).await.unwrap();
        });

        let mut client = UdsTransport::connect(&path2).await.unwrap();
        let req = Request::new(Id::Number(1), "ping", None);
        client.send(&Message::Request(req.clone())).await.unwrap();
        let echo = client.next_message().await.unwrap().unwrap();
        match echo {
            Message::Request(r) => assert_eq!(r, req),
            other => panic!("expected echo request, got {other:?}"),
        }
        server.await.unwrap();
    }
}

// Rust guideline compliant 2026-02-21

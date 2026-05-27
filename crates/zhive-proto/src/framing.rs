//! LSP-style `Content-Length:` framing for JSON-RPC 2.0 over byte streams.
//!
//! The wire format is byte-for-byte compatible with the
//! [Language Server Protocol base protocol][lsp-base], which both ACP and
//! MCP also re-use. Each frame is:
//!
//! ```text
//! Content-Length: <N>\r\n
//! \r\n
//! <N bytes of UTF-8 JSON>
//! ```
//!
//! Optional `Content-Type:` and other headers are tolerated on read and
//! ignored. Reads are length-bound so a hostile peer cannot `DoS` the
//! process via a giant header or body; the limits are intentionally
//! generous (16 KiB headers, 16 MiB body) to avoid surprising legitimate
//! clients.
//!
//! [lsp-base]: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#baseProtocol

use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::Message;

/// Maximum cumulative header bytes accepted per frame.
///
/// 16 KiB is roughly 200x the typical LSP / ACP header set; values above
/// this are almost certainly a malformed or hostile peer.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Maximum body bytes accepted per frame.
///
/// 16 MiB caps single-message memory usage. Real ACP / MCP messages stay
/// well under 1 MiB; bigger payloads should stream via tool-side channels,
/// not the RPC frame.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Errors produced by [`read_message`] and [`write_message`].
#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    /// Underlying I/O failure.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    /// Peer closed the connection before a complete frame arrived.
    #[error("unexpected eof while reading frame")]
    UnexpectedEof,
    /// A header line was malformed or `Content-Length` was missing / invalid.
    #[error("invalid header: {0}")]
    InvalidHeader(String),
    /// Header section exceeded [`MAX_HEADER_BYTES`].
    #[error("header section exceeded {MAX_HEADER_BYTES} bytes")]
    OverlongHeader,
    /// Body length declared by `Content-Length` exceeded [`MAX_BODY_BYTES`].
    #[error("body length {0} exceeded {MAX_BODY_BYTES} byte limit")]
    OversizeBody(usize),
    /// Body bytes failed to deserialize as JSON-RPC 2.0.
    #[error("malformed json body: {0}")]
    Json(#[from] serde_json::Error),
}

/// Reads one length-delimited JSON-RPC message from `reader`.
///
/// Headers are case-insensitive on the key (per HTTP/RFC 7230) and only
/// `Content-Length` is required; other headers are ignored.
///
/// # Errors
/// Returns [`FramingError::UnexpectedEof`] if the stream closes mid-frame,
/// [`FramingError::InvalidHeader`] / [`FramingError::OverlongHeader`] for
/// malformed framing, [`FramingError::OversizeBody`] when the declared
/// body length exceeds [`MAX_BODY_BYTES`], or [`FramingError::Json`] for
/// payloads that are not valid JSON-RPC 2.0.
///
/// # Examples
/// ```
/// use tokio::io::BufReader;
/// # let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
/// # rt.block_on(async {
/// let frame = b"Content-Length: 40\r\n\r\n\
///               {\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"id\":1}";
/// let mut reader = BufReader::new(&frame[..]);
/// let msg = zhive_proto::framing::read_message(&mut reader).await.unwrap();
/// match msg {
///     zhive_proto::Message::Request(r) => assert_eq!(r.method, "ping"),
///     _ => panic!("expected request"),
/// }
/// # });
/// ```
pub async fn read_message<R>(reader: &mut R) -> Result<Message, FramingError>
where
    R: AsyncBufRead + Unpin,
{
    let mut content_length: Option<usize> = None;
    let mut header_bytes: usize = 0;
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(FramingError::UnexpectedEof);
        }
        header_bytes = header_bytes.saturating_add(n);
        if header_bytes > MAX_HEADER_BYTES {
            return Err(FramingError::OverlongHeader);
        }

        // Tolerate both CRLF and LF terminators for ergonomic testing.
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }

        let (name, value) = trimmed
            .split_once(':')
            .ok_or_else(|| FramingError::InvalidHeader(trimmed.to_string()))?;
        if name.eq_ignore_ascii_case("content-length") {
            let parsed: usize = value
                .trim()
                .parse()
                .map_err(|_unused| FramingError::InvalidHeader(trimmed.to_string()))?;
            content_length = Some(parsed);
        }
        // All other headers (including Content-Type) are ignored on read.
    }

    let len = content_length
        .ok_or_else(|| FramingError::InvalidHeader("missing Content-Length".to_string()))?;
    if len > MAX_BODY_BYTES {
        return Err(FramingError::OversizeBody(len));
    }

    let mut body = vec![0_u8; len];
    reader.read_exact(&mut body).await?;
    let msg = serde_json::from_slice::<Message>(&body)?;
    Ok(msg)
}

/// Writes one length-delimited JSON-RPC message to `writer` and flushes.
///
/// The frame is fully buffered before the single underlying write to keep
/// header and body atomic against concurrent peers.
///
/// # Errors
/// Returns [`FramingError::Json`] if the message cannot be serialized (this
/// indicates a programmer error, since [`Message`] is `Serialize`) and
/// [`FramingError::Io`] for transport failures.
///
/// # Examples
/// ```
/// use zhive_proto::{Id, Message, Request};
/// # let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
/// # rt.block_on(async {
/// let mut buf: Vec<u8> = Vec::new();
/// let req = Request::new(Id::Number(7), "ping", None);
/// zhive_proto::framing::write_message(&mut buf, &Message::Request(req))
///     .await
///     .unwrap();
/// assert!(std::str::from_utf8(&buf).unwrap().starts_with("Content-Length: "));
/// # });
/// ```
pub async fn write_message<W>(writer: &mut W, msg: &Message) -> Result<(), FramingError>
where
    W: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(msg)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut frame = Vec::with_capacity(header.len() + body.len());
    frame.extend_from_slice(header.as_bytes());
    frame.extend_from_slice(&body);
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Id, Notification, Request};
    use tokio::io::BufReader;

    #[tokio::test]
    async fn roundtrip_request() {
        let req = Request::new(Id::Number(42), "session/prompt", None);
        let mut buf = Vec::new();
        write_message(&mut buf, &Message::Request(req.clone()))
            .await
            .unwrap();

        let mut reader = BufReader::new(&buf[..]);
        let parsed = read_message(&mut reader).await.unwrap();
        match parsed {
            Message::Request(r) => assert_eq!(r, req),
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_missing_content_length() {
        let frame = b"X-Other: 1\r\n\r\n{}";
        let mut reader = BufReader::new(&frame[..]);
        let err = read_message(&mut reader).await.unwrap_err();
        assert!(matches!(err, FramingError::InvalidHeader(_)));
    }

    #[tokio::test]
    async fn rejects_oversize_body() {
        let frame = b"Content-Length: 99999999999\r\n\r\n";
        let mut reader = BufReader::new(&frame[..]);
        let err = read_message(&mut reader).await.unwrap_err();
        assert!(matches!(err, FramingError::OversizeBody(_)));
    }

    #[tokio::test]
    async fn parses_notification() {
        let n = Notification::new("session/cancel", None);
        let mut buf = Vec::new();
        write_message(&mut buf, &Message::Notification(n.clone()))
            .await
            .unwrap();

        let mut reader = BufReader::new(&buf[..]);
        let parsed = read_message(&mut reader).await.unwrap();
        match parsed {
            Message::Notification(got) => assert_eq!(got, n),
            other => panic!("expected notification, got {other:?}"),
        }
    }
}

// Rust guideline compliant 2026-02-21

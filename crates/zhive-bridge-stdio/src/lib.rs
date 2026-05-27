//! Transparent stdio <-> UDS forwarder for hosts that can only spawn child processes.
//!
//! Many editors / desktops (Claude Desktop, Cursor, Zed plugins) drive
//! "agents" by spawning a binary and exchanging JSON-RPC over its stdin /
//! stdout. zhive's primary transport is a Unix Domain Socket so that
//! multiple clients can attach to one engine; this bridge fills that gap.
//!
//! Per D-005 / D-010, the bridge is byte-pump only:
//!
//! ```text
//! host stdin  ──►  forwarder.up    ──►  UDS write half  ──►  engine
//! host stdout ◄──  forwarder.down  ◄──  UDS read half   ◄──  engine
//! ```
//!
//! It MUST NOT parse the JSON-RPC envelope. Touching the wire bytes here
//! would re-introduce the N×M schema-translation matrix that D-005
//! explicitly forbids. If schema-aware translation is ever needed, build
//! `zhive-bridge-mcp` or `zhive-bridge-acp` (Phase 2) instead.

#![forbid(unsafe_code)]

use std::path::Path;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, copy};
use tokio::net::UnixStream;

/// Errors produced by [`run`].
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// Could not connect to the engine socket.
    #[error("connect to {path:?} failed: {source}")]
    Connect {
        /// Socket path we tried to dial.
        path: std::path::PathBuf,
        /// Underlying connect error.
        #[source]
        source: std::io::Error,
    },
    /// One of the byte-copy half-duplexes failed.
    #[error("forwarding error: {0}")]
    Forward(#[from] std::io::Error),
}

/// Connects to the engine UDS at `socket_path` and bridges `stdin` / `stdout`.
///
/// `stdin` is anything implementing [`AsyncRead`] (in production:
/// [`tokio::io::stdin`]); `stdout` is anything implementing [`AsyncWrite`]
/// (in production: [`tokio::io::stdout`]). Returns as soon as either
/// direction finishes (host closed its stdin, or the engine closed the
/// socket).
///
/// # Errors
/// Returns [`BridgeError::Connect`] if the socket cannot be reached and
/// [`BridgeError::Forward`] if either copy half fails mid-stream.
///
/// # Examples
/// ```no_run
/// # let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
/// # rt.block_on(async {
/// let exit = zhive_bridge_stdio::run(
///     "/run/user/1000/zhive.sock",
///     tokio::io::stdin(),
///     tokio::io::stdout(),
/// )
/// .await;
/// std::process::exit(if exit.is_ok() { 0 } else { 1 });
/// # });
/// ```
pub async fn run<R, W>(
    socket_path: impl AsRef<Path>,
    mut stdin: R,
    mut stdout: W,
) -> Result<(), BridgeError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let path = socket_path.as_ref().to_path_buf();
    let stream = UnixStream::connect(&path)
        .await
        .map_err(|source| BridgeError::Connect { path, source })?;
    let (mut sock_read, mut sock_write) = stream.into_split();

    // Both halves run concurrently. Each half copies until its source EOFs
    // and then half-closes its destination so the other side sees EOF and
    // can drain cleanly. `try_join!` waits for BOTH halves to complete,
    // which is the only correct shape for a bidirectional byte pump --
    // selecting on either half would truncate in-flight responses.
    let upstream = async {
        copy(&mut stdin, &mut sock_write).await?;
        sock_write.shutdown().await
    };
    let downstream = async {
        copy(&mut sock_read, &mut stdout).await?;
        stdout.shutdown().await
    };

    tokio::try_join!(upstream, downstream)?;
    Ok(())
}

/// Reports this crate's package version.
#[must_use]
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    /// Spins up a server on a temp UDS that echoes everything back. The
    /// bridge must faithfully pump bytes both directions and exit cleanly
    /// once both halves see EOF.
    #[tokio::test]
    async fn forwards_both_directions() {
        let tmp = tempfile::TempDir::new().expect("tmp dir");
        let sock = tmp.path().join("zhive.sock");

        let listener = UnixListener::bind(&sock).expect("bind uds");
        let echo_task = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.expect("accept");
            let (mut r, mut w) = conn.split();
            tokio::io::copy(&mut r, &mut w).await.expect("echo copy");
            // EOF-propagate so the bridge's downstream half can finish.
            w.shutdown().await.expect("shutdown");
        });

        let input: &[u8] = b"Content-Length: 2\r\n\r\nok";
        let mut output: Vec<u8> = Vec::new();
        let cursor = std::io::Cursor::new(input);

        run(&sock, cursor, &mut output).await.expect("run");
        echo_task.await.expect("echo task join");

        assert_eq!(output, input);
    }
}

// Rust guideline compliant 2026-02-21

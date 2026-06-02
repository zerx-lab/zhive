//! Neutral configuration types consumed by [`crate::McpManager`].
//!
//! These types are intentionally decoupled from any front end (the CLI, a
//! config file format, etc.). A caller maps its own configuration — for
//! example a `[mcp.servers.<name>]` TOML table — into an [`McpServerConfig`]
//! before handing it to [`crate::McpManager::connect_all`]. Keeping the
//! manager's input neutral means `zhive-mcp` never depends on the CLI crate.
//!
//! Two transports are supported, mirroring the MCP transports `rmcp` exposes
//! on the client: a stdio child process ([`McpTransport::Stdio`]) and a
//! Streamable-HTTP endpoint ([`McpTransport::Http`]).

use std::collections::BTreeMap;
use std::time::Duration;

/// How long a single server is given to connect, handshake, and be discovered.
///
/// Large enough to absorb a slow `npx` cold start or a sluggish remote host,
/// small enough that a hung child or unreachable URL is skipped promptly.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a single tool call may run before it is abandoned.
///
/// Mirrors typical MCP tool latencies (file reads, shell-outs) with headroom
/// for slow servers; the per-call race in [`crate::McpTool`] enforces it.
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// One MCP server to connect to, identified by a caller-chosen `name`.
///
/// The `name` becomes the `mcp__<name>__<tool>` prefix on every tool the
/// server exposes, so it must be unique across the set of configured servers.
///
/// # Examples
///
/// ```
/// use zhive_mcp::{McpServerConfig, McpTransport};
///
/// let server = McpServerConfig {
///     name: "filesystem".to_owned(),
///     transport: McpTransport::Stdio {
///         command: "npx".to_owned(),
///         args: vec!["-y".to_owned(), "@modelcontextprotocol/server-filesystem".to_owned()],
///         env: Default::default(),
///         cwd: None,
///     },
/// };
/// assert_eq!(server.name, "filesystem");
/// ```
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Unique server name; becomes the `mcp__<name>__*` tool prefix.
    pub name: String,
    /// How to reach the server.
    pub transport: McpTransport,
}

/// Transport used to reach one MCP server.
///
/// `Stdio` spawns a child process and speaks newline-delimited JSON-RPC over
/// its stdin/stdout (the child's stderr is inherited so its logs surface to
/// the operator). `Http` connects to a Streamable-HTTP endpoint.
#[derive(Debug, Clone)]
pub enum McpTransport {
    /// Spawn a child process and talk JSON-RPC over stdio.
    Stdio {
        /// Executable to run (resolved against `PATH`).
        command: String,
        /// Arguments passed to the executable.
        args: Vec<String>,
        /// Extra environment variables for the child.
        env: BTreeMap<String, String>,
        /// Working directory for the child; inherits the parent's when `None`.
        cwd: Option<String>,
    },
    /// Connect to a Streamable-HTTP MCP endpoint.
    Http {
        /// Endpoint URL, e.g. `http://localhost:8000/mcp`.
        url: String,
        /// Custom headers sent with every request.
        headers: BTreeMap<String, String>,
        /// Bearer token, sent as `Authorization: Bearer <token>` when present.
        auth_token: Option<String>,
    },
}

/// Tunables shared across every server in a [`crate::McpManager::connect_all`].
///
/// Use [`McpConnectOptions::default`] for sensible production defaults, or the
/// `with_*` helpers to override a single field.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use zhive_mcp::McpConnectOptions;
///
/// let opts = McpConnectOptions::default().with_call_timeout(Duration::from_secs(30));
/// assert_eq!(opts.call_timeout, Duration::from_secs(30));
/// ```
#[derive(Debug, Clone, Copy)]
pub struct McpConnectOptions {
    /// Per-server budget for connect + handshake + discovery.
    pub connect_timeout: Duration,
    /// Per-call budget enforced inside every adapted tool.
    pub call_timeout: Duration,
}

impl Default for McpConnectOptions {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            call_timeout: DEFAULT_CALL_TIMEOUT,
        }
    }
}

impl McpConnectOptions {
    /// Returns options with the given per-server connect timeout.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zhive_mcp::McpConnectOptions;
    /// let opts = McpConnectOptions::default().with_connect_timeout(Duration::from_secs(5));
    /// assert_eq!(opts.connect_timeout, Duration::from_secs(5));
    /// ```
    #[must_use]
    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Returns options with the given per-call timeout.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    /// use zhive_mcp::McpConnectOptions;
    /// let opts = McpConnectOptions::default().with_call_timeout(Duration::from_secs(15));
    /// assert_eq!(opts.call_timeout, Duration::from_secs(15));
    /// ```
    #[must_use]
    pub fn with_call_timeout(mut self, timeout: Duration) -> Self {
        self.call_timeout = timeout;
        self
    }
}

// Rust guideline compliant 2026-02-21

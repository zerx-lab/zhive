//! Connects to MCP servers and exposes their tools, resources, and prompts.
//!
//! [`McpManager`] owns the live `rmcp` connections for the engine's lifetime.
//! [`McpManager::connect_all`] starts every configured server in parallel,
//! skips (with a warning) any that fail to start, and caches each surviving
//! server's discovered metadata. The adapted tools returned by
//! [`McpManager::tools`] hold cheap [`rmcp::service::Peer`] handles into those
//! connections; dropping the manager (or calling [`McpManager::shutdown`])
//! tears every connection down.
//!
//! # Resource and prompt access
//!
//! After discovery, callers can retrieve resource content and prompt messages
//! on demand via [`McpManager::read_resource`] and [`McpManager::get_prompt`].
//! Both methods look up the named server, issue a single synchronous RPC, and
//! return owned result types so no `rmcp` model types bleed into callers.

use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{ErrorCode, GetPromptRequestParams, ReadResourceRequestParams, ResourceContents};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::TokioChildProcess;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
};
use serde_json::Value;
use tokio::process::Command;
use zhive_core::tools::Tool;

use crate::config::{McpConnectOptions, McpServerConfig, McpTransport};
use crate::convert::{namespaced_name, schema_to_value};
use crate::error::McpError;
use crate::tool::McpTool;

/// Live, no-op client service handle to one MCP server.
///
/// `()` is `rmcp`'s built-in no-op [`rmcp::ClientHandler`]; a tool-consuming
/// client needs nothing more.
type Client = RunningService<RoleClient, ()>;

/// A resource advertised by a connected MCP server.
///
/// A neutral, owned snapshot taken at connect time so callers need not depend
/// on `rmcp` model types to enumerate what a server offers.
#[derive(Debug, Clone)]
pub struct DiscoveredResource {
    /// Server the resource belongs to (the `mcp__<server>` prefix's middle).
    pub server: String,
    /// Resource URI, e.g. `file:///path/to/file`.
    pub uri: String,
    /// Human-readable resource name.
    pub name: String,
    /// Optional resource description.
    pub description: Option<String>,
}

/// A prompt advertised by a connected MCP server.
#[derive(Debug, Clone)]
pub struct DiscoveredPrompt {
    /// Server the prompt belongs to.
    pub server: String,
    /// Prompt name.
    pub name: String,
    /// Optional prompt description.
    pub description: Option<String>,
}

/// The textual or binary content returned by a `resources/read` call.
///
/// Mirrors the MCP spec's `TextResourceContents` / `BlobResourceContents`
/// union without exposing `rmcp` model types to callers.
#[derive(Debug, Clone, PartialEq)]
pub enum ResourceContent {
    /// UTF-8 text content.
    Text {
        /// Resource URI echo-ed back by the server.
        uri: String,
        /// Optional MIME type declared by the server (e.g. `"text/plain"`).
        mime_type: Option<String>,
        /// The resource's text body.
        text: String,
    },
    /// Base-64-encoded binary content.
    Blob {
        /// Resource URI echo-ed back by the server.
        uri: String,
        /// Optional MIME type declared by the server.
        mime_type: Option<String>,
        /// Base-64-encoded blob body.
        blob: String,
    },
}

/// One message returned inside a `prompts/get` response.
///
/// Each message carries a role (`"user"` or `"assistant"`) and a JSON value
/// representing the content (text, image, or embedded resource) exactly as
/// the server sent it. Callers that only need the text portion can use
/// [`PromptMessage::text`].
#[derive(Debug, Clone, PartialEq)]
pub struct PromptMessage {
    /// The sender role (`"user"` or `"assistant"`).
    pub role: String,
    /// Full content blob serialized from the server's response.
    pub content: Value,
}

impl PromptMessage {
    /// Extracts the plain text from a `"text"` content block, if present.
    ///
    /// Returns `None` when the content is an image, embedded resource, or any
    /// non-text variant.
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.content.get("text").and_then(|v| v.as_str())
    }
}

/// One connected server plus the metadata discovered from it.
struct ConnectedServer {
    name: Arc<str>,
    client: Client,
    tools: Vec<Arc<dyn Tool>>,
    resources: Vec<DiscoveredResource>,
    prompts: Vec<DiscoveredPrompt>,
}

/// Holds live MCP connections and exposes their adapted tools.
///
/// Construct it once at boot with [`McpManager::connect_all`], register
/// [`McpManager::tools`] into the engine's tool registry, and keep the manager
/// alive for as long as those tools may be called. Call
/// [`McpManager::shutdown`] on teardown for a graceful close.
///
/// # Examples
///
/// ```no_run
/// # async fn run() {
/// use zhive_mcp::{McpManager, McpConnectOptions};
///
/// // An empty config connects to nothing and performs no I/O.
/// let manager = McpManager::connect_all(Vec::new(), McpConnectOptions::default()).await;
/// assert!(manager.tools().is_empty());
/// manager.shutdown().await;
/// # }
/// ```
#[derive(Default)]
pub struct McpManager {
    servers: Vec<ConnectedServer>,
}

impl std::fmt::Debug for McpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpManager")
            .field("server_count", &self.servers.len())
            .field(
                "servers",
                &self.servers.iter().map(|s| &s.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl McpManager {
    /// Connects to every configured server in parallel, skipping failures.
    ///
    /// Each server gets its own connect-handshake-discover future, bounded by
    /// `opts.connect_timeout`. A server that fails to start (bad command,
    /// unreachable URL, handshake error, timeout) is logged at `warn` and
    /// dropped; it never aborts the others. The returned manager holds only the
    /// connections that came up successfully.
    pub async fn connect_all(configs: Vec<McpServerConfig>, opts: McpConnectOptions) -> Self {
        let attempts = configs
            .into_iter()
            .map(|config| connect_one(config, opts))
            .collect::<Vec<_>>();

        let results = futures::future::join_all(attempts).await;

        let mut servers = Vec::new();
        for result in results {
            match result {
                Ok(server) => {
                    tracing::info!(
                        name: "mcp.server.connected",
                        server = %server.name,
                        tool_count = server.tools.len(),
                        resource_count = server.resources.len(),
                        prompt_count = server.prompts.len(),
                        "MCP server connected"
                    );
                    servers.push(server);
                }
                Err((name, err)) => {
                    tracing::warn!(
                        name: "mcp.server.connect_failed",
                        server = %name,
                        error = %err,
                        "MCP server failed to start; skipping"
                    );
                }
            }
        }
        Self { servers }
    }

    /// Returns every discovered tool as a ready-to-register trait object.
    ///
    /// Each tool's name is the engine-wide unique `mcp__<server>__<tool>`.
    #[must_use]
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.servers
            .iter()
            .flat_map(|s| s.tools.iter().map(Arc::clone))
            .collect()
    }

    /// Returns every resource discovered across all connected servers.
    #[must_use]
    pub fn resources(&self) -> Vec<DiscoveredResource> {
        self.servers
            .iter()
            .flat_map(|s| s.resources.iter().cloned())
            .collect()
    }

    /// Returns every prompt discovered across all connected servers.
    #[must_use]
    pub fn prompts(&self) -> Vec<DiscoveredPrompt> {
        self.servers
            .iter()
            .flat_map(|s| s.prompts.iter().cloned())
            .collect()
    }

    /// Reads a single resource from a named server via `resources/read`.
    ///
    /// Looks up the server whose name matches `server`, issues an rmcp
    /// `resources/read` RPC for the given `uri`, and returns the resource
    /// contents as a list of [`ResourceContent`] items (a single URI can
    /// contain multiple content blocks per the MCP spec).
    ///
    /// # Errors
    ///
    /// - [`McpError::UnknownServer`] when no connected server has the given name.
    /// - [`McpError::ReadResource`] when the rmcp call fails (transport error,
    ///   server-side error, or unknown URI).
    pub async fn read_resource(
        &self,
        server: impl AsRef<str>,
        uri: impl AsRef<str>,
    ) -> Result<Vec<ResourceContent>, McpError> {
        let server = server.as_ref();
        let uri = uri.as_ref();

        let connected = self
            .servers
            .iter()
            .find(|s| s.name.as_ref() == server)
            .ok_or_else(|| McpError::UnknownServer(server.to_owned()))?;

        let params = ReadResourceRequestParams::new(uri);
        let result = connected
            .client
            .peer()
            .read_resource(params)
            .await
            .map_err(|e| McpError::ReadResource {
                uri: uri.to_owned(),
                reason: e.to_string(),
            })?;

        let contents = result
            .contents
            .into_iter()
            .map(resource_contents_to_owned)
            .collect();

        Ok(contents)
    }

    /// Retrieves a named prompt from a server via `prompts/get`.
    ///
    /// Looks up the server whose name matches `server`, issues an rmcp
    /// `prompts/get` RPC for the given `name`, and returns the prompt
    /// messages as a list of [`PromptMessage`] items.
    ///
    /// `arguments` is an optional JSON object whose keys and values fill the
    /// prompt's declared template variables. Pass `None` when the prompt
    /// requires no arguments, or when the server ignores them.
    ///
    /// # Errors
    ///
    /// - [`McpError::UnknownServer`] when no connected server has the given name.
    /// - [`McpError::GetPrompt`] when the rmcp call fails (transport error,
    ///   server-side error, or unknown prompt name).
    pub async fn get_prompt(
        &self,
        server: impl AsRef<str>,
        name: impl AsRef<str>,
        arguments: Option<Value>,
    ) -> Result<Vec<PromptMessage>, McpError> {
        let server = server.as_ref();
        let name = name.as_ref();

        let connected = self
            .servers
            .iter()
            .find(|s| s.name.as_ref() == server)
            .ok_or_else(|| McpError::UnknownServer(server.to_owned()))?;

        let mut params = GetPromptRequestParams::new(name);
        // The rmcp SDK accepts a JsonObject (Map<String, Value>) for prompt
        // arguments. The MCP spec § 5.6 recommends string-valued entries, but
        // we pass the object as-is and let the server validate. Non-object
        // values are silently ignored: the prompt still runs without arguments.
        if let Some(Value::Object(map)) = arguments {
            params = params.with_arguments(map);
        }

        let result = connected
            .client
            .peer()
            .get_prompt(params)
            .await
            .map_err(|e| McpError::GetPrompt {
                name: name.to_owned(),
                reason: e.to_string(),
            })?;

        let messages = result
            .messages
            .iter()
            .map(prompt_message_to_owned)
            .collect();

        Ok(messages)
    }

    /// Gracefully closes every connection, consuming the manager.
    ///
    /// Sends each server an `rmcp` cancellation and awaits its task. Errors are
    /// logged rather than propagated, since shutdown is best-effort.
    pub async fn shutdown(self) {
        for server in self.servers {
            let name = Arc::clone(&server.name);
            match server.client.cancel().await {
                Ok(reason) => tracing::debug!(
                    name: "mcp.server.shutdown",
                    server = %name,
                    quit_reason = ?reason,
                    "MCP server connection closed"
                ),
                Err(err) => tracing::warn!(
                    name: "mcp.server.shutdown_error",
                    server = %name,
                    error = %err,
                    "MCP server connection failed to close cleanly"
                ),
            }
        }
    }

    /// Builds a manager from one already-running client, for in-process tests.
    ///
    /// Lets a test inject a loopback `rmcp` connection (e.g. over
    /// [`tokio::io::duplex`]) without spawning a process or opening a socket.
    #[cfg(test)]
    pub(crate) async fn from_running(
        name: impl Into<String>,
        client: Client,
        call_timeout: Duration,
    ) -> Self {
        let name: Arc<str> = Arc::from(name.into());
        match discover(&name, &client, call_timeout).await {
            Ok((tools, resources, prompts)) => Self {
                servers: vec![ConnectedServer {
                    name,
                    client,
                    tools,
                    resources,
                    prompts,
                }],
            },
            Err(_) => Self::default(),
        }
    }
}

/// Connects to and discovers one server, tagging failures with the server name.
async fn connect_one(
    config: McpServerConfig,
    opts: McpConnectOptions,
) -> Result<ConnectedServer, (String, McpError)> {
    let name = config.name.clone();
    let attempt = tokio::time::timeout(opts.connect_timeout, connect_and_discover(config, opts));
    match attempt.await {
        Ok(Ok(server)) => Ok(server),
        Ok(Err(err)) => Err((name, err)),
        Err(_elapsed) => Err((name, McpError::ConnectTimeout(opts.connect_timeout))),
    }
}

/// Builds the transport, serves the client, and discovers its catalog.
async fn connect_and_discover(
    config: McpServerConfig,
    opts: McpConnectOptions,
) -> Result<ConnectedServer, McpError> {
    let name: Arc<str> = Arc::from(config.name);
    let client = match config.transport {
        McpTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            let mut cmd = Command::new(&command);
            cmd.args(&args);
            for (key, value) in &env {
                cmd.env(key, value);
            }
            if let Some(dir) = &cwd {
                cmd.current_dir(dir);
            }
            let transport =
                TokioChildProcess::new(cmd).map_err(|e| McpError::Transport(e.to_string()))?;
            ().serve(transport)
                .await
                .map_err(|e| McpError::Initialize(e.to_string()))?
        }
        McpTransport::Http {
            url,
            headers,
            auth_token,
        } => {
            let http_config = build_http_config(&url, &headers, auth_token.as_deref())?;
            let transport = StreamableHttpClientTransport::from_config(http_config);
            ().serve(transport)
                .await
                .map_err(|e| McpError::Initialize(e.to_string()))?
        }
    };

    let (tools, resources, prompts) = discover(&name, &client, opts.call_timeout).await?;
    Ok(ConnectedServer {
        name,
        client,
        tools,
        resources,
        prompts,
    })
}

/// Builds an HTTP Streamable transport config with optional headers and token.
///
/// Returning the config (rather than the transport) keeps `reqwest`'s `Client`
/// type — which is not a direct dependency of this crate — out of the
/// signature; the caller turns the config into a transport via `from_config`.
fn build_http_config(
    url: &str,
    headers: &std::collections::BTreeMap<String, String>,
    auth_token: Option<&str>,
) -> Result<StreamableHttpClientTransportConfig, McpError> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    if let Some(token) = auth_token {
        config = config.auth_header(token);
    }
    if !headers.is_empty() {
        let mut custom = std::collections::HashMap::with_capacity(headers.len());
        for (key, value) in headers {
            let header_name = http::HeaderName::from_bytes(key.as_bytes())
                .map_err(|e| McpError::Transport(format!("invalid header name {key:?}: {e}")))?;
            let header_value = http::HeaderValue::from_str(value).map_err(|e| {
                McpError::Transport(format!("invalid header value for {key:?}: {e}"))
            })?;
            custom.insert(header_name, header_value);
        }
        config = config.custom_headers(custom);
    }
    Ok(config)
}

/// Returns `true` when a [`ServiceError`] represents JSON-RPC method-not-found (-32601).
///
/// A spec-conformant resources-only or prompts-only server returns this code
/// for any method it does not implement (e.g. `tools/list`). The code is
/// defined as [`rmcp::model::ErrorCode::METHOD_NOT_FOUND`].
pub(crate) fn is_method_not_found(err: &ServiceError) -> bool {
    matches!(
        err,
        ServiceError::McpError(e) if e.code == ErrorCode::METHOD_NOT_FOUND
    )
}

/// Lists a server's tools, resources, and prompts and adapts the tools.
///
/// Tool discovery is mandatory: a server with no tools yields an empty list,
/// but a method-not-found (-32601) error from `tools/list` is also accepted as
/// "zero tools" because spec-conformant resources/prompts-only servers are
/// allowed to omit the method. Any other error (transport, timeout, etc.) is
/// fatal and causes the whole server connection to be dropped.
///
/// Resource and prompt discovery are optional: any error (including
/// method-not-found) results in an empty list, but non-method-not-found errors
/// are logged at `warn` so that transport failures remain visible.
async fn discover(
    name: &Arc<str>,
    client: &Client,
    call_timeout: Duration,
) -> Result<
    (
        Vec<Arc<dyn Tool>>,
        Vec<DiscoveredResource>,
        Vec<DiscoveredPrompt>,
    ),
    McpError,
> {
    let peer = client.peer().clone();

    // Fix #1: treat method-not-found on tools/list as "zero tools"; keep other
    // errors fatal so a broken transport still drops the server.
    let remote_tools = match client.list_all_tools().await {
        Ok(tools) => tools,
        Err(ref e) if is_method_not_found(e) => {
            tracing::debug!(
                name: "mcp.discover.tools_not_supported",
                server = %name,
                "server does not implement tools/list; treating as zero tools"
            );
            Vec::new()
        }
        Err(e) => return Err(McpError::Discovery(e.to_string())),
    };
    let tools: Vec<Arc<dyn Tool>> = remote_tools
        .into_iter()
        .map(|t| {
            let remote_name = t.name.to_string();
            let namespaced = namespaced_name(name, &remote_name);
            let description = t.description.map(|d| d.to_string());
            let schema = schema_to_value(&t.input_schema);
            let tool: Arc<dyn Tool> = Arc::new(McpTool::new(
                namespaced,
                remote_name,
                description,
                schema,
                peer.clone(),
                call_timeout,
            ));
            tool
        })
        .collect();

    // Fix #2: downgrade to empty ONLY on method-not-found; log a warning for
    // any other error so that transport failures remain visible in the logs.
    let resources = match client.list_all_resources().await {
        Ok(list) => list
            .into_iter()
            .map(|r| DiscoveredResource {
                server: name.to_string(),
                uri: r.raw.uri.clone(),
                name: r.raw.name.clone(),
                description: r.raw.description.clone(),
            })
            .collect(),
        Err(ref e) if is_method_not_found(e) => Vec::new(),
        Err(e) => {
            tracing::warn!(
                name: "mcp.discover.resources_error",
                server = %name,
                error = %e,
                "resources/list failed (non-fatal); treating as zero resources"
            );
            Vec::new()
        }
    };

    let prompts = match client.list_all_prompts().await {
        Ok(list) => list
            .into_iter()
            .map(|p| DiscoveredPrompt {
                server: name.to_string(),
                name: p.name.clone(),
                description: p.description.clone(),
            })
            .collect(),
        Err(ref e) if is_method_not_found(e) => Vec::new(),
        Err(e) => {
            tracing::warn!(
                name: "mcp.discover.prompts_error",
                server = %name,
                error = %e,
                "prompts/list failed (non-fatal); treating as zero prompts"
            );
            Vec::new()
        }
    };

    Ok((tools, resources, prompts))
}

/// Converts one `rmcp` [`ResourceContents`] item to the crate-owned [`ResourceContent`].
fn resource_contents_to_owned(rc: ResourceContents) -> ResourceContent {
    match rc {
        ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } => ResourceContent::Text {
            uri,
            mime_type,
            text,
        },
        ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            ..
        } => ResourceContent::Blob {
            uri,
            mime_type,
            blob,
        },
    }
}

/// Converts one `rmcp` [`rmcp::model::PromptMessage`] item to the crate-owned [`PromptMessage`].
///
/// The content is serialized to a [`Value`] so callers never need to import
/// `rmcp::model::PromptMessageContent`.
fn prompt_message_to_owned(pm: &rmcp::model::PromptMessage) -> PromptMessage {
    let role = match pm.role {
        rmcp::model::PromptMessageRole::User => "user".to_owned(),
        rmcp::model::PromptMessageRole::Assistant => "assistant".to_owned(),
    };
    // Serialize the content to a JSON Value; fall back to a plain error
    // string rather than panicking if serialization fails (which is
    // theoretically impossible for well-formed model types).
    let content = serde_json::to_value(&pm.content)
        .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }));
    PromptMessage { role, content }
}

#[cfg(test)]
mod tests {
    use rmcp::model::{ErrorCode, ErrorData};
    use rmcp::service::ServiceError;

    use super::is_method_not_found;

    /// Regression test for finding #1/#2: the -32601 helper must match only the
    /// specific JSON-RPC code, not other errors.
    #[test]
    fn is_method_not_found_matches_exactly_32601() {
        // True for -32601 (METHOD_NOT_FOUND)
        let mnf_err = ErrorData::new(ErrorCode::METHOD_NOT_FOUND, "method not found", None);
        assert!(
            is_method_not_found(&ServiceError::McpError(mnf_err)),
            "-32601 must be detected as method-not-found"
        );
    }

    #[test]
    fn is_method_not_found_rejects_other_mcp_codes() {
        // False for a different MCP error code (INVALID_PARAMS = -32602)
        let other_err = ErrorData::new(ErrorCode::INVALID_PARAMS, "bad params", None);
        assert!(
            !is_method_not_found(&ServiceError::McpError(other_err)),
            "non-32601 MCP error must not be detected as method-not-found"
        );
    }

    #[test]
    fn is_method_not_found_rejects_transport_errors() {
        assert!(
            !is_method_not_found(&ServiceError::TransportClosed),
            "TransportClosed must not be detected as method-not-found"
        );
    }

    #[test]
    fn is_method_not_found_rejects_timeout() {
        assert!(
            !is_method_not_found(&ServiceError::Timeout {
                timeout: std::time::Duration::from_secs(5)
            }),
            "Timeout must not be detected as method-not-found"
        );
    }
}

// Rust guideline compliant 2026-02-21

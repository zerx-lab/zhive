//! Adapter exposing one MCP tool as a [`zhive_core::tools::Tool`].
//!
//! [`McpTool`] holds a cheap clonable [`rmcp::service::Peer`] handle into a live
//! server connection and forwards each `execute` call to that server's
//! `tools/call`. The adapter owns its namespaced name as an [`String`] and
//! returns a borrow of it from [`zhive_core::tools::Tool::name`], so no
//! `&'static str` leak is needed for runtime-discovered tools.

use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, ClientRequest, JsonObject, ServerResult};
use rmcp::service::{Peer, PeerRequestOptions, RoleClient, ServiceError};
use serde_json::Value;
use zhive_core::tools::{Tool, ToolContext, ToolError, ToolKind, ToolOutput};

use crate::convert::{call_result_to_output, error_result_to_tool_error, map_service_error};

/// A single MCP server tool adapted to the engine's [`Tool`] trait.
///
/// Constructed by [`crate::McpManager`] during discovery; not built directly by
/// callers. Each instance carries the unqualified `remote_name` the server
/// expects in `tools/call`, the engine-facing namespaced `name`, the input
/// schema, a clonable peer handle, and the per-call timeout.
pub struct McpTool {
    /// Engine-wide unique name `mcp__<server>__<tool>`; returned by `name()`.
    name: String,
    /// Unqualified tool name the server expects in `tools/call`.
    remote_name: String,
    /// Optional human-readable description advertised to the model.
    description: Option<String>,
    /// JSON-Schema object describing the tool's input arguments.
    input_schema: Value,
    /// Cheap clonable handle into the owning server's live connection.
    peer: Peer<RoleClient>,
    /// Per-call budget enforced via a timeout race in `execute`.
    call_timeout: Duration,
}

impl std::fmt::Debug for McpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpTool")
            .field("name", &self.name)
            .field("remote_name", &self.remote_name)
            .field("description", &self.description)
            .field("call_timeout", &self.call_timeout)
            .finish_non_exhaustive()
    }
}

impl McpTool {
    /// Builds an adapter for one discovered MCP tool.
    ///
    /// `name` is the namespaced engine identifier; `remote_name` is what the
    /// server expects in `tools/call`. The `peer` is a clone of the owning
    /// connection's [`Peer`] handle.
    pub(crate) fn new(
        name: String,
        remote_name: String,
        description: Option<String>,
        input_schema: Value,
        peer: Peer<RoleClient>,
        call_timeout: Duration,
    ) -> Self {
        Self {
            name,
            remote_name,
            description,
            input_schema,
            peer,
            call_timeout,
        }
    }

    /// Converts the engine's JSON arguments into MCP call parameters.
    ///
    /// MCP requires arguments to be a JSON object (or absent). A `null` or
    /// missing value maps to no arguments; any non-object value is a usage
    /// error surfaced as [`ToolError::Execution`].
    fn build_params(&self, args: Value) -> Result<CallToolRequestParams, ToolError> {
        let params = CallToolRequestParams::new(self.remote_name.clone());
        match args {
            Value::Object(map) => {
                let arguments: JsonObject = map;
                Ok(params.with_arguments(arguments))
            }
            Value::Null => Ok(params),
            other => Err(ToolError::Execution(format!(
                "MCP tool arguments must be a JSON object, got {other}"
            ))),
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> ToolKind {
        // MCP tools are opaque to the engine's permission model; classifying
        // them as `Other` avoids over-promising read/write semantics we cannot
        // verify from the server's advertised metadata alone.
        ToolKind::Other
    }

    fn description(&self) -> Option<String> {
        self.description.clone()
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let params = self.build_params(args)?;

        // Fix #3: issue the call via send_cancellable_request so the SDK owns the
        // timeout budget. When RequestHandle::await_response() times out it
        // automatically sends a notifications/cancelled to the server, preventing
        // orphaned work. On a ctx.cancel signal we also notify the server
        // explicitly via RequestHandle::cancel before returning ToolError::Cancelled.
        //
        // `CallToolRequest` (Request<_, _>) is #[non_exhaustive], so use its
        // provided `::new(params)` constructor. `PeerRequestOptions` is also
        // #[non_exhaustive], so mutate the public field after `no_options()`.
        let request = ClientRequest::CallToolRequest(
            // Request<M,P>::new requires M: Default, which CallToolRequestMethod satisfies.
            rmcp::model::CallToolRequest::new(params),
        );
        // The SDK's await_response() enforces this budget and sends
        // CancelledNotification on expiry; no separate tokio::time::timeout
        // wrapper is needed here.
        let mut options = PeerRequestOptions::no_options();
        options.timeout = Some(self.call_timeout);
        let handle = self
            .peer
            .send_cancellable_request(request, options)
            .await
            .map_err(|e| {
                tracing::warn!(
                    name: "mcp.tool.send_error",
                    tool = %self.name,
                    error = %e,
                    "MCP tool call failed to send"
                );
                map_service_error(&e)
            })?;

        // Save a clone of the peer and the request id before consuming the
        // handle in await_response, so the cancel branch can still send a
        // CancelledNotification to the server if the turn is cancelled first.
        let cancel_peer = handle.peer.clone();
        let cancel_id = handle.id.clone();

        // Race the awaited response against turn cancellation. biased so a
        // fired cancel wins over a simultaneously-ready response.
        //
        // await_response() takes ownership of the handle and the SDK handles
        // the timeout-triggered CancelledNotification internally when the
        // PeerRequestOptions::timeout elapses. We keep the cancel branch
        // separate using the cloned peer + id.
        let service_result: Result<ServerResult, ServiceError> = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => {
                tracing::debug!(
                    name: "mcp.tool.cancelled",
                    tool = %self.name,
                    "MCP tool call cancelled by turn; notifying server"
                );
                // Notify the MCP server to stop work; ignore send errors
                // because the connection may already be closing.
                //
                // CancelledNotification is Notification<_, _> which is
                // #[non_exhaustive], so use its Notification::new() constructor.
                // CancelledNotification impl From<CancelledNotification> for
                // ClientNotification, so .into() provides the wrapping.
                let cancelled_params = rmcp::model::CancelledNotificationParam {
                    request_id: cancel_id,
                    reason: Some("turn cancelled".to_owned()),
                };
                let notification: rmcp::model::CancelledNotification =
                    rmcp::model::CancelledNotification::new(cancelled_params);
                let _ = cancel_peer
                    .send_notification(notification.into())
                    .await;
                return Err(ToolError::Cancelled);
            }
            outcome = handle.await_response() => outcome,
        };

        let server_result = service_result.map_err(|e| {
            if matches!(e, ServiceError::Timeout { .. }) {
                tracing::warn!(
                    name: "mcp.tool.timeout",
                    tool = %self.name,
                    timeout_secs = self.call_timeout.as_secs(),
                    "MCP tool call timed out"
                );
                ToolError::Execution(format!(
                    "MCP tool '{}' timed out after {:?}",
                    self.name, self.call_timeout
                ))
            } else {
                tracing::warn!(
                    name: "mcp.tool.transport_error",
                    tool = %self.name,
                    error = %e,
                    "MCP tool call failed at the transport layer"
                );
                map_service_error(&e)
            }
        })?;

        let ServerResult::CallToolResult(result) = server_result else {
            return Err(ToolError::Execution(
                "MCP server returned unexpected response type for tools/call".to_owned(),
            ));
        };

        if result.is_error.unwrap_or(false) {
            return Err(error_result_to_tool_error(&result));
        }
        Ok(call_result_to_output(result))
    }
}

// Rust guideline compliant 2026-02-21

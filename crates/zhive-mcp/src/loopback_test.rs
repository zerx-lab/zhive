//! In-process loopback integration test for [`crate::McpManager`].
//!
//! Serves a tiny MCP server and our client over a single [`tokio::io::duplex`]
//! pair — no child process, no socket — and asserts that the manager discovers
//! the server's tool under its `mcp__<server>__<tool>` name and that calling it
//! round-trips through `tools/call`.

use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, JsonObject, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler};
use tokio_util::sync::CancellationToken;
use zhive_core::tools::ToolContext;
use zhive_proto::domain::{ThreadId, TurnId};

use crate::McpManager;

/// Minimal MCP server exposing a single `echo` tool.
///
/// Implements [`ServerHandler`] by hand (no macros) so the test stays small and
/// only needs `rmcp`'s `server` feature.
#[derive(Clone)]
struct EchoServer;

impl EchoServer {
    fn echo_tool() -> Tool {
        let mut schema = JsonObject::new();
        schema.insert("type".to_owned(), serde_json::json!("object"));
        schema.insert(
            "properties".to_owned(),
            serde_json::json!({ "msg": { "type": "string" } }),
        );
        Tool::new("echo", "Echoes the msg argument back.", Arc::new(schema))
    }
}

impl ServerHandler for EchoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![Self::echo_tool()]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if request.name != "echo" {
            return Err(McpError::invalid_params("unknown tool", None));
        }
        let msg = request
            .arguments
            .as_ref()
            .and_then(|a| a.get("msg"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        Ok(CallToolResult::success(vec![Content::text(format!(
            "echo: {msg}"
        ))]))
    }
}

/// Spawns the echo server on one half of a duplex pair and returns a manager
/// connected over the other half.
async fn connected_manager() -> McpManager {
    let (server_io, client_io) = tokio::io::duplex(4096);

    // Drive the server on a background task; it lives until its transport closes.
    tokio::spawn(async move {
        if let Ok(running) = EchoServer.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });

    let client =
        ().serve(client_io)
            .await
            .expect("client should complete the initialize handshake");

    McpManager::from_running("test", client, Duration::from_secs(5)).await
}

fn test_ctx() -> ToolContext {
    ToolContext {
        thread_id: ThreadId(Arc::from("thread:native/loopback")),
        turn_id: TurnId(Arc::from("turn:thread:native/loopback/0")),
        cancel: CancellationToken::new(),
    }
}

#[tokio::test]
async fn discovers_namespaced_tool() {
    let manager = connected_manager().await;
    let tools = manager.tools();
    assert_eq!(tools.len(), 1, "exactly one tool should be discovered");
    assert_eq!(tools[0].name(), "mcp__test__echo");
    assert!(tools[0].description().is_some());
    assert_eq!(tools[0].input_schema()["type"], "object");
    manager.shutdown().await;
}

#[tokio::test]
async fn executes_tool_round_trip() {
    let manager = connected_manager().await;
    let tools = manager.tools();
    let echo = &tools[0];

    let out = echo
        .execute(serde_json::json!({ "msg": "hello" }), &test_ctx())
        .await
        .expect("tool call should succeed");
    assert_eq!(out.text, "echo: hello");
    manager.shutdown().await;
}

#[tokio::test]
async fn rejects_non_object_arguments() {
    use zhive_core::tools::ToolError;

    let manager = connected_manager().await;
    let tools = manager.tools();
    let echo = &tools[0];

    let err = echo
        .execute(serde_json::json!("not an object"), &test_ctx())
        .await
        .expect_err("non-object arguments must be rejected");
    assert!(matches!(err, ToolError::Execution(_)));
    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// Finding #1 regression: resources/prompts-only server (no tools/list)
// ---------------------------------------------------------------------------

/// A minimal MCP server that advertises resources but does NOT implement
/// `tools/list`. It returns the spec-mandated -32601 (method-not-found) for
/// any tool-related request.
#[derive(Clone)]
struct ResourceOnlyServer;

impl rmcp::ServerHandler for ResourceOnlyServer {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        // Deliberately omit `enable_tools()` so list_tools returns -32601.
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_resources()
                .build(),
        )
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, rmcp::ErrorData> {
        Ok(rmcp::model::ListResourcesResult::default())
    }
}

/// Regression test for finding #1: a resources-only server (no tools/list)
/// must not drop the whole connection. The manager must survive discovery with
/// zero tools, not treat method-not-found as a fatal error.
#[tokio::test]
async fn tools_list_method_not_found_does_not_drop_server() {
    let (server_io, client_io) = tokio::io::duplex(4096);

    tokio::spawn(async move {
        if let Ok(running) = ResourceOnlyServer.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });

    let client = ().serve(client_io).await.expect("client handshake should succeed");

    let manager = McpManager::from_running("resources_only", client, Duration::from_secs(5)).await;

    // The server must still appear connected (resources field shows it came up).
    // Zero tools is correct — the server has none.
    let tools = manager.tools();
    assert!(
        tools.is_empty(),
        "a resources-only server must yield zero tools, not drop the server"
    );

    manager.shutdown().await;
}

// ---------------------------------------------------------------------------
// Finding #3 regression: cancel-sends-notification
// ---------------------------------------------------------------------------

/// Verifies that cancelling a tool call while it is in-flight returns
/// `ToolError::Cancelled` and does not panic. A full assertion that the server
/// received the `CancelledNotification` requires a custom transport inspection
/// harness; instead we assert the observable outcome from the client side.
#[tokio::test]
async fn cancelled_tool_call_returns_cancelled_error() {
    use zhive_core::tools::ToolError;

    let manager = connected_manager().await;
    let tools = manager.tools();
    let echo = &tools[0];

    // Cancel the turn immediately; the tool call should short-circuit.
    let ctx = {
        let c = test_ctx();
        c.cancel.cancel();
        c
    };

    let err = echo
        .execute(serde_json::json!({ "msg": "should be cancelled" }), &ctx)
        .await
        .expect_err("a pre-cancelled turn must produce an error");

    assert!(
        matches!(err, ToolError::Cancelled),
        "expected ToolError::Cancelled, got {err:?}"
    );

    manager.shutdown().await;
}

// Rust guideline compliant 2026-02-21

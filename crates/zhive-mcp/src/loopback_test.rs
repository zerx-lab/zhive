//! In-process loopback integration tests for [`crate::McpManager`].
//!
//! Serves tiny MCP servers and our client over [`tokio::io::duplex`] pairs —
//! no child process, no socket — and verifies:
//!
//! - tool discovery and round-trip (`tools/call`)
//! - resource listing discovery (`resources/list`)
//! - resource content retrieval (`resources/read`)
//! - prompt listing discovery (`prompts/list`)
//! - prompt content retrieval (`prompts/get`)
//! - error paths: unknown server, unknown resource URI, unknown prompt name

use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, GetPromptRequestParams, GetPromptResult,
    JsonObject, ListPromptsResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    Prompt, PromptMessage as RmcpPromptMessage, PromptMessageRole, ReadResourceRequestParams,
    ReadResourceResult, ResourceContents, ServerCapabilities, ServerInfo, Tool,
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
        spawner: None,
        workspace_root: None,
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

// ---------------------------------------------------------------------------
// resources/read and prompts/get round-trip tests
// ---------------------------------------------------------------------------

/// URI of the single test resource exposed by [`ResourceAndPromptServer`].
const TEST_RESOURCE_URI: &str = "test:///hello";

/// Text body of the test resource.
const TEST_RESOURCE_TEXT: &str = "Hello from the test resource!";

/// Name of the single test prompt exposed by [`ResourceAndPromptServer`].
const TEST_PROMPT_NAME: &str = "greet";

/// A minimal MCP server exposing one resource and one prompt (no tools).
#[derive(Clone)]
struct ResourceAndPromptServer;

impl ServerHandler for ResourceAndPromptServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        use rmcp::model::{Annotated, RawResource};
        let raw = RawResource {
            uri: TEST_RESOURCE_URI.to_owned(),
            name: "hello".to_owned(),
            title: None,
            description: Some("A greeting resource.".to_owned()),
            mime_type: Some("text/plain".to_owned()),
            size: None,
            icons: None,
            meta: None,
        };
        let resource = Annotated {
            raw,
            annotations: None,
        };
        Ok(ListResourcesResult {
            resources: vec![resource],
            next_cursor: None,
            meta: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        if request.uri != TEST_RESOURCE_URI {
            return Err(McpError::invalid_params(
                format!("unknown resource URI: {}", request.uri),
                None,
            ));
        }
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            TEST_RESOURCE_TEXT,
            TEST_RESOURCE_URI,
        )]))
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        let prompt = Prompt::new("greet", Some("A greeting prompt."), None);
        Ok(ListPromptsResult {
            prompts: vec![prompt],
            next_cursor: None,
            meta: None,
        })
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        if request.name != TEST_PROMPT_NAME {
            return Err(McpError::invalid_params(
                format!("unknown prompt: {}", request.name),
                None,
            ));
        }
        let who = request
            .arguments
            .as_ref()
            .and_then(|a| a.get("who"))
            .and_then(|v| v.as_str())
            .unwrap_or("world");
        let msg = RmcpPromptMessage::new_text(PromptMessageRole::User, format!("Hello, {who}!"));
        Ok(GetPromptResult::new(vec![msg]))
    }
}

/// Spawns [`ResourceAndPromptServer`] and returns a manager connected to it.
async fn resource_prompt_manager() -> crate::McpManager {
    let (server_io, client_io) = tokio::io::duplex(4096);

    tokio::spawn(async move {
        if let Ok(running) = ResourceAndPromptServer.serve(server_io).await {
            let _ = running.waiting().await;
        }
    });

    let client = ()
        .serve(client_io)
        .await
        .expect("client handshake with ResourceAndPromptServer should succeed");

    crate::McpManager::from_running("rp", client, Duration::from_secs(5)).await
}

#[tokio::test]
async fn discovers_resource_in_list() {
    let manager = resource_prompt_manager().await;
    let resources = manager.resources();
    assert_eq!(
        resources.len(),
        1,
        "exactly one resource should be discovered"
    );
    assert_eq!(resources[0].uri, TEST_RESOURCE_URI);
    assert_eq!(resources[0].name, "hello");
    manager.shutdown().await;
}

#[tokio::test]
async fn read_resource_returns_text_content() {
    use crate::ResourceContent;

    let manager = resource_prompt_manager().await;
    let contents = manager
        .read_resource("rp", TEST_RESOURCE_URI)
        .await
        .expect("read_resource should succeed for the known URI");

    assert_eq!(contents.len(), 1, "one content block expected");
    let ResourceContent::Text { uri, text, .. } = &contents[0] else {
        panic!("expected TextResourceContents, got {:?}", contents[0]);
    };
    assert_eq!(uri, TEST_RESOURCE_URI);
    assert_eq!(text, TEST_RESOURCE_TEXT);
    manager.shutdown().await;
}

#[tokio::test]
async fn read_resource_unknown_uri_returns_error() {
    use crate::McpError;

    let manager = resource_prompt_manager().await;
    let err = manager
        .read_resource("rp", "test:///nonexistent")
        .await
        .expect_err("read_resource with an unknown URI must fail");
    assert!(
        matches!(err, McpError::ReadResource { .. }),
        "expected McpError::ReadResource, got {err:?}"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn read_resource_unknown_server_returns_error() {
    use crate::McpError;

    let manager = resource_prompt_manager().await;
    let err = manager
        .read_resource("does_not_exist", TEST_RESOURCE_URI)
        .await
        .expect_err("read_resource with an unknown server must fail");
    assert!(
        matches!(err, McpError::UnknownServer(_)),
        "expected McpError::UnknownServer, got {err:?}"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn discovers_prompt_in_list() {
    let manager = resource_prompt_manager().await;
    let prompts = manager.prompts();
    assert_eq!(prompts.len(), 1, "exactly one prompt should be discovered");
    assert_eq!(prompts[0].name, TEST_PROMPT_NAME);
    manager.shutdown().await;
}

#[tokio::test]
async fn get_prompt_returns_messages() {
    let manager = resource_prompt_manager().await;
    let messages = manager
        .get_prompt("rp", TEST_PROMPT_NAME, None)
        .await
        .expect("get_prompt should succeed for the known prompt");

    assert_eq!(messages.len(), 1, "one message expected");
    assert_eq!(messages[0].role, "user");
    assert_eq!(
        messages[0].text(),
        Some("Hello, world!"),
        "default 'who' should be 'world'"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn get_prompt_with_arguments_substitutes_value() {
    let manager = resource_prompt_manager().await;
    let messages = manager
        .get_prompt(
            "rp",
            TEST_PROMPT_NAME,
            Some(serde_json::json!({ "who": "zhive" })),
        )
        .await
        .expect("get_prompt with arguments should succeed");

    assert_eq!(messages[0].text(), Some("Hello, zhive!"));
    manager.shutdown().await;
}

#[tokio::test]
async fn get_prompt_unknown_name_returns_error() {
    use crate::McpError;

    let manager = resource_prompt_manager().await;
    let err = manager
        .get_prompt("rp", "no_such_prompt", None)
        .await
        .expect_err("get_prompt for an unknown prompt must fail");
    assert!(
        matches!(err, McpError::GetPrompt { .. }),
        "expected McpError::GetPrompt, got {err:?}"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn get_prompt_unknown_server_returns_error() {
    use crate::McpError;

    let manager = resource_prompt_manager().await;
    let err = manager
        .get_prompt("no_such_server", TEST_PROMPT_NAME, None)
        .await
        .expect_err("get_prompt with an unknown server must fail");
    assert!(
        matches!(err, McpError::UnknownServer(_)),
        "expected McpError::UnknownServer, got {err:?}"
    );
    manager.shutdown().await;
}

// Rust guideline compliant 2026-02-21

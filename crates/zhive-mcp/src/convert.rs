//! Pure conversions between `rmcp` model types and `zhive-core` tool types.
//!
//! Everything here is side-effect free and synchronous so it can be unit
//! tested without a live MCP connection. The functions translate:
//!
//! - an MCP tool's JSON-Schema input ([`schema_to_value`]),
//! - a successful [`rmcp::model::CallToolResult`] into a
//!   [`zhive_core::tools::ToolOutput`] ([`call_result_to_output`]),
//! - a failed call or transport error into a
//!   [`zhive_core::tools::ToolError`] ([`flatten_text`], [`map_service_error`]),
//! - and a server/tool name pair into the engine-wide unique
//!   `mcp__<server>__<tool>` identifier ([`namespaced_name`]).

use std::sync::Arc;

use rmcp::model::{CallToolResult, Content, JsonObject};
use rmcp::service::ServiceError;
use serde_json::Value;
use zhive_core::tools::{ToolError, ToolOutput};

/// Prefix that marks every MCP-sourced tool name.
///
/// Kept in one place so the manager, the adapter, and tests agree. Changing it
/// would silently break any consumer that pattern-matches on the prefix.
const MCP_PREFIX: &str = "mcp";

/// Builds the engine-wide unique tool name `mcp__<server>__<tool>`.
///
/// The double-underscore delimiter matches the convention used by other tool
/// sources so the engine can route a call back to its origin server.
///
/// # Examples
///
/// ```
/// use zhive_mcp::convert::namespaced_name;
/// assert_eq!(namespaced_name("fs", "read_file"), "mcp__fs__read_file");
/// ```
#[must_use]
pub fn namespaced_name(server: &str, tool: &str) -> String {
    format!("{MCP_PREFIX}__{server}__{tool}")
}

/// Converts an MCP tool's `Arc<JsonObject>` input schema into a JSON value.
///
/// When a server advertises no schema (an empty object), falls back to the
/// permissive `{"type": "object"}` to match the engine's default tool schema
/// contract.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use rmcp::model::JsonObject;
/// use zhive_mcp::convert::schema_to_value;
///
/// let mut obj = JsonObject::new();
/// obj.insert("type".to_owned(), serde_json::json!("object"));
/// let schema = schema_to_value(&Arc::new(obj));
/// assert_eq!(schema["type"], "object");
/// ```
#[must_use]
pub fn schema_to_value(schema: &Arc<JsonObject>) -> Value {
    if schema.is_empty() {
        return serde_json::json!({ "type": "object" });
    }
    Value::Object((**schema).clone())
}

/// Joins the text of every text content block with newlines.
///
/// Non-text blocks (images, resources) are skipped here; they are preserved
/// separately in the structured value by [`call_result_to_output`].
///
/// # Examples
///
/// ```
/// use rmcp::model::Content;
/// use zhive_mcp::convert::flatten_text;
///
/// let blocks = vec![Content::text("a"), Content::text("b")];
/// assert_eq!(flatten_text(&blocks), "a\nb");
/// ```
#[must_use]
pub fn flatten_text(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|c| c.raw.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Converts a successful [`CallToolResult`] into a [`ToolOutput`].
///
/// The text field flattens every text block. The structured value prefers the
/// server's `structured_content`; otherwise the raw content array is serialized
/// so non-text blocks (images, embedded resources) are not lost.
///
/// # Examples
///
/// ```
/// use rmcp::model::{CallToolResult, Content};
/// use zhive_mcp::convert::call_result_to_output;
///
/// let res = CallToolResult::success(vec![Content::text("hello")]);
/// let out = call_result_to_output(res);
/// assert_eq!(out.text, "hello");
/// ```
#[must_use]
pub fn call_result_to_output(res: CallToolResult) -> ToolOutput {
    let text = flatten_text(&res.content);
    let value = res
        .structured_content
        .or_else(|| serde_json::to_value(&res.content).ok());
    match value {
        Some(v) => ToolOutput::with_value(text, v),
        None => ToolOutput::text(text),
    }
}

/// Maps a tool result whose `is_error` flag is set into a [`ToolError`].
///
/// Flattens the result's text blocks into the error message so the failure
/// reason travels back to the model.
///
/// # Examples
///
/// ```
/// use rmcp::model::{CallToolResult, Content};
/// use zhive_core::tools::ToolError;
/// use zhive_mcp::convert::error_result_to_tool_error;
///
/// let res = CallToolResult::error(vec![Content::text("boom")]);
/// match error_result_to_tool_error(&res) {
///     ToolError::Execution(msg) => assert!(msg.contains("boom")),
///     other => panic!("unexpected: {other:?}"),
/// }
/// ```
#[must_use]
pub fn error_result_to_tool_error(res: &CallToolResult) -> ToolError {
    let text = flatten_text(&res.content);
    let msg = if text.is_empty() {
        "MCP tool reported an error with no detail".to_owned()
    } else {
        text
    };
    ToolError::Execution(msg)
}

/// Maps a transport/protocol [`ServiceError`] into a [`ToolError`].
///
/// All `rmcp` service errors surface as [`ToolError::Execution`] carrying the
/// error's display text, which the engine feeds back to the model as a denial.
///
/// # Examples
///
/// ```
/// use rmcp::service::ServiceError;
/// use zhive_core::tools::ToolError;
/// use zhive_mcp::convert::map_service_error;
///
/// let err = map_service_error(&ServiceError::TransportClosed);
/// assert!(matches!(err, ToolError::Execution(_)));
/// ```
#[must_use]
pub fn map_service_error(err: &ServiceError) -> ToolError {
    ToolError::Execution(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaced_name_uses_double_underscore_delimiters() {
        assert_eq!(namespaced_name("fs", "read_file"), "mcp__fs__read_file");
        assert_eq!(namespaced_name("a", "b"), "mcp__a__b");
    }

    #[test]
    fn empty_schema_falls_back_to_object() {
        let schema = schema_to_value(&Arc::new(JsonObject::new()));
        assert_eq!(schema, serde_json::json!({ "type": "object" }));
    }

    #[test]
    fn non_empty_schema_is_preserved() {
        let mut obj = JsonObject::new();
        obj.insert("type".to_owned(), serde_json::json!("object"));
        obj.insert(
            "properties".to_owned(),
            serde_json::json!({ "path": { "type": "string" } }),
        );
        let schema = schema_to_value(&Arc::new(obj));
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["path"]["type"], "string");
    }

    #[test]
    fn flatten_text_joins_only_text_blocks_with_newlines() {
        let blocks = vec![
            Content::text("first"),
            Content::image("base64data".to_owned(), "image/png".to_owned()),
            Content::text("second"),
        ];
        assert_eq!(flatten_text(&blocks), "first\nsecond");
    }

    #[test]
    fn call_result_prefers_structured_content() {
        let mut res = CallToolResult::success(vec![Content::text("text-repr")]);
        res.structured_content = Some(serde_json::json!({ "rows": 3 }));
        let out = call_result_to_output(res);
        assert_eq!(out.text, "text-repr");
        assert_eq!(out.value, Some(serde_json::json!({ "rows": 3 })));
    }

    #[test]
    fn call_result_without_structured_serializes_content_array() {
        let res = CallToolResult::success(vec![Content::text("hi")]);
        let out = call_result_to_output(res);
        assert_eq!(out.text, "hi");
        // The fallback value is the serialized content array, not null.
        let value = out.value.expect("content array should serialize");
        assert!(value.is_array());
    }

    #[test]
    fn error_result_flattens_text_into_execution_error() {
        let res = CallToolResult::error(vec![Content::text("line1"), Content::text("line2")]);
        match error_result_to_tool_error(&res) {
            ToolError::Execution(msg) => assert_eq!(msg, "line1\nline2"),
            other => panic!("expected Execution, got {other:?}"),
        }
    }

    #[test]
    fn error_result_with_no_text_has_a_default_message() {
        let res = CallToolResult::error(vec![]);
        match error_result_to_tool_error(&res) {
            ToolError::Execution(msg) => assert!(!msg.is_empty()),
            other => panic!("expected Execution, got {other:?}"),
        }
    }

    #[test]
    fn service_error_maps_to_execution() {
        let err = map_service_error(&ServiceError::TransportClosed);
        assert!(matches!(err, ToolError::Execution(_)));
    }
}

// Rust guideline compliant 2026-02-21

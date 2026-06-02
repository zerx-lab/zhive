//! [`AgentTool`]: delegate a self-contained sub-task to a child agent.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::{Tool, ToolContext, ToolError, ToolKind, ToolOutput};

// ============================================================
// AgentTool
// ============================================================

/// Default child-agent name used when the model omits `name`.
///
/// Subagent definitions require a non-empty name; this placeholder keeps the
/// tool usable when the model only supplies a `prompt`.
const DEFAULT_AGENT_NAME: &str = "agent";

/// Default child-agent description used when the model omits `description`.
///
/// Carried into the [`zhive_proto::permission::SubagentDefinition`] for
/// observability; it has no effect on the child's behaviour.
const DEFAULT_AGENT_DESCRIPTION: &str = "Delegated sub-task";

/// Delegates a self-contained sub-task to a child agent and returns its result.
///
/// The model invokes this tool to spawn an independent subagent that runs in a
/// fresh context window, inheriting the parent's tool allowlist and permission
/// mode. The tool blocks until the child produces its final message, then
/// returns that text. Subagents cannot spawn further subagents (recursion is
/// forbidden), so the call fails with an error result when invoked from inside
/// an existing subagent.
///
/// Arguments (`prompt` is required; `name` and `description` are optional):
///
/// ```json
/// { "prompt": "Summarise the build errors", "name": "scout", "description": "read-only" }
/// ```
///
/// Spawning is only available inside a real engine turn. In test or non-engine
/// contexts (where [`ToolContext::spawner`] is `None`) the tool returns
/// [`ToolError::Execution`].
///
/// # Examples
///
/// ```
/// use zhive_core::tools::builtin::AgentTool;
/// use zhive_core::tools::{Tool, ToolKind};
/// assert_eq!(AgentTool.name(), "agent");
/// assert_eq!(AgentTool.kind(), ToolKind::Other);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct AgentTool;

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &'static str {
        "agent"
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn description(&self) -> Option<String> {
        Some(
            "Delegate a self-contained sub-task to a child agent. The child runs \
             independently with a fresh context window and only its final message \
             is returned. Use for focused, parallelisable work; the child cannot \
             spawn further subagents."
                .to_owned(),
        )
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Instruction for the subagent describing the sub-task."
                },
                "description": {
                    "type": "string",
                    "description": "Short human-readable label for the sub-task (optional)."
                },
                "name": {
                    "type": "string",
                    "description": "Name for the child agent (optional)."
                }
            },
            "required": ["prompt"],
            "additionalProperties": false
        })
    }

    /// Spawns the subagent and returns its final message.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError::Execution`] when `prompt` is missing or empty, when
    /// subagent spawning is unavailable in this context (no
    /// [`ToolContext::spawner`]), or when the spawn / child turn fails (the
    /// failure reason from [`crate::tools::SubagentSpawner::spawn_and_await`]).
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let prompt = args["prompt"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::Execution("`prompt` must be a non-empty string".to_owned()))?
            .to_owned();

        let name = args["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_AGENT_NAME)
            .to_owned();

        let description = args["description"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_AGENT_DESCRIPTION)
            .to_owned();

        let spawner = ctx.spawner.as_ref().ok_or_else(|| {
            ToolError::Execution("subagent spawning unavailable in this context".to_owned())
        })?;

        spawner
            .spawn_and_await(name, description, prompt)
            .await
            .map(ToolOutput::text)
            .map_err(ToolError::Execution)
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio_util::sync::CancellationToken;
    use zhive_proto::domain::{ThreadId, TurnId};

    use super::*;
    use crate::tools::SubagentSpawner;

    /// A spawner that records its inputs and returns a fixed result.
    #[derive(Debug)]
    struct MockSpawner {
        result: Result<String, String>,
    }

    #[async_trait]
    impl SubagentSpawner for MockSpawner {
        async fn spawn_and_await(
            &self,
            name: String,
            description: String,
            prompt: String,
        ) -> Result<String, String> {
            // Echo the inputs into the success payload so the test can assert
            // the tool forwarded its arguments faithfully.
            self.result.clone().map(|r| {
                format!("name={name};description={description};prompt={prompt};result={r}")
            })
        }
    }

    fn ctx_with_spawner(result: Result<String, String>) -> ToolContext {
        ToolContext {
            thread_id: ThreadId(Arc::from("thread:native/test")),
            turn_id: TurnId(Arc::from("turn:0")),
            cancel: CancellationToken::new(),
            spawner: Some(Arc::new(MockSpawner { result })),
        }
    }

    fn ctx_without_spawner() -> ToolContext {
        ToolContext {
            thread_id: ThreadId(Arc::from("thread:native/test")),
            turn_id: TurnId(Arc::from("turn:0")),
            cancel: CancellationToken::new(),
            spawner: None,
        }
    }

    #[tokio::test]
    async fn execute_forwards_args_and_returns_child_result() {
        let ctx = ctx_with_spawner(Ok("child done".to_owned()));
        let args = serde_json::json!({
            "prompt": "do the thing",
            "name": "scout",
            "description": "scoped"
        });
        let out = AgentTool.execute(args, &ctx).await.expect("must succeed");
        assert!(out.text.contains("prompt=do the thing"));
        assert!(out.text.contains("name=scout"));
        assert!(out.text.contains("description=scoped"));
        assert!(out.text.contains("result=child done"));
    }

    #[tokio::test]
    async fn execute_uses_defaults_for_optional_args() {
        let ctx = ctx_with_spawner(Ok("ok".to_owned()));
        let args = serde_json::json!({ "prompt": "minimal" });
        let out = AgentTool.execute(args, &ctx).await.expect("must succeed");
        assert!(out.text.contains(&format!("name={DEFAULT_AGENT_NAME}")));
        assert!(
            out.text
                .contains(&format!("description={DEFAULT_AGENT_DESCRIPTION}"))
        );
    }

    #[tokio::test]
    async fn execute_without_spawner_returns_execution_error() {
        let ctx = ctx_without_spawner();
        let args = serde_json::json!({ "prompt": "anything" });
        let err = AgentTool.execute(args, &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::Execution(ref m) if m.contains("unavailable")));
    }

    #[tokio::test]
    async fn execute_missing_prompt_returns_execution_error() {
        let ctx = ctx_with_spawner(Ok("unused".to_owned()));
        let err = AgentTool
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(ref m) if m.contains("prompt")));
    }

    #[tokio::test]
    async fn execute_propagates_spawn_error() {
        let ctx = ctx_with_spawner(Err("recursion forbidden".to_owned()));
        let err = AgentTool
            .execute(serde_json::json!({ "prompt": "x" }), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Execution(ref m) if m.contains("recursion forbidden")));
    }

    #[test]
    fn agent_tool_advertises_required_prompt() {
        let schema = AgentTool.input_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"][0], "prompt");
        assert!(AgentTool.description().is_some());
    }
}

// Rust guideline compliant 2026-02-21

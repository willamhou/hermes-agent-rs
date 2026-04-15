//! Delegation tool: spawns a child agent to handle a focused subtask.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use hermes_core::{
    error::{HermesError, Result},
    message::ToolResult,
    tool::{ApprovalDecision, ApprovalRequest, Tool, ToolContext, ToolSchema},
};
use hermes_memory::MemoryManager;
use hermes_tools::registry::ToolRegistry;
use serde_json::json;
use tokio::sync::mpsc;

use crate::{
    compressor::CompressionConfig,
    loop_runner::{Agent, AgentConfig},
};

/// Maximum delegation depth. Parent = 0, child = 1.
const MAX_DELEGATION_DEPTH: u32 = 1;

/// Default iteration budget for child agents.
const CHILD_MAX_ITERATIONS: u32 = 50;

/// Tool that spawns a focused child agent to complete a subtask.
pub struct DelegationTool {
    memory_dir: PathBuf,
}

impl DelegationTool {
    pub fn new(memory_dir: PathBuf) -> Self {
        Self { memory_dir }
    }
}

#[async_trait]
impl Tool for DelegationTool {
    fn name(&self) -> &str {
        "delegate_task"
    }

    fn toolset(&self) -> &str {
        "delegation"
    }

    fn is_exclusive(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "delegate_task".into(),
            description: "Spawn a focused subagent for a specific task. The child gets its own \
                          context and budget."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "Clear, specific task description"
                    },
                    "context": {
                        "type": "string",
                        "description": "Background info (files, errors, constraints)"
                    }
                },
                "required": ["goal"]
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        // 1. Depth limit
        if ctx.delegation_depth >= MAX_DELEGATION_DEPTH {
            return Ok(ToolResult::error(
                "delegation depth limit reached (max 1 level)",
            ));
        }

        // 2. Parse args
        let goal = args
            .get("goal")
            .and_then(|v| v.as_str())
            .ok_or_else(|| HermesError::Tool {
                name: "delegate_task".into(),
                message: "goal is required".into(),
            })?;

        let context = args.get("context").and_then(|v| v.as_str()).unwrap_or("");

        // 3. Get provider
        let provider = ctx
            .aux_provider
            .as_ref()
            .ok_or_else(|| HermesError::Tool {
                name: "delegate_task".into(),
                message: "no provider available for delegation".into(),
            })?
            .clone();

        // 4. Build child system prompt
        let context_section = if context.is_empty() {
            String::new()
        } else {
            format!("## Context\n{context}\n\n")
        };
        let system_prompt = format!(
            "You are a focused AI assistant working on a specific task.\n\n\
             ## Task\n{goal}\n\n\
             {context_section}\
             ## Working Directory\n{working_dir}\n\n\
             Complete the task efficiently. Be concise in your final response.",
            working_dir = ctx.working_dir.display(),
        );

        // 5. Create child memory (isolated)
        let child_memory =
            MemoryManager::new(self.memory_dir.clone(), None).map_err(|e| HermesError::Tool {
                name: "delegate_task".into(),
                message: format!("failed to create child memory: {e}"),
            })?;

        // 6. Create child approval channel (auto-allow)
        let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalRequest>(8);
        // Auto-approve task terminates when approval_tx is dropped (when child Agent is dropped).
        // No explicit cancellation needed for depth-1 delegation.
        tokio::spawn(async move {
            while let Some(req) = approval_rx.recv().await {
                let _ = req.response_tx.send(ApprovalDecision::Allow);
            }
        });

        // 7. Build child registry from inventory (same compile-time set)
        // Child registry uses from_inventory() which excludes DelegationTool
        // (registered manually in tooling.rs, not via inventory::submit!).
        // This prevents recursive delegation. The depth check is a second guard.
        let child_registry = Arc::new(ToolRegistry::from_inventory());

        // 8. Create child agent
        let child_session_id = uuid::Uuid::new_v4().to_string();
        let mut child = Agent::new(AgentConfig {
            provider,
            registry: child_registry,
            max_iterations: CHILD_MAX_ITERATIONS,
            system_prompt,
            session_id: child_session_id,
            working_dir: ctx.working_dir.clone(),
            approval_tx,
            tool_config: Arc::clone(&ctx.tool_config),
            memory: child_memory,
            skills: None,
            compression: CompressionConfig::default(),
            delegation_depth: ctx.delegation_depth + 1,
            clarify_tx: None,
        });

        // 9. Run child conversation
        let start = Instant::now();
        // Child streaming output intentionally discarded — parent only receives the final summary.
        // The send() calls inside the child's tool execution silently fail, which is correct behavior.
        let (delta_tx, _delta_rx) = mpsc::channel(64);
        let mut child_history = Vec::new();

        let result = child
            .run_conversation(goal, &mut child_history, delta_tx)
            .await;

        let duration_ms = start.elapsed().as_millis() as u64;
        let iterations_used = CHILD_MAX_ITERATIONS - child.remaining_budget();

        // 10. Format result
        match result {
            Ok(summary) => Ok(ToolResult::ok(
                json!({
                    "status": "completed",
                    "summary": summary,
                    "iterations_used": iterations_used,
                    "duration_ms": duration_ms,
                })
                .to_string(),
            )),
            Err(e) => Ok(ToolResult::ok(
                json!({
                    "status": "failed",
                    "error": e.to_string(),
                    "iterations_used": iterations_used,
                    "duration_ms": duration_ms,
                })
                .to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use hermes_core::{
        error::Result as HermesResult,
        provider::{ChatRequest, ChatResponse, FinishReason, ModelInfo, ModelPricing, TokenUsage},
        stream::StreamDelta,
        tool::ToolConfig,
    };

    use super::*;

    // ── MockProvider ──────────────────────────────────────────────────────────

    struct MockProvider {
        responses: Mutex<Vec<ChatResponse>>,
    }

    impl MockProvider {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    fn static_model_info() -> &'static ModelInfo {
        static INFO: std::sync::OnceLock<ModelInfo> = std::sync::OnceLock::new();
        INFO.get_or_init(|| ModelInfo {
            id: "mock".to_string(),
            provider: "mock".to_string(),
            max_context: 128_000,
            max_output: 4096,
            supports_tools: true,
            supports_vision: false,
            supports_reasoning: false,
            supports_caching: false,
            pricing: ModelPricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cache_read_per_mtok: 0.0,
                cache_create_per_mtok: 0.0,
            },
        })
    }

    #[async_trait]
    impl hermes_core::provider::Provider for MockProvider {
        async fn chat(
            &self,
            _request: &ChatRequest<'_>,
            _delta_tx: Option<&mpsc::Sender<StreamDelta>>,
        ) -> HermesResult<ChatResponse> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Ok(ChatResponse {
                    content: "no more responses".to_string(),
                    tool_calls: vec![],
                    reasoning: None,
                    finish_reason: FinishReason::Stop,
                    usage: TokenUsage::default(),
                    cache_meta: None,
                });
            }
            Ok(responses.remove(0))
        }

        fn model_info(&self) -> &ModelInfo {
            static_model_info()
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn simple_response(content: &str) -> ChatResponse {
        ChatResponse {
            content: content.to_string(),
            tool_calls: vec![],
            reasoning: None,
            finish_reason: FinishReason::Stop,
            usage: TokenUsage::default(),
            cache_meta: None,
        }
    }

    fn make_ctx_with_provider(
        provider: Arc<dyn hermes_core::provider::Provider>,
    ) -> (
        ToolContext,
        mpsc::Receiver<ApprovalRequest>,
        mpsc::Receiver<StreamDelta>,
    ) {
        make_ctx_with_depth(Some(provider), 0)
    }

    fn make_ctx_with_depth(
        provider: Option<Arc<dyn hermes_core::provider::Provider>>,
        depth: u32,
    ) -> (
        ToolContext,
        mpsc::Receiver<ApprovalRequest>,
        mpsc::Receiver<StreamDelta>,
    ) {
        let (approval_tx, approval_rx) = mpsc::channel(8);
        let (delta_tx, delta_rx) = mpsc::channel(8);
        let ctx = ToolContext {
            session_id: "test-session".to_string(),
            working_dir: std::env::temp_dir(),
            approval_tx,
            delta_tx,
            tool_config: Arc::new(ToolConfig::default()),
            memory: None,
            aux_provider: provider,
            skills: None,
            delegation_depth: depth,
            clarify_tx: None,
        };
        (ctx, approval_rx, delta_rx)
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_delegation_depth_limit() {
        let provider = Arc::new(MockProvider::new(vec![simple_response("hi")]));
        let (ctx, _approval_rx, _delta_rx) = make_ctx_with_depth(Some(provider), 1);
        let tool = DelegationTool::new(std::env::temp_dir().join("delegate-depth-test"));

        let result = tool
            .execute(json!({"goal": "do something"}), &ctx)
            .await
            .expect("execute should not fail");

        assert!(result.is_error);
        assert!(result.content.contains("depth limit"));
    }

    #[tokio::test]
    async fn test_delegation_no_provider() {
        let (ctx, _approval_rx, _delta_rx) = make_ctx_with_depth(None, 0);
        let tool = DelegationTool::new(std::env::temp_dir().join("delegate-no-provider"));

        let result = tool.execute(json!({"goal": "do something"}), &ctx).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("no provider"));
    }

    #[tokio::test]
    async fn test_delegation_basic() {
        let provider = Arc::new(MockProvider::new(vec![simple_response(
            "Task completed successfully.",
        )]));
        let (ctx, _approval_rx, _delta_rx) = make_ctx_with_provider(provider);
        let memory_dir =
            std::env::temp_dir().join(format!("delegate-basic-test-{}", uuid::Uuid::new_v4()));
        let tool = DelegationTool::new(memory_dir);

        let result = tool
            .execute(
                json!({"goal": "write a haiku", "context": "about rust programming"}),
                &ctx,
            )
            .await
            .expect("execute should not fail");

        assert!(!result.is_error);
        let parsed: serde_json::Value =
            serde_json::from_str(&result.content).expect("result should be JSON");
        assert_eq!(parsed["status"], "completed");
        assert_eq!(parsed["summary"], "Task completed successfully.");
        assert!(parsed["iterations_used"].as_u64().unwrap() > 0);
        assert!(parsed["duration_ms"].as_u64().is_some());
    }

    #[tokio::test]
    async fn test_delegation_child_history_isolated() {
        // The parent should only see the delegation result summary, not the
        // child's intermediate conversation history.
        let provider = Arc::new(MockProvider::new(vec![simple_response(
            "Child summary only.",
        )]));
        let (ctx, _approval_rx, _delta_rx) = make_ctx_with_provider(provider);
        let memory_dir =
            std::env::temp_dir().join(format!("delegate-isolation-test-{}", uuid::Uuid::new_v4()));
        let tool = DelegationTool::new(memory_dir);

        let result = tool
            .execute(json!({"goal": "analyze something"}), &ctx)
            .await
            .expect("execute should not fail");

        assert!(!result.is_error);
        let parsed: serde_json::Value =
            serde_json::from_str(&result.content).expect("result should be JSON");

        // Parent receives a structured JSON summary, not raw history
        assert_eq!(parsed["status"], "completed");
        assert_eq!(parsed["summary"], "Child summary only.");
        // The summary is a single string, not an array of messages
        assert!(parsed["summary"].is_string());
    }
}

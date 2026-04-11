//! Agent loop: orchestrates provider calls, tool execution, and budget control.

use std::sync::Arc;

use anyhow::Result;
use hermes_core::{
    message::{Content, Message, Role},
    provider::{ChatRequest, Provider},
    stream::StreamDelta,
    tool::{ApprovalDecision, ApprovalRequest, ToolContext, ToolSchema},
};
use hermes_tools::registry::ToolRegistry;
use tokio::sync::mpsc;

use crate::{
    budget::IterationBudget,
    parallel::{execute_parallel, execute_sequential, should_parallelize},
};

/// Configuration for constructing an `Agent`.
pub struct AgentConfig {
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<ToolRegistry>,
    pub max_iterations: u32,
    pub system_prompt: String,
    pub session_id: String,
}

/// Stateful agent that drives a conversation loop.
pub struct Agent {
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    budget: IterationBudget,
    system_prompt: String,
    session_id: String,
}

impl Agent {
    /// Construct an agent from `AgentConfig`.
    pub fn new(config: AgentConfig) -> Self {
        Self {
            provider: config.provider,
            registry: config.registry,
            budget: IterationBudget::new(config.max_iterations),
            system_prompt: config.system_prompt,
            session_id: config.session_id,
        }
    }

    /// Run one conversation turn.
    ///
    /// Pushes `user_message` onto `history`, then iterates until the provider
    /// returns a response with no tool calls or the budget is exhausted.
    pub async fn run_conversation(
        &mut self,
        user_message: &str,
        history: &mut Vec<Message>,
        delta_tx: mpsc::Sender<StreamDelta>,
    ) -> Result<String> {
        history.push(Message::user(user_message));

        // Dummy approval channel — always denies; callers that need real
        // approval must supply their own channel via a higher-level wrapper.
        let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalRequest>(8);
        tokio::spawn(async move {
            while let Some(req) = approval_rx.recv().await {
                let _ = req.response_tx.send(ApprovalDecision::Deny);
            }
        });

        while self.budget.try_consume() {
            let schemas: Vec<ToolSchema> = self.registry.available_schemas();

            let request = ChatRequest {
                system: &self.system_prompt,
                system_segments: None,
                messages: history.as_slice(),
                tools: &schemas,
                max_tokens: 4096,
                temperature: 0.0,
                reasoning: false,
                stop_sequences: vec![],
            };

            let response = self
                .provider
                .chat(&request, Some(&delta_tx))
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;

            // Push assistant turn to history.
            let assistant_msg = Message {
                role: Role::Assistant,
                content: Content::Text(response.content.clone()),
                tool_calls: response.tool_calls.clone(),
                reasoning: response.reasoning.clone(),
                name: None,
                tool_call_id: None,
            };
            history.push(assistant_msg);

            if response.tool_calls.is_empty() {
                return Ok(response.content);
            }

            // Execute tools.
            let ctx = ToolContext {
                session_id: self.session_id.clone(),
                working_dir: std::path::PathBuf::from("."),
                approval_tx: approval_tx.clone(),
                delta_tx: delta_tx.clone(),
            };

            let tool_results = if should_parallelize(&response.tool_calls, &self.registry) {
                execute_parallel(&response.tool_calls, Arc::clone(&self.registry), &ctx).await
            } else {
                execute_sequential(&response.tool_calls, Arc::clone(&self.registry), &ctx).await
            };

            // Push one tool-result message per result.
            for tr in tool_results {
                history.push(Message {
                    role: Role::Tool,
                    content: Content::Text(tr.result.content),
                    tool_calls: vec![],
                    reasoning: None,
                    name: Some(tr.tool_name),
                    tool_call_id: Some(tr.call_id),
                });
            }
        }

        Ok("[iteration budget exhausted]".to_string())
    }

    /// Iterations remaining in the current budget.
    pub fn remaining_budget(&self) -> u32 {
        self.budget.remaining()
    }

    /// Refund `n` iterations (saturating at `max`).
    pub fn refund_budget(&mut self, n: u32) {
        self.budget.refund(n);
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use hermes_core::{
        error::Result as HermesResult,
        message::ToolCall,
        provider::{ChatRequest, ChatResponse, FinishReason, ModelInfo, ModelPricing, TokenUsage},
        stream::StreamDelta,
    };

    use super::*;

    // ── MockProvider ──────────────────────────────────────────────────────────

    /// Returns responses from a pre-loaded queue, front-first.
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
    impl Provider for MockProvider {
        async fn chat(
            &self,
            _request: &ChatRequest<'_>,
            _delta_tx: Option<&mpsc::Sender<StreamDelta>>,
        ) -> HermesResult<ChatResponse> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                // Default fallback.
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

    fn tool_use_response(tool_name: &str) -> ChatResponse {
        ChatResponse {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: tool_name.to_string(),
                arguments: serde_json::json!({}),
            }],
            reasoning: None,
            finish_reason: FinishReason::ToolUse,
            usage: TokenUsage::default(),
            cache_meta: None,
        }
    }

    fn make_agent(responses: Vec<ChatResponse>, max_iterations: u32) -> Agent {
        let provider = Arc::new(MockProvider::new(responses));
        let registry = Arc::new(ToolRegistry::new());
        Agent::new(AgentConfig {
            provider,
            registry,
            max_iterations,
            system_prompt: "You are a helpful assistant.".to_string(),
            session_id: "test-session".to_string(),
        })
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_simple_conversation_no_tools() {
        let mut agent = make_agent(vec![simple_response("Hello, world!")], 10);
        let (delta_tx, _delta_rx) = mpsc::channel(32);
        let mut history: Vec<Message> = vec![];

        let result = agent
            .run_conversation("Hi", &mut history, delta_tx)
            .await
            .unwrap();

        assert_eq!(result, "Hello, world!");
        // user + assistant = 2 messages
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[1].role, Role::Assistant);
    }

    #[tokio::test]
    async fn test_tool_call_then_response() {
        // Iteration 1: provider returns a tool call.
        // Iteration 2: provider returns a final text response.
        let responses = vec![tool_use_response("unknown_tool"), simple_response("Done!")];
        let mut agent = make_agent(responses, 10);
        let (delta_tx, _delta_rx) = mpsc::channel(32);
        let mut history: Vec<Message> = vec![];

        let result = agent
            .run_conversation("Do something", &mut history, delta_tx)
            .await
            .unwrap();

        assert_eq!(result, "Done!");
        // user, assistant (tool_use), tool_result, assistant (final) = 4
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[1].role, Role::Assistant);
        assert_eq!(history[2].role, Role::Tool);
        assert_eq!(history[3].role, Role::Assistant);
    }

    #[tokio::test]
    async fn test_budget_exhaustion() {
        // Provider always returns a tool call → budget will be consumed.
        // budget=2 means 2 iterations; each consumes one iteration.
        let responses: Vec<ChatResponse> =
            (0..10).map(|_| tool_use_response("unknown_tool")).collect();
        let mut agent = make_agent(responses, 2);
        let (delta_tx, _delta_rx) = mpsc::channel(32);
        let mut history: Vec<Message> = vec![];

        let result = agent
            .run_conversation("Keep going", &mut history, delta_tx)
            .await
            .unwrap();

        assert_eq!(result, "[iteration budget exhausted]");
        assert!(agent.remaining_budget() == 0);
    }
}

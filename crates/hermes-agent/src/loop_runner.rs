//! Agent loop: orchestrates provider calls, tool execution, and budget control.

use std::{collections::HashSet, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use hermes_core::{
    clarify::ClarifyRequest,
    error::Result,
    message::{Content, Message, Role, ToolCall},
    provider::{ChatRequest, Provider},
    stream::StreamDelta,
    tool::{
        ApprovalRequest, SkillAccess, ToolConfig, ToolContext, ToolExecutionObserver, ToolSchema,
    },
};
use hermes_tools::registry::ToolRegistry;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, mpsc};

use crate::{
    budget::IterationBudget,
    cache_manager::PromptCacheManager,
    compressor::{CompressionConfig, CompressionResult, ContextCompressor},
    parallel::{execute_parallel, execute_sequential, should_parallelize},
};

#[async_trait]
pub trait ConversationCheckpointObserver: Send + Sync {
    async fn on_history_checkpoint(&self, history: &[Message]) -> Result<()>;

    async fn on_provider_call_started(
        &self,
        boundary: ConversationContinuationBoundary,
        request_history_len: usize,
        tool_count: usize,
    ) -> Result<()> {
        let _ = (boundary, request_history_len, tool_count);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationContinuationBoundaryKind {
    UserCheckpointed,
    AssistantResponseCheckpointed,
    PendingToolCalls,
    ToolResultsCheckpointed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationContinuationAction {
    CallProvider,
    ExecutePendingTools,
    CompleteTurn,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationContinuationBoundary {
    pub kind: ConversationContinuationBoundaryKind,
    pub safe_action: ConversationContinuationAction,
    pub history_len: usize,
    #[serde(default)]
    pub pending_tool_calls: usize,
}

fn pending_tool_calls_from_history(history: &[Message]) -> Vec<ToolCall> {
    let Some((assistant_index, assistant)) = history
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| message.role == Role::Assistant && !message.tool_calls.is_empty())
    else {
        return Vec::new();
    };

    let following = &history[assistant_index + 1..];
    if following.iter().any(|message| message.role != Role::Tool) {
        return Vec::new();
    }

    let completed_call_ids = following
        .iter()
        .filter_map(|message| message.tool_call_id.as_deref())
        .collect::<HashSet<_>>();
    assistant
        .tool_calls
        .iter()
        .filter(|tool_call| !completed_call_ids.contains(tool_call.id.as_str()))
        .cloned()
        .collect()
}

fn latest_assistant_response_text(history: &[Message]) -> Option<String> {
    history
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| message.content.as_text_lossy())
}

pub fn analyze_continuation_boundary(
    history: &[Message],
) -> Option<ConversationContinuationBoundary> {
    let pending_tool_calls = pending_tool_calls_from_history(history);
    if !pending_tool_calls.is_empty() {
        return Some(ConversationContinuationBoundary {
            kind: ConversationContinuationBoundaryKind::PendingToolCalls,
            safe_action: ConversationContinuationAction::ExecutePendingTools,
            history_len: history.len(),
            pending_tool_calls: pending_tool_calls.len(),
        });
    }

    let last = history.last()?;
    let (kind, safe_action) = match last.role {
        Role::User => (
            ConversationContinuationBoundaryKind::UserCheckpointed,
            ConversationContinuationAction::CallProvider,
        ),
        Role::Assistant => (
            ConversationContinuationBoundaryKind::AssistantResponseCheckpointed,
            ConversationContinuationAction::CompleteTurn,
        ),
        Role::Tool => (
            ConversationContinuationBoundaryKind::ToolResultsCheckpointed,
            ConversationContinuationAction::CallProvider,
        ),
        Role::System => return None,
    };

    Some(ConversationContinuationBoundary {
        kind,
        safe_action,
        history_len: history.len(),
        pending_tool_calls: 0,
    })
}

/// Configuration for constructing an `Agent`.
pub struct AgentConfig {
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<ToolRegistry>,
    pub max_iterations: u32,
    pub system_prompt: String,
    pub session_id: String,
    pub working_dir: PathBuf,
    pub approval_tx: mpsc::Sender<ApprovalRequest>,
    pub tool_config: Arc<ToolConfig>,
    pub execution_observer: Option<Arc<dyn ToolExecutionObserver>>,
    pub memory: hermes_memory::MemoryManager,
    pub skills: Option<Arc<RwLock<hermes_skills::SkillManager>>>,
    pub compression: CompressionConfig,
    /// Delegation depth: 0 for root agents, incremented for each child.
    pub delegation_depth: u32,
    /// Channel for clarify requests to the UI. Children get `None`.
    pub clarify_tx: Option<mpsc::Sender<ClarifyRequest>>,
    /// Optional observer for safe-point conversation checkpoints.
    pub checkpoint_observer: Option<Arc<dyn ConversationCheckpointObserver>>,
}

/// Stateful agent that drives a conversation loop.
pub struct Agent {
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    budget: IterationBudget,
    system_prompt: String,
    session_id: String,
    working_dir: PathBuf,
    approval_tx: mpsc::Sender<ApprovalRequest>,
    tool_config: Arc<ToolConfig>,
    execution_observer: Option<Arc<dyn ToolExecutionObserver>>,
    memory: hermes_memory::MemoryManager,
    skills: Option<Arc<RwLock<hermes_skills::SkillManager>>>,
    cache_manager: PromptCacheManager,
    compressor: ContextCompressor,
    delegation_depth: u32,
    clarify_tx: Option<mpsc::Sender<ClarifyRequest>>,
    checkpoint_observer: Option<Arc<dyn ConversationCheckpointObserver>>,
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
            working_dir: config.working_dir,
            approval_tx: config.approval_tx,
            tool_config: config.tool_config,
            execution_observer: config.execution_observer,
            memory: config.memory,
            skills: config.skills,
            cache_manager: PromptCacheManager::new(),
            compressor: ContextCompressor::new(config.compression),
            delegation_depth: config.delegation_depth,
            clarify_tx: config.clarify_tx,
            checkpoint_observer: config.checkpoint_observer,
        }
    }

    async fn checkpoint_history(&self, history: &[Message]) {
        let Some(observer) = &self.checkpoint_observer else {
            return;
        };

        if let Err(err) = observer.on_history_checkpoint(history).await {
            tracing::warn!(
                session_id = %self.session_id,
                "conversation checkpoint observer failed: {err}"
            );
        }
    }

    async fn note_provider_call_started(
        &self,
        history: &[Message],
        request_history_len: usize,
        tool_count: usize,
    ) {
        let Some(observer) = &self.checkpoint_observer else {
            return;
        };
        let Some(boundary) = analyze_continuation_boundary(history) else {
            return;
        };

        if let Err(err) = observer
            .on_provider_call_started(boundary, request_history_len, tool_count)
            .await
        {
            tracing::warn!(
                session_id = %self.session_id,
                "provider call fence observer failed: {err}"
            );
        }
    }

    fn current_turn_prompt(&self, user_message: Option<&str>, history: &[Message]) -> String {
        user_message
            .map(ToOwned::to_owned)
            .or_else(|| {
                history
                    .iter()
                    .rev()
                    .find(|message| message.role == Role::User)
                    .map(|message| message.content.as_text_lossy())
            })
            .unwrap_or_default()
    }

    fn pending_tool_calls(history: &[Message]) -> Vec<ToolCall> {
        pending_tool_calls_from_history(history)
    }

    fn tool_context(&self, delta_tx: mpsc::Sender<StreamDelta>) -> ToolContext {
        ToolContext {
            session_id: self.session_id.clone(),
            working_dir: self.working_dir.clone(),
            approval_tx: self.approval_tx.clone(),
            delta_tx,
            execution_observer: self.execution_observer.clone(),
            tool_config: Arc::clone(&self.tool_config),
            memory: Some(self.memory.tool_handle()),
            aux_provider: Some(Arc::clone(&self.provider)),
            skills: self.skills.as_ref().map(|skills| {
                Arc::new(hermes_skills::SharedSkillManager::new(Arc::clone(skills)))
                    as Arc<dyn SkillAccess>
            }),
            delegation_depth: self.delegation_depth,
            clarify_tx: self.clarify_tx.clone(),
        }
    }

    async fn execute_tool_calls(
        &mut self,
        tool_calls: &[ToolCall],
        history: &mut Vec<Message>,
        delta_tx: mpsc::Sender<StreamDelta>,
    ) -> bool {
        let ctx = self.tool_context(delta_tx);
        let tool_results = if should_parallelize(tool_calls, &self.registry) {
            execute_parallel(tool_calls, Arc::clone(&self.registry), &ctx).await
        } else {
            execute_sequential(tool_calls, Arc::clone(&self.registry), &ctx).await
        };

        let memory_write_succeeded = tool_results
            .iter()
            .any(|tr| tr.tool_name == "memory_write" && !tr.result.is_error);

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
        self.checkpoint_history(history).await;
        memory_write_succeeded
    }

    async fn post_tool_execution_maintenance(
        &mut self,
        history: &mut Vec<Message>,
        full_system: &mut String,
        segments: &mut Option<Vec<hermes_core::provider::CacheSegment>>,
        active_skills: &[hermes_skills::Skill],
        tool_count: usize,
        memory_write_succeeded: bool,
    ) {
        if memory_write_succeeded {
            self.cache_manager.invalidate();
            let memory_block = self.memory.system_prompt_blocks();
            *full_system = if memory_block.is_empty() {
                self.system_prompt.clone()
            } else {
                format!("{}\n\n{}", self.system_prompt, memory_block)
            };
            *segments = if self.provider.supports_caching() {
                Some(
                    self.cache_manager
                        .get_or_freeze(&self.system_prompt, &memory_block),
                )
            } else {
                None
            };
        }

        let compression_history =
            inject_active_skills_into_history(self.skills.as_ref(), active_skills, history).await;
        if self
            .compressor
            .should_compress(full_system, &compression_history, tool_count)
        {
            tracing::info!("context compression triggered");
            self.do_compression(history, full_system, segments).await;
        }

        const MAX_HISTORY_MESSAGES: usize = 500;
        if history.len() > MAX_HISTORY_MESSAGES {
            tracing::info!(
                count = history.len(),
                "history message cap reached, forcing compression"
            );
            self.do_compression(history, full_system, segments).await;
        }
    }

    async fn run_conversation_inner(
        &mut self,
        user_message: Option<&str>,
        history: &mut Vec<Message>,
        delta_tx: mpsc::Sender<StreamDelta>,
    ) -> Result<String> {
        if let Some(user_message) = user_message {
            history.push(Message::user(user_message));
            self.checkpoint_history(history).await;
        }

        let turn_prompt = self.current_turn_prompt(user_message, history);
        let active_skills = if let Some(skills) = &self.skills {
            skills.read().await.match_for_turn(&turn_prompt, history, 3)
        } else {
            Vec::new()
        };

        // Take prefetched memory context (for future external providers)
        let _memory_ctx = self.memory.take_prefetched(&self.session_id).await;

        // Build system prompt with memory blocks
        let memory_block = self.memory.system_prompt_blocks();
        let mut full_system = if memory_block.is_empty() {
            self.system_prompt.clone()
        } else {
            format!("{}\n\n{}", self.system_prompt, memory_block)
        };
        let _ = &mut full_system; // Task 8 (compression) will reassign this

        // Build cache segments (provider-gated)
        let mut segments = if self.provider.supports_caching() {
            let memory_block = self.memory.system_prompt_blocks();
            Some(
                self.cache_manager
                    .get_or_freeze(&self.system_prompt, &memory_block),
            )
        } else {
            None
        };
        let _ = &mut segments; // Task 8 (compression) will reassign

        let mut final_response = None;

        while self.budget.try_consume() {
            let schemas: Vec<ToolSchema> = self.registry.available_schemas();
            if user_message.is_none() {
                if let Some(boundary) = analyze_continuation_boundary(history) {
                    if boundary.safe_action == ConversationContinuationAction::CompleteTurn {
                        tracing::info!(
                            history_len = boundary.history_len,
                            "continuing from checkpointed final assistant response"
                        );
                        final_response = latest_assistant_response_text(history);
                        break;
                    }
                }
            }
            let pending_tool_calls = Self::pending_tool_calls(history);
            if !pending_tool_calls.is_empty() {
                tracing::info!(
                    pending_calls = pending_tool_calls.len(),
                    "continuing from checkpointed pending tool calls"
                );
                let memory_write_succeeded = self
                    .execute_tool_calls(&pending_tool_calls, history, delta_tx.clone())
                    .await;
                self.post_tool_execution_maintenance(
                    history,
                    &mut full_system,
                    &mut segments,
                    &active_skills,
                    schemas.len(),
                    memory_write_succeeded,
                )
                .await;
            }
            let request_history =
                inject_active_skills_into_history(self.skills.as_ref(), &active_skills, history)
                    .await;

            let request = ChatRequest {
                system: &full_system,
                system_segments: segments.as_deref(),
                messages: request_history.as_slice(),
                tools: &schemas,
                max_tokens: 4096,
                temperature: 0.0,
                reasoning: false,
                stop_sequences: vec![],
            };

            self.note_provider_call_started(history, request_history.len(), schemas.len())
                .await;

            tracing::debug!(
                budget_remaining = self.budget.remaining(),
                tools = schemas.len(),
                history_len = history.len(),
                "agent loop: calling provider"
            );
            let response = self.provider.chat(&request, Some(&delta_tx)).await?;
            tracing::debug!(
                content_len = response.content.len(),
                tool_calls = response.tool_calls.len(),
                "agent loop: provider returned"
            );

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
            self.checkpoint_history(history).await;

            if response.tool_calls.is_empty() {
                final_response = Some(response.content);
                break;
            }

            let memory_write_succeeded = self
                .execute_tool_calls(&response.tool_calls, history, delta_tx.clone())
                .await;
            self.post_tool_execution_maintenance(
                history,
                &mut full_system,
                &mut segments,
                &active_skills,
                schemas.len(),
                memory_write_succeeded,
            )
            .await;
        }

        let Some(final_response) = final_response else {
            return Ok("[iteration budget exhausted]".to_string());
        };

        // Memory lifecycle: sync turn data and prefetch for next turn
        self.memory
            .sync_turn(&turn_prompt, &final_response, &self.session_id);
        self.memory
            .queue_prefetch(&final_response, &self.session_id);

        Ok(final_response)
    }

    /// Run one conversation turn by appending a fresh user message.
    pub async fn run_conversation(
        &mut self,
        user_message: &str,
        history: &mut Vec<Message>,
        delta_tx: mpsc::Sender<StreamDelta>,
    ) -> Result<String> {
        self.run_conversation_inner(Some(user_message), history, delta_tx)
            .await
    }

    /// Continue an in-flight turn from already persisted history without
    /// injecting a new user message.
    pub async fn continue_conversation(
        &mut self,
        history: &mut Vec<Message>,
        delta_tx: mpsc::Sender<StreamDelta>,
    ) -> Result<String> {
        self.run_conversation_inner(None, history, delta_tx).await
    }

    /// Run the compression pipeline and rebuild system prompt / cache segments.
    ///
    /// Called from both the token-pressure check and the message-count hard cap.
    async fn do_compression(
        &mut self,
        history: &mut Vec<Message>,
        full_system: &mut String,
        segments: &mut Option<Vec<hermes_core::provider::CacheSegment>>,
    ) {
        let contrib = self.memory.on_pre_compress(history).await;
        match self
            .compressor
            .compress(history, self.provider.as_ref(), contrib.as_deref())
            .await
        {
            Ok(CompressionResult::Compressed {
                before_tokens,
                after_tokens,
                ..
            }) => {
                tracing::info!(
                    before = before_tokens,
                    after = after_tokens,
                    "compression complete"
                );
                // Invalidate prompt cache — system prompt context changed
                self.cache_manager.invalidate();
                // Refresh memory snapshot
                let _ = self.memory.refresh_snapshot();
                // Rebuild full_system and segments
                let memory_block = self.memory.system_prompt_blocks();
                *full_system = if memory_block.is_empty() {
                    self.system_prompt.clone()
                } else {
                    format!("{}\n\n{}", self.system_prompt, memory_block)
                };
                if self.provider.supports_caching() {
                    *segments = Some(
                        self.cache_manager
                            .get_or_freeze(&self.system_prompt, &memory_block),
                    );
                }
            }
            Ok(CompressionResult::NotNeeded) => {}
            Err(e) => {
                tracing::warn!("compression failed: {e}");
            }
        }
    }

    /// Iterations remaining in the current budget.
    pub fn remaining_budget(&self) -> u32 {
        self.budget.remaining()
    }

    /// Refund `n` iterations (saturating at `max`).
    pub fn refund_budget(&mut self, n: u32) {
        self.budget.refund(n);
    }

    /// Borrow the tool registry.
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Manually trigger context compression from the CLI.
    ///
    /// Returns a `CompressionResult` so callers can report outcomes without
    /// coupling this library code to stdout/stderr.
    pub async fn manual_compress(
        &mut self,
        history: &mut Vec<Message>,
    ) -> hermes_core::error::Result<CompressionResult> {
        // Rebuild full system prompt with memory blocks (same as run_conversation)
        let memory_block = self.memory.system_prompt_blocks();
        let full_system = if memory_block.is_empty() {
            self.system_prompt.clone()
        } else {
            format!("{}\n\n{}", self.system_prompt, memory_block)
        };

        let tool_count = self.registry.available_schemas().len();
        if !self
            .compressor
            .should_compress(&full_system, history, tool_count)
        {
            return Ok(CompressionResult::NotNeeded);
        }

        let contrib = self.memory.on_pre_compress(history).await;
        let result = self
            .compressor
            .compress(history, self.provider.as_ref(), contrib.as_deref())
            .await?;

        if matches!(&result, CompressionResult::Compressed { .. }) {
            tracing::info!("manual compression complete");
            self.cache_manager.invalidate();
            let _ = self.memory.refresh_snapshot();
        }

        Ok(result)
    }
}

async fn inject_active_skills_into_history(
    skills: Option<&Arc<RwLock<hermes_skills::SkillManager>>>,
    active_skills: &[hermes_skills::Skill],
    history: &[Message],
) -> Vec<Message> {
    if active_skills.is_empty() {
        return history.to_vec();
    }

    let mut request_history = history.to_vec();
    if let Some(skills) = skills {
        skills
            .read()
            .await
            .inject_active_into_history(active_skills, &mut request_history);
    }
    request_history
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::fs;
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

    fn make_agent(
        responses: Vec<ChatResponse>,
        max_iterations: u32,
    ) -> (Agent, mpsc::Receiver<ApprovalRequest>) {
        make_agent_with_config_and_skills(
            responses,
            max_iterations,
            CompressionConfig::default(),
            None,
        )
    }

    fn make_agent_with_config_and_skills(
        responses: Vec<ChatResponse>,
        max_iterations: u32,
        compression: CompressionConfig,
        skills: Option<Arc<RwLock<hermes_skills::SkillManager>>>,
    ) -> (Agent, mpsc::Receiver<ApprovalRequest>) {
        use hermes_memory::MemoryManager;

        let provider = Arc::new(MockProvider::new(responses));
        let registry = Arc::new(ToolRegistry::new());
        let (approval_tx, approval_rx) = mpsc::channel(8);
        let memory_dir = std::env::temp_dir().join(format!("hermes-test-{}", uuid::Uuid::new_v4()));
        let memory = MemoryManager::new(memory_dir, None).unwrap();
        let agent = Agent::new(AgentConfig {
            provider,
            registry,
            max_iterations,
            system_prompt: "You are a helpful assistant.".to_string(),
            session_id: "test-session".to_string(),
            working_dir: std::env::temp_dir(),
            approval_tx,
            tool_config: Arc::new(ToolConfig::default()),
            execution_observer: None,
            memory,
            skills,
            compression,
            delegation_depth: 0,
            clarify_tx: None,
            checkpoint_observer: None,
        });
        (agent, approval_rx)
    }

    struct RecordingCheckpointObserver {
        checkpoints: Arc<Mutex<Vec<usize>>>,
    }

    #[async_trait]
    impl ConversationCheckpointObserver for RecordingCheckpointObserver {
        async fn on_history_checkpoint(&self, history: &[Message]) -> Result<()> {
            self.checkpoints.lock().unwrap().push(history.len());
            Ok(())
        }
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_simple_conversation_no_tools() {
        let (mut agent, _rx) = make_agent(vec![simple_response("Hello, world!")], 10);
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
        let (mut agent, _rx) = make_agent(responses, 10);
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
    async fn test_continue_conversation_executes_pending_tool_calls_before_provider_call() {
        let responses = vec![simple_response("Done!")];
        let (mut agent, _rx) = make_agent(responses, 10);
        let (delta_tx, _delta_rx) = mpsc::channel(32);
        let mut history: Vec<Message> = vec![Message::user("Do something")];
        history.push(Message {
            role: Role::Assistant,
            content: Content::Text(String::new()),
            tool_calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "unknown_tool".to_string(),
                arguments: serde_json::json!({}),
            }],
            reasoning: None,
            name: None,
            tool_call_id: None,
        });

        let result = agent
            .continue_conversation(&mut history, delta_tx)
            .await
            .unwrap();

        assert_eq!(result, "Done!");
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[1].role, Role::Assistant);
        assert_eq!(history[2].role, Role::Tool);
        assert_eq!(history[2].tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(history[3].role, Role::Assistant);
    }

    #[tokio::test]
    async fn test_continue_conversation_returns_checkpointed_final_assistant_response() {
        let (mut agent, _rx) = make_agent(Vec::new(), 10);
        let (delta_tx, _delta_rx) = mpsc::channel(32);
        let mut history: Vec<Message> = vec![
            Message::user("Do something"),
            Message::assistant("Already done."),
        ];

        let result = agent
            .continue_conversation(&mut history, delta_tx)
            .await
            .unwrap();

        assert_eq!(result, "Already done.");
        assert_eq!(history.len(), 2);
        assert_eq!(
            analyze_continuation_boundary(&history)
                .as_ref()
                .map(|boundary| boundary.kind),
            Some(ConversationContinuationBoundaryKind::AssistantResponseCheckpointed)
        );
    }

    #[tokio::test]
    async fn test_checkpoint_observer_records_safe_points_without_tools() {
        let checkpoints = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(MockProvider::new(vec![simple_response("Hello, world!")]));
        let registry = Arc::new(ToolRegistry::new());
        let (approval_tx, _approval_rx) = mpsc::channel(8);
        let memory_dir = std::env::temp_dir().join(format!("hermes-test-{}", uuid::Uuid::new_v4()));
        let memory = hermes_memory::MemoryManager::new(memory_dir, None).unwrap();
        let observer: Arc<dyn ConversationCheckpointObserver> =
            Arc::new(RecordingCheckpointObserver {
                checkpoints: Arc::clone(&checkpoints),
            });
        let mut agent = Agent::new(AgentConfig {
            provider,
            registry,
            max_iterations: 10,
            system_prompt: "You are a helpful assistant.".to_string(),
            session_id: "test-session".to_string(),
            working_dir: std::env::temp_dir(),
            approval_tx,
            tool_config: Arc::new(ToolConfig::default()),
            execution_observer: None,
            memory,
            skills: None,
            compression: CompressionConfig::default(),
            delegation_depth: 0,
            clarify_tx: None,
            checkpoint_observer: Some(observer),
        });
        let (delta_tx, _delta_rx) = mpsc::channel(32);
        let mut history: Vec<Message> = vec![];

        let result = agent
            .run_conversation("Hi", &mut history, delta_tx)
            .await
            .unwrap();

        assert_eq!(result, "Hello, world!");
        assert_eq!(*checkpoints.lock().unwrap(), vec![1, 2]);
    }

    #[tokio::test]
    async fn test_checkpoint_observer_records_safe_points_with_tool_results() {
        let checkpoints = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(MockProvider::new(vec![
            tool_use_response("unknown_tool"),
            simple_response("Done!"),
        ]));
        let registry = Arc::new(ToolRegistry::new());
        let (approval_tx, _approval_rx) = mpsc::channel(8);
        let memory_dir = std::env::temp_dir().join(format!("hermes-test-{}", uuid::Uuid::new_v4()));
        let memory = hermes_memory::MemoryManager::new(memory_dir, None).unwrap();
        let observer: Arc<dyn ConversationCheckpointObserver> =
            Arc::new(RecordingCheckpointObserver {
                checkpoints: Arc::clone(&checkpoints),
            });
        let mut agent = Agent::new(AgentConfig {
            provider,
            registry,
            max_iterations: 10,
            system_prompt: "You are a helpful assistant.".to_string(),
            session_id: "test-session".to_string(),
            working_dir: std::env::temp_dir(),
            approval_tx,
            tool_config: Arc::new(ToolConfig::default()),
            execution_observer: None,
            memory,
            skills: None,
            compression: CompressionConfig::default(),
            delegation_depth: 0,
            clarify_tx: None,
            checkpoint_observer: Some(observer),
        });
        let (delta_tx, _delta_rx) = mpsc::channel(32);
        let mut history: Vec<Message> = vec![];

        let result = agent
            .run_conversation("Do something", &mut history, delta_tx)
            .await
            .unwrap();

        assert_eq!(result, "Done!");
        assert_eq!(*checkpoints.lock().unwrap(), vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn test_budget_exhaustion() {
        // Provider always returns a tool call → budget will be consumed.
        // budget=2 means 2 iterations; each consumes one iteration.
        let responses: Vec<ChatResponse> =
            (0..10).map(|_| tool_use_response("unknown_tool")).collect();
        let (mut agent, _rx) = make_agent(responses, 2);
        let (delta_tx, _delta_rx) = mpsc::channel(32);
        let mut history: Vec<Message> = vec![];

        let result = agent
            .run_conversation("Keep going", &mut history, delta_tx)
            .await
            .unwrap();

        assert_eq!(result, "[iteration budget exhausted]");
        assert!(agent.remaining_budget() == 0);
    }

    #[tokio::test]
    async fn test_skill_injection_counts_toward_compression() {
        let skills_dir = tempfile::tempdir().unwrap();
        let skill_dir = skills_dir.path().join("compress-helper");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                r#"---
name: compress-helper
description: Helps with compression tests
platforms: [linux]
---

{}
"#,
                "A".repeat(2_400)
            ),
        )
        .unwrap();

        let skills = Arc::new(RwLock::new(
            hermes_skills::SkillManager::new(vec![skills_dir.path().to_path_buf()]).unwrap(),
        ));
        let compression = CompressionConfig {
            max_context_tokens: 200,
            pressure_threshold: 0.4,
            target_after_compression: 0.1,
            protect_head_messages: 1,
        };
        let compressor = ContextCompressor::new(compression);
        let history = vec![Message::user("please use $compress-helper")];
        let active_skills =
            skills
                .read()
                .await
                .match_for_turn("please use $compress-helper", &history, 3);
        let request_history =
            inject_active_skills_into_history(Some(&skills), &active_skills, &history).await;

        assert!(!compressor.should_compress("You are a helpful assistant.", &history, 0));
        assert!(compressor.should_compress("You are a helpful assistant.", &request_history, 0));
        assert!(
            request_history[0]
                .content
                .as_text_lossy()
                .contains("[Active skills for this turn]")
        );
    }
}

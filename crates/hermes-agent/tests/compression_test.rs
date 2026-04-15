//! Integration tests verifying the context-compression pipeline fires
//! during a long agent conversation, and stays silent for short ones.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hermes_agent::{
    compressor::CompressionConfig,
    loop_runner::{Agent, AgentConfig},
};
use hermes_core::{
    error::Result as HermesResult,
    message::{Role, ToolCall},
    provider::{
        ChatRequest, ChatResponse, FinishReason, ModelInfo, ModelPricing, Provider, TokenUsage,
    },
    stream::StreamDelta,
    tool::{ApprovalRequest, ToolConfig},
};
use hermes_memory::MemoryManager;
use hermes_tools::registry::ToolRegistry;
use tokio::sync::mpsc;

// ── MockProvider ──────────────────────────────────────────────────────────────

/// Returns tool-call responses for the first `max_tool_rounds` invocations,
/// then a final Stop response.  Each tool-call response also carries a long
/// content string so that the conversation accumulates tokens quickly.
struct LongConversationProvider {
    call_count: Mutex<usize>,
    max_tool_rounds: usize,
}

impl LongConversationProvider {
    fn new(max_tool_rounds: usize) -> Self {
        Self {
            call_count: Mutex::new(0),
            max_tool_rounds,
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
impl Provider for LongConversationProvider {
    async fn chat(
        &self,
        _req: &ChatRequest<'_>,
        _tx: Option<&mpsc::Sender<StreamDelta>>,
    ) -> HermesResult<ChatResponse> {
        let mut count = self.call_count.lock().unwrap();
        *count += 1;
        let n = *count;

        if n <= self.max_tool_rounds {
            Ok(ChatResponse {
                content: format!("Round {} — calling tool. {}", n, "x".repeat(100)),
                tool_calls: vec![ToolCall {
                    id: format!("call_{n}"),
                    name: "unknown_tool".to_string(),
                    arguments: serde_json::json!({ "data": "y".repeat(100) }),
                }],
                reasoning: None,
                finish_reason: FinishReason::ToolUse,
                usage: TokenUsage::default(),
                cache_meta: None,
            })
        } else {
            Ok(ChatResponse {
                content: "All done! Conversation complete.".to_string(),
                tool_calls: vec![],
                reasoning: None,
                finish_reason: FinishReason::Stop,
                usage: TokenUsage::default(),
                cache_meta: None,
            })
        }
    }

    fn model_info(&self) -> &ModelInfo {
        static_model_info()
    }
}

// ── Helper ────────────────────────────────────────────────────────────────────

fn make_agent(
    provider: Arc<dyn Provider>,
    workspace: &tempfile::TempDir,
    compression: CompressionConfig,
    max_iterations: u32,
) -> (Agent, mpsc::Receiver<ApprovalRequest>) {
    let registry = Arc::new(ToolRegistry::new());
    let (approval_tx, approval_rx) = mpsc::channel::<ApprovalRequest>(8);
    let memory = MemoryManager::new(workspace.path().join(".hermes-memory"), None).unwrap();

    let agent = Agent::new(AgentConfig {
        provider,
        registry,
        max_iterations,
        system_prompt: "test".to_string(),
        session_id: "compression-test".to_string(),
        working_dir: workspace.path().to_path_buf(),
        approval_tx,
        tool_config: Arc::new(ToolConfig::default()),
        memory,
        skills: None,
        compression,
    });

    (agent, approval_rx)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Compression fires when a long conversation exceeds the pressure threshold.
///
/// Setup: 10 tool rounds with ~200 chars of content each + a very low
/// max_context_tokens (500) triggers compression before the conversation ends.
#[tokio::test]
async fn test_compression_fires_during_long_conversation() {
    let workspace = tempfile::TempDir::new().unwrap();

    // Very low thresholds so compression fires after a few tool rounds.
    let compression = CompressionConfig {
        max_context_tokens: 500,
        pressure_threshold: 0.30, // triggers at ~150 tokens
        target_after_compression: 0.20,
        protect_head_messages: 2,
    };

    let provider = Arc::new(LongConversationProvider::new(10));
    let (mut agent, _rx) = make_agent(provider, &workspace, compression, 30);

    let (tx, _rx) = mpsc::channel(64);
    let mut history = Vec::new();

    let result = agent
        .run_conversation("Start a long task", &mut history, tx)
        .await
        .unwrap();

    // Agent should complete normally, not exhaust budget.
    assert_eq!(result, "All done! Conversation complete.");

    // Last message must be the final assistant turn.
    let last = history.last().unwrap();
    assert_eq!(last.role, Role::Assistant);
    assert!(
        last.content.as_text_lossy().contains("All done"),
        "last message should be the final response"
    );

    // Compression inserts a <context-summary> user message into history.
    let has_summary = history
        .iter()
        .any(|m| m.content.as_text_lossy().contains("<context-summary>"));
    assert!(
        has_summary,
        "compression should have inserted a <context-summary> message; history len = {}",
        history.len()
    );

    // With compression the history must be shorter than the uncompressed maximum:
    // 1 user + 10 * (assistant + tool_result) + 1 final assistant = 22 messages.
    println!("History length after compression: {}", history.len());
    assert!(
        history.len() < 22,
        "expected compression to shorten history below 22, got {}",
        history.len()
    );
}

/// No compression when the conversation is short and thresholds are high.
#[tokio::test]
async fn test_no_compression_when_below_threshold() {
    let workspace = tempfile::TempDir::new().unwrap();

    // Default CompressionConfig has 200k max — nowhere near threshold for 2 rounds.
    let compression = CompressionConfig::default();

    let provider = Arc::new(LongConversationProvider::new(2));
    let (mut agent, _rx) = make_agent(provider, &workspace, compression, 10);

    let (tx, _rx) = mpsc::channel(64);
    let mut history = Vec::new();

    let result = agent
        .run_conversation("Short task", &mut history, tx)
        .await
        .unwrap();

    assert_eq!(result, "All done! Conversation complete.");

    // No <context-summary> should appear in history.
    let has_summary = history
        .iter()
        .any(|m| m.content.as_text_lossy().contains("<context-summary>"));
    assert!(
        !has_summary,
        "no compression expected for a short conversation, but found <context-summary>"
    );

    // Exact message count: 1 user + 2*(assistant+tool_result) + 1 final = 6
    assert_eq!(
        history.len(),
        6,
        "expected 6 messages in uncompressed history, got {}: {history:#?}",
        history.len()
    );
}

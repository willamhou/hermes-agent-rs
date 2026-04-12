//! Context compression: layered pruning, summarization, and tool-pair sanitization.

use std::cmp::Reverse;
use std::collections::HashSet;

use hermes_core::{
    error::Result,
    message::{Content, Message, Role},
    provider::{ChatRequest, Provider},
};

use crate::token_counter::TokenCounter;

// ─── Config ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CompressionConfig {
    pub max_context_tokens: usize,
    pub pressure_threshold: f32,
    pub target_after_compression: f32,
    pub protect_head_messages: usize,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            max_context_tokens: 200_000,
            pressure_threshold: 0.50,
            target_after_compression: 0.20,
            protect_head_messages: 3,
        }
    }
}

// ─── Result ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum CompressionResult {
    NotNeeded,
    Compressed {
        before_tokens: usize,
        after_tokens: usize,
        messages_removed: usize,
        messages_kept: usize,
    },
}

// ─── Compressor ──────────────────────────────────────────────────────────────

pub struct ContextCompressor {
    config: CompressionConfig,
    previous_summary: Option<String>,
}

impl ContextCompressor {
    pub fn new(config: CompressionConfig) -> Self {
        Self {
            config,
            previous_summary: None,
        }
    }

    /// Returns `true` if the estimated token count exceeds the pressure threshold.
    pub fn should_compress(&self, system: &str, messages: &[Message], tool_count: usize) -> bool {
        let total = TokenCounter::estimate_request(system, messages, tool_count);
        let threshold =
            (self.config.pressure_threshold * self.config.max_context_tokens as f32) as usize;
        total >= threshold
    }

    /// Run the full 5-phase compression pipeline, modifying `messages` in place.
    pub async fn compress(
        &mut self,
        messages: &mut Vec<Message>,
        provider: &dyn Provider,
        memory_contribution: Option<&str>,
    ) -> Result<CompressionResult> {
        let before_tokens = TokenCounter::count_messages(messages);

        // Phase 1 — prune tool results
        self.prune_tool_results(messages);

        // Phase 2 — find boundaries
        let head_end = self.config.protect_head_messages.min(messages.len());
        let tail_start = find_tail_start(messages, &self.config);

        if head_end >= tail_start {
            return Ok(CompressionResult::NotNeeded);
        }

        let messages_removed = tail_start - head_end;

        // Phase 3 — summarize
        let summary = self
            .summarize(
                messages,
                head_end,
                tail_start,
                provider,
                memory_contribution,
            )
            .await;

        // Phase 4 — rebuild
        messages.drain(head_end..tail_start);
        messages.insert(
            head_end,
            Message::user(format!("<context-summary>\n{summary}\n</context-summary>")),
        );

        // Phase 5 — sanitize tool pairs
        sanitize_tool_pairs(messages);

        let after_tokens = TokenCounter::count_messages(messages);

        Ok(CompressionResult::Compressed {
            before_tokens,
            after_tokens,
            messages_removed,
            messages_kept: messages.len(),
        })
    }

    // ── Phase 1 ──────────────────────────────────────────────────────────────

    fn prune_tool_results(&self, messages: &mut [Message]) {
        let tail_budget =
            (self.config.target_after_compression * self.config.max_context_tokens as f32) as usize;

        // Walk backward to find the tail protection boundary (token offset from end).
        let mut tail_tokens = 0;
        let mut tail_boundary = messages.len();
        for i in (0..messages.len()).rev() {
            tail_tokens += TokenCounter::count_message(&messages[i]);
            if tail_tokens >= tail_budget {
                tail_boundary = i;
                break;
            }
        }

        // Walk forward: prune long tool results before the boundary.
        for msg in messages.iter_mut().take(tail_boundary) {
            if msg.role == Role::Tool {
                let text = msg.content.as_text_lossy();
                if text.len() > 200 {
                    msg.content = Content::Text("[Previous tool output cleared]".to_string());
                }
            }
        }
    }

    // ── Phase 3 ──────────────────────────────────────────────────────────────

    async fn summarize(
        &mut self,
        messages: &[Message],
        head_end: usize,
        tail_start: usize,
        provider: &dyn Provider,
        memory_contribution: Option<&str>,
    ) -> String {
        let compressible = &messages[head_end..tail_start];
        let serialized = serialize_messages(compressible);

        let mut prompt = String::from(
            "Summarize this conversation context. Preserve:\n\
             - Goal: What the user is trying to accomplish\n\
             - Progress: What's been done, in progress, blocked\n\
             - Key Decisions: Technical choices and rationale\n\
             - Relevant Files: Paths mentioned with context\n\
             - Next Steps: What to do next\n\
             - Critical Context: Specific values, errors, config\n",
        );

        if let Some(prev) = &self.previous_summary {
            prompt.push_str("\n--- PREVIOUS SUMMARY ---\n");
            prompt.push_str(prev);
            prompt.push('\n');
        }

        if let Some(mem) = memory_contribution {
            prompt.push_str("\n--- MEMORY CONTEXT ---\n");
            prompt.push_str(mem);
            prompt.push('\n');
        }

        prompt.push_str("\n--- MESSAGES TO COMPRESS ---\n");
        prompt.push_str(&serialized);

        let request = ChatRequest {
            system: "You are a conversation summarizer.",
            system_segments: None,
            messages: &[Message::user(prompt)],
            tools: &[],
            max_tokens: 4096,
            temperature: 0.0,
            reasoning: false,
            stop_sequences: vec![],
        };

        match provider.chat(&request, None).await {
            Ok(response) => {
                self.previous_summary = Some(response.content.clone());
                response.content
            }
            Err(e) => {
                tracing::warn!("Summarization failed: {e}");
                let fallback = "[Context compressed - summary unavailable]".to_string();
                self.previous_summary = Some(fallback.clone());
                fallback
            }
        }
    }
}

// ─── Phase 2: find_tail_start ────────────────────────────────────────────────

fn find_tail_start(messages: &[Message], config: &CompressionConfig) -> usize {
    let tail_budget = (config.target_after_compression * config.max_context_tokens as f32) as usize;
    let min_tail = 3;

    let mut accumulated = 0;
    let mut raw_tail_start = messages.len();

    for i in (0..messages.len()).rev() {
        accumulated += TokenCounter::count_message(&messages[i]);
        raw_tail_start = i;
        if accumulated >= tail_budget {
            break;
        }
    }

    // Ensure at least min_tail messages in the tail.
    let max_tail_start = messages.len().saturating_sub(min_tail);
    let mut tail_start = raw_tail_start.min(max_tail_start);

    // Tool pair alignment: if tail_start lands on a Tool message, walk back to
    // include the parent assistant message that issued the tool call.
    if tail_start < messages.len() && messages[tail_start].role == Role::Tool {
        if let Some(ref tool_call_id) = messages[tail_start].tool_call_id {
            for j in (0..tail_start).rev() {
                if messages[j].role == Role::Assistant
                    && messages[j]
                        .tool_calls
                        .iter()
                        .any(|tc| &tc.id == tool_call_id)
                {
                    tail_start = j;
                    break;
                }
            }
        }
    }

    tail_start
}

// ─── Phase 5: sanitize_tool_pairs ────────────────────────────────────────────

fn sanitize_tool_pairs(messages: &mut Vec<Message>) {
    // Collect expected IDs (from assistant tool_calls).
    let expected: HashSet<String> = messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| m.tool_calls.iter().map(|tc| tc.id.clone()))
        .collect();

    // Collect actual result IDs (from Tool messages).
    let actual: HashSet<String> = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    // Remove orphan tool results (result with no matching call).
    messages.retain(|m| {
        if m.role == Role::Tool {
            if let Some(ref id) = m.tool_call_id {
                return expected.contains(id);
            }
        }
        true
    });

    // Add stubs for missing results (call with no matching result).
    // Each stub must be placed immediately after the assistant message that issued the call,
    // not at the end — the Anthropic API requires tool results to follow their parent.
    let missing: Vec<String> = expected.difference(&actual).cloned().collect();

    // Collect (insert_pos, stub) pairs, then insert from bottom to top so that
    // earlier insertions don't shift the positions of later ones.
    let mut insertions: Vec<(usize, Message)> = missing
        .into_iter()
        .map(|id| {
            let insert_pos = messages
                .iter()
                .position(|m| {
                    m.role == Role::Assistant && m.tool_calls.iter().any(|tc| tc.id == id)
                })
                .map(|i| i + 1)
                .unwrap_or(messages.len());
            let stub = Message {
                role: Role::Tool,
                content: Content::Text("[Tool result removed during compression]".to_string()),
                tool_calls: vec![],
                reasoning: None,
                name: None,
                tool_call_id: Some(id),
            };
            (insert_pos, stub)
        })
        .collect();

    // Sort descending by position so insertions from the bottom don't invalidate earlier positions.
    insertions.sort_by_key(|b| Reverse(b.0));
    for (pos, stub) in insertions {
        messages.insert(pos, stub);
    }
}

// ─── Serialization helpers ───────────────────────────────────────────────────

fn serialize_messages(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        match msg.role {
            Role::User => {
                let text = truncate(&msg.content.as_text_lossy(), 6000);
                out.push_str(&format!("[User]: {text}\n"));
            }
            Role::Assistant => {
                let text = truncate(&msg.content.as_text_lossy(), 6000);
                out.push_str(&format!("[Assistant]: {text}"));
                for tc in &msg.tool_calls {
                    let args = truncate(&tc.arguments.to_string(), 1500);
                    out.push_str(&format!(" [called {}({})]", tc.name, args));
                }
                out.push('\n');
            }
            Role::Tool => {
                let name = msg.name.as_deref().unwrap_or("unknown");
                let text = truncate(&msg.content.as_text_lossy(), 6000);
                out.push_str(&format!("[{name} result]: {text}\n"));
            }
            Role::System => {
                let text = truncate(&msg.content.as_text_lossy(), 6000);
                out.push_str(&format!("[System]: {text}\n"));
            }
        }
    }
    out
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        // Use char_indices for an efficient byte-boundary slice.
        let end = s
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}...", &s[..end])
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use hermes_core::{
        error::Result as HermesResult,
        message::{Content, Message, Role, ToolCall},
        provider::{
            ChatRequest, ChatResponse, FinishReason, ModelInfo, ModelPricing, Provider, TokenUsage,
        },
        stream::StreamDelta,
    };
    use tokio::sync::mpsc;

    use super::*;

    // ── MockProvider ──────────────────────────────────────────────────────────

    struct MockProvider {
        responses: Mutex<Vec<ChatResponse>>,
        captured_requests: Arc<Mutex<Vec<String>>>,
    }

    impl MockProvider {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                captured_requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_capture(responses: Vec<ChatResponse>) -> (Self, Arc<Mutex<Vec<String>>>) {
            let captured = Arc::new(Mutex::new(Vec::new()));
            let provider = Self {
                responses: Mutex::new(responses),
                captured_requests: Arc::clone(&captured),
            };
            (provider, captured)
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
            request: &ChatRequest<'_>,
            _delta_tx: Option<&mpsc::Sender<StreamDelta>>,
        ) -> HermesResult<ChatResponse> {
            // Capture the user message content for test inspection.
            if let Some(msg) = request.messages.first() {
                self.captured_requests
                    .lock()
                    .unwrap()
                    .push(msg.content.as_text_lossy());
            }

            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Ok(ChatResponse {
                    content: "summary placeholder".to_string(),
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

    fn summary_response(text: &str) -> ChatResponse {
        ChatResponse {
            content: text.to_string(),
            tool_calls: vec![],
            reasoning: None,
            finish_reason: FinishReason::Stop,
            usage: TokenUsage::default(),
            cache_meta: None,
        }
    }

    fn make_tool_message(name: &str, call_id: &str, content: &str) -> Message {
        Message {
            role: Role::Tool,
            content: Content::Text(content.to_string()),
            tool_calls: vec![],
            reasoning: None,
            name: Some(name.to_string()),
            tool_call_id: Some(call_id.to_string()),
        }
    }

    fn make_assistant_with_tool_call(call_id: &str, tool_name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: Content::Text(String::new()),
            tool_calls: vec![ToolCall {
                id: call_id.to_string(),
                name: tool_name.to_string(),
                arguments: serde_json::json!({}),
            }],
            reasoning: None,
            name: None,
            tool_call_id: None,
        }
    }

    fn make_compressor(max_tokens: usize, threshold: f32, target: f32) -> ContextCompressor {
        ContextCompressor::new(CompressionConfig {
            max_context_tokens: max_tokens,
            pressure_threshold: threshold,
            target_after_compression: target,
            protect_head_messages: 3,
        })
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    // 1. should_compress below threshold
    #[test]
    fn test_should_compress_below_threshold() {
        let compressor = make_compressor(200_000, 0.50, 0.20);
        let messages: Vec<Message> = (0..10).map(|i| Message::user(format!("msg {i}"))).collect();
        assert!(!compressor.should_compress("system", &messages, 0));
    }

    // 2. should_compress above threshold
    #[test]
    fn test_should_compress_above_threshold() {
        let compressor = make_compressor(100, 0.50, 0.20);
        let messages: Vec<Message> = (0..20)
            .map(|i| Message::user(format!("long message content number {i} with extra words")))
            .collect();
        assert!(compressor.should_compress("system", &messages, 0));
    }

    // 3. should_compress includes system prompt
    #[test]
    fn test_should_compress_includes_system() {
        let compressor = make_compressor(100, 0.50, 0.20);
        let big_system = "x".repeat(400);
        let messages = vec![Message::user("hi")];
        // big system alone should push over 50% of 100 tokens
        assert!(compressor.should_compress(&big_system, &messages, 0));
    }

    // 4. prune_tool_results — long content replaced
    #[test]
    fn test_prune_tool_results_long() {
        let compressor = make_compressor(200_000, 0.50, 0.20);
        let long_content = "x".repeat(500);
        let mut messages = vec![
            Message::user("start"),
            make_tool_message("bash", "c1", &long_content),
            Message::user("end"),
        ];
        compressor.prune_tool_results(&mut messages);
        assert_eq!(
            messages[1].content.as_text_lossy(),
            "[Previous tool output cleared]"
        );
    }

    // 5. prune_tool_results — short content preserved
    #[test]
    fn test_prune_tool_results_short_preserved() {
        let compressor = make_compressor(200_000, 0.50, 0.20);
        let short_content = "x".repeat(50);
        let mut messages = vec![
            Message::user("start"),
            make_tool_message("bash", "c1", &short_content),
            Message::user("end"),
        ];
        compressor.prune_tool_results(&mut messages);
        assert_eq!(messages[1].content.as_text_lossy(), short_content);
    }

    // 6. find_boundaries_basic
    #[test]
    fn test_find_boundaries_basic() {
        let config = CompressionConfig {
            max_context_tokens: 200,
            pressure_threshold: 0.50,
            target_after_compression: 0.10, // tail budget = 20 tokens → ~3-4 messages
            protect_head_messages: 3,
        };
        // Each message is ~20 tokens ("message number NN with extra padding text")
        let messages: Vec<Message> = (0..20)
            .map(|i| {
                Message::user(format!(
                    "message number {i} with extra padding text to ensure size"
                ))
            })
            .collect();
        let tail_start = find_tail_start(&messages, &config);

        // tail_start should be between head_end(3) and len-3
        assert!(tail_start >= 3, "tail_start={tail_start} should be >= 3");
        assert!(tail_start <= 17, "tail_start={tail_start} should be <= 17");
    }

    // 7. find_boundaries — tool pair alignment
    #[test]
    fn test_find_boundaries_tool_pair_alignment() {
        // Build a conversation where naive tail_start would land on a Tool message.
        // Use a tiny budget so that only the last few messages fit.
        let config = CompressionConfig {
            max_context_tokens: 200,
            pressure_threshold: 0.50,
            target_after_compression: 0.10, // very small tail budget
            protect_head_messages: 2,
        };

        let mut messages: Vec<Message> = Vec::new();
        // Head: 2 messages
        messages.push(Message::user("first"));
        messages.push(Message::assistant("ok"));
        // Middle: filler
        for i in 0..5 {
            messages.push(Message::user(format!("filler {i}")));
        }
        // End: assistant with tool call, then tool result, then final user
        messages.push(make_assistant_with_tool_call("call-99", "read_file"));
        messages.push(make_tool_message("read_file", "call-99", "file contents"));
        messages.push(Message::user("thanks"));

        let tail_start = find_tail_start(&messages, &config);

        // If tail_start would land on the tool message (index 8), it should
        // back up to include the assistant message (index 7).
        if tail_start <= 8 {
            assert_ne!(messages[tail_start].role, Role::Tool);
        }
    }

    // 8. sanitize — orphan result removed
    #[test]
    fn test_sanitize_orphan_result_removed() {
        let mut messages = vec![
            Message::user("hello"),
            make_tool_message("bash", "orphan-id", "some output"),
            Message::assistant("done"),
        ];
        sanitize_tool_pairs(&mut messages);
        // The orphan tool message should be removed.
        assert!(
            !messages
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some("orphan-id"))
        );
    }

    // 9. sanitize — missing result gets stub
    #[test]
    fn test_sanitize_missing_result_stub_added() {
        let mut messages = vec![
            Message::user("hello"),
            make_assistant_with_tool_call("call-42", "read_file"),
            Message::assistant("done"),
        ];
        sanitize_tool_pairs(&mut messages);
        // A stub should have been appended.
        let stub = messages
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call-42"));
        assert!(stub.is_some());
        assert_eq!(
            stub.unwrap().content.as_text_lossy(),
            "[Tool result removed during compression]"
        );
    }

    // 10. sanitize — stub inserted immediately after its parent assistant message
    #[test]
    fn test_sanitize_stub_position_after_parent() {
        // History: [user, assistant(tool_calls=[c1]), user_msg]
        // No tool result for c1.  After sanitize the stub must be at index 2
        // (right after the assistant), NOT at the end (index 3).
        let mut messages = vec![
            Message::user("start"),
            make_assistant_with_tool_call("c1", "read_file"),
            Message::user("follow up"),
        ];
        sanitize_tool_pairs(&mut messages);

        // Verify the stub exists.
        let stub_pos = messages
            .iter()
            .position(|m| m.tool_call_id.as_deref() == Some("c1"))
            .expect("stub should be present");

        // The assistant is at index 1; the stub must be at index 2 (right after).
        assert_eq!(
            stub_pos, 2,
            "stub should be at index 2, right after the assistant message"
        );

        // The original follow-up user message is now shifted to index 3.
        assert_eq!(messages[3].content.as_text_lossy(), "follow up");
    }

    // 11. sanitize — valid pairs unchanged
    #[test]
    fn test_sanitize_valid_pairs_unchanged() {
        let mut messages = vec![
            Message::user("hello"),
            make_assistant_with_tool_call("call-1", "bash"),
            make_tool_message("bash", "call-1", "output"),
            Message::assistant("done"),
        ];
        let original_len = messages.len();
        sanitize_tool_pairs(&mut messages);
        assert_eq!(messages.len(), original_len);
    }

    // 12. full compress with MockProvider
    #[tokio::test]
    async fn test_compress_full_with_mock() {
        let provider = MockProvider::new(vec![summary_response("This is the summary.")]);
        let mut compressor = make_compressor(300, 0.30, 0.10);

        // Build enough messages to trigger compression.
        let mut messages: Vec<Message> = Vec::new();
        // Head (3 protected)
        messages.push(Message::user("task: implement feature X"));
        messages.push(Message::assistant("ok, starting"));
        messages.push(Message::user("use Rust"));
        // Compressible middle
        for i in 0..15 {
            messages.push(Message::user(format!(
                "detail about step {i} with extra context"
            )));
            messages.push(Message::assistant(format!("acknowledged step {i}")));
        }
        // Tail (should be preserved)
        messages.push(Message::user("final instruction"));
        messages.push(Message::assistant("will do"));
        messages.push(Message::user("confirm"));

        let result = compressor.compress(&mut messages, &provider, None).await;
        let result = result.unwrap();

        match result {
            CompressionResult::Compressed {
                messages_removed,
                messages_kept,
                ..
            } => {
                assert!(messages_removed > 0);
                assert!(messages_kept > 0);
            }
            CompressionResult::NotNeeded => panic!("Expected compression to happen"),
        }

        // Verify head is preserved.
        assert_eq!(
            messages[0].content.as_text_lossy(),
            "task: implement feature X"
        );
        assert_eq!(messages[1].content.as_text_lossy(), "ok, starting");
        assert_eq!(messages[2].content.as_text_lossy(), "use Rust");

        // Verify summary is inserted as a user message with the tag.
        let summary_msg = messages
            .iter()
            .find(|m| m.content.as_text_lossy().contains("<context-summary>"));
        assert!(summary_msg.is_some());
        let summary_text = summary_msg.unwrap().content.as_text_lossy();
        assert!(summary_text.contains("This is the summary."));

        // Verify tail messages are still present.
        let last = messages.last().unwrap().content.as_text_lossy();
        assert_eq!(last, "confirm");
    }

    // 13. iterative compression preserves previous_summary
    #[tokio::test]
    async fn test_compress_iterative_summary() {
        let provider = MockProvider::new(vec![
            summary_response("First summary."),
            summary_response("Second summary."),
        ]);

        let mut compressor = make_compressor(200, 0.30, 0.10);

        // First compression round
        let mut messages: Vec<Message> = (0..20)
            .map(|i| Message::user(format!("message {i} with enough content to exceed budget")))
            .collect();
        let _ = compressor.compress(&mut messages, &provider, None).await;
        assert_eq!(
            compressor.previous_summary.as_deref(),
            Some("First summary.")
        );

        // Add more messages and compress again.
        for i in 20..40 {
            messages.push(Message::user(format!(
                "new message {i} with enough content to exceed budget"
            )));
        }
        let _ = compressor.compress(&mut messages, &provider, None).await;
        assert_eq!(
            compressor.previous_summary.as_deref(),
            Some("Second summary.")
        );

        // Verify the second summarization prompt included the previous summary.
        let captured = provider.captured_requests.lock().unwrap();
        assert!(captured.len() >= 2);
        assert!(captured[1].contains("PREVIOUS SUMMARY"));
        assert!(captured[1].contains("First summary."));
    }

    // 14. compress not needed — few messages, high threshold
    #[tokio::test]
    async fn test_compress_not_needed() {
        let provider = MockProvider::new(vec![]);
        let mut compressor = make_compressor(200_000, 0.50, 0.90);

        let mut messages = vec![
            Message::user("hi"),
            Message::assistant("hello"),
            Message::user("bye"),
        ];
        let result = compressor
            .compress(&mut messages, &provider, None)
            .await
            .unwrap();

        assert!(matches!(result, CompressionResult::NotNeeded));
    }

    // 15. compress — verify summary request shape
    #[tokio::test]
    async fn test_compress_summary_request_shape() {
        let (provider, captured) =
            MockProvider::with_capture(vec![summary_response("shape test summary")]);

        let mut compressor = make_compressor(300, 0.30, 0.10);

        let mut messages: Vec<Message> = (0..20)
            .map(|i| Message::user(format!("content for message {i} with padding text here")))
            .collect();

        let _ = compressor.compress(&mut messages, &provider, None).await;

        let requests = captured.lock().unwrap();
        assert!(!requests.is_empty(), "Provider should have been called");

        let prompt = &requests[0];
        // Verify the prompt contains the serialized conversation.
        assert!(prompt.contains("MESSAGES TO COMPRESS"));
        assert!(prompt.contains("[User]:"));
        // Verify it contains the structured preservation instructions.
        assert!(prompt.contains("Goal:"));
        assert!(prompt.contains("Progress:"));
    }
}

//! Anthropic Claude provider with SSE streaming, prompt caching, and thinking blocks.

use std::time::Duration;

use async_trait::async_trait;
use hermes_core::{
    error::{HermesError, ProviderError, Result},
    message::{Content, ContentPart, Message, Role},
    provider::{ChatRequest, ChatResponse, FinishReason, ModelInfo, Provider, TokenUsage},
    stream::StreamDelta,
    tool::ToolSchema,
};
use reqwest::{Client, Response, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::warn;

use crate::sse::SseStream;

// ─── Config ───────────────────────────────────────────────────────────────────

pub struct AnthropicConfig {
    pub base_url: String,
    pub api_key: SecretString,
    pub model: String,
    pub api_version: String,
    pub max_thinking_tokens: Option<u32>,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.anthropic.com/v1".to_string(),
            api_key: SecretString::new("".into()),
            model: "claude-opus-4-5".to_string(),
            api_version: "2023-06-01".to_string(),
            max_thinking_tokens: None,
        }
    }
}

// ─── Pending tool state during streaming ─────────────────────────────────────

#[derive(Default)]
struct PendingTool {
    id: String,
    name: String,
    input_json: String,
}

// ─── Provider ────────────────────────────────────────────────────────────────

pub struct AnthropicProvider {
    client: Client,
    config: AnthropicConfig,
    info: ModelInfo,
}

impl AnthropicProvider {
    /// Build a new provider. Creates a reqwest client with a 300-second timeout.
    pub fn new(config: AnthropicConfig, info: ModelInfo) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("failed to build reqwest client");

        Self {
            client,
            config,
            info,
        }
    }

    /// Returns auth headers.
    ///
    /// OAuth tokens (`sk-ant-oat*`) use Bearer Authorization; API keys use `x-api-key`.
    fn auth_headers(&self) -> Vec<(String, String)> {
        let key = self.config.api_key.expose_secret();
        if key.starts_with("sk-ant-oat") {
            vec![("Authorization".to_string(), format!("Bearer {key}"))]
        } else {
            vec![("x-api-key".to_string(), key.to_string())]
        }
    }

    /// Build the JSON request body for the Anthropic Messages API.
    fn build_request_body(&self, request: &ChatRequest<'_>) -> Value {
        let messages = self.convert_messages(request.messages);

        // System prompt: if cache segments are provided, emit an array of typed blocks;
        // otherwise fall back to a plain string.
        let system_value: Value = if let Some(segments) = request.system_segments {
            if segments.is_empty() {
                json!(request.system)
            } else {
                let blocks: Vec<Value> = segments
                    .iter()
                    .map(|seg| {
                        if seg.cache_control {
                            json!({
                                "type": "text",
                                "text": seg.text,
                                "cache_control": { "type": "ephemeral" },
                            })
                        } else {
                            json!({ "type": "text", "text": seg.text })
                        }
                    })
                    .collect();
                json!(blocks)
            }
        } else if !request.system.is_empty() {
            json!(request.system)
        } else {
            json!(null)
        };

        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(Self::schema_to_tool_json)
            .collect();

        let mut body = json!({
            "model": self.config.model,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "messages": messages,
            "stream": true,
        });

        if !system_value.is_null() {
            body["system"] = system_value;
        }

        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }

        if !request.stop_sequences.is_empty() {
            body["stop_sequences"] = json!(request.stop_sequences);
        }

        // Extended thinking block
        if request.reasoning {
            if let Some(budget) = self.config.max_thinking_tokens {
                body["thinking"] = json!({
                    "type": "enabled",
                    "budget_tokens": budget,
                });
            }
        }

        body
    }

    /// Convert a `ToolSchema` to the Anthropic tool JSON shape.
    fn schema_to_tool_json(schema: &ToolSchema) -> Value {
        json!({
            "name": schema.name,
            "description": schema.description,
            "input_schema": schema.parameters,
        })
    }

    /// Convert core `Message` slice into Anthropic's strict-alternating format.
    ///
    /// Rules:
    /// - `Role::System` messages in the slice are skipped (system goes in the top-level field).
    /// - `Role::User` → `{"role":"user","content":[...]}`
    /// - `Role::Assistant` → `{"role":"assistant","content":[thinking?, text, tool_use*]}`
    /// - `Role::Tool` → `{"role":"user","content":[{"type":"tool_result",...}]}`
    /// - Adjacent same-role entries are merged via `push_or_merge`.
    fn convert_messages(&self, messages: &[Message]) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();

        for msg in messages {
            match msg.role {
                Role::System => {
                    // System messages in the slice are ignored; they belong in the system field.
                }
                Role::User => {
                    let content_blocks = self.convert_content(&msg.content);
                    let entry = json!({ "role": "user", "content": content_blocks });
                    Self::push_or_merge(&mut out, entry);
                }
                Role::Assistant => {
                    let mut blocks: Vec<Value> = Vec::new();

                    // Thinking block (reasoning)
                    if let Some(thinking) = &msg.reasoning {
                        if !thinking.is_empty() {
                            blocks.push(json!({
                                "type": "thinking",
                                "thinking": thinking,
                            }));
                        }
                    }

                    // Text content
                    match &msg.content {
                        Content::Text(s) if !s.is_empty() => {
                            blocks.push(json!({ "type": "text", "text": s }));
                        }
                        Content::Parts(parts) => {
                            for part in parts {
                                match part {
                                    ContentPart::Text { text } if !text.is_empty() => {
                                        blocks.push(json!({ "type": "text", "text": text }));
                                    }
                                    ContentPart::Image { data, media_type } => {
                                        blocks.push(json!({
                                            "type": "image",
                                            "source": {
                                                "type": "base64",
                                                "media_type": media_type,
                                                "data": data,
                                            }
                                        }));
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }

                    // Tool use blocks
                    for tc in &msg.tool_calls {
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.name,
                            "input": tc.arguments,
                        }));
                    }

                    let entry = json!({ "role": "assistant", "content": blocks });
                    Self::push_or_merge(&mut out, entry);
                }
                Role::Tool => {
                    // Tool results become user messages with tool_result content blocks.
                    let result_block = json!({
                        "type": "tool_result",
                        "tool_use_id": msg.tool_call_id.as_deref().unwrap_or(""),
                        "content": msg.content.as_text_lossy(),
                    });
                    let entry = json!({ "role": "user", "content": [result_block] });
                    Self::push_or_merge(&mut out, entry);
                }
            }
        }

        out
    }

    /// Convert a `Content` value to an array of Anthropic content blocks.
    fn convert_content(&self, content: &Content) -> Vec<Value> {
        match content {
            Content::Text(s) => vec![json!({ "type": "text", "text": s })],
            Content::Parts(parts) => parts
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text } => json!({ "type": "text", "text": text }),
                    ContentPart::Image { data, media_type } => json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": media_type,
                            "data": data,
                        }
                    }),
                })
                .collect(),
        }
    }

    /// Append `msg` to `result`, merging with the last entry if they share the same role.
    ///
    /// Adjacent same-role messages are merged by extending their `content` arrays so the
    /// final output strictly alternates user/assistant as required by the Anthropic API.
    fn push_or_merge(result: &mut Vec<Value>, msg: Value) {
        let role = msg["role"].as_str().unwrap_or("").to_string();

        if let Some(last) = result.last_mut() {
            if last["role"].as_str() == Some(&role) {
                // Merge: extend the existing content array.
                if let (Some(existing), Some(new_blocks)) =
                    (last["content"].as_array_mut(), msg["content"].as_array())
                {
                    existing.extend(new_blocks.iter().cloned());
                    return;
                }
            }
        }

        result.push(msg);
    }

    /// Drive the SSE stream, emitting deltas and assembling the full response.
    async fn stream_response(
        http_response: Response,
        delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) -> Result<ChatResponse> {
        let mut sse = SseStream::new(http_response.bytes_stream());

        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tool_calls: Vec<hermes_core::message::ToolCall> = Vec::new();
        let mut usage = TokenUsage::default();
        let mut finish_reason = FinishReason::Stop;

        // Track per-block state.
        // block_index → type ("text" | "thinking" | "tool_use")
        let mut block_types: Vec<String> = Vec::new();
        // For the currently-streaming tool_use block
        let mut pending_tool: Option<PendingTool> = None;
        // Index of the active block being streamed
        let mut active_block: usize = 0;

        loop {
            let event = sse
                .next_event()
                .await
                .map_err(|e| HermesError::Provider(ProviderError::SseParse(e.to_string())))?;

            let Some(ev) = event else {
                break;
            };

            let ev_type = ev.event.as_deref().unwrap_or("");

            let parsed: Value = match serde_json::from_str(&ev.data) {
                Ok(v) => v,
                Err(e) => {
                    warn!("anthropic: failed to parse SSE JSON: {e}: {}", ev.data);
                    continue;
                }
            };

            match ev_type {
                "message_start" => {
                    // Extract initial usage (cache tokens appear here)
                    if let Some(msg) = parsed.get("message") {
                        if let Some(u) = msg.get("usage") {
                            usage.input_tokens =
                                u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0)
                                    as usize;
                            usage.cache_creation_tokens =
                                u.get("cache_creation_input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0) as usize;
                            usage.cache_read_tokens =
                                u.get("cache_read_input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0) as usize;
                        }
                    }
                }

                "content_block_start" => {
                    let index = parsed.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    active_block = index;

                    let block = parsed.get("content_block");
                    let block_type = block
                        .and_then(|b| b.get("type"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("text")
                        .to_string();

                    // Grow the block_types vec if needed
                    if block_types.len() <= index {
                        block_types.resize(index + 1, String::new());
                    }
                    block_types[index] = block_type.clone();

                    if block_type == "tool_use" {
                        let id = block
                            .and_then(|b| b.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .and_then(|b| b.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        if let Some(tx) = delta_tx {
                            let _ = tx
                                .send(StreamDelta::ToolCallStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                })
                                .await;
                        }

                        pending_tool = Some(PendingTool {
                            id,
                            name,
                            input_json: String::new(),
                        });
                    }
                }

                "content_block_delta" => {
                    let index = parsed
                        .get("index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(active_block as u64) as usize;

                    let delta = match parsed.get("delta") {
                        Some(d) => d,
                        None => continue,
                    };
                    let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");

                    let btype = block_types.get(index).map(|s| s.as_str()).unwrap_or("text");

                    match (btype, delta_type) {
                        ("thinking", "thinking_delta") => {
                            if let Some(text) = delta.get("thinking").and_then(|v| v.as_str()) {
                                if !text.is_empty() {
                                    reasoning.push_str(text);
                                    if let Some(tx) = delta_tx {
                                        let _ = tx
                                            .send(StreamDelta::ReasoningDelta(text.to_string()))
                                            .await;
                                    }
                                }
                            }
                        }
                        ("text", "text_delta") => {
                            if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                if !text.is_empty() {
                                    content.push_str(text);
                                    if let Some(tx) = delta_tx {
                                        let _ =
                                            tx.send(StreamDelta::TextDelta(text.to_string())).await;
                                    }
                                }
                            }
                        }
                        ("tool_use", "input_json_delta") => {
                            if let Some(partial) =
                                delta.get("partial_json").and_then(|v| v.as_str())
                            {
                                if !partial.is_empty() {
                                    if let Some(pt) = &mut pending_tool {
                                        let id = pt.id.clone();
                                        pt.input_json.push_str(partial);
                                        if let Some(tx) = delta_tx {
                                            let _ = tx
                                                .send(StreamDelta::ToolCallArgsDelta {
                                                    id,
                                                    delta: partial.to_string(),
                                                })
                                                .await;
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }

                "content_block_stop" => {
                    // Finalise any pending tool_use block.
                    if let Some(pt) = pending_tool.take() {
                        let arguments =
                            serde_json::from_str::<Value>(&pt.input_json).unwrap_or(json!({}));
                        tool_calls.push(hermes_core::message::ToolCall {
                            id: pt.id,
                            name: pt.name,
                            arguments,
                        });
                    }
                }

                "message_delta" => {
                    // Final token counts and stop reason
                    if let Some(u) = parsed.get("usage") {
                        usage.output_tokens =
                            u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    }
                    if let Some(reason) = parsed
                        .get("delta")
                        .and_then(|d| d.get("stop_reason"))
                        .and_then(|v| v.as_str())
                    {
                        finish_reason = match reason {
                            "end_turn" => FinishReason::Stop,
                            "tool_use" => FinishReason::ToolUse,
                            "max_tokens" => FinishReason::MaxTokens,
                            _ => FinishReason::Stop,
                        };
                    }
                }

                "message_stop" => {
                    break;
                }

                _ => {}
            }
        }

        if let Some(tx) = delta_tx {
            let _ = tx.send(StreamDelta::Done).await;
        }

        let cache_meta = if usage.cache_creation_tokens > 0 || usage.cache_read_tokens > 0 {
            Some(hermes_core::provider::CacheMeta {
                cache_creation_tokens: usage.cache_creation_tokens,
                cache_read_tokens: usage.cache_read_tokens,
            })
        } else {
            None
        };

        Ok(ChatResponse {
            content,
            tool_calls,
            reasoning: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning)
            },
            finish_reason,
            usage,
            cache_meta,
        })
    }
}

// ─── Provider impl ────────────────────────────────────────────────────────────

#[async_trait]
impl Provider for AnthropicProvider {
    async fn chat(
        &self,
        request: &ChatRequest<'_>,
        delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) -> Result<ChatResponse> {
        let url = format!("{}/messages", self.config.base_url.trim_end_matches('/'));
        let body = self.build_request_body(request);

        let mut req = self
            .client
            .post(&url)
            .header("anthropic-version", &self.config.api_version)
            .header("content-type", "application/json")
            .json(&body);

        for (key, val) in self.auth_headers() {
            req = req.header(key, val);
        }

        let response = req.send().await.map_err(|e| {
            if e.is_timeout() {
                HermesError::Provider(ProviderError::Timeout(300))
            } else {
                HermesError::Provider(ProviderError::Network(e.to_string()))
            }
        })?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(classify_anthropic_error(status, &body_text));
        }

        Self::stream_response(response, delta_tx).await
    }

    fn supports_reasoning(&self) -> bool {
        self.config.max_thinking_tokens.is_some()
    }

    fn supports_caching(&self) -> bool {
        true
    }

    fn model_info(&self) -> &ModelInfo {
        &self.info
    }
}

// ─── HTTP error classifier ────────────────────────────────────────────────────

/// Map an Anthropic HTTP status code and response body to an appropriate `HermesError`.
pub fn classify_anthropic_error(status: StatusCode, body: &str) -> HermesError {
    match status.as_u16() {
        401 => HermesError::Provider(ProviderError::AuthError),
        429 => HermesError::Provider(ProviderError::RateLimited { retry_after: None }),
        529 => HermesError::Provider(ProviderError::ApiError {
            status: 529,
            message: "overloaded".to_string(),
        }),
        _ => {
            // Check for context-length error in body
            let lower = body.to_lowercase();
            if lower.contains("context length") || lower.contains("context_length_exceeded") {
                return HermesError::Provider(ProviderError::ContextLengthExceeded {
                    used: 0,
                    max: 0,
                });
            }
            let message = extract_anthropic_error_message(body);
            HermesError::Provider(ProviderError::ApiError {
                status: status.as_u16(),
                message,
            })
        }
    }
}

fn extract_anthropic_error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| body.chars().take(256).collect())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use hermes_core::{
        error::ProviderError,
        message::{Content, Message, Role, ToolCall},
        provider::{ModelInfo, ModelPricing},
    };
    use reqwest::StatusCode;
    use secrecy::SecretString;

    use super::*;

    fn dummy_info() -> ModelInfo {
        ModelInfo {
            id: "claude-opus-4-5".to_string(),
            provider: "anthropic".to_string(),
            max_context: 200_000,
            max_output: 8192,
            supports_tools: true,
            supports_vision: true,
            supports_reasoning: true,
            supports_caching: true,
            pricing: ModelPricing {
                input_per_mtok: 15.0,
                output_per_mtok: 75.0,
                cache_read_per_mtok: 1.5,
                cache_create_per_mtok: 18.75,
            },
        }
    }

    fn make_provider() -> AnthropicProvider {
        let config = AnthropicConfig {
            base_url: "https://api.anthropic.com/v1".to_string(),
            api_key: SecretString::new("sk-ant-api-test".into()),
            model: "claude-opus-4-5".to_string(),
            api_version: "2023-06-01".to_string(),
            max_thinking_tokens: Some(10_000),
        };
        AnthropicProvider::new(config, dummy_info())
    }

    // ── convert_messages: strict alternation ─────────────────────────────────

    #[test]
    fn test_convert_messages_strict_alternation() {
        let provider = make_provider();

        // Assistant with a tool_use
        let mut assistant_msg = Message::assistant("");
        assistant_msg.tool_calls = vec![
            ToolCall {
                id: "call_1".to_string(),
                name: "tool_a".to_string(),
                arguments: serde_json::json!({"a": 1}),
            },
            ToolCall {
                id: "call_2".to_string(),
                name: "tool_b".to_string(),
                arguments: serde_json::json!({"b": 2}),
            },
        ];

        // Two consecutive tool result messages → must be merged into one user message
        let tool_result_1 = Message {
            role: Role::Tool,
            content: Content::Text("result_a".to_string()),
            tool_calls: vec![],
            reasoning: None,
            name: None,
            tool_call_id: Some("call_1".to_string()),
        };
        let tool_result_2 = Message {
            role: Role::Tool,
            content: Content::Text("result_b".to_string()),
            tool_calls: vec![],
            reasoning: None,
            name: None,
            tool_call_id: Some("call_2".to_string()),
        };

        let messages = vec![
            Message::user("Do both tools"),
            assistant_msg,
            tool_result_1,
            tool_result_2,
        ];

        let result = provider.convert_messages(&messages);

        // Must be exactly 3 messages: user, assistant, user (merged tool results)
        assert_eq!(result.len(), 3, "expected strict alternation: u/a/u");

        // The last message should be a user message with two tool_result blocks
        let last = &result[2];
        assert_eq!(last["role"], "user");
        let blocks = last["content"].as_array().unwrap();
        assert_eq!(
            blocks.len(),
            2,
            "two tool results merged into one user message"
        );
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "call_1");
        assert_eq!(blocks[1]["type"], "tool_result");
        assert_eq!(blocks[1]["tool_use_id"], "call_2");
    }

    // ── convert_messages: thinking block preserved ───────────────────────────

    #[test]
    fn test_convert_messages_with_thinking() {
        let provider = make_provider();

        let mut assistant_msg = Message::assistant("My answer");
        assistant_msg.reasoning = Some("I need to think carefully...".to_string());

        let messages = vec![Message::user("Question?"), assistant_msg];
        let result = provider.convert_messages(&messages);

        assert_eq!(result.len(), 2);
        let blocks = result[1]["content"].as_array().unwrap();

        // First block must be "thinking"
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "I need to think carefully...");

        // Second block is text
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "My answer");
    }

    // ── classify_anthropic_error: overloaded (529) ───────────────────────────

    #[test]
    fn test_classify_anthropic_error_overloaded() {
        let err = classify_anthropic_error(
            StatusCode::from_u16(529).unwrap(),
            r#"{"error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        );
        match err {
            HermesError::Provider(ProviderError::ApiError {
                status,
                ref message,
            }) => {
                assert_eq!(status, 529);
                assert_eq!(message, "overloaded");
            }
            other => panic!("expected ApiError(529), got {other:?}"),
        }
    }

    // ── classify_anthropic_error: context length ─────────────────────────────

    #[test]
    fn test_classify_anthropic_error_context_length() {
        let body = r#"{"error":{"type":"invalid_request_error","message":"context length exceeded maximum"}}"#;
        let err = classify_anthropic_error(StatusCode::BAD_REQUEST, body);
        assert!(
            matches!(
                err,
                HermesError::Provider(ProviderError::ContextLengthExceeded { .. })
            ),
            "expected ContextLengthExceeded, got {err:?}"
        );
    }

    // ── auth_header: API key ──────────────────────────────────────────────────

    #[test]
    fn test_auth_header_api_key() {
        let config = AnthropicConfig {
            api_key: SecretString::new("sk-ant-api-03-testkey".into()),
            ..AnthropicConfig::default()
        };
        let provider = AnthropicProvider::new(config, dummy_info());
        let headers = provider.auth_headers();

        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "x-api-key");
        assert_eq!(headers[0].1, "sk-ant-api-03-testkey");
    }

    // ── auth_header: OAuth ────────────────────────────────────────────────────

    #[test]
    fn test_auth_header_oauth() {
        let config = AnthropicConfig {
            api_key: SecretString::new("sk-ant-oat01-myoauthtoken".into()),
            ..AnthropicConfig::default()
        };
        let provider = AnthropicProvider::new(config, dummy_info());
        let headers = provider.auth_headers();

        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Authorization");
        assert_eq!(headers[0].1, "Bearer sk-ant-oat01-myoauthtoken");
    }
}

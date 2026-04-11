//! OpenAI-compatible provider with SSE streaming support.

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

use crate::{sse::SseStream, tool_assembler::ToolCallAssembler};

// ─── Auth style ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum AuthStyle {
    Bearer,
    AzureApiKey,
}

// ─── Config ───────────────────────────────────────────────────────────────────

pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: SecretString,
    pub model: String,
    pub org_id: Option<String>,
    pub auth_style: AuthStyle,
}

// ─── Provider ────────────────────────────────────────────────────────────────

pub struct OpenAiProvider {
    client: Client,
    config: OpenAiConfig,
    info: ModelInfo,
}

impl OpenAiProvider {
    /// Build a new provider. Creates a reqwest client with a 300-second timeout.
    pub fn new(config: OpenAiConfig, info: ModelInfo) -> hermes_core::error::Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| HermesError::Config(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            config,
            info,
        })
    }

    /// Returns auth and org headers for the configured auth style.
    fn auth_headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![];

        match self.config.auth_style {
            AuthStyle::Bearer => {
                headers.push((
                    "Authorization".to_string(),
                    format!("Bearer {}", self.config.api_key.expose_secret()),
                ));
            }
            AuthStyle::AzureApiKey => {
                headers.push((
                    "api-key".to_string(),
                    self.config.api_key.expose_secret().to_string(),
                ));
            }
        }

        if let Some(org) = &self.config.org_id {
            headers.push(("OpenAI-Organization".to_string(), org.clone()));
        }

        headers
    }

    /// Build the JSON request body for the Chat Completions API.
    fn build_request_body(&self, request: &ChatRequest<'_>) -> Value {
        let messages = self.convert_messages(request.system, request.messages);

        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(Self::schema_to_tool_json)
            .collect();

        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "stream": true,
            "stream_options": { "include_usage": true },
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }

        if !request.stop_sequences.is_empty() {
            body["stop"] = json!(request.stop_sequences);
        }

        body
    }

    /// Convert a `ToolSchema` to the OpenAI function-calling JSON shape.
    fn schema_to_tool_json(schema: &ToolSchema) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": schema.name,
                "description": schema.description,
                "parameters": schema.parameters,
            }
        })
    }

    /// Build the messages array, prepending the system prompt if non-empty.
    fn convert_messages(&self, system: &str, messages: &[Message]) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::with_capacity(messages.len() + 1);

        if !system.is_empty() {
            out.push(json!({ "role": "system", "content": system }));
        }

        for msg in messages {
            match msg.role {
                Role::System => {
                    out.push(json!({
                        "role": "system",
                        "content": Self::convert_content(&msg.content),
                    }));
                }
                Role::User => {
                    out.push(json!({
                        "role": "user",
                        "content": Self::convert_content(&msg.content),
                    }));
                }
                Role::Assistant => {
                    let mut entry = json!({
                        "role": "assistant",
                        "content": Self::convert_content(&msg.content),
                    });

                    if !msg.tool_calls.is_empty() {
                        let tc: Vec<Value> = msg
                            .tool_calls
                            .iter()
                            .map(|tc| {
                                json!({
                                    "id": tc.id,
                                    "type": "function",
                                    "function": {
                                        "name": tc.name,
                                        "arguments": tc.arguments.to_string(),
                                    }
                                })
                            })
                            .collect();
                        entry["tool_calls"] = json!(tc);
                    }

                    out.push(entry);
                }
                Role::Tool => {
                    out.push(json!({
                        "role": "tool",
                        "tool_call_id": msg.tool_call_id.as_deref().unwrap_or(""),
                        "content": Self::convert_content(&msg.content),
                    }));
                }
            }
        }

        out
    }

    /// Convert a `Content` value to the appropriate JSON form.
    ///
    /// - `Content::Text` → plain JSON string
    /// - `Content::Parts` → array of `{type, text}` / `{type, image_url}` objects
    fn convert_content(content: &Content) -> Value {
        match content {
            Content::Text(s) => json!(s),
            Content::Parts(parts) => {
                let arr: Vec<Value> = parts
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text { text } => json!({ "type": "text", "text": text }),
                        ContentPart::Image { data, media_type } => json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{media_type};base64,{data}"),
                            }
                        }),
                    })
                    .collect();
                json!(arr)
            }
        }
    }

    /// Drive the SSE stream, emitting deltas and assembling the full response.
    async fn stream_response(
        http_response: Response,
        delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) -> Result<ChatResponse> {
        let mut sse = SseStream::new(http_response.bytes_stream());
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut assembler = ToolCallAssembler::new();
        let mut usage = TokenUsage::default();
        let mut finish_reason = FinishReason::Stop;

        loop {
            let event = sse
                .next_event()
                .await
                .map_err(|e| HermesError::Provider(ProviderError::SseParse(e.to_string())))?;

            let Some(ev) = event else {
                break;
            };

            let parsed: Value = match serde_json::from_str(&ev.data) {
                Ok(v) => v,
                Err(e) => {
                    warn!("openai: failed to parse SSE JSON: {e}: {}", ev.data);
                    continue;
                }
            };

            // Usage chunk (stream_options includes a final usage event)
            if let Some(usage_obj) = parsed.get("usage").filter(|v| !v.is_null()) {
                usage.input_tokens = usage_obj
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                usage.output_tokens = usage_obj
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                // completion_tokens_details may carry reasoning_tokens
                if let Some(details) = usage_obj.get("completion_tokens_details") {
                    usage.reasoning_tokens = details
                        .get("reasoning_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize;
                }
            }

            let Some(choices) = parsed.get("choices").and_then(|v| v.as_array()) else {
                continue;
            };

            let Some(choice) = choices.first() else {
                continue;
            };

            // finish_reason
            if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                finish_reason = match fr {
                    "stop" => FinishReason::Stop,
                    "tool_calls" => FinishReason::ToolUse,
                    "length" => FinishReason::MaxTokens,
                    "content_filter" => FinishReason::ContentFilter,
                    _ => FinishReason::Stop,
                };
            }

            let Some(delta) = choice.get("delta") else {
                continue;
            };

            // Text delta
            if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    content.push_str(text);
                    if let Some(tx) = delta_tx {
                        let _ = tx.send(StreamDelta::TextDelta(text.to_string())).await;
                    }
                }
            }

            // Reasoning delta (o-series models use "reasoning_content")
            if let Some(text) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    reasoning.push_str(text);
                    if let Some(tx) = delta_tx {
                        let _ = tx.send(StreamDelta::ReasoningDelta(text.to_string())).await;
                    }
                }
            }

            // Tool-call deltas
            if let Some(tc_arr) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tc_arr {
                    let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

                    // If this delta starts a new tool call
                    if let Some(fn_obj) = tc.get("function") {
                        let name = fn_obj
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let id = tc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        if !name.is_empty() {
                            assembler.start(index, &id, &name);
                            if let Some(tx) = delta_tx {
                                let _ = tx
                                    .send(StreamDelta::ToolCallStart {
                                        id: id.clone(),
                                        name,
                                    })
                                    .await;
                            }
                        }

                        // Argument fragment
                        if let Some(args_delta) = fn_obj.get("arguments").and_then(|v| v.as_str()) {
                            if !args_delta.is_empty() {
                                // We need the id for the delta event; look it up from id field
                                let call_id = tc
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                assembler.append_arguments(index, args_delta);
                                if let Some(tx) = delta_tx {
                                    let _ = tx
                                        .send(StreamDelta::ToolCallArgsDelta {
                                            id: call_id,
                                            delta: args_delta.to_string(),
                                        })
                                        .await;
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some(tx) = delta_tx {
            let _ = tx.send(StreamDelta::Done).await;
        }

        let tool_calls = assembler.finish();
        if !tool_calls.is_empty() && finish_reason == FinishReason::Stop {
            finish_reason = FinishReason::ToolUse;
        }

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
            cache_meta: None,
        })
    }
}

// ─── Provider impl ────────────────────────────────────────────────────────────

impl OpenAiProvider {
    /// Single attempt at the Chat Completions API. Used by the retry loop in `chat()`.
    async fn try_chat(
        &self,
        request: &ChatRequest<'_>,
        delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) -> Result<ChatResponse> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let body = self.build_request_body(request);

        let mut req = self.client.post(&url).json(&body);

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
            return Err(classify_http_error(status, &body_text));
        }

        Self::stream_response(response, delta_tx).await
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn chat(
        &self,
        request: &ChatRequest<'_>,
        delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) -> Result<ChatResponse> {
        use crate::retry::{RetryAction, RetryPolicy};

        let policy = RetryPolicy::default();
        let mut last_error: Option<HermesError> = None;

        for attempt in 0..=policy.max_retries {
            if attempt > 0 {
                if let Some(ref err) = last_error {
                    match policy.should_retry(err, attempt - 1, None) {
                        RetryAction::RetryAfter(delay) => {
                            tracing::warn!(attempt, ?delay, "openai: retrying after error");
                            tokio::time::sleep(delay).await;
                        }
                        RetryAction::DoNotRetry => return Err(last_error.unwrap()),
                    }
                }
            }

            match self.try_chat(request, delta_tx).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap())
    }

    fn supports_tool_calling(&self) -> bool {
        self.info.supports_tools
    }

    fn supports_reasoning(&self) -> bool {
        self.info.supports_reasoning
    }

    fn supports_caching(&self) -> bool {
        self.info.supports_caching
    }

    fn model_info(&self) -> &ModelInfo {
        &self.info
    }
}

// ─── HTTP error classifier ────────────────────────────────────────────────────

/// Map an HTTP status code and response body to an appropriate `HermesError`.
pub fn classify_http_error(status: StatusCode, body: &str) -> HermesError {
    let message = extract_error_message(body);

    match status.as_u16() {
        401 | 403 => HermesError::Provider(ProviderError::AuthError),
        429 => {
            // Try to parse retry-after from the body
            let retry_after = serde_json::from_str::<Value>(body).ok().and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("retry_after"))
                    .and_then(|v| v.as_f64())
            });
            HermesError::Provider(ProviderError::RateLimited { retry_after })
        }
        404 => HermesError::Provider(ProviderError::ModelNotFound(message)),
        _ => HermesError::Provider(ProviderError::ApiError {
            status: status.as_u16(),
            message,
        }),
    }
}

fn extract_error_message(body: &str) -> String {
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
            id: "gpt-4o".to_string(),
            provider: "openai".to_string(),
            max_context: 128_000,
            max_output: 4096,
            supports_tools: true,
            supports_vision: true,
            supports_reasoning: false,
            supports_caching: false,
            pricing: ModelPricing {
                input_per_mtok: 5.0,
                output_per_mtok: 15.0,
                cache_read_per_mtok: 0.5,
                cache_create_per_mtok: 1.25,
            },
        }
    }

    fn make_provider() -> OpenAiProvider {
        let config = OpenAiConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: SecretString::new("sk-test".into()),
            model: "gpt-4o".to_string(),
            org_id: None,
            auth_style: AuthStyle::Bearer,
        };
        OpenAiProvider::new(config, dummy_info()).expect("failed to build test provider")
    }

    // ── classify_http_error ───────────────────────────────────────────────────

    #[test]
    fn test_classify_http_error_401() {
        let err = classify_http_error(StatusCode::UNAUTHORIZED, "");
        assert!(
            matches!(err, HermesError::Provider(ProviderError::AuthError)),
            "expected AuthError, got {err:?}"
        );
    }

    #[test]
    fn test_classify_http_error_429() {
        let err = classify_http_error(StatusCode::TOO_MANY_REQUESTS, "");
        assert!(
            matches!(
                err,
                HermesError::Provider(ProviderError::RateLimited { .. })
            ),
            "expected RateLimited, got {err:?}"
        );
    }

    #[test]
    fn test_classify_http_error_500() {
        let body = r#"{"error":{"message":"Internal server error"}}"#;
        let err = classify_http_error(StatusCode::INTERNAL_SERVER_ERROR, body);
        match err {
            HermesError::Provider(ProviderError::ApiError { status, message }) => {
                assert_eq!(status, 500);
                assert_eq!(message, "Internal server error");
            }
            other => panic!("expected ApiError, got {other:?}"),
        }
    }

    // ── convert_messages ─────────────────────────────────────────────────────

    #[test]
    fn test_convert_messages_system_first() {
        let provider = make_provider();
        let messages = vec![Message::user("Hello"), Message::assistant("Hi there")];
        let result = provider.convert_messages("You are helpful.", &messages);

        assert_eq!(result.len(), 3, "system + 2 messages");
        assert_eq!(result[0]["role"], "system");
        assert_eq!(result[0]["content"], "You are helpful.");
        assert_eq!(result[1]["role"], "user");
        assert_eq!(result[1]["content"], "Hello");
        assert_eq!(result[2]["role"], "assistant");
        assert_eq!(result[2]["content"], "Hi there");
    }

    #[test]
    fn test_convert_messages_with_tool_calls_and_results() {
        let provider = make_provider();

        // Assistant message that triggered a tool call
        let mut assistant_msg = Message::assistant("");
        assistant_msg.tool_calls = vec![ToolCall {
            id: "call_123".to_string(),
            name: "get_weather".to_string(),
            arguments: serde_json::json!({"location": "Paris"}),
        }];

        // Tool result message
        let tool_msg = Message {
            role: Role::Tool,
            content: Content::Text(r#"{"temp": 20}"#.to_string()),
            tool_calls: vec![],
            reasoning: None,
            name: None,
            tool_call_id: Some("call_123".to_string()),
        };
        // suppress unused warning
        let _ = &tool_msg;

        let messages = vec![
            Message::user("What's the weather?"),
            assistant_msg,
            tool_msg,
        ];

        let result = provider.convert_messages("", &messages);

        // No system prompt inserted because system is empty
        assert_eq!(result.len(), 3);

        let assistant = &result[1];
        assert_eq!(assistant["role"], "assistant");
        let tcs = assistant["tool_calls"].as_array().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["id"], "call_123");
        assert_eq!(tcs[0]["function"]["name"], "get_weather");

        let tool = &result[2];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "call_123");
        assert_eq!(tool["content"], r#"{"temp": 20}"#);
    }
}

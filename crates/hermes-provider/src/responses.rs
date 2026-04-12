//! OpenAI Responses API provider with SSE streaming support.

use std::{collections::HashMap, time::Duration};

use async_trait::async_trait;
use hermes_core::{
    error::{HermesError, ProviderError, Result},
    message::{Content, ContentPart, Message, Role, ToolCall},
    provider::{ChatRequest, ChatResponse, FinishReason, ModelInfo, Provider, TokenUsage},
    stream::StreamDelta,
    tool::ToolSchema,
};
use reqwest::{Client, Response};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::warn;

use crate::{
    openai::{AuthStyle, OpenAiConfig, classify_http_error},
    retry::{RetryAction, RetryPolicy},
    sse::SseStream,
};

pub struct ResponsesProvider {
    client: Client,
    config: OpenAiConfig,
    info: ModelInfo,
}

impl ResponsesProvider {
    pub fn new(config: OpenAiConfig, info: ModelInfo) -> Result<Self> {
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

    fn build_request_body(&self, request: &ChatRequest<'_>) -> Value {
        let input = self.convert_messages(request.messages);
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(Self::schema_to_tool_json)
            .collect();

        let mut body = json!({
            "model": self.config.model,
            "input": input,
            "max_output_tokens": request.max_tokens,
            "temperature": request.temperature,
            "stream": true,
            "store": false,
            "text": { "format": { "type": "text" } },
        });

        if !request.system.is_empty() {
            body["instructions"] = json!(request.system);
        }

        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["parallel_tool_calls"] = json!(true);
            body["tool_choice"] = json!("auto");
        }

        if request.reasoning {
            body["reasoning"] = json!({ "effort": "medium" });
        }

        if !request.stop_sequences.is_empty() {
            warn!("responses: stop sequences are not currently mapped; ignoring");
        }

        body
    }

    fn schema_to_tool_json(schema: &ToolSchema) -> Value {
        json!({
            "type": "function",
            "name": schema.name,
            "description": schema.description,
            "parameters": schema.parameters,
            "strict": true,
        })
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<Value> {
        let mut out = Vec::new();

        for msg in messages {
            match msg.role {
                Role::System => out.push(json!({
                    "role": "system",
                    "content": Self::convert_message_content(&msg.content),
                })),
                Role::User => out.push(json!({
                    "role": "user",
                    "content": Self::convert_message_content(&msg.content),
                })),
                Role::Assistant => {
                    if Self::message_has_content(&msg.content) {
                        out.push(json!({
                            "role": "assistant",
                            "content": Self::convert_message_content(&msg.content),
                        }));
                    }

                    for tool_call in &msg.tool_calls {
                        out.push(json!({
                            "type": "function_call",
                            "call_id": tool_call.id,
                            "name": tool_call.name,
                            "arguments": tool_call.arguments.to_string(),
                        }));
                    }
                }
                Role::Tool => {
                    if let Some(call_id) = &msg.tool_call_id {
                        out.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": Self::convert_tool_output_content(&msg.content),
                        }));
                    }
                }
            }
        }

        out
    }

    fn message_has_content(content: &Content) -> bool {
        match content {
            Content::Text(text) => !text.is_empty(),
            Content::Parts(parts) => !parts.is_empty(),
        }
    }

    fn convert_message_content(content: &Content) -> Value {
        match content {
            Content::Text(text) => json!(text),
            Content::Parts(parts) => json!(Self::convert_parts(parts)),
        }
    }

    fn convert_tool_output_content(content: &Content) -> Value {
        match content {
            Content::Text(text) => json!(text),
            Content::Parts(parts) => json!(Self::convert_parts(parts)),
        }
    }

    fn convert_parts(parts: &[ContentPart]) -> Vec<Value> {
        parts
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => json!({
                    "type": "input_text",
                    "text": text,
                }),
                ContentPart::Image { data, media_type } => json!({
                    "type": "input_image",
                    "image_url": format!("data:{media_type};base64,{data}"),
                }),
            })
            .collect()
    }

    async fn maybe_emit_tool_start(
        parsed: &Value,
        tool_ids: &mut HashMap<usize, String>,
        delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) {
        let Some(item) = parsed.get("item") else {
            return;
        };
        if item.get("type").and_then(|v| v.as_str()) != Some("function_call") {
            return;
        }

        let output_index = parsed
            .get("output_index")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let call_id = item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let entry = tool_ids.entry(output_index);
        let stream_id = match entry {
            std::collections::hash_map::Entry::Occupied(existing) => existing.get().clone(),
            std::collections::hash_map::Entry::Vacant(vacant) => {
                let id = if call_id.is_empty() {
                    format!("responses-tool-{output_index}")
                } else {
                    call_id
                };
                vacant.insert(id.clone());
                if let Some(tx) = delta_tx {
                    let _ = tx
                        .send(StreamDelta::ToolCallStart {
                            id: id.clone(),
                            name,
                        })
                        .await;
                }
                id
            }
        };

        if let Some(args) = item.get("arguments").and_then(|v| v.as_str()) {
            if !args.is_empty() {
                if let Some(tx) = delta_tx {
                    let _ = tx
                        .send(StreamDelta::ToolCallArgsDelta {
                            id: stream_id,
                            delta: args.to_string(),
                        })
                        .await;
                }
            }
        }
    }

    async fn maybe_emit_tool_arguments(
        parsed: &Value,
        tool_ids: &HashMap<usize, String>,
        delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) {
        let Some(delta) = parsed.get("delta").and_then(|v| v.as_str()) else {
            return;
        };
        if delta.is_empty() {
            return;
        }

        let output_index = parsed
            .get("output_index")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let Some(id) = tool_ids.get(&output_index) else {
            return;
        };

        if let Some(tx) = delta_tx {
            let _ = tx
                .send(StreamDelta::ToolCallArgsDelta {
                    id: id.clone(),
                    delta: delta.to_string(),
                })
                .await;
        }
    }

    fn parse_response_object(response: &Value, streamed_reasoning: &str) -> ChatResponse {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tool_calls = Vec::new();

        for item in response
            .get("output")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            match item.get("type").and_then(|v| v.as_str()) {
                Some("message")
                    if item.get("role").and_then(|v| v.as_str()) == Some("assistant") =>
                {
                    for part in item
                        .get("content")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten()
                    {
                        match part.get("type").and_then(|v| v.as_str()) {
                            Some("output_text") => {
                                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                    content.push_str(text);
                                }
                            }
                            Some("refusal") => {
                                if let Some(text) = part
                                    .get("refusal")
                                    .or_else(|| part.get("text"))
                                    .and_then(|v| v.as_str())
                                {
                                    content.push_str(text);
                                }
                            }
                            Some("reasoning_text") | Some("reasoning_summary_text") => {
                                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                    reasoning.push_str(text);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Some("function_call") => {
                    let Some(name) = item.get("name").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    let raw_arguments =
                        item.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                    let id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(|v| v.as_str())
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| format!("responses-tool-{}", tool_calls.len()));
                    tool_calls.push(ToolCall {
                        id,
                        name: name.to_string(),
                        arguments: Self::parse_arguments(raw_arguments),
                    });
                }
                _ => {}
            }
        }

        if reasoning.is_empty() && !streamed_reasoning.is_empty() {
            reasoning.push_str(streamed_reasoning);
        }

        let usage = TokenUsage {
            input_tokens: response
                .get("usage")
                .and_then(|v| v.get("input_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
            output_tokens: response
                .get("usage")
                .and_then(|v| v.get("output_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            reasoning_tokens: response
                .get("usage")
                .and_then(|v| v.get("output_tokens_details"))
                .and_then(|v| v.get("reasoning_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
        };

        let finish_reason = if !tool_calls.is_empty() {
            FinishReason::ToolUse
        } else {
            match response
                .get("incomplete_details")
                .and_then(|v| v.get("reason"))
                .and_then(|v| v.as_str())
            {
                Some("max_output_tokens") | Some("max_tokens") => FinishReason::MaxTokens,
                Some("content_filter") => FinishReason::ContentFilter,
                _ => FinishReason::Stop,
            }
        };

        ChatResponse {
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
        }
    }

    fn parse_arguments(raw_arguments: &str) -> Value {
        match serde_json::from_str::<Value>(raw_arguments) {
            Ok(value) => value,
            Err(err) => json!({
                "_raw": raw_arguments,
                "_error": err.to_string(),
            }),
        }
    }

    fn stream_failure_error(response: &Value) -> HermesError {
        let message = response
            .get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("responses request failed")
            .to_string();
        HermesError::Provider(ProviderError::ApiError {
            status: 500,
            message,
        })
    }

    async fn stream_response(
        http_response: Response,
        delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) -> Result<ChatResponse> {
        let mut sse = SseStream::new(http_response.bytes_stream());
        let mut tool_ids = HashMap::new();
        let mut streamed_reasoning = String::new();
        let mut completed_response: Option<Value> = None;
        let mut stream_error: Option<HermesError> = None;

        loop {
            let event = sse
                .next_event()
                .await
                .map_err(|e| HermesError::Provider(ProviderError::SseParse(e.to_string())))?;

            let Some(ev) = event else {
                break;
            };

            let parsed: Value = match serde_json::from_str(&ev.data) {
                Ok(value) => value,
                Err(err) => {
                    warn!("responses: failed to parse SSE JSON: {err}: {}", ev.data);
                    continue;
                }
            };

            match parsed.get("type").and_then(|v| v.as_str()) {
                Some("response.output_text.delta") | Some("response.refusal.delta") => {
                    if let Some(text) = parsed.get("delta").and_then(|v| v.as_str()) {
                        if let Some(tx) = delta_tx {
                            let _ = tx.send(StreamDelta::TextDelta(text.to_string())).await;
                        }
                    }
                }
                Some("response.reasoning_text.delta")
                | Some("response.reasoning_summary_text.delta") => {
                    if let Some(text) = parsed.get("delta").and_then(|v| v.as_str()) {
                        streamed_reasoning.push_str(text);
                        if let Some(tx) = delta_tx {
                            let _ = tx.send(StreamDelta::ReasoningDelta(text.to_string())).await;
                        }
                    }
                }
                Some("response.output_item.added") | Some("response.output_item.done") => {
                    Self::maybe_emit_tool_start(&parsed, &mut tool_ids, delta_tx).await;
                }
                Some("response.function_call_arguments.delta") => {
                    Self::maybe_emit_tool_arguments(&parsed, &tool_ids, delta_tx).await;
                }
                Some("response.completed") | Some("response.incomplete") => {
                    completed_response = parsed.get("response").cloned();
                }
                Some("response.failed") => {
                    if let Some(response) = parsed.get("response") {
                        stream_error = Some(Self::stream_failure_error(response));
                    }
                }
                _ => {}
            }
        }

        if let Some(err) = stream_error {
            return Err(err);
        }

        let Some(response) = completed_response else {
            return Err(HermesError::Provider(ProviderError::ApiError {
                status: 500,
                message: "responses stream ended without a completed response".to_string(),
            }));
        };

        if let Some(tx) = delta_tx {
            let _ = tx.send(StreamDelta::Done).await;
        }

        Ok(Self::parse_response_object(&response, &streamed_reasoning))
    }

    async fn try_chat(
        &self,
        request: &ChatRequest<'_>,
        delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) -> Result<ChatResponse> {
        let url = format!("{}/responses", self.config.base_url.trim_end_matches('/'));
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
impl Provider for ResponsesProvider {
    async fn chat(
        &self,
        request: &ChatRequest<'_>,
        delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) -> Result<ChatResponse> {
        let policy = RetryPolicy::default();
        let mut last_error: Option<HermesError> = None;

        for attempt in 0..=policy.max_retries {
            if attempt > 0 {
                if let Some(ref err) = last_error {
                    match policy.should_retry(err, attempt - 1, None) {
                        RetryAction::RetryAfter(delay) => {
                            tracing::warn!(attempt, ?delay, "responses: retrying after error");
                            tokio::time::sleep(delay).await;
                        }
                        RetryAction::DoNotRetry => return Err(last_error.unwrap()),
                    }
                }
            }

            match self.try_chat(request, delta_tx).await {
                Ok(response) => return Ok(response),
                Err(err) => last_error = Some(err),
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

#[cfg(test)]
mod tests {
    use secrecy::SecretString;

    use hermes_core::{
        message::{Content, Message, Role},
        provider::ModelPricing,
    };

    use super::*;

    fn dummy_info() -> ModelInfo {
        ModelInfo {
            id: "gpt-5".to_string(),
            provider: "openai-codex".to_string(),
            max_context: 128_000,
            max_output: 16_384,
            supports_tools: true,
            supports_vision: true,
            supports_reasoning: true,
            supports_caching: false,
            pricing: ModelPricing {
                input_per_mtok: 5.0,
                output_per_mtok: 15.0,
                cache_read_per_mtok: 0.0,
                cache_create_per_mtok: 0.0,
            },
        }
    }

    fn make_provider() -> ResponsesProvider {
        let config = OpenAiConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: SecretString::new("sk-test".into()),
            model: "gpt-5".to_string(),
            org_id: None,
            auth_style: AuthStyle::Bearer,
        };
        ResponsesProvider::new(config, dummy_info()).expect("failed to build test provider")
    }

    #[test]
    fn convert_messages_maps_tool_calls_and_outputs() {
        let provider = make_provider();

        let mut assistant = Message::assistant("Let me check.");
        assistant.tool_calls = vec![ToolCall {
            id: "call_weather".to_string(),
            name: "get_weather".to_string(),
            arguments: json!({"location": "Paris"}),
        }];
        let tool = Message {
            role: Role::Tool,
            content: Content::Text(r#"{"temp_c": 20}"#.to_string()),
            tool_calls: vec![],
            reasoning: None,
            name: None,
            tool_call_id: Some("call_weather".to_string()),
        };

        let converted = provider.convert_messages(&[Message::user("Weather?"), assistant, tool]);
        assert_eq!(converted.len(), 4);
        assert_eq!(converted[0]["role"], "user");
        assert_eq!(converted[1]["role"], "assistant");
        assert_eq!(converted[1]["content"], "Let me check.");
        assert_eq!(converted[2]["type"], "function_call");
        assert_eq!(converted[2]["call_id"], "call_weather");
        assert_eq!(converted[2]["name"], "get_weather");
        assert_eq!(converted[3]["type"], "function_call_output");
        assert_eq!(converted[3]["call_id"], "call_weather");
        assert_eq!(converted[3]["output"], r#"{"temp_c": 20}"#);
    }

    #[test]
    fn build_request_body_uses_responses_fields() {
        let provider = make_provider();
        let tool = ToolSchema {
            name: "get_weather".to_string(),
            description: "Fetch the weather".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "location": {"type": "string"}
                },
                "required": ["location"]
            }),
        };
        let request = ChatRequest {
            system: "You are helpful.",
            system_segments: None,
            messages: &[Message::user("Hello")],
            tools: &[tool],
            max_tokens: 512,
            temperature: 0.2,
            reasoning: true,
            stop_sequences: vec![],
        };

        let body = provider.build_request_body(&request);
        assert_eq!(body["model"], "gpt-5");
        assert_eq!(body["instructions"], "You are helpful.");
        assert_eq!(body["max_output_tokens"], 512);
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "get_weather");
        assert_eq!(body["reasoning"]["effort"], "medium");
    }

    #[test]
    fn parse_response_object_extracts_text_and_usage() {
        let response = json!({
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "Hello from responses",
                    "annotations": []
                }]
            }],
            "usage": {
                "input_tokens": 12,
                "output_tokens": 7,
                "output_tokens_details": {
                    "reasoning_tokens": 3
                }
            }
        });

        let parsed = ResponsesProvider::parse_response_object(&response, "");
        assert_eq!(parsed.content, "Hello from responses");
        assert_eq!(parsed.finish_reason, FinishReason::Stop);
        assert_eq!(parsed.usage.input_tokens, 12);
        assert_eq!(parsed.usage.output_tokens, 7);
        assert_eq!(parsed.usage.cache_creation_tokens, 0);
        assert_eq!(parsed.usage.cache_read_tokens, 0);
        assert_eq!(parsed.usage.reasoning_tokens, 3);
    }

    #[test]
    fn parse_response_object_extracts_function_calls() {
        let response = json!({
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "call_123",
                "name": "get_weather",
                "arguments": "{\"location\":\"Paris\"}"
            }],
            "usage": {
                "input_tokens": 5,
                "output_tokens": 3,
                "output_tokens_details": {
                    "reasoning_tokens": 0
                }
            }
        });

        let parsed = ResponsesProvider::parse_response_object(&response, "");
        assert_eq!(parsed.finish_reason, FinishReason::ToolUse);
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id, "call_123");
        assert_eq!(parsed.tool_calls[0].name, "get_weather");
        assert_eq!(parsed.tool_calls[0].arguments, json!({"location": "Paris"}));
    }
}

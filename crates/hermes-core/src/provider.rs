use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::Result;
use crate::message::{Message, ToolCall};
use crate::stream::StreamDelta;
use crate::tool::ToolSchema;

pub struct ChatRequest<'a> {
    pub system: &'a str,
    pub system_segments: Option<&'a [CacheSegment]>,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSchema],
    pub max_tokens: u32,
    pub temperature: f32,
    pub reasoning: bool,
    pub stop_sequences: Vec<String>,
}

#[derive(Clone)]
pub struct CacheSegment {
    pub text: String,
    pub label: &'static str,
    pub cache_control: bool,
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub reasoning: Option<String>,
    pub finish_reason: FinishReason,
    pub usage: TokenUsage,
    pub cache_meta: Option<CacheMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    ToolUse,
    MaxTokens,
    ContentFilter,
}

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cache_creation_tokens: usize,
    pub cache_read_tokens: usize,
    pub reasoning_tokens: usize,
}

#[derive(Debug, Clone)]
pub struct CacheMeta {
    pub cache_creation_tokens: usize,
    pub cache_read_tokens: usize,
}

pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub max_context: usize,
    pub max_output: usize,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub supports_reasoning: bool,
    pub supports_caching: bool,
    pub pricing: ModelPricing,
}

#[derive(Debug, Clone)]
pub struct ModelPricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: f64,
    pub cache_create_per_mtok: f64,
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn chat(
        &self,
        request: &ChatRequest<'_>,
        delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) -> Result<ChatResponse>;

    fn supports_tool_calling(&self) -> bool {
        true
    }
    fn supports_reasoning(&self) -> bool {
        false
    }
    fn supports_caching(&self) -> bool {
        false
    }
    fn model_info(&self) -> &ModelInfo;
}

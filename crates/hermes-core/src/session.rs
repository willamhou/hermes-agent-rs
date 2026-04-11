use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::message::Message;
use crate::provider::TokenUsage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub source: String,
    pub model: String,
    pub system_prompt: String,
    pub cwd: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub message_count: u32,
    pub tool_call_count: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub title: Option<String>,
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create_session(&self, meta: &SessionMeta) -> Result<()>;
    async fn end_session(&self, session_id: &str) -> Result<()>;
    async fn append_message(&self, session_id: &str, msg: &Message) -> Result<i64>;
    async fn load_history(&self, session_id: &str) -> Result<Vec<Message>>;
    async fn get_session(&self, session_id: &str) -> Result<Option<SessionMeta>>;
    async fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMeta>>;
    async fn update_usage(&self, session_id: &str, usage: &TokenUsage) -> Result<()>;
}

use async_trait::async_trait;

use crate::error::Result;
use crate::message::Message;

#[async_trait]
pub trait MemoryProvider: Send + Sync {
    fn system_prompt_block(&self) -> Option<String>;
    async fn prefetch(&self, query: &str, session_id: &str) -> Result<String>;
    async fn sync_turn(&self, user: &str, assistant: &str, session_id: &str) -> Result<()>;
    async fn on_turn_start(&self, turn: u32, message: &str) -> Result<()>;
    async fn on_turn_end(&self, user: &str, assistant: &str) -> Result<()>;
    async fn on_session_end(&self, messages: &[Message]) -> Result<()>;
    async fn on_pre_compress(&self, messages: &[Message]) -> Result<Option<String>>;
    async fn on_memory_write(&self, action: &str, target: &str, content: &str) -> Result<()>;
    async fn on_delegation(&self, task: &str, result: &str, child_session_id: &str) -> Result<()>;
    async fn shutdown(&self) -> Result<()>;
}

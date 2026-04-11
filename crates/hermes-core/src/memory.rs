use async_trait::async_trait;

use crate::error::Result;
use crate::message::Message;

#[async_trait]
pub trait MemoryProvider: Send + Sync {
    fn system_prompt_block(&self) -> Option<String>;

    async fn prefetch(&self, query: &str, session_id: &str) -> Result<String> {
        let _ = (query, session_id);
        Ok(String::new())
    }

    async fn sync_turn(&self, user: &str, assistant: &str, session_id: &str) -> Result<()> {
        let _ = (user, assistant, session_id);
        Ok(())
    }

    async fn on_pre_compress(&self, messages: &[Message]) -> Result<Option<String>> {
        let _ = messages;
        Ok(None)
    }

    async fn on_memory_write(&self, action: &str, target: &str, content: &str) -> Result<()> {
        let _ = (action, target, content);
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

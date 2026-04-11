use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct MessageEvent {
    pub platform: String,
    pub chat_id: String,
    pub user_id: String,
    pub text: String,
    pub reply_to: Option<String>,
}

#[derive(Debug)]
pub enum PlatformEvent {
    Message(MessageEvent),
    Shutdown,
}

#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    fn platform_name(&self) -> &str;
    async fn start(&self, event_tx: mpsc::Sender<PlatformEvent>) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn send_response(&self, event: &MessageEvent, response: &str) -> Result<()>;
}

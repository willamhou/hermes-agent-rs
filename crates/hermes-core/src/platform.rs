use crate::error::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ChatType {
    #[default]
    DirectMessage,
    Group,
    Channel,
}

#[derive(Debug, Clone)]
pub struct MessageEvent {
    pub platform: String,
    pub chat_id: String,
    pub user_id: String,
    pub user_name: Option<String>,
    pub text: String,
    pub reply_to: Option<String>,
    pub chat_type: ChatType,
    pub thread_id: Option<String>,
}

#[derive(Debug)]
pub enum PlatformEvent {
    Message(MessageEvent),
    Shutdown,
}

#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    fn platform_name(&self) -> &str;
    async fn run(self: Arc<Self>, event_tx: mpsc::Sender<PlatformEvent>) -> Result<()>;
    async fn send_response(&self, event: &MessageEvent, response: &str) -> Result<()>;
}

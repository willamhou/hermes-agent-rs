use std::path::PathBuf;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::Result;
use crate::message::ToolResult;
use crate::stream::StreamDelta;

pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub working_dir: PathBuf,
    pub approval_tx: mpsc::Sender<ApprovalRequest>,
    pub delta_tx: mpsc::Sender<StreamDelta>,
}

pub struct ApprovalRequest {
    pub tool_name: String,
    pub command: String,
    pub reason: String,
    pub response_tx: tokio::sync::oneshot::Sender<ApprovalDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    AllowSession,
    AllowAlways,
    Deny,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> ToolSchema;
    fn toolset(&self) -> &str;
    fn is_available(&self) -> bool {
        true
    }
    fn is_read_only(&self) -> bool {
        false
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult>;
}

use std::path::PathBuf;
use std::sync::Arc;

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

#[derive(Debug, Clone)]
pub struct ToolConfig {
    pub terminal: TerminalToolConfig,
    pub file: FileToolConfig,
    pub workspace_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct TerminalToolConfig {
    pub timeout: u64,
    pub max_timeout: u64,
    pub output_max_chars: usize,
}

#[derive(Debug, Clone)]
pub struct FileToolConfig {
    pub read_max_chars: usize,
    pub read_max_lines: usize,
    pub blocked_prefixes: Vec<PathBuf>,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            terminal: TerminalToolConfig::default(),
            file: FileToolConfig::default(),
            workspace_root: PathBuf::from("."),
        }
    }
}

impl Default for TerminalToolConfig {
    fn default() -> Self {
        Self {
            timeout: 180,
            max_timeout: 600,
            output_max_chars: 50_000,
        }
    }
}

impl Default for FileToolConfig {
    fn default() -> Self {
        Self {
            read_max_chars: 100_000,
            read_max_lines: 2000,
            blocked_prefixes: vec![
                PathBuf::from("/etc/"),
                PathBuf::from("/boot/"),
                PathBuf::from("/usr/lib/systemd/"),
            ],
        }
    }
}

#[derive(Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub working_dir: PathBuf,
    pub approval_tx: mpsc::Sender<ApprovalRequest>,
    pub delta_tx: mpsc::Sender<StreamDelta>,
    pub tool_config: Arc<ToolConfig>,
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
    fn is_exclusive(&self) -> bool {
        false
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult>;
}

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::clarify::ClarifyRequest;
use crate::error::Result;
use crate::message::Message;
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
    pub browser: BrowserToolConfig,
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

#[derive(Debug, Clone)]
pub struct BrowserToolConfig {
    pub headless: bool,
    pub sandbox: bool,
    pub launch_timeout_secs: u64,
    pub action_timeout_secs: u64,
    pub output_max_chars: usize,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub executable: Option<PathBuf>,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            terminal: TerminalToolConfig::default(),
            file: FileToolConfig::default(),
            browser: BrowserToolConfig::default(),
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

impl Default for BrowserToolConfig {
    fn default() -> Self {
        Self {
            headless: true,
            sandbox: true,
            launch_timeout_secs: 20,
            action_timeout_secs: 30,
            output_max_chars: 50_000,
            viewport_width: 1280,
            viewport_height: 720,
            executable: None,
        }
    }
}

#[derive(Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub working_dir: PathBuf,
    pub approval_tx: mpsc::Sender<ApprovalRequest>,
    pub delta_tx: mpsc::Sender<StreamDelta>,
    pub execution_observer: Option<Arc<dyn ToolExecutionObserver>>,
    pub tool_config: Arc<ToolConfig>,
    pub memory: Option<Arc<dyn MemoryAccess>>,
    pub aux_provider: Option<Arc<dyn crate::provider::Provider>>,
    pub skills: Option<Arc<dyn SkillAccess>>,
    pub delegation_depth: u32,
    pub clarify_tx: Option<mpsc::Sender<ClarifyRequest>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SkillDoc {
    pub name: String,
    pub description: String,
    pub body: String,
}

#[async_trait]
pub trait MemoryAccess: Send + Sync {
    fn read_live(&self, key: &str) -> Result<Option<String>>;
    fn write_live(&self, key: &str, content: &str) -> Result<()>;
    fn refresh_snapshot(&self) -> Result<()>;

    async fn on_memory_write(&self, action: &str, target: &str, content: &str) -> Result<()> {
        let _ = (action, target, content);
        Ok(())
    }
}

#[async_trait]
pub trait SkillAccess: Send + Sync {
    async fn list(&self) -> Result<Vec<SkillSummary>>;
    async fn get(&self, name: &str) -> Result<Option<SkillDoc>>;
    async fn match_for_turn(
        &self,
        user_message: &str,
        history: &[Message],
        max_skills: usize,
    ) -> Result<Vec<SkillDoc>>;
    async fn create(&self, name: &str, content: &str) -> Result<()>;
    async fn edit(&self, name: &str, content: &str) -> Result<()>;
    async fn delete(&self, name: &str) -> Result<()>;
    async fn reload(&self) -> Result<()>;
}

pub struct ApprovalRequest {
    pub tool_name: String,
    pub memory_key: String,
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

#[derive(Debug, Clone)]
pub struct ToolExecutionObservation {
    pub session_id: String,
    pub call_id: String,
    pub tool_name: String,
    pub toolset: Option<String>,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ToolExecutionResultObservation {
    pub request: ToolExecutionObservation,
    pub result: ToolResult,
}

#[async_trait]
pub trait ToolExecutionObserver: Send + Sync {
    async fn on_tool_call(
        &self,
        observation: ToolExecutionObservation,
        ctx: &ToolContext,
    ) -> Result<()> {
        let _ = (observation, ctx);
        Ok(())
    }

    async fn on_tool_result(
        &self,
        observation: ToolExecutionResultObservation,
        ctx: &ToolContext,
    ) -> Result<()> {
        let _ = (observation, ctx);
        Ok(())
    }
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

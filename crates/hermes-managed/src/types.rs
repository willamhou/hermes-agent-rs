use chrono::{DateTime, Utc};
use hermes_core::error::{HermesError, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedApprovalPolicy {
    #[default]
    Ask,
    Yolo,
    Deny,
}

impl ManagedApprovalPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Yolo => "yolo",
            Self::Deny => "deny",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "ask" => Some(Self::Ask),
            "yolo" => Some(Self::Yolo),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Interrupted,
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ManagedRunEventKind {
    #[serde(rename = "run.created")]
    RunCreated,
    #[serde(rename = "run.started")]
    RunStarted,
    #[serde(rename = "tool.call_started")]
    ToolCallStarted,
    #[serde(rename = "tool.progress")]
    ToolProgress,
    #[serde(rename = "tool.process_started")]
    ToolProcessStarted,
    #[serde(rename = "tool.process_completed")]
    ToolProcessCompleted,
    #[serde(rename = "tool.process_failed")]
    ToolProcessFailed,
    #[serde(rename = "tool.process_timed_out")]
    ToolProcessTimedOut,
    #[serde(rename = "tool.browser_action_started")]
    ToolBrowserActionStarted,
    #[serde(rename = "tool.browser_action_completed")]
    ToolBrowserActionCompleted,
    #[serde(rename = "tool.browser_action_failed")]
    ToolBrowserActionFailed,
    #[serde(rename = "tool.mcp_call_started")]
    ToolMcpCallStarted,
    #[serde(rename = "tool.mcp_call_completed")]
    ToolMcpCallCompleted,
    #[serde(rename = "tool.mcp_call_failed")]
    ToolMcpCallFailed,
    #[serde(rename = "tool.request_signed")]
    ToolRequestSigned,
    #[serde(rename = "tool.response_signed")]
    ToolResponseSigned,
    #[serde(rename = "run.provider_call_started")]
    RunProviderCallStarted,
    #[serde(rename = "run.ownership_claimed")]
    RunOwnershipClaimed,
    #[serde(rename = "run.ownership_released")]
    RunOwnershipReleased,
    #[serde(rename = "run.completed")]
    RunCompleted,
    #[serde(rename = "run.failed")]
    RunFailed,
    #[serde(rename = "run.mcp_admission_rejected")]
    RunMcpAdmissionRejected,
    #[serde(rename = "run.cleanup_failed")]
    RunCleanupFailed,
    #[serde(rename = "run.continuation_checkpoint")]
    RunContinuationCheckpoint,
    #[serde(rename = "run.browser_session_checkpoint")]
    RunBrowserSessionCheckpoint,
    #[serde(rename = "run.mcp_runtime_checkpoint")]
    RunMcpRuntimeCheckpoint,
    #[serde(rename = "run.takeover_assessed")]
    RunTakeoverAssessed,
    #[serde(rename = "run.recovery_decided")]
    RunRecoveryDecision,
    #[serde(rename = "run.takeover_established")]
    RunTakeoverEstablished,
    #[serde(rename = "run.takeover_updated")]
    RunTakeoverUpdated,
    #[serde(rename = "run.replayed")]
    RunReplayed,
    #[serde(rename = "run.interrupted")]
    RunInterrupted,
    #[serde(rename = "run.cancelled")]
    RunCancelled,
    #[serde(rename = "run.timed_out")]
    RunTimedOut,
}

impl ManagedRunEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RunCreated => "run.created",
            Self::RunStarted => "run.started",
            Self::ToolCallStarted => "tool.call_started",
            Self::ToolProgress => "tool.progress",
            Self::ToolProcessStarted => "tool.process_started",
            Self::ToolProcessCompleted => "tool.process_completed",
            Self::ToolProcessFailed => "tool.process_failed",
            Self::ToolProcessTimedOut => "tool.process_timed_out",
            Self::ToolBrowserActionStarted => "tool.browser_action_started",
            Self::ToolBrowserActionCompleted => "tool.browser_action_completed",
            Self::ToolBrowserActionFailed => "tool.browser_action_failed",
            Self::ToolMcpCallStarted => "tool.mcp_call_started",
            Self::ToolMcpCallCompleted => "tool.mcp_call_completed",
            Self::ToolMcpCallFailed => "tool.mcp_call_failed",
            Self::ToolRequestSigned => "tool.request_signed",
            Self::ToolResponseSigned => "tool.response_signed",
            Self::RunProviderCallStarted => "run.provider_call_started",
            Self::RunOwnershipClaimed => "run.ownership_claimed",
            Self::RunOwnershipReleased => "run.ownership_released",
            Self::RunCompleted => "run.completed",
            Self::RunFailed => "run.failed",
            Self::RunMcpAdmissionRejected => "run.mcp_admission_rejected",
            Self::RunCleanupFailed => "run.cleanup_failed",
            Self::RunContinuationCheckpoint => "run.continuation_checkpoint",
            Self::RunBrowserSessionCheckpoint => "run.browser_session_checkpoint",
            Self::RunMcpRuntimeCheckpoint => "run.mcp_runtime_checkpoint",
            Self::RunTakeoverAssessed => "run.takeover_assessed",
            Self::RunRecoveryDecision => "run.recovery_decided",
            Self::RunTakeoverEstablished => "run.takeover_established",
            Self::RunTakeoverUpdated => "run.takeover_updated",
            Self::RunReplayed => "run.replayed",
            Self::RunInterrupted => "run.interrupted",
            Self::RunCancelled => "run.cancelled",
            Self::RunTimedOut => "run.timed_out",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "run.created" => Some(Self::RunCreated),
            "run.started" => Some(Self::RunStarted),
            "tool.call_started" => Some(Self::ToolCallStarted),
            "tool.progress" => Some(Self::ToolProgress),
            "tool.process_started" => Some(Self::ToolProcessStarted),
            "tool.process_completed" => Some(Self::ToolProcessCompleted),
            "tool.process_failed" => Some(Self::ToolProcessFailed),
            "tool.process_timed_out" => Some(Self::ToolProcessTimedOut),
            "tool.browser_action_started" => Some(Self::ToolBrowserActionStarted),
            "tool.browser_action_completed" => Some(Self::ToolBrowserActionCompleted),
            "tool.browser_action_failed" => Some(Self::ToolBrowserActionFailed),
            "tool.mcp_call_started" => Some(Self::ToolMcpCallStarted),
            "tool.mcp_call_completed" => Some(Self::ToolMcpCallCompleted),
            "tool.mcp_call_failed" => Some(Self::ToolMcpCallFailed),
            "tool.request_signed" => Some(Self::ToolRequestSigned),
            "tool.response_signed" => Some(Self::ToolResponseSigned),
            "run.provider_call_started" => Some(Self::RunProviderCallStarted),
            "run.ownership_claimed" => Some(Self::RunOwnershipClaimed),
            "run.ownership_released" => Some(Self::RunOwnershipReleased),
            "run.completed" => Some(Self::RunCompleted),
            "run.failed" => Some(Self::RunFailed),
            "run.mcp_admission_rejected" => Some(Self::RunMcpAdmissionRejected),
            "run.cleanup_failed" => Some(Self::RunCleanupFailed),
            "run.continuation_checkpoint" => Some(Self::RunContinuationCheckpoint),
            "run.browser_session_checkpoint" => Some(Self::RunBrowserSessionCheckpoint),
            "run.mcp_runtime_checkpoint" => Some(Self::RunMcpRuntimeCheckpoint),
            "run.takeover_assessed" => Some(Self::RunTakeoverAssessed),
            "run.recovery_decided" => Some(Self::RunRecoveryDecision),
            "run.takeover_established" => Some(Self::RunTakeoverEstablished),
            "run.takeover_updated" => Some(Self::RunTakeoverUpdated),
            "run.replayed" => Some(Self::RunReplayed),
            "run.interrupted" => Some(Self::RunInterrupted),
            "run.cancelled" => Some(Self::RunCancelled),
            "run.timed_out" => Some(Self::RunTimedOut),
            _ => None,
        }
    }
}

impl ManagedRunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "interrupted" => Some(Self::Interrupted),
            "cancelled" => Some(Self::Cancelled),
            "timed_out" => Some(Self::TimedOut),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Interrupted | Self::Cancelled | Self::TimedOut
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedAgent {
    pub id: String,
    pub name: String,
    pub latest_version: u32,
    pub archived: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ManagedAgent {
    pub fn new(name: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: new_id("agent"),
            name: name.into(),
            latest_version: 0,
            archived: false,
            created_at: now,
            updated_at: now,
        }
    }
}

pub fn validate_managed_agent_name(name: &str) -> Result<()> {
    let len = name.len();
    let valid = (1..=64).contains(&len)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));

    if valid {
        Ok(())
    } else {
        Err(HermesError::Config(
            "managed agent name must be 1-64 chars of ASCII letters, digits, '-' or '_'"
                .to_string(),
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagedAgentVersion {
    pub agent_id: String,
    pub version: u32,
    pub model: String,
    pub base_url: Option<String>,
    pub system_prompt: String,
    pub allowed_tools: Vec<String>,
    pub allowed_skills: Vec<String>,
    pub max_iterations: u32,
    pub temperature: f64,
    pub approval_policy: ManagedApprovalPolicy,
    pub timeout_secs: u32,
    pub created_at: DateTime<Utc>,
}

impl ManagedAgentVersion {
    pub fn new(
        agent_id: impl Into<String>,
        version: u32,
        model: impl Into<String>,
        system_prompt: impl Into<String>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            version,
            model: model.into(),
            base_url: None,
            system_prompt: system_prompt.into(),
            allowed_tools: Vec::new(),
            allowed_skills: Vec::new(),
            max_iterations: 90,
            temperature: 0.0,
            approval_policy: ManagedApprovalPolicy::Ask,
            timeout_secs: 300,
            created_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagedAgentVersionDraft {
    pub model: String,
    pub base_url: Option<String>,
    pub system_prompt: String,
    pub allowed_tools: Vec<String>,
    pub allowed_skills: Vec<String>,
    pub max_iterations: u32,
    pub temperature: f64,
    pub approval_policy: ManagedApprovalPolicy,
    pub timeout_secs: u32,
}

impl ManagedAgentVersionDraft {
    pub fn new(model: impl Into<String>, system_prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: None,
            system_prompt: system_prompt.into(),
            allowed_tools: Vec::new(),
            allowed_skills: Vec::new(),
            max_iterations: 90,
            temperature: 0.0,
            approval_policy: ManagedApprovalPolicy::Ask,
            timeout_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRun {
    pub id: String,
    pub agent_id: String,
    pub agent_version: u32,
    pub status: ManagedRunStatus,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_of_run_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub cancel_requested_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

impl ManagedRun {
    pub fn new(agent_id: impl Into<String>, agent_version: u32, model: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            id: new_id("run"),
            agent_id: agent_id.into(),
            agent_version,
            status: ManagedRunStatus::Pending,
            model: model.into(),
            session_id: None,
            prompt: String::new(),
            replay_of_run_id: None,
            started_at: now,
            updated_at: now,
            ended_at: None,
            cancel_requested_at: None,
            last_error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunOwnerState {
    Active,
    Expired,
    Incomplete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunOwnerSnapshot {
    pub worker_id: String,
    pub state: ManagedRunOwnerState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunCleanupResourceKind {
    Pid,
    ProcessGroup,
    BrowserSession,
    McpHttpResourceSubscription,
    McpHttpSession,
}

impl ManagedRunCleanupResourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pid => "pid",
            Self::ProcessGroup => "process_group",
            Self::BrowserSession => "browser_session",
            Self::McpHttpResourceSubscription => "mcp_http_resource_subscription",
            Self::McpHttpSession => "mcp_http_session",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pid" => Some(Self::Pid),
            "process_group" => Some(Self::ProcessGroup),
            "browser_session" => Some(Self::BrowserSession),
            "mcp_http_resource_subscription" => Some(Self::McpHttpResourceSubscription),
            "mcp_http_session" => Some(Self::McpHttpSession),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunCleanupResource {
    pub run_id: String,
    pub entry_id: u64,
    pub kind: ManagedRunCleanupResourceKind,
    pub label: String,
    pub target_value: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunArtifactKind {
    AssistantOutput,
    ToolOutput,
}

impl ManagedRunArtifactKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AssistantOutput => "assistant_output",
            Self::ToolOutput => "tool_output",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "assistant_output" => Some(Self::AssistantOutput),
            "tool_output" => Some(Self::ToolOutput),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagedRunArtifact {
    pub id: u64,
    pub run_id: String,
    pub kind: ManagedRunArtifactKind,
    pub label: String,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub content: String,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagedRunArtifactDraft {
    pub kind: ManagedRunArtifactKind,
    pub label: String,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub content: String,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagedRunEvent {
    pub id: u64,
    pub run_id: String,
    pub kind: ManagedRunEventKind,
    pub message: Option<String>,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagedRunEventDraft {
    pub kind: ManagedRunEventKind,
    pub message: Option<String>,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

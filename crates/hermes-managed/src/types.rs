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
    #[serde(rename = "tool.request_signed")]
    ToolRequestSigned,
    #[serde(rename = "tool.response_signed")]
    ToolResponseSigned,
    #[serde(rename = "run.completed")]
    RunCompleted,
    #[serde(rename = "run.failed")]
    RunFailed,
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
            Self::ToolRequestSigned => "tool.request_signed",
            Self::ToolResponseSigned => "tool.response_signed",
            Self::RunCompleted => "run.completed",
            Self::RunFailed => "run.failed",
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
            "tool.request_signed" => Some(Self::ToolRequestSigned),
            "tool.response_signed" => Some(Self::ToolResponseSigned),
            "run.completed" => Some(Self::RunCompleted),
            "run.failed" => Some(Self::RunFailed),
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
            "cancelled" => Some(Self::Cancelled),
            "timed_out" => Some(Self::TimedOut),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut
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

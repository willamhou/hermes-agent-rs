use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use hermes_agent::{
    ConversationContinuationAction, ConversationContinuationBoundary,
    ConversationContinuationBoundaryKind,
};
use hermes_tools::browser_handoff::BrowserActionEvent;
use hermes_tools::process_handoff::ProcessExecutionEvent;
use serde::{Deserialize, Serialize};

use crate::filtered_registry::{
    ManagedMcpAdmissionRejection, managed_mcp_admission_rejection_from_event,
};
use crate::store::ManagedStore;
use crate::types::{
    ManagedRun, ManagedRunArtifact, ManagedRunArtifactKind, ManagedRunEvent, ManagedRunEventKind,
    ManagedRunOwnerSnapshot, ManagedRunOwnerState,
};
use hermes_core::error::Result;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunCleanupFailureSummary {
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub attempted: usize,
    #[serde(default)]
    pub cleaned: usize,
    #[serde(default)]
    pub failures: Vec<String>,
}

impl ManagedRunCleanupFailureSummary {
    pub fn is_empty(&self) -> bool {
        self.phase.is_empty()
            && self.attempted == 0
            && self.cleaned == 0
            && self.failures.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunInterruptionCause {
    LeaseExpired,
    OwnershipNotEstablished,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunInterruptionSummary {
    pub cause: ManagedRunInterruptionCause,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_claimed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_last_heartbeat_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_lease_expires_at: Option<DateTime<Utc>>,
}

impl ManagedRunInterruptionSummary {
    pub fn message(&self) -> String {
        match (self.cause, self.owner_worker_id.as_deref()) {
            (ManagedRunInterruptionCause::LeaseExpired, Some(worker_id)) => {
                format!("managed run interrupted after worker {worker_id} lost its lease")
            }
            (ManagedRunInterruptionCause::LeaseExpired, None) => {
                "managed run interrupted after worker lease expiry".to_string()
            }
            (ManagedRunInterruptionCause::OwnershipNotEstablished, _) => {
                "managed run interrupted before ownership was established".to_string()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunOwnershipReleaseReason {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunOwnershipClaimSummary {
    pub worker_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_lineage_id: Option<String>,
}

impl ManagedRunOwnershipClaimSummary {
    pub fn message(&self) -> String {
        format!("worker {} claimed managed run ownership", self.worker_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunOwnershipReleaseSummary {
    pub worker_id: String,
    pub reason: ManagedRunOwnershipReleaseReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_claimed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_last_heartbeat_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_lease_expires_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ManagedRunOwnershipReleaseSummary {
    pub fn message(&self) -> String {
        let reason = match self.reason {
            ManagedRunOwnershipReleaseReason::Completed => "completed the run",
            ManagedRunOwnershipReleaseReason::Failed => "failed the run",
            ManagedRunOwnershipReleaseReason::Cancelled => "cancelled the run",
            ManagedRunOwnershipReleaseReason::TimedOut => "timed out the run",
            ManagedRunOwnershipReleaseReason::Interrupted => {
                "lost ownership when the run was interrupted"
            }
        };
        format!(
            "worker {} released managed run ownership after it {reason}",
            self.worker_id
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunRecoveryHint {
    #[serde(default)]
    pub replayable: bool,
    #[serde(default)]
    pub reuses_session_id: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ManagedRunRecoveryHint {
    pub fn is_empty(&self) -> bool {
        !self.replayable
            && !self.reuses_session_id
            && self.suggested_action.is_none()
            && self.note.is_none()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunContinuationBoundaryKind {
    UserCheckpointed,
    AssistantResponseCheckpointed,
    PendingToolCalls,
    ToolResultsCheckpointed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunContinuationAction {
    CallProvider,
    ExecutePendingTools,
    CompleteTurn,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunContinuationCheckpointSummary {
    pub kind: ManagedRunContinuationBoundaryKind,
    pub safe_action: ManagedRunContinuationAction,
    pub history_len: usize,
    #[serde(default)]
    pub pending_tool_calls: usize,
}

impl ManagedRunContinuationCheckpointSummary {
    pub fn from_boundary(
        boundary: ConversationContinuationBoundary,
    ) -> ManagedRunContinuationCheckpointSummary {
        ManagedRunContinuationCheckpointSummary {
            kind: match boundary.kind {
                ConversationContinuationBoundaryKind::UserCheckpointed => {
                    ManagedRunContinuationBoundaryKind::UserCheckpointed
                }
                ConversationContinuationBoundaryKind::AssistantResponseCheckpointed => {
                    ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed
                }
                ConversationContinuationBoundaryKind::PendingToolCalls => {
                    ManagedRunContinuationBoundaryKind::PendingToolCalls
                }
                ConversationContinuationBoundaryKind::ToolResultsCheckpointed => {
                    ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed
                }
            },
            safe_action: match boundary.safe_action {
                ConversationContinuationAction::CallProvider => {
                    ManagedRunContinuationAction::CallProvider
                }
                ConversationContinuationAction::ExecutePendingTools => {
                    ManagedRunContinuationAction::ExecutePendingTools
                }
                ConversationContinuationAction::CompleteTurn => {
                    ManagedRunContinuationAction::CompleteTurn
                }
            },
            history_len: boundary.history_len,
            pending_tool_calls: boundary.pending_tool_calls,
        }
    }

    pub fn message(&self) -> String {
        match self.kind {
            ManagedRunContinuationBoundaryKind::UserCheckpointed => {
                "managed run checkpointed after user input".to_string()
            }
            ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed => {
                "managed run checkpointed after final assistant response".to_string()
            }
            ManagedRunContinuationBoundaryKind::PendingToolCalls => format!(
                "managed run checkpointed with {} pending tool call(s)",
                self.pending_tool_calls
            ),
            ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed => {
                "managed run checkpointed after tool results".to_string()
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunProviderCallFenceSummary {
    pub request_history_len: usize,
    pub tool_count: usize,
    pub safe_resume_from: ManagedRunContinuationCheckpointSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ManagedRunProviderCallFenceSummary {
    pub fn message(&self) -> String {
        format!(
            "managed run provider call started from {} boundary",
            match self.safe_resume_from.kind {
                ManagedRunContinuationBoundaryKind::UserCheckpointed => "user checkpointed",
                ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed => {
                    "assistant response checkpointed"
                }
                ManagedRunContinuationBoundaryKind::PendingToolCalls => "pending tool calls",
                ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed => {
                    "tool results checkpointed"
                }
            }
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunProcessHandoffState {
    Running,
    Completed,
    Failed,
    TimedOut,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunProcessReplayDisposition {
    SafeToReplay,
    UnsafeSideEffectWindow,
    CompletedButNotRecorded,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunProcessHandoffSummary {
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub state: ManagedRunProcessHandoffState,
    pub replay_disposition: ManagedRunProcessReplayDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_group: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ManagedRunProcessHandoffSummary {
    pub fn message(&self) -> String {
        match self.state {
            ManagedRunProcessHandoffState::Running => format!(
                "{} started and may still have side effects in flight",
                self.tool_name
            ),
            ManagedRunProcessHandoffState::Completed => format!(
                "{} completed before its tool result was durably checkpointed",
                self.tool_name
            ),
            ManagedRunProcessHandoffState::Failed => format!(
                "{} failed before its tool result was durably checkpointed",
                self.tool_name
            ),
            ManagedRunProcessHandoffState::TimedOut => format!(
                "{} timed out before its tool result was durably checkpointed",
                self.tool_name
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunBrowserHandoffState {
    Started,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunBrowserReplayDisposition {
    SafeToReplay,
    UnsafeSideEffectWindow,
    CompletedButNotRecorded,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunBrowserHandoffSummary {
    pub action: String,
    pub state: ManagedRunBrowserHandoffState,
    pub replay_disposition: ManagedRunBrowserReplayDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default)]
    pub wait_for_navigation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ManagedRunBrowserHandoffSummary {
    pub fn message(&self) -> String {
        match self.state {
            ManagedRunBrowserHandoffState::Started => format!(
                "browser action '{}' started and may still have page or external side effects in flight",
                self.action
            ),
            ManagedRunBrowserHandoffState::Completed => format!(
                "browser action '{}' completed before its tool result was durably checkpointed",
                self.action
            ),
            ManagedRunBrowserHandoffState::Failed => format!(
                "browser action '{}' failed before its tool result was durably checkpointed",
                self.action
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunBrowserSessionCheckpointSummary {
    pub action: String,
    #[serde(default)]
    pub session_open: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ManagedRunBrowserSessionCheckpointSummary {
    pub fn message(&self) -> String {
        if self.session_open {
            format!(
                "browser session checkpointed after '{}' with live session state",
                self.action
            )
        } else {
            format!(
                "browser session checkpointed after '{}' with no live session state",
                self.action
            )
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunMcpHandoffState {
    Started,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunMcpReplayDisposition {
    SafeToReplay,
    UnsafeSideEffectWindow,
    CompletedButNotRecorded,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunMcpHandoffSummary {
    pub tool_name: String,
    pub state: ManagedRunMcpHandoffState,
    pub replay_disposition: ManagedRunMcpReplayDisposition,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub requires_live_runtime: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ManagedRunMcpHandoffSummary {
    pub fn message(&self) -> String {
        match self.state {
            ManagedRunMcpHandoffState::Started => {
                if self.read_only && !self.requires_live_runtime {
                    format!(
                        "read-only MCP tool '{}' started before a durable tool-result checkpoint",
                        self.tool_name
                    )
                } else {
                    format!(
                        "MCP tool '{}' started and may still have runtime or external side effects in flight",
                        self.tool_name
                    )
                }
            }
            ManagedRunMcpHandoffState::Completed => {
                if self.read_only && !self.requires_live_runtime {
                    format!(
                        "read-only MCP tool '{}' completed before its tool result was durably checkpointed",
                        self.tool_name
                    )
                } else {
                    format!(
                        "MCP tool '{}' completed before its tool result was durably checkpointed",
                        self.tool_name
                    )
                }
            }
            ManagedRunMcpHandoffState::Failed => format!(
                "MCP tool '{}' failed before its tool result was durably checkpointed",
                self.tool_name
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunMcpRuntimeCheckpointSummary {
    pub tool_name: String,
    #[serde(default)]
    pub live_runtime_required: bool,
    #[serde(default)]
    pub active_subscription_count: usize,
    #[serde(default)]
    pub active_servers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ManagedRunMcpRuntimeCheckpointSummary {
    pub fn message(&self) -> String {
        if self.live_runtime_required {
            if self.active_subscription_count > 0 {
                format!(
                    "MCP runtime checkpointed after '{}' with {} active subscription(s) requiring live runtime continuity",
                    self.tool_name, self.active_subscription_count
                )
            } else {
                format!(
                    "MCP runtime checkpointed after '{}' with unresolved live runtime/session state",
                    self.tool_name
                )
            }
        } else {
            format!(
                "MCP runtime checkpointed after '{}' with no live runtime dependency",
                self.tool_name
            )
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunArtifactContinuitySummary {
    pub latest_kind: ManagedRunArtifactKind,
    pub latest_label: String,
    pub latest_run_id: String,
    #[serde(default)]
    pub latest_run_is_current: bool,
    #[serde(default)]
    pub lineage_depth: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_content_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunReplayTrigger {
    ManualReplay,
    InterruptedAutoReplay,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunReplayProvenanceSummary {
    pub source_run_id: String,
    pub root_run_id: String,
    pub replay_depth: u32,
    pub trigger: ManagedRunReplayTrigger,
    pub trigger_worker_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_lineage_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_status: Option<crate::types::ManagedRunStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_interruption_cause: Option<ManagedRunInterruptionCause>,
    #[serde(default)]
    pub reused_session_id: bool,
    #[serde(default)]
    pub resumed_existing_turn: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_boundary: Option<ManagedRunContinuationBoundaryKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ManagedRunReplayProvenanceSummary {
    pub fn message(&self) -> String {
        let trigger = match self.trigger {
            ManagedRunReplayTrigger::ManualReplay => "manual replay",
            ManagedRunReplayTrigger::InterruptedAutoReplay => "interrupted auto replay",
        };
        format!(
            "{trigger} from {} at depth {}",
            self.source_run_id, self.replay_depth
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunContinuationSummary {
    pub source_run_id: String,
    pub root_run_id: String,
    pub replay_depth: u32,
    pub trigger: ManagedRunReplayTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_lineage_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_status: Option<crate::types::ManagedRunStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_interruption_cause: Option<ManagedRunInterruptionCause>,
    #[serde(default)]
    pub reused_session_id: bool,
    #[serde(default)]
    pub resumed_existing_turn: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_boundary: Option<ManagedRunContinuationBoundaryKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluated_by_worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ManagedRunContinuationSummary {
    pub fn message(&self) -> String {
        let trigger = match self.trigger {
            ManagedRunReplayTrigger::ManualReplay => "manual replay",
            ManagedRunReplayTrigger::InterruptedAutoReplay => "interrupted auto replay",
        };
        format!(
            "{trigger} continuation of {} at depth {}",
            self.source_run_id, self.replay_depth
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunReplayChildSummary {
    pub latest_run_id: String,
    pub latest_status: crate::types::ManagedRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_lineage_id: Option<String>,
    #[serde(default)]
    pub replay_child_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<ManagedRunReplayTrigger>,
    #[serde(default)]
    pub reused_session_id: bool,
    #[serde(default)]
    pub resumed_existing_turn: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_boundary: Option<ManagedRunContinuationBoundaryKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ManagedRunReplayChildSummary {
    pub fn message(&self) -> String {
        if self.replay_child_count > 1 {
            format!(
                "latest replay child {} is {} ({} total replay children)",
                self.latest_run_id,
                self.latest_status.as_str(),
                self.replay_child_count
            )
        } else {
            format!(
                "replay child {} is {}",
                self.latest_run_id,
                self.latest_status.as_str()
            )
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunTakeoverState {
    Active,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunTakeoverSummary {
    pub replay_run_id: String,
    pub replay_run_status: crate::types::ManagedRunStatus,
    pub takeover_state: ManagedRunTakeoverState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_lineage_id: Option<String>,
    #[serde(default)]
    pub replay_child_count: usize,
    #[serde(default = "default_takeover_lineage_depth")]
    pub lineage_depth: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<ManagedRunReplayTrigger>,
    #[serde(default)]
    pub reused_session_id: bool,
    #[serde(default)]
    pub resumed_existing_turn: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_boundary: Option<ManagedRunContinuationBoundaryKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluated_by_worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_owner: Option<ManagedRunOwnerSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_target_ownership_claim: Option<ManagedRunOwnershipClaimSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_target_recovery_decision: Option<ManagedRunRecoveryDecisionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_target_continuation_checkpoint: Option<ManagedRunContinuationCheckpointSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_target_provider_call_fence: Option<ManagedRunProviderCallFenceSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_target_process_handoff: Option<ManagedRunProcessHandoffSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_target_browser_handoff: Option<ManagedRunBrowserHandoffSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_target_browser_session_checkpoint: Option<ManagedRunBrowserSessionCheckpointSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_target_mcp_handoff: Option<ManagedRunMcpHandoffSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_target_mcp_runtime_checkpoint: Option<ManagedRunMcpRuntimeCheckpointSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_target_artifact_continuity: Option<ManagedRunArtifactContinuitySummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_target_takeover_assessment: Option<ManagedRunTakeoverAssessmentSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_target_ownership_release: Option<ManagedRunOwnershipReleaseSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ManagedRunTakeoverSummary {
    pub fn message(&self) -> String {
        let state = match self.takeover_state {
            ManagedRunTakeoverState::Active => "actively owns",
            ManagedRunTakeoverState::Completed => "completed and owns",
            ManagedRunTakeoverState::Failed => "failed after taking over",
            ManagedRunTakeoverState::Cancelled => "was cancelled after taking over",
            ManagedRunTakeoverState::TimedOut => "timed out after taking over",
            ManagedRunTakeoverState::Interrupted => "was interrupted after taking over",
        };
        let mut message = if self.lineage_depth > 1 {
            format!(
                "replay descendant {} at depth {} {} continuation lineage ({})",
                self.replay_run_id,
                self.lineage_depth,
                state,
                self.replay_run_status.as_str()
            )
        } else {
            format!(
                "replay child {} {} continuation lineage ({})",
                self.replay_run_id,
                state,
                self.replay_run_status.as_str()
            )
        };
        if let Some(owner) = &self.current_owner {
            let owner_state = match owner.state {
                ManagedRunOwnerState::Active => "active",
                ManagedRunOwnerState::Expired => "expired",
                ManagedRunOwnerState::Incomplete => "incomplete",
            };
            message.push_str(&format!(
                " via worker {} ({owner_state} lease)",
                owner.worker_id
            ));
        }
        message
    }
}

fn default_takeover_lineage_depth() -> usize {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunTakeoverAssessmentSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_lineage_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluated_by_worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_boundary: Option<ManagedRunContinuationBoundaryKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interruption_cause: Option<ManagedRunInterruptionCause>,
    #[serde(default)]
    pub provider_call_in_flight: bool,
    #[serde(default)]
    pub process_handoff_risk: bool,
    #[serde(default)]
    pub browser_handoff_risk: bool,
    #[serde(default)]
    pub browser_session_state: bool,
    #[serde(default)]
    pub mcp_handoff_risk: bool,
    #[serde(default)]
    pub mcp_runtime_state: bool,
    #[serde(default)]
    pub replay_depth: u32,
    #[serde(default)]
    pub max_auto_replays: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ManagedRunTakeoverAssessmentSummary {
    pub fn message(&self) -> String {
        let blocking_risk_count = [
            self.process_handoff_risk,
            self.browser_handoff_risk,
            self.browser_session_state,
            self.mcp_handoff_risk,
            self.mcp_runtime_state,
        ]
        .into_iter()
        .filter(|flag| *flag)
        .count();
        let base = match (self.evaluated_by_worker_id.as_deref(), blocking_risk_count) {
            (Some(worker_id), 0) => format!(
                "worker {worker_id} assessed interrupted run takeover with no blocking runtime risks"
            ),
            (Some(worker_id), 1) => format!(
                "worker {worker_id} assessed interrupted run takeover with 1 blocking runtime risk"
            ),
            (Some(worker_id), count) => format!(
                "worker {worker_id} assessed interrupted run takeover with {count} blocking runtime risks"
            ),
            (None, 0) => {
                "interrupted run takeover assessed with no blocking runtime risks".to_string()
            }
            (None, 1) => {
                "interrupted run takeover assessed with 1 blocking runtime risk".to_string()
            }
            (None, count) => {
                format!("interrupted run takeover assessed with {count} blocking runtime risks")
            }
        };

        if self.provider_call_in_flight {
            format!("{base}; provider dispatch was already in flight")
        } else {
            base
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunRecoveryDecisionKind {
    ReplayStarted,
    FollowReplay,
    ManualReview,
    Blocked,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunRecoveryDecisionReason {
    RunStillActive,
    ReplayChildActive,
    DepthLimitReached,
    ProcessHandoffRisk,
    BrowserHandoffRisk,
    BrowserSessionState,
    McpHandoffRisk,
    McpRuntimeState,
    ReplaySpawnFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunRecoveryDecisionSummary {
    pub decision: ManagedRunRecoveryDecisionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<ManagedRunRecoveryDecisionReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_lineage_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluated_by_worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_worker_id: Option<String>,
    #[doc(hidden)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_follow_target_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_follow_target_status: Option<crate::types::ManagedRunStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_follow_target_lineage_depth: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_boundary: Option<ManagedRunContinuationBoundaryKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ManagedRunRecoveryDecisionSummary {
    pub fn message(&self) -> String {
        match (self.decision, self.reason) {
            (ManagedRunRecoveryDecisionKind::ReplayStarted, _) => {
                if let Some(run_id) = self.replay_run_id.as_deref() {
                    format!("automatic replay started as {run_id}")
                } else {
                    "automatic replay started".to_string()
                }
            }
            (ManagedRunRecoveryDecisionKind::FollowReplay, _) => {
                if let (Some(run_id), Some(depth)) = (
                    self.active_follow_target_run_id.as_deref(),
                    self.active_follow_target_lineage_depth,
                ) {
                    if depth > 1 {
                        return format!(
                            "continuation is owned by replay descendant {run_id} at depth {depth}"
                        );
                    }
                }
                if let Some(run_id) = self
                    .active_follow_target_run_id
                    .as_deref()
                    .or(self.replay_run_id.as_deref())
                {
                    format!("continuation is owned by replay child {run_id}")
                } else {
                    "continuation is owned by an active replay child".to_string()
                }
            }
            (ManagedRunRecoveryDecisionKind::ManualReview, Some(reason)) => format!(
                "automatic replay requires manual review because {}",
                recovery_decision_reason_label(reason)
            ),
            (ManagedRunRecoveryDecisionKind::Blocked, Some(reason)) => format!(
                "automatic replay is blocked because {}",
                recovery_decision_reason_label(reason)
            ),
            (ManagedRunRecoveryDecisionKind::Failed, Some(reason)) => format!(
                "automatic replay failed because {}",
                recovery_decision_reason_label(reason)
            ),
            (ManagedRunRecoveryDecisionKind::ManualReview, None) => {
                "automatic replay requires manual review".to_string()
            }
            (ManagedRunRecoveryDecisionKind::Blocked, None) => {
                "automatic replay is blocked".to_string()
            }
            (ManagedRunRecoveryDecisionKind::Failed, None) => "automatic replay failed".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManagedRunDerivedSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_admission_rejection: Option<ManagedMcpAdmissionRejection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_failure: Option<ManagedRunCleanupFailureSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interruption: Option<ManagedRunInterruptionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership_claim: Option<ManagedRunOwnershipClaimSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership_release: Option<ManagedRunOwnershipReleaseSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_checkpoint: Option<ManagedRunContinuationCheckpointSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_call_fence: Option<ManagedRunProviderCallFenceSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_handoff: Option<ManagedRunProcessHandoffSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_handoff: Option<ManagedRunBrowserHandoffSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_session_checkpoint: Option<ManagedRunBrowserSessionCheckpointSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_handoff: Option<ManagedRunMcpHandoffSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_runtime_checkpoint: Option<ManagedRunMcpRuntimeCheckpointSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_continuity: Option<ManagedRunArtifactContinuitySummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_provenance: Option<ManagedRunReplayProvenanceSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<ManagedRunContinuationSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_child: Option<ManagedRunReplayChildSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership: Option<ManagedRunOwnerSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover: Option<ManagedRunTakeoverSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover_assessment: Option<ManagedRunTakeoverAssessmentSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_decision: Option<ManagedRunRecoveryDecisionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_hint: Option<ManagedRunRecoveryHint>,
}

impl ManagedRunDerivedSummary {
    pub fn is_empty(&self) -> bool {
        self.mcp_admission_rejection.is_none()
            && self.cleanup_failure.is_none()
            && self.interruption.is_none()
            && self.ownership_claim.is_none()
            && self.ownership_release.is_none()
            && self.continuation_checkpoint.is_none()
            && self.provider_call_fence.is_none()
            && self.process_handoff.is_none()
            && self.browser_handoff.is_none()
            && self.browser_session_checkpoint.is_none()
            && self.mcp_handoff.is_none()
            && self.mcp_runtime_checkpoint.is_none()
            && self.artifact_continuity.is_none()
            && self.replay_provenance.is_none()
            && self.continuation.is_none()
            && self.replay_child.is_none()
            && self.ownership.is_none()
            && self.takeover.is_none()
            && self.takeover_assessment.is_none()
            && self.recovery_decision.is_none()
            && self.recovery_hint.is_none()
    }
}

struct ManagedRunRecoveryContext<'a> {
    continuation_checkpoint: Option<&'a ManagedRunContinuationCheckpointSummary>,
    provider_call_fence: Option<&'a ManagedRunProviderCallFenceSummary>,
    process_handoff: Option<&'a ManagedRunProcessHandoffSummary>,
    browser_handoff: Option<&'a ManagedRunBrowserHandoffSummary>,
    browser_session_checkpoint: Option<&'a ManagedRunBrowserSessionCheckpointSummary>,
    mcp_handoff: Option<&'a ManagedRunMcpHandoffSummary>,
    mcp_runtime_checkpoint: Option<&'a ManagedRunMcpRuntimeCheckpointSummary>,
    artifact_continuity: Option<&'a ManagedRunArtifactContinuitySummary>,
    replay_child: Option<&'a ManagedRunReplayChildSummary>,
    takeover: Option<&'a ManagedRunTakeoverSummary>,
}

fn artifact_content_preview(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    const MAX_PREVIEW_CHARS: usize = 240;
    let mut preview = trimmed.chars().take(MAX_PREVIEW_CHARS).collect::<String>();
    if trimmed.chars().count() > MAX_PREVIEW_CHARS {
        preview.push_str("...");
    }
    Some(preview)
}

fn artifact_kind_label(kind: &ManagedRunArtifactKind) -> &'static str {
    match kind {
        ManagedRunArtifactKind::AssistantOutput => "assistant output",
        ManagedRunArtifactKind::ToolOutput => "tool output",
    }
}

fn managed_run_artifact_continuity_summary(
    current_run_id: &str,
    lineage_depth: usize,
    artifact: ManagedRunArtifact,
) -> ManagedRunArtifactContinuitySummary {
    let latest_run_is_current = artifact.run_id == current_run_id;
    let source = if latest_run_is_current {
        "current run"
    } else {
        "replay lineage"
    };
    let kind_label = artifact_kind_label(&artifact.kind);
    let note = Some(format!(
        "latest checkpointed {kind_label} is available from {source}"
    ));

    ManagedRunArtifactContinuitySummary {
        latest_kind: artifact.kind,
        latest_label: artifact.label,
        latest_run_id: artifact.run_id,
        latest_run_is_current,
        lineage_depth,
        latest_tool_name: artifact.tool_name,
        latest_tool_call_id: artifact.tool_call_id,
        latest_content_preview: artifact_content_preview(&artifact.content),
        note,
    }
}

fn parse_continuation_boundary_kind(value: &str) -> Option<ManagedRunContinuationBoundaryKind> {
    match value {
        "user_checkpointed" => Some(ManagedRunContinuationBoundaryKind::UserCheckpointed),
        "assistant_response_checkpointed" => {
            Some(ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed)
        }
        "pending_tool_calls" => Some(ManagedRunContinuationBoundaryKind::PendingToolCalls),
        "tool_results_checkpointed" => {
            Some(ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed)
        }
        _ => None,
    }
}

fn parse_recovery_decision_kind(value: &str) -> Option<ManagedRunRecoveryDecisionKind> {
    match value {
        "replay_started" => Some(ManagedRunRecoveryDecisionKind::ReplayStarted),
        "follow_replay" => Some(ManagedRunRecoveryDecisionKind::FollowReplay),
        "manual_review" => Some(ManagedRunRecoveryDecisionKind::ManualReview),
        "blocked" => Some(ManagedRunRecoveryDecisionKind::Blocked),
        "failed" => Some(ManagedRunRecoveryDecisionKind::Failed),
        _ => None,
    }
}

fn parse_recovery_decision_reason(value: &str) -> Option<ManagedRunRecoveryDecisionReason> {
    match value {
        "run_still_active" => Some(ManagedRunRecoveryDecisionReason::RunStillActive),
        "replay_child_active" => Some(ManagedRunRecoveryDecisionReason::ReplayChildActive),
        "depth_limit_reached" => Some(ManagedRunRecoveryDecisionReason::DepthLimitReached),
        "process_handoff_risk" => Some(ManagedRunRecoveryDecisionReason::ProcessHandoffRisk),
        "browser_handoff_risk" => Some(ManagedRunRecoveryDecisionReason::BrowserHandoffRisk),
        "browser_session_state" => Some(ManagedRunRecoveryDecisionReason::BrowserSessionState),
        "mcp_handoff_risk" => Some(ManagedRunRecoveryDecisionReason::McpHandoffRisk),
        "mcp_runtime_state" => Some(ManagedRunRecoveryDecisionReason::McpRuntimeState),
        "replay_spawn_failed" => Some(ManagedRunRecoveryDecisionReason::ReplaySpawnFailed),
        _ => None,
    }
}

fn recovery_decision_reason_label(reason: ManagedRunRecoveryDecisionReason) -> &'static str {
    match reason {
        ManagedRunRecoveryDecisionReason::RunStillActive => {
            "the requested run is still pending or running"
        }
        ManagedRunRecoveryDecisionReason::ReplayChildActive => {
            "the run already has an active replay child"
        }
        ManagedRunRecoveryDecisionReason::DepthLimitReached => {
            "the configured replay depth limit was reached"
        }
        ManagedRunRecoveryDecisionReason::ProcessHandoffRisk => {
            "a process handoff may already have produced side effects"
        }
        ManagedRunRecoveryDecisionReason::BrowserHandoffRisk => {
            "a browser action may already have changed page or external state"
        }
        ManagedRunRecoveryDecisionReason::BrowserSessionState => {
            "a live browser session is still required"
        }
        ManagedRunRecoveryDecisionReason::McpHandoffRisk => {
            "an MCP tool call may already have changed remote or runtime state"
        }
        ManagedRunRecoveryDecisionReason::McpRuntimeState => {
            "a live MCP runtime or session is still required"
        }
        ManagedRunRecoveryDecisionReason::ReplaySpawnFailed => {
            "the replay child could not be created"
        }
    }
}

pub fn managed_run_replay_provenance_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunReplayProvenanceSummary> {
    if event.kind != ManagedRunEventKind::RunCreated {
        return None;
    }

    let metadata = event.metadata.as_ref()?;
    let source_run_id = metadata.get("replay_of_run_id")?.as_str()?.to_string();
    let root_run_id = metadata
        .get("replay_root_run_id")
        .and_then(|value| value.as_str())
        .unwrap_or(source_run_id.as_str())
        .to_string();
    let replay_depth = metadata
        .get("replay_depth")
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(1);
    let trigger = match metadata
        .get("replay_trigger")
        .and_then(|value| value.as_str())
        .unwrap_or("manual_replay")
    {
        "interrupted_auto_replay" => ManagedRunReplayTrigger::InterruptedAutoReplay,
        _ => ManagedRunReplayTrigger::ManualReplay,
    };
    let trigger_worker_id = metadata
        .get("replay_trigger_worker_id")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let takeover_lineage_id = metadata
        .get("takeover_lineage_id")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
        .or_else(|| Some(event.run_id.clone()));
    let source_status = metadata
        .get("replay_source_status")
        .and_then(|value| value.as_str())
        .and_then(crate::types::ManagedRunStatus::parse);
    let source_interruption_cause = metadata
        .get("replay_source_interruption_cause")
        .and_then(|value| value.as_str())
        .and_then(|value| match value {
            "lease_expired" => Some(ManagedRunInterruptionCause::LeaseExpired),
            "ownership_not_established" => {
                Some(ManagedRunInterruptionCause::OwnershipNotEstablished)
            }
            _ => None,
        });
    let reused_session_id = metadata
        .get("reused_session_id")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let resumed_existing_turn = metadata
        .get("resumed_existing_turn")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let source_boundary = metadata
        .get("replay_source_boundary")
        .and_then(|value| value.as_str())
        .and_then(parse_continuation_boundary_kind);
    let note = metadata
        .get("replay_note")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);

    Some(ManagedRunReplayProvenanceSummary {
        source_run_id,
        root_run_id,
        replay_depth,
        trigger,
        trigger_worker_id,
        takeover_lineage_id,
        source_status,
        source_interruption_cause,
        reused_session_id,
        resumed_existing_turn,
        source_boundary,
        note,
    })
}

pub fn managed_run_continuation_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunContinuationSummary> {
    if event.kind != ManagedRunEventKind::RunTakeoverEstablished {
        return None;
    }

    let metadata = event.metadata.as_ref()?;
    serde_json::from_value(metadata.clone()).ok()
}

pub fn managed_run_recovery_decision_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunRecoveryDecisionSummary> {
    if event.kind != ManagedRunEventKind::RunRecoveryDecision {
        return None;
    }

    let metadata = event.metadata.as_ref()?;
    let decision = metadata
        .get("decision")
        .and_then(|value| value.as_str())
        .and_then(parse_recovery_decision_kind)?;
    let reason = metadata
        .get("reason")
        .and_then(|value| value.as_str())
        .and_then(parse_recovery_decision_reason);
    let replay_run_id = metadata
        .get("replay_run_id")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let evaluated_by_worker_id = metadata
        .get("evaluated_by_worker_id")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let takeover_worker_id = metadata
        .get("takeover_worker_id")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let worker_id = metadata
        .get("worker_id")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let active_follow_target_run_id = metadata
        .get("active_follow_target_run_id")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let active_follow_target_status = metadata
        .get("active_follow_target_status")
        .and_then(|value| value.as_str())
        .and_then(crate::types::ManagedRunStatus::parse);
    let active_follow_target_lineage_depth = metadata
        .get("active_follow_target_lineage_depth")
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok());
    let takeover_lineage_id = metadata
        .get("takeover_lineage_id")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
        .or_else(|| replay_run_id.clone())
        .or_else(|| active_follow_target_run_id.clone());
    let source_boundary = metadata
        .get("source_boundary")
        .and_then(|value| value.as_str())
        .and_then(parse_continuation_boundary_kind);
    let note = metadata
        .get("note")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);

    Some(ManagedRunRecoveryDecisionSummary {
        decision,
        reason,
        replay_run_id,
        takeover_lineage_id,
        evaluated_by_worker_id: evaluated_by_worker_id.or_else(|| worker_id.clone()),
        takeover_worker_id: takeover_worker_id.or_else(|| {
            if decision == ManagedRunRecoveryDecisionKind::FollowReplay {
                worker_id.clone()
            } else {
                None
            }
        }),
        worker_id,
        active_follow_target_run_id,
        active_follow_target_status,
        active_follow_target_lineage_depth,
        source_boundary,
        note,
    })
}

fn align_follow_replay_decision_with_takeover(
    recovery_decision: &mut Option<ManagedRunRecoveryDecisionSummary>,
    takeover: Option<&ManagedRunTakeoverSummary>,
) {
    let Some(decision) = recovery_decision.as_mut() else {
        return;
    };
    if decision.decision != ManagedRunRecoveryDecisionKind::FollowReplay {
        return;
    }
    let Some(takeover) = takeover else {
        return;
    };
    decision.active_follow_target_run_id = Some(takeover.replay_run_id.clone());
    decision.active_follow_target_status = Some(takeover.replay_run_status.clone());
    decision.active_follow_target_lineage_depth = Some(takeover.lineage_depth);
    decision.takeover_lineage_id = takeover.takeover_lineage_id.clone();
    decision.takeover_worker_id = takeover
        .current_owner
        .as_ref()
        .map(|owner| owner.worker_id.clone())
        .or_else(|| takeover.takeover_worker_id.clone())
        .or_else(|| decision.takeover_worker_id.clone());
}

fn legacy_managed_run_continuation_summary(
    provenance: &ManagedRunReplayProvenanceSummary,
    source_decision: Option<&ManagedRunRecoveryDecisionSummary>,
) -> ManagedRunContinuationSummary {
    ManagedRunContinuationSummary {
        source_run_id: provenance.source_run_id.clone(),
        root_run_id: provenance.root_run_id.clone(),
        replay_depth: provenance.replay_depth,
        trigger: provenance.trigger,
        takeover_lineage_id: provenance.takeover_lineage_id.clone(),
        source_status: provenance.source_status.clone(),
        source_interruption_cause: provenance.source_interruption_cause,
        reused_session_id: provenance.reused_session_id,
        resumed_existing_turn: provenance.resumed_existing_turn,
        source_boundary: source_decision
            .and_then(|decision| decision.source_boundary)
            .or(provenance.source_boundary),
        evaluated_by_worker_id: source_decision
            .and_then(|decision| decision.evaluated_by_worker_id.clone())
            .or_else(|| Some(provenance.trigger_worker_id.clone())),
        takeover_worker_id: source_decision
            .and_then(|decision| decision.takeover_worker_id.clone())
            .or_else(|| Some(provenance.trigger_worker_id.clone())),
        note: source_decision
            .and_then(|decision| decision.note.clone())
            .or_else(|| provenance.note.clone()),
    }
}

fn managed_run_replay_child_summary(
    child: &ManagedRun,
    replay_child_count: usize,
    provenance: Option<&ManagedRunReplayProvenanceSummary>,
) -> ManagedRunReplayChildSummary {
    let trigger = provenance.map(|summary| summary.trigger);
    let reused_session_id = provenance
        .map(|summary| summary.reused_session_id)
        .unwrap_or(false);
    let resumed_existing_turn = provenance
        .map(|summary| summary.resumed_existing_turn)
        .unwrap_or(false);
    let source_boundary = provenance.and_then(|summary| summary.source_boundary);
    let note = Some(if replay_child_count > 1 {
        format!(
            "latest replay child {} is {} and supersedes older replay attempts",
            child.id,
            child.status.as_str()
        )
    } else {
        format!(
            "replay child {} currently owns continuation of this run",
            child.id
        )
    });

    ManagedRunReplayChildSummary {
        latest_run_id: child.id.clone(),
        latest_status: child.status.clone(),
        takeover_lineage_id: provenance.and_then(|summary| summary.takeover_lineage_id.clone()),
        replay_child_count,
        trigger,
        reused_session_id,
        resumed_existing_turn,
        source_boundary,
        note,
    }
}

fn managed_run_replay_child_from_event(
    event: &ManagedRunEvent,
    replay_child_count: Option<usize>,
) -> Option<ManagedRunReplayChildSummary> {
    if event.kind != ManagedRunEventKind::RunReplayed {
        return None;
    }

    let metadata = event.metadata.as_ref()?;
    let latest_run_id = metadata.get("replay_run_id")?.as_str()?.to_string();
    let latest_status = metadata
        .get("replay_run_status")
        .and_then(|value| value.as_str())
        .and_then(crate::types::ManagedRunStatus::parse)?;
    let takeover_lineage_id = metadata
        .get("takeover_lineage_id")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
        .or_else(|| Some(latest_run_id.clone()));
    let trigger = metadata
        .get("replay_trigger")
        .and_then(|value| value.as_str())
        .map(|value| match value {
            "interrupted_auto_replay" => ManagedRunReplayTrigger::InterruptedAutoReplay,
            _ => ManagedRunReplayTrigger::ManualReplay,
        });
    let reused_session_id = metadata
        .get("reused_session_id")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let resumed_existing_turn = metadata
        .get("resumed_existing_turn")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let source_boundary = metadata
        .get("replay_source_boundary")
        .and_then(|value| value.as_str())
        .and_then(parse_continuation_boundary_kind);
    let replay_child_count = replay_child_count.unwrap_or(1).max(1);
    let note = Some(if replay_child_count > 1 {
        format!(
            "latest replay child {} is {} and supersedes older replay attempts",
            latest_run_id,
            latest_status.as_str()
        )
    } else {
        format!(
            "replay child {} currently owns continuation of this run",
            latest_run_id
        )
    });

    Some(ManagedRunReplayChildSummary {
        latest_run_id,
        latest_status,
        takeover_lineage_id,
        replay_child_count,
        trigger,
        reused_session_id,
        resumed_existing_turn,
        source_boundary,
        note,
    })
}

fn artifact_recovery_suffix(summary: &ManagedRunArtifactContinuitySummary) -> String {
    let kind = artifact_kind_label(&summary.latest_kind);
    let source = if summary.latest_run_is_current {
        "current run"
    } else {
        "replay lineage"
    };
    format!(
        "latest checkpointed {kind} '{}' is available from {source}",
        summary.latest_label
    )
}

fn with_artifact_context(base: String, context: &ManagedRunRecoveryContext<'_>) -> String {
    match context.artifact_continuity {
        Some(summary) => format!("{base}; {}", artifact_recovery_suffix(summary)),
        None => base,
    }
}

fn replay_child_recovery_suffix(summary: &ManagedRunReplayChildSummary) -> String {
    if summary.replay_child_count > 1 {
        format!(
            "latest replay child is {} ({}) and {} replay children already exist",
            summary.latest_run_id,
            summary.latest_status.as_str(),
            summary.replay_child_count
        )
    } else {
        format!(
            "run has already been replayed as {} ({})",
            summary.latest_run_id,
            summary.latest_status.as_str()
        )
    }
}

fn managed_run_takeover_state(status: crate::types::ManagedRunStatus) -> ManagedRunTakeoverState {
    match status {
        crate::types::ManagedRunStatus::Pending | crate::types::ManagedRunStatus::Running => {
            ManagedRunTakeoverState::Active
        }
        crate::types::ManagedRunStatus::Completed => ManagedRunTakeoverState::Completed,
        crate::types::ManagedRunStatus::Failed => ManagedRunTakeoverState::Failed,
        crate::types::ManagedRunStatus::Cancelled => ManagedRunTakeoverState::Cancelled,
        crate::types::ManagedRunStatus::TimedOut => ManagedRunTakeoverState::TimedOut,
        crate::types::ManagedRunStatus::Interrupted => ManagedRunTakeoverState::Interrupted,
    }
}

async fn load_replay_run_takeover_context(
    store: &ManagedStore,
    run: &ManagedRun,
) -> Result<(
    Option<ManagedRunReplayProvenanceSummary>,
    Option<ManagedRunContinuationSummary>,
    Option<ManagedRunOwnerSnapshot>,
    Option<ManagedRunOwnershipClaimSummary>,
    Option<ManagedRunRecoveryDecisionSummary>,
    Option<ManagedRunContinuationCheckpointSummary>,
    Option<ManagedRunProviderCallFenceSummary>,
    Option<ManagedRunProcessHandoffSummary>,
    Option<ManagedRunBrowserHandoffSummary>,
    Option<ManagedRunBrowserSessionCheckpointSummary>,
    Option<ManagedRunMcpHandoffSummary>,
    Option<ManagedRunMcpRuntimeCheckpointSummary>,
    Option<ManagedRunArtifactContinuitySummary>,
    Option<ManagedRunTakeoverAssessmentSummary>,
    Option<ManagedRunOwnershipReleaseSummary>,
)> {
    let created_event = store
        .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunCreated)
        .await?;
    let replay_provenance = created_event
        .as_ref()
        .and_then(managed_run_replay_provenance_from_event);
    let takeover_established_event = store
        .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunTakeoverEstablished)
        .await?;
    let continuation = if let Some(continuation) = takeover_established_event
        .as_ref()
        .and_then(managed_run_continuation_from_event)
    {
        Some(continuation)
    } else if let Some(provenance) = replay_provenance.as_ref() {
        let source_recovery_decision = match run.replay_of_run_id.as_deref() {
            Some(source_run_id) => store
                .get_latest_run_event_by_kind(
                    source_run_id,
                    ManagedRunEventKind::RunRecoveryDecision,
                )
                .await?
                .as_ref()
                .and_then(managed_run_recovery_decision_from_event)
                .filter(|decision| {
                    decision
                        .replay_run_id
                        .as_deref()
                        .map(|replay_run_id| replay_run_id == run.id)
                        .unwrap_or(false)
                }),
            None => None,
        };
        Some(legacy_managed_run_continuation_summary(
            provenance,
            source_recovery_decision.as_ref(),
        ))
    } else {
        None
    };
    let current_owner = store.get_run_owner_snapshot(&run.id).await?;
    let ownership_claim = store
        .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunOwnershipClaimed)
        .await?
        .as_ref()
        .and_then(managed_run_ownership_claim_from_event);
    let recovery_decision = store
        .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunRecoveryDecision)
        .await?
        .as_ref()
        .and_then(managed_run_recovery_decision_from_event);
    let continuation_checkpoint = store
        .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunContinuationCheckpoint)
        .await?
        .as_ref()
        .and_then(managed_run_continuation_checkpoint_from_event);
    let provider_call_event = store
        .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunProviderCallStarted)
        .await?;
    let provider_call_fence = match (
        provider_call_event
            .as_ref()
            .and_then(managed_run_provider_call_fence_from_event),
        provider_call_event.as_ref().map(|event| event.id),
        store
            .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunContinuationCheckpoint)
            .await?
            .as_ref()
            .map(|event| event.id),
    ) {
        (Some(fence), Some(provider_id), Some(checkpoint_id)) if provider_id > checkpoint_id => {
            Some(fence)
        }
        (Some(fence), Some(_), None) => Some(fence),
        _ => None,
    };
    let mut latest_process_event = None;
    for kind in [
        ManagedRunEventKind::ToolProcessStarted,
        ManagedRunEventKind::ToolProcessCompleted,
        ManagedRunEventKind::ToolProcessFailed,
        ManagedRunEventKind::ToolProcessTimedOut,
    ] {
        if let Some(event) = store.get_latest_run_event_by_kind(&run.id, kind).await? {
            let replace = latest_process_event
                .as_ref()
                .map(|current: &ManagedRunEvent| event.id > current.id)
                .unwrap_or(true);
            if replace {
                latest_process_event = Some(event);
            }
        }
    }
    let process_handoff = match (
        latest_process_event
            .as_ref()
            .and_then(managed_run_process_handoff_from_event),
        latest_process_event.as_ref().map(|event| event.id),
        store
            .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunContinuationCheckpoint)
            .await?
            .as_ref()
            .map(|event| event.id),
    ) {
        (Some(handoff), Some(process_id), Some(checkpoint_id)) if process_id > checkpoint_id => {
            Some(handoff)
        }
        (Some(handoff), Some(_), None) => Some(handoff),
        _ => None,
    };
    let mut latest_browser_event = None;
    for kind in [
        ManagedRunEventKind::ToolBrowserActionStarted,
        ManagedRunEventKind::ToolBrowserActionCompleted,
        ManagedRunEventKind::ToolBrowserActionFailed,
    ] {
        if let Some(event) = store.get_latest_run_event_by_kind(&run.id, kind).await? {
            let replace = latest_browser_event
                .as_ref()
                .map(|current: &ManagedRunEvent| event.id > current.id)
                .unwrap_or(true);
            if replace {
                latest_browser_event = Some(event);
            }
        }
    }
    let browser_handoff = match (
        latest_browser_event
            .as_ref()
            .and_then(managed_run_browser_handoff_from_event),
        latest_browser_event.as_ref().map(|event| event.id),
        store
            .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunContinuationCheckpoint)
            .await?
            .as_ref()
            .map(|event| event.id),
    ) {
        (Some(handoff), Some(browser_id), Some(checkpoint_id)) if browser_id > checkpoint_id => {
            Some(handoff)
        }
        (Some(handoff), Some(_), None) => Some(handoff),
        _ => None,
    };
    let browser_session_checkpoint = store
        .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunBrowserSessionCheckpoint)
        .await?
        .as_ref()
        .and_then(managed_run_browser_session_checkpoint_from_event)
        .filter(|checkpoint| checkpoint.session_open);
    let mut latest_mcp_event = None;
    for kind in [
        ManagedRunEventKind::ToolMcpCallStarted,
        ManagedRunEventKind::ToolMcpCallCompleted,
        ManagedRunEventKind::ToolMcpCallFailed,
    ] {
        if let Some(event) = store.get_latest_run_event_by_kind(&run.id, kind).await? {
            let replace = latest_mcp_event
                .as_ref()
                .map(|current: &ManagedRunEvent| event.id > current.id)
                .unwrap_or(true);
            if replace {
                latest_mcp_event = Some(event);
            }
        }
    }
    let mcp_handoff = match (
        latest_mcp_event
            .as_ref()
            .and_then(managed_run_mcp_handoff_from_event),
        latest_mcp_event.as_ref().map(|event| event.id),
        store
            .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunContinuationCheckpoint)
            .await?
            .as_ref()
            .map(|event| event.id),
    ) {
        (Some(handoff), Some(mcp_id), Some(checkpoint_id)) if mcp_id > checkpoint_id => {
            Some(handoff)
        }
        (Some(handoff), Some(_), None) => Some(handoff),
        _ => None,
    };
    let mcp_runtime_checkpoint = store
        .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunMcpRuntimeCheckpoint)
        .await?
        .as_ref()
        .and_then(managed_run_mcp_runtime_checkpoint_from_event)
        .filter(|checkpoint| checkpoint.live_runtime_required);
    let (artifact_lineage, artifacts) = store
        .list_run_artifacts_with_replay_lineage(&run.id, 1, 64)
        .await?;
    let artifact_continuity = artifacts.into_iter().last().map(|artifact| {
        let lineage_depth = artifact_lineage
            .iter()
            .position(|lineage_run| lineage_run.id == artifact.run_id)
            .map(|position| artifact_lineage.len().saturating_sub(position + 1))
            .unwrap_or_default();
        managed_run_artifact_continuity_summary(&run.id, lineage_depth, artifact)
    });
    let takeover_assessment = store
        .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunTakeoverAssessed)
        .await?
        .as_ref()
        .and_then(managed_run_takeover_assessment_from_event);
    let ownership_release = store
        .get_latest_run_event_by_kind(&run.id, ManagedRunEventKind::RunOwnershipReleased)
        .await?
        .as_ref()
        .and_then(managed_run_ownership_release_from_event);

    Ok((
        replay_provenance,
        continuation,
        current_owner,
        ownership_claim,
        recovery_decision,
        continuation_checkpoint,
        provider_call_fence,
        process_handoff,
        browser_handoff,
        browser_session_checkpoint,
        mcp_handoff,
        mcp_runtime_checkpoint,
        artifact_continuity,
        takeover_assessment,
        ownership_release,
    ))
}

struct ManagedRunTakeoverContext<'a> {
    replay_provenance: Option<&'a ManagedRunReplayProvenanceSummary>,
    continuation: Option<&'a ManagedRunContinuationSummary>,
    recovery_decision: Option<&'a ManagedRunRecoveryDecisionSummary>,
    current_owner: Option<ManagedRunOwnerSnapshot>,
    follow_target_ownership_claim: Option<ManagedRunOwnershipClaimSummary>,
    follow_target_recovery_decision: Option<ManagedRunRecoveryDecisionSummary>,
    follow_target_continuation_checkpoint: Option<ManagedRunContinuationCheckpointSummary>,
    follow_target_provider_call_fence: Option<ManagedRunProviderCallFenceSummary>,
    follow_target_process_handoff: Option<ManagedRunProcessHandoffSummary>,
    follow_target_browser_handoff: Option<ManagedRunBrowserHandoffSummary>,
    follow_target_browser_session_checkpoint: Option<ManagedRunBrowserSessionCheckpointSummary>,
    follow_target_mcp_handoff: Option<ManagedRunMcpHandoffSummary>,
    follow_target_mcp_runtime_checkpoint: Option<ManagedRunMcpRuntimeCheckpointSummary>,
    follow_target_artifact_continuity: Option<ManagedRunArtifactContinuitySummary>,
    follow_target_takeover_assessment: Option<ManagedRunTakeoverAssessmentSummary>,
    follow_target_ownership_release: Option<ManagedRunOwnershipReleaseSummary>,
}

fn managed_run_takeover_summary(
    replay_run: &ManagedRun,
    replay_child_count: usize,
    lineage_depth: usize,
    context: ManagedRunTakeoverContext<'_>,
) -> Option<ManagedRunTakeoverSummary> {
    let decision = context
        .recovery_decision
        .filter(|decision| decision.decision == ManagedRunRecoveryDecisionKind::FollowReplay)
        .filter(|decision| {
            decision
                .replay_run_id
                .as_deref()
                .map(|run_id| run_id == replay_run.id)
                .unwrap_or(true)
        });

    Some(ManagedRunTakeoverSummary {
        replay_run_id: replay_run.id.clone(),
        replay_run_status: replay_run.status.clone(),
        takeover_state: managed_run_takeover_state(replay_run.status.clone()),
        takeover_lineage_id: context
            .continuation
            .and_then(|summary| summary.takeover_lineage_id.clone())
            .or_else(|| {
                context
                    .replay_provenance
                    .and_then(|summary| summary.takeover_lineage_id.clone())
            })
            .or_else(|| decision.and_then(|summary| summary.takeover_lineage_id.clone())),
        replay_child_count,
        lineage_depth,
        trigger: context.replay_provenance.map(|summary| summary.trigger),
        reused_session_id: context
            .replay_provenance
            .map(|summary| summary.reused_session_id)
            .unwrap_or(false),
        resumed_existing_turn: context
            .replay_provenance
            .map(|summary| summary.resumed_existing_turn)
            .unwrap_or(false),
        source_boundary: context
            .continuation
            .and_then(|summary| summary.source_boundary)
            .or_else(|| decision.and_then(|summary| summary.source_boundary))
            .or_else(|| {
                context
                    .replay_provenance
                    .and_then(|summary| summary.source_boundary)
            }),
        evaluated_by_worker_id: context
            .continuation
            .and_then(|summary| summary.evaluated_by_worker_id.clone())
            .or_else(|| decision.and_then(|summary| summary.evaluated_by_worker_id.clone())),
        takeover_worker_id: context
            .continuation
            .and_then(|summary| summary.takeover_worker_id.clone())
            .or_else(|| decision.and_then(|summary| summary.takeover_worker_id.clone())),
        current_owner: context.current_owner,
        follow_target_ownership_claim: context.follow_target_ownership_claim,
        follow_target_recovery_decision: context.follow_target_recovery_decision,
        follow_target_continuation_checkpoint: context.follow_target_continuation_checkpoint,
        follow_target_provider_call_fence: context.follow_target_provider_call_fence,
        follow_target_process_handoff: context.follow_target_process_handoff,
        follow_target_browser_handoff: context.follow_target_browser_handoff,
        follow_target_browser_session_checkpoint: context.follow_target_browser_session_checkpoint,
        follow_target_mcp_handoff: context.follow_target_mcp_handoff,
        follow_target_mcp_runtime_checkpoint: context.follow_target_mcp_runtime_checkpoint,
        follow_target_artifact_continuity: context.follow_target_artifact_continuity,
        follow_target_takeover_assessment: context.follow_target_takeover_assessment,
        follow_target_ownership_release: context.follow_target_ownership_release,
        note: context
            .continuation
            .and_then(|summary| summary.note.clone())
            .or_else(|| decision.and_then(|summary| summary.note.clone()))
            .or_else(|| {
                Some(if lineage_depth > 1 {
                    format!(
                        "latest continuation leaf {} is replay descendant depth {}",
                        replay_run.id, lineage_depth
                    )
                } else {
                    format!(
                        "replay child {} currently owns continuation of this run",
                        replay_run.id
                    )
                })
            }),
    })
}

fn managed_run_recovery_note(
    replayable: bool,
    reuses_session_id: bool,
    context: &ManagedRunRecoveryContext<'_>,
) -> String {
    if let Some(takeover) = context.takeover {
        let follow_target_recovery = takeover
            .follow_target_recovery_decision
            .as_ref()
            .map(|decision| {
                format!(
                    "; latest continuation leaf currently reports {}",
                    decision.message()
                )
            })
            .unwrap_or_default();
        let follow_target_checkpoint = takeover
            .follow_target_continuation_checkpoint
            .as_ref()
            .map(|checkpoint| {
                format!(
                    "; latest continuation leaf last reached the {} safe boundary",
                    managed_continuation_boundary_phrase(checkpoint.kind)
                )
            })
            .unwrap_or_default();
        let follow_target_provider_fence = takeover
            .follow_target_provider_call_fence
            .as_ref()
            .map(|_| {
                "; latest continuation leaf has an unresolved provider-call fence and may reissue its last provider call if replayed again"
                    .to_string()
            })
            .unwrap_or_default();
        let follow_target_process_handoff = takeover
            .follow_target_process_handoff
            .as_ref()
            .map(|handoff| {
                format!(
                    "; latest continuation leaf still carries unresolved process handoff state for '{}'",
                    handoff.tool_name
                )
            })
            .unwrap_or_default();
        let follow_target_browser_handoff = takeover
            .follow_target_browser_handoff
            .as_ref()
            .map(|handoff| {
                format!(
                    "; latest continuation leaf still carries unresolved browser action '{}'",
                    handoff.action
                )
            })
            .unwrap_or_default();
        let follow_target_browser_session = takeover
            .follow_target_browser_session_checkpoint
            .as_ref()
            .map(|checkpoint| {
                match checkpoint.page_url.as_deref() {
                    Some(page_url) => format!(
                        "; latest continuation leaf still holds live browser session state at {page_url}"
                    ),
                    None => {
                        "; latest continuation leaf still holds live browser session state"
                            .to_string()
                    }
                }
            })
            .unwrap_or_default();
        let follow_target_mcp_runtime = takeover
            .follow_target_mcp_runtime_checkpoint
            .as_ref()
            .map(|checkpoint| {
                if checkpoint.active_subscription_count > 0 {
                    format!(
                        "; latest continuation leaf still depends on {} live MCP subscription(s)",
                        checkpoint.active_subscription_count
                    )
                } else {
                    "; latest continuation leaf still depends on live MCP runtime/session state"
                        .to_string()
                }
            })
            .unwrap_or_default();
        let follow_target_mcp_handoff = takeover
            .follow_target_mcp_handoff
            .as_ref()
            .map(|handoff| {
                format!(
                    "; latest continuation leaf still carries unresolved MCP tool call '{}'",
                    handoff.tool_name
                )
            })
            .unwrap_or_default();
        let follow_target_artifact = takeover
            .follow_target_artifact_continuity
            .as_ref()
            .map(|artifact| {
                format!(
                    "; latest continuation leaf has checkpointed {} continuity from {}",
                    artifact_kind_label(&artifact.latest_kind),
                    if artifact.latest_run_is_current {
                        "its current run"
                    } else {
                        "its replay lineage"
                    }
                )
            })
            .unwrap_or_default();
        return with_artifact_context(
            format!(
                "run is interrupted, but {}; follow that run instead of replaying the source run again{}{}{}{}{}{}{}{}{}",
                takeover.message(),
                follow_target_recovery,
                follow_target_checkpoint,
                follow_target_provider_fence,
                follow_target_process_handoff,
                follow_target_browser_handoff,
                follow_target_browser_session,
                follow_target_mcp_handoff,
                follow_target_mcp_runtime,
                follow_target_artifact
            ),
            context,
        );
    }

    if let Some(replay_child) = context.replay_child {
        return with_artifact_context(
            format!(
                "run is interrupted, but {}; follow that run instead of replaying again",
                replay_child_recovery_suffix(replay_child)
            ),
            context,
        );
    }

    if !replayable {
        return with_artifact_context(
            "run is interrupted but replay is unavailable because the persisted prompt is empty"
                .to_string(),
            context,
        );
    }

    let prefix = if reuses_session_id {
        "manual replay is recommended and will reuse the persisted session context"
    } else {
        "manual replay is recommended"
    };

    if let Some(handoff) = context.process_handoff {
        return match handoff.replay_disposition {
            ManagedRunProcessReplayDisposition::SafeToReplay => with_artifact_context(
                format!(
                    "{prefix}; {} reached a replay-safe process boundary",
                    handoff.tool_name
                ),
                context,
            ),
            ManagedRunProcessReplayDisposition::UnsafeSideEffectWindow => with_artifact_context(
                format!(
                    "{prefix}; automatic replay is blocked because {} was interrupted after process start and may have already produced side effects",
                    handoff.tool_name
                ),
                context,
            ),
            ManagedRunProcessReplayDisposition::CompletedButNotRecorded => with_artifact_context(
                format!(
                    "{prefix}; automatic replay is blocked because {} finished before its tool result was durably checkpointed, so replay may duplicate side effects",
                    handoff.tool_name
                ),
                context,
            ),
        };
    }

    if let Some(handoff) = context.browser_handoff {
        if browser_action_requires_live_session(&handoff.action) {
            return with_artifact_context(
                format!(
                    "{prefix}; browser action '{}' reached a tool boundary, but Hermes does not restore live browser session/page state automatically",
                    handoff.action
                ),
                context,
            );
        }
        return match handoff.replay_disposition {
            ManagedRunBrowserReplayDisposition::SafeToReplay => with_artifact_context(
                format!(
                    "{prefix}; browser action '{}' reached a replay-safe boundary",
                    handoff.action
                ),
                context,
            ),
            ManagedRunBrowserReplayDisposition::UnsafeSideEffectWindow => with_artifact_context(
                format!(
                    "{prefix}; automatic replay is blocked because browser action '{}' was interrupted after dispatch and may have already changed page or external state",
                    handoff.action
                ),
                context,
            ),
            ManagedRunBrowserReplayDisposition::CompletedButNotRecorded => with_artifact_context(
                format!(
                    "{prefix}; automatic replay is blocked because browser action '{}' completed before its tool result was durably checkpointed, so replay may duplicate side effects",
                    handoff.action
                ),
                context,
            ),
        };
    }

    if let Some(checkpoint) = context.browser_session_checkpoint {
        if checkpoint.session_open {
            let location = match (&checkpoint.page_title, &checkpoint.page_url) {
                (Some(title), Some(url)) => format!("{title} ({url})"),
                (Some(title), None) => title.clone(),
                (None, Some(url)) => url.clone(),
                (None, None) => "unknown page".to_string(),
            };
            return with_artifact_context(
                format!(
                    "{prefix}; last safe browser checkpoint was after '{}' at {}, but replay launches a fresh browser session and does not restore live page/runtime state",
                    checkpoint.action, location
                ),
                context,
            );
        }
    }

    if let Some(handoff) = context.mcp_handoff {
        if handoff.requires_live_runtime {
            return with_artifact_context(
                format!(
                    "{prefix}; MCP tool '{}' depends on live MCP runtime/session state that Hermes does not reattach automatically",
                    handoff.tool_name
                ),
                context,
            );
        }
        return match handoff.replay_disposition {
            ManagedRunMcpReplayDisposition::SafeToReplay => with_artifact_context(
                format!(
                    "{prefix}; MCP tool '{}' reached a replay-safe boundary",
                    handoff.tool_name
                ),
                context,
            ),
            ManagedRunMcpReplayDisposition::UnsafeSideEffectWindow => with_artifact_context(
                format!(
                    "{prefix}; automatic replay is blocked because MCP tool '{}' was interrupted after dispatch and may have already changed remote subscription or local runtime state",
                    handoff.tool_name
                ),
                context,
            ),
            ManagedRunMcpReplayDisposition::CompletedButNotRecorded => with_artifact_context(
                format!(
                    "{prefix}; automatic replay is blocked because MCP tool '{}' completed before its tool result was durably checkpointed, so replay may duplicate state changes or lose runtime continuity",
                    handoff.tool_name
                ),
                context,
            ),
        };
    }

    if let Some(checkpoint) = context.mcp_runtime_checkpoint {
        if checkpoint.live_runtime_required {
            if checkpoint.active_subscription_count > 0 {
                return with_artifact_context(
                    format!(
                        "{prefix}; last safe MCP checkpoint was after '{}' with {} active subscription(s) still tied to a live MCP runtime/session, and replay starts a fresh runtime without restoring update continuity",
                        checkpoint.tool_name, checkpoint.active_subscription_count
                    ),
                    context,
                );
            }
            return with_artifact_context(
                format!(
                    "{prefix}; last safe MCP checkpoint was after '{}' but Hermes does not restore the live MCP runtime/session state that action left behind",
                    checkpoint.tool_name
                ),
                context,
            );
        }
    }

    if let Some(fence) = context.provider_call_fence {
        return with_artifact_context(
            format!(
                "{prefix}; interruption happened after provider call dispatch and before a durable response checkpoint, so replay may re-issue the last provider call from the {} boundary",
                match fence.safe_resume_from.kind {
                    ManagedRunContinuationBoundaryKind::UserCheckpointed => "user checkpointed",
                    ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed => {
                        "assistant response checkpointed"
                    }
                    ManagedRunContinuationBoundaryKind::PendingToolCalls => "pending tool call",
                    ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed => {
                        "tool results checkpointed"
                    }
                }
            ),
            context,
        );
    }

    match context
        .continuation_checkpoint
        .map(|checkpoint| checkpoint.kind)
    {
        Some(ManagedRunContinuationBoundaryKind::PendingToolCalls) => with_artifact_context(
            format!(
                "{prefix}; continuation can resume by executing {} checkpointed pending tool call(s)",
                context
                    .continuation_checkpoint
                    .map(|checkpoint| checkpoint.pending_tool_calls)
                    .unwrap_or_default()
            ),
            context,
        ),
        Some(ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed) => {
            with_artifact_context(
                format!(
                    "{prefix}; continuation can complete from a checkpointed final assistant response"
                ),
                context,
            )
        }
        Some(ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed) => with_artifact_context(
            format!("{prefix}; continuation can resume from checkpointed tool results"),
            context,
        ),
        Some(ManagedRunContinuationBoundaryKind::UserCheckpointed) => with_artifact_context(
            format!("{prefix}; continuation can resume from checkpointed user input"),
            context,
        ),
        None => with_artifact_context(prefix.to_string(), context),
    }
}

pub fn managed_run_recovery_hint_from_run(
    run: &ManagedRun,
    summary: &ManagedRunDerivedSummary,
) -> Option<ManagedRunRecoveryHint> {
    if run.status != crate::types::ManagedRunStatus::Interrupted {
        return None;
    }

    let replayable = !run.prompt.trim().is_empty();
    let reuses_session_id = replayable && run.session_id.is_some();
    let context = ManagedRunRecoveryContext {
        continuation_checkpoint: summary.continuation_checkpoint.as_ref(),
        provider_call_fence: summary.provider_call_fence.as_ref(),
        process_handoff: summary.process_handoff.as_ref(),
        browser_handoff: summary.browser_handoff.as_ref(),
        browser_session_checkpoint: summary.browser_session_checkpoint.as_ref(),
        mcp_handoff: summary.mcp_handoff.as_ref(),
        mcp_runtime_checkpoint: summary.mcp_runtime_checkpoint.as_ref(),
        artifact_continuity: summary.artifact_continuity.as_ref(),
        replay_child: summary.replay_child.as_ref(),
        takeover: summary.takeover.as_ref(),
    };
    let suggested_action = if context.replay_child.is_some() {
        Some("follow_replay".to_string())
    } else {
        match (
            context
                .process_handoff
                .map(|handoff| handoff.replay_disposition),
            context
                .browser_handoff
                .map(|handoff| handoff.replay_disposition),
            context
                .browser_handoff
                .map(|handoff| browser_action_requires_live_session(&handoff.action)),
            context
                .browser_session_checkpoint
                .map(|checkpoint| checkpoint.session_open),
            context
                .mcp_handoff
                .map(|handoff| handoff.replay_disposition),
            context
                .mcp_handoff
                .map(|handoff| handoff.requires_live_runtime),
            context
                .mcp_runtime_checkpoint
                .map(|checkpoint| checkpoint.live_runtime_required),
        ) {
            (
                Some(
                    ManagedRunProcessReplayDisposition::UnsafeSideEffectWindow
                    | ManagedRunProcessReplayDisposition::CompletedButNotRecorded,
                ),
                _,
                _,
                _,
                _,
                _,
                _,
            )
            | (
                _,
                Some(
                    ManagedRunBrowserReplayDisposition::UnsafeSideEffectWindow
                    | ManagedRunBrowserReplayDisposition::CompletedButNotRecorded,
                ),
                _,
                _,
                _,
                _,
                _,
            )
            | (_, _, Some(true), _, _, _, _)
            | (_, _, _, Some(true), _, _, _)
            | (
                _,
                _,
                _,
                _,
                Some(
                    ManagedRunMcpReplayDisposition::UnsafeSideEffectWindow
                    | ManagedRunMcpReplayDisposition::CompletedButNotRecorded,
                ),
                _,
                _,
            )
            | (_, _, _, _, _, Some(true), _)
            | (_, _, _, _, _, _, Some(true)) => Some("manual_review".to_string()),
            _ => replayable.then_some("replay".to_string()),
        }
    };
    let note = Some(managed_run_recovery_note(
        replayable,
        reuses_session_id,
        &context,
    ));

    let hint = ManagedRunRecoveryHint {
        replayable,
        reuses_session_id,
        suggested_action,
        note,
    };
    if hint.is_empty() { None } else { Some(hint) }
}

fn managed_continuation_boundary_phrase(
    boundary: ManagedRunContinuationBoundaryKind,
) -> &'static str {
    match boundary {
        ManagedRunContinuationBoundaryKind::UserCheckpointed => "user-input",
        ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed => {
            "final-assistant-response"
        }
        ManagedRunContinuationBoundaryKind::PendingToolCalls => "pending-tool-calls",
        ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed => "tool-results",
    }
}

fn browser_action_is_replay_safe(action: &str) -> bool {
    matches!(action, "snapshot" | "extract_text" | "wait" | "close")
}

fn browser_action_requires_live_session(action: &str) -> bool {
    action != "close"
}

pub fn managed_run_cleanup_failure_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunCleanupFailureSummary> {
    if event.kind != ManagedRunEventKind::RunCleanupFailed {
        return None;
    }

    let metadata = event.metadata.as_ref()?;
    let summary: ManagedRunCleanupFailureSummary = serde_json::from_value(metadata.clone()).ok()?;
    if summary.is_empty() {
        return None;
    }
    Some(summary)
}

pub fn managed_run_interruption_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunInterruptionSummary> {
    if event.kind != ManagedRunEventKind::RunInterrupted {
        return None;
    }

    if let Some(metadata) = event.metadata.as_ref() {
        if let Ok(summary) = serde_json::from_value(metadata.clone()) {
            return Some(summary);
        }
    }

    let message = event.message.as_deref().unwrap_or_default();
    Some(if message.contains("lease expired") {
        ManagedRunInterruptionSummary {
            cause: ManagedRunInterruptionCause::LeaseExpired,
            owner_worker_id: None,
            owner_claimed_at: None,
            owner_last_heartbeat_at: None,
            owner_lease_expires_at: None,
        }
    } else {
        ManagedRunInterruptionSummary {
            cause: ManagedRunInterruptionCause::OwnershipNotEstablished,
            owner_worker_id: None,
            owner_claimed_at: None,
            owner_last_heartbeat_at: None,
            owner_lease_expires_at: None,
        }
    })
}

pub fn managed_run_ownership_release_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunOwnershipReleaseSummary> {
    if event.kind != ManagedRunEventKind::RunOwnershipReleased {
        return None;
    }

    let metadata = event.metadata.as_ref()?;
    serde_json::from_value(metadata.clone()).ok()
}

pub fn managed_run_ownership_claim_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunOwnershipClaimSummary> {
    if event.kind != ManagedRunEventKind::RunOwnershipClaimed {
        return None;
    }

    let metadata = event.metadata.as_ref()?;
    serde_json::from_value(metadata.clone()).ok()
}

pub fn managed_run_continuation_checkpoint_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunContinuationCheckpointSummary> {
    if event.kind != ManagedRunEventKind::RunContinuationCheckpoint {
        return None;
    }

    let metadata = event.metadata.as_ref()?;
    serde_json::from_value(metadata.clone()).ok()
}

pub fn managed_run_provider_call_fence_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunProviderCallFenceSummary> {
    if event.kind != ManagedRunEventKind::RunProviderCallStarted {
        return None;
    }

    let metadata = event.metadata.as_ref()?;
    serde_json::from_value(metadata.clone()).ok()
}

pub fn managed_run_process_handoff_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunProcessHandoffSummary> {
    let metadata = event.metadata.as_ref()?;
    let payload: ProcessExecutionEvent = serde_json::from_value(metadata.clone()).ok()?;
    let tool_name = event.tool_name.clone()?;

    let (state, replay_disposition) = match event.kind {
        ManagedRunEventKind::ToolProcessStarted => (
            ManagedRunProcessHandoffState::Running,
            ManagedRunProcessReplayDisposition::UnsafeSideEffectWindow,
        ),
        ManagedRunEventKind::ToolProcessCompleted => (
            ManagedRunProcessHandoffState::Completed,
            ManagedRunProcessReplayDisposition::CompletedButNotRecorded,
        ),
        ManagedRunEventKind::ToolProcessFailed => (
            ManagedRunProcessHandoffState::Failed,
            ManagedRunProcessReplayDisposition::CompletedButNotRecorded,
        ),
        ManagedRunEventKind::ToolProcessTimedOut => (
            ManagedRunProcessHandoffState::TimedOut,
            ManagedRunProcessReplayDisposition::CompletedButNotRecorded,
        ),
        _ => return None,
    };

    Some(ManagedRunProcessHandoffSummary {
        tool_name,
        tool_call_id: event.tool_call_id.clone(),
        state,
        replay_disposition,
        process_group: payload.process_group,
        timeout_secs: payload.timeout_secs,
        exit_code: payload.exit_code,
        stdout_preview: payload.stdout_preview,
        stderr_preview: payload.stderr_preview,
        note: payload.note,
    })
}

pub fn managed_run_browser_handoff_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunBrowserHandoffSummary> {
    let metadata = event.metadata.as_ref()?;
    let payload: BrowserActionEvent = serde_json::from_value(metadata.clone()).ok()?;
    let replay_safe = browser_action_is_replay_safe(&payload.action);

    let (state, replay_disposition) = match event.kind {
        ManagedRunEventKind::ToolBrowserActionStarted => (
            ManagedRunBrowserHandoffState::Started,
            if replay_safe {
                ManagedRunBrowserReplayDisposition::SafeToReplay
            } else {
                ManagedRunBrowserReplayDisposition::UnsafeSideEffectWindow
            },
        ),
        ManagedRunEventKind::ToolBrowserActionCompleted => (
            ManagedRunBrowserHandoffState::Completed,
            if replay_safe {
                ManagedRunBrowserReplayDisposition::SafeToReplay
            } else {
                ManagedRunBrowserReplayDisposition::CompletedButNotRecorded
            },
        ),
        ManagedRunEventKind::ToolBrowserActionFailed => (
            ManagedRunBrowserHandoffState::Failed,
            if replay_safe {
                ManagedRunBrowserReplayDisposition::SafeToReplay
            } else {
                ManagedRunBrowserReplayDisposition::CompletedButNotRecorded
            },
        ),
        _ => return None,
    };

    Some(ManagedRunBrowserHandoffSummary {
        action: payload.action,
        state,
        replay_disposition,
        target: payload.target,
        wait_for_navigation: payload.wait_for_navigation,
        page_url: payload.page_url,
        page_title: payload.page_title,
        output_preview: payload.output_preview,
        note: payload.note,
    })
}

pub fn managed_run_browser_session_checkpoint_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunBrowserSessionCheckpointSummary> {
    if event.kind != ManagedRunEventKind::RunBrowserSessionCheckpoint {
        return None;
    }

    let metadata = event.metadata.as_ref()?;
    serde_json::from_value(metadata.clone()).ok()
}

pub fn managed_run_mcp_handoff_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunMcpHandoffSummary> {
    match event.kind {
        ManagedRunEventKind::ToolMcpCallStarted
        | ManagedRunEventKind::ToolMcpCallCompleted
        | ManagedRunEventKind::ToolMcpCallFailed => {}
        _ => return None,
    }

    let metadata = event.metadata.as_ref()?;
    serde_json::from_value(metadata.clone()).ok()
}

pub fn managed_run_mcp_runtime_checkpoint_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunMcpRuntimeCheckpointSummary> {
    if event.kind != ManagedRunEventKind::RunMcpRuntimeCheckpoint {
        return None;
    }

    let metadata = event.metadata.as_ref()?;
    serde_json::from_value(metadata.clone()).ok()
}

pub fn managed_run_takeover_assessment_from_event(
    event: &ManagedRunEvent,
) -> Option<ManagedRunTakeoverAssessmentSummary> {
    if event.kind != ManagedRunEventKind::RunTakeoverAssessed {
        return None;
    }

    let metadata = event.metadata.as_ref()?;
    let mut summary: ManagedRunTakeoverAssessmentSummary =
        serde_json::from_value(metadata.clone()).ok()?;
    if summary.takeover_lineage_id.is_none() {
        summary.takeover_lineage_id = metadata
            .get("replay_run_id")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned);
    }
    Some(summary)
}

pub async fn load_managed_run_derived_summary(
    store: &ManagedStore,
    run_id: &str,
) -> Result<ManagedRunDerivedSummary> {
    let run = store.get_run(run_id).await?;
    let ownership = store.get_run_owner_snapshot(run_id).await?;
    let created_event = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunCreated)
        .await?;
    let latest_replay_child = store.get_latest_replay_child(run_id).await?;
    let latest_replay_descendant = store.get_latest_replay_descendant(run_id, 64).await?;
    let replay_child_event = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunReplayed)
        .await?;
    let recovery_decision_event = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunRecoveryDecision)
        .await?;
    let takeover_assessment_event = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunTakeoverAssessed)
        .await?;
    let takeover_established_event = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunTakeoverEstablished)
        .await?;
    let (artifact_lineage, artifacts) = store
        .list_run_artifacts_with_replay_lineage(run_id, 1, 64)
        .await?;
    let mcp_event = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunMcpAdmissionRejected)
        .await?;
    let cleanup_event = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunCleanupFailed)
        .await?;
    let interruption_event = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunInterrupted)
        .await?;
    let ownership_claim_event = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunOwnershipClaimed)
        .await?;
    let ownership_release_event = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunOwnershipReleased)
        .await?;
    let continuation_event = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunContinuationCheckpoint)
        .await?;
    let browser_session_checkpoint_event = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunBrowserSessionCheckpoint)
        .await?;
    let mcp_runtime_checkpoint_event = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunMcpRuntimeCheckpoint)
        .await?;
    let provider_call_event = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunProviderCallStarted)
        .await?;
    let mut latest_process_event = None;
    for kind in [
        ManagedRunEventKind::ToolProcessStarted,
        ManagedRunEventKind::ToolProcessCompleted,
        ManagedRunEventKind::ToolProcessFailed,
        ManagedRunEventKind::ToolProcessTimedOut,
    ] {
        if let Some(event) = store.get_latest_run_event_by_kind(run_id, kind).await? {
            let replace = latest_process_event
                .as_ref()
                .map(|current: &ManagedRunEvent| event.id > current.id)
                .unwrap_or(true);
            if replace {
                latest_process_event = Some(event);
            }
        }
    }
    let mut latest_browser_event = None;
    for kind in [
        ManagedRunEventKind::ToolBrowserActionStarted,
        ManagedRunEventKind::ToolBrowserActionCompleted,
        ManagedRunEventKind::ToolBrowserActionFailed,
    ] {
        if let Some(event) = store.get_latest_run_event_by_kind(run_id, kind).await? {
            let replace = latest_browser_event
                .as_ref()
                .map(|current: &ManagedRunEvent| event.id > current.id)
                .unwrap_or(true);
            if replace {
                latest_browser_event = Some(event);
            }
        }
    }
    let mut latest_mcp_event = None;
    for kind in [
        ManagedRunEventKind::ToolMcpCallStarted,
        ManagedRunEventKind::ToolMcpCallCompleted,
        ManagedRunEventKind::ToolMcpCallFailed,
    ] {
        if let Some(event) = store.get_latest_run_event_by_kind(run_id, kind).await? {
            let replace = latest_mcp_event
                .as_ref()
                .map(|current: &ManagedRunEvent| event.id > current.id)
                .unwrap_or(true);
            if replace {
                latest_mcp_event = Some(event);
            }
        }
    }
    let continuation_checkpoint = continuation_event
        .as_ref()
        .and_then(managed_run_continuation_checkpoint_from_event);
    let provider_call_fence = match (
        provider_call_event
            .as_ref()
            .and_then(managed_run_provider_call_fence_from_event),
        provider_call_event.as_ref().map(|event| event.id),
        continuation_event.as_ref().map(|event| event.id),
    ) {
        (Some(fence), Some(provider_id), Some(checkpoint_id)) if provider_id > checkpoint_id => {
            Some(fence)
        }
        (Some(fence), Some(_), None) => Some(fence),
        _ => None,
    };
    let process_handoff = match (
        latest_process_event
            .as_ref()
            .and_then(managed_run_process_handoff_from_event),
        latest_process_event.as_ref().map(|event| event.id),
        continuation_event.as_ref().map(|event| event.id),
    ) {
        (Some(handoff), Some(process_id), Some(checkpoint_id)) if process_id > checkpoint_id => {
            Some(handoff)
        }
        (Some(handoff), Some(_), None) => Some(handoff),
        _ => None,
    };
    let browser_handoff = match (
        latest_browser_event
            .as_ref()
            .and_then(managed_run_browser_handoff_from_event),
        latest_browser_event.as_ref().map(|event| event.id),
        continuation_event.as_ref().map(|event| event.id),
    ) {
        (Some(handoff), Some(browser_id), Some(checkpoint_id)) if browser_id > checkpoint_id => {
            Some(handoff)
        }
        (Some(handoff), Some(_), None) => Some(handoff),
        _ => None,
    };
    let browser_session_checkpoint = browser_session_checkpoint_event
        .as_ref()
        .and_then(managed_run_browser_session_checkpoint_from_event);
    let mcp_runtime_checkpoint = mcp_runtime_checkpoint_event
        .as_ref()
        .and_then(managed_run_mcp_runtime_checkpoint_from_event);
    let artifact_continuity = artifacts.into_iter().last().map(|artifact| {
        let lineage_depth = artifact_lineage
            .iter()
            .position(|lineage_run| lineage_run.id == artifact.run_id)
            .map(|position| artifact_lineage.len().saturating_sub(position + 1))
            .unwrap_or_default();
        managed_run_artifact_continuity_summary(run_id, lineage_depth, artifact)
    });
    let replay_child = if let Some(event) = replay_child_event.as_ref() {
        managed_run_replay_child_from_event(
            event,
            latest_replay_child
                .as_ref()
                .map(|(_, replay_child_count)| *replay_child_count),
        )
    } else {
        match latest_replay_child {
            Some((child, replay_child_count)) => {
                let child_created_event = store
                    .get_latest_run_event_by_kind(&child.id, ManagedRunEventKind::RunCreated)
                    .await?;
                let child_takeover_established_event = store
                    .get_latest_run_event_by_kind(
                        &child.id,
                        ManagedRunEventKind::RunTakeoverEstablished,
                    )
                    .await?;
                let child_ownership_claimed_event = store
                    .get_latest_run_event_by_kind(
                        &child.id,
                        ManagedRunEventKind::RunOwnershipClaimed,
                    )
                    .await?;
                let child_provenance = child_created_event
                    .as_ref()
                    .and_then(managed_run_replay_provenance_from_event);
                (child_takeover_established_event.is_some()
                    || child_ownership_claimed_event.is_some())
                .then(|| {
                    managed_run_replay_child_summary(
                        &child,
                        replay_child_count,
                        child_provenance.as_ref(),
                    )
                })
            }
            None => None,
        }
    };
    let mcp_handoff = match (
        latest_mcp_event
            .as_ref()
            .and_then(managed_run_mcp_handoff_from_event),
        latest_mcp_event.as_ref().map(|event| event.id),
        continuation_event.as_ref().map(|event| event.id),
    ) {
        (Some(handoff), Some(mcp_id), Some(checkpoint_id)) if mcp_id > checkpoint_id => {
            Some(handoff)
        }
        (Some(handoff), Some(_), None) => Some(handoff),
        _ => None,
    };

    let mut summary = ManagedRunDerivedSummary {
        mcp_admission_rejection: mcp_event
            .as_ref()
            .and_then(managed_mcp_admission_rejection_from_event),
        cleanup_failure: cleanup_event
            .as_ref()
            .and_then(managed_run_cleanup_failure_from_event),
        interruption: interruption_event
            .as_ref()
            .and_then(managed_run_interruption_from_event),
        ownership_claim: ownership_claim_event
            .as_ref()
            .and_then(managed_run_ownership_claim_from_event),
        ownership_release: ownership_release_event
            .as_ref()
            .and_then(managed_run_ownership_release_from_event),
        continuation_checkpoint: continuation_checkpoint.clone(),
        provider_call_fence: provider_call_fence.clone(),
        process_handoff: process_handoff.clone(),
        browser_handoff: browser_handoff.clone(),
        browser_session_checkpoint: browser_session_checkpoint.clone(),
        mcp_handoff: mcp_handoff.clone(),
        mcp_runtime_checkpoint: mcp_runtime_checkpoint.clone(),
        artifact_continuity: artifact_continuity.clone(),
        replay_provenance: created_event
            .as_ref()
            .and_then(managed_run_replay_provenance_from_event),
        continuation: None,
        replay_child,
        ownership,
        takeover: None,
        takeover_assessment: takeover_assessment_event
            .as_ref()
            .and_then(managed_run_takeover_assessment_from_event),
        recovery_decision: recovery_decision_event
            .as_ref()
            .and_then(managed_run_recovery_decision_from_event),
        recovery_hint: None,
    };
    if let Some(continuation) = takeover_established_event
        .as_ref()
        .and_then(managed_run_continuation_from_event)
    {
        summary.continuation = Some(continuation);
    } else if let Some(provenance) = summary.replay_provenance.as_ref() {
        let source_recovery_decision =
            match run.as_ref().and_then(|run| run.replay_of_run_id.as_ref()) {
                Some(source_run_id) => store
                    .get_latest_run_event_by_kind(
                        source_run_id,
                        ManagedRunEventKind::RunRecoveryDecision,
                    )
                    .await?
                    .as_ref()
                    .and_then(managed_run_recovery_decision_from_event)
                    .filter(|decision| {
                        decision
                            .replay_run_id
                            .as_deref()
                            .map(|replay_run_id| replay_run_id == run_id)
                            .unwrap_or(false)
                    }),
                None => None,
            };
        summary.continuation = Some(legacy_managed_run_continuation_summary(
            provenance,
            source_recovery_decision.as_ref(),
        ));
    }
    if let Some((leaf_run, lineage_depth)) = latest_replay_descendant.as_ref() {
        let (
            leaf_provenance,
            leaf_continuation,
            current_owner,
            follow_target_ownership_claim,
            follow_target_recovery_decision,
            follow_target_continuation_checkpoint,
            follow_target_provider_call_fence,
            follow_target_process_handoff,
            follow_target_browser_handoff,
            follow_target_browser_session_checkpoint,
            follow_target_mcp_handoff,
            follow_target_mcp_runtime_checkpoint,
            follow_target_artifact_continuity,
            follow_target_takeover_assessment,
            follow_target_ownership_release,
        ) = load_replay_run_takeover_context(store, leaf_run).await?;
        if let Some(replay_child) = summary.replay_child.as_ref() {
            summary.takeover = managed_run_takeover_summary(
                leaf_run,
                replay_child.replay_child_count,
                *lineage_depth,
                ManagedRunTakeoverContext {
                    replay_provenance: leaf_provenance.as_ref(),
                    continuation: leaf_continuation.as_ref(),
                    recovery_decision: summary.recovery_decision.as_ref(),
                    current_owner,
                    follow_target_ownership_claim,
                    follow_target_recovery_decision,
                    follow_target_continuation_checkpoint,
                    follow_target_provider_call_fence,
                    follow_target_process_handoff,
                    follow_target_browser_handoff,
                    follow_target_browser_session_checkpoint,
                    follow_target_mcp_handoff,
                    follow_target_mcp_runtime_checkpoint,
                    follow_target_artifact_continuity,
                    follow_target_takeover_assessment,
                    follow_target_ownership_release,
                },
            );
        }
    }
    align_follow_replay_decision_with_takeover(
        &mut summary.recovery_decision,
        summary.takeover.as_ref(),
    );
    summary.recovery_hint = run
        .as_ref()
        .and_then(|run| managed_run_recovery_hint_from_run(run, &summary));
    Ok(summary)
}

pub async fn load_managed_run_derived_summaries(
    store: &ManagedStore,
    runs: &[ManagedRun],
) -> Result<BTreeMap<String, ManagedRunDerivedSummary>> {
    let mut summaries = BTreeMap::new();
    for run in runs {
        let summary = load_managed_run_derived_summary(store, &run.id).await?;
        if !summary.is_empty() {
            summaries.insert(run.id.clone(), summary);
        }
    }
    Ok(summaries)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tempfile::TempDir;

    use super::*;
    use crate::store::ManagedStore;
    use crate::types::{
        ManagedAgent, ManagedAgentVersion, ManagedRun, ManagedRunArtifactDraft,
        ManagedRunArtifactKind, ManagedRunEventDraft, ManagedRunStatus,
    };

    async fn temp_store() -> (TempDir, ManagedStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ManagedStore::open_at(&dir.path().join("state.db"))
            .await
            .unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_latest_mcp_rejection() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("summary-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Failed;
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunMcpAdmissionRejected,
                    message: Some("Managed MCP admission rejected: disabled_by_operator_policy".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "code": "disabled_by_operator_policy",
                        "error": "managed MCP tools are disabled by operator policy: mcp_resource_read",
                        "requested_tools": ["mcp_resource_read"],
                        "requested_read_only_tools": ["mcp_resource_read"],
                        "requested_side_effect_tools": [],
                        "requested_dynamic_tools": [],
                        "allowed_servers": [],
                        "allowed_transports": [],
                        "allow_side_effects": false,
                        "allowed_stdio_servers": [],
                        "allowed_stdio_env_keys": [],
                        "stdio_server_summaries": []
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .mcp_admission_rejection
                .as_ref()
                .map(|rejection| rejection.code.as_str()),
            Some("disabled_by_operator_policy")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_latest_cleanup_failure() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("cleanup-summary-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Failed;
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCleanupFailed,
                    message: Some("Managed run cleanup failed for 1 resource(s)".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "phase": "terminal_cleanup",
                        "attempted": 2,
                        "cleaned": 1,
                        "failures": ["failed to clean durable resource"],
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .cleanup_failure
                .as_ref()
                .map(|cleanup| cleanup.phase.as_str()),
            Some("terminal_cleanup")
        );
        assert_eq!(
            summary
                .cleanup_failure
                .as_ref()
                .map(|cleanup| cleanup.failures.len()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_structured_interruption() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("interruption-summary-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        store.create_run(&run).await.unwrap();

        let claimed_at = Utc::now() - chrono::Duration::seconds(30);
        let last_heartbeat_at = Utc::now() - chrono::Duration::seconds(10);
        let lease_expires_at = Utc::now() - chrono::Duration::seconds(5);
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunInterrupted,
                    message: Some(
                        "managed run interrupted after worker lease expired during execution"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "cause": "lease_expired",
                        "owner_worker_id": "gw_expired",
                        "owner_claimed_at": claimed_at,
                        "owner_last_heartbeat_at": last_heartbeat_at,
                        "owner_lease_expires_at": lease_expires_at,
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary.interruption.as_ref().map(|value| value.cause),
            Some(ManagedRunInterruptionCause::LeaseExpired)
        );
        assert_eq!(
            summary
                .interruption
                .as_ref()
                .and_then(|value| value.owner_worker_id.as_deref()),
            Some("gw_expired")
        );
        assert_eq!(
            summary
                .interruption
                .as_ref()
                .and_then(|value| value.owner_lease_expires_at),
            Some(lease_expires_at)
        );
    }

    #[test]
    fn managed_run_interruption_from_event_falls_back_to_legacy_message() {
        let event = ManagedRunEvent {
            id: 1,
            run_id: "run_legacy".to_string(),
            kind: ManagedRunEventKind::RunInterrupted,
            message: Some(
                "managed run interrupted before managed run ownership was established".to_string(),
            ),
            tool_name: None,
            tool_call_id: None,
            metadata: None,
            created_at: Utc::now(),
        };

        let summary = managed_run_interruption_from_event(&event)
            .expect("legacy interrupted event should still parse");
        assert_eq!(
            summary.cause,
            ManagedRunInterruptionCause::OwnershipNotEstablished
        );
        assert!(summary.owner_worker_id.is_none());
    }

    #[test]
    fn managed_run_recovery_decision_from_event_prefers_direct_replay_child_for_legacy_lineage() {
        let event = ManagedRunEvent {
            id: 2,
            run_id: "run_source".to_string(),
            kind: ManagedRunEventKind::RunRecoveryDecision,
            message: Some("continuation is owned by replay descendant run_leaf at depth 2".into()),
            tool_name: None,
            tool_call_id: None,
            metadata: Some(serde_json::json!({
                "decision": "follow_replay",
                "replay_run_id": "run_child",
                "active_follow_target_run_id": "run_leaf",
                "active_follow_target_status": "running",
                "active_follow_target_lineage_depth": 2,
            })),
            created_at: Utc::now(),
        };

        let decision = managed_run_recovery_decision_from_event(&event)
            .expect("legacy follow_replay decision should parse");
        assert_eq!(decision.replay_run_id.as_deref(), Some("run_child"));
        assert_eq!(decision.takeover_lineage_id.as_deref(), Some("run_child"));
        assert_eq!(
            decision.active_follow_target_run_id.as_deref(),
            Some("run_leaf")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_takeover_assessment() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("takeover-assessment-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        store.create_run(&run).await.unwrap();

        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunTakeoverAssessed,
                    message: Some(
                        "worker gw_eval assessed interrupted run takeover with 2 unresolved replay risks"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "takeover_lineage_id": "lineage_takeover_1",
                        "evaluated_by_worker_id": "gw_eval",
                        "source_boundary": "tool_results_checkpointed",
                        "interruption_cause": "lease_expired",
                        "provider_call_in_flight": true,
                        "process_handoff_risk": false,
                        "browser_handoff_risk": false,
                        "browser_session_state": false,
                        "mcp_handoff_risk": true,
                        "mcp_runtime_state": false,
                        "replay_depth": 1,
                        "max_auto_replays": 3,
                        "note": "provider call was already dispatched before interruption",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .takeover_assessment
                .as_ref()
                .and_then(|value| value.takeover_lineage_id.as_deref()),
            Some("lineage_takeover_1")
        );
        assert_eq!(
            summary
                .takeover_assessment
                .as_ref()
                .and_then(|value| value.evaluated_by_worker_id.as_deref()),
            Some("gw_eval")
        );
        assert_eq!(
            summary
                .takeover_assessment
                .as_ref()
                .and_then(|value| value.interruption_cause),
            Some(ManagedRunInterruptionCause::LeaseExpired)
        );
        assert_eq!(
            summary
                .takeover_assessment
                .as_ref()
                .map(|value| value.provider_call_in_flight),
            Some(true)
        );
        assert_eq!(
            summary
                .takeover_assessment
                .as_ref()
                .map(|value| value.mcp_handoff_risk),
            Some(true)
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_latest_continuation_checkpoint() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("continuation-summary-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry this run".to_string();
        run.session_id = Some("session_123".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunContinuationCheckpoint,
                    message: Some(
                        "managed run checkpointed after final assistant response".to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "kind": "assistant_response_checkpointed",
                        "safe_action": "complete_turn",
                        "history_len": 2,
                        "pending_tool_calls": 0,
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .continuation_checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.kind),
            Some(ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed)
        );
        assert_eq!(
            summary
                .continuation_checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.safe_action),
            Some(ManagedRunContinuationAction::CompleteTurn)
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_unresolved_provider_call_fence() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("provider-fence-summary-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry this run".to_string();
        run.session_id = Some("session_123".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunContinuationCheckpoint,
                    message: Some("managed run checkpointed after user input".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "kind": "user_checkpointed",
                        "safe_action": "call_provider",
                        "history_len": 1,
                        "pending_tool_calls": 0,
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunProviderCallStarted,
                    message: Some(
                        "managed run provider call started from user checkpointed boundary"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "request_history_len": 1,
                        "tool_count": 0,
                        "safe_resume_from": {
                            "kind": "user_checkpointed",
                            "safe_action": "call_provider",
                            "history_len": 1,
                            "pending_tool_calls": 0,
                        },
                        "note": "provider call dispatched before a newer durable response checkpoint",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .provider_call_fence
                .as_ref()
                .map(|fence| fence.safe_resume_from.kind),
            Some(ManagedRunContinuationBoundaryKind::UserCheckpointed)
        );
        assert!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.note.as_deref())
                .unwrap_or_default()
                .contains("re-issue the last provider call")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_recovery_hint_for_interrupted_run() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("recovery-summary-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry this run".to_string();
        run.session_id = Some("session_123".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunContinuationCheckpoint,
                    message: Some(
                        "managed run checkpointed with 1 pending tool call(s)".to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "kind": "pending_tool_calls",
                        "safe_action": "execute_pending_tools",
                        "history_len": 2,
                        "pending_tool_calls": 1,
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.suggested_action.as_deref()),
            Some("replay")
        );
        assert_eq!(
            summary
                .recovery_hint
                .as_ref()
                .map(|hint| hint.reuses_session_id),
            Some(true)
        );
        assert!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.note.as_deref())
                .unwrap_or_default()
                .contains("pending tool call")
        );
        assert!(summary.provider_call_fence.is_none());
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_artifact_continuity_from_current_run() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("artifact-current-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry this run".to_string();
        run.session_id = Some("session_123".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_artifact(
                &run.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::AssistantOutput,
                    label: "assistant_output".to_string(),
                    tool_name: None,
                    tool_call_id: None,
                    content: "Checkpointed assistant response".to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();
        store
            .append_run_artifact(
                &run.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::ToolOutput,
                    label: "browser.extract_text".to_string(),
                    tool_name: Some("browser".to_string()),
                    tool_call_id: Some("call_browser_2".to_string()),
                    content: "Latest tool artifact".to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .artifact_continuity
                .as_ref()
                .map(|artifact| artifact.latest_kind.clone()),
            Some(ManagedRunArtifactKind::ToolOutput)
        );
        assert_eq!(
            summary
                .artifact_continuity
                .as_ref()
                .map(|artifact| artifact.latest_run_is_current),
            Some(true)
        );
        assert_eq!(
            summary
                .artifact_continuity
                .as_ref()
                .map(|artifact| artifact.lineage_depth),
            Some(0)
        );
        assert!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.note.as_deref())
                .unwrap_or_default()
                .contains("latest checkpointed tool output")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_artifact_continuity_from_replay_lineage() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("artifact-lineage-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut root = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        root.status = ManagedRunStatus::Completed;
        store.create_run(&root).await.unwrap();
        store
            .append_run_artifact(
                &root.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::ToolOutput,
                    label: "browser.extract_text".to_string(),
                    tool_name: Some("browser".to_string()),
                    tool_call_id: Some("call_browser_1".to_string()),
                    content: "Ready".to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();

        let mut replay = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        replay.status = ManagedRunStatus::Interrupted;
        replay.prompt = "Retry this run".to_string();
        replay.session_id = Some("session_456".to_string());
        replay.replay_of_run_id = Some(root.id.clone());
        store.create_run(&replay).await.unwrap();

        let summary = load_managed_run_derived_summary(&store, &replay.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .artifact_continuity
                .as_ref()
                .map(|artifact| artifact.latest_kind.clone()),
            Some(ManagedRunArtifactKind::ToolOutput)
        );
        assert_eq!(
            summary
                .artifact_continuity
                .as_ref()
                .map(|artifact| artifact.latest_run_id.as_str()),
            Some(root.id.as_str())
        );
        assert_eq!(
            summary
                .artifact_continuity
                .as_ref()
                .map(|artifact| artifact.latest_run_is_current),
            Some(false)
        );
        assert_eq!(
            summary
                .artifact_continuity
                .as_ref()
                .map(|artifact| artifact.lineage_depth),
            Some(1)
        );
        assert_eq!(
            summary
                .artifact_continuity
                .as_ref()
                .and_then(|artifact| artifact.latest_tool_name.as_deref()),
            Some("browser")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_replay_provenance() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("replay-provenance-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        run.replay_of_run_id = Some("run_source".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some(
                        "managed run replayed from run_source for replay-provenance-reader@1"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "prompt_chars": 12,
                        "session_id": "session_123",
                        "replay_of_run_id": "run_source",
                        "replay_root_run_id": "run_root",
                        "replay_depth": 2,
                        "replay_trigger": "interrupted_auto_replay",
                        "replay_trigger_worker_id": "worker_abc",
                        "replay_source_status": "interrupted",
                        "replay_source_interruption_cause": "lease_expired",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                        "replay_source_boundary": "pending_tool_calls",
                        "replay_note": "continued from interrupted safe point"
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .replay_provenance
                .as_ref()
                .map(|provenance| provenance.trigger),
            Some(ManagedRunReplayTrigger::InterruptedAutoReplay)
        );
        assert_eq!(
            summary
                .replay_provenance
                .as_ref()
                .map(|provenance| provenance.replay_depth),
            Some(2)
        );
        assert_eq!(
            summary
                .replay_provenance
                .as_ref()
                .and_then(|provenance| provenance.takeover_lineage_id.as_deref()),
            Some(run.id.as_str())
        );
        assert_eq!(
            summary
                .replay_provenance
                .as_ref()
                .map(|provenance| provenance.source_status.clone()),
            Some(Some(ManagedRunStatus::Interrupted))
        );
        assert_eq!(
            summary
                .replay_provenance
                .as_ref()
                .and_then(|provenance| provenance.source_interruption_cause),
            Some(ManagedRunInterruptionCause::LeaseExpired)
        );
        assert_eq!(
            summary
                .replay_provenance
                .as_ref()
                .map(|provenance| provenance.resumed_existing_turn),
            Some(true)
        );
        assert_eq!(
            summary
                .replay_provenance
                .as_ref()
                .and_then(|provenance| provenance.source_boundary),
            Some(ManagedRunContinuationBoundaryKind::PendingToolCalls)
        );
        assert_eq!(
            summary
                .continuation
                .as_ref()
                .map(|continuation| continuation.source_run_id.as_str()),
            Some("run_source")
        );
        assert_eq!(
            summary
                .continuation
                .as_ref()
                .map(|continuation| continuation.takeover_worker_id.as_deref()),
            Some(Some("worker_abc"))
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_prefers_takeover_established_event_for_continuation()
    {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("continuation-summary-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        run.replay_of_run_id = Some("run_source".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": "run_source",
                        "replay_root_run_id": "run_root",
                        "replay_depth": 2,
                        "replay_trigger": "interrupted_auto_replay",
                        "replay_trigger_worker_id": "worker_created",
                        "replay_source_status": "interrupted",
                        "replay_source_interruption_cause": "ownership_not_established",
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunTakeoverEstablished,
                    message: Some("takeover established".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "source_run_id": "run_source",
                        "root_run_id": "run_root",
                        "replay_depth": 2,
                        "trigger": "interrupted_auto_replay",
                        "source_status": "interrupted",
                        "source_interruption_cause": "lease_expired",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                        "source_boundary": "tool_results_checkpointed",
                        "evaluated_by_worker_id": "worker_eval",
                        "takeover_worker_id": "worker_takeover",
                        "note": "takeover event is authoritative"
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .continuation
                .as_ref()
                .map(|continuation| continuation.evaluated_by_worker_id.as_deref()),
            Some(Some("worker_eval"))
        );
        assert_eq!(
            summary
                .continuation
                .as_ref()
                .map(|continuation| continuation.takeover_worker_id.as_deref()),
            Some(Some("worker_takeover"))
        );
        assert_eq!(
            summary
                .continuation
                .as_ref()
                .and_then(|continuation| continuation.source_interruption_cause),
            Some(ManagedRunInterruptionCause::LeaseExpired)
        );
        assert_eq!(
            summary
                .continuation
                .as_ref()
                .and_then(|continuation| continuation.source_boundary),
            Some(ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed)
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_replay_child_for_interrupted_source() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("replay-child-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut source = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        source.status = ManagedRunStatus::Interrupted;
        source.prompt = "retry me".to_string();
        source.session_id = Some("session_source".to_string());
        store.create_run(&source).await.unwrap();
        let source_run_id = source.id.clone();

        let mut older_child = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        older_child.status = ManagedRunStatus::Completed;
        older_child.prompt = source.prompt.clone();
        older_child.replay_of_run_id = Some(source_run_id.clone());
        store.create_run(&older_child).await.unwrap();
        store
            .append_run_event(
                &older_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": source_run_id.as_str(),
                        "replay_root_run_id": source_run_id.as_str(),
                        "replay_depth": 1,
                        "replay_trigger": "manual_replay",
                    })),
                },
            )
            .await
            .unwrap();

        let mut latest_child = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        latest_child.status = ManagedRunStatus::Running;
        latest_child.prompt = source.prompt.clone();
        latest_child.session_id = source.session_id.clone();
        latest_child.replay_of_run_id = Some(source_run_id.clone());
        store.create_run(&latest_child).await.unwrap();
        let claimed_at = Utc::now();
        store
            .claim_run_ownership(
                &latest_child.id,
                "worker_replay_owner",
                "claim_replay_owner",
                claimed_at,
                claimed_at + chrono::Duration::seconds(30),
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &source.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunReplayed,
                    message: Some("managed run continued as replay child".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_run_id": latest_child.id.as_str(),
                        "replay_run_status": latest_child.status.as_str(),
                        "replay_root_run_id": source_run_id.as_str(),
                        "replay_trigger": "interrupted_auto_replay",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                        "replay_source_boundary": "tool_results_checkpointed",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &source.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .replay_child
                .as_ref()
                .map(|child| child.latest_run_id.as_str()),
            Some(latest_child.id.as_str())
        );
        assert_eq!(
            summary
                .replay_child
                .as_ref()
                .map(|child| child.replay_child_count),
            Some(2)
        );
        assert_eq!(
            summary
                .replay_child
                .as_ref()
                .and_then(|child| child.trigger),
            Some(ManagedRunReplayTrigger::InterruptedAutoReplay)
        );
        assert_eq!(
            summary
                .replay_child
                .as_ref()
                .and_then(|child| child.source_boundary),
            Some(ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed)
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .map(|takeover| takeover.replay_run_id.as_str()),
            Some(latest_child.id.as_str())
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .map(|takeover| takeover.takeover_state),
            Some(ManagedRunTakeoverState::Active)
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.current_owner.as_ref())
                .map(|owner| owner.worker_id.as_str()),
            Some("worker_replay_owner")
        );
        assert_eq!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.suggested_action.as_deref()),
            Some("follow_replay")
        );
        assert!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.note.as_deref())
                .is_some_and(|note| note.contains(latest_child.id.as_str()))
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_keeps_follow_replay_for_failed_child_lineage() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("takeover-terminal-lineage");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut source = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        source.status = ManagedRunStatus::Interrupted;
        source.prompt = "resume me".to_string();
        source.session_id = Some("managed_session_terminal".to_string());
        store.create_run(&source).await.unwrap();

        let mut child = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        child.status = ManagedRunStatus::Failed;
        child.prompt = source.prompt.clone();
        child.session_id = source.session_id.clone();
        child.replay_of_run_id = Some(source.id.clone());
        store.create_run(&child).await.unwrap();
        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": source.id.as_str(),
                        "replay_root_run_id": source.id.as_str(),
                        "replay_depth": 1,
                        "replay_trigger": "manual_replay",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &source.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunRecoveryDecision,
                    message: Some(format!(
                        "continuation is owned by replay child {}",
                        child.id
                    )),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "decision": "follow_replay",
                        "reason": "replay_child_active",
                        "replay_run_id": child.id.as_str(),
                        "evaluated_by_worker_id": "worker_eval_terminal",
                        "takeover_worker_id": "worker_takeover_terminal",
                        "worker_id": "worker_takeover_terminal",
                        "note": "follow the replay child lineage even if the latest replay child is terminal",
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &source.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunReplayed,
                    message: Some(format!("managed run replayed as {}", child.id)),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_run_id": child.id.as_str(),
                        "replay_run_status": "failed",
                        "replay_root_run_id": source.id.as_str(),
                        "replay_trigger": "manual_replay",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &source.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .takeover
                .as_ref()
                .map(|takeover| takeover.takeover_state),
            Some(ManagedRunTakeoverState::Failed)
        );
        assert_eq!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.suggested_action.as_deref()),
            Some("follow_replay")
        );
        assert!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.note.as_deref())
                .is_some_and(|note| note.contains("failed after taking over"))
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_surfaces_leaf_recovery_decision_in_takeover() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("takeover-leaf-recovery-decision");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut source = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        source.status = ManagedRunStatus::Interrupted;
        source.prompt = "resume me".to_string();
        source.session_id = Some("managed_session_leaf_decision".to_string());
        store.create_run(&source).await.unwrap();

        let mut child = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        child.status = ManagedRunStatus::Interrupted;
        child.prompt = source.prompt.clone();
        child.session_id = source.session_id.clone();
        child.replay_of_run_id = Some(source.id.clone());
        store.create_run(&child).await.unwrap();

        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": source.id.as_str(),
                        "replay_root_run_id": source.id.as_str(),
                        "replay_depth": 1,
                        "replay_trigger": "interrupted_auto_replay",
                        "replay_trigger_worker_id": "worker_leaf_eval",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunOwnershipClaimed,
                    message: Some("managed run ownership claimed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "worker_id": "worker_leaf_owner",
                        "claimed_at": "2026-04-23T12:08:00Z",
                        "lease_expires_at": "2026-04-23T12:09:00Z",
                        "takeover_lineage_id": child.id.as_str(),
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunContinuationCheckpoint,
                    message: Some("managed run checkpointed after tool results".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "kind": "tool_results_checkpointed",
                        "safe_action": "execute_pending_tools",
                        "history_len": 7,
                        "pending_tool_calls": 1,
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunProviderCallStarted,
                    message: Some(
                        "managed run provider call started from tool-results boundary"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "request_history_len": 8,
                        "tool_count": 0,
                        "safe_resume_from": {
                            "kind": "tool_results_checkpointed",
                            "safe_action": "execute_pending_tools",
                            "history_len": 7,
                            "pending_tool_calls": 1,
                        },
                        "note": "leaf provider call was dispatched after the last durable checkpoint",
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunRecoveryDecision,
                    message: Some(
                        "automatic replay requires manual review because a process handoff may already have produced side effects"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "decision": "manual_review",
                        "reason": "process_handoff_risk",
                        "evaluated_by_worker_id": "worker_leaf_eval",
                        "worker_id": "worker_leaf_eval",
                        "source_boundary": "tool_results_checkpointed",
                        "note": "leaf continuation now requires manual review",
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::ToolBrowserActionStarted,
                    message: Some("browser action 'click' started".to_string()),
                    tool_name: Some("browser".to_string()),
                    tool_call_id: Some("call_browser_1".to_string()),
                    metadata: Some(serde_json::json!({
                        "state": "started",
                        "action": "click",
                        "target": "#submit",
                        "wait_for_navigation": true,
                        "page_url": "https://example.com/form",
                        "page_title": "Form",
                        "note": "browser action 'click' started and may still have page or external side effects in flight",
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunBrowserSessionCheckpoint,
                    message: Some(
                        "browser session checkpointed after 'navigate' with live session state"
                            .to_string(),
                    ),
                    tool_name: Some("browser".to_string()),
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "action": "navigate",
                        "session_open": true,
                        "target": "https://example.com/dashboard",
                        "page_url": "https://example.com/dashboard",
                        "page_title": "Dashboard",
                        "note": "leaf browser session remained open after navigation",
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::ToolMcpCallStarted,
                    message: Some(
                        "MCP tool 'mcp_resource_subscribe' started and may still have runtime or external side effects in flight"
                            .to_string(),
                    ),
                    tool_name: Some("mcp_resource_subscribe".to_string()),
                    tool_call_id: Some("call_mcp_1".to_string()),
                    metadata: Some(serde_json::json!({
                        "tool_name": "mcp_resource_subscribe",
                        "state": "started",
                        "replay_disposition": "unsafe_side_effect_window",
                        "read_only": false,
                        "requires_live_runtime": true,
                        "server": "docs",
                        "transport": "http",
                        "target": "uri:docs://guide",
                        "note": "MCP tool 'mcp_resource_subscribe' started and may still have runtime or external side effects in flight",
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunTakeoverAssessed,
                    message: Some(
                        "worker worker_leaf_eval assessed interrupted run takeover with 1 blocking runtime risk"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "evaluated_by_worker_id": "worker_leaf_eval",
                        "source_boundary": "tool_results_checkpointed",
                        "interruption_cause": "lease_expired",
                        "provider_call_in_flight": false,
                        "process_handoff_risk": true,
                        "browser_handoff_risk": false,
                        "browser_session_state": false,
                        "mcp_handoff_risk": false,
                        "mcp_runtime_state": false,
                        "replay_depth": 1,
                        "max_auto_replays": 3,
                        "note": "leaf takeover assessment flagged process handoff risk",
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::ToolProcessStarted,
                    message: Some(
                        "terminal process group 4242 started before a durable tool-result checkpoint"
                            .to_string(),
                    ),
                    tool_name: Some("terminal".to_string()),
                    tool_call_id: Some("call_terminal_1".to_string()),
                    metadata: Some(serde_json::json!({
                        "state": "started",
                        "process_group": 4242,
                        "timeout_secs": 30,
                        "stdout_chars": 11,
                        "stderr_chars": 0,
                        "stdout_preview": "building...",
                        "note": "terminal process started and may still have side effects in flight",
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunOwnershipReleased,
                    message: Some(
                        "worker worker_leaf_owner released managed run ownership after it lost ownership when the run was interrupted"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "worker_id": "worker_leaf_owner",
                        "reason": "interrupted",
                        "note": "leaf owner released ownership after interruption",
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_artifact(
                &child.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::AssistantOutput,
                    label: "assistant_output".to_string(),
                    tool_name: None,
                    tool_call_id: None,
                    content: "Recovered answer preview".to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &source.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunRecoveryDecision,
                    message: Some(format!(
                        "continuation is owned by replay child {}",
                        child.id
                    )),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "decision": "follow_replay",
                        "reason": "replay_child_active",
                        "replay_run_id": child.id.as_str(),
                        "evaluated_by_worker_id": "worker_source_eval",
                        "takeover_worker_id": "worker_leaf_owner",
                        "worker_id": "worker_leaf_owner",
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &source.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunReplayed,
                    message: Some(format!("managed run replayed as {}", child.id)),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_run_id": child.id.as_str(),
                        "replay_run_status": "interrupted",
                        "replay_root_run_id": source.id.as_str(),
                        "replay_trigger": "interrupted_auto_replay",
                        "replay_trigger_worker_id": "worker_leaf_eval",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &source.id)
            .await
            .unwrap();

        let follow_target_recovery = summary
            .takeover
            .as_ref()
            .and_then(|takeover| takeover.follow_target_recovery_decision.as_ref())
            .expect("follow target recovery decision missing");
        assert_eq!(
            follow_target_recovery.decision,
            ManagedRunRecoveryDecisionKind::ManualReview
        );
        assert_eq!(
            follow_target_recovery.reason,
            Some(ManagedRunRecoveryDecisionReason::ProcessHandoffRisk)
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.follow_target_continuation_checkpoint.as_ref())
                .map(|checkpoint| checkpoint.kind),
            Some(ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed)
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.follow_target_provider_call_fence.as_ref())
                .map(|fence| fence.request_history_len),
            Some(8)
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.follow_target_browser_session_checkpoint.as_ref())
                .and_then(|checkpoint| checkpoint.page_url.as_deref()),
            Some("https://example.com/dashboard")
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.follow_target_process_handoff.as_ref())
                .map(|handoff| handoff.tool_name.as_str()),
            Some("terminal")
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.follow_target_browser_handoff.as_ref())
                .map(|handoff| handoff.action.as_str()),
            Some("click")
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.follow_target_mcp_handoff.as_ref())
                .map(|handoff| handoff.tool_name.as_str()),
            Some("mcp_resource_subscribe")
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.follow_target_process_handoff.as_ref())
                .and_then(|handoff| handoff.process_group),
            Some(4242)
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.follow_target_ownership_claim.as_ref())
                .map(|claim| claim.worker_id.as_str()),
            Some("worker_leaf_owner")
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.follow_target_ownership_claim.as_ref())
                .and_then(|claim| claim.takeover_lineage_id.as_deref()),
            Some(child.id.as_str())
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.follow_target_ownership_release.as_ref())
                .map(|release| release.worker_id.as_str()),
            Some("worker_leaf_owner")
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.follow_target_artifact_continuity.as_ref())
                .map(|artifact| artifact.latest_kind.clone()),
            Some(ManagedRunArtifactKind::AssistantOutput)
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.follow_target_artifact_continuity.as_ref())
                .and_then(|artifact| artifact.latest_content_preview.as_deref()),
            Some("Recovered answer preview")
        );
        assert!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.follow_target_takeover_assessment.as_ref())
                .is_some_and(|assessment| assessment.process_handoff_risk)
        );
        assert!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.note.as_deref())
                .is_some_and(|note| note.contains("automatic replay requires manual review"))
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_points_takeover_at_latest_replay_leaf() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("takeover-leaf-lineage");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut source = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        source.status = ManagedRunStatus::Interrupted;
        source.prompt = "resume me".to_string();
        source.session_id = Some("managed_session_leaf".to_string());
        store.create_run(&source).await.unwrap();

        let mut child = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        child.status = ManagedRunStatus::Interrupted;
        child.prompt = source.prompt.clone();
        child.session_id = source.session_id.clone();
        child.replay_of_run_id = Some(source.id.clone());
        store.create_run(&child).await.unwrap();
        store
            .append_run_event(
                &child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": source.id.as_str(),
                        "replay_root_run_id": source.id.as_str(),
                        "replay_depth": 1,
                        "replay_trigger": "manual_replay",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &source.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunRecoveryDecision,
                    message: Some(format!(
                        "continuation is owned by replay child {}",
                        child.id
                    )),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "decision": "follow_replay",
                        "reason": "replay_child_active",
                        "replay_run_id": child.id.as_str(),
                        "evaluated_by_worker_id": "worker_eval_source",
                        "takeover_worker_id": "worker_takeover_source",
                        "worker_id": "worker_takeover_source",
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &source.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunReplayed,
                    message: Some(format!("managed run replayed as {}", child.id)),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_run_id": child.id.as_str(),
                        "replay_run_status": "interrupted",
                        "replay_root_run_id": source.id.as_str(),
                        "replay_trigger": "manual_replay",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                    })),
                },
            )
            .await
            .unwrap();

        let mut grandchild = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        grandchild.status = ManagedRunStatus::Running;
        grandchild.prompt = source.prompt.clone();
        grandchild.session_id = source.session_id.clone();
        grandchild.replay_of_run_id = Some(child.id.clone());
        store.create_run(&grandchild).await.unwrap();
        store
            .append_run_event(
                &grandchild.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": child.id.as_str(),
                        "replay_root_run_id": source.id.as_str(),
                        "replay_depth": 2,
                        "replay_trigger": "interrupted_auto_replay",
                        "replay_trigger_worker_id": "worker_leaf_eval",
                        "replay_source_status": "interrupted",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                        "replay_source_boundary": "tool_results_checkpointed",
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &grandchild.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunTakeoverEstablished,
                    message: Some("managed replay takeover established".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(
                        serde_json::to_value(&ManagedRunContinuationSummary {
                            source_run_id: child.id.clone(),
                            root_run_id: source.id.clone(),
                            replay_depth: 2,
                            trigger: ManagedRunReplayTrigger::InterruptedAutoReplay,
                            takeover_lineage_id: Some(child.id.clone()),
                            source_status: Some(ManagedRunStatus::Interrupted),
                            source_interruption_cause: Some(
                                ManagedRunInterruptionCause::LeaseExpired,
                            ),
                            reused_session_id: true,
                            resumed_existing_turn: true,
                            source_boundary: Some(
                                ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed,
                            ),
                            evaluated_by_worker_id: Some("worker_leaf_eval".to_string()),
                            takeover_worker_id: Some("worker_leaf_owner".to_string()),
                            note: Some("grandchild took over continuation".to_string()),
                        })
                        .unwrap(),
                    ),
                },
            )
            .await
            .unwrap();
        let claimed_at = Utc::now();
        store
            .claim_run_ownership(
                &grandchild.id,
                "worker_leaf_owner",
                "claim_leaf_owner",
                claimed_at,
                claimed_at + chrono::Duration::seconds(30),
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &source.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .replay_child
                .as_ref()
                .map(|child_summary| child_summary.latest_run_id.as_str()),
            Some(child.id.as_str())
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .map(|takeover| takeover.replay_run_id.as_str()),
            Some(grandchild.id.as_str())
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .map(|takeover| takeover.lineage_depth),
            Some(2)
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.current_owner.as_ref())
                .map(|owner| owner.worker_id.as_str()),
            Some("worker_leaf_owner")
        );
        assert_eq!(
            summary
                .takeover
                .as_ref()
                .and_then(|takeover| takeover.takeover_worker_id.as_deref()),
            Some("worker_leaf_owner")
        );
        assert_eq!(
            summary
                .recovery_decision
                .as_ref()
                .and_then(|decision| decision.replay_run_id.as_deref()),
            Some(child.id.as_str())
        );
        assert_eq!(
            summary
                .recovery_decision
                .as_ref()
                .and_then(|decision| decision.active_follow_target_run_id.as_deref()),
            Some(grandchild.id.as_str())
        );
        assert_eq!(
            summary
                .recovery_decision
                .as_ref()
                .and_then(|decision| decision.active_follow_target_lineage_depth),
            Some(2)
        );
        assert_eq!(
            summary
                .recovery_decision
                .as_ref()
                .and_then(|decision| decision.takeover_worker_id.as_deref()),
            Some("worker_leaf_owner")
        );
        assert!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.note.as_deref())
                .is_some_and(|note| note.contains(grandchild.id.as_str()))
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_recovery_decision() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("recovery-decision-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "review me".to_string();
        store.create_run(&run).await.unwrap();

        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunRecoveryDecision,
                    message: Some(
                        "automatic replay requires manual review because a process handoff may already have produced side effects".to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "decision": "manual_review",
                        "reason": "process_handoff_risk",
                        "evaluated_by_worker_id": "worker_eval_123",
                        "worker_id": "worker_123",
                        "source_boundary": "pending_tool_calls",
                        "note": "automatic replay is blocked pending operator review",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .recovery_decision
                .as_ref()
                .map(|decision| decision.decision),
            Some(ManagedRunRecoveryDecisionKind::ManualReview)
        );
        assert_eq!(
            summary
                .recovery_decision
                .as_ref()
                .and_then(|decision| decision.reason),
            Some(ManagedRunRecoveryDecisionReason::ProcessHandoffRisk)
        );
        assert_eq!(
            summary
                .recovery_decision
                .as_ref()
                .and_then(|decision| decision.evaluated_by_worker_id.as_deref()),
            Some("worker_eval_123")
        );
        assert_eq!(
            summary
                .recovery_decision
                .as_ref()
                .and_then(|decision| decision.worker_id.as_deref()),
            Some("worker_123")
        );
        assert_eq!(
            summary
                .recovery_decision
                .as_ref()
                .and_then(|decision| decision.source_boundary),
            Some(ManagedRunContinuationBoundaryKind::PendingToolCalls)
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_active_run_ownership() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("ownership-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        run.prompt = "stay leased".to_string();
        store.create_run(&run).await.unwrap();

        let claimed_at = Utc::now();
        store
            .claim_run_ownership(
                &run.id,
                "worker_live_owner",
                "claim_live_owner",
                claimed_at,
                claimed_at + chrono::Duration::seconds(30),
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .ownership
                .as_ref()
                .map(|ownership| ownership.worker_id.as_str()),
            Some("worker_live_owner")
        );
        assert_eq!(
            summary.ownership.as_ref().map(|ownership| ownership.state),
            Some(ManagedRunOwnerState::Active)
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_latest_ownership_release() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("ownership-release-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Completed;
        run.prompt = "done".to_string();
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunOwnershipReleased,
                    message: Some(
                        "worker worker_release released managed run ownership after it completed the run"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "worker_id": "worker_release",
                        "reason": "completed",
                        "owner_claimed_at": Utc::now().to_rfc3339(),
                        "note": "ownership ended when managed run became completed",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .ownership_release
                .as_ref()
                .map(|release| release.worker_id.as_str()),
            Some("worker_release")
        );
        assert_eq!(
            summary
                .ownership_release
                .as_ref()
                .map(|release| release.reason),
            Some(ManagedRunOwnershipReleaseReason::Completed)
        );
        assert_eq!(
            summary
                .ownership_release
                .as_ref()
                .and_then(|release| release.note.as_deref()),
            Some("ownership ended when managed run became completed")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_running_process_handoff() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("process-handoff-running");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry this run".to_string();
        run.session_id = Some("session_123".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::ToolProcessStarted,
                    message: Some(
                        "process started and may produce side effects before interruption"
                            .to_string(),
                    ),
                    tool_name: Some("terminal".to_string()),
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "state": "started",
                        "process_group": 42,
                        "timeout_secs": 30,
                        "stdout_chars": 0,
                        "stderr_chars": 0,
                        "note": "process started and may produce side effects before interruption",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .process_handoff
                .as_ref()
                .map(|handoff| handoff.tool_name.as_str()),
            Some("terminal")
        );
        assert_eq!(
            summary
                .process_handoff
                .as_ref()
                .map(|handoff| handoff.state),
            Some(ManagedRunProcessHandoffState::Running)
        );
        assert_eq!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.suggested_action.as_deref()),
            Some("manual_review")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_completed_process_handoff_after_checkpoint_gap()
    {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("process-handoff-completed");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry this run".to_string();
        run.session_id = Some("session_123".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunContinuationCheckpoint,
                    message: Some("managed run checkpointed after user input".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "kind": "user_checkpointed",
                        "safe_action": "call_provider",
                        "history_len": 1,
                        "pending_tool_calls": 0,
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::ToolProcessCompleted,
                    message: Some(
                        "process completed before a newer durable tool-result checkpoint"
                            .to_string(),
                    ),
                    tool_name: Some("execute_code".to_string()),
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "state": "completed",
                        "process_group": 88,
                        "exit_code": 0,
                        "stdout_chars": 12,
                        "stderr_chars": 0,
                        "stdout_preview": "done\n",
                        "stderr_preview": null,
                        "note": "process completed before a newer durable tool-result checkpoint",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .process_handoff
                .as_ref()
                .map(|handoff| handoff.state),
            Some(ManagedRunProcessHandoffState::Completed)
        );
        assert_eq!(
            summary
                .process_handoff
                .as_ref()
                .map(|handoff| handoff.replay_disposition),
            Some(ManagedRunProcessReplayDisposition::CompletedButNotRecorded)
        );
        assert_eq!(
            summary
                .process_handoff
                .as_ref()
                .and_then(|handoff| handoff.stdout_preview.as_deref()),
            Some("done\n")
        );
        assert!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.note.as_deref())
                .unwrap_or_default()
                .contains("finished before its tool result was durably checkpointed")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_risky_browser_handoff() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("browser-handoff-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry browser action".to_string();
        run.session_id = Some("session_123".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::ToolBrowserActionStarted,
                    message: Some("browser action 'click' started".to_string()),
                    tool_name: Some("browser".to_string()),
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "state": "started",
                        "action": "click",
                        "target": "selector:#submit",
                        "wait_for_navigation": true,
                        "page_url": "file:///checkout.html",
                        "page_title": "Checkout",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .browser_handoff
                .as_ref()
                .map(|handoff| handoff.action.as_str()),
            Some("click")
        );
        assert_eq!(
            summary
                .browser_handoff
                .as_ref()
                .map(|handoff| handoff.replay_disposition),
            Some(ManagedRunBrowserReplayDisposition::UnsafeSideEffectWindow)
        );
        assert_eq!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.suggested_action.as_deref()),
            Some("manual_review")
        );
        assert!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.note.as_deref())
                .unwrap_or_default()
                .contains("browser action 'click'")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_safe_browser_handoff_after_checkpoint_gap() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("browser-safe-handoff-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry browser read".to_string();
        run.session_id = Some("session_123".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunContinuationCheckpoint,
                    message: Some("managed run checkpointed after tool results".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "kind": "tool_results_checkpointed",
                        "safe_action": "call_provider",
                        "history_len": 4,
                        "pending_tool_calls": 0,
                    })),
                },
            )
            .await
            .unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::ToolBrowserActionCompleted,
                    message: Some("browser action 'extract_text' completed".to_string()),
                    tool_name: Some("browser".to_string()),
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "state": "completed",
                        "action": "extract_text",
                        "target": "selector:#status",
                        "wait_for_navigation": false,
                        "page_url": "file:///status.html",
                        "page_title": "Status",
                        "output_preview": "Ready",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .browser_handoff
                .as_ref()
                .map(|handoff| handoff.action.as_str()),
            Some("extract_text")
        );
        assert_eq!(
            summary
                .browser_handoff
                .as_ref()
                .map(|handoff| handoff.replay_disposition),
            Some(ManagedRunBrowserReplayDisposition::SafeToReplay)
        );
        assert_eq!(
            summary
                .browser_handoff
                .as_ref()
                .and_then(|handoff| handoff.output_preview.as_deref()),
            Some("Ready")
        );
        assert_eq!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.suggested_action.as_deref()),
            Some("manual_review")
        );
        assert!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.note.as_deref())
                .unwrap_or_default()
                .contains("does not restore live browser session/page state")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_open_browser_session_checkpoint() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("browser-session-checkpoint-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry browser session".to_string();
        run.session_id = Some("session_123".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunBrowserSessionCheckpoint,
                    message: Some(
                        "browser session checkpointed after 'extract_text' with live session state"
                            .to_string(),
                    ),
                    tool_name: Some("browser".to_string()),
                    tool_call_id: Some("call_browser_1".to_string()),
                    metadata: Some(serde_json::json!({
                        "action": "extract_text",
                        "session_open": true,
                        "target": "selector:#status",
                        "page_url": "file:///status.html",
                        "page_title": "Status",
                        "output_preview": "Ready",
                        "note": "fresh browser session required on replay",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .browser_session_checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.action.as_str()),
            Some("extract_text")
        );
        assert_eq!(
            summary
                .browser_session_checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.session_open),
            Some(true)
        );
        assert_eq!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.suggested_action.as_deref()),
            Some("manual_review")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_closed_browser_session_checkpoint() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("browser-close-checkpoint-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry after close".to_string();
        run.session_id = Some("session_123".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunBrowserSessionCheckpoint,
                    message: Some(
                        "browser session checkpointed after 'close' with no live session state"
                            .to_string(),
                    ),
                    tool_name: Some("browser".to_string()),
                    tool_call_id: Some("call_browser_2".to_string()),
                    metadata: Some(serde_json::json!({
                        "action": "close",
                        "session_open": false,
                        "target": null,
                        "page_url": null,
                        "page_title": null,
                        "output_preview": "closed:true",
                        "note": "browser session was explicitly closed",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .browser_session_checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.session_open),
            Some(false)
        );
        assert_eq!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.suggested_action.as_deref()),
            Some("replay")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_safe_mcp_handoff() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("mcp-safe-handoff-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry MCP read".to_string();
        run.session_id = Some("session_123".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::ToolMcpCallCompleted,
                    message: Some(
                        "read-only MCP tool 'mcp_resource_read' completed before its tool result was durably checkpointed"
                            .to_string(),
                    ),
                    tool_name: Some("mcp_resource_read".to_string()),
                    tool_call_id: Some("call_mcp_1".to_string()),
                    metadata: Some(serde_json::json!({
                        "tool_name": "mcp_resource_read",
                        "state": "completed",
                        "replay_disposition": "safe_to_replay",
                        "read_only": true,
                        "requires_live_runtime": false,
                        "server": "docs",
                        "transport": "http",
                        "target": "uri:docs://guide",
                        "output_preview": "{\"server\":\"docs\"}",
                        "note": "read-only MCP request can be replayed from a fresh MCP session",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .mcp_handoff
                .as_ref()
                .map(|handoff| handoff.tool_name.as_str()),
            Some("mcp_resource_read")
        );
        assert_eq!(
            summary
                .mcp_handoff
                .as_ref()
                .map(|handoff| handoff.replay_disposition),
            Some(ManagedRunMcpReplayDisposition::SafeToReplay)
        );
        assert_eq!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.suggested_action.as_deref()),
            Some("replay")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_risky_mcp_handoff() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("mcp-risky-handoff-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry MCP subscribe".to_string();
        run.session_id = Some("session_123".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::ToolMcpCallStarted,
                    message: Some(
                        "MCP tool 'mcp_resource_subscribe' started and may still have runtime or external side effects in flight"
                            .to_string(),
                    ),
                    tool_name: Some("mcp_resource_subscribe".to_string()),
                    tool_call_id: Some("call_mcp_2".to_string()),
                    metadata: Some(serde_json::json!({
                        "tool_name": "mcp_resource_subscribe",
                        "state": "started",
                        "replay_disposition": "unsafe_side_effect_window",
                        "read_only": false,
                        "requires_live_runtime": true,
                        "server": "docs",
                        "transport": "stdio",
                        "target": "uri:docs://guide",
                        "note": "MCP subscription state may already have changed before interruption",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .mcp_handoff
                .as_ref()
                .map(|handoff| handoff.tool_name.as_str()),
            Some("mcp_resource_subscribe")
        );
        assert_eq!(
            summary
                .mcp_handoff
                .as_ref()
                .map(|handoff| handoff.replay_disposition),
            Some(ManagedRunMcpReplayDisposition::UnsafeSideEffectWindow)
        );
        assert_eq!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.suggested_action.as_deref()),
            Some("manual_review")
        );
        assert!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.note.as_deref())
                .unwrap_or_default()
                .contains("depends on live MCP runtime/session state")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_live_mcp_runtime_checkpoint() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("mcp-runtime-checkpoint-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry MCP runtime".to_string();
        run.session_id = Some("session_123".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunMcpRuntimeCheckpoint,
                    message: Some(
                        "MCP runtime checkpointed after 'mcp_resource_subscribe' with 1 active subscription(s) requiring live runtime continuity"
                            .to_string(),
                    ),
                    tool_name: Some("mcp_resource_subscribe".to_string()),
                    tool_call_id: Some("call_mcp_runtime_1".to_string()),
                    metadata: Some(serde_json::json!({
                        "tool_name": "mcp_resource_subscribe",
                        "live_runtime_required": true,
                        "active_subscription_count": 1,
                        "active_servers": ["docs"],
                        "server": "docs",
                        "transport": "http",
                        "target": "uri:docs://guide",
                        "note": "1 active MCP subscription(s) still depend on a live runtime/session after 'mcp_resource_subscribe'",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .mcp_runtime_checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.live_runtime_required),
            Some(true)
        );
        assert_eq!(
            summary
                .mcp_runtime_checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.active_subscription_count),
            Some(1)
        );
        assert_eq!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.suggested_action.as_deref()),
            Some("manual_review")
        );
        assert!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.note.as_deref())
                .unwrap_or_default()
                .contains("active subscription")
        );
    }

    #[tokio::test]
    async fn load_managed_run_derived_summary_reads_cleared_mcp_runtime_checkpoint() {
        let (_dir, store) = temp_store().await;

        let agent = ManagedAgent::new("mcp-runtime-cleared-reader");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "summary");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry after unsubscribe".to_string();
        run.session_id = Some("session_123".to_string());
        store.create_run(&run).await.unwrap();
        store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunMcpRuntimeCheckpoint,
                    message: Some(
                        "MCP runtime checkpointed after 'mcp_resource_unsubscribe' with no live runtime dependency"
                            .to_string(),
                    ),
                    tool_name: Some("mcp_resource_unsubscribe".to_string()),
                    tool_call_id: Some("call_mcp_runtime_2".to_string()),
                    metadata: Some(serde_json::json!({
                        "tool_name": "mcp_resource_unsubscribe",
                        "live_runtime_required": false,
                        "active_subscription_count": 0,
                        "active_servers": [],
                        "server": "docs",
                        "transport": "http",
                        "target": "uri:docs://guide",
                        "note": "'mcp_resource_unsubscribe' left no live MCP runtime/session dependency",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = load_managed_run_derived_summary(&store, &run.id)
            .await
            .unwrap();

        assert_eq!(
            summary
                .mcp_runtime_checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.live_runtime_required),
            Some(false)
        );
        assert_eq!(
            summary
                .recovery_hint
                .as_ref()
                .and_then(|hint| hint.suggested_action.as_deref()),
            Some("replay")
        );
    }
}

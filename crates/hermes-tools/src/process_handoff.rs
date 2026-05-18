use hermes_core::{stream::StreamDelta, tool::ToolContext};
use serde::{Deserialize, Serialize};

const TOOL_PROCESS_STARTED_KIND: &str = "tool.process_started";
const TOOL_PROCESS_COMPLETED_KIND: &str = "tool.process_completed";
const TOOL_PROCESS_FAILED_KIND: &str = "tool.process_failed";
const TOOL_PROCESS_TIMED_OUT_KIND: &str = "tool.process_timed_out";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProcessExecutionState {
    Started,
    Completed,
    Failed,
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessExecutionEvent {
    pub state: ProcessExecutionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_group: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stdout_chars: usize,
    #[serde(default)]
    pub stderr_chars: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletedProcessCapture {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_group: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stdout_chars: usize,
    #[serde(default)]
    pub stderr_chars: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_preview: Option<String>,
}

async fn emit_process_event(
    ctx: &ToolContext,
    tool: &str,
    kind: &str,
    event: ProcessExecutionEvent,
) {
    let _ = ctx
        .delta_tx
        .send(StreamDelta::ToolEvent {
            kind: kind.to_string(),
            tool: tool.to_string(),
            call_id: None,
            message: event.note.clone(),
            metadata: serde_json::to_value(event).ok(),
        })
        .await;
}

pub async fn emit_process_started(
    ctx: &ToolContext,
    tool: &str,
    process_group: Option<u32>,
    timeout_secs: u64,
) {
    emit_process_event(
        ctx,
        tool,
        TOOL_PROCESS_STARTED_KIND,
        ProcessExecutionEvent {
            state: ProcessExecutionState::Started,
            process_group,
            timeout_secs: Some(timeout_secs),
            exit_code: None,
            stdout_chars: 0,
            stderr_chars: 0,
            stdout_preview: None,
            stderr_preview: None,
            note: Some("process started and may produce side effects before interruption".into()),
        },
    )
    .await;
}

pub async fn emit_process_completed(
    ctx: &ToolContext,
    tool: &str,
    capture: CompletedProcessCapture,
) {
    emit_process_event(
        ctx,
        tool,
        TOOL_PROCESS_COMPLETED_KIND,
        ProcessExecutionEvent {
            state: ProcessExecutionState::Completed,
            process_group: capture.process_group,
            timeout_secs: None,
            exit_code: capture.exit_code,
            stdout_chars: capture.stdout_chars,
            stderr_chars: capture.stderr_chars,
            stdout_preview: capture.stdout_preview,
            stderr_preview: capture.stderr_preview,
            note: Some("process completed before a newer durable tool-result checkpoint".into()),
        },
    )
    .await;
}

pub async fn emit_process_failed(
    ctx: &ToolContext,
    tool: &str,
    process_group: Option<u32>,
    note: impl Into<String>,
) {
    emit_process_event(
        ctx,
        tool,
        TOOL_PROCESS_FAILED_KIND,
        ProcessExecutionEvent {
            state: ProcessExecutionState::Failed,
            process_group,
            timeout_secs: None,
            exit_code: None,
            stdout_chars: 0,
            stderr_chars: 0,
            stdout_preview: None,
            stderr_preview: None,
            note: Some(note.into()),
        },
    )
    .await;
}

pub async fn emit_process_timed_out(
    ctx: &ToolContext,
    tool: &str,
    process_group: Option<u32>,
    timeout_secs: u64,
) {
    emit_process_event(
        ctx,
        tool,
        TOOL_PROCESS_TIMED_OUT_KIND,
        ProcessExecutionEvent {
            state: ProcessExecutionState::TimedOut,
            process_group,
            timeout_secs: Some(timeout_secs),
            exit_code: Some(124),
            stdout_chars: 0,
            stderr_chars: 0,
            stdout_preview: None,
            stderr_preview: None,
            note: Some("process timed out before a durable tool-result checkpoint".into()),
        },
    )
    .await;
}

use hermes_core::{stream::StreamDelta, tool::ToolContext};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserActionState {
    Started,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserActionEvent {
    pub state: BrowserActionState,
    pub action: String,
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

#[derive(Debug, Clone, Default)]
pub struct BrowserActionSnapshot {
    pub target: Option<String>,
    pub wait_for_navigation: bool,
    pub page_url: Option<String>,
    pub page_title: Option<String>,
}

async fn emit_browser_action_event(
    ctx: &ToolContext,
    tool: &str,
    kind: &str,
    message: Option<String>,
    event: &BrowserActionEvent,
) {
    let _ = ctx
        .delta_tx
        .send(StreamDelta::ToolEvent {
            kind: kind.to_string(),
            tool: tool.to_string(),
            call_id: None,
            message,
            metadata: serde_json::to_value(event).ok(),
        })
        .await;
}

pub async fn emit_browser_action_started(
    ctx: &ToolContext,
    tool: &str,
    action: &str,
    snapshot: BrowserActionSnapshot,
) {
    emit_browser_action_event(
        ctx,
        tool,
        "tool.browser_action_started",
        Some(format!("browser action '{action}' started")),
        &BrowserActionEvent {
            state: BrowserActionState::Started,
            action: action.to_string(),
            target: snapshot.target,
            wait_for_navigation: snapshot.wait_for_navigation,
            page_url: snapshot.page_url,
            page_title: snapshot.page_title,
            output_preview: None,
            note: None,
        },
    )
    .await;
}

pub async fn emit_browser_action_completed(
    ctx: &ToolContext,
    tool: &str,
    action: &str,
    snapshot: BrowserActionSnapshot,
    output_preview: Option<String>,
) {
    emit_browser_action_event(
        ctx,
        tool,
        "tool.browser_action_completed",
        Some(format!("browser action '{action}' completed")),
        &BrowserActionEvent {
            state: BrowserActionState::Completed,
            action: action.to_string(),
            target: snapshot.target,
            wait_for_navigation: snapshot.wait_for_navigation,
            page_url: snapshot.page_url,
            page_title: snapshot.page_title,
            output_preview,
            note: None,
        },
    )
    .await;
}

pub async fn emit_browser_action_failed(
    ctx: &ToolContext,
    tool: &str,
    action: &str,
    snapshot: BrowserActionSnapshot,
    note: String,
) {
    emit_browser_action_event(
        ctx,
        tool,
        "tool.browser_action_failed",
        Some(format!("browser action '{action}' failed")),
        &BrowserActionEvent {
            state: BrowserActionState::Failed,
            action: action.to_string(),
            target: snapshot.target,
            wait_for_navigation: snapshot.wait_for_navigation,
            page_url: snapshot.page_url,
            page_title: snapshot.page_title,
            output_preview: None,
            note: Some(note),
        },
    )
    .await;
}

//! API server adapter — REST + OpenAI-compatible `/v1/chat/completions` with SSE.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use dashmap::DashMap;
use hermes_agent::{
    ConversationCheckpointObserver, ConversationContinuationBoundary, analyze_continuation_boundary,
};
use hermes_config::config::{ApiServerGatewayConfig, AppConfig};
use hermes_core::{
    error::Result,
    message::{Message, Role},
    platform::{ChatType, MessageEvent, PlatformAdapter, PlatformEvent},
    session::{SessionMeta, SessionStore},
    stream::StreamDelta,
    tool::{ToolExecutionObserver, ToolExecutionResultObservation},
};
use hermes_managed::{
    ManagedAgent, ManagedAgentVersion, ManagedAgentVersionDraft, ManagedApprovalPolicy,
    ManagedMcpAdmissionRejection, ManagedRun, ManagedRunArtifact, ManagedRunArtifactDraft,
    ManagedRunArtifactKind, ManagedRunCleanupFailureSummary,
    ManagedRunContinuationCheckpointSummary, ManagedRunContinuationSummary,
    ManagedRunDerivedSummary, ManagedRunEvent, ManagedRunEventDraft, ManagedRunEventKind,
    ManagedRunInterruptionCause, ManagedRunMcpHandoffState, ManagedRunMcpHandoffSummary,
    ManagedRunMcpReplayDisposition, ManagedRunMcpRuntimeCheckpointSummary,
    ManagedRunOwnershipReleaseReason, ManagedRunOwnershipReleaseSummary,
    ManagedRunProviderCallFenceSummary, ManagedRunRecoveryDecisionKind,
    ManagedRunRecoveryDecisionReason, ManagedRunRecoveryDecisionSummary, ManagedRunRecoveryHint,
    ManagedRunStatus, ManagedRunTakeoverAssessmentSummary, ManagedRuntimeBuildContext,
    ManagedStore, ResolvedManagedVersionDefaults, RunRegistry, build_filtered_skill_manager,
    build_managed_runtime, load_managed_run_derived_summaries, load_managed_run_derived_summary,
    managed_run_recovery_decision_from_event, managed_run_takeover_assessment_from_event,
    preflight_managed_model, resolve_managed_version_defaults, validate_managed_agent_name,
    validate_managed_beta_tools,
};
use hermes_tools::session_cleanup;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info};

use crate::session::{SessionRouter, SharedState};

// ─── Adapter ──────────────────────────────────────────────────────────────────

pub struct ApiServerAdapter {
    config: ApiServerGatewayConfig,
    pending: Arc<DashMap<String, oneshot::Sender<String>>>,
    router: std::sync::Mutex<Option<SessionRouter>>,
    managed: std::sync::Mutex<Option<ManagedApiState>>,
}

impl ApiServerAdapter {
    pub fn new(config: ApiServerGatewayConfig) -> Self {
        Self {
            config,
            pending: Arc::new(DashMap::new()),
            router: std::sync::Mutex::new(None),
            managed: std::sync::Mutex::new(None),
        }
    }
}

#[derive(Clone)]
pub(crate) struct ManagedApiState {
    shared: Arc<SharedState>,
    app_config: AppConfig,
    store: Arc<ManagedStore>,
    runs: Arc<RunRegistry>,
    worker_id: String,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ManagedAutoReplaySummary {
    pub candidates: usize,
    pub replayed_run_ids: Vec<String>,
    pub skipped_depth_limit: usize,
    pub skipped_handoff_risk: usize,
    pub skipped_browser_handoff_risk: usize,
    pub skipped_browser_session_state: usize,
    pub skipped_mcp_handoff_risk: usize,
    pub skipped_mcp_runtime_state: usize,
    pub failures: Vec<String>,
}

impl ManagedAutoReplaySummary {
    pub fn is_empty(&self) -> bool {
        self.candidates == 0
            && self.replayed_run_ids.is_empty()
            && self.skipped_depth_limit == 0
            && self.skipped_handoff_risk == 0
            && self.skipped_browser_handoff_risk == 0
            && self.skipped_browser_session_state == 0
            && self.skipped_mcp_handoff_risk == 0
            && self.skipped_mcp_runtime_state == 0
            && self.failures.is_empty()
    }
}

enum ManagedRunOutcome {
    Completed(String),
    Failed(String),
}

struct ManagedSessionCheckpointObserver {
    store: Arc<ManagedStore>,
    run_id: String,
    session_store: Arc<dyn SessionStore>,
    session_id: String,
    persisted_len: Mutex<usize>,
    checkpoint_state: Arc<ManagedRunCheckpointState>,
}

#[derive(Default)]
struct ManagedRunCheckpointState {
    pending_browser_checkpoints:
        Mutex<BTreeMap<String, hermes_managed::ManagedRunBrowserSessionCheckpointSummary>>,
    pending_mcp_runtime_checkpoints: Mutex<BTreeMap<String, ManagedRunMcpRuntimeCheckpointSummary>>,
    mcp_runtime_state: Mutex<ManagedMcpRuntimeState>,
}

struct ManagedToolCheckpointObserver {
    store: Arc<ManagedStore>,
    run_id: String,
    checkpoint_state: Arc<ManagedRunCheckpointState>,
    mcp_transport_by_server: BTreeMap<String, String>,
}

#[derive(Clone, Copy)]
struct ManagedMcpReplayClassification {
    read_only: bool,
    requires_live_runtime: bool,
}

#[derive(Default)]
struct ManagedMcpRuntimeState {
    active_subscriptions: BTreeMap<String, BTreeSet<String>>,
    unresolved_runtime_servers: BTreeSet<String>,
}

impl ManagedMcpRuntimeState {
    fn insert_subscription(&mut self, server: &str, uri: &str) {
        self.active_subscriptions
            .entry(server.to_string())
            .or_default()
            .insert(uri.to_string());
    }

    fn remove_subscription(&mut self, server: &str, uri: &str) {
        if let Some(uris) = self.active_subscriptions.get_mut(server) {
            uris.remove(uri);
            if uris.is_empty() {
                self.active_subscriptions.remove(server);
            }
        }
    }

    fn mark_unresolved_runtime_server(&mut self, server: Option<&str>) {
        self.unresolved_runtime_servers
            .insert(server.unwrap_or("unknown").to_string());
    }

    fn active_subscription_count(&self) -> usize {
        self.active_subscriptions.values().map(BTreeSet::len).sum()
    }

    fn active_servers(&self) -> Vec<String> {
        let mut servers = self
            .active_subscriptions
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        servers.extend(self.unresolved_runtime_servers.iter().cloned());
        servers.into_iter().collect()
    }

    fn live_runtime_required(&self) -> bool {
        self.active_subscription_count() > 0 || !self.unresolved_runtime_servers.is_empty()
    }
}

impl ManagedSessionCheckpointObserver {
    fn new(
        store: Arc<ManagedStore>,
        run_id: String,
        session_store: Arc<dyn SessionStore>,
        session_id: String,
        persisted_len: usize,
        checkpoint_state: Arc<ManagedRunCheckpointState>,
    ) -> Self {
        Self {
            store,
            run_id,
            session_store,
            session_id,
            persisted_len: Mutex::new(persisted_len),
            checkpoint_state,
        }
    }
}

fn browser_action_target_from_args(action: &str, args: &serde_json::Value) -> Option<String> {
    match action {
        "navigate" => args
            .get("url")
            .and_then(|value| value.as_str())
            .map(|url| format!("url:{url}")),
        "click" | "type" | "extract_text" | "snapshot" => args
            .get("selector")
            .and_then(|value| value.as_str())
            .map(|selector| format!("selector:{selector}")),
        "press_key" => {
            let selector = args
                .get("selector")
                .and_then(|value| value.as_str())
                .unwrap_or("body");
            let key = args.get("key").and_then(|value| value.as_str())?;
            Some(format!("selector:{selector}|key:{key}"))
        }
        "wait" => args
            .get("selector")
            .and_then(|value| value.as_str())
            .map(|selector| format!("selector:{selector}"))
            .or_else(|| {
                args.get("timeout_ms")
                    .and_then(|value| value.as_u64())
                    .map(|timeout_ms| format!("sleep_ms:{timeout_ms}"))
            }),
        "close" => None,
        _ => None,
    }
}

fn run_artifact_from_checkpointed_message(message: &Message) -> Option<ManagedRunArtifactDraft> {
    let content = message.content.as_text_lossy();
    if content.trim().is_empty() {
        return None;
    }

    match message.role {
        Role::Assistant if message.tool_calls.is_empty() => Some(ManagedRunArtifactDraft {
            kind: ManagedRunArtifactKind::AssistantOutput,
            label: "assistant_output".to_string(),
            tool_name: None,
            tool_call_id: None,
            content,
            metadata: None,
        }),
        Role::Tool => Some(ManagedRunArtifactDraft {
            kind: ManagedRunArtifactKind::ToolOutput,
            label: message
                .name
                .clone()
                .unwrap_or_else(|| "tool_output".to_string()),
            tool_name: message.name.clone(),
            tool_call_id: message.tool_call_id.clone(),
            content,
            metadata: None,
        }),
        _ => None,
    }
}

fn browser_session_checkpoint_from_tool_result(
    observation: &ToolExecutionResultObservation,
) -> Option<hermes_managed::ManagedRunBrowserSessionCheckpointSummary> {
    if observation.result.is_error || observation.request.tool_name != "browser" {
        return None;
    }

    let action = observation
        .request
        .arguments
        .get("action")
        .and_then(|value| value.as_str())?
        .to_string();
    let payload: serde_json::Value = serde_json::from_str(&observation.result.content).ok()?;

    let page_url = payload
        .get("url")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let page_title = payload
        .get("title")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let output_preview = payload
        .get("content")
        .and_then(|value| value.as_str())
        .map(|value| value.chars().take(2_000).collect::<String>())
        .or_else(|| {
            payload
                .get("closed")
                .and_then(|value| value.as_bool())
                .map(|closed| format!("closed:{closed}"))
        });
    let target = browser_action_target_from_args(&action, &observation.request.arguments);
    let session_open = !(action == "close"
        && payload
            .get("closed")
            .and_then(|value| value.as_bool())
            .unwrap_or(false));
    let note = if session_open {
        Some("fresh browser session required on replay".to_string())
    } else {
        Some("browser session was explicitly closed".to_string())
    };

    Some(hermes_managed::ManagedRunBrowserSessionCheckpointSummary {
        action,
        session_open,
        target,
        page_url,
        page_title,
        output_preview,
        note,
    })
}

fn managed_mcp_transport_by_server(app_config: &AppConfig) -> BTreeMap<String, String> {
    app_config
        .mcp_servers
        .iter()
        .map(|server| {
            let transport = match server.transport {
                hermes_config::config::McpTransportKind::Stdio => "stdio",
                hermes_config::config::McpTransportKind::Http => "http",
            };
            (server.name.clone(), transport.to_string())
        })
        .collect()
}

fn mcp_target_from_args(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    match tool_name {
        "mcp_prompt_get" => args
            .get("name")
            .and_then(|value| value.as_str())
            .map(|name| format!("prompt:{name}")),
        "mcp_resource_read" | "mcp_resource_subscribe" | "mcp_resource_unsubscribe" => args
            .get("uri")
            .and_then(|value| value.as_str())
            .map(|uri| format!("uri:{uri}")),
        "mcp_resource_updates" => args
            .get("server")
            .and_then(|value| value.as_str())
            .map(|server| format!("server:{server}"))
            .or_else(|| Some("buffer".to_string())),
        _ => args
            .get("server")
            .and_then(|value| value.as_str())
            .map(|server| format!("server:{server}")),
    }
}

fn classify_mcp_replay(
    tool_name: &str,
    args: &serde_json::Value,
) -> ManagedMcpReplayClassification {
    match tool_name {
        "mcp_prompt_list"
        | "mcp_prompt_get"
        | "mcp_resource_list"
        | "mcp_resource_template_list"
        | "mcp_resource_read" => ManagedMcpReplayClassification {
            read_only: true,
            requires_live_runtime: false,
        },
        "mcp_resource_updates" => {
            let clears_state = args
                .get("clear")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            ManagedMcpReplayClassification {
                read_only: !clears_state,
                requires_live_runtime: clears_state,
            }
        }
        "mcp_resource_subscribe" | "mcp_resource_unsubscribe" => ManagedMcpReplayClassification {
            read_only: false,
            requires_live_runtime: true,
        },
        _ => ManagedMcpReplayClassification {
            read_only: false,
            requires_live_runtime: true,
        },
    }
}

fn mcp_replay_disposition(
    state: ManagedRunMcpHandoffState,
    classification: ManagedMcpReplayClassification,
) -> ManagedRunMcpReplayDisposition {
    if classification.read_only && !classification.requires_live_runtime {
        ManagedRunMcpReplayDisposition::SafeToReplay
    } else {
        match state {
            ManagedRunMcpHandoffState::Started => {
                ManagedRunMcpReplayDisposition::UnsafeSideEffectWindow
            }
            ManagedRunMcpHandoffState::Completed | ManagedRunMcpHandoffState::Failed => {
                ManagedRunMcpReplayDisposition::CompletedButNotRecorded
            }
        }
    }
}

fn mcp_handoff_note(
    tool_name: &str,
    classification: ManagedMcpReplayClassification,
) -> Option<String> {
    let note = match tool_name {
        "mcp_prompt_list"
        | "mcp_prompt_get"
        | "mcp_resource_list"
        | "mcp_resource_template_list"
        | "mcp_resource_read" => "read-only MCP request can be replayed from a fresh MCP session",
        "mcp_resource_updates" if classification.read_only => {
            "reads the persisted MCP resource-update buffer without clearing it"
        }
        "mcp_resource_updates" => {
            "clears buffered MCP resource updates and therefore is not replay-safe"
        }
        "mcp_resource_subscribe" => {
            "MCP subscription state may already have changed before interruption"
        }
        "mcp_resource_unsubscribe" => {
            "MCP subscription state may already have changed before interruption"
        }
        _ => "MCP tool is not yet classified as replay-safe and requires manual review",
    };
    Some(note.to_string())
}

fn mcp_result_server(observation: &ToolExecutionResultObservation) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&observation.result.content)
        .ok()
        .and_then(|payload| {
            payload
                .get("server")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        })
        .or_else(|| {
            observation
                .request
                .arguments
                .get("server")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        })
}

fn mcp_output_preview(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    const MAX_PREVIEW_CHARS: usize = 2_000;
    let mut preview = trimmed.chars().take(MAX_PREVIEW_CHARS).collect::<String>();
    if trimmed.chars().count() > MAX_PREVIEW_CHARS {
        preview.push_str("...");
    }
    Some(preview)
}

fn mcp_runtime_checkpoint_note(
    tool_name: &str,
    live_runtime_required: bool,
    active_subscription_count: usize,
) -> Option<String> {
    let note = if live_runtime_required {
        if active_subscription_count > 0 {
            format!(
                "{active_subscription_count} active MCP subscription(s) still depend on a live runtime/session after '{tool_name}'"
            )
        } else {
            format!(
                "'{tool_name}' left MCP runtime/session state that Hermes cannot reattach automatically"
            )
        }
    } else {
        format!("'{tool_name}' left no live MCP runtime/session dependency")
    };
    Some(note)
}

fn mcp_runtime_checkpoint_from_tool_result(
    observation: &ToolExecutionResultObservation,
    mcp_transport_by_server: &BTreeMap<String, String>,
    state: &mut ManagedMcpRuntimeState,
) -> Option<ManagedRunMcpRuntimeCheckpointSummary> {
    let tool_name = observation.request.tool_name.as_str();
    let server = mcp_result_server(observation);
    let transport = server
        .as_deref()
        .and_then(|server_name| mcp_transport_by_server.get(server_name).cloned());
    let target = mcp_target_from_args(tool_name, &observation.request.arguments);
    let classification = classify_mcp_replay(tool_name, &observation.request.arguments);

    let mut emit = false;
    match tool_name {
        "mcp_resource_subscribe" => {
            emit = true;
            if observation.result.is_error {
                state.mark_unresolved_runtime_server(server.as_deref());
            } else if let (Some(server_name), Some(uri)) = (
                server.as_deref(),
                observation
                    .request
                    .arguments
                    .get("uri")
                    .and_then(|value| value.as_str()),
            ) {
                state.insert_subscription(server_name, uri);
            } else {
                state.mark_unresolved_runtime_server(server.as_deref());
            }
        }
        "mcp_resource_unsubscribe" => {
            emit = true;
            if observation.result.is_error {
                state.mark_unresolved_runtime_server(server.as_deref());
            } else if let (Some(server_name), Some(uri)) = (
                server.as_deref(),
                observation
                    .request
                    .arguments
                    .get("uri")
                    .and_then(|value| value.as_str()),
            ) {
                state.remove_subscription(server_name, uri);
            }
        }
        "mcp_resource_updates" => {
            let clears_state = observation
                .request
                .arguments
                .get("clear")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            emit = clears_state && state.live_runtime_required();
        }
        _ if !classification.read_only && classification.requires_live_runtime => {
            emit = true;
            state.mark_unresolved_runtime_server(server.as_deref());
        }
        _ if observation.result.is_error
            && (!classification.read_only || classification.requires_live_runtime) =>
        {
            emit = true;
            state.mark_unresolved_runtime_server(server.as_deref());
        }
        _ => {}
    }

    if !emit {
        return None;
    }

    let active_subscription_count = state.active_subscription_count();
    let live_runtime_required = state.live_runtime_required();
    Some(ManagedRunMcpRuntimeCheckpointSummary {
        tool_name: observation.request.tool_name.clone(),
        live_runtime_required,
        active_subscription_count,
        active_servers: state.active_servers(),
        server,
        transport,
        target,
        note: mcp_runtime_checkpoint_note(
            tool_name,
            live_runtime_required,
            active_subscription_count,
        ),
    })
}

fn mcp_handoff_from_tool_call(
    observation: &hermes_core::tool::ToolExecutionObservation,
    mcp_transport_by_server: &BTreeMap<String, String>,
) -> Option<ManagedRunMcpHandoffSummary> {
    let classification = classify_mcp_replay(&observation.tool_name, &observation.arguments);
    let server = observation
        .arguments
        .get("server")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);

    Some(ManagedRunMcpHandoffSummary {
        tool_name: observation.tool_name.clone(),
        state: ManagedRunMcpHandoffState::Started,
        replay_disposition: mcp_replay_disposition(
            ManagedRunMcpHandoffState::Started,
            classification,
        ),
        read_only: classification.read_only,
        requires_live_runtime: classification.requires_live_runtime,
        transport: server
            .as_deref()
            .and_then(|server_name| mcp_transport_by_server.get(server_name).cloned()),
        server,
        target: mcp_target_from_args(&observation.tool_name, &observation.arguments),
        output_preview: None,
        note: mcp_handoff_note(&observation.tool_name, classification),
    })
}

fn mcp_handoff_from_tool_result(
    observation: &ToolExecutionResultObservation,
    mcp_transport_by_server: &BTreeMap<String, String>,
) -> Option<ManagedRunMcpHandoffSummary> {
    let classification = classify_mcp_replay(
        &observation.request.tool_name,
        &observation.request.arguments,
    );
    let state = if observation.result.is_error {
        ManagedRunMcpHandoffState::Failed
    } else {
        ManagedRunMcpHandoffState::Completed
    };
    let server = mcp_result_server(observation);

    Some(ManagedRunMcpHandoffSummary {
        tool_name: observation.request.tool_name.clone(),
        state,
        replay_disposition: mcp_replay_disposition(state, classification),
        read_only: classification.read_only,
        requires_live_runtime: classification.requires_live_runtime,
        transport: server
            .as_deref()
            .and_then(|server_name| mcp_transport_by_server.get(server_name).cloned()),
        server,
        target: mcp_target_from_args(
            &observation.request.tool_name,
            &observation.request.arguments,
        ),
        output_preview: mcp_output_preview(&observation.result.content),
        note: mcp_handoff_note(&observation.request.tool_name, classification),
    })
}

#[async_trait]
impl ToolExecutionObserver for ManagedToolCheckpointObserver {
    async fn on_tool_call(
        &self,
        observation: hermes_core::tool::ToolExecutionObservation,
        _ctx: &hermes_core::tool::ToolContext,
    ) -> Result<()> {
        if observation.toolset.as_deref() != Some("mcp") {
            return Ok(());
        }

        let Some(summary) = mcp_handoff_from_tool_call(&observation, &self.mcp_transport_by_server)
        else {
            return Ok(());
        };

        self.store
            .append_run_event(
                &self.run_id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::ToolMcpCallStarted,
                    message: Some(summary.message()),
                    tool_name: Some(observation.tool_name.clone()),
                    tool_call_id: Some(observation.call_id.clone()),
                    metadata: serde_json::to_value(&summary).ok(),
                },
            )
            .await?;
        Ok(())
    }

    async fn on_tool_result(
        &self,
        observation: ToolExecutionResultObservation,
        _ctx: &hermes_core::tool::ToolContext,
    ) -> Result<()> {
        if observation.request.toolset.as_deref() == Some("mcp") {
            if let Some(summary) =
                mcp_handoff_from_tool_result(&observation, &self.mcp_transport_by_server)
            {
                let kind = match summary.state {
                    ManagedRunMcpHandoffState::Started => ManagedRunEventKind::ToolMcpCallStarted,
                    ManagedRunMcpHandoffState::Completed => {
                        ManagedRunEventKind::ToolMcpCallCompleted
                    }
                    ManagedRunMcpHandoffState::Failed => ManagedRunEventKind::ToolMcpCallFailed,
                };
                self.store
                    .append_run_event(
                        &self.run_id,
                        &ManagedRunEventDraft {
                            kind,
                            message: Some(summary.message()),
                            tool_name: Some(observation.request.tool_name.clone()),
                            tool_call_id: Some(observation.request.call_id.clone()),
                            metadata: serde_json::to_value(&summary).ok(),
                        },
                    )
                    .await?;
            }

            let mut runtime_state = self.checkpoint_state.mcp_runtime_state.lock().await;
            if let Some(summary) = mcp_runtime_checkpoint_from_tool_result(
                &observation,
                &self.mcp_transport_by_server,
                &mut runtime_state,
            ) {
                drop(runtime_state);
                self.checkpoint_state
                    .pending_mcp_runtime_checkpoints
                    .lock()
                    .await
                    .insert(observation.request.call_id.clone(), summary);
            }
        }

        let Some(summary) = browser_session_checkpoint_from_tool_result(&observation) else {
            return Ok(());
        };

        self.checkpoint_state
            .pending_browser_checkpoints
            .lock()
            .await
            .insert(observation.request.call_id.clone(), summary);
        Ok(())
    }
}

#[async_trait]
impl ConversationCheckpointObserver for ManagedSessionCheckpointObserver {
    async fn on_history_checkpoint(&self, history: &[Message]) -> Result<()> {
        let mut persisted_len = self.persisted_len.lock().await;
        let persist_start = (*persisted_len).min(history.len());

        for message in &history[persist_start..] {
            self.session_store
                .append_message(&self.session_id, message)
                .await?;
            *persisted_len += 1;

            if let Some(artifact) = run_artifact_from_checkpointed_message(message) {
                self.store
                    .append_run_artifact(&self.run_id, &artifact)
                    .await?;
            }

            if message.role == Role::Tool && message.name.as_deref() == Some("browser") {
                if let Some(tool_call_id) = message.tool_call_id.as_deref() {
                    if let Some(summary) = self
                        .checkpoint_state
                        .pending_browser_checkpoints
                        .lock()
                        .await
                        .remove(tool_call_id)
                    {
                        self.store
                            .append_run_event(
                                &self.run_id,
                                &ManagedRunEventDraft {
                                    kind: ManagedRunEventKind::RunBrowserSessionCheckpoint,
                                    message: Some(summary.message()),
                                    tool_name: Some("browser".to_string()),
                                    tool_call_id: Some(tool_call_id.to_string()),
                                    metadata: serde_json::to_value(&summary).ok(),
                                },
                            )
                            .await?;
                    }
                }
            }

            if message.role == Role::Tool {
                if let Some(tool_call_id) = message.tool_call_id.as_deref() {
                    if let Some(summary) = self
                        .checkpoint_state
                        .pending_mcp_runtime_checkpoints
                        .lock()
                        .await
                        .remove(tool_call_id)
                    {
                        self.store
                            .append_run_event(
                                &self.run_id,
                                &ManagedRunEventDraft {
                                    kind: ManagedRunEventKind::RunMcpRuntimeCheckpoint,
                                    message: Some(summary.message()),
                                    tool_name: Some(summary.tool_name.clone()),
                                    tool_call_id: Some(tool_call_id.to_string()),
                                    metadata: serde_json::to_value(&summary).ok(),
                                },
                            )
                            .await?;
                    }
                }
            }
        }

        if let Some(boundary) = analyze_continuation_boundary(history) {
            let checkpoint = ManagedRunContinuationCheckpointSummary::from_boundary(boundary);
            self.store
                .append_run_event(
                    &self.run_id,
                    &ManagedRunEventDraft {
                        kind: ManagedRunEventKind::RunContinuationCheckpoint,
                        message: Some(checkpoint.message()),
                        tool_name: None,
                        tool_call_id: None,
                        metadata: serde_json::to_value(&checkpoint).ok(),
                    },
                )
                .await?;
        }

        Ok(())
    }

    async fn on_provider_call_started(
        &self,
        boundary: ConversationContinuationBoundary,
        request_history_len: usize,
        tool_count: usize,
    ) -> Result<()> {
        let fence = ManagedRunProviderCallFenceSummary {
            request_history_len,
            tool_count,
            safe_resume_from: ManagedRunContinuationCheckpointSummary::from_boundary(boundary),
            note: Some(
                "provider call dispatched before a newer durable response checkpoint".to_string(),
            ),
        };
        self.store
            .append_run_event(
                &self.run_id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunProviderCallStarted,
                    message: Some(fence.message()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: serde_json::to_value(&fence).ok(),
                },
            )
            .await?;
        Ok(())
    }
}

const MANAGED_RUN_HEARTBEAT_INTERVAL_SECS: u64 = 10;
const MANAGED_RUN_LEASE_SECS: i64 = 45;

// ─── Axum state ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ApiState {
    event_tx: mpsc::Sender<PlatformEvent>,
    pending: Arc<DashMap<String, oneshot::Sender<String>>>,
    api_key: Option<SecretString>,
    model_name: String,
    /// Session router for streaming endpoints — avoids going through event channel.
    router: Option<SessionRouter>,
    managed: Option<ManagedApiState>,
}

// ─── Legacy request / response types ─────────────────────────────────────────

#[derive(Deserialize)]
struct ChatRequest {
    message: String,
    session_id: Option<String>,
    user_id: Option<String>,
}

#[derive(Serialize)]
struct ChatResponseBody {
    response: String,
    session_id: String,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
}

// ─── OpenAI-compatible types ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct OaiChatRequest {
    #[serde(default)]
    model: Option<String>,
    messages: Vec<OaiMessage>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Deserialize, Serialize, Clone)]
struct OaiMessage {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct OaiChatResponse {
    id: String,
    object: &'static str,
    created: u64,
    model: String,
    choices: Vec<OaiChoice>,
    usage: OaiUsage,
}

#[derive(Serialize)]
struct OaiChoice {
    index: u32,
    message: OaiMessage,
    finish_reason: &'static str,
}

#[derive(Serialize)]
struct OaiStreamChunk {
    id: String,
    object: &'static str,
    created: u64,
    model: String,
    choices: Vec<OaiStreamChoice>,
}

#[derive(Serialize)]
struct OaiStreamChoice {
    index: u32,
    delta: OaiDelta,
    finish_reason: Option<&'static str>,
}

#[derive(Serialize)]
struct OaiDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
}

#[derive(Serialize)]
struct OaiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}

#[derive(Serialize)]
struct OaiModelList {
    object: &'static str,
    data: Vec<OaiModel>,
}

#[derive(Serialize)]
struct OaiModel {
    id: String,
    object: &'static str,
    owned_by: &'static str,
}

#[derive(Serialize)]
struct ManagedAgentEnvelope {
    agent: ManagedAgent,
    latest_version: Option<ManagedAgentVersion>,
}

#[derive(Serialize)]
struct ManagedAgentListResponse {
    object: &'static str,
    data: Vec<ManagedAgent>,
}

#[derive(Serialize)]
struct ManagedAgentVersionEnvelope {
    version: ManagedAgentVersion,
}

#[derive(Serialize)]
struct ManagedAgentVersionListResponse {
    object: &'static str,
    data: Vec<ManagedAgentVersion>,
}

#[derive(Serialize)]
struct ManagedRunEnvelope {
    run: hermes_managed::ManagedRun,
    #[serde(default, skip_serializing_if = "ManagedRunDerivedSummary::is_empty")]
    summary: ManagedRunDerivedSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp_admission_rejection: Option<ManagedMcpAdmissionRejection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleanup_failure: Option<ManagedRunCleanupFailureSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ownership: Option<hermes_managed::ManagedRunOwnerSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    takeover_assessment: Option<ManagedRunTakeoverAssessmentSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    takeover: Option<hermes_managed::ManagedRunTakeoverSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_decision: Option<ManagedRunRecoveryDecisionSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_hint: Option<ManagedRunRecoveryHint>,
}

#[derive(Serialize)]
struct ManagedRunListResponse {
    object: &'static str,
    data: Vec<hermes_managed::ManagedRun>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    summaries: BTreeMap<String, ManagedRunDerivedSummary>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    mcp_admission_rejections: BTreeMap<String, ManagedMcpAdmissionRejection>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    cleanup_failures: BTreeMap<String, ManagedRunCleanupFailureSummary>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    ownerships: BTreeMap<String, hermes_managed::ManagedRunOwnerSnapshot>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    takeover_assessments: BTreeMap<String, ManagedRunTakeoverAssessmentSummary>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    takeovers: BTreeMap<String, hermes_managed::ManagedRunTakeoverSummary>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    recovery_decisions: BTreeMap<String, ManagedRunRecoveryDecisionSummary>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    recovery_hints: BTreeMap<String, ManagedRunRecoveryHint>,
}

#[derive(Serialize)]
struct ManagedRunEventListResponse {
    object: &'static str,
    data: Vec<ManagedRunEvent>,
    #[serde(default, skip_serializing_if = "ManagedRunDerivedSummary::is_empty")]
    summary: ManagedRunDerivedSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    mcp_admission_rejection: Option<ManagedMcpAdmissionRejection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cleanup_failure: Option<ManagedRunCleanupFailureSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ownership: Option<hermes_managed::ManagedRunOwnerSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    takeover_assessment: Option<ManagedRunTakeoverAssessmentSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    takeover: Option<hermes_managed::ManagedRunTakeoverSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_decision: Option<ManagedRunRecoveryDecisionSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery_hint: Option<ManagedRunRecoveryHint>,
}

#[derive(Serialize)]
struct ManagedRunArtifactListResponse {
    object: &'static str,
    data: Vec<ManagedRunArtifact>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    lineage_run_ids: Vec<String>,
}

#[derive(Deserialize, Default)]
struct RunsListQuery {
    limit: Option<usize>,
}

#[derive(Deserialize, Default)]
struct RunEventsListQuery {
    limit: Option<usize>,
}

#[derive(Deserialize, Default)]
struct RunArtifactsListQuery {
    limit: Option<usize>,
    lineage: Option<bool>,
}

#[derive(Deserialize, Default)]
struct ManagedAgentsListQuery {
    limit: Option<usize>,
    include_archived: Option<bool>,
}

#[derive(Deserialize)]
struct CreateManagedAgentRequest {
    name: String,
}

#[derive(Deserialize)]
struct CreateManagedAgentVersionRequest {
    #[serde(default)]
    model: String,
    #[serde(default)]
    base_url: Option<String>,
    system_prompt: String,
    #[serde(default)]
    allowed_tools: Vec<String>,
    #[serde(default)]
    allowed_skills: Vec<String>,
    #[serde(default)]
    max_iterations: Option<u32>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    approval_policy: Option<ManagedApprovalPolicy>,
    #[serde(default)]
    timeout_secs: Option<u32>,
}

// ─── PlatformAdapter impl ─────────────────────────────────────────────────────

impl ApiServerAdapter {
    /// Set the session router for streaming endpoints (called by GatewayRunner after construction).
    pub fn set_router(&self, router: SessionRouter) {
        self.router
            .lock()
            .expect("router lock poisoned")
            .replace(router);
    }

    pub fn set_managed_state(
        &self,
        shared: Arc<SharedState>,
        app_config: AppConfig,
        store: Arc<ManagedStore>,
        runs: Arc<RunRegistry>,
        worker_id: String,
    ) {
        self.managed
            .lock()
            .expect("managed lock poisoned")
            .replace(ManagedApiState {
                shared,
                app_config,
                store,
                runs,
                worker_id,
            });
    }
}

#[async_trait]
impl PlatformAdapter for ApiServerAdapter {
    fn platform_name(&self) -> &str {
        "api"
    }

    async fn run(self: Arc<Self>, event_tx: mpsc::Sender<PlatformEvent>) -> Result<()> {
        let state = ApiState {
            event_tx,
            pending: Arc::clone(&self.pending),
            api_key: self
                .config
                .api_key
                .as_deref()
                .map(|k| SecretString::new(k.into())),
            model_name: self.config.model_name.clone().unwrap_or_default(),
            router: self.router.lock().expect("router lock").clone(),
            managed: self.managed.lock().expect("managed lock").clone(),
        };

        let app = Router::new()
            .route("/api/chat", post(handle_chat))
            .route("/v1/chat/completions", post(handle_oai_chat))
            .route(
                "/v1/agents",
                get(handle_managed_agents_list).post(handle_managed_agent_create),
            )
            .route(
                "/v1/agents/{id}",
                get(handle_managed_agent_get).delete(handle_managed_agent_archive),
            )
            .route(
                "/v1/agents/{id}/versions",
                get(handle_managed_agent_versions_list).post(handle_managed_agent_version_create),
            )
            .route(
                "/v1/agents/{id}/versions/{version}",
                get(handle_managed_agent_version_get),
            )
            .route("/v1/runs", get(handle_managed_runs_list))
            .route(
                "/v1/runs/{id}",
                get(handle_managed_run_get).delete(handle_managed_run_cancel),
            )
            .route("/v1/runs/{id}/replay", post(handle_managed_run_replay))
            .route("/v1/runs/{id}/events", get(handle_managed_run_events_list))
            .route(
                "/v1/runs/{id}/artifacts",
                get(handle_managed_run_artifacts_list),
            )
            .route("/v1/models", get(handle_oai_models))
            .route("/health", get(handle_health))
            .with_state(state);

        let bind_addr = self.config.bind_addr.clone();
        info!("ApiServerAdapter: listening on {bind_addr}");

        let listener = tokio::net::TcpListener::bind(&bind_addr)
            .await
            .map_err(|e| hermes_core::error::HermesError::Config(e.to_string()))?;

        axum::serve(listener, app)
            .await
            .map_err(|e| hermes_core::error::HermesError::Config(e.to_string()))?;

        Ok(())
    }

    async fn send_response(&self, event: &MessageEvent, response: &str) -> Result<()> {
        let Some(request_id) = &event.reply_to else {
            // No reply_to — not an API request (e.g. Telegram), no-op.
            return Ok(());
        };

        if let Some((_, tx)) = self.pending.remove(request_id.as_str()) {
            if tx.send(response.to_string()).is_err() {
                error!(
                    request_id,
                    "failed to send response to pending receiver (already dropped)"
                );
            }
        }
        // If not found the waiter already timed out; no-op.
        Ok(())
    }
}

// ─── Auth helpers ─────────────────────────────────────────────────────────────

/// Compare two strings in constant time to resist timing side-channel attacks.
///
/// The early-exit on length mismatch leaks length information, but this is
/// acceptable for API keys where the expected length is publicly derivable from
/// the key format.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_health() -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok".to_string(),
    })
}

async fn handle_chat(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> impl IntoResponse {
    // API key authentication
    if let Some(ref expected_key) = state.api_key {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match provided {
            Some(key) if constant_time_eq(key, expected_key.expose_secret()) => {}
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"error": "unauthorized"})),
                )
                    .into_response();
            }
        }
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let session_id = req
        .session_id
        .unwrap_or_else(|| format!("api-{request_id}"));
    let user_id = req.user_id.unwrap_or_else(|| "api-user".into());

    // Create response channel and register in pending map.
    let (tx, rx) = oneshot::channel::<String>();
    state.pending.insert(request_id.clone(), tx);

    let event = MessageEvent {
        platform: "api".into(),
        chat_id: session_id.clone(),
        user_id,
        user_name: None,
        text: req.message,
        reply_to: Some(request_id.clone()),
        chat_type: ChatType::DirectMessage,
        thread_id: None,
    };

    if state
        .event_tx
        .send(PlatformEvent::Message(event))
        .await
        .is_err()
    {
        state.pending.remove(&request_id);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "gateway not running"})),
        )
            .into_response();
    }

    // Wait up to 300 s for the agent to reply.
    match tokio::time::timeout(Duration::from_secs(300), rx).await {
        Ok(Ok(response)) => Json(ChatResponseBody {
            response,
            session_id,
        })
        .into_response(),
        Ok(Err(_)) => {
            state.pending.remove(&request_id);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "session dropped"})),
            )
                .into_response()
        }
        Err(_) => {
            state.pending.remove(&request_id);
            (
                StatusCode::GATEWAY_TIMEOUT,
                Json(serde_json::json!({"error": "agent timed out (300s)"})),
            )
                .into_response()
        }
    }
}

async fn handle_managed_agents_list(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<ManagedAgentsListQuery>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    let Some(managed) = state.managed else {
        return managed_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed runtime not available",
        );
    };

    let limit = clamp_agents_limit(query.limit);
    let include_archived = query.include_archived.unwrap_or(false);
    let mut agents = match managed.store.list_agents(limit).await {
        Ok(agents) => agents,
        Err(e) => {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to list managed agents: {e}"),
            );
        }
    };

    if !include_archived {
        agents.retain(|agent| !agent.archived);
    }

    Json(ManagedAgentListResponse {
        object: "list",
        data: agents,
    })
    .into_response()
}

async fn handle_managed_agent_create(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(req): Json<CreateManagedAgentRequest>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    let Some(managed) = state.managed else {
        return managed_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed runtime not available",
        );
    };

    let name = req.name.trim();
    if let Err(e) = validate_managed_agent_name(name) {
        return managed_error_response(StatusCode::BAD_REQUEST, e.to_string());
    }

    let agent = ManagedAgent::new(name.to_string());
    match managed.store.create_agent(&agent).await {
        Ok(()) => (
            StatusCode::CREATED,
            Json(ManagedAgentEnvelope {
                agent,
                latest_version: None,
            }),
        )
            .into_response(),
        Err(e) if is_conflict_error(&e.to_string()) => managed_error_response(
            StatusCode::CONFLICT,
            format!("managed agent already exists: {name}"),
        ),
        Err(e) => managed_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to create managed agent: {e}"),
        ),
    }
}

async fn handle_managed_agent_get(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    let Some(managed) = state.managed else {
        return managed_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed runtime not available",
        );
    };

    let agent = match load_managed_agent(&managed, &agent_id).await {
        Ok(agent) => agent,
        Err(response) => return response,
    };
    let latest_version = match load_managed_latest_version(&managed, &agent).await {
        Ok(version) => version,
        Err(response) => return response,
    };

    Json(ManagedAgentEnvelope {
        agent,
        latest_version,
    })
    .into_response()
}

async fn handle_managed_agent_archive(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    let Some(managed) = state.managed else {
        return managed_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed runtime not available",
        );
    };

    let agent = match load_managed_agent(&managed, &agent_id).await {
        Ok(agent) => agent,
        Err(response) => return response,
    };

    if !agent.archived {
        if let Err(e) = managed.store.archive_agent(&agent_id).await {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to archive managed agent: {e}"),
            );
        }
    }

    let archived = match load_managed_agent(&managed, &agent_id).await {
        Ok(agent) => agent,
        Err(response) => return response,
    };
    let latest_version = match load_managed_latest_version(&managed, &archived).await {
        Ok(version) => version,
        Err(response) => return response,
    };

    Json(ManagedAgentEnvelope {
        agent: archived,
        latest_version,
    })
    .into_response()
}

async fn handle_managed_agent_versions_list(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    let Some(managed) = state.managed else {
        return managed_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed runtime not available",
        );
    };

    if let Err(response) = load_managed_agent(&managed, &agent_id).await {
        return response;
    }

    let versions = match managed.store.list_agent_versions(&agent_id).await {
        Ok(versions) => versions,
        Err(e) => {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to list managed agent versions: {e}"),
            );
        }
    };

    Json(ManagedAgentVersionListResponse {
        object: "list",
        data: versions,
    })
    .into_response()
}

async fn handle_managed_agent_version_create(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    Json(req): Json<CreateManagedAgentVersionRequest>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    let Some(managed) = state.managed else {
        return managed_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed runtime not available",
        );
    };

    let agent = match load_managed_agent(&managed, &agent_id).await {
        Ok(agent) => agent,
        Err(response) => return response,
    };
    if agent.archived {
        return managed_error_response(
            StatusCode::CONFLICT,
            format!("managed agent is archived: {}", agent.id),
        );
    }

    let resolved = match validate_managed_version_request(&managed, &req).await {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };

    let mut draft = ManagedAgentVersionDraft::new(&resolved.model, req.system_prompt.trim());
    draft.base_url = resolved.base_url;
    draft.allowed_tools = req.allowed_tools.clone();
    draft.allowed_skills = req.allowed_skills.clone();
    draft.max_iterations = req.max_iterations.unwrap_or(90);
    draft.temperature = req.temperature.unwrap_or(0.0);
    draft.approval_policy = req.approval_policy.unwrap_or(ManagedApprovalPolicy::Ask);
    draft.timeout_secs = req.timeout_secs.unwrap_or(300);

    let version = match managed
        .store
        .create_next_agent_version(&agent_id, &draft)
        .await
    {
        Ok(version) => version,
        Err(e) => {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to create managed agent version: {e}"),
            );
        }
    };

    (
        StatusCode::CREATED,
        Json(ManagedAgentVersionEnvelope { version }),
    )
        .into_response()
}

async fn handle_managed_agent_version_get(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path((agent_id, version)): Path<(String, u32)>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    let Some(managed) = state.managed else {
        return managed_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed runtime not available",
        );
    };

    if let Err(response) = load_managed_agent(&managed, &agent_id).await {
        return response;
    }

    match managed.store.get_agent_version(&agent_id, version).await {
        Ok(Some(version)) => Json(ManagedAgentVersionEnvelope { version }).into_response(),
        Ok(None) => managed_error_response(
            StatusCode::NOT_FOUND,
            format!("managed agent version not found: {agent_id}@{version}"),
        ),
        Err(e) => managed_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to load managed agent version: {e}"),
        ),
    }
}

async fn handle_managed_runs_list(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Query(query): Query<RunsListQuery>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    let Some(managed) = state.managed else {
        return managed_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed runtime not available",
        );
    };

    let limit = clamp_runs_limit(query.limit);
    let runs = match managed.store.list_runs(limit).await {
        Ok(runs) => runs,
        Err(e) => {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to list managed runs: {e}"),
            );
        }
    };

    let summaries = match load_managed_run_derived_summaries(managed.store.as_ref(), &runs).await {
        Ok(summaries) => summaries,
        Err(e) => {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load managed run summaries: {e}"),
            );
        }
    };
    let mcp_admission_rejections = collect_run_mcp_admission_rejections(summaries.clone());
    let cleanup_failures = collect_run_cleanup_failures(summaries.clone());
    let ownerships = collect_run_ownerships(summaries.clone());
    let takeover_assessments = collect_run_takeover_assessments(summaries.clone());
    let takeovers = collect_run_takeovers(summaries.clone());
    let recovery_decisions = collect_run_recovery_decisions(summaries.clone());
    let recovery_hints = collect_run_recovery_hints(summaries.clone());

    let data = runs
        .into_iter()
        .map(|run| apply_run_snapshot(run.clone(), managed.runs.snapshot(&run.id)))
        .collect();

    Json(ManagedRunListResponse {
        object: "list",
        data,
        summaries,
        mcp_admission_rejections,
        cleanup_failures,
        ownerships,
        takeover_assessments,
        takeovers,
        recovery_decisions,
        recovery_hints,
    })
    .into_response()
}

async fn handle_managed_run_get(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    let Some(managed) = state.managed else {
        return managed_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed runtime not available",
        );
    };

    let run = match managed.store.get_run(&run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => {
            return managed_error_response(
                StatusCode::NOT_FOUND,
                format!("managed run not found: {run_id}"),
            );
        }
        Err(e) => {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load managed run: {e}"),
            );
        }
    };

    match build_managed_run_envelope(&managed, run).await {
        Ok(envelope) => Json(envelope).into_response(),
        Err(e) => managed_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to load managed run summary: {e}"),
        ),
    }
}

async fn handle_managed_run_events_list(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
    Query(query): Query<RunEventsListQuery>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    let Some(managed) = state.managed else {
        return managed_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed runtime not available",
        );
    };

    match managed.store.get_run(&run_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return managed_error_response(
                StatusCode::NOT_FOUND,
                format!("managed run not found: {run_id}"),
            );
        }
        Err(e) => {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load managed run: {e}"),
            );
        }
    }

    let limit = clamp_run_events_limit(query.limit);
    match managed.store.list_run_events_tail(&run_id, limit).await {
        Ok(events) => match load_managed_run_derived_summary(managed.store.as_ref(), &run_id).await
        {
            Ok(summary) => Json(ManagedRunEventListResponse {
                object: "list",
                data: events,
                summary: summary.clone(),
                mcp_admission_rejection: summary.mcp_admission_rejection,
                cleanup_failure: summary.cleanup_failure,
                ownership: summary.ownership,
                takeover_assessment: summary.takeover_assessment,
                takeover: summary.takeover,
                recovery_decision: summary.recovery_decision,
                recovery_hint: summary.recovery_hint,
            })
            .into_response(),
            Err(e) => managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load managed run event summary: {e}"),
            ),
        },
        Err(e) => managed_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to list managed run events: {e}"),
        ),
    }
}

async fn handle_managed_run_artifacts_list(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
    Query(query): Query<RunArtifactsListQuery>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    let Some(managed) = state.managed else {
        return managed_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed runtime not available",
        );
    };

    match managed.store.get_run(&run_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return managed_error_response(
                StatusCode::NOT_FOUND,
                format!("managed run not found: {run_id}"),
            );
        }
        Err(e) => {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load managed run: {e}"),
            );
        }
    }

    let limit = clamp_run_artifacts_limit(query.limit);
    let include_lineage = query.lineage.unwrap_or(false);
    let result = if include_lineage {
        managed
            .store
            .list_run_artifacts_with_replay_lineage(&run_id, limit, 64)
            .await
            .map(|(lineage, artifacts)| ManagedRunArtifactListResponse {
                object: "list",
                data: artifacts,
                lineage_run_ids: lineage.into_iter().map(|run| run.id).collect(),
            })
    } else {
        managed
            .store
            .list_run_artifacts(&run_id, limit)
            .await
            .map(|artifacts| ManagedRunArtifactListResponse {
                object: "list",
                data: artifacts,
                lineage_run_ids: Vec::new(),
            })
    };

    match result {
        Ok(response) => Json(response).into_response(),
        Err(e) => managed_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to list managed run artifacts: {e}"),
        ),
    }
}

async fn handle_managed_run_cancel(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    let Some(managed) = state.managed else {
        return managed_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed runtime not available",
        );
    };

    let requested_run_id = run_id;
    let current = match managed.store.get_run(&requested_run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => {
            return managed_error_response(
                StatusCode::NOT_FOUND,
                format!("managed run not found: {requested_run_id}"),
            );
        }
        Err(e) => {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load managed run: {e}"),
            );
        }
    };
    let summary = match load_managed_run_derived_summary(managed.store.as_ref(), &current.id).await
    {
        Ok(summary) => summary,
        Err(e) => {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load managed run summary: {e}"),
            );
        }
    };
    let (target_run_id, target_current, redirected_cancel) = match summary.takeover.as_ref().filter(
        |takeover| {
            takeover.takeover_state == hermes_managed::ManagedRunTakeoverState::Active
                && takeover.replay_run_id != current.id
        },
    ) {
        Some(takeover) => match managed.store.get_run(&takeover.replay_run_id).await {
            Ok(Some(run)) => (run.id.clone(), run, true),
            Ok(None) => {
                tracing::warn!(
                    run_id = current.id,
                    replay_run_id = takeover.replay_run_id,
                    "active takeover target disappeared before cancellation; falling back to requested run"
                );
                (current.id.clone(), current.clone(), false)
            }
            Err(e) => {
                return managed_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to load active takeover run for cancellation: {e}"),
                );
            }
        },
        None => (current.id.clone(), current.clone(), false),
    };

    let final_target_run = if target_current.status.is_terminal() {
        target_current
    } else if managed.runs.snapshot(&target_run_id).is_some() {
        terminate_managed_run(
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            target_run_id.clone(),
            ManagedRunStatus::Cancelled,
            Some("cancelled via API".to_string()),
        )
        .await;
        match managed.store.get_run(&target_run_id).await {
            Ok(Some(run)) => apply_run_snapshot(run, managed.runs.snapshot(&target_run_id)),
            Ok(None) => {
                return managed_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("managed run disappeared after cancellation: {target_run_id}"),
                );
            }
            Err(e) => {
                return managed_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to reload managed run after cancellation: {e}"),
                );
            }
        }
    } else {
        if let Err(e) = managed
            .store
            .record_run_terminal_intent(
                &target_run_id,
                ManagedRunStatus::Cancelled,
                Some("cancelled via API"),
            )
            .await
        {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to record managed run cancel intent: {e}"),
            );
        }
        if let Err(e) = managed
            .store
            .update_run_status(
                &target_run_id,
                ManagedRunStatus::Cancelled,
                Some("cancelled via API"),
            )
            .await
        {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to update managed run status: {e}"),
            );
        }
        match managed.store.get_run(&target_run_id).await {
            Ok(Some(run)) => run,
            Ok(None) => {
                return managed_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("managed run disappeared after cancellation: {target_run_id}"),
                );
            }
            Err(e) => {
                return managed_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to reload managed run after cancellation: {e}"),
                );
            }
        }
    };
    if final_target_run.replay_of_run_id.is_some() {
        append_source_takeover_update_for_replay_child(
            managed.store.as_ref(),
            &final_target_run,
            None,
        )
        .await;
        append_ancestor_follow_replay_decisions_for_replay_child(
            managed.store.as_ref(),
            &final_target_run,
        )
        .await;
    }

    let response_run = if redirected_cancel {
        match managed.store.get_run(&requested_run_id).await {
            Ok(Some(run)) => run,
            Ok(None) => {
                return managed_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "managed source run disappeared after cancelling active takeover: {requested_run_id}"
                    ),
                );
            }
            Err(e) => {
                return managed_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!(
                        "failed to reload managed source run after cancelling active takeover: {e}"
                    ),
                );
            }
        }
    } else {
        final_target_run
    };

    match build_managed_run_envelope(&managed, response_run).await {
        Ok(envelope) => Json(envelope).into_response(),
        Err(e) => managed_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to load managed run summary: {e}"),
        ),
    }
}

// ─── OpenAI-compatible handlers ──────────────────────────────────────────────

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// Extract and verify Bearer token. Returns Err response on auth failure.
fn check_bearer_auth(
    headers: &HeaderMap,
    expected: &Option<SecretString>,
) -> std::result::Result<(), (StatusCode, Json<serde_json::Value>)> {
    if let Some(expected_key) = expected {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        match provided {
            Some(key) if constant_time_eq(key, expected_key.expose_secret()) => Ok(()),
            _ => Err((
                StatusCode::UNAUTHORIZED,
                Json(
                    serde_json::json!({"error": {"message": "Incorrect API key", "type": "invalid_request_error"}}),
                ),
            )),
        }
    } else {
        Ok(())
    }
}

/// Build a prompt string from the OpenAI messages array.
///
/// For a single user message, returns it directly. For multi-turn conversations,
/// formats prior messages as context so the agent sees the full conversation history.
fn build_oai_prompt(messages: &[OaiMessage]) -> String {
    // Single user message — no context needed
    if messages.len() == 1 && messages[0].role == "user" {
        return messages[0].content.clone();
    }

    let mut parts = Vec::new();
    let last_user_idx = messages.iter().rposition(|m| m.role == "user");

    // Format prior messages as conversation context
    let context_msgs: Vec<&OaiMessage> = match last_user_idx {
        Some(idx) => messages[..idx].iter().collect(),
        None => messages.iter().collect(),
    };

    if !context_msgs.is_empty() {
        parts.push("<conversation-history>".to_string());
        for msg in &context_msgs {
            parts.push(format!("[{}]: {}", msg.role, msg.content));
        }
        parts.push("</conversation-history>".to_string());
        parts.push(String::new());
    }

    // Append the last user message as the actual prompt
    if let Some(idx) = last_user_idx {
        parts.push(messages[idx].content.clone());
    }

    parts.join("\n")
}

enum ManagedPromptError {
    NoUserMessage,
    LastMessageNotUser,
}

impl ManagedPromptError {
    fn into_response(self) -> Response {
        match self {
            Self::NoUserMessage => oai_error_response(
                StatusCode::BAD_REQUEST,
                "no user message found",
                "invalid_request_error",
            ),
            Self::LastMessageNotUser => oai_error_response(
                StatusCode::BAD_REQUEST,
                "managed session requests require the last message to be a user turn",
                "invalid_request_error",
            ),
        }
    }
}

fn new_managed_session_id() -> String {
    format!("msess_{}", uuid::Uuid::new_v4().simple())
}

fn new_managed_run_claim_token() -> String {
    format!("claim_{}", uuid::Uuid::new_v4().simple())
}

fn managed_run_lease_expires_at(
    now: chrono::DateTime<chrono::Utc>,
) -> chrono::DateTime<chrono::Utc> {
    now + chrono::Duration::seconds(MANAGED_RUN_LEASE_SECS)
}

fn resolve_managed_turn_prompt(
    req: &OaiChatRequest,
) -> std::result::Result<String, ManagedPromptError> {
    if req.session_id.is_none() {
        return Ok(build_oai_prompt(&req.messages));
    }

    match req.messages.last() {
        Some(message) if message.role == "user" => Ok(message.content.clone()),
        Some(_) => Err(ManagedPromptError::LastMessageNotUser),
        None => Err(ManagedPromptError::NoUserMessage),
    }
}

fn with_managed_response_headers(
    mut response: Response,
    run_id: &str,
    session_id: Option<&str>,
) -> Response {
    if let Ok(value) = HeaderValue::from_str(run_id) {
        response.headers_mut().insert("x-hermes-run-id", value);
    }
    if let Some(session_id) = session_id {
        if let Ok(value) = HeaderValue::from_str(session_id) {
            response.headers_mut().insert("x-hermes-session-id", value);
        }
    }
    response
}

async fn ensure_managed_session(
    session_store: &dyn SessionStore,
    session_id: &str,
    version: &ManagedAgentVersion,
    working_dir: &std::path::Path,
) -> Result<()> {
    if session_store.get_session(session_id).await?.is_some() {
        return Ok(());
    }

    let meta = SessionMeta {
        id: session_id.to_string(),
        source: "managed".to_string(),
        model: version.model.clone(),
        system_prompt: version.system_prompt.clone(),
        cwd: working_dir.to_string_lossy().to_string(),
        started_at: chrono::Utc::now().to_rfc3339(),
        ended_at: None,
        message_count: 0,
        tool_call_count: 0,
        input_tokens: 0,
        output_tokens: 0,
        title: None,
    };

    session_store.create_session(&meta).await
}

async fn persist_managed_session_turn(
    session_store: &dyn SessionStore,
    session_id: &str,
    history: &[Message],
    pre_len: usize,
) {
    let persist_start = pre_len.min(history.len());
    for msg in &history[persist_start..] {
        if let Err(e) = session_store.append_message(session_id, msg).await {
            tracing::warn!(session_id, "failed to persist managed session message: {e}");
            break;
        }
    }
}

fn oai_error_response(
    status: StatusCode,
    message: impl Into<String>,
    error_type: &'static str,
) -> Response {
    oai_error_response_with_code(status, message, error_type, None)
}

fn oai_error_response_with_code(
    status: StatusCode,
    message: impl Into<String>,
    error_type: &'static str,
    error_code: Option<&str>,
) -> Response {
    let mut error = serde_json::Map::new();
    error.insert(
        "message".to_string(),
        serde_json::Value::String(message.into()),
    );
    error.insert(
        "type".to_string(),
        serde_json::Value::String(error_type.to_string()),
    );
    if let Some(code) = error_code {
        error.insert(
            "code".to_string(),
            serde_json::Value::String(code.to_string()),
        );
    }

    (
        status,
        Json(serde_json::json!({
            "error": error
        })),
    )
        .into_response()
}

fn managed_error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
}

fn is_conflict_error(error_text: &str) -> bool {
    error_text.contains("UNIQUE constraint failed")
}

fn apply_run_snapshot(
    mut run: hermes_managed::ManagedRun,
    snapshot: Option<hermes_managed::RunStatusSnapshot>,
) -> hermes_managed::ManagedRun {
    if let Some(snapshot) = snapshot {
        run.status = snapshot.status;
        run.started_at = snapshot.started_at;
        run.updated_at = snapshot.updated_at;
        run.ended_at = snapshot.ended_at;
        run.cancel_requested_at = snapshot.cancel_requested_at;
        run.last_error = snapshot.last_error;
    }
    run
}

fn clamp_runs_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(100).clamp(1, 1000)
}

fn clamp_run_events_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(200).clamp(1, 1000)
}

fn clamp_run_artifacts_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(200).clamp(1, 1000)
}

fn clamp_agents_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(100).clamp(1, 1000)
}

fn terminal_run_event_kind(status: &ManagedRunStatus) -> Option<ManagedRunEventKind> {
    match status {
        ManagedRunStatus::Completed => Some(ManagedRunEventKind::RunCompleted),
        ManagedRunStatus::Failed => Some(ManagedRunEventKind::RunFailed),
        ManagedRunStatus::Interrupted => Some(ManagedRunEventKind::RunInterrupted),
        ManagedRunStatus::Cancelled => Some(ManagedRunEventKind::RunCancelled),
        ManagedRunStatus::TimedOut => Some(ManagedRunEventKind::RunTimedOut),
        ManagedRunStatus::Pending | ManagedRunStatus::Running => None,
    }
}

fn run_event_from_delta(delta: &StreamDelta) -> Option<ManagedRunEventDraft> {
    match delta {
        StreamDelta::ToolCallStart { id, name } => Some(ManagedRunEventDraft {
            kind: ManagedRunEventKind::ToolCallStarted,
            message: None,
            tool_name: Some(name.clone()),
            tool_call_id: Some(id.clone()),
            metadata: None,
        }),
        StreamDelta::ToolProgress { tool, status } => Some(ManagedRunEventDraft {
            kind: ManagedRunEventKind::ToolProgress,
            message: Some(status.clone()),
            tool_name: Some(tool.clone()),
            tool_call_id: None,
            metadata: None,
        }),
        StreamDelta::ToolEvent {
            kind,
            tool,
            call_id,
            message,
            metadata,
        } => Some(ManagedRunEventDraft {
            kind: ManagedRunEventKind::parse(kind)?,
            message: message.clone(),
            tool_name: Some(tool.clone()),
            tool_call_id: call_id.clone(),
            metadata: metadata.clone(),
        }),
        StreamDelta::TextDelta(_)
        | StreamDelta::ReasoningDelta(_)
        | StreamDelta::ToolCallArgsDelta { .. }
        | StreamDelta::Done => None,
    }
}

async fn append_run_event(store: &ManagedStore, run_id: &str, event: ManagedRunEventDraft) {
    let _ = store.append_run_event(run_id, &event).await;
}

async fn append_terminal_run_event(
    store: &ManagedStore,
    run_id: &str,
    status: &ManagedRunStatus,
    message: Option<String>,
) {
    let Some(kind) = terminal_run_event_kind(status) else {
        return;
    };

    append_run_event(
        store,
        run_id,
        ManagedRunEventDraft {
            kind,
            message,
            tool_name: None,
            tool_call_id: None,
            metadata: None,
        },
    )
    .await;
}

fn managed_takeover_state_label(status: &ManagedRunStatus) -> Option<&'static str> {
    match status {
        ManagedRunStatus::Pending => None,
        ManagedRunStatus::Running => Some("active"),
        ManagedRunStatus::Completed => Some("completed"),
        ManagedRunStatus::Failed => Some("failed"),
        ManagedRunStatus::Cancelled => Some("cancelled"),
        ManagedRunStatus::TimedOut => Some("timed_out"),
        ManagedRunStatus::Interrupted => Some("interrupted"),
    }
}

fn managed_takeover_update_message(
    status: &ManagedRunStatus,
    replay_run_id: &str,
    source_run_id: &str,
    lineage_depth: usize,
) -> Option<String> {
    let detail = match status {
        ManagedRunStatus::Running => "actively owns continuation of",
        ManagedRunStatus::Completed => "completed continuation lineage for",
        ManagedRunStatus::Failed => "failed after taking over continuation of",
        ManagedRunStatus::Cancelled => "was cancelled after taking over continuation of",
        ManagedRunStatus::TimedOut => "timed out after taking over continuation of",
        ManagedRunStatus::Interrupted => "was interrupted after taking over continuation of",
        ManagedRunStatus::Pending => return None,
    };
    Some(if lineage_depth > 1 {
        format!(
            "replay descendant {replay_run_id} at depth {lineage_depth} {detail} {source_run_id}"
        )
    } else {
        format!("replay child {replay_run_id} {detail} {source_run_id}")
    })
}

async fn append_takeover_update_for_source_run(
    store: &ManagedStore,
    replay_run: &ManagedRun,
    source_run_id: &str,
    lineage_depth: usize,
    current_owner_worker_id: Option<&str>,
) {
    let Some(takeover_state) = managed_takeover_state_label(&replay_run.status) else {
        return;
    };
    let Some(message) = managed_takeover_update_message(
        &replay_run.status,
        &replay_run.id,
        source_run_id,
        lineage_depth,
    ) else {
        return;
    };

    let summary = match load_managed_run_derived_summary(store, &replay_run.id).await {
        Ok(summary) => Some(summary),
        Err(e) => {
            tracing::warn!(
                run_id = replay_run.id,
                source_run_id,
                "failed to load replay child summary for source takeover update: {e}"
            );
            None
        }
    };
    let continuation = summary
        .as_ref()
        .and_then(|summary| summary.continuation.as_ref());
    let replay_provenance = summary
        .as_ref()
        .and_then(|summary| summary.replay_provenance.as_ref());
    let follow_target_owner_snapshot = summary
        .as_ref()
        .and_then(|summary| summary.ownership.as_ref());
    let follow_target_ownership_claim = summary
        .as_ref()
        .and_then(|summary| summary.ownership_claim.as_ref());
    let follow_target_recovery_decision = summary
        .as_ref()
        .and_then(|summary| summary.recovery_decision.as_ref());
    let follow_target_continuation_checkpoint = summary
        .as_ref()
        .and_then(|summary| summary.continuation_checkpoint.as_ref());
    let follow_target_provider_call_fence = summary
        .as_ref()
        .and_then(|summary| summary.provider_call_fence.as_ref());
    let follow_target_process_handoff = summary
        .as_ref()
        .and_then(|summary| summary.process_handoff.as_ref());
    let follow_target_browser_handoff = summary
        .as_ref()
        .and_then(|summary| summary.browser_handoff.as_ref());
    let follow_target_browser_session_checkpoint = summary
        .as_ref()
        .and_then(|summary| summary.browser_session_checkpoint.as_ref())
        .filter(|checkpoint| checkpoint.session_open);
    let follow_target_mcp_handoff = summary
        .as_ref()
        .and_then(|summary| summary.mcp_handoff.as_ref());
    let follow_target_mcp_runtime_checkpoint = summary
        .as_ref()
        .and_then(|summary| summary.mcp_runtime_checkpoint.as_ref())
        .filter(|checkpoint| checkpoint.live_runtime_required);
    let follow_target_artifact_continuity = summary
        .as_ref()
        .and_then(|summary| summary.artifact_continuity.as_ref());
    let follow_target_ownership_release = summary
        .as_ref()
        .and_then(|summary| summary.ownership_release.as_ref());
    let follow_target_takeover_assessment = match store
        .get_latest_run_event_by_kind(&replay_run.id, ManagedRunEventKind::RunTakeoverAssessed)
        .await
    {
        Ok(event) => event
            .as_ref()
            .and_then(hermes_managed::managed_run_takeover_assessment_from_event),
        Err(e) => {
            tracing::warn!(
                run_id = replay_run.id,
                source_run_id,
                "failed to load replay child takeover assessment for source takeover update: {e}"
            );
            None
        }
    };
    let source_boundary = if lineage_depth == 1 {
        continuation
            .and_then(|value| value.source_boundary)
            .map(|boundary| {
                match boundary {
            hermes_managed::ManagedRunContinuationBoundaryKind::UserCheckpointed => {
                "user_checkpointed"
            }
            hermes_managed::ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed => {
                "assistant_response_checkpointed"
            }
            hermes_managed::ManagedRunContinuationBoundaryKind::PendingToolCalls => {
                "pending_tool_calls"
            }
            hermes_managed::ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed => {
                "tool_results_checkpointed"
            }
        }
            })
    } else {
        None
    };
    let source_interruption_cause = if lineage_depth == 1 {
        continuation
            .and_then(|value| value.source_interruption_cause)
            .map(|cause| match cause {
                ManagedRunInterruptionCause::LeaseExpired => "lease_expired",
                ManagedRunInterruptionCause::OwnershipNotEstablished => "ownership_not_established",
            })
    } else {
        None
    };
    let leaf_source_run_id = (lineage_depth > 1)
        .then(|| continuation.map(|value| value.source_run_id.as_str()))
        .flatten();
    let leaf_source_boundary = (lineage_depth > 1)
        .then_some(
            continuation.and_then(|value| value.source_boundary).map(|boundary| match boundary {
                hermes_managed::ManagedRunContinuationBoundaryKind::UserCheckpointed => {
                    "user_checkpointed"
                }
                hermes_managed::ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed => {
                    "assistant_response_checkpointed"
                }
                hermes_managed::ManagedRunContinuationBoundaryKind::PendingToolCalls => {
                    "pending_tool_calls"
                }
                hermes_managed::ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed => {
                    "tool_results_checkpointed"
                }
            }),
        )
        .flatten();
    let leaf_source_interruption_cause = (lineage_depth > 1)
        .then_some(
            continuation
                .and_then(|value| value.source_interruption_cause)
                .map(|cause| match cause {
                    ManagedRunInterruptionCause::LeaseExpired => "lease_expired",
                    ManagedRunInterruptionCause::OwnershipNotEstablished => {
                        "ownership_not_established"
                    }
                }),
        )
        .flatten();

    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "replay_run_id".to_string(),
        serde_json::json!(replay_run.id.as_str()),
    );
    metadata.insert(
        "replay_run_status".to_string(),
        serde_json::json!(replay_run.status.as_str()),
    );
    metadata.insert(
        "takeover_state".to_string(),
        serde_json::json!(takeover_state),
    );
    metadata.insert(
        "takeover_lineage_id".to_string(),
        serde_json::json!(
            continuation
                .and_then(|value| value.takeover_lineage_id.as_deref())
                .or_else(
                    || replay_provenance.and_then(|value| value.takeover_lineage_id.as_deref())
                )
        ),
    );
    metadata.insert(
        "source_run_id".to_string(),
        serde_json::json!(source_run_id),
    );
    metadata.insert(
        "lineage_depth".to_string(),
        serde_json::json!(lineage_depth),
    );
    metadata.insert(
        "replay_root_run_id".to_string(),
        serde_json::json!(replay_provenance.map(|value| value.root_run_id.as_str())),
    );
    metadata.insert(
        "replay_depth".to_string(),
        serde_json::json!(replay_provenance.map(|value| value.replay_depth)),
    );
    metadata.insert(
        "replay_trigger".to_string(),
        serde_json::json!(replay_provenance.map(|value| match value.trigger {
            hermes_managed::ManagedRunReplayTrigger::ManualReplay => "manual_replay",
            hermes_managed::ManagedRunReplayTrigger::InterruptedAutoReplay => {
                "interrupted_auto_replay"
            }
        })),
    );
    metadata.insert(
        "source_boundary".to_string(),
        serde_json::json!(source_boundary),
    );
    metadata.insert(
        "source_interruption_cause".to_string(),
        serde_json::json!(source_interruption_cause),
    );
    metadata.insert(
        "leaf_source_run_id".to_string(),
        serde_json::json!(leaf_source_run_id),
    );
    metadata.insert(
        "leaf_source_boundary".to_string(),
        serde_json::json!(leaf_source_boundary),
    );
    metadata.insert(
        "leaf_source_interruption_cause".to_string(),
        serde_json::json!(leaf_source_interruption_cause),
    );
    metadata.insert(
        "evaluated_by_worker_id".to_string(),
        serde_json::json!(continuation.and_then(|value| value.evaluated_by_worker_id.as_deref())),
    );
    metadata.insert(
        "takeover_worker_id".to_string(),
        serde_json::json!(continuation.and_then(|value| value.takeover_worker_id.as_deref())),
    );
    metadata.insert(
        "current_owner_worker_id".to_string(),
        serde_json::json!(current_owner_worker_id),
    );
    metadata.insert(
        "current_owner_state".to_string(),
        serde_json::json!(follow_target_owner_snapshot.map(|owner| match owner.state {
            hermes_managed::ManagedRunOwnerState::Active => "active",
            hermes_managed::ManagedRunOwnerState::Expired => "expired",
            hermes_managed::ManagedRunOwnerState::Incomplete => "incomplete",
        })),
    );
    metadata.insert(
        "current_owner_claimed_at".to_string(),
        serde_json::json!(
            follow_target_owner_snapshot
                .and_then(|owner| owner.claimed_at)
                .map(|value| value.to_rfc3339())
        ),
    );
    metadata.insert(
        "current_owner_last_heartbeat_at".to_string(),
        serde_json::json!(
            follow_target_owner_snapshot
                .and_then(|owner| owner.last_heartbeat_at)
                .map(|value| value.to_rfc3339())
        ),
    );
    metadata.insert(
        "current_owner_lease_expires_at".to_string(),
        serde_json::json!(
            follow_target_owner_snapshot
                .and_then(|owner| owner.lease_expires_at)
                .map(|value| value.to_rfc3339())
        ),
    );
    metadata.insert(
        "follow_target_ownership_claim_worker_id".to_string(),
        serde_json::json!(follow_target_ownership_claim.map(|claim| claim.worker_id.as_str())),
    );
    metadata.insert(
        "follow_target_ownership_claim_claimed_at".to_string(),
        serde_json::json!(
            follow_target_ownership_claim
                .and_then(|claim| claim.claimed_at)
                .map(|value| value.to_rfc3339())
        ),
    );
    metadata.insert(
        "follow_target_ownership_claim_lease_expires_at".to_string(),
        serde_json::json!(
            follow_target_ownership_claim
                .and_then(|claim| claim.lease_expires_at)
                .map(|value| value.to_rfc3339())
        ),
    );
    metadata.insert(
        "reused_session_id".to_string(),
        serde_json::json!(continuation.map(|value| value.reused_session_id)),
    );
    metadata.insert(
        "resumed_existing_turn".to_string(),
        serde_json::json!(continuation.map(|value| value.resumed_existing_turn)),
    );
    metadata.insert(
        "follow_target_recovery_decision".to_string(),
        serde_json::json!(
            follow_target_recovery_decision
                .map(|decision| managed_recovery_decision_kind_str(decision.decision))
        ),
    );
    metadata.insert(
        "follow_target_recovery_reason".to_string(),
        serde_json::json!(
            follow_target_recovery_decision
                .and_then(|decision| decision.reason)
                .map(managed_recovery_decision_reason_str)
        ),
    );
    metadata.insert(
        "follow_target_recovery_evaluated_by_worker_id".to_string(),
        serde_json::json!(
            follow_target_recovery_decision
                .and_then(|decision| decision.evaluated_by_worker_id.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_recovery_note".to_string(),
        serde_json::json!(
            follow_target_recovery_decision.and_then(|decision| decision.note.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_checkpoint_kind".to_string(),
        serde_json::json!(
            follow_target_continuation_checkpoint
                .map(|checkpoint| managed_continuation_boundary_str(checkpoint.kind))
        ),
    );
    metadata.insert(
        "follow_target_checkpoint_safe_action".to_string(),
        serde_json::json!(
            follow_target_continuation_checkpoint
                .map(|checkpoint| managed_continuation_action_str(checkpoint.safe_action))
        ),
    );
    metadata.insert(
        "follow_target_checkpoint_history_len".to_string(),
        serde_json::json!(
            follow_target_continuation_checkpoint.map(|checkpoint| checkpoint.history_len)
        ),
    );
    metadata.insert(
        "follow_target_checkpoint_pending_tool_calls".to_string(),
        serde_json::json!(
            follow_target_continuation_checkpoint.map(|checkpoint| checkpoint.pending_tool_calls)
        ),
    );
    metadata.insert(
        "follow_target_provider_fence_request_history_len".to_string(),
        serde_json::json!(follow_target_provider_call_fence.map(|fence| fence.request_history_len)),
    );
    metadata.insert(
        "follow_target_provider_fence_tool_count".to_string(),
        serde_json::json!(follow_target_provider_call_fence.map(|fence| fence.tool_count)),
    );
    metadata.insert(
        "follow_target_provider_fence_safe_resume_from_kind".to_string(),
        serde_json::json!(
            follow_target_provider_call_fence
                .map(|fence| managed_continuation_boundary_str(fence.safe_resume_from.kind))
        ),
    );
    metadata.insert(
        "follow_target_provider_fence_safe_resume_from_action".to_string(),
        serde_json::json!(
            follow_target_provider_call_fence
                .map(|fence| managed_continuation_action_str(fence.safe_resume_from.safe_action))
        ),
    );
    metadata.insert(
        "follow_target_provider_fence_safe_resume_from_history_len".to_string(),
        serde_json::json!(
            follow_target_provider_call_fence.map(|fence| fence.safe_resume_from.history_len)
        ),
    );
    metadata.insert(
        "follow_target_provider_fence_safe_resume_from_pending_tool_calls".to_string(),
        serde_json::json!(
            follow_target_provider_call_fence
                .map(|fence| fence.safe_resume_from.pending_tool_calls)
        ),
    );
    metadata.insert(
        "follow_target_provider_fence_note".to_string(),
        serde_json::json!(
            follow_target_provider_call_fence.and_then(|fence| fence.note.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_process_handoff_tool_name".to_string(),
        serde_json::json!(follow_target_process_handoff.map(|handoff| handoff.tool_name.as_str())),
    );
    metadata.insert(
        "follow_target_process_handoff_tool_call_id".to_string(),
        serde_json::json!(
            follow_target_process_handoff.and_then(|handoff| handoff.tool_call_id.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_process_handoff_state".to_string(),
        serde_json::json!(
            follow_target_process_handoff.map(|handoff| match handoff.state {
                hermes_managed::ManagedRunProcessHandoffState::Running => "running",
                hermes_managed::ManagedRunProcessHandoffState::Completed => "completed",
                hermes_managed::ManagedRunProcessHandoffState::Failed => "failed",
                hermes_managed::ManagedRunProcessHandoffState::TimedOut => "timed_out",
            })
        ),
    );
    metadata.insert(
        "follow_target_process_handoff_replay_disposition".to_string(),
        serde_json::json!(follow_target_process_handoff.map(|handoff| {
            match handoff.replay_disposition {
                hermes_managed::ManagedRunProcessReplayDisposition::SafeToReplay => {
                    "safe_to_replay"
                }
                hermes_managed::ManagedRunProcessReplayDisposition::UnsafeSideEffectWindow => {
                    "unsafe_side_effect_window"
                }
                hermes_managed::ManagedRunProcessReplayDisposition::CompletedButNotRecorded => {
                    "completed_but_not_recorded"
                }
            }
        })),
    );
    metadata.insert(
        "follow_target_process_handoff_process_group".to_string(),
        serde_json::json!(follow_target_process_handoff.and_then(|handoff| handoff.process_group)),
    );
    metadata.insert(
        "follow_target_process_handoff_timeout_secs".to_string(),
        serde_json::json!(follow_target_process_handoff.and_then(|handoff| handoff.timeout_secs)),
    );
    metadata.insert(
        "follow_target_process_handoff_exit_code".to_string(),
        serde_json::json!(follow_target_process_handoff.and_then(|handoff| handoff.exit_code)),
    );
    metadata.insert(
        "follow_target_process_handoff_stdout_preview".to_string(),
        serde_json::json!(
            follow_target_process_handoff.and_then(|handoff| handoff.stdout_preview.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_process_handoff_stderr_preview".to_string(),
        serde_json::json!(
            follow_target_process_handoff.and_then(|handoff| handoff.stderr_preview.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_process_handoff_note".to_string(),
        serde_json::json!(
            follow_target_process_handoff.and_then(|handoff| handoff.note.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_browser_handoff_action".to_string(),
        serde_json::json!(follow_target_browser_handoff.map(|handoff| handoff.action.as_str())),
    );
    metadata.insert(
        "follow_target_browser_handoff_state".to_string(),
        serde_json::json!(
            follow_target_browser_handoff.map(|handoff| match handoff.state {
                hermes_managed::ManagedRunBrowserHandoffState::Started => "started",
                hermes_managed::ManagedRunBrowserHandoffState::Completed => "completed",
                hermes_managed::ManagedRunBrowserHandoffState::Failed => "failed",
            })
        ),
    );
    metadata.insert(
        "follow_target_browser_handoff_replay_disposition".to_string(),
        serde_json::json!(follow_target_browser_handoff.map(|handoff| {
            match handoff.replay_disposition {
                hermes_managed::ManagedRunBrowserReplayDisposition::SafeToReplay => {
                    "safe_to_replay"
                }
                hermes_managed::ManagedRunBrowserReplayDisposition::UnsafeSideEffectWindow => {
                    "unsafe_side_effect_window"
                }
                hermes_managed::ManagedRunBrowserReplayDisposition::CompletedButNotRecorded => {
                    "completed_but_not_recorded"
                }
            }
        })),
    );
    metadata.insert(
        "follow_target_browser_handoff_target".to_string(),
        serde_json::json!(
            follow_target_browser_handoff.and_then(|handoff| handoff.target.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_browser_handoff_wait_for_navigation".to_string(),
        serde_json::json!(follow_target_browser_handoff.map(|handoff| handoff.wait_for_navigation)),
    );
    metadata.insert(
        "follow_target_browser_handoff_page_url".to_string(),
        serde_json::json!(
            follow_target_browser_handoff.and_then(|handoff| handoff.page_url.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_browser_handoff_page_title".to_string(),
        serde_json::json!(
            follow_target_browser_handoff.and_then(|handoff| handoff.page_title.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_browser_handoff_output_preview".to_string(),
        serde_json::json!(
            follow_target_browser_handoff.and_then(|handoff| handoff.output_preview.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_browser_handoff_note".to_string(),
        serde_json::json!(
            follow_target_browser_handoff.and_then(|handoff| handoff.note.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_browser_session_action".to_string(),
        serde_json::json!(
            follow_target_browser_session_checkpoint.map(|checkpoint| checkpoint.action.as_str())
        ),
    );
    metadata.insert(
        "follow_target_browser_session_open".to_string(),
        serde_json::json!(
            follow_target_browser_session_checkpoint.map(|checkpoint| checkpoint.session_open)
        ),
    );
    metadata.insert(
        "follow_target_browser_session_target".to_string(),
        serde_json::json!(
            follow_target_browser_session_checkpoint
                .and_then(|checkpoint| checkpoint.target.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_browser_session_page_url".to_string(),
        serde_json::json!(
            follow_target_browser_session_checkpoint
                .and_then(|checkpoint| checkpoint.page_url.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_browser_session_page_title".to_string(),
        serde_json::json!(
            follow_target_browser_session_checkpoint
                .and_then(|checkpoint| checkpoint.page_title.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_browser_session_note".to_string(),
        serde_json::json!(
            follow_target_browser_session_checkpoint
                .and_then(|checkpoint| checkpoint.note.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_mcp_handoff_tool_name".to_string(),
        serde_json::json!(follow_target_mcp_handoff.map(|handoff| handoff.tool_name.as_str())),
    );
    metadata.insert(
        "follow_target_mcp_handoff_state".to_string(),
        serde_json::json!(
            follow_target_mcp_handoff.map(|handoff| match handoff.state {
                hermes_managed::ManagedRunMcpHandoffState::Started => "started",
                hermes_managed::ManagedRunMcpHandoffState::Completed => "completed",
                hermes_managed::ManagedRunMcpHandoffState::Failed => "failed",
            })
        ),
    );
    metadata.insert(
        "follow_target_mcp_handoff_replay_disposition".to_string(),
        serde_json::json!(follow_target_mcp_handoff.map(
            |handoff| match handoff.replay_disposition {
                hermes_managed::ManagedRunMcpReplayDisposition::SafeToReplay => "safe_to_replay",
                hermes_managed::ManagedRunMcpReplayDisposition::UnsafeSideEffectWindow =>
                    "unsafe_side_effect_window",
                hermes_managed::ManagedRunMcpReplayDisposition::CompletedButNotRecorded =>
                    "completed_but_not_recorded",
            }
        )),
    );
    metadata.insert(
        "follow_target_mcp_handoff_read_only".to_string(),
        serde_json::json!(follow_target_mcp_handoff.map(|handoff| handoff.read_only)),
    );
    metadata.insert(
        "follow_target_mcp_handoff_requires_live_runtime".to_string(),
        serde_json::json!(follow_target_mcp_handoff.map(|handoff| handoff.requires_live_runtime)),
    );
    metadata.insert(
        "follow_target_mcp_handoff_server".to_string(),
        serde_json::json!(follow_target_mcp_handoff.and_then(|handoff| handoff.server.as_deref())),
    );
    metadata.insert(
        "follow_target_mcp_handoff_transport".to_string(),
        serde_json::json!(
            follow_target_mcp_handoff.and_then(|handoff| handoff.transport.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_mcp_handoff_target".to_string(),
        serde_json::json!(follow_target_mcp_handoff.and_then(|handoff| handoff.target.as_deref())),
    );
    metadata.insert(
        "follow_target_mcp_handoff_output_preview".to_string(),
        serde_json::json!(
            follow_target_mcp_handoff.and_then(|handoff| handoff.output_preview.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_mcp_handoff_note".to_string(),
        serde_json::json!(follow_target_mcp_handoff.and_then(|handoff| handoff.note.as_deref())),
    );
    metadata.insert(
        "follow_target_mcp_runtime_tool_name".to_string(),
        serde_json::json!(
            follow_target_mcp_runtime_checkpoint.map(|checkpoint| checkpoint.tool_name.as_str())
        ),
    );
    metadata.insert(
        "follow_target_mcp_runtime_live_runtime_required".to_string(),
        serde_json::json!(
            follow_target_mcp_runtime_checkpoint.map(|checkpoint| checkpoint.live_runtime_required)
        ),
    );
    metadata.insert(
        "follow_target_mcp_runtime_active_subscription_count".to_string(),
        serde_json::json!(
            follow_target_mcp_runtime_checkpoint
                .map(|checkpoint| checkpoint.active_subscription_count)
        ),
    );
    metadata.insert(
        "follow_target_mcp_runtime_active_servers".to_string(),
        serde_json::json!(
            follow_target_mcp_runtime_checkpoint
                .map(|checkpoint| checkpoint.active_servers.clone())
        ),
    );
    metadata.insert(
        "follow_target_mcp_runtime_server".to_string(),
        serde_json::json!(
            follow_target_mcp_runtime_checkpoint
                .and_then(|checkpoint| checkpoint.server.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_mcp_runtime_transport".to_string(),
        serde_json::json!(
            follow_target_mcp_runtime_checkpoint
                .and_then(|checkpoint| checkpoint.transport.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_mcp_runtime_target".to_string(),
        serde_json::json!(
            follow_target_mcp_runtime_checkpoint
                .and_then(|checkpoint| checkpoint.target.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_mcp_runtime_note".to_string(),
        serde_json::json!(
            follow_target_mcp_runtime_checkpoint.and_then(|checkpoint| checkpoint.note.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_artifact_kind".to_string(),
        serde_json::json!(
            follow_target_artifact_continuity.map(|artifact| artifact.latest_kind.as_str())
        ),
    );
    metadata.insert(
        "follow_target_artifact_label".to_string(),
        serde_json::json!(
            follow_target_artifact_continuity.map(|artifact| artifact.latest_label.as_str())
        ),
    );
    metadata.insert(
        "follow_target_artifact_run_id".to_string(),
        serde_json::json!(
            follow_target_artifact_continuity.map(|artifact| artifact.latest_run_id.as_str())
        ),
    );
    metadata.insert(
        "follow_target_artifact_run_is_current".to_string(),
        serde_json::json!(
            follow_target_artifact_continuity.map(|artifact| artifact.latest_run_is_current)
        ),
    );
    metadata.insert(
        "follow_target_artifact_lineage_depth".to_string(),
        serde_json::json!(follow_target_artifact_continuity.map(|artifact| artifact.lineage_depth)),
    );
    metadata.insert(
        "follow_target_artifact_tool_name".to_string(),
        serde_json::json!(
            follow_target_artifact_continuity
                .and_then(|artifact| artifact.latest_tool_name.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_artifact_tool_call_id".to_string(),
        serde_json::json!(
            follow_target_artifact_continuity
                .and_then(|artifact| artifact.latest_tool_call_id.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_artifact_content_preview".to_string(),
        serde_json::json!(
            follow_target_artifact_continuity
                .and_then(|artifact| artifact.latest_content_preview.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_artifact_note".to_string(),
        serde_json::json!(
            follow_target_artifact_continuity.and_then(|artifact| artifact.note.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_provider_call_in_flight".to_string(),
        serde_json::json!(
            follow_target_takeover_assessment
                .as_ref()
                .map(|assessment| assessment.provider_call_in_flight)
        ),
    );
    metadata.insert(
        "follow_target_process_handoff_risk".to_string(),
        serde_json::json!(
            follow_target_takeover_assessment
                .as_ref()
                .map(|assessment| assessment.process_handoff_risk)
        ),
    );
    metadata.insert(
        "follow_target_browser_handoff_risk".to_string(),
        serde_json::json!(
            follow_target_takeover_assessment
                .as_ref()
                .map(|assessment| assessment.browser_handoff_risk)
        ),
    );
    metadata.insert(
        "follow_target_browser_session_state".to_string(),
        serde_json::json!(
            follow_target_takeover_assessment
                .as_ref()
                .map(|assessment| assessment.browser_session_state)
        ),
    );
    metadata.insert(
        "follow_target_mcp_handoff_risk".to_string(),
        serde_json::json!(
            follow_target_takeover_assessment
                .as_ref()
                .map(|assessment| assessment.mcp_handoff_risk)
        ),
    );
    metadata.insert(
        "follow_target_mcp_runtime_state".to_string(),
        serde_json::json!(
            follow_target_takeover_assessment
                .as_ref()
                .map(|assessment| assessment.mcp_runtime_state)
        ),
    );
    metadata.insert(
        "follow_target_replay_depth".to_string(),
        serde_json::json!(
            follow_target_takeover_assessment
                .as_ref()
                .map(|assessment| assessment.replay_depth)
        ),
    );
    metadata.insert(
        "follow_target_max_auto_replays".to_string(),
        serde_json::json!(
            follow_target_takeover_assessment
                .as_ref()
                .map(|assessment| assessment.max_auto_replays)
        ),
    );
    metadata.insert(
        "follow_target_assessed_by_worker_id".to_string(),
        serde_json::json!(
            follow_target_takeover_assessment
                .as_ref()
                .and_then(|assessment| assessment.evaluated_by_worker_id.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_assessment_note".to_string(),
        serde_json::json!(
            follow_target_takeover_assessment
                .as_ref()
                .and_then(|assessment| assessment.note.as_deref())
        ),
    );
    metadata.insert(
        "follow_target_ownership_released_worker_id".to_string(),
        serde_json::json!(
            follow_target_ownership_release.map(|release| release.worker_id.as_str())
        ),
    );
    metadata.insert(
        "follow_target_ownership_released_reason".to_string(),
        serde_json::json!(
            follow_target_ownership_release
                .map(|release| managed_ownership_release_reason_str(release.reason))
        ),
    );
    metadata.insert(
        "note".to_string(),
        serde_json::json!(continuation.and_then(|value| value.note.as_deref())),
    );

    let event = ManagedRunEventDraft {
        kind: ManagedRunEventKind::RunTakeoverUpdated,
        message: Some(message),
        tool_name: None,
        tool_call_id: None,
        metadata: Some(serde_json::Value::Object(metadata)),
    };

    match store
        .get_latest_run_event_by_kind(source_run_id, ManagedRunEventKind::RunTakeoverUpdated)
        .await
    {
        Ok(Some(existing))
            if existing.message == event.message && existing.metadata == event.metadata =>
        {
            return;
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(
            run_id = replay_run.id,
            source_run_id,
            "failed to inspect existing source takeover update event: {e}"
        ),
    }

    append_run_event(store, source_run_id, event).await;
}

pub(crate) async fn append_source_takeover_update_for_replay_child(
    store: &ManagedStore,
    child_run: &ManagedRun,
    current_owner_worker_id: Option<&str>,
) {
    let Some(mut source_run_id) = child_run.replay_of_run_id.clone() else {
        return;
    };
    let mut lineage_depth = 1usize;
    for _ in 0..64 {
        append_takeover_update_for_source_run(
            store,
            child_run,
            &source_run_id,
            lineage_depth,
            current_owner_worker_id,
        )
        .await;

        let next_source_run = match store.get_run(&source_run_id).await {
            Ok(Some(run)) => run,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(
                    run_id = child_run.id,
                    source_run_id,
                    "failed to load replay source run while propagating takeover update: {e}"
                );
                return;
            }
        };
        let Some(parent_source_run_id) = next_source_run.replay_of_run_id else {
            return;
        };
        source_run_id = parent_source_run_id;
        lineage_depth += 1;
    }

    tracing::warn!(
        run_id = child_run.id,
        "stopped replay-lineage takeover propagation after reaching max ancestry depth"
    );
}

pub(crate) async fn append_ancestor_follow_replay_decisions_for_replay_child(
    store: &ManagedStore,
    replay_leaf_run: &ManagedRun,
) {
    let Some(mut source_run_id) = replay_leaf_run.replay_of_run_id.clone() else {
        return;
    };
    let leaf_summary = match load_managed_run_derived_summary(store, &replay_leaf_run.id).await {
        Ok(summary) => summary,
        Err(e) => {
            tracing::warn!(
                run_id = replay_leaf_run.id,
                "failed to load replay leaf summary for ancestor follow_replay propagation: {e}"
            );
            return;
        }
    };
    let Some(continuation) = leaf_summary.continuation.as_ref() else {
        return;
    };
    let evaluated_by_worker_id = continuation.evaluated_by_worker_id.clone().or_else(|| {
        leaf_summary
            .replay_provenance
            .as_ref()
            .map(|value| value.trigger_worker_id.clone())
    });
    let takeover_worker_id = leaf_summary
        .ownership
        .as_ref()
        .map(|owner| owner.worker_id.clone())
        .or_else(|| continuation.takeover_worker_id.clone());
    let takeover_lineage_id = continuation.takeover_lineage_id.clone().or_else(|| {
        leaf_summary
            .replay_provenance
            .as_ref()
            .and_then(|summary| summary.takeover_lineage_id.clone())
    });

    let mut direct_follow_run = replay_leaf_run.clone();
    let mut lineage_depth = 1usize;
    for _ in 0..64 {
        let Some(source_run) = (match store.get_run(&source_run_id).await {
            Ok(run) => run,
            Err(e) => {
                tracing::warn!(
                    run_id = replay_leaf_run.id,
                    source_run_id,
                    "failed to load replay source run while propagating follow_replay decisions: {e}"
                );
                return;
            }
        }) else {
            return;
        };

        if lineage_depth > 1 {
            let reason = (replay_leaf_run.status == ManagedRunStatus::Running)
                .then_some(ManagedRunRecoveryDecisionReason::ReplayChildActive);
            let note = if replay_leaf_run.status == ManagedRunStatus::Running {
                format!(
                    "continuation is currently owned by replay descendant {} at depth {} via direct child {}",
                    replay_leaf_run.id, lineage_depth, direct_follow_run.id
                )
            } else {
                format!(
                    "latest replay descendant {} at depth {} is {} via direct child {}",
                    replay_leaf_run.id,
                    lineage_depth,
                    replay_leaf_run.status.as_str(),
                    direct_follow_run.id
                )
            };
            let decision = ManagedRunRecoveryDecisionSummary {
                decision: ManagedRunRecoveryDecisionKind::FollowReplay,
                reason,
                replay_run_id: Some(direct_follow_run.id.clone()),
                takeover_lineage_id: takeover_lineage_id.clone(),
                evaluated_by_worker_id: evaluated_by_worker_id.clone(),
                takeover_worker_id: takeover_worker_id.clone(),
                worker_id: takeover_worker_id.clone(),
                active_follow_target_run_id: Some(replay_leaf_run.id.clone()),
                active_follow_target_status: Some(replay_leaf_run.status.clone()),
                active_follow_target_lineage_depth: Some(lineage_depth),
                source_boundary: None,
                note: Some(note),
            };
            if let Err(e) =
                append_managed_recovery_decision_if_changed(store, &source_run.id, decision).await
            {
                tracing::warn!(
                    run_id = replay_leaf_run.id,
                    source_run_id = source_run.id,
                    "failed to append ancestor follow_replay decision: {e}"
                );
            }
        }

        let Some(parent_source_run_id) = source_run.replay_of_run_id.clone() else {
            return;
        };
        direct_follow_run = source_run;
        source_run_id = parent_source_run_id;
        lineage_depth += 1;
    }

    tracing::warn!(
        run_id = replay_leaf_run.id,
        "stopped replay-lineage follow_replay propagation after reaching max ancestry depth"
    );
}

fn managed_mcp_admission_rejection_event(
    rejection: &hermes_managed::ManagedMcpAdmissionRejection,
) -> ManagedRunEventDraft {
    ManagedRunEventDraft {
        kind: ManagedRunEventKind::RunMcpAdmissionRejected,
        message: Some(format!(
            "Managed MCP admission rejected: {}",
            rejection.code
        )),
        tool_name: None,
        tool_call_id: None,
        metadata: serde_json::to_value(rejection).ok(),
    }
}

fn collect_run_mcp_admission_rejections(
    summaries: BTreeMap<String, ManagedRunDerivedSummary>,
) -> BTreeMap<String, ManagedMcpAdmissionRejection> {
    summaries
        .into_iter()
        .filter_map(|(run_id, summary)| {
            summary
                .mcp_admission_rejection
                .map(|rejection| (run_id, rejection))
        })
        .collect()
}

fn collect_run_cleanup_failures(
    summaries: BTreeMap<String, ManagedRunDerivedSummary>,
) -> BTreeMap<String, ManagedRunCleanupFailureSummary> {
    summaries
        .into_iter()
        .filter_map(|(run_id, summary)| summary.cleanup_failure.map(|cleanup| (run_id, cleanup)))
        .collect()
}

fn collect_run_recovery_decisions(
    summaries: BTreeMap<String, ManagedRunDerivedSummary>,
) -> BTreeMap<String, ManagedRunRecoveryDecisionSummary> {
    summaries
        .into_iter()
        .filter_map(|(run_id, summary)| {
            summary.recovery_decision.map(|decision| (run_id, decision))
        })
        .collect()
}

fn collect_run_takeovers(
    summaries: BTreeMap<String, ManagedRunDerivedSummary>,
) -> BTreeMap<String, hermes_managed::ManagedRunTakeoverSummary> {
    summaries
        .into_iter()
        .filter_map(|(run_id, summary)| summary.takeover.map(|takeover| (run_id, takeover)))
        .collect()
}

fn collect_run_ownerships(
    summaries: BTreeMap<String, ManagedRunDerivedSummary>,
) -> BTreeMap<String, hermes_managed::ManagedRunOwnerSnapshot> {
    summaries
        .into_iter()
        .filter_map(|(run_id, summary)| summary.ownership.map(|ownership| (run_id, ownership)))
        .collect()
}

fn collect_run_takeover_assessments(
    summaries: BTreeMap<String, ManagedRunDerivedSummary>,
) -> BTreeMap<String, ManagedRunTakeoverAssessmentSummary> {
    summaries
        .into_iter()
        .filter_map(|(run_id, summary)| summary.takeover_assessment.map(|value| (run_id, value)))
        .collect()
}

fn collect_run_recovery_hints(
    summaries: BTreeMap<String, ManagedRunDerivedSummary>,
) -> BTreeMap<String, ManagedRunRecoveryHint> {
    summaries
        .into_iter()
        .filter_map(|(run_id, summary)| summary.recovery_hint.map(|hint| (run_id, hint)))
        .collect()
}

async fn build_managed_run_envelope(
    managed: &ManagedApiState,
    run: ManagedRun,
) -> Result<ManagedRunEnvelope> {
    let run_id = run.id.clone();
    let summary = load_managed_run_derived_summary(managed.store.as_ref(), &run_id).await?;
    Ok(ManagedRunEnvelope {
        run: apply_run_snapshot(run, managed.runs.snapshot(&run_id)),
        summary: summary.clone(),
        mcp_admission_rejection: summary.mcp_admission_rejection,
        cleanup_failure: summary.cleanup_failure,
        ownership: summary.ownership,
        takeover_assessment: summary.takeover_assessment,
        takeover: summary.takeover,
        recovery_decision: summary.recovery_decision,
        recovery_hint: summary.recovery_hint,
    })
}

async fn fail_managed_run_before_start(
    store: &ManagedStore,
    run_id: &str,
    error_text: &str,
    pre_terminal_event: Option<ManagedRunEventDraft>,
) {
    if let Some(event) = pre_terminal_event {
        append_run_event(store, run_id, event).await;
    }
    let _ = store
        .record_run_terminal_intent(run_id, ManagedRunStatus::Failed, Some(error_text))
        .await;
    let _ = store
        .update_run_status(run_id, ManagedRunStatus::Failed, Some(error_text))
        .await;
    append_terminal_run_event(
        store,
        run_id,
        &ManagedRunStatus::Failed,
        Some(error_text.to_string()),
    )
    .await;
}

async fn run_is_terminal(store: &ManagedStore, runs: &RunRegistry, run_id: &str) -> bool {
    if let Some(snapshot) = runs.snapshot(run_id) {
        return snapshot.status.is_terminal();
    }

    match store.get_run(run_id).await {
        Ok(Some(run)) => run.status.is_terminal(),
        Ok(None) | Err(_) => false,
    }
}

fn managed_response_status_message(response: &Response) -> String {
    let status = response.status();
    match status.canonical_reason() {
        Some(reason) => format!("{} {}", status.as_u16(), reason),
        None => status.as_u16().to_string(),
    }
}

async fn managed_run_replay_depth(store: &ManagedStore, run: &ManagedRun) -> Result<u32> {
    let mut depth = 0u32;
    let mut parent_run_id = run.replay_of_run_id.clone();

    while let Some(run_id) = parent_run_id {
        depth = depth.saturating_add(1);
        parent_run_id = store
            .get_run(&run_id)
            .await?
            .and_then(|parent| parent.replay_of_run_id);
    }

    Ok(depth)
}

async fn managed_run_root_run_id(store: &ManagedStore, run: &ManagedRun) -> Result<String> {
    let mut root_run_id = run.id.clone();
    let mut parent_run_id = run.replay_of_run_id.clone();

    while let Some(run_id) = parent_run_id {
        root_run_id = run_id.clone();
        parent_run_id = store
            .get_run(&run_id)
            .await?
            .and_then(|parent| parent.replay_of_run_id);
    }

    Ok(root_run_id)
}

#[derive(Clone, Copy)]
enum ManagedReplayTrigger {
    ManualReplay,
    InterruptedAutoReplay,
}

impl ManagedReplayTrigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::ManualReplay => "manual_replay",
            Self::InterruptedAutoReplay => "interrupted_auto_replay",
        }
    }
}

fn managed_continuation_boundary_str(
    boundary: hermes_managed::ManagedRunContinuationBoundaryKind,
) -> &'static str {
    match boundary {
        hermes_managed::ManagedRunContinuationBoundaryKind::UserCheckpointed => "user_checkpointed",
        hermes_managed::ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed => {
            "assistant_response_checkpointed"
        }
        hermes_managed::ManagedRunContinuationBoundaryKind::PendingToolCalls => {
            "pending_tool_calls"
        }
        hermes_managed::ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed => {
            "tool_results_checkpointed"
        }
    }
}

fn managed_recovery_decision_reason_str(reason: ManagedRunRecoveryDecisionReason) -> &'static str {
    match reason {
        ManagedRunRecoveryDecisionReason::RunStillActive => "run_still_active",
        ManagedRunRecoveryDecisionReason::ReplayChildActive => "replay_child_active",
        ManagedRunRecoveryDecisionReason::DepthLimitReached => "depth_limit_reached",
        ManagedRunRecoveryDecisionReason::ProcessHandoffRisk => "process_handoff_risk",
        ManagedRunRecoveryDecisionReason::BrowserHandoffRisk => "browser_handoff_risk",
        ManagedRunRecoveryDecisionReason::BrowserSessionState => "browser_session_state",
        ManagedRunRecoveryDecisionReason::McpHandoffRisk => "mcp_handoff_risk",
        ManagedRunRecoveryDecisionReason::McpRuntimeState => "mcp_runtime_state",
        ManagedRunRecoveryDecisionReason::ReplaySpawnFailed => "replay_spawn_failed",
    }
}

fn managed_recovery_decision_kind_str(kind: ManagedRunRecoveryDecisionKind) -> &'static str {
    match kind {
        ManagedRunRecoveryDecisionKind::ReplayStarted => "replay_started",
        ManagedRunRecoveryDecisionKind::FollowReplay => "follow_replay",
        ManagedRunRecoveryDecisionKind::ManualReview => "manual_review",
        ManagedRunRecoveryDecisionKind::Blocked => "blocked",
        ManagedRunRecoveryDecisionKind::Failed => "failed",
    }
}

fn managed_continuation_action_str(
    action: hermes_managed::ManagedRunContinuationAction,
) -> &'static str {
    match action {
        hermes_managed::ManagedRunContinuationAction::CallProvider => "call_provider",
        hermes_managed::ManagedRunContinuationAction::ExecutePendingTools => {
            "execute_pending_tools"
        }
        hermes_managed::ManagedRunContinuationAction::CompleteTurn => "complete_turn",
    }
}

fn managed_recovery_decision_event(
    decision: &ManagedRunRecoveryDecisionSummary,
) -> ManagedRunEventDraft {
    ManagedRunEventDraft {
        kind: ManagedRunEventKind::RunRecoveryDecision,
        message: Some(decision.message()),
        tool_name: None,
        tool_call_id: None,
        metadata: Some(serde_json::json!({
            "decision": managed_recovery_decision_kind_str(decision.decision),
            "reason": decision.reason.map(managed_recovery_decision_reason_str),
            "replay_run_id": decision.replay_run_id.as_deref(),
            "takeover_lineage_id": decision.takeover_lineage_id.as_deref(),
            "evaluated_by_worker_id": decision.evaluated_by_worker_id.as_deref(),
            "takeover_worker_id": decision.takeover_worker_id.as_deref(),
            "worker_id": decision.worker_id.as_deref(),
            "active_follow_target_run_id": decision.active_follow_target_run_id.as_deref(),
            "active_follow_target_status": decision.active_follow_target_status.as_ref().map(|status| status.as_str()),
            "active_follow_target_lineage_depth": decision.active_follow_target_lineage_depth,
            "source_boundary": decision.source_boundary.map(managed_continuation_boundary_str),
            "note": decision.note.as_deref(),
        })),
    }
}

fn managed_takeover_assessment_event(
    assessment: &ManagedRunTakeoverAssessmentSummary,
) -> ManagedRunEventDraft {
    ManagedRunEventDraft {
        kind: ManagedRunEventKind::RunTakeoverAssessed,
        message: Some(assessment.message()),
        tool_name: None,
        tool_call_id: None,
        metadata: Some(serde_json::json!({
            "takeover_lineage_id": assessment.takeover_lineage_id.as_deref(),
            "evaluated_by_worker_id": assessment.evaluated_by_worker_id.as_deref(),
            "source_boundary": assessment.source_boundary.map(managed_continuation_boundary_str),
            "interruption_cause": assessment.interruption_cause.map(|cause| match cause {
                ManagedRunInterruptionCause::LeaseExpired => "lease_expired",
                ManagedRunInterruptionCause::OwnershipNotEstablished => "ownership_not_established",
            }),
            "provider_call_in_flight": assessment.provider_call_in_flight,
            "process_handoff_risk": assessment.process_handoff_risk,
            "browser_handoff_risk": assessment.browser_handoff_risk,
            "browser_session_state": assessment.browser_session_state,
            "mcp_handoff_risk": assessment.mcp_handoff_risk,
            "mcp_runtime_state": assessment.mcp_runtime_state,
            "replay_depth": assessment.replay_depth,
            "max_auto_replays": assessment.max_auto_replays,
            "note": assessment.note.as_deref(),
        })),
    }
}

fn managed_takeover_established_event(
    continuation: &ManagedRunContinuationSummary,
) -> ManagedRunEventDraft {
    ManagedRunEventDraft {
        kind: ManagedRunEventKind::RunTakeoverEstablished,
        message: Some(continuation.message()),
        tool_name: None,
        tool_call_id: None,
        metadata: Some(serde_json::json!({
            "source_run_id": continuation.source_run_id.as_str(),
            "root_run_id": continuation.root_run_id.as_str(),
            "replay_depth": continuation.replay_depth,
            "takeover_lineage_id": continuation.takeover_lineage_id.as_deref(),
            "trigger": match continuation.trigger {
                hermes_managed::ManagedRunReplayTrigger::ManualReplay => "manual_replay",
                hermes_managed::ManagedRunReplayTrigger::InterruptedAutoReplay => "interrupted_auto_replay",
            },
            "source_status": continuation.source_status.as_ref().map(|status| status.as_str()),
            "source_interruption_cause": continuation.source_interruption_cause.map(|cause| match cause {
                ManagedRunInterruptionCause::LeaseExpired => "lease_expired",
                ManagedRunInterruptionCause::OwnershipNotEstablished => "ownership_not_established",
            }),
            "reused_session_id": continuation.reused_session_id,
            "resumed_existing_turn": continuation.resumed_existing_turn,
            "source_boundary": continuation.source_boundary.map(managed_continuation_boundary_str),
            "evaluated_by_worker_id": continuation.evaluated_by_worker_id.as_deref(),
            "takeover_worker_id": continuation.takeover_worker_id.as_deref(),
            "note": continuation.note.as_deref(),
        })),
    }
}

async fn append_managed_recovery_decision_if_changed(
    store: &ManagedStore,
    run_id: &str,
    decision: ManagedRunRecoveryDecisionSummary,
) -> Result<()> {
    let latest = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunRecoveryDecision)
        .await?;
    if latest
        .as_ref()
        .and_then(managed_run_recovery_decision_from_event)
        .as_ref()
        == Some(&decision)
    {
        return Ok(());
    }

    store
        .append_run_event(run_id, &managed_recovery_decision_event(&decision))
        .await
        .map(|_| ())
}

async fn append_managed_takeover_assessment_if_changed(
    store: &ManagedStore,
    run_id: &str,
    assessment: ManagedRunTakeoverAssessmentSummary,
) -> Result<()> {
    let latest = store
        .get_latest_run_event_by_kind(run_id, ManagedRunEventKind::RunTakeoverAssessed)
        .await?;
    if latest
        .as_ref()
        .and_then(managed_run_takeover_assessment_from_event)
        .as_ref()
        == Some(&assessment)
    {
        return Ok(());
    }

    store
        .append_run_event(run_id, &managed_takeover_assessment_event(&assessment))
        .await
        .map(|_| ())
}

#[derive(Clone)]
struct ManagedReplayProvenanceDraft {
    source_run_id: String,
    root_run_id: String,
    replay_depth: u32,
    trigger: ManagedReplayTrigger,
    trigger_worker_id: String,
    takeover_lineage_id: Option<String>,
    source_status: ManagedRunStatus,
    source_interruption_cause: Option<ManagedRunInterruptionCause>,
    reused_session_id: bool,
    resumed_existing_turn: bool,
    source_boundary: Option<hermes_managed::ManagedRunContinuationBoundaryKind>,
    note: String,
}

async fn build_managed_replay_provenance(
    managed: &ManagedApiState,
    source_run: &ManagedRun,
    trigger: ManagedReplayTrigger,
    resumed_existing_turn: bool,
) -> Result<ManagedReplayProvenanceDraft> {
    let root_run_id = managed_run_root_run_id(managed.store.as_ref(), source_run).await?;
    let replay_depth = managed_run_replay_depth(managed.store.as_ref(), source_run)
        .await?
        .saturating_add(1);
    let source_summary =
        load_managed_run_derived_summary(managed.store.as_ref(), &source_run.id).await?;
    let note = match trigger {
        ManagedReplayTrigger::ManualReplay => {
            "manual replay created a new managed run from persisted source context".to_string()
        }
        ManagedReplayTrigger::InterruptedAutoReplay => {
            "ownership-loss recovery auto-replayed the interrupted run onto a new worker"
                .to_string()
        }
    };

    Ok(ManagedReplayProvenanceDraft {
        source_run_id: source_run.id.clone(),
        root_run_id,
        replay_depth,
        trigger,
        trigger_worker_id: managed.worker_id.clone(),
        takeover_lineage_id: source_summary
            .continuation
            .as_ref()
            .and_then(|summary| summary.takeover_lineage_id.clone())
            .or_else(|| {
                source_summary
                    .replay_provenance
                    .as_ref()
                    .and_then(|summary| summary.takeover_lineage_id.clone())
            }),
        source_status: source_run.status.clone(),
        source_interruption_cause: (source_run.status == ManagedRunStatus::Interrupted)
            .then(|| {
                source_summary
                    .interruption
                    .as_ref()
                    .map(|value| value.cause)
            })
            .flatten(),
        reused_session_id: source_run.session_id.is_some(),
        resumed_existing_turn,
        source_boundary: source_summary
            .continuation_checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.kind),
        note,
    })
}

fn effective_takeover_lineage_id(
    replay_provenance: Option<&ManagedReplayProvenanceDraft>,
    run_id: &str,
) -> String {
    replay_provenance
        .and_then(|provenance| provenance.takeover_lineage_id.clone())
        .unwrap_or_else(|| run_id.to_string())
}

async fn auto_replay_managed_run(
    managed: &ManagedApiState,
    source_run: &ManagedRun,
) -> std::result::Result<String, String> {
    let (agent, version) = load_managed_agent_version_for_run(managed, source_run)
        .await
        .map_err(|response| {
            format!(
                "failed to load managed replay source {}: {}",
                source_run.id,
                managed_response_status_message(&response)
            )
        })?;

    let replay_provenance = build_managed_replay_provenance(
        managed,
        source_run,
        ManagedReplayTrigger::InterruptedAutoReplay,
        source_run.session_id.is_some(),
    )
    .await
    .map_err(|e| {
        format!(
            "failed to build managed replay provenance for {}: {e}",
            source_run.id
        )
    })?;

    spawn_managed_run_with_version(
        managed,
        &agent,
        &version,
        source_run.prompt.clone(),
        source_run.session_id.clone(),
        Some(replay_provenance),
        source_run.session_id.is_some(),
    )
    .await
    .map(|(run_id, _, _, _, _)| run_id)
    .map_err(|response| {
        format!(
            "failed to auto-replay interrupted run {}: {}",
            source_run.id,
            managed_response_status_message(&response)
        )
    })
}

fn build_managed_takeover_assessment(
    derived_summary: &ManagedRunDerivedSummary,
    worker_id: &str,
    replay_depth: u32,
    max_auto_replays: u32,
) -> ManagedRunTakeoverAssessmentSummary {
    ManagedRunTakeoverAssessmentSummary {
        takeover_lineage_id: derived_summary
            .continuation
            .as_ref()
            .and_then(|summary| summary.takeover_lineage_id.clone())
            .or_else(|| {
                derived_summary
                    .replay_provenance
                    .as_ref()
                    .and_then(|summary| summary.takeover_lineage_id.clone())
            }),
        evaluated_by_worker_id: Some(worker_id.to_string()),
        source_boundary: derived_summary
            .continuation_checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.kind),
        interruption_cause: derived_summary
            .interruption
            .as_ref()
            .map(|value| value.cause),
        provider_call_in_flight: derived_summary.provider_call_fence.is_some(),
        process_handoff_risk: derived_summary.process_handoff.is_some(),
        browser_handoff_risk: derived_summary
            .browser_handoff
            .as_ref()
            .is_some_and(|handoff| {
                handoff.action != "close"
                    || !matches!(
                        handoff.replay_disposition,
                        hermes_managed::ManagedRunBrowserReplayDisposition::SafeToReplay
                    )
            }),
        browser_session_state: derived_summary
            .browser_session_checkpoint
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.session_open),
        mcp_handoff_risk: derived_summary.mcp_handoff.as_ref().is_some_and(|handoff| {
            handoff.requires_live_runtime
                || !matches!(
                    handoff.replay_disposition,
                    hermes_managed::ManagedRunMcpReplayDisposition::SafeToReplay
                )
        }),
        mcp_runtime_state: derived_summary
            .mcp_runtime_checkpoint
            .as_ref()
            .is_some_and(|checkpoint| checkpoint.live_runtime_required),
        replay_depth,
        max_auto_replays,
        note: derived_summary
            .recovery_hint
            .as_ref()
            .and_then(|hint| hint.note.clone()),
    }
}

pub(crate) async fn maybe_auto_replay_interrupted_runs(
    shared: Arc<SharedState>,
    app_config: AppConfig,
    store: Arc<ManagedStore>,
    runs: Arc<RunRegistry>,
    worker_id: String,
    limit: usize,
) -> Result<ManagedAutoReplaySummary> {
    if !app_config.managed.recovery.auto_replay_interrupted || limit == 0 {
        return Ok(ManagedAutoReplaySummary::default());
    }

    let max_auto_replays = app_config.managed.recovery.max_auto_replays_per_root_run;
    let managed = ManagedApiState {
        shared,
        app_config,
        store: Arc::clone(&store),
        runs,
        worker_id,
    };
    let candidates = store.list_interrupted_runs_pending_replay(limit).await?;
    let mut summary = ManagedAutoReplaySummary {
        candidates: candidates.len(),
        ..ManagedAutoReplaySummary::default()
    };

    for source_run in candidates {
        let derived_summary =
            load_managed_run_derived_summary(store.as_ref(), &source_run.id).await?;
        let takeover_lineage_id = derived_summary
            .continuation
            .as_ref()
            .and_then(|summary| summary.takeover_lineage_id.clone())
            .or_else(|| {
                derived_summary
                    .replay_provenance
                    .as_ref()
                    .and_then(|summary| summary.takeover_lineage_id.clone())
            });
        let replay_depth = managed_run_replay_depth(store.as_ref(), &source_run).await?;
        let assessment = build_managed_takeover_assessment(
            &derived_summary,
            &managed.worker_id,
            replay_depth,
            max_auto_replays,
        );
        let source_boundary = assessment.source_boundary;
        append_managed_takeover_assessment_if_changed(
            store.as_ref(),
            &source_run.id,
            assessment.clone(),
        )
        .await?;

        if assessment.process_handoff_risk {
            append_managed_recovery_decision_if_changed(
                store.as_ref(),
                &source_run.id,
                ManagedRunRecoveryDecisionSummary {
                    decision: ManagedRunRecoveryDecisionKind::ManualReview,
                    reason: Some(ManagedRunRecoveryDecisionReason::ProcessHandoffRisk),
                    replay_run_id: None,
                    takeover_lineage_id: takeover_lineage_id.clone(),
                    evaluated_by_worker_id: Some(managed.worker_id.clone()),
                    takeover_worker_id: None,
                    worker_id: Some(managed.worker_id.clone()),
                    active_follow_target_run_id: None,
                    active_follow_target_status: None,
                    active_follow_target_lineage_depth: None,
                    source_boundary,
                    note: derived_summary
                        .recovery_hint
                        .as_ref()
                        .and_then(|hint| hint.note.clone()),
                },
            )
            .await?;
            summary.skipped_handoff_risk += 1;
            continue;
        }
        if assessment.browser_handoff_risk {
            append_managed_recovery_decision_if_changed(
                store.as_ref(),
                &source_run.id,
                ManagedRunRecoveryDecisionSummary {
                    decision: ManagedRunRecoveryDecisionKind::ManualReview,
                    reason: Some(ManagedRunRecoveryDecisionReason::BrowserHandoffRisk),
                    replay_run_id: None,
                    takeover_lineage_id: takeover_lineage_id.clone(),
                    evaluated_by_worker_id: Some(managed.worker_id.clone()),
                    takeover_worker_id: None,
                    worker_id: Some(managed.worker_id.clone()),
                    active_follow_target_run_id: None,
                    active_follow_target_status: None,
                    active_follow_target_lineage_depth: None,
                    source_boundary,
                    note: derived_summary
                        .recovery_hint
                        .as_ref()
                        .and_then(|hint| hint.note.clone()),
                },
            )
            .await?;
            summary.skipped_browser_handoff_risk += 1;
            continue;
        }
        if assessment.browser_session_state {
            append_managed_recovery_decision_if_changed(
                store.as_ref(),
                &source_run.id,
                ManagedRunRecoveryDecisionSummary {
                    decision: ManagedRunRecoveryDecisionKind::ManualReview,
                    reason: Some(ManagedRunRecoveryDecisionReason::BrowserSessionState),
                    replay_run_id: None,
                    takeover_lineage_id: takeover_lineage_id.clone(),
                    evaluated_by_worker_id: Some(managed.worker_id.clone()),
                    takeover_worker_id: None,
                    worker_id: Some(managed.worker_id.clone()),
                    active_follow_target_run_id: None,
                    active_follow_target_status: None,
                    active_follow_target_lineage_depth: None,
                    source_boundary,
                    note: derived_summary
                        .recovery_hint
                        .as_ref()
                        .and_then(|hint| hint.note.clone()),
                },
            )
            .await?;
            summary.skipped_browser_session_state += 1;
            continue;
        }
        if assessment.mcp_handoff_risk {
            append_managed_recovery_decision_if_changed(
                store.as_ref(),
                &source_run.id,
                ManagedRunRecoveryDecisionSummary {
                    decision: ManagedRunRecoveryDecisionKind::ManualReview,
                    reason: Some(ManagedRunRecoveryDecisionReason::McpHandoffRisk),
                    replay_run_id: None,
                    takeover_lineage_id: takeover_lineage_id.clone(),
                    evaluated_by_worker_id: Some(managed.worker_id.clone()),
                    takeover_worker_id: None,
                    worker_id: Some(managed.worker_id.clone()),
                    active_follow_target_run_id: None,
                    active_follow_target_status: None,
                    active_follow_target_lineage_depth: None,
                    source_boundary,
                    note: derived_summary
                        .recovery_hint
                        .as_ref()
                        .and_then(|hint| hint.note.clone()),
                },
            )
            .await?;
            summary.skipped_mcp_handoff_risk += 1;
            continue;
        }
        if assessment.mcp_runtime_state {
            append_managed_recovery_decision_if_changed(
                store.as_ref(),
                &source_run.id,
                ManagedRunRecoveryDecisionSummary {
                    decision: ManagedRunRecoveryDecisionKind::ManualReview,
                    reason: Some(ManagedRunRecoveryDecisionReason::McpRuntimeState),
                    replay_run_id: None,
                    takeover_lineage_id: takeover_lineage_id.clone(),
                    evaluated_by_worker_id: Some(managed.worker_id.clone()),
                    takeover_worker_id: None,
                    worker_id: Some(managed.worker_id.clone()),
                    active_follow_target_run_id: None,
                    active_follow_target_status: None,
                    active_follow_target_lineage_depth: None,
                    source_boundary,
                    note: derived_summary
                        .recovery_hint
                        .as_ref()
                        .and_then(|hint| hint.note.clone()),
                },
            )
            .await?;
            summary.skipped_mcp_runtime_state += 1;
            continue;
        }
        if replay_depth >= max_auto_replays {
            append_managed_recovery_decision_if_changed(
                store.as_ref(),
                &source_run.id,
                ManagedRunRecoveryDecisionSummary {
                    decision: ManagedRunRecoveryDecisionKind::Blocked,
                    reason: Some(ManagedRunRecoveryDecisionReason::DepthLimitReached),
                    replay_run_id: None,
                    takeover_lineage_id: takeover_lineage_id.clone(),
                    evaluated_by_worker_id: Some(managed.worker_id.clone()),
                    takeover_worker_id: None,
                    worker_id: Some(managed.worker_id.clone()),
                    active_follow_target_run_id: None,
                    active_follow_target_status: None,
                    active_follow_target_lineage_depth: None,
                    source_boundary,
                    note: Some(format!(
                        "automatic replay skipped because replay depth {} reached configured limit {}; manual replay remains available",
                        replay_depth, max_auto_replays
                    )),
                },
            )
            .await?;
            summary.skipped_depth_limit += 1;
            continue;
        }

        match auto_replay_managed_run(&managed, &source_run).await {
            Ok(run_id) => summary.replayed_run_ids.push(run_id),
            Err(err) => {
                append_managed_recovery_decision_if_changed(
                    store.as_ref(),
                    &source_run.id,
                    ManagedRunRecoveryDecisionSummary {
                        decision: ManagedRunRecoveryDecisionKind::Failed,
                        reason: Some(ManagedRunRecoveryDecisionReason::ReplaySpawnFailed),
                        replay_run_id: None,
                        takeover_lineage_id: takeover_lineage_id.clone(),
                        evaluated_by_worker_id: Some(managed.worker_id.clone()),
                        takeover_worker_id: None,
                        worker_id: Some(managed.worker_id.clone()),
                        active_follow_target_run_id: None,
                        active_follow_target_status: None,
                        active_follow_target_lineage_depth: None,
                        source_boundary,
                        note: Some(err.clone()),
                    },
                )
                .await?;
                summary.failures.push(err);
            }
        }
    }

    Ok(summary)
}

fn managed_agent_name(model: &str) -> Option<&str> {
    model.strip_prefix("agent:").filter(|name| !name.is_empty())
}

async fn load_managed_agent(
    managed: &ManagedApiState,
    agent_id: &str,
) -> std::result::Result<ManagedAgent, Response> {
    match managed.store.get_agent(agent_id).await {
        Ok(Some(agent)) => Ok(agent),
        Ok(None) => Err(managed_error_response(
            StatusCode::NOT_FOUND,
            format!("managed agent not found: {agent_id}"),
        )),
        Err(e) => Err(managed_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to load managed agent: {e}"),
        )),
    }
}

async fn load_managed_latest_version(
    managed: &ManagedApiState,
    agent: &ManagedAgent,
) -> std::result::Result<Option<ManagedAgentVersion>, Response> {
    if agent.latest_version == 0 {
        return Ok(None);
    }

    match managed
        .store
        .get_agent_version(&agent.id, agent.latest_version)
        .await
    {
        Ok(Some(version)) => Ok(Some(version)),
        Ok(None) => Err(managed_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "managed agent latest version missing: {}@{}",
                agent.id, agent.latest_version
            ),
        )),
        Err(e) => Err(managed_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to load managed agent latest version: {e}"),
        )),
    }
}

async fn validate_managed_version_request(
    managed: &ManagedApiState,
    req: &CreateManagedAgentVersionRequest,
) -> std::result::Result<ResolvedManagedVersionDefaults, Response> {
    let resolved = resolve_managed_version_defaults(
        Some(req.model.as_str()),
        req.base_url.as_deref(),
        &managed.app_config,
    )
    .map_err(|e| managed_error_response(StatusCode::BAD_REQUEST, e.to_string()))?;

    if req.system_prompt.trim().is_empty() {
        return Err(managed_error_response(
            StatusCode::BAD_REQUEST,
            "managed agent version system_prompt is required",
        ));
    }
    if req.max_iterations.is_some_and(|value| value == 0) {
        return Err(managed_error_response(
            StatusCode::BAD_REQUEST,
            "managed agent version max_iterations must be greater than 0",
        ));
    }
    if req.timeout_secs.is_some_and(|value| value == 0) {
        return Err(managed_error_response(
            StatusCode::BAD_REQUEST,
            "managed agent version timeout_secs must be greater than 0",
        ));
    }

    let temperature = req.temperature.unwrap_or(0.0);
    if !temperature.is_finite() {
        return Err(managed_error_response(
            StatusCode::BAD_REQUEST,
            "managed agent version temperature must be finite",
        ));
    }

    validate_managed_beta_tools(&req.allowed_tools)
        .map_err(|e| managed_error_response(StatusCode::BAD_REQUEST, e.to_string()))?;

    preflight_managed_model(
        &managed.app_config,
        &resolved.model,
        resolved.base_url.as_deref(),
    )
    .await
    .map_err(|e| managed_error_response(StatusCode::BAD_REQUEST, e.to_string()))?;

    if req.allowed_skills.is_empty() {
        return Ok(resolved);
    }

    let Some(source_skills) = managed.shared.skills.as_ref() else {
        return Err(managed_error_response(
            StatusCode::BAD_REQUEST,
            "managed skill allowlist requires loaded skills",
        ));
    };

    let guard = source_skills.read().await;
    build_filtered_skill_manager(&guard, &req.allowed_skills)
        .map_err(|e| managed_error_response(StatusCode::BAD_REQUEST, e.to_string()))?;

    Ok(resolved)
}

async fn load_managed_agent_version(
    managed: &ManagedApiState,
    agent_name: &str,
) -> std::result::Result<(ManagedAgent, ManagedAgentVersion), Response> {
    let agent = match managed.store.get_agent_by_name(agent_name).await {
        Ok(Some(agent)) => agent,
        Ok(None) => {
            return Err(oai_error_response(
                StatusCode::BAD_REQUEST,
                format!("managed agent not found: {agent_name}"),
                "invalid_request_error",
            ));
        }
        Err(e) => {
            return Err(oai_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load managed agent: {e}"),
                "server_error",
            ));
        }
    };

    if agent.latest_version == 0 {
        return Err(oai_error_response(
            StatusCode::BAD_REQUEST,
            format!("managed agent has no versions: {agent_name}"),
            "invalid_request_error",
        ));
    }

    let version = match managed
        .store
        .get_agent_version(&agent.id, agent.latest_version)
        .await
    {
        Ok(Some(version)) => version,
        Ok(None) => {
            return Err(oai_error_response(
                StatusCode::BAD_REQUEST,
                format!(
                    "managed agent latest version missing: {}@{}",
                    agent_name, agent.latest_version
                ),
                "invalid_request_error",
            ));
        }
        Err(e) => {
            return Err(oai_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load managed agent version: {e}"),
                "server_error",
            ));
        }
    };

    Ok((agent, version))
}

async fn load_managed_agent_version_for_run(
    managed: &ManagedApiState,
    run: &hermes_managed::ManagedRun,
) -> std::result::Result<(ManagedAgent, ManagedAgentVersion), Response> {
    let agent = match managed.store.get_agent(&run.agent_id).await {
        Ok(Some(agent)) => agent,
        Ok(None) => {
            return Err(managed_error_response(
                StatusCode::NOT_FOUND,
                format!(
                    "managed agent not found for run {}: {}",
                    run.id, run.agent_id
                ),
            ));
        }
        Err(e) => {
            return Err(managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load managed agent for run {}: {e}", run.id),
            ));
        }
    };

    let version = match managed
        .store
        .get_agent_version(&run.agent_id, run.agent_version)
        .await
    {
        Ok(Some(version)) => version,
        Ok(None) => {
            return Err(managed_error_response(
                StatusCode::NOT_FOUND,
                format!(
                    "managed agent version not found for run {}: {}@{}",
                    run.id, run.agent_id, run.agent_version
                ),
            ));
        }
        Err(e) => {
            return Err(managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "failed to load managed agent version for run {}: {e}",
                    run.id
                ),
            ));
        }
    };

    Ok((agent, version))
}

async fn finalize_managed_run(
    store: Arc<ManagedStore>,
    runs: Arc<RunRegistry>,
    run_id: String,
    desired_status: ManagedRunStatus,
    last_error: Option<String>,
) {
    let was_terminal = run_is_terminal(store.as_ref(), runs.as_ref(), &run_id).await;
    let snapshot = runs
        .update_status(&run_id, desired_status.clone(), last_error.clone())
        .ok();
    if snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.status != desired_status)
    {
        return;
    }
    if was_terminal && snapshot.is_none() {
        return;
    }

    let status_to_store = snapshot
        .as_ref()
        .map(|snapshot| snapshot.status.clone())
        .unwrap_or_else(|| desired_status.clone());
    let error_to_store = snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.last_error.clone())
        .or(last_error);
    let owner = snapshot.as_ref().and_then(|snapshot| {
        Some((
            snapshot.owner_worker_id.as_deref()?,
            snapshot.owner_claim_token.as_deref()?,
        ))
    });
    let current_owner_worker_id = owner.map(|(worker_id, _)| worker_id);
    let mut ownership_release = None;

    if let Some((worker_id, claim_token)) = owner {
        let owner_snapshot = match store.get_run_owner_snapshot(&run_id).await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                tracing::warn!(
                    run_id,
                    "failed to load managed owner snapshot before finalize: {e}"
                );
                None
            }
        };
        if matches!(
            status_to_store,
            ManagedRunStatus::Completed | ManagedRunStatus::Failed
        ) {
            match store
                .record_run_terminal_intent_if_owned(
                    &run_id,
                    worker_id,
                    claim_token,
                    status_to_store.clone(),
                    error_to_store.as_deref(),
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(
                        run_id,
                        "managed run finalize skipped after ownership changed"
                    );
                    let _ = runs.remove(&run_id);
                    return;
                }
                Err(e) => tracing::warn!(run_id, "failed to record managed terminal intent: {e}"),
            }
        }

        match store
            .update_run_status_if_owned(
                &run_id,
                worker_id,
                claim_token,
                status_to_store.clone(),
                error_to_store.as_deref(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    run_id,
                    "managed run finalize skipped after ownership changed"
                );
                let _ = runs.remove(&run_id);
                return;
            }
            Err(e) => {
                tracing::warn!(run_id, "failed to update owned managed run status: {e}");
                return;
            }
        }
        ownership_release = owner_snapshot.map(|snapshot| ManagedRunOwnershipReleaseSummary {
            worker_id: snapshot.worker_id,
            reason: managed_ownership_release_reason_for_status(&status_to_store),
            owner_claimed_at: snapshot.claimed_at,
            owner_last_heartbeat_at: snapshot.last_heartbeat_at,
            owner_lease_expires_at: snapshot.lease_expires_at,
            note: Some(format!(
                "ownership ended when managed run became {}",
                status_to_store.as_str()
            )),
        });
    } else {
        if matches!(
            status_to_store,
            ManagedRunStatus::Completed | ManagedRunStatus::Failed
        ) {
            let _ = store
                .record_run_terminal_intent(
                    &run_id,
                    status_to_store.clone(),
                    error_to_store.as_deref(),
                )
                .await;
        }

        let _ = store
            .update_run_status(&run_id, status_to_store.clone(), error_to_store.as_deref())
            .await;
    }
    if !was_terminal {
        append_terminal_run_event(store.as_ref(), &run_id, &status_to_store, error_to_store).await;
        if let Some(summary) = ownership_release.as_ref() {
            append_run_event(
                store.as_ref(),
                &run_id,
                managed_ownership_release_event(summary),
            )
            .await;
        }
        match store.get_run(&run_id).await {
            Ok(Some(updated_run)) => {
                append_source_takeover_update_for_replay_child(
                    store.as_ref(),
                    &updated_run,
                    current_owner_worker_id,
                )
                .await;
                append_ancestor_follow_replay_decisions_for_replay_child(
                    store.as_ref(),
                    &updated_run,
                )
                .await;
            }
            Ok(None) => tracing::warn!(
                run_id,
                "managed run disappeared before source takeover update could be recorded"
            ),
            Err(e) => tracing::warn!(
                run_id,
                "failed to load managed run for source takeover update: {e}"
            ),
        }
    }
    log_managed_cleanup(
        store.as_ref(),
        &run_id,
        session_cleanup::cleanup_session(&run_id).await,
    )
    .await;
    let _ = runs.remove(&run_id);
}

async fn terminate_managed_run(
    store: Arc<ManagedStore>,
    runs: Arc<RunRegistry>,
    run_id: String,
    status: ManagedRunStatus,
    last_error: Option<String>,
) {
    let was_terminal = run_is_terminal(store.as_ref(), runs.as_ref(), &run_id).await;

    let snapshot = runs
        .terminate_run(&run_id, status.clone(), last_error.clone())
        .ok();
    if was_terminal && snapshot.is_none() {
        return;
    }
    let (status_to_store, error_to_store) = match snapshot {
        Some(snapshot) => (snapshot.status, snapshot.last_error),
        None => (status, last_error),
    };
    let owner = runs
        .snapshot(&run_id)
        .and_then(|snapshot| Some((snapshot.owner_worker_id?, snapshot.owner_claim_token?)));
    let current_owner_worker_id = owner.as_ref().map(|(worker_id, _)| worker_id.clone());
    let mut ownership_release = None;

    if let Some((worker_id, claim_token)) = owner {
        let owner_snapshot = match store.get_run_owner_snapshot(&run_id).await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                tracing::warn!(
                    run_id,
                    "failed to load managed owner snapshot before termination: {e}"
                );
                None
            }
        };
        if matches!(
            status_to_store,
            ManagedRunStatus::Cancelled | ManagedRunStatus::Failed | ManagedRunStatus::TimedOut
        ) {
            match store
                .record_run_terminal_intent_if_owned(
                    &run_id,
                    &worker_id,
                    &claim_token,
                    status_to_store.clone(),
                    error_to_store.as_deref(),
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(
                        run_id,
                        "managed run terminate skipped after ownership changed"
                    );
                    let _ = runs.remove(&run_id);
                    return;
                }
                Err(e) => tracing::warn!(run_id, "failed to record managed terminal intent: {e}"),
            }
        }

        match store
            .update_run_status_if_owned(
                &run_id,
                &worker_id,
                &claim_token,
                status_to_store.clone(),
                error_to_store.as_deref(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(
                    run_id,
                    "managed run terminate skipped after ownership changed"
                );
                let _ = runs.remove(&run_id);
                return;
            }
            Err(e) => {
                tracing::warn!(run_id, "failed to update owned managed run status: {e}");
                return;
            }
        }
        ownership_release = owner_snapshot.map(|snapshot| ManagedRunOwnershipReleaseSummary {
            worker_id: snapshot.worker_id,
            reason: managed_ownership_release_reason_for_status(&status_to_store),
            owner_claimed_at: snapshot.claimed_at,
            owner_last_heartbeat_at: snapshot.last_heartbeat_at,
            owner_lease_expires_at: snapshot.lease_expires_at,
            note: Some(format!(
                "ownership ended when managed run became {}",
                status_to_store.as_str()
            )),
        });
    } else {
        if matches!(
            status_to_store,
            ManagedRunStatus::Cancelled | ManagedRunStatus::Failed | ManagedRunStatus::TimedOut
        ) {
            let _ = store
                .record_run_terminal_intent(
                    &run_id,
                    status_to_store.clone(),
                    error_to_store.as_deref(),
                )
                .await;
        }

        let _ = store
            .update_run_status(&run_id, status_to_store.clone(), error_to_store.as_deref())
            .await;
    }
    if !was_terminal {
        append_terminal_run_event(store.as_ref(), &run_id, &status_to_store, error_to_store).await;
        if let Some(summary) = ownership_release.as_ref() {
            append_run_event(
                store.as_ref(),
                &run_id,
                managed_ownership_release_event(summary),
            )
            .await;
        }
        match store.get_run(&run_id).await {
            Ok(Some(updated_run)) => {
                append_source_takeover_update_for_replay_child(
                    store.as_ref(),
                    &updated_run,
                    current_owner_worker_id.as_deref(),
                )
                .await;
                append_ancestor_follow_replay_decisions_for_replay_child(
                    store.as_ref(),
                    &updated_run,
                )
                .await;
            }
            Ok(None) => tracing::warn!(
                run_id,
                "managed run disappeared before source takeover update could be recorded"
            ),
            Err(e) => tracing::warn!(
                run_id,
                "failed to load managed run for source takeover update: {e}"
            ),
        }
    }
    log_managed_cleanup(
        store.as_ref(),
        &run_id,
        session_cleanup::cleanup_session(&run_id).await,
    )
    .await;
    let _ = runs.remove(&run_id);
}

fn managed_cleanup_failure_event(
    phase: &str,
    summary: &session_cleanup::SessionCleanupSummary,
) -> ManagedRunEventDraft {
    ManagedRunEventDraft {
        kind: ManagedRunEventKind::RunCleanupFailed,
        message: Some(format!(
            "Managed run cleanup failed for {} resource(s)",
            summary.failures.len()
        )),
        tool_name: None,
        tool_call_id: None,
        metadata: Some(serde_json::json!({
            "phase": phase,
            "attempted": summary.attempted,
            "cleaned": summary.cleaned,
            "failures": summary.failures,
        })),
    }
}

fn managed_ownership_release_reason_str(reason: ManagedRunOwnershipReleaseReason) -> &'static str {
    match reason {
        ManagedRunOwnershipReleaseReason::Completed => "completed",
        ManagedRunOwnershipReleaseReason::Failed => "failed",
        ManagedRunOwnershipReleaseReason::Cancelled => "cancelled",
        ManagedRunOwnershipReleaseReason::TimedOut => "timed_out",
        ManagedRunOwnershipReleaseReason::Interrupted => "interrupted",
    }
}

fn managed_ownership_release_reason_for_status(
    status: &ManagedRunStatus,
) -> ManagedRunOwnershipReleaseReason {
    match status {
        ManagedRunStatus::Completed => ManagedRunOwnershipReleaseReason::Completed,
        ManagedRunStatus::Failed => ManagedRunOwnershipReleaseReason::Failed,
        ManagedRunStatus::Cancelled => ManagedRunOwnershipReleaseReason::Cancelled,
        ManagedRunStatus::TimedOut => ManagedRunOwnershipReleaseReason::TimedOut,
        ManagedRunStatus::Interrupted => ManagedRunOwnershipReleaseReason::Interrupted,
        _ => ManagedRunOwnershipReleaseReason::Interrupted,
    }
}

fn managed_ownership_release_event(
    summary: &ManagedRunOwnershipReleaseSummary,
) -> ManagedRunEventDraft {
    ManagedRunEventDraft {
        kind: ManagedRunEventKind::RunOwnershipReleased,
        message: Some(summary.message()),
        tool_name: None,
        tool_call_id: None,
        metadata: Some(serde_json::json!({
            "worker_id": summary.worker_id.as_str(),
            "reason": managed_ownership_release_reason_str(summary.reason),
            "owner_claimed_at": summary.owner_claimed_at.map(|value: chrono::DateTime<chrono::Utc>| value.to_rfc3339()),
            "owner_last_heartbeat_at": summary.owner_last_heartbeat_at.map(|value: chrono::DateTime<chrono::Utc>| value.to_rfc3339()),
            "owner_lease_expires_at": summary.owner_lease_expires_at.map(|value: chrono::DateTime<chrono::Utc>| value.to_rfc3339()),
            "note": summary.note.as_deref(),
        })),
    }
}

async fn log_managed_cleanup(
    store: &ManagedStore,
    run_id: &str,
    summary: session_cleanup::SessionCleanupSummary,
) {
    if summary.attempted == 0 {
        return;
    }

    if summary.failures.is_empty() {
        tracing::info!(
            run_id,
            attempted = summary.attempted,
            cleaned = summary.cleaned,
            "cleaned up session-scoped resources for managed run"
        );
    } else {
        tracing::warn!(
            run_id,
            attempted = summary.attempted,
            cleaned = summary.cleaned,
            failures = ?summary.failures,
            "managed run cleanup completed with failures"
        );
        append_run_event(
            store,
            run_id,
            managed_cleanup_failure_event("terminal_cleanup", &summary),
        )
        .await;
    }
}

async fn spawn_managed_run(
    managed: &ManagedApiState,
    agent_name: &str,
    prompt: String,
    requested_session_id: Option<String>,
) -> std::result::Result<
    (
        String,
        Option<String>,
        u64,
        broadcast::Receiver<StreamDelta>,
        oneshot::Receiver<ManagedRunOutcome>,
    ),
    Response,
> {
    let (agent, version) = load_managed_agent_version(managed, agent_name).await?;
    spawn_managed_run_with_version(
        managed,
        &agent,
        &version,
        prompt,
        requested_session_id,
        None,
        false,
    )
    .await
}

async fn spawn_managed_run_with_version(
    managed: &ManagedApiState,
    agent: &ManagedAgent,
    version: &ManagedAgentVersion,
    prompt: String,
    requested_session_id: Option<String>,
    replay_provenance: Option<ManagedReplayProvenanceDraft>,
    resume_existing_turn: bool,
) -> std::result::Result<
    (
        String,
        Option<String>,
        u64,
        broadcast::Receiver<StreamDelta>,
        oneshot::Receiver<ManagedRunOutcome>,
    ),
    Response,
> {
    let working_dir = std::env::current_dir().map_err(|e| {
        oai_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to resolve working directory: {e}"),
            "server_error",
        )
    })?;

    let (managed_session_id, initial_history, session_store) =
        match managed.shared.session_store.clone() {
            Some(session_store) => {
                let managed_session_id =
                    requested_session_id.unwrap_or_else(new_managed_session_id);
                ensure_managed_session(
                    session_store.as_ref(),
                    &managed_session_id,
                    version,
                    &working_dir,
                )
                .await
                .map_err(|e| {
                    oai_error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("failed to initialize managed session: {e}"),
                        "server_error",
                    )
                })?;
                let initial_history = session_store
                    .load_history(&managed_session_id)
                    .await
                    .map_err(|e| {
                        oai_error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("failed to load managed session history: {e}"),
                            "server_error",
                        )
                    })?;
                (
                    Some(managed_session_id),
                    initial_history,
                    Some(session_store),
                )
            }
            None => {
                if requested_session_id.is_some() {
                    return Err(oai_error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "managed session persistence is unavailable",
                        "server_error",
                    ));
                }
                (None, Vec::new(), None)
            }
        };

    let mut run = ManagedRun::new(&agent.id, version.version, &version.model);
    run.status = ManagedRunStatus::Pending;
    run.updated_at = chrono::Utc::now();
    run.session_id = managed_session_id.clone();
    run.prompt = prompt.clone();
    run.replay_of_run_id = replay_provenance
        .as_ref()
        .map(|provenance| provenance.source_run_id.clone());
    let takeover_lineage_id =
        effective_takeover_lineage_id(replay_provenance.as_ref(), run.id.as_str());
    managed.store.create_run(&run).await.map_err(|e| {
        oai_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to persist managed run: {e}"),
            "server_error",
        )
    })?;
    append_run_event(
        managed.store.as_ref(),
        &run.id,
        ManagedRunEventDraft {
            kind: ManagedRunEventKind::RunCreated,
            message: Some(match &replay_provenance {
                Some(provenance) => format!(
                    "managed run replayed from {} for {}@{}",
                    provenance.source_run_id, agent.name, version.version
                ),
                None => format!("managed run created for {}@{}", agent.name, version.version),
            }),
            tool_name: None,
            tool_call_id: None,
            metadata: Some(serde_json::json!({
                "prompt_chars": run.prompt.chars().count(),
                "session_id": run.session_id,
                "replay_of_run_id": replay_provenance.as_ref().map(|provenance| provenance.source_run_id.as_str()),
                "replay_root_run_id": replay_provenance.as_ref().map(|provenance| provenance.root_run_id.as_str()),
                "replay_depth": replay_provenance.as_ref().map(|provenance| provenance.replay_depth),
                "replay_trigger": replay_provenance.as_ref().map(|provenance| provenance.trigger.as_str()),
                "replay_trigger_worker_id": replay_provenance.as_ref().map(|provenance| provenance.trigger_worker_id.as_str()),
                "takeover_lineage_id": replay_provenance.as_ref().map(|_| takeover_lineage_id.as_str()),
                "replay_source_status": replay_provenance.as_ref().map(|provenance| provenance.source_status.as_str()),
                "replay_source_interruption_cause": replay_provenance.as_ref().and_then(|provenance| provenance.source_interruption_cause).map(|cause| match cause {
                    ManagedRunInterruptionCause::LeaseExpired => "lease_expired",
                    ManagedRunInterruptionCause::OwnershipNotEstablished => "ownership_not_established",
                }),
                "reused_session_id": replay_provenance.as_ref().map(|provenance| provenance.reused_session_id),
                "resumed_existing_turn": replay_provenance.as_ref().map(|provenance| provenance.resumed_existing_turn),
                "replay_source_boundary": replay_provenance.as_ref().and_then(|provenance| provenance.source_boundary).map(|boundary| match boundary {
                    hermes_managed::ManagedRunContinuationBoundaryKind::UserCheckpointed => "user_checkpointed",
                    hermes_managed::ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed => "assistant_response_checkpointed",
                    hermes_managed::ManagedRunContinuationBoundaryKind::PendingToolCalls => "pending_tool_calls",
                    hermes_managed::ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed => "tool_results_checkpointed",
                }),
                "replay_note": replay_provenance.as_ref().map(|provenance| provenance.note.as_str()),
            })),
        },
    )
    .await;
    let checkpoint_state = Arc::new(ManagedRunCheckpointState::default());
    let mcp_transport_by_server = managed_mcp_transport_by_server(&managed.app_config);
    let checkpoint_observer = match (&session_store, &managed_session_id) {
        (Some(session_store), Some(session_id)) => {
            Some(Arc::new(ManagedSessionCheckpointObserver::new(
                Arc::clone(&managed.store),
                run.id.clone(),
                Arc::clone(session_store),
                session_id.clone(),
                initial_history.len(),
                Arc::clone(&checkpoint_state),
            )) as Arc<dyn ConversationCheckpointObserver>)
        }
        _ => None,
    };
    let use_incremental_session_checkpointing = checkpoint_observer.is_some();

    let mut runtime_context = ManagedRuntimeBuildContext::new(run.clone(), working_dir);
    runtime_context.checkpoint_observer = checkpoint_observer;
    runtime_context.execution_observer = Some(Arc::new(ManagedToolCheckpointObserver {
        store: Arc::clone(&managed.store),
        run_id: run.id.clone(),
        checkpoint_state,
        mcp_transport_by_server,
    }) as Arc<dyn ToolExecutionObserver>);
    let runtime = match build_managed_runtime(
        agent,
        version,
        managed.shared.registry.as_ref(),
        managed.shared.skills.as_ref(),
        &managed.app_config,
        runtime_context,
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(e) => {
            let error_text = format!("failed to build managed runtime: {e}");
            let rejection = e.mcp_admission_rejection().cloned();
            let rejection_event = rejection
                .as_ref()
                .map(managed_mcp_admission_rejection_event);
            fail_managed_run_before_start(
                managed.store.as_ref(),
                &run.id,
                &error_text,
                rejection_event,
            )
            .await;
            return Err(oai_error_response_with_code(
                StatusCode::INTERNAL_SERVER_ERROR,
                error_text,
                "server_error",
                rejection.as_ref().map(|rejection| rejection.code.as_str()),
            ));
        }
    };

    let timeout_secs = u64::from(runtime.timeout_secs.max(1));
    let claim_token = new_managed_run_claim_token();
    let claimed_at = chrono::Utc::now();
    let lease_expires_at = managed_run_lease_expires_at(claimed_at);
    let claimed = managed
        .store
        .claim_run_ownership(
            &run.id,
            &managed.worker_id,
            &claim_token,
            claimed_at,
            lease_expires_at,
        )
        .await
        .map_err(|e| {
            oai_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to claim managed run ownership: {e}"),
                "server_error",
            )
        })?;
    if !claimed {
        return Err(oai_error_response(
            StatusCode::CONFLICT,
            "managed run ownership changed before execution could start",
            "conflict_error",
        ));
    }
    append_run_event(
        managed.store.as_ref(),
        &run.id,
        ManagedRunEventDraft {
            kind: ManagedRunEventKind::RunOwnershipClaimed,
            message: Some(format!(
                "managed run ownership claimed by worker {}",
                managed.worker_id
            )),
            tool_name: None,
            tool_call_id: None,
            metadata: Some(serde_json::json!({
                "worker_id": managed.worker_id.as_str(),
                "claimed_at": claimed_at.to_rfc3339(),
                "lease_expires_at": lease_expires_at.to_rfc3339(),
                "takeover_lineage_id": run.replay_of_run_id.as_ref().map(|_| takeover_lineage_id.as_str()),
            })),
        },
    )
    .await;

    run.status = ManagedRunStatus::Running;
    run.updated_at = claimed_at;
    if let Some(provenance) = &replay_provenance {
        append_run_event(
            managed.store.as_ref(),
            &run.id,
            managed_takeover_established_event(&ManagedRunContinuationSummary {
                source_run_id: provenance.source_run_id.clone(),
                root_run_id: provenance.root_run_id.clone(),
                replay_depth: provenance.replay_depth,
                trigger: match provenance.trigger {
                    ManagedReplayTrigger::ManualReplay => {
                        hermes_managed::ManagedRunReplayTrigger::ManualReplay
                    }
                    ManagedReplayTrigger::InterruptedAutoReplay => {
                        hermes_managed::ManagedRunReplayTrigger::InterruptedAutoReplay
                    }
                },
                takeover_lineage_id: Some(takeover_lineage_id.clone()),
                source_status: Some(provenance.source_status.clone()),
                source_interruption_cause: provenance.source_interruption_cause,
                reused_session_id: provenance.reused_session_id,
                resumed_existing_turn: provenance.resumed_existing_turn,
                source_boundary: provenance.source_boundary,
                evaluated_by_worker_id: Some(provenance.trigger_worker_id.clone()),
                takeover_worker_id: Some(provenance.trigger_worker_id.clone()),
                note: Some(format!(
                    "replay child {} took over continuation of {}",
                    run.id, provenance.source_run_id
                )),
            }),
        )
        .await;
        append_run_event(
            managed.store.as_ref(),
            &provenance.source_run_id,
            ManagedRunEventDraft {
                kind: ManagedRunEventKind::RunReplayed,
                message: Some(format!(
                    "managed run continued as replay child {} via {}",
                    run.id,
                    provenance.trigger.as_str()
                )),
                tool_name: None,
                tool_call_id: None,
                metadata: Some(serde_json::json!({
                    "replay_run_id": run.id.as_str(),
                    "replay_run_status": run.status.as_str(),
                    "takeover_lineage_id": takeover_lineage_id.as_str(),
                    "replay_root_run_id": provenance.root_run_id.as_str(),
                    "replay_depth": provenance.replay_depth,
                    "replay_trigger": provenance.trigger.as_str(),
                    "replay_trigger_worker_id": provenance.trigger_worker_id.as_str(),
                    "replay_source_status": provenance.source_status.as_str(),
                    "replay_source_interruption_cause": provenance.source_interruption_cause.map(|cause| match cause {
                        ManagedRunInterruptionCause::LeaseExpired => "lease_expired",
                        ManagedRunInterruptionCause::OwnershipNotEstablished => "ownership_not_established",
                    }),
                    "reused_session_id": provenance.reused_session_id,
                    "resumed_existing_turn": provenance.resumed_existing_turn,
                    "replay_source_boundary": provenance.source_boundary.map(|boundary| match boundary {
                        hermes_managed::ManagedRunContinuationBoundaryKind::UserCheckpointed => "user_checkpointed",
                        hermes_managed::ManagedRunContinuationBoundaryKind::AssistantResponseCheckpointed => "assistant_response_checkpointed",
                        hermes_managed::ManagedRunContinuationBoundaryKind::PendingToolCalls => "pending_tool_calls",
                        hermes_managed::ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed => "tool_results_checkpointed",
                    }),
                    "replay_note": provenance.note.as_str(),
                })),
            },
        )
        .await;
        append_run_event(
            managed.store.as_ref(),
            &provenance.source_run_id,
            managed_recovery_decision_event(&ManagedRunRecoveryDecisionSummary {
                decision: ManagedRunRecoveryDecisionKind::FollowReplay,
                reason: Some(ManagedRunRecoveryDecisionReason::ReplayChildActive),
                replay_run_id: Some(run.id.clone()),
                takeover_lineage_id: Some(takeover_lineage_id.clone()),
                evaluated_by_worker_id: Some(provenance.trigger_worker_id.clone()),
                takeover_worker_id: Some(provenance.trigger_worker_id.clone()),
                worker_id: Some(provenance.trigger_worker_id.clone()),
                active_follow_target_run_id: None,
                active_follow_target_status: None,
                active_follow_target_lineage_depth: None,
                source_boundary: provenance.source_boundary,
                note: Some(format!(
                    "continuation is now owned by replay child {} created via {}",
                    run.id,
                    provenance.trigger.as_str()
                )),
            }),
        )
        .await;
    }
    append_source_takeover_update_for_replay_child(
        managed.store.as_ref(),
        &run,
        Some(managed.worker_id.as_str()),
    )
    .await;
    append_ancestor_follow_replay_decisions_for_replay_child(managed.store.as_ref(), &run).await;
    let run_id = run.id.clone();
    let (delta_tx, mut delta_rx) = mpsc::channel::<StreamDelta>(64);
    let (broadcast_tx, broadcast_rx) = broadcast::channel::<StreamDelta>(64);
    let (outcome_tx, outcome_rx) = oneshot::channel::<ManagedRunOutcome>();
    let store = Arc::clone(&managed.store);
    let runs = Arc::clone(&managed.runs);
    let run_id_for_task = run_id.clone();
    let prompt_for_task = prompt;
    let session_id_for_task = managed_session_id.clone();
    let session_store_for_task = session_store.clone();
    let mut agent_runner = runtime.agent;
    let resume_existing_turn_for_task = resume_existing_turn && !initial_history.is_empty();
    let use_incremental_session_checkpointing_for_task = use_incremental_session_checkpointing;
    let heartbeat_store = Arc::clone(&managed.store);
    let heartbeat_run_id = run_id.clone();
    let heartbeat_worker_id = managed.worker_id.clone();
    let heartbeat_claim_token = claim_token.clone();

    let task = tokio::spawn(async move {
        let relay_store = Arc::clone(&store);
        let relay_run_id = run_id_for_task.clone();
        let relay_handle = tokio::spawn(async move {
            while let Some(delta) = delta_rx.recv().await {
                if let Some(event) = run_event_from_delta(&delta) {
                    append_run_event(relay_store.as_ref(), &relay_run_id, event).await;
                }
                let _ = broadcast_tx.send(delta);
            }
        });

        let mut history = initial_history;
        let pre_len = history.len();
        let result = if resume_existing_turn_for_task {
            agent_runner
                .continue_conversation(&mut history, delta_tx)
                .await
        } else {
            agent_runner
                .run_conversation(&prompt_for_task, &mut history, delta_tx)
                .await
        };
        let _ = relay_handle.await;

        match result {
            Ok(text) => {
                if !use_incremental_session_checkpointing_for_task {
                    if let (Some(session_store), Some(session_id)) =
                        (session_store_for_task, session_id_for_task.as_deref())
                    {
                        persist_managed_session_turn(
                            session_store.as_ref(),
                            session_id,
                            &history,
                            pre_len,
                        )
                        .await;
                    }
                }
                finalize_managed_run(
                    store,
                    runs,
                    run_id_for_task.clone(),
                    ManagedRunStatus::Completed,
                    None,
                )
                .await;
                let _ = outcome_tx.send(ManagedRunOutcome::Completed(text));
            }
            Err(e) => {
                let error_text = e.to_string();
                finalize_managed_run(
                    store,
                    runs,
                    run_id_for_task.clone(),
                    ManagedRunStatus::Failed,
                    Some(error_text.clone()),
                )
                .await;
                let _ = outcome_tx.send(ManagedRunOutcome::Failed(error_text));
            }
        }
    });

    let heartbeat_task = tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(Duration::from_secs(MANAGED_RUN_HEARTBEAT_INTERVAL_SECS));
        interval.tick().await;
        loop {
            interval.tick().await;
            let heartbeat_at = chrono::Utc::now();
            match heartbeat_store
                .heartbeat_run_ownership(
                    &heartbeat_run_id,
                    &heartbeat_worker_id,
                    &heartbeat_claim_token,
                    heartbeat_at,
                    managed_run_lease_expires_at(heartbeat_at),
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    tracing::warn!(
                        run_id = heartbeat_run_id,
                        "managed run heartbeat failed: {e}"
                    );
                    break;
                }
            }
        }
    });

    managed
        .runs
        .register_owned(
            &run,
            runtime.timeout_secs,
            task,
            heartbeat_task,
            managed.worker_id.clone(),
            claim_token,
        )
        .map_err(|e| {
            oai_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to register managed run: {e}"),
                "server_error",
            )
        })?;
    append_run_event(
        managed.store.as_ref(),
        &run.id,
        ManagedRunEventDraft {
            kind: ManagedRunEventKind::RunStarted,
            message: None,
            tool_name: None,
            tool_call_id: None,
            metadata: None,
        },
    )
    .await;

    Ok((
        run_id,
        managed_session_id,
        timeout_secs,
        broadcast_rx,
        outcome_rx,
    ))
}

async fn handle_managed_run_replay(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    let Some(managed) = state.managed else {
        return managed_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed runtime not available",
        );
    };

    let requested_run = match managed.store.get_run(&run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => {
            return managed_error_response(
                StatusCode::NOT_FOUND,
                format!("managed run not found: {run_id}"),
            );
        }
        Err(e) => {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load managed run: {e}"),
            );
        }
    };

    let requested_summary = match hermes_managed::load_managed_run_derived_summary(
        managed.store.as_ref(),
        &requested_run.id,
    )
    .await
    {
        Ok(summary) => summary,
        Err(e) => {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load managed run recovery summary: {e}"),
            );
        }
    };
    if matches!(
        requested_run.status,
        ManagedRunStatus::Pending | ManagedRunStatus::Running
    ) {
        let owner_worker_id = requested_summary
            .ownership
            .as_ref()
            .map(|owner| owner.worker_id.clone());
        let note = match (requested_run.status.clone(), owner_worker_id.as_deref()) {
            (ManagedRunStatus::Running, Some(worker_id)) => format!(
                "manual replay blocked because run {} is still running under worker {}",
                requested_run.id, worker_id
            ),
            (ManagedRunStatus::Running, None) => format!(
                "manual replay blocked because run {} is still running",
                requested_run.id
            ),
            (ManagedRunStatus::Pending, _) => format!(
                "manual replay blocked because run {} has not finished starting",
                requested_run.id
            ),
            _ => format!(
                "manual replay blocked because run {} is still active",
                requested_run.id
            ),
        };
        let decision = ManagedRunRecoveryDecisionSummary {
            decision: ManagedRunRecoveryDecisionKind::Blocked,
            reason: Some(ManagedRunRecoveryDecisionReason::RunStillActive),
            replay_run_id: None,
            takeover_lineage_id: requested_summary
                .continuation
                .as_ref()
                .and_then(|summary| summary.takeover_lineage_id.clone())
                .or_else(|| {
                    requested_summary
                        .replay_provenance
                        .as_ref()
                        .and_then(|summary| summary.takeover_lineage_id.clone())
                }),
            evaluated_by_worker_id: Some(managed.worker_id.clone()),
            takeover_worker_id: owner_worker_id.clone(),
            worker_id: Some(managed.worker_id.clone()),
            active_follow_target_run_id: Some(requested_run.id.clone()),
            active_follow_target_status: Some(requested_run.status.clone()),
            active_follow_target_lineage_depth: Some(0),
            source_boundary: requested_summary
                .continuation_checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.kind),
            note: Some(note),
        };
        if let Err(e) = append_managed_recovery_decision_if_changed(
            managed.store.as_ref(),
            &requested_run.id,
            decision,
        )
        .await
        {
            tracing::warn!(
                run_id = requested_run.id,
                "failed to append blocked active-run replay decision: {e}"
            );
        }
        return managed_error_response(
            StatusCode::CONFLICT,
            format!(
                "managed run is still active and cannot be replayed: {}",
                requested_run.id
            ),
        );
    }
    if let Some(takeover) = requested_summary.takeover.as_ref().filter(|takeover| {
        takeover.takeover_state == hermes_managed::ManagedRunTakeoverState::Active
    }) {
        let decision = ManagedRunRecoveryDecisionSummary {
            decision: ManagedRunRecoveryDecisionKind::Blocked,
            reason: Some(ManagedRunRecoveryDecisionReason::ReplayChildActive),
            replay_run_id: Some(takeover.replay_run_id.clone()),
            takeover_lineage_id: takeover.takeover_lineage_id.clone(),
            evaluated_by_worker_id: Some(managed.worker_id.clone()),
            takeover_worker_id: takeover.takeover_worker_id.clone().or_else(|| {
                takeover
                    .current_owner
                    .as_ref()
                    .map(|owner| owner.worker_id.clone())
            }),
            worker_id: Some(managed.worker_id.clone()),
            active_follow_target_run_id: Some(takeover.replay_run_id.clone()),
            active_follow_target_status: Some(takeover.replay_run_status.clone()),
            active_follow_target_lineage_depth: Some(takeover.lineage_depth),
            source_boundary: takeover.source_boundary,
            note: Some(format!(
                "manual replay blocked because continuation is already owned by active replay run {}",
                takeover.replay_run_id
            )),
        };
        if let Err(e) = append_managed_recovery_decision_if_changed(
            managed.store.as_ref(),
            &requested_run.id,
            decision,
        )
        .await
        {
            tracing::warn!(
                run_id = requested_run.id,
                "failed to append blocked manual replay decision: {e}"
            );
        }
        return managed_error_response(
            StatusCode::CONFLICT,
            format!(
                "managed run already has an active replay continuation: {}",
                takeover.replay_run_id
            ),
        );
    }

    let replay_source_run = match requested_summary.takeover.as_ref() {
        Some(takeover) if takeover.replay_run_id != requested_run.id => {
            match managed.store.get_run(&takeover.replay_run_id).await {
                Ok(Some(run)) => run,
                Ok(None) => {
                    return managed_error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!(
                            "latest managed replay continuation disappeared before replay: {}",
                            takeover.replay_run_id
                        ),
                    );
                }
                Err(e) => {
                    return managed_error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("failed to load latest managed replay continuation source: {e}"),
                    );
                }
            }
        }
        _ => requested_run.clone(),
    };

    if replay_source_run.prompt.trim().is_empty() {
        return managed_error_response(
            StatusCode::CONFLICT,
            format!(
                "managed run is not replayable yet: {}",
                replay_source_run.id
            ),
        );
    }

    let (agent, version) =
        match load_managed_agent_version_for_run(&managed, &replay_source_run).await {
            Ok(value) => value,
            Err(response) => return response,
        };
    let replay_provenance = match build_managed_replay_provenance(
        &managed,
        &replay_source_run,
        ManagedReplayTrigger::ManualReplay,
        replay_source_run.status == ManagedRunStatus::Interrupted
            && replay_source_run.session_id.is_some(),
    )
    .await
    {
        Ok(value) => value,
        Err(e) => {
            return managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to build managed replay provenance: {e}"),
            );
        }
    };

    let new_run_id = match spawn_managed_run_with_version(
        &managed,
        &agent,
        &version,
        replay_source_run.prompt.clone(),
        replay_source_run.session_id.clone(),
        Some(replay_provenance),
        replay_source_run.status == ManagedRunStatus::Interrupted
            && replay_source_run.session_id.is_some(),
    )
    .await
    {
        Ok((run_id, _, _, _, _)) => run_id,
        Err(response) => return response,
    };

    match managed.store.get_run(&new_run_id).await {
        Ok(Some(run)) => match build_managed_run_envelope(&managed, run).await {
            Ok(envelope) => (StatusCode::CREATED, Json(envelope)).into_response(),
            Err(e) => managed_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load replayed managed run summary: {e}"),
            ),
        },
        Ok(None) => managed_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("managed replayed run disappeared after creation: {new_run_id}"),
        ),
        Err(e) => managed_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to reload replayed managed run: {e}"),
        ),
    }
}

async fn handle_managed_oai_chat(
    state: ApiState,
    req: OaiChatRequest,
    request_id: String,
    model_for_resp: String,
    agent_name: String,
) -> Response {
    let Some(managed) = state.managed.clone() else {
        return oai_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed runtime not available",
            "server_error",
        );
    };

    let prompt = match resolve_managed_turn_prompt(&req) {
        Ok(prompt) => prompt,
        Err(error) => return error.into_response(),
    };
    let (run_id, session_id, timeout_secs, mut stream_rx, outcome_rx) =
        match spawn_managed_run(&managed, &agent_name, prompt, req.session_id.clone()).await {
            Ok(value) => value,
            Err(response) => return response,
        };

    if req.stream {
        let rid = request_id.clone();
        let model = model_for_resp.clone();
        let created = epoch_secs();
        let store = Arc::clone(&managed.store);
        let runs = Arc::clone(&managed.runs);
        let stream_run_id = run_id.clone();
        let response_run_id = run_id.clone();
        let response_session_id = session_id.clone();
        let (tx, rx) = mpsc::channel::<std::result::Result<Event, Infallible>>(64);
        let outcome_rx = outcome_rx;

        tokio::spawn(async move {
            let result = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
                let initial = OaiStreamChunk {
                    id: rid.clone(),
                    object: "chat.completion.chunk",
                    created,
                    model: model.clone(),
                    choices: vec![OaiStreamChoice {
                        index: 0,
                        delta: OaiDelta {
                            role: Some("assistant"),
                            content: None,
                        },
                        finish_reason: None,
                    }],
                };
                if tx
                    .send(Ok(
                        Event::default().data(serde_json::to_string(&initial).unwrap_or_default())
                    ))
                    .await
                    .is_err()
                {
                    terminate_managed_run(
                        store,
                        runs,
                        stream_run_id.clone(),
                        ManagedRunStatus::Cancelled,
                        Some("client disconnected".to_string()),
                    )
                    .await;
                    return;
                }

                loop {
                    let delta = match stream_rx.recv().await {
                        Ok(delta) => delta,
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    };
                    let content = match delta {
                        StreamDelta::TextDelta(text) => Some(text),
                        StreamDelta::Done => break,
                        _ => None,
                    };
                    if let Some(text) = content {
                        let chunk = OaiStreamChunk {
                            id: rid.clone(),
                            object: "chat.completion.chunk",
                            created,
                            model: model.clone(),
                            choices: vec![OaiStreamChoice {
                                index: 0,
                                delta: OaiDelta {
                                    role: None,
                                    content: Some(text),
                                },
                                finish_reason: None,
                            }],
                        };
                        if tx
                            .send(Ok(Event::default()
                                .data(serde_json::to_string(&chunk).unwrap_or_default())))
                            .await
                            .is_err()
                        {
                            terminate_managed_run(
                                store,
                                runs,
                                stream_run_id.clone(),
                                ManagedRunStatus::Cancelled,
                                Some("client disconnected".to_string()),
                            )
                            .await;
                            return;
                        }
                    }
                }

                match outcome_rx.await {
                    Ok(ManagedRunOutcome::Completed(_)) => {
                        let final_chunk = OaiStreamChunk {
                            id: rid,
                            object: "chat.completion.chunk",
                            created,
                            model,
                            choices: vec![OaiStreamChoice {
                                index: 0,
                                delta: OaiDelta {
                                    role: None,
                                    content: None,
                                },
                                finish_reason: Some("stop"),
                            }],
                        };
                        let _ =
                            tx.send(Ok(Event::default()
                                .data(serde_json::to_string(&final_chunk).unwrap_or_default())))
                                .await;
                        let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
                    }
                    Ok(ManagedRunOutcome::Failed(_)) | Err(_) => {}
                }
            })
            .await;

            if result.is_err() {
                terminate_managed_run(
                    Arc::clone(&managed.store),
                    Arc::clone(&managed.runs),
                    stream_run_id.clone(),
                    ManagedRunStatus::TimedOut,
                    Some(format!("managed run timed out after {timeout_secs}s")),
                )
                .await;
            }
        });

        let response = Sse::new(ReceiverStream::new(rx))
            .keep_alive(
                axum::response::sse::KeepAlive::new()
                    .interval(Duration::from_secs(15))
                    .text(""),
            )
            .into_response();
        with_managed_response_headers(response, &response_run_id, response_session_id.as_deref())
    } else {
        let response = match tokio::time::timeout(Duration::from_secs(timeout_secs), outcome_rx)
            .await
        {
            Ok(Ok(ManagedRunOutcome::Completed(response))) => Json(OaiChatResponse {
                id: request_id,
                object: "chat.completion",
                created: epoch_secs(),
                model: model_for_resp,
                choices: vec![OaiChoice {
                    index: 0,
                    message: OaiMessage {
                        role: "assistant".into(),
                        content: response,
                    },
                    finish_reason: "stop",
                }],
                usage: OaiUsage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                },
            })
            .into_response(),
            Ok(Ok(ManagedRunOutcome::Failed(error_text))) => oai_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                error_text,
                "server_error",
            ),
            Ok(Err(_)) => match managed.store.get_run(&run_id).await {
                Ok(Some(run)) if run.status == ManagedRunStatus::TimedOut => oai_error_response(
                    StatusCode::GATEWAY_TIMEOUT,
                    run.last_error
                        .unwrap_or_else(|| "managed run timed out".to_string()),
                    "server_error",
                ),
                Ok(Some(run)) if run.status == ManagedRunStatus::Cancelled => oai_error_response(
                    StatusCode::CONFLICT,
                    run.last_error
                        .unwrap_or_else(|| "managed run cancelled".to_string()),
                    "server_error",
                ),
                Ok(Some(run)) if run.status == ManagedRunStatus::Failed => oai_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    run.last_error
                        .unwrap_or_else(|| "managed run failed".to_string()),
                    "server_error",
                ),
                Ok(Some(run)) if run.status == ManagedRunStatus::Interrupted => oai_error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    run.last_error
                        .unwrap_or_else(|| "managed run interrupted during recovery".to_string()),
                    "server_error",
                ),
                _ => {
                    finalize_managed_run(
                        Arc::clone(&managed.store),
                        Arc::clone(&managed.runs),
                        run_id.clone(),
                        ManagedRunStatus::Failed,
                        Some("managed run response dropped".to_string()),
                    )
                    .await;
                    oai_error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "managed run response dropped",
                        "server_error",
                    )
                }
            },
            Err(_) => {
                terminate_managed_run(
                    Arc::clone(&managed.store),
                    Arc::clone(&managed.runs),
                    run_id.clone(),
                    ManagedRunStatus::TimedOut,
                    Some(format!("managed run timed out after {timeout_secs}s")),
                )
                .await;
                oai_error_response(
                    StatusCode::GATEWAY_TIMEOUT,
                    format!("managed run timed out ({timeout_secs}s)"),
                    "server_error",
                )
            }
        };
        with_managed_response_headers(response, &run_id, session_id.as_deref())
    }
}

async fn handle_oai_chat(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(req): Json<OaiChatRequest>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    // Validate: at least one user message required
    let has_user_msg = req.messages.iter().any(|m| m.role == "user");
    if !has_user_msg {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": {"message": "no user message found", "type": "invalid_request_error"}})),
        )
            .into_response();
    }

    let request_id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let requested_model = req
        .model
        .clone()
        .unwrap_or_else(|| state.model_name.clone());
    if let Some(agent_name) = managed_agent_name(&requested_model) {
        let agent_name = agent_name.to_string();
        let model_for_resp = if requested_model.is_empty() {
            "hermes".to_string()
        } else {
            requested_model.clone()
        };
        return handle_managed_oai_chat(state, req, request_id, model_for_resp, agent_name).await;
    }

    let Some(router) = state.router else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": {"message": "router not available", "type": "server_error"}})),
        )
            .into_response();
    };

    // Build prompt from the full conversation history.
    // If there are prior messages (system/assistant/user turns), format them as context
    // so the agent sees the full conversation. The last user message is the prompt.
    let prompt = build_oai_prompt(&req.messages);

    let user_id = req.user.unwrap_or_else(|| "api-user".into());
    let model_for_resp = if requested_model.is_empty() {
        "hermes".to_string()
    } else {
        requested_model
    };

    // Stateless: each OAI request gets a fresh session (unique chat_id).
    // Sessions are cleaned up by the idle timeout.
    let event = MessageEvent {
        platform: "api".into(),
        chat_id: format!("oai-{}", uuid::Uuid::new_v4().simple()),
        user_id,
        user_name: None,
        text: prompt,
        reply_to: Some(request_id.clone()),
        chat_type: ChatType::DirectMessage,
        thread_id: None,
    };

    if req.stream {
        // ── SSE streaming ─────────────────────────────────────────────────
        let (mut stream_rx, _response_rx) = router.route_streaming(event).await;
        let rid = request_id.clone();
        let model = model_for_resp.clone();
        let created = epoch_secs();

        // Convert StreamDelta into SSE Event stream (300s timeout)
        let (tx, rx) = mpsc::channel::<std::result::Result<Event, Infallible>>(64);
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(300), async {
                // Send initial chunk with role
                let initial = OaiStreamChunk {
                    id: rid.clone(),
                    object: "chat.completion.chunk",
                    created,
                    model: model.clone(),
                    choices: vec![OaiStreamChoice {
                        index: 0,
                        delta: OaiDelta {
                            role: Some("assistant"),
                            content: None,
                        },
                        finish_reason: None,
                    }],
                };
                let _ = tx
                    .send(Ok(
                        Event::default().data(serde_json::to_string(&initial).unwrap_or_default())
                    ))
                    .await;

                while let Some(delta) = stream_rx.recv().await {
                    let content = match delta {
                        StreamDelta::TextDelta(text) => Some(text),
                        StreamDelta::Done => break,
                        _ => None,
                    };
                    if let Some(text) = content {
                        let chunk = OaiStreamChunk {
                            id: rid.clone(),
                            object: "chat.completion.chunk",
                            created,
                            model: model.clone(),
                            choices: vec![OaiStreamChoice {
                                index: 0,
                                delta: OaiDelta {
                                    role: None,
                                    content: Some(text),
                                },
                                finish_reason: None,
                            }],
                        };
                        if tx
                            .send(Ok(Event::default()
                                .data(serde_json::to_string(&chunk).unwrap_or_default())))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }

                // Send final chunk with finish_reason
                let final_chunk = OaiStreamChunk {
                    id: rid,
                    object: "chat.completion.chunk",
                    created,
                    model,
                    choices: vec![OaiStreamChoice {
                        index: 0,
                        delta: OaiDelta {
                            role: None,
                            content: None,
                        },
                        finish_reason: Some("stop"),
                    }],
                };
                let _ =
                    tx.send(Ok(Event::default()
                        .data(serde_json::to_string(&final_chunk).unwrap_or_default())))
                        .await;
                let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
            })
            .await; // timeout ends here — if it fires, tx drops and SSE closes
        });

        Sse::new(ReceiverStream::new(rx))
            .keep_alive(
                axum::response::sse::KeepAlive::new()
                    .interval(Duration::from_secs(15))
                    .text(""),
            )
            .into_response()
    } else {
        // ── Non-streaming (300s timeout) ──────────────────────────────────
        match tokio::time::timeout(Duration::from_secs(300), router.route(event)).await {
            Ok(response) => {
                let resp = OaiChatResponse {
                    id: request_id,
                    object: "chat.completion",
                    created: epoch_secs(),
                    model: model_for_resp,
                    choices: vec![OaiChoice {
                        index: 0,
                        message: OaiMessage {
                            role: "assistant".into(),
                            content: response,
                        },
                        finish_reason: "stop",
                    }],
                    usage: OaiUsage {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                        total_tokens: 0,
                    },
                };
                Json(resp).into_response()
            }
            Err(_) => (
                StatusCode::GATEWAY_TIMEOUT,
                Json(serde_json::json!({"error": {"message": "agent timed out (300s)", "type": "server_error"}})),
            )
                .into_response(),
        }
    }
}

async fn handle_oai_models(State(state): State<ApiState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }
    let model_id = if state.model_name.is_empty() {
        "hermes".to_string()
    } else {
        state.model_name.clone()
    };
    let mut data = vec![OaiModel {
        id: model_id,
        object: "model",
        owned_by: "hermes",
    }];
    if let Some(managed) = state.managed {
        if let Ok(mut agents) = managed.store.list_agents(1000).await {
            agents.retain(|agent| !agent.archived && agent.latest_version > 0);
            agents.sort_by(|a, b| a.name.cmp(&b.name));
            data.extend(agents.into_iter().map(|agent| OaiModel {
                id: format!("agent:{}", agent.name),
                object: "model",
                owned_by: "hermes",
            }));
        }
    }
    Json(OaiModelList {
        object: "list",
        data,
    })
    .into_response()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        fs, io,
        sync::atomic::{AtomicUsize, Ordering},
        sync::{Arc as StdArc, LazyLock, Mutex as StdMutex, OnceLock},
    };

    use super::*;
    use axum::body::Body;
    use chrono::Utc;
    use hermes_config::config::{ManagedConfigYaml, ManagedRecoveryPolicyYaml};
    use hermes_core::{
        error::HermesError,
        message::{Content, Role},
        provider::{
            ChatRequest as ProviderChatRequest, ChatResponse, ModelInfo, ModelPricing, Provider,
        },
        tool::ToolConfig,
    };
    use hermes_managed::{
        ManagedMcpAdmissionRejection, ManagedMcpReadOnlyCapabilityAttribution, ManagedRun,
    };
    use hermes_tools::{ToolRegistry, session_cleanup};
    use http::{Method, Request};
    use http_body_util::BodyExt;
    use tempfile::{NamedTempFile, TempDir};
    use tower::ServiceExt;

    static ENV_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    struct EnvVarGuard {
        name: String,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(name: &str, value: &str) -> Self {
            let previous = std::env::var(name).ok();
            unsafe {
                std::env::set_var(name, value);
            }
            Self {
                name: name.to_string(),
                previous,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(&self.name, value) },
                None => unsafe { std::env::remove_var(&self.name) },
            }
        }
    }

    fn listener_bind_is_unavailable(err: &io::Error) -> bool {
        matches!(
            err.kind(),
            io::ErrorKind::PermissionDenied
                | io::ErrorKind::AddrNotAvailable
                | io::ErrorKind::Unsupported
        )
    }

    async fn bind_test_listener() -> Option<tokio::net::TcpListener> {
        match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => Some(listener),
            Err(err) if listener_bind_is_unavailable(&err) => {
                eprintln!("skipping network-bound managed gateway test: {err}");
                None
            }
            Err(err) => panic!("failed to bind test listener: {err}"),
        }
    }

    fn pid_is_alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    async fn wait_for_pid_file(path: &std::path::Path) -> u32 {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(contents) = fs::read_to_string(path) {
                    if let Ok(pid) = contents.trim().parse::<u32>() {
                        return pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }

    #[derive(Clone, Copy)]
    enum MockOpenAiBehavior {
        HoldOpen,
        DelayedText { delay_ms: u64, text: &'static str },
        ServerError { message: &'static str },
    }

    #[derive(Clone, Copy)]
    enum MockManagedProviderProtocol {
        OpenAi,
        Anthropic,
        Responses,
    }

    #[derive(Debug, Clone)]
    struct MockProviderRequestAudit {
        auth_header: Option<String>,
        api_key_header: Option<String>,
        anthropic_version: Option<String>,
        model: Option<String>,
        messages: Vec<String>,
    }

    fn provider_request_messages(body: &serde_json::Value) -> Vec<String> {
        body.get("messages")
            .and_then(|value| value.as_array())
            .map(|messages| {
                messages
                    .iter()
                    .filter_map(|message| {
                        let content = message.get("content")?;
                        if let Some(text) = content.as_str() {
                            return Some(text.to_string());
                        }
                        let parts = content.as_array()?;
                        let merged = parts
                            .iter()
                            .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
                            .collect::<Vec<_>>()
                            .join("");
                        if merged.is_empty() {
                            None
                        } else {
                            Some(merged)
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    struct DummyProvider;

    #[async_trait]
    impl Provider for DummyProvider {
        async fn chat(
            &self,
            _request: &ProviderChatRequest<'_>,
            _delta_tx: Option<&mpsc::Sender<StreamDelta>>,
        ) -> hermes_core::error::Result<ChatResponse> {
            Err(HermesError::Config("unused".to_string()))
        }

        fn model_info(&self) -> &ModelInfo {
            static INFO: OnceLock<ModelInfo> = OnceLock::new();
            INFO.get_or_init(|| ModelInfo {
                id: "dummy".to_string(),
                provider: "test".to_string(),
                max_context: 8192,
                max_output: 1024,
                supports_tools: true,
                supports_vision: false,
                supports_reasoning: false,
                supports_caching: false,
                pricing: ModelPricing {
                    input_per_mtok: 0.0,
                    output_per_mtok: 0.0,
                    cache_read_per_mtok: 0.0,
                    cache_create_per_mtok: 0.0,
                },
            })
        }
    }

    fn build_stateful_app(state: ApiState) -> Router {
        Router::new()
            .route("/api/chat", post(handle_chat))
            .route("/v1/chat/completions", post(handle_oai_chat))
            .route(
                "/v1/agents",
                get(handle_managed_agents_list).post(handle_managed_agent_create),
            )
            .route(
                "/v1/agents/{id}",
                get(handle_managed_agent_get).delete(handle_managed_agent_archive),
            )
            .route(
                "/v1/agents/{id}/versions",
                get(handle_managed_agent_versions_list).post(handle_managed_agent_version_create),
            )
            .route(
                "/v1/agents/{id}/versions/{version}",
                get(handle_managed_agent_version_get),
            )
            .route("/v1/runs", get(handle_managed_runs_list))
            .route(
                "/v1/runs/{id}",
                get(handle_managed_run_get).delete(handle_managed_run_cancel),
            )
            .route("/v1/runs/{id}/events", get(handle_managed_run_events_list))
            .route(
                "/v1/runs/{id}/artifacts",
                get(handle_managed_run_artifacts_list),
            )
            .route("/v1/models", get(handle_oai_models))
            .route("/health", get(handle_health))
            .with_state(state)
    }

    fn build_app(event_tx: mpsc::Sender<PlatformEvent>) -> Router {
        let state = ApiState {
            event_tx,
            pending: Arc::new(DashMap::new()),
            api_key: None,
            model_name: "test-model".into(),
            router: None,
            managed: None,
        };
        build_stateful_app(state)
    }

    async fn build_managed_test_state_with_version(
        app_config: AppConfig,
        configure_version: impl FnOnce(&mut ManagedAgentVersion),
    ) -> (TempDir, ApiState, ManagedAgent, ManagedAgentVersion) {
        let tmp = tempfile::tempdir().unwrap();
        let session_store = Arc::new(
            hermes_config::SqliteSessionStore::open_at(&tmp.path().join("state.db"))
                .await
                .unwrap(),
        );
        let store = Arc::new(
            ManagedStore::open_at(&tmp.path().join("state.db"))
                .await
                .unwrap(),
        );
        let agent = ManagedAgent::new("code-reviewer");
        store.create_agent(&agent).await.unwrap();
        let mut version =
            ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "system prompt");
        configure_version(&mut version);
        store.create_agent_version(&version).await.unwrap();

        let (tx, _rx) = mpsc::channel(8);
        let state = ApiState {
            event_tx: tx,
            pending: Arc::new(DashMap::new()),
            api_key: None,
            model_name: "test-model".into(),
            router: None,
            managed: Some(ManagedApiState {
                shared: Arc::new(SharedState {
                    provider: Arc::new(DummyProvider),
                    registry: Arc::new(ToolRegistry::new()),
                    tool_config: Arc::new(ToolConfig::default()),
                    skills: None,
                    session_store: Some(session_store),
                    adapters: HashMap::new(),
                }),
                app_config,
                store,
                runs: Arc::new(RunRegistry::new()),
                worker_id: "gw_test".to_string(),
            }),
        };

        (tmp, state, agent, version)
    }

    async fn build_managed_test_state() -> (TempDir, ApiState, ManagedAgent, ManagedAgentVersion) {
        build_managed_test_state_with_version(AppConfig::default(), |_| {}).await
    }

    #[test]
    fn managed_mcp_rejection_event_serializes_structured_read_only_attribution() {
        let rejection = ManagedMcpAdmissionRejection {
            code: "read_only_prompt_capability_blocked_by_allowlist".to_string(),
            error: "ignored".to_string(),
            requested_tools: vec!["mcp_prompt_list".to_string()],
            requested_read_only_tools: vec!["mcp_prompt_list".to_string()],
            requested_side_effect_tools: vec![],
            requested_dynamic_tools: vec![],
            allowed_servers: vec!["docs".to_string()],
            allowed_transports: vec!["http".to_string()],
            allow_side_effects: false,
            allowed_stdio_servers: vec![],
            allowed_stdio_env_keys: vec![],
            stdio_server_summaries: vec![],
            read_only_capability_attribution: ManagedMcpReadOnlyCapabilityAttribution {
                prompt_tools: vec!["mcp_prompt_list".to_string()],
                resource_tools: vec![],
                blocked_http_prompt_servers: vec!["archive".to_string()],
                blocked_http_resource_servers: vec![],
            },
        };

        let event = managed_mcp_admission_rejection_event(&rejection);
        let metadata = event
            .metadata
            .expect("structured rejection metadata missing");
        assert_eq!(
            metadata
                .get("read_only_capability_attribution")
                .and_then(|value| value.get("prompt_tools"))
                .and_then(|value| value.as_array())
                .and_then(|values| values.first())
                .and_then(|value| value.as_str()),
            Some("mcp_prompt_list")
        );
        assert_eq!(
            metadata
                .get("read_only_capability_attribution")
                .and_then(|value| value.get("blocked_http_prompt_servers"))
                .and_then(|value| value.as_array())
                .and_then(|values| values.first())
                .and_then(|value| value.as_str()),
            Some("archive")
        );
    }

    #[tokio::test]
    async fn log_managed_cleanup_persists_failure_event() {
        let (_tmp, state, agent, _version) = build_managed_test_state().await;
        let managed = state.managed.as_ref().unwrap();
        let run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        managed.store.create_run(&run).await.unwrap();

        log_managed_cleanup(
            managed.store.as_ref(),
            &run.id,
            session_cleanup::SessionCleanupSummary {
                session_id: run.id.clone(),
                attempted: 2,
                cleaned: 1,
                failures: vec!["failed to clean durable resource".to_string()],
            },
        )
        .await;

        let events = managed.store.list_run_events(&run.id, 32).await.unwrap();
        let cleanup_failed = events
            .iter()
            .find(|event| event.kind == ManagedRunEventKind::RunCleanupFailed)
            .expect("expected cleanup failure event");
        assert_eq!(
            cleanup_failed.message.as_deref(),
            Some("Managed run cleanup failed for 1 resource(s)")
        );
        assert_eq!(
            cleanup_failed.metadata.as_ref().unwrap()["phase"],
            "terminal_cleanup"
        );
        assert_eq!(cleanup_failed.metadata.as_ref().unwrap()["attempted"], 2);
        assert_eq!(cleanup_failed.metadata.as_ref().unwrap()["cleaned"], 1);
    }

    async fn spawn_mock_openai_server(
        behavior: MockOpenAiBehavior,
    ) -> Option<(String, oneshot::Receiver<()>, tokio::task::JoinHandle<()>)> {
        let (started_tx, started_rx) = oneshot::channel::<()>();
        let started_tx = StdArc::new(StdMutex::new(Some(started_tx)));

        let app = Router::new().route(
            "/v1/chat/completions",
            post({
                let started_tx = StdArc::clone(&started_tx);
                move || {
                    let started_tx = StdArc::clone(&started_tx);
                    async move {
                        if let Some(tx) = started_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }

                        if let MockOpenAiBehavior::ServerError { message } = behavior {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(serde_json::json!({
                                    "error": { "message": message }
                                })),
                            )
                                .into_response();
                        }

                        let (tx, rx) = mpsc::channel::<std::result::Result<Event, Infallible>>(8);
                        tokio::spawn(async move {
                            match behavior {
                                MockOpenAiBehavior::HoldOpen => {
                                    tokio::time::sleep(Duration::from_secs(60)).await;
                                }
                                MockOpenAiBehavior::DelayedText { delay_ms, text } => {
                                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                    let chunk = serde_json::json!({
                                        "choices": [{
                                            "delta": { "content": text },
                                            "finish_reason": null
                                        }]
                                    });
                                    let _ = tx
                                        .send(Ok(Event::default()
                                            .data(serde_json::to_string(&chunk).unwrap())))
                                        .await;
                                    tokio::time::sleep(Duration::from_secs(60)).await;
                                }
                                MockOpenAiBehavior::ServerError { .. } => {}
                            }
                        });

                        Sse::new(ReceiverStream::new(rx)).into_response()
                    }
                }
            }),
        );

        let listener = bind_test_listener().await?;
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Some((format!("http://{addr}/v1"), started_rx, handle))
    }

    async fn spawn_mock_model_catalog_server(
        model_ids: Vec<&'static str>,
    ) -> Option<(String, tokio::task::JoinHandle<()>)> {
        let app = Router::new().route(
            "/v1/models",
            get(move || {
                let payload = serde_json::json!({
                    "object": "list",
                    "data": model_ids.iter().map(|id| serde_json::json!({ "id": id })).collect::<Vec<_>>(),
                });
                async move { Json(payload) }
            }),
        );

        let listener = bind_test_listener().await?;
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Some((format!("http://{addr}/v1"), handle))
    }

    async fn spawn_mock_managed_provider_server(
        protocol: MockManagedProviderProtocol,
        response_text: &'static str,
    ) -> Option<(
        String,
        StdArc<StdMutex<Vec<MockProviderRequestAudit>>>,
        tokio::task::JoinHandle<()>,
    )> {
        let audits = StdArc::new(StdMutex::new(Vec::<MockProviderRequestAudit>::new()));

        let app = match protocol {
            MockManagedProviderProtocol::OpenAi => Router::new().route(
                "/v1/chat/completions",
                post({
                    let audits = StdArc::clone(&audits);
                    move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                        let audits = StdArc::clone(&audits);
                        async move {
                            audits.lock().unwrap().push(MockProviderRequestAudit {
                                auth_header: headers
                                    .get("authorization")
                                    .and_then(|v| v.to_str().ok())
                                    .map(ToOwned::to_owned),
                                api_key_header: None,
                                anthropic_version: None,
                                model: body
                                    .get("model")
                                    .and_then(|v| v.as_str())
                                    .map(ToOwned::to_owned),
                                messages: provider_request_messages(&body),
                            });

                            let (tx, rx) =
                                mpsc::channel::<std::result::Result<Event, Infallible>>(8);
                            tokio::spawn(async move {
                                let text_chunk = serde_json::json!({
                                    "choices": [{
                                        "delta": { "content": response_text },
                                        "finish_reason": null
                                    }]
                                });
                                let finish_chunk = serde_json::json!({
                                    "choices": [{
                                        "delta": {},
                                        "finish_reason": "stop"
                                    }],
                                    "usage": {
                                        "prompt_tokens": 3,
                                        "completion_tokens": 2
                                    }
                                });
                                let _ = tx
                                    .send(Ok(Event::default().data(
                                        serde_json::to_string(&text_chunk).unwrap(),
                                    )))
                                    .await;
                                let _ = tx
                                    .send(Ok(Event::default().data(
                                        serde_json::to_string(&finish_chunk).unwrap(),
                                    )))
                                    .await;
                            });

                            Sse::new(ReceiverStream::new(rx)).into_response()
                        }
                    }
                }),
            ),
            MockManagedProviderProtocol::Anthropic => Router::new().route(
                "/v1/messages",
                post({
                    let audits = StdArc::clone(&audits);
                    move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                        let audits = StdArc::clone(&audits);
                        async move {
                            audits.lock().unwrap().push(MockProviderRequestAudit {
                                auth_header: headers
                                    .get("authorization")
                                    .and_then(|v| v.to_str().ok())
                                    .map(ToOwned::to_owned),
                                api_key_header: headers
                                    .get("x-api-key")
                                    .and_then(|v| v.to_str().ok())
                                    .map(ToOwned::to_owned),
                                anthropic_version: headers
                                    .get("anthropic-version")
                                    .and_then(|v| v.to_str().ok())
                                    .map(ToOwned::to_owned),
                                model: body
                                    .get("model")
                                    .and_then(|v| v.as_str())
                                    .map(ToOwned::to_owned),
                                messages: provider_request_messages(&body),
                            });

                            let (tx, rx) =
                                mpsc::channel::<std::result::Result<Event, Infallible>>(8);
                            tokio::spawn(async move {
                                let _ = tx
                                    .send(Ok(Event::default()
                                        .event("message_start")
                                        .data(r#"{"message":{"usage":{"input_tokens":3}}}"#)))
                                    .await;
                                let _ = tx
                                    .send(Ok(Event::default().event("content_block_start").data(
                                        r#"{"index":0,"content_block":{"type":"text","text":""}}"#,
                                    )))
                                    .await;
                                let _ = tx
                                    .send(Ok(Event::default()
                                        .event("content_block_delta")
                                        .data(format!(
                                            r#"{{"index":0,"delta":{{"type":"text_delta","text":"{response_text}"}}}}"#
                                        ))))
                                    .await;
                                let _ = tx
                                    .send(Ok(Event::default()
                                        .event("content_block_stop")
                                        .data(r#"{"index":0}"#)))
                                    .await;
                                let _ = tx
                                    .send(Ok(Event::default().event("message_delta").data(
                                        r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
                                    )))
                                    .await;
                                let _ = tx
                                    .send(Ok(
                                        Event::default().event("message_stop").data(r#"{}"#),
                                    ))
                                    .await;
                            });

                            Sse::new(ReceiverStream::new(rx)).into_response()
                        }
                    }
                }),
            ),
            MockManagedProviderProtocol::Responses => Router::new().route(
                "/v1/responses",
                post({
                    let audits = StdArc::clone(&audits);
                    move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                        let audits = StdArc::clone(&audits);
                        async move {
                            audits.lock().unwrap().push(MockProviderRequestAudit {
                                auth_header: headers
                                    .get("authorization")
                                    .and_then(|v| v.to_str().ok())
                                    .map(ToOwned::to_owned),
                                api_key_header: None,
                                anthropic_version: None,
                                model: body
                                    .get("model")
                                    .and_then(|v| v.as_str())
                                    .map(ToOwned::to_owned),
                                messages: provider_request_messages(&body),
                            });

                            let (tx, rx) =
                                mpsc::channel::<std::result::Result<Event, Infallible>>(8);
                            tokio::spawn(async move {
                                let text_delta = serde_json::json!({
                                    "type": "response.output_text.delta",
                                    "delta": response_text
                                });
                                let completed = serde_json::json!({
                                    "type": "response.completed",
                                    "response": {
                                        "output": [{
                                            "type": "message",
                                            "role": "assistant",
                                            "content": [{
                                                "type": "output_text",
                                                "text": response_text
                                            }]
                                        }],
                                        "usage": {
                                            "input_tokens": 3,
                                            "output_tokens": 2,
                                            "output_tokens_details": {
                                                "reasoning_tokens": 0
                                            }
                                        }
                                    }
                                });
                                let _ = tx
                                    .send(Ok(Event::default().data(
                                        serde_json::to_string(&text_delta).unwrap(),
                                    )))
                                    .await;
                                let _ = tx
                                    .send(Ok(Event::default().data(
                                        serde_json::to_string(&completed).unwrap(),
                                    )))
                                    .await;
                            });

                            Sse::new(ReceiverStream::new(rx)).into_response()
                        }
                    }
                }),
            ),
        };

        let listener = bind_test_listener().await?;
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Some((format!("http://{addr}/v1"), audits, handle))
    }

    async fn spawn_mock_managed_tool_then_hold_server() -> Option<(
        String,
        StdArc<StdMutex<Vec<MockProviderRequestAudit>>>,
        tokio::task::JoinHandle<()>,
    )> {
        let audits = StdArc::new(StdMutex::new(Vec::<MockProviderRequestAudit>::new()));
        let request_count = StdArc::new(AtomicUsize::new(0));

        let app = Router::new().route(
            "/v1/chat/completions",
            post({
                let audits = StdArc::clone(&audits);
                let request_count = StdArc::clone(&request_count);
                move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                    let audits = StdArc::clone(&audits);
                    let request_count = StdArc::clone(&request_count);
                    async move {
                        audits.lock().unwrap().push(MockProviderRequestAudit {
                            auth_header: headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .map(ToOwned::to_owned),
                            api_key_header: None,
                            anthropic_version: None,
                            model: body
                                .get("model")
                                .and_then(|v| v.as_str())
                                .map(ToOwned::to_owned),
                            messages: provider_request_messages(&body),
                        });
                        let request_index = request_count.fetch_add(1, Ordering::SeqCst);

                        let (tx, rx) = mpsc::channel::<std::result::Result<Event, Infallible>>(8);
                        tokio::spawn(async move {
                            if request_index == 0 {
                                let tool_call_chunk = serde_json::json!({
                                    "choices": [{
                                        "delta": {
                                            "tool_calls": [{
                                                "index": 0,
                                                "id": "call_resume",
                                                "function": {
                                                    "name": "unknown_tool",
                                                    "arguments": "{}"
                                                }
                                            }]
                                        },
                                        "finish_reason": null
                                    }]
                                });
                                let finish_chunk = serde_json::json!({
                                    "choices": [{
                                        "delta": {},
                                        "finish_reason": "tool_calls"
                                    }],
                                    "usage": {
                                        "prompt_tokens": 3,
                                        "completion_tokens": 1
                                    }
                                });
                                let _ = tx
                                    .send(Ok(Event::default()
                                        .data(serde_json::to_string(&tool_call_chunk).unwrap())))
                                    .await;
                                let _ = tx
                                    .send(Ok(Event::default()
                                        .data(serde_json::to_string(&finish_chunk).unwrap())))
                                    .await;
                            } else {
                                tokio::time::sleep(Duration::from_secs(60)).await;
                            }
                        });

                        Sse::new(ReceiverStream::new(rx)).into_response()
                    }
                }
            }),
        );

        let listener = bind_test_listener().await?;
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Some((format!("http://{addr}/v1"), audits, handle))
    }

    async fn wait_for_created_run(store: &ManagedStore) -> hermes_managed::ManagedRun {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(run) = store.list_runs(10).await.unwrap().into_iter().next() {
                    return run;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }

    async fn run_managed_provider_case(
        protocol: MockManagedProviderProtocol,
        version_model: &str,
        env_var: &str,
        api_key: &str,
        expected_request_model: &str,
        expected_response_text: &'static str,
    ) {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set(env_var, api_key);
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let Some((base_url, audits, server_handle)) =
            spawn_mock_managed_provider_server(protocol, expected_response_text).await
        else {
            return;
        };
        let (_tmp, state, _agent, _version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.model = version_model.to_string();
                version.base_url = Some(base_url.clone());
                version.timeout_secs = 5;
            })
            .await;
        let managed = state.managed.clone().unwrap();
        let app = build_stateful_app(state);

        let body = serde_json::json!({
            "model": "agent:code-reviewer",
            "messages": [{"role": "user", "content": "hello"}]
        })
        .to_string();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["choices"][0]["message"]["content"],
            expected_response_text
        );
        assert_eq!(json["model"], "agent:code-reviewer");

        let run = wait_for_created_run(managed.store.as_ref()).await;
        let stored =
            wait_for_run_status(managed.store.as_ref(), &run.id, ManagedRunStatus::Completed).await;
        assert_eq!(stored.model, version_model);
        assert_eq!(stored.last_error, None);

        let audits = audits.lock().unwrap();
        assert_eq!(audits.len(), 1);
        let audit = &audits[0];
        assert_eq!(audit.model.as_deref(), Some(expected_request_model));
        match protocol {
            MockManagedProviderProtocol::OpenAi | MockManagedProviderProtocol::Responses => {
                let expected_auth = format!("Bearer {api_key}");
                assert_eq!(audit.auth_header.as_deref(), Some(expected_auth.as_str()));
                assert!(audit.api_key_header.is_none());
                assert!(audit.anthropic_version.is_none());
            }
            MockManagedProviderProtocol::Anthropic => {
                assert!(audit.auth_header.is_none());
                assert_eq!(audit.api_key_header.as_deref(), Some(api_key));
                assert_eq!(audit.anthropic_version.as_deref(), Some("2023-06-01"));
            }
        }
        drop(audits);

        server_handle.abort();
    }

    async fn wait_for_run_status(
        store: &ManagedStore,
        run_id: &str,
        status: ManagedRunStatus,
    ) -> hermes_managed::ManagedRun {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(run) = store.get_run(run_id).await.unwrap() {
                    if run.status == status {
                        return run;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }

    async fn wait_for_run_eviction(runs: &RunRegistry, run_id: &str) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if runs.snapshot(run_id).is_none() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }

    async fn wait_for_session_history_len(
        session_store: &dyn SessionStore,
        session_id: &str,
        expected_len: usize,
    ) -> Vec<Message> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let history = session_store.load_history(session_id).await.unwrap();
                if history.len() >= expected_len {
                    return history;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let (tx, _rx) = mpsc::channel(8);
        let app = build_app(tx);

        let request = Request::builder()
            .method(Method::GET)
            .uri("/health")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[test]
    fn test_chat_request_json() {
        let json = r#"{"message":"hello","session_id":"s1","user_id":"u1"}"#;
        let req: ChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.message, "hello");
        assert_eq!(req.session_id.as_deref(), Some("s1"));
        assert_eq!(req.user_id.as_deref(), Some("u1"));

        // Optional fields absent
        let json_min = r#"{"message":"hi"}"#;
        let req_min: ChatRequest = serde_json::from_str(json_min).unwrap();
        assert_eq!(req_min.message, "hi");
        assert!(req_min.session_id.is_none());
        assert!(req_min.user_id.is_none());
    }

    #[tokio::test]
    async fn test_pending_map_resolve() {
        let pending: Arc<DashMap<String, oneshot::Sender<String>>> = Arc::new(DashMap::new());
        let (tx, rx) = oneshot::channel::<String>();
        let request_id = "test-req-123".to_string();
        pending.insert(request_id.clone(), tx);

        // Simulate send_response: look up + remove + send
        let event = MessageEvent {
            platform: "api".into(),
            chat_id: "session1".into(),
            user_id: "user1".into(),
            user_name: None,
            text: "hello".into(),
            reply_to: Some(request_id.clone()),
            chat_type: ChatType::DirectMessage,
            thread_id: None,
        };

        if let Some((_, sender)) = pending.remove(event.reply_to.as_deref().unwrap_or("")) {
            sender.send("agent reply".to_string()).unwrap();
        }

        let received = rx.await.unwrap();
        assert_eq!(received, "agent reply");
        // Entry was removed
        assert!(!pending.contains_key(&request_id));
    }

    #[tokio::test]
    async fn test_chat_gateway_error() {
        // Drop the receiver immediately so the channel is "closed".
        let (tx, rx) = mpsc::channel::<PlatformEvent>(1);
        drop(rx);

        let app = build_app(tx);

        let body = serde_json::json!({"message": "ping"}).to_string();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/chat")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"], "gateway not running");
    }

    #[tokio::test]
    async fn test_oai_models_endpoint() {
        let (tx, _rx) = mpsc::channel(8);
        let app = build_app(tx);

        let request = Request::builder()
            .method(Method::GET)
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["object"], "list");
        assert_eq!(json["data"][0]["id"], "test-model");
        assert_eq!(json["data"][0]["object"], "model");
    }

    #[tokio::test]
    async fn test_oai_models_endpoint_lists_managed_agents() {
        let (_tmp, state, _agent, _version) = build_managed_test_state().await;
        let app = Router::new()
            .route("/v1/models", get(handle_oai_models))
            .with_state(state);

        let request = Request::builder()
            .method(Method::GET)
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let ids = json["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();

        assert!(ids.contains(&"test-model".to_string()));
        assert!(ids.contains(&"agent:code-reviewer".to_string()));
    }

    #[tokio::test]
    async fn test_managed_agents_create_get_list_and_archive() {
        let (_tmp, state, _agent, _version) = build_managed_test_state().await;
        let app = build_stateful_app(state.clone());

        let create_body = serde_json::json!({ "name": "planner_01" }).to_string();
        let create_request = Request::builder()
            .method(Method::POST)
            .uri("/v1/agents")
            .header("content-type", "application/json")
            .body(Body::from(create_body))
            .unwrap();
        let create_response = app.clone().oneshot(create_request).await.unwrap();
        assert_eq!(create_response.status(), StatusCode::CREATED);

        let create_bytes = create_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let create_json: serde_json::Value = serde_json::from_slice(&create_bytes).unwrap();
        let created_agent_id = create_json["agent"]["id"].as_str().unwrap().to_string();
        assert_eq!(create_json["agent"]["name"], "planner_01");
        assert!(create_json["latest_version"].is_null());

        let list_request = Request::builder()
            .method(Method::GET)
            .uri("/v1/agents")
            .body(Body::empty())
            .unwrap();
        let list_response = app.clone().oneshot(list_request).await.unwrap();
        assert_eq!(list_response.status(), StatusCode::OK);
        let list_bytes = list_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let list_json: serde_json::Value = serde_json::from_slice(&list_bytes).unwrap();
        let names = list_json["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(names.contains(&"code-reviewer".to_string()));
        assert!(names.contains(&"planner_01".to_string()));

        let get_request = Request::builder()
            .method(Method::GET)
            .uri(format!("/v1/agents/{created_agent_id}"))
            .body(Body::empty())
            .unwrap();
        let get_response = app.clone().oneshot(get_request).await.unwrap();
        assert_eq!(get_response.status(), StatusCode::OK);
        let get_bytes = get_response.into_body().collect().await.unwrap().to_bytes();
        let get_json: serde_json::Value = serde_json::from_slice(&get_bytes).unwrap();
        assert_eq!(get_json["agent"]["id"], created_agent_id);
        assert!(get_json["latest_version"].is_null());

        let archive_request = Request::builder()
            .method(Method::DELETE)
            .uri(format!("/v1/agents/{created_agent_id}"))
            .body(Body::empty())
            .unwrap();
        let archive_response = app.clone().oneshot(archive_request).await.unwrap();
        assert_eq!(archive_response.status(), StatusCode::OK);
        let archive_bytes = archive_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let archive_json: serde_json::Value = serde_json::from_slice(&archive_bytes).unwrap();
        assert_eq!(archive_json["agent"]["archived"], true);

        let list_active_request = Request::builder()
            .method(Method::GET)
            .uri("/v1/agents")
            .body(Body::empty())
            .unwrap();
        let list_active_response = app.clone().oneshot(list_active_request).await.unwrap();
        let list_active_bytes = list_active_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let list_active_json: serde_json::Value =
            serde_json::from_slice(&list_active_bytes).unwrap();
        let active_names = list_active_json["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(!active_names.contains(&"planner_01".to_string()));

        let list_all_request = Request::builder()
            .method(Method::GET)
            .uri("/v1/agents?include_archived=true")
            .body(Body::empty())
            .unwrap();
        let list_all_response = app.oneshot(list_all_request).await.unwrap();
        let list_all_bytes = list_all_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let list_all_json: serde_json::Value = serde_json::from_slice(&list_all_bytes).unwrap();
        let archived_flags = list_all_json["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["id"] == created_agent_id)
            .map(|entry| entry["archived"].as_bool().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(archived_flags, vec![true]);
    }

    #[tokio::test]
    async fn test_managed_agent_versions_create_list_and_get() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");

        let Some((base_url, server_handle)) = spawn_mock_model_catalog_server(vec!["gpt-4o"]).await
        else {
            return;
        };
        let app_config = AppConfig {
            base_url: Some(base_url.clone()),
            ..AppConfig::default()
        };
        let (_tmp, state, agent, _version) =
            build_managed_test_state_with_version(app_config, |_| {}).await;
        let app = build_stateful_app(state.clone());

        let create_body = serde_json::json!({
            "model": "openai/gpt-4o",
            "base_url": base_url,
            "system_prompt": "review everything",
            "allowed_tools": ["read_file", "search_files"],
            "max_iterations": 42,
            "temperature": 0.3,
            "approval_policy": "deny",
            "timeout_secs": 180
        })
        .to_string();
        let create_request = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/agents/{}/versions", agent.id))
            .header("content-type", "application/json")
            .body(Body::from(create_body))
            .unwrap();
        let create_response = app.clone().oneshot(create_request).await.unwrap();
        assert_eq!(create_response.status(), StatusCode::CREATED);

        let create_bytes = create_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let create_json: serde_json::Value = serde_json::from_slice(&create_bytes).unwrap();
        assert_eq!(create_json["version"]["version"], 2);
        assert_eq!(create_json["version"]["approval_policy"], "deny");

        let get_agent_request = Request::builder()
            .method(Method::GET)
            .uri(format!("/v1/agents/{}", agent.id))
            .body(Body::empty())
            .unwrap();
        let get_agent_response = app.clone().oneshot(get_agent_request).await.unwrap();
        let get_agent_bytes = get_agent_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let get_agent_json: serde_json::Value = serde_json::from_slice(&get_agent_bytes).unwrap();
        assert_eq!(get_agent_json["latest_version"]["version"], 2);

        let list_request = Request::builder()
            .method(Method::GET)
            .uri(format!("/v1/agents/{}/versions", agent.id))
            .body(Body::empty())
            .unwrap();
        let list_response = app.clone().oneshot(list_request).await.unwrap();
        assert_eq!(list_response.status(), StatusCode::OK);
        let list_bytes = list_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let list_json: serde_json::Value = serde_json::from_slice(&list_bytes).unwrap();
        assert_eq!(list_json["data"][0]["version"], 2);
        assert_eq!(list_json["data"][1]["version"], 1);

        let get_request = Request::builder()
            .method(Method::GET)
            .uri(format!("/v1/agents/{}/versions/2", agent.id))
            .body(Body::empty())
            .unwrap();
        let get_response = app.oneshot(get_request).await.unwrap();
        assert_eq!(get_response.status(), StatusCode::OK);
        let get_bytes = get_response.into_body().collect().await.unwrap().to_bytes();
        let get_json: serde_json::Value = serde_json::from_slice(&get_bytes).unwrap();
        assert_eq!(get_json["version"]["model"], "openai/gpt-4o");
        assert_eq!(get_json["version"]["timeout_secs"], 180);

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_managed_agent_version_create_inherits_app_config_model_and_base_url() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");

        let Some((base_url, server_handle)) =
            spawn_mock_model_catalog_server(vec!["gpt-4o-mini"]).await
        else {
            return;
        };
        let app_config = AppConfig {
            model: "openai/gpt-4o-mini".to_string(),
            base_url: Some(base_url.clone()),
            ..AppConfig::default()
        };
        let (_tmp, state, agent, _version) =
            build_managed_test_state_with_version(app_config, |_| {}).await;
        let app = build_stateful_app(state);

        let create_body = serde_json::json!({
            "system_prompt": "review everything",
            "allowed_tools": ["read_file"],
            "max_iterations": 8
        })
        .to_string();
        let create_request = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/agents/{}/versions", agent.id))
            .header("content-type", "application/json")
            .body(Body::from(create_body))
            .unwrap();
        let create_response = app.oneshot(create_request).await.unwrap();
        assert_eq!(create_response.status(), StatusCode::CREATED);

        let create_bytes = create_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let create_json: serde_json::Value = serde_json::from_slice(&create_bytes).unwrap();
        assert_eq!(create_json["version"]["model"], "openai/gpt-4o-mini");
        assert_eq!(create_json["version"]["base_url"], base_url);

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_managed_agent_version_create_rejects_unsupported_tools() {
        let (_tmp, state, agent, _version) = build_managed_test_state().await;
        let app = build_stateful_app(state);

        let body = serde_json::json!({
            "model": "openai/gpt-4o-mini",
            "system_prompt": "review",
            "allowed_tools": ["terminal"]
        })
        .to_string();
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/agents/{}/versions", agent.id))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(json["error"].as_str().unwrap().contains("terminal"));
    }

    #[tokio::test]
    async fn test_managed_runs_list_and_get() {
        let (_tmp, state, agent, _version) = build_managed_test_state().await;
        let managed = state.managed.clone().unwrap();

        let mut run = hermes_managed::ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry this run".to_string();
        run.session_id = Some("session_123".to_string());
        managed.store.create_run(&run).await.unwrap();
        let source_run_id = run.id.clone();
        managed
            .store
            .append_run_event(
                &run.id,
                &managed_mcp_admission_rejection_event(&ManagedMcpAdmissionRejection {
                    code: "disabled_by_operator_policy".to_string(),
                    error: "managed MCP tools are disabled by operator policy: mcp_resource_read"
                        .to_string(),
                    requested_tools: vec!["mcp_resource_read".to_string()],
                    requested_read_only_tools: vec!["mcp_resource_read".to_string()],
                    requested_side_effect_tools: vec![],
                    requested_dynamic_tools: vec![],
                    allowed_servers: vec![],
                    allowed_transports: vec![],
                    allow_side_effects: false,
                    allowed_stdio_servers: vec![],
                    allowed_stdio_env_keys: vec![],
                    stdio_server_summaries: vec![],
                    read_only_capability_attribution:
                        ManagedMcpReadOnlyCapabilityAttribution::default(),
                }),
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_artifact(
                &run.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::AssistantOutput,
                    label: "assistant_output".to_string(),
                    tool_name: None,
                    tool_call_id: None,
                    content: "Checkpointed answer".to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();
        managed
            .store
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
        managed
            .store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunProviderCallStarted,
                    message: Some(
                        "managed run provider call started from pending tool calls boundary"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "request_history_len": 2,
                        "tool_count": 4,
                        "safe_resume_from": {
                            "kind": "pending_tool_calls",
                            "safe_action": "execute_pending_tools",
                            "history_len": 2,
                            "pending_tool_calls": 1,
                        },
                        "note": "provider call dispatched before a newer durable response checkpoint",
                    })),
                },
            )
            .await
            .unwrap();
        managed
            .store
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
        managed
            .store
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
        let mut replay_child = hermes_managed::ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        replay_child.status = ManagedRunStatus::Running;
        replay_child.prompt = run.prompt.clone();
        replay_child.session_id = run.session_id.clone();
        replay_child.replay_of_run_id = Some(source_run_id.clone());
        managed.store.create_run(&replay_child).await.unwrap();
        let claimed_at = Utc::now();
        managed
            .store
            .claim_run_ownership(
                &replay_child.id,
                "worker_gateway_takeover",
                "claim_gateway_takeover",
                claimed_at,
                claimed_at + chrono::Duration::seconds(30),
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunOwnershipClaimed,
                    message: Some(
                        "managed run ownership claimed by worker_gateway_takeover".to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "worker_id": "worker_gateway_takeover",
                        "claimed_at": claimed_at.to_rfc3339(),
                        "lease_expires_at": (claimed_at + chrono::Duration::seconds(30)).to_rfc3339(),
                        "takeover_lineage_id": replay_child.id.as_str(),
                    })),
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &replay_child.id,
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
        managed
            .store
            .append_run_event(
                &replay_child.id,
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
        managed
            .store
            .append_run_event(
                &replay_child.id,
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
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunTakeoverAssessed,
                    message: Some(
                        "worker gw_leaf_eval assessed interrupted run takeover with 1 blocking runtime risk"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "evaluated_by_worker_id": "gw_leaf_eval",
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
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunTakeoverAssessed,
                    message: Some(
                        "worker gw_leaf_eval assessed interrupted run takeover with 1 blocking runtime risk"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "evaluated_by_worker_id": "gw_leaf_eval",
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
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": source_run_id.as_str(),
                        "replay_root_run_id": source_run_id.as_str(),
                        "replay_depth": 1,
                        "replay_trigger": "interrupted_auto_replay",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                    })),
                },
            )
            .await
            .unwrap();

        let app = Router::new()
            .route("/v1/runs", get(handle_managed_runs_list))
            .route("/v1/runs/{id}", get(handle_managed_run_get))
            .with_state(state);

        let list_request = Request::builder()
            .method(Method::GET)
            .uri("/v1/runs")
            .body(Body::empty())
            .unwrap();
        let list_response = app.clone().oneshot(list_request).await.unwrap();
        assert_eq!(list_response.status(), StatusCode::OK);

        let list_body = list_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
        assert_eq!(list_json["object"], "list");
        let list_entry = list_json["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == run.id)
            .expect("source run missing from list response");
        assert_eq!(list_entry["id"], run.id);
        assert_eq!(
            list_json["mcp_admission_rejections"][run.id.as_str()]["code"],
            "disabled_by_operator_policy"
        );
        assert_eq!(
            list_json["cleanup_failures"][run.id.as_str()]["phase"],
            "terminal_cleanup"
        );
        assert_eq!(
            list_json["recovery_hints"][run.id.as_str()]["suggested_action"],
            "follow_replay"
        );
        assert_eq!(
            list_json["takeovers"][run.id.as_str()]["current_owner"]["worker_id"],
            "worker_gateway_takeover"
        );
        assert_eq!(
            list_json["takeovers"][run.id.as_str()]["current_owner"]["state"],
            "active"
        );
        assert_eq!(
            list_json["takeovers"][run.id.as_str()]["replay_run_id"],
            replay_child.id
        );
        assert_eq!(
            list_json["takeovers"][run.id.as_str()]["takeover_state"],
            "active"
        );
        assert_eq!(
            list_json["summaries"][run.id.as_str()]["continuation_checkpoint"]["kind"],
            "pending_tool_calls"
        );
        assert_eq!(
            list_json["summaries"][run.id.as_str()]["provider_call_fence"]["safe_resume_from"]["kind"],
            "pending_tool_calls"
        );
        assert_eq!(
            list_json["summaries"][run.id.as_str()]["artifact_continuity"]["latest_kind"],
            "assistant_output"
        );
        assert_eq!(
            list_json["summaries"][run.id.as_str()]["replay_child"]["latest_run_id"],
            replay_child.id
        );

        let get_request = Request::builder()
            .method(Method::GET)
            .uri(format!("/v1/runs/{}", run.id))
            .body(Body::empty())
            .unwrap();
        let get_response = app.oneshot(get_request).await.unwrap();
        assert_eq!(get_response.status(), StatusCode::OK);

        let get_body = get_response.into_body().collect().await.unwrap().to_bytes();
        let get_json: serde_json::Value = serde_json::from_slice(&get_body).unwrap();
        assert_eq!(get_json["run"]["id"], run.id);
        assert_eq!(get_json["run"]["status"], "interrupted");
        assert_eq!(
            get_json["mcp_admission_rejection"]["code"],
            "disabled_by_operator_policy"
        );
        assert_eq!(get_json["cleanup_failure"]["phase"], "terminal_cleanup");
        assert_eq!(
            get_json["recovery_hint"]["suggested_action"],
            "follow_replay"
        );
        assert_eq!(
            get_json["takeover"]["current_owner"]["worker_id"],
            "worker_gateway_takeover"
        );
        assert_eq!(get_json["takeover"]["replay_run_id"], replay_child.id);
        assert_eq!(get_json["takeover"]["takeover_state"], "active");
        assert_eq!(
            get_json["summary"]["continuation_checkpoint"]["kind"],
            "pending_tool_calls"
        );
        assert_eq!(
            get_json["summary"]["provider_call_fence"]["safe_resume_from"]["kind"],
            "pending_tool_calls"
        );
        assert_eq!(
            get_json["summary"]["artifact_continuity"]["latest_content_preview"],
            "Checkpointed answer"
        );
        assert_eq!(
            get_json["summary"]["replay_child"]["latest_run_id"],
            replay_child.id
        );
    }

    #[tokio::test]
    async fn test_managed_run_events_list() {
        let (_tmp, state, agent, _version) = build_managed_test_state().await;
        let managed = state.managed.clone().unwrap();

        let mut run = hermes_managed::ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.prompt = "Retry this run".to_string();
        run.session_id = Some("session_123".to_string());
        managed.store.create_run(&run).await.unwrap();
        let source_run_id = run.id.clone();
        managed
            .store
            .append_run_event(
                &run.id,
                &managed_mcp_admission_rejection_event(&ManagedMcpAdmissionRejection {
                    code: "disabled_by_operator_policy".to_string(),
                    error: "managed MCP tools are disabled by operator policy: mcp_resource_read"
                        .to_string(),
                    requested_tools: vec!["mcp_resource_read".to_string()],
                    requested_read_only_tools: vec!["mcp_resource_read".to_string()],
                    requested_side_effect_tools: vec![],
                    requested_dynamic_tools: vec![],
                    allowed_servers: vec![],
                    allowed_transports: vec![],
                    allow_side_effects: false,
                    allowed_stdio_servers: vec![],
                    allowed_stdio_env_keys: vec![],
                    stdio_server_summaries: vec![],
                    read_only_capability_attribution:
                        ManagedMcpReadOnlyCapabilityAttribution::default(),
                }),
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_artifact(
                &run.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::ToolOutput,
                    label: "read_file".to_string(),
                    tool_name: Some("read_file".to_string()),
                    tool_call_id: Some("call_read_file_1".to_string()),
                    content: "README contents".to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run created".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: None,
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::ToolProgress,
                    message: Some("reading README.md".to_string()),
                    tool_name: Some("read_file".to_string()),
                    tool_call_id: None,
                    metadata: None,
                },
            )
            .await
            .unwrap();
        managed
            .store
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
        managed
            .store
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
                        "request_history_len": 2,
                        "tool_count": 4,
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
        managed
            .store
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
        let mut replay_child = hermes_managed::ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        replay_child.status = ManagedRunStatus::Running;
        replay_child.prompt = run.prompt.clone();
        replay_child.session_id = run.session_id.clone();
        replay_child.replay_of_run_id = Some(source_run_id.clone());
        managed.store.create_run(&replay_child).await.unwrap();
        let claimed_at = Utc::now();
        managed
            .store
            .claim_run_ownership(
                &replay_child.id,
                "worker_gateway_takeover",
                "claim_gateway_takeover",
                claimed_at,
                claimed_at + chrono::Duration::seconds(30),
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunOwnershipClaimed,
                    message: Some(
                        "managed run ownership claimed by worker_gateway_takeover".to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "worker_id": "worker_gateway_takeover",
                        "claimed_at": claimed_at.to_rfc3339(),
                        "lease_expires_at": (claimed_at + chrono::Duration::seconds(30)).to_rfc3339(),
                        "takeover_lineage_id": replay_child.id.as_str(),
                    })),
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": source_run_id.as_str(),
                        "replay_root_run_id": source_run_id.as_str(),
                        "replay_depth": 1,
                        "replay_trigger": "interrupted_auto_replay",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                    })),
                },
            )
            .await
            .unwrap();

        let app = build_stateful_app(state);
        let request = Request::builder()
            .method(Method::GET)
            .uri(format!("/v1/runs/{}/events", run.id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["object"], "list");
        let kinds = json["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["kind"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(kinds.contains(&"run.mcp_admission_rejected".to_string()));
        assert!(kinds.contains(&"run.created".to_string()));
        assert!(kinds.contains(&"tool.progress".to_string()));
        let tool_progress = json["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["kind"] == "tool.progress")
            .expect("tool.progress event missing");
        assert_eq!(tool_progress["tool_name"], "read_file");
        assert_eq!(tool_progress["message"], "reading README.md");
        assert_eq!(
            json["mcp_admission_rejection"]["code"],
            "disabled_by_operator_policy"
        );
        assert_eq!(json["cleanup_failure"]["phase"], "terminal_cleanup");
        assert_eq!(json["recovery_hint"]["suggested_action"], "follow_replay");
        assert_eq!(
            json["takeover"]["current_owner"]["worker_id"],
            "worker_gateway_takeover"
        );
        assert_eq!(json["takeover"]["replay_run_id"], replay_child.id);
        assert_eq!(json["takeover"]["takeover_state"], "active");
        assert_eq!(
            json["summary"]["continuation_checkpoint"]["kind"],
            "user_checkpointed"
        );
        assert_eq!(
            json["summary"]["provider_call_fence"]["safe_resume_from"]["kind"],
            "user_checkpointed"
        );
        assert_eq!(
            json["summary"]["artifact_continuity"]["latest_tool_name"],
            "read_file"
        );
        assert_eq!(
            json["summary"]["replay_child"]["latest_run_id"],
            replay_child.id
        );
    }

    #[tokio::test]
    async fn test_managed_run_artifacts_list() {
        let (_tmp, state, agent, _version) = build_managed_test_state().await;
        let managed = state.managed.clone().unwrap();

        let run = hermes_managed::ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        managed.store.create_run(&run).await.unwrap();
        managed
            .store
            .append_run_artifact(
                &run.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::AssistantOutput,
                    label: "assistant_output".to_string(),
                    tool_name: None,
                    tool_call_id: None,
                    content: "Final answer".to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_artifact(
                &run.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::ToolOutput,
                    label: "browser".to_string(),
                    tool_name: Some("browser".to_string()),
                    tool_call_id: Some("call_browser_1".to_string()),
                    content: "{\"url\":\"https://example.com\"}".to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();

        let app = build_stateful_app(state);
        let request = Request::builder()
            .method(Method::GET)
            .uri(format!("/v1/runs/{}/artifacts", run.id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["object"], "list");
        assert_eq!(json["data"][0]["kind"], "assistant_output");
        assert_eq!(json["data"][0]["content"], "Final answer");
        assert_eq!(json["data"][1]["kind"], "tool_output");
        assert_eq!(json["data"][1]["tool_name"], "browser");
    }

    #[tokio::test]
    async fn test_managed_run_artifacts_list_with_lineage() {
        let (_tmp, state, agent, _version) = build_managed_test_state().await;
        let managed = state.managed.clone().unwrap();

        let root = hermes_managed::ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        managed.store.create_run(&root).await.unwrap();

        let mut replay = hermes_managed::ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        replay.replay_of_run_id = Some(root.id.clone());
        managed.store.create_run(&replay).await.unwrap();

        managed
            .store
            .append_run_artifact(
                &root.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::AssistantOutput,
                    label: "assistant_output".to_string(),
                    tool_name: None,
                    tool_call_id: None,
                    content: "from root".to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_artifact(
                &replay.id,
                &ManagedRunArtifactDraft {
                    kind: ManagedRunArtifactKind::AssistantOutput,
                    label: "assistant_output".to_string(),
                    tool_name: None,
                    tool_call_id: None,
                    content: "from replay".to_string(),
                    metadata: None,
                },
            )
            .await
            .unwrap();

        let app = build_stateful_app(state);
        let request = Request::builder()
            .method(Method::GET)
            .uri(format!("/v1/runs/{}/artifacts?lineage=true", replay.id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["object"], "list");
        assert_eq!(json["lineage_run_ids"][0], root.id);
        assert_eq!(json["lineage_run_ids"][1], replay.id);
        assert_eq!(json["data"][0]["run_id"], root.id);
        assert_eq!(json["data"][0]["content"], "from root");
        assert_eq!(json["data"][1]["run_id"], replay.id);
        assert_eq!(json["data"][1]["content"], "from replay");
    }

    #[tokio::test]
    async fn test_managed_run_cancel_endpoint_cancels_active_run() {
        let (_tmp, state, agent, _version) = build_managed_test_state().await;
        let managed = state.managed.clone().unwrap();

        let mut run = hermes_managed::ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        managed.store.create_run(&run).await.unwrap();
        let pid_file = NamedTempFile::new().unwrap();
        let script = format!(
            r#"
import pathlib
import subprocess
import time

pid_file = pathlib.Path(r"{pid_file}")
child = subprocess.Popen(["sleep", "30"])
pid_file.write_text(str(child.pid))
time.sleep(30)
"#,
            pid_file = pid_file.path().display(),
        );
        let mut child = tokio::process::Command::new("setsid")
            .arg("python3")
            .arg("-c")
            .arg(&script)
            .spawn()
            .unwrap();
        let process_group = child.id().unwrap();
        let descendant_pid = wait_for_pid_file(pid_file.path()).await;
        let _cleanup_registration = hermes_tools::session_cleanup::register_process_group(
            &run.id,
            process_group,
            "managed cancel test",
        )
        .unwrap();
        managed
            .runs
            .register(
                &run,
                60,
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }),
            )
            .unwrap();

        let app = Router::new()
            .route(
                "/v1/runs/{id}",
                axum::routing::delete(handle_managed_run_cancel),
            )
            .with_state(state);

        let request = Request::builder()
            .method(Method::DELETE)
            .uri(format!("/v1/runs/{}", run.id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["run"]["status"], "cancelled");
        assert!(json["run"]["cancel_requested_at"].is_string());

        let stored = managed.store.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(stored.status, ManagedRunStatus::Cancelled);
        wait_for_run_eviction(managed.runs.as_ref(), &run.id).await;
        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("cleanup should kill registered child")
            .expect("child wait should succeed");
        assert!(!status.success());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !pid_is_alive(descendant_pid),
            "descendant pid {descendant_pid} should be terminated with the managed process group"
        );
    }

    #[tokio::test]
    async fn test_managed_run_cancel_endpoint_runs_async_cleanup() {
        let (_tmp, state, agent, _version) = build_managed_test_state().await;
        let managed = state.managed.clone().unwrap();

        let mut run = hermes_managed::ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        managed.store.create_run(&run).await.unwrap();

        let cleaned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cleaned_flag = Arc::clone(&cleaned);
        let _cleanup_registration = hermes_tools::session_cleanup::register_async_cleanup(
            &run.id,
            "managed async test",
            move || {
                let cleaned_flag = Arc::clone(&cleaned_flag);
                async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    cleaned_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                }
            },
        );

        managed
            .runs
            .register(
                &run,
                60,
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }),
            )
            .unwrap();

        let app = Router::new()
            .route(
                "/v1/runs/{id}",
                axum::routing::delete(handle_managed_run_cancel),
            )
            .with_state(state);

        let request = Request::builder()
            .method(Method::DELETE)
            .uri(format!("/v1/runs/{}", run.id))
            .body(Body::empty())
            .unwrap();

        let started = std::time::Instant::now();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            started.elapsed() >= Duration::from_millis(80),
            "expected response to wait for async cleanup"
        );
        assert!(cleaned.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_managed_run_cancel_endpoint_follows_active_replay_takeover() {
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(AppConfig::default(), |_| {}).await;
        let managed = state.managed.clone().unwrap();

        let mut source_run =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        source_run.status = ManagedRunStatus::Interrupted;
        source_run.prompt = "resume me".to_string();
        managed.store.create_run(&source_run).await.unwrap();
        let source_run_id = source_run.id.clone();

        let mut replay_child =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        replay_child.status = ManagedRunStatus::Running;
        replay_child.prompt = source_run.prompt.clone();
        replay_child.replay_of_run_id = Some(source_run_id.clone());
        managed.store.create_run(&replay_child).await.unwrap();
        let claimed_at = Utc::now();
        managed
            .store
            .claim_run_ownership(
                &replay_child.id,
                "worker_cancel_takeover",
                "claim_cancel_takeover",
                claimed_at,
                claimed_at + chrono::Duration::seconds(30),
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunOwnershipClaimed,
                    message: Some(
                        "managed run ownership claimed by worker_cancel_takeover".to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "worker_id": "worker_cancel_takeover",
                        "claimed_at": claimed_at.to_rfc3339(),
                        "lease_expires_at": (claimed_at + chrono::Duration::seconds(30)).to_rfc3339(),
                        "takeover_lineage_id": replay_child.id.as_str(),
                    })),
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": source_run_id.as_str(),
                        "replay_root_run_id": source_run_id.as_str(),
                        "replay_depth": 1,
                        "replay_trigger": "interrupted_auto_replay",
                        "takeover_lineage_id": replay_child.id.as_str(),
                    })),
                },
            )
            .await
            .unwrap();

        managed
            .runs
            .register_owned(
                &replay_child,
                60,
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }),
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }),
                "worker_cancel_takeover".to_string(),
                "claim_cancel_takeover".to_string(),
            )
            .unwrap();

        let app = Router::new()
            .route(
                "/v1/runs/{id}",
                axum::routing::delete(handle_managed_run_cancel),
            )
            .with_state(state);

        let request = Request::builder()
            .method(Method::DELETE)
            .uri(format!("/v1/runs/{}", source_run.id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["run"]["id"], source_run.id);
        assert_eq!(json["run"]["status"], "interrupted");
        assert_eq!(json["takeover"]["replay_run_id"], replay_child.id);
        assert_eq!(json["takeover"]["takeover_state"], "cancelled");

        let cancelled_child = wait_for_run_status(
            managed.store.as_ref(),
            &replay_child.id,
            ManagedRunStatus::Cancelled,
        )
        .await;
        assert_eq!(
            cancelled_child.last_error.as_deref(),
            Some("cancelled via API")
        );
        wait_for_run_eviction(managed.runs.as_ref(), &replay_child.id).await;

        let source_summary = hermes_managed::load_managed_run_derived_summary(
            managed.store.as_ref(),
            &source_run.id,
        )
        .await
        .unwrap();
        assert_eq!(
            source_summary
                .takeover
                .as_ref()
                .map(|takeover| takeover.takeover_state),
            Some(hermes_managed::ManagedRunTakeoverState::Cancelled)
        );
    }

    #[tokio::test]
    async fn test_managed_run_cancel_missing_returns_404() {
        let (_tmp, state, _agent, _version) = build_managed_test_state().await;
        let app = Router::new()
            .route(
                "/v1/runs/{id}",
                axum::routing::delete(handle_managed_run_cancel),
            )
            .with_state(state);

        let request = Request::builder()
            .method(Method::DELETE)
            .uri("/v1/runs/run_missing")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_terminate_managed_run_timeout_does_not_mark_cancel_requested() {
        let _env_guard = ENV_LOCK.lock().await;
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(AppConfig::default(), |_| {}).await;
        let managed = state.managed.clone().unwrap();

        let mut run = hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        run.status = ManagedRunStatus::Running;
        managed.store.create_run(&run).await.unwrap();

        terminate_managed_run(
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            run.id.clone(),
            ManagedRunStatus::TimedOut,
            Some("managed run timed out after 5s".to_string()),
        )
        .await;

        let stored = managed.store.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(stored.status, ManagedRunStatus::TimedOut);
        assert!(stored.cancel_requested_at.is_none());
        assert_eq!(
            stored.last_error.as_deref(),
            Some("managed run timed out after 5s")
        );

        let events = managed.store.list_run_events(&run.id, 10).await.unwrap();
        let terminal_event = events
            .iter()
            .find(|event| event.kind == ManagedRunEventKind::RunTimedOut)
            .expect("timed out event missing");
        assert_eq!(
            terminal_event.message.as_deref(),
            Some("managed run timed out after 5s")
        );
    }

    #[tokio::test]
    async fn test_managed_run_replay_creates_new_run_from_original_prompt() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let Some((base_url, _audits, server_handle)) = spawn_mock_managed_provider_server(
            MockManagedProviderProtocol::OpenAi,
            "Hello from replay",
        )
        .await
        else {
            return;
        };
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.base_url = Some(base_url.clone());
                version.timeout_secs = 5;
            })
            .await;
        let managed = state.managed.clone().unwrap();

        let mut source_run =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        source_run.status = ManagedRunStatus::Completed;
        source_run.prompt = "Re-run this review".to_string();
        managed.store.create_run(&source_run).await.unwrap();
        managed
            .store
            .append_run_event(
                &source_run.id,
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

        let app = Router::new()
            .route("/v1/runs/{id}/replay", post(handle_managed_run_replay))
            .with_state(state);

        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/runs/{}/replay", source_run.id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let replay_run_id = json["run"]["id"].as_str().unwrap().to_string();
        assert_ne!(replay_run_id, source_run.id);
        assert_eq!(json["run"]["replay_of_run_id"], source_run.id);
        assert_eq!(json["run"]["prompt"], "Re-run this review");

        let replayed = wait_for_run_status(
            managed.store.as_ref(),
            &replay_run_id,
            ManagedRunStatus::Completed,
        )
        .await;
        assert_eq!(replayed.prompt, "Re-run this review");
        assert_eq!(
            replayed.replay_of_run_id.as_deref(),
            Some(source_run.id.as_str())
        );
        wait_for_run_eviction(managed.runs.as_ref(), &replay_run_id).await;

        let events = managed
            .store
            .list_run_events(&replay_run_id, 10)
            .await
            .unwrap();
        assert_eq!(events[0].kind, ManagedRunEventKind::RunCreated);
        assert_eq!(
            events[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_of_run_id"))
                .and_then(|value| value.as_str()),
            Some(source_run.id.as_str())
        );
        assert_eq!(
            events[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_trigger"))
                .and_then(|value| value.as_str()),
            Some("manual_replay")
        );
        assert_eq!(
            events[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_depth"))
                .and_then(|value| value.as_u64()),
            Some(1)
        );
        assert_eq!(
            events[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_source_boundary"))
                .and_then(|value| value.as_str()),
            Some("pending_tool_calls")
        );
        let takeover_event = events
            .iter()
            .find(|event| event.kind == ManagedRunEventKind::RunTakeoverEstablished)
            .expect("replay takeover event missing");
        let ownership_event = events
            .iter()
            .find(|event| event.kind == ManagedRunEventKind::RunOwnershipClaimed)
            .expect("replay child ownership claim event missing");
        assert_eq!(
            takeover_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_worker_id"))
                .and_then(|value| value.as_str()),
            Some("gw_test")
        );
        assert!(
            ownership_event.id < takeover_event.id,
            "replay child should claim ownership before takeover is established"
        );
        let source_events = managed
            .store
            .list_run_events(&source_run.id, 10)
            .await
            .unwrap();
        let source_replay_event = source_events
            .iter()
            .find(|event| event.kind == ManagedRunEventKind::RunReplayed)
            .expect("source replay event missing");
        assert_eq!(
            source_replay_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_run_id"))
                .and_then(|value| value.as_str()),
            Some(replay_run_id.as_str())
        );
        assert_eq!(
            source_replay_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_run_status"))
                .and_then(|value| value.as_str()),
            Some("running")
        );
        assert_eq!(
            source_replay_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_trigger"))
                .and_then(|value| value.as_str()),
            Some("manual_replay")
        );
        assert_eq!(
            source_replay_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_source_boundary"))
                .and_then(|value| value.as_str()),
            Some("pending_tool_calls")
        );
        let source_recovery_decision = source_events
            .iter()
            .find(|event| event.kind == ManagedRunEventKind::RunRecoveryDecision)
            .expect("source recovery decision missing");
        assert_eq!(
            source_recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("decision"))
                .and_then(|value| value.as_str()),
            Some("follow_replay")
        );
        assert_eq!(
            source_recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("reason"))
                .and_then(|value| value.as_str()),
            Some("replay_child_active")
        );
        assert_eq!(
            source_recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_run_id"))
                .and_then(|value| value.as_str()),
            Some(replay_run_id.as_str())
        );
        assert_eq!(
            source_recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("evaluated_by_worker_id"))
                .and_then(|value| value.as_str()),
            Some("gw_test")
        );
        assert_eq!(
            source_recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_worker_id"))
                .and_then(|value| value.as_str()),
            Some("gw_test")
        );

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_managed_run_replay_build_failure_does_not_emit_premature_takeover_events() {
        let _env_guard = ENV_LOCK.lock().await;
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.allowed_tools = vec!["mcp_resource_read".to_string()];
            })
            .await;
        let managed = state.managed.clone().unwrap();

        let mut source_run =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        source_run.status = ManagedRunStatus::Completed;
        source_run.prompt = "Re-run this review".to_string();
        managed.store.create_run(&source_run).await.unwrap();

        let app = Router::new()
            .route("/v1/runs/{id}/replay", post(handle_managed_run_replay))
            .with_state(state);

        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/runs/{}/replay", source_run.id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["error"]["code"].as_str(),
            Some("disabled_by_operator_policy")
        );

        let replay_run = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(run) = managed
                    .store
                    .list_runs(10)
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|run| run.replay_of_run_id.as_deref() == Some(source_run.id.as_str()))
                {
                    break run;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(replay_run.status, ManagedRunStatus::Failed);
        assert!(
            replay_run
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("managed MCP tools are disabled by operator policy")
        );

        let replay_events = managed
            .store
            .list_run_events(&replay_run.id, 10)
            .await
            .unwrap();
        assert!(
            replay_events
                .iter()
                .all(|event| event.kind != ManagedRunEventKind::RunOwnershipClaimed),
            "replay child should not claim ownership after build failure"
        );
        assert!(
            replay_events
                .iter()
                .all(|event| event.kind != ManagedRunEventKind::RunTakeoverEstablished),
            "replay child should not emit takeover_established after build failure"
        );

        let source_events = managed
            .store
            .list_run_events(&source_run.id, 10)
            .await
            .unwrap();
        assert!(
            source_events
                .iter()
                .all(|event| event.kind != ManagedRunEventKind::RunReplayed),
            "source run should not emit run.replayed when replay child never claimed ownership"
        );
        assert!(
            source_events.iter().all(|event| {
                event.kind != ManagedRunEventKind::RunRecoveryDecision
                    || event
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get("decision"))
                        .and_then(|value| value.as_str())
                        != Some("follow_replay")
            }),
            "source run should not emit follow_replay when replay child never claimed ownership"
        );
        let source_summary = hermes_managed::load_managed_run_derived_summary(
            managed.store.as_ref(),
            &source_run.id,
        )
        .await
        .unwrap();
        assert!(
            source_summary.replay_child.is_none(),
            "source summary should not surface replay_child before takeover is established"
        );
        assert!(
            source_summary.takeover.is_none(),
            "source summary should not surface takeover before child ownership is established"
        );
    }

    #[tokio::test]
    async fn test_managed_run_replay_from_ancestor_follows_latest_terminal_leaf() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let Some((base_url, _audits, server_handle)) = spawn_mock_managed_provider_server(
            MockManagedProviderProtocol::OpenAi,
            "Hello from terminal leaf replay",
        )
        .await
        else {
            return;
        };
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.base_url = Some(base_url.clone());
                version.timeout_secs = 5;
            })
            .await;
        let managed = state.managed.clone().unwrap();

        let mut source_run =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        source_run.status = ManagedRunStatus::Interrupted;
        source_run.session_id = Some("managed_session_stale_source".to_string());
        managed.store.create_run(&source_run).await.unwrap();

        let mut failed_leaf =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        failed_leaf.status = ManagedRunStatus::Failed;
        failed_leaf.prompt = "resume the latest replay leaf".to_string();
        failed_leaf.session_id = Some("managed_session_latest_leaf".to_string());
        failed_leaf.replay_of_run_id = Some(source_run.id.clone());
        managed.store.create_run(&failed_leaf).await.unwrap();
        managed
            .store
            .append_run_event(
                &failed_leaf.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": source_run.id.as_str(),
                        "replay_root_run_id": source_run.id.as_str(),
                        "replay_depth": 1,
                        "replay_trigger": "manual_replay",
                        "reused_session_id": true,
                        "resumed_existing_turn": false,
                    })),
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &source_run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunRecoveryDecision,
                    message: Some(format!(
                        "continuation is owned by replay child {}",
                        failed_leaf.id
                    )),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "decision": "follow_replay",
                        "reason": "replay_child_active",
                        "replay_run_id": failed_leaf.id.as_str(),
                        "evaluated_by_worker_id": "worker_eval_terminal_leaf",
                        "takeover_worker_id": "worker_terminal_leaf",
                        "worker_id": "worker_terminal_leaf",
                        "note": "follow the terminal replay leaf lineage instead of branching from the stale ancestor source",
                    })),
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &source_run.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunReplayed,
                    message: Some(format!("managed run replayed as {}", failed_leaf.id)),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_run_id": failed_leaf.id.as_str(),
                        "replay_run_status": "failed",
                        "replay_root_run_id": source_run.id.as_str(),
                        "replay_trigger": "manual_replay",
                        "reused_session_id": true,
                        "resumed_existing_turn": false,
                    })),
                },
            )
            .await
            .unwrap();

        let app = Router::new()
            .route("/v1/runs/{id}/replay", post(handle_managed_run_replay))
            .with_state(state);

        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/runs/{}/replay", source_run.id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let replay_run_id = json["run"]["id"].as_str().unwrap().to_string();
        assert_ne!(replay_run_id, source_run.id);
        assert_ne!(replay_run_id, failed_leaf.id);
        assert_eq!(json["run"]["replay_of_run_id"], failed_leaf.id);
        assert_eq!(json["run"]["prompt"], failed_leaf.prompt);
        assert_eq!(
            json["run"]["session_id"].as_str(),
            failed_leaf.session_id.as_deref()
        );

        let replayed = wait_for_run_status(
            managed.store.as_ref(),
            &replay_run_id,
            ManagedRunStatus::Completed,
        )
        .await;
        assert_eq!(
            replayed.replay_of_run_id.as_deref(),
            Some(failed_leaf.id.as_str())
        );
        assert_eq!(replayed.prompt, failed_leaf.prompt);
        assert_eq!(
            replayed.session_id.as_deref(),
            failed_leaf.session_id.as_deref()
        );
        wait_for_run_eviction(managed.runs.as_ref(), &replay_run_id).await;

        let replay_events = managed
            .store
            .list_run_events(&replay_run_id, 10)
            .await
            .unwrap();
        assert_eq!(replay_events[0].kind, ManagedRunEventKind::RunCreated);
        assert_eq!(
            replay_events[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_of_run_id"))
                .and_then(|value| value.as_str()),
            Some(failed_leaf.id.as_str())
        );
        assert_eq!(
            replay_events[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_root_run_id"))
                .and_then(|value| value.as_str()),
            Some(source_run.id.as_str())
        );
        assert_eq!(
            replay_events[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_depth"))
                .and_then(|value| value.as_u64()),
            Some(2)
        );

        let source_summary = hermes_managed::load_managed_run_derived_summary(
            managed.store.as_ref(),
            &source_run.id,
        )
        .await
        .unwrap();
        assert_eq!(
            source_summary
                .takeover
                .as_ref()
                .map(|takeover| takeover.replay_run_id.as_str()),
            Some(replay_run_id.as_str())
        );
        assert_eq!(
            source_summary
                .takeover
                .as_ref()
                .map(|takeover| takeover.lineage_depth),
            Some(2)
        );

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_managed_run_replay_blocks_when_requested_run_is_still_active() {
        let _env_guard = ENV_LOCK.lock().await;
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(AppConfig::default(), |_| {}).await;
        let managed = state.managed.clone().unwrap();

        let mut active_run =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        active_run.status = ManagedRunStatus::Pending;
        active_run.prompt = "still running".to_string();
        managed.store.create_run(&active_run).await.unwrap();
        let claimed_at = Utc::now();
        let claimed = managed
            .store
            .claim_run_ownership(
                &active_run.id,
                "worker_active_source",
                "claim_active_source",
                claimed_at,
                claimed_at + chrono::Duration::seconds(30),
            )
            .await
            .unwrap();
        assert!(claimed);

        let app = Router::new()
            .route("/v1/runs/{id}/replay", post(handle_managed_run_replay))
            .with_state(state);

        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/runs/{}/replay", active_run.id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let expected_error = format!(
            "managed run is still active and cannot be replayed: {}",
            active_run.id
        );
        assert_eq!(json["error"].as_str(), Some(expected_error.as_str()));

        let runs = managed.store.list_runs(10).await.unwrap();
        assert_eq!(
            runs.len(),
            1,
            "active run replay should not create a child run"
        );
        assert_eq!(runs[0].id, active_run.id);

        let latest_decision = managed
            .store
            .get_latest_run_event_by_kind(&active_run.id, ManagedRunEventKind::RunRecoveryDecision)
            .await
            .unwrap()
            .expect("active run replay block should persist a recovery decision");
        assert_eq!(
            latest_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("decision"))
                .and_then(|value| value.as_str()),
            Some("blocked")
        );
        assert_eq!(
            latest_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("reason"))
                .and_then(|value| value.as_str()),
            Some("run_still_active")
        );
        assert_eq!(
            latest_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("active_follow_target_run_id"))
                .and_then(|value| value.as_str()),
            Some(active_run.id.as_str())
        );
        assert_eq!(
            latest_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_worker_id"))
                .and_then(|value| value.as_str()),
            Some("worker_active_source")
        );
    }

    #[tokio::test]
    async fn test_managed_run_replay_blocks_when_active_replay_continuation_exists() {
        let _env_guard = ENV_LOCK.lock().await;
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(AppConfig::default(), |_| {}).await;
        let managed = state.managed.clone().unwrap();

        let mut source_run =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        source_run.status = ManagedRunStatus::Interrupted;
        source_run.prompt = "resume me".to_string();
        managed.store.create_run(&source_run).await.unwrap();
        let source_run_id = source_run.id.clone();

        let mut active_child =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        active_child.status = ManagedRunStatus::Running;
        active_child.prompt = source_run.prompt.clone();
        active_child.replay_of_run_id = Some(source_run_id.clone());
        managed.store.create_run(&active_child).await.unwrap();
        let claimed_at = Utc::now();
        managed
            .store
            .claim_run_ownership(
                &active_child.id,
                "worker_existing_takeover",
                "claim_existing_takeover",
                claimed_at,
                claimed_at + chrono::Duration::seconds(30),
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &active_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunOwnershipClaimed,
                    message: Some(
                        "managed run ownership claimed by worker_existing_takeover".to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "worker_id": "worker_existing_takeover",
                        "claimed_at": claimed_at.to_rfc3339(),
                        "lease_expires_at": (claimed_at + chrono::Duration::seconds(30)).to_rfc3339(),
                        "takeover_lineage_id": active_child.id.as_str(),
                    })),
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &active_child.id,
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
                        "takeover_lineage_id": active_child.id.as_str(),
                    })),
                },
            )
            .await
            .unwrap();

        let app = Router::new()
            .route("/v1/runs/{id}/replay", post(handle_managed_run_replay))
            .with_state(state);

        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/runs/{}/replay", source_run.id))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("active replay continuation")
        );

        let source_events = managed
            .store
            .list_run_events(&source_run.id, 10)
            .await
            .unwrap();
        let blocked = source_events
            .iter()
            .find(|event| {
                event.kind == ManagedRunEventKind::RunRecoveryDecision
                    && event
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get("decision"))
                        .and_then(|value| value.as_str())
                        == Some("blocked")
            })
            .expect("blocked recovery decision missing");
        assert_eq!(
            blocked
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("reason"))
                .and_then(|value| value.as_str()),
            Some("replay_child_active")
        );
        assert_eq!(
            blocked
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("active_follow_target_run_id"))
                .and_then(|value| value.as_str()),
            Some(active_child.id.as_str())
        );

        let replay_children: Vec<_> = managed
            .store
            .list_runs(10)
            .await
            .unwrap()
            .into_iter()
            .filter(|run| run.replay_of_run_id.as_deref() == Some(source_run.id.as_str()))
            .collect();
        assert_eq!(
            replay_children.len(),
            1,
            "manual replay should not create a second replay child when takeover is active"
        );
    }

    #[tokio::test]
    async fn auto_replay_interrupted_runs_creates_replay_run_when_enabled() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let Some((base_url, _audits, server_handle)) = spawn_mock_managed_provider_server(
            MockManagedProviderProtocol::OpenAi,
            "Recovered by replay",
        )
        .await
        else {
            return;
        };
        let app_config = AppConfig {
            managed: ManagedConfigYaml {
                recovery: ManagedRecoveryPolicyYaml {
                    auto_replay_interrupted: true,
                    max_auto_replays_per_root_run: 3,
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(app_config, |version| {
                version.base_url = Some(base_url.clone());
                version.timeout_secs = 5;
            })
            .await;
        let managed = state.managed.clone().unwrap();

        let mut source_run =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        source_run.status = ManagedRunStatus::Interrupted;
        source_run.prompt = "Try again after restart".to_string();
        source_run.session_id = Some("managed_session_123".to_string());
        managed.store.create_run(&source_run).await.unwrap();
        managed
            .store
            .append_run_event(
                &source_run.id,
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
                    })),
                },
            )
            .await
            .unwrap();

        let summary = maybe_auto_replay_interrupted_runs(
            Arc::clone(&managed.shared),
            managed.app_config.clone(),
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            managed.worker_id.clone(),
            10,
        )
        .await
        .unwrap();

        assert_eq!(summary.candidates, 1);
        assert_eq!(summary.replayed_run_ids.len(), 1);
        assert_eq!(summary.skipped_depth_limit, 0);
        assert!(summary.failures.is_empty());

        let replayed = wait_for_run_status(
            managed.store.as_ref(),
            &summary.replayed_run_ids[0],
            ManagedRunStatus::Completed,
        )
        .await;
        assert_eq!(replayed.prompt, source_run.prompt);
        assert_eq!(
            replayed.replay_of_run_id.as_deref(),
            Some(source_run.id.as_str())
        );
        assert_eq!(
            replayed.session_id.as_deref(),
            source_run.session_id.as_deref()
        );
        let replay_events = managed
            .store
            .list_run_events(&replayed.id, 10)
            .await
            .unwrap();
        assert_eq!(
            replay_events[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_trigger"))
                .and_then(|value| value.as_str()),
            Some("interrupted_auto_replay")
        );
        assert_eq!(
            replay_events[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_source_status"))
                .and_then(|value| value.as_str()),
            Some("interrupted")
        );
        assert_eq!(
            replay_events[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_source_interruption_cause"))
                .and_then(|value| value.as_str()),
            Some("lease_expired")
        );
        assert_eq!(
            replay_events[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_lineage_id"))
                .and_then(|value| value.as_str()),
            Some(replayed.id.as_str())
        );
        let takeover_event = replay_events
            .iter()
            .find(|event| event.kind == ManagedRunEventKind::RunTakeoverEstablished)
            .expect("replay takeover event missing");
        assert_eq!(
            takeover_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("source_run_id"))
                .and_then(|value| value.as_str()),
            Some(source_run.id.as_str())
        );
        assert_eq!(
            takeover_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("evaluated_by_worker_id"))
                .and_then(|value| value.as_str()),
            Some("gw_test")
        );
        assert_eq!(
            takeover_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_worker_id"))
                .and_then(|value| value.as_str()),
            Some("gw_test")
        );
        assert_eq!(
            takeover_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_lineage_id"))
                .and_then(|value| value.as_str()),
            Some(replayed.id.as_str())
        );
        let ownership_event = replay_events
            .iter()
            .find(|event| event.kind == ManagedRunEventKind::RunOwnershipClaimed)
            .expect("replay ownership event missing");
        assert_eq!(
            ownership_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_lineage_id"))
                .and_then(|value| value.as_str()),
            Some(replayed.id.as_str())
        );
        let replay_summary =
            hermes_managed::load_managed_run_derived_summary(managed.store.as_ref(), &replayed.id)
                .await
                .unwrap();
        assert_eq!(
            replay_summary
                .continuation
                .as_ref()
                .map(|continuation| continuation.source_run_id.as_str()),
            Some(source_run.id.as_str())
        );
        assert_eq!(
            replay_summary
                .continuation
                .as_ref()
                .and_then(|continuation| continuation.takeover_worker_id.as_deref()),
            Some("gw_test")
        );
        assert_eq!(
            replay_summary
                .continuation
                .as_ref()
                .and_then(|continuation| continuation.takeover_lineage_id.as_deref()),
            Some(replayed.id.as_str())
        );
        assert_eq!(
            replay_summary
                .continuation
                .as_ref()
                .and_then(|continuation| continuation.source_interruption_cause),
            Some(ManagedRunInterruptionCause::LeaseExpired)
        );
        let source_events = managed
            .store
            .list_run_events(&source_run.id, 10)
            .await
            .unwrap();
        let source_replay_event = source_events
            .iter()
            .find(|event| event.kind == ManagedRunEventKind::RunReplayed)
            .expect("source replay event missing");
        assert_eq!(
            source_replay_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_run_id"))
                .and_then(|value| value.as_str()),
            Some(replayed.id.as_str())
        );
        assert_eq!(
            source_replay_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_trigger"))
                .and_then(|value| value.as_str()),
            Some("interrupted_auto_replay")
        );
        assert_eq!(
            source_replay_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_source_interruption_cause"))
                .and_then(|value| value.as_str()),
            Some("lease_expired")
        );
        assert_eq!(
            source_replay_event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_lineage_id"))
                .and_then(|value| value.as_str()),
            Some(replayed.id.as_str())
        );
        let recovery_decision = source_events
            .iter()
            .find(|event| event.kind == ManagedRunEventKind::RunRecoveryDecision)
            .expect("source recovery decision missing");
        assert_eq!(
            recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("decision"))
                .and_then(|value| value.as_str()),
            Some("follow_replay")
        );
        assert_eq!(
            recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("reason"))
                .and_then(|value| value.as_str()),
            Some("replay_child_active")
        );
        assert_eq!(
            recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_run_id"))
                .and_then(|value| value.as_str()),
            Some(replayed.id.as_str())
        );
        assert_eq!(
            recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("evaluated_by_worker_id"))
                .and_then(|value| value.as_str()),
            Some("gw_test")
        );
        assert_eq!(
            recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_worker_id"))
                .and_then(|value| value.as_str()),
            Some("gw_test")
        );
        assert_eq!(
            recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_lineage_id"))
                .and_then(|value| value.as_str()),
            Some(replayed.id.as_str())
        );
        let takeover_updates: Vec<_> = source_events
            .iter()
            .filter(|event| event.kind == ManagedRunEventKind::RunTakeoverUpdated)
            .collect();
        assert_eq!(takeover_updates.len(), 2);
        assert!(takeover_updates.iter().any(|event| {
            event
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_state"))
                .and_then(|value| value.as_str())
                == Some("active")
        }));
        let terminal_takeover_update = takeover_updates
            .iter()
            .find(|event| {
                event
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("takeover_state"))
                    .and_then(|value| value.as_str())
                    == Some("completed")
            })
            .expect("terminal source takeover update missing");
        assert_eq!(
            terminal_takeover_update
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_run_id"))
                .and_then(|value| value.as_str()),
            Some(replayed.id.as_str())
        );
        assert_eq!(
            terminal_takeover_update
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_run_status"))
                .and_then(|value| value.as_str()),
            Some("completed")
        );
        assert_eq!(
            terminal_takeover_update
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("source_interruption_cause"))
                .and_then(|value| value.as_str()),
            Some("lease_expired")
        );
        wait_for_run_eviction(managed.runs.as_ref(), &replayed.id).await;

        server_handle.abort();
    }

    #[tokio::test]
    async fn auto_replay_interrupted_runs_respects_depth_limit() {
        let app_config = AppConfig {
            managed: ManagedConfigYaml {
                recovery: ManagedRecoveryPolicyYaml {
                    auto_replay_interrupted: true,
                    max_auto_replays_per_root_run: 1,
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(app_config, |_| {}).await;
        let managed = state.managed.clone().unwrap();

        let mut root_run =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        root_run.status = ManagedRunStatus::Completed;
        root_run.prompt = "root".to_string();
        managed.store.create_run(&root_run).await.unwrap();

        let mut interrupted =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        interrupted.status = ManagedRunStatus::Interrupted;
        interrupted.prompt = "leaf".to_string();
        interrupted.replay_of_run_id = Some(root_run.id.clone());
        managed.store.create_run(&interrupted).await.unwrap();

        let summary = maybe_auto_replay_interrupted_runs(
            Arc::clone(&managed.shared),
            managed.app_config.clone(),
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            managed.worker_id.clone(),
            10,
        )
        .await
        .unwrap();

        assert_eq!(summary.candidates, 1);
        assert!(summary.replayed_run_ids.is_empty());
        assert_eq!(summary.skipped_depth_limit, 1);
        assert!(summary.failures.is_empty());
        assert_eq!(managed.store.list_runs(10).await.unwrap().len(), 2);
        let events = managed
            .store
            .list_run_events(&interrupted.id, 10)
            .await
            .unwrap();
        let recovery_decision = events
            .iter()
            .find(|event| event.kind == ManagedRunEventKind::RunRecoveryDecision)
            .expect("recovery decision event missing");
        assert_eq!(
            recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("decision"))
                .and_then(|value| value.as_str()),
            Some("blocked")
        );
        assert_eq!(
            recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("reason"))
                .and_then(|value| value.as_str()),
            Some("depth_limit_reached")
        );
    }

    #[tokio::test]
    async fn append_source_takeover_update_for_replay_child_dedupes_interrupted_lineage() {
        let (_tmp, state, agent, version) = build_managed_test_state().await;
        let managed = state.managed.clone().unwrap();

        let mut source_run =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        source_run.status = ManagedRunStatus::Interrupted;
        source_run.prompt = "Resume this investigation".to_string();
        source_run.session_id = Some("managed_session_takeover".to_string());
        managed.store.create_run(&source_run).await.unwrap();

        let mut replay_child =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        replay_child.status = ManagedRunStatus::Interrupted;
        replay_child.prompt = source_run.prompt.clone();
        replay_child.session_id = source_run.session_id.clone();
        replay_child.replay_of_run_id = Some(source_run.id.clone());
        managed.store.create_run(&replay_child).await.unwrap();
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": source_run.id.as_str(),
                        "replay_root_run_id": source_run.id.as_str(),
                        "replay_depth": 1,
                        "replay_trigger": "interrupted_auto_replay",
                        "replay_trigger_worker_id": "gw_eval",
                        "replay_source_status": "interrupted",
                        "replay_source_interruption_cause": "lease_expired",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                        "replay_source_boundary": "tool_results_checkpointed",
                    })),
                },
            )
            .await
            .unwrap();
        managed
            .store
            .claim_run_ownership(
                &replay_child.id,
                "gw_takeover",
                "claim_takeover_leaf",
                chrono::DateTime::parse_from_rfc3339("2026-04-23T12:08:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
                chrono::DateTime::parse_from_rfc3339("2026-04-23T12:09:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunOwnershipClaimed,
                    message: Some("managed run ownership claimed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "worker_id": "gw_takeover",
                        "claimed_at": "2026-04-23T12:08:00Z",
                        "lease_expires_at": "2026-04-23T12:09:00Z",
                        "takeover_lineage_id": replay_child.id.as_str(),
                    })),
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunTakeoverEstablished,
                    message: Some("managed replay takeover established".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(
                        serde_json::to_value(&ManagedRunContinuationSummary {
                            source_run_id: source_run.id.clone(),
                            root_run_id: source_run.id.clone(),
                            replay_depth: 1,
                            trigger: hermes_managed::ManagedRunReplayTrigger::InterruptedAutoReplay,
                            takeover_lineage_id: Some(replay_child.id.clone()),
                            source_status: Some(ManagedRunStatus::Interrupted),
                            source_interruption_cause: Some(ManagedRunInterruptionCause::LeaseExpired),
                            reused_session_id: true,
                            resumed_existing_turn: true,
                            source_boundary: Some(
                                hermes_managed::ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed,
                            ),
                            evaluated_by_worker_id: Some("gw_eval".to_string()),
                            takeover_worker_id: Some("gw_takeover".to_string()),
                            note: Some("replay child took over".to_string()),
                        })
                        .unwrap(),
                    ),
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &replay_child.id,
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
        managed
            .store
            .append_run_event(
                &replay_child.id,
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
        managed
            .store
            .append_run_event(
                &replay_child.id,
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
        managed
            .store
            .append_run_event(
                &replay_child.id,
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
        managed
            .store
            .append_run_event(
                &replay_child.id,
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
        managed
            .store
            .append_run_event(
                &replay_child.id,
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
        managed
            .store
            .append_run_event(
                &replay_child.id,
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
                        "evaluated_by_worker_id": "gw_leaf_eval",
                        "worker_id": "gw_leaf_eval",
                        "source_boundary": "tool_results_checkpointed",
                        "note": "leaf continuation now requires manual review",
                    })),
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunTakeoverAssessed,
                    message: Some(
                        "worker gw_leaf_eval assessed interrupted run takeover with 1 blocking runtime risk"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "evaluated_by_worker_id": "gw_leaf_eval",
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
        managed
            .store
            .append_run_event(
                &replay_child.id,
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
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunOwnershipReleased,
                    message: Some(
                        "worker gw_takeover released managed run ownership after it lost ownership when the run was interrupted"
                            .to_string(),
                    ),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "worker_id": "gw_takeover",
                        "reason": "interrupted",
                        "note": "leaf owner released ownership after interruption",
                    })),
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_artifact(
                &replay_child.id,
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

        append_source_takeover_update_for_replay_child(managed.store.as_ref(), &replay_child, None)
            .await;
        append_source_takeover_update_for_replay_child(managed.store.as_ref(), &replay_child, None)
            .await;

        let source_events = managed
            .store
            .list_run_events(&source_run.id, 10)
            .await
            .unwrap();
        let takeover_updates: Vec<_> = source_events
            .iter()
            .filter(|event| event.kind == ManagedRunEventKind::RunTakeoverUpdated)
            .collect();
        assert_eq!(takeover_updates.len(), 1);
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_run_id"))
                .and_then(|value| value.as_str()),
            Some(replay_child.id.as_str())
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_run_status"))
                .and_then(|value| value.as_str()),
            Some("interrupted")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_state"))
                .and_then(|value| value.as_str()),
            Some("interrupted")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("source_interruption_cause"))
                .and_then(|value| value.as_str()),
            Some("lease_expired")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_worker_id"))
                .and_then(|value| value.as_str()),
            Some("gw_takeover")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_ownership_claim_worker_id"))
                .and_then(|value| value.as_str()),
            Some("gw_takeover")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_ownership_claim_lease_expires_at"))
                .and_then(|value| value.as_str()),
            Some("2026-04-23T12:09:00+00:00")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_checkpoint_kind"))
                .and_then(|value| value.as_str()),
            Some("tool_results_checkpointed")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_checkpoint_safe_action"))
                .and_then(|value| value.as_str()),
            Some("execute_pending_tools")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_checkpoint_pending_tool_calls"))
                .and_then(|value| value.as_u64()),
            Some(1)
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(
                    |metadata| metadata.get("follow_target_provider_fence_request_history_len")
                )
                .and_then(|value| value.as_u64()),
            Some(8)
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(
                    |metadata| metadata.get("follow_target_provider_fence_safe_resume_from_kind")
                )
                .and_then(|value| value.as_str()),
            Some("tool_results_checkpointed")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_process_handoff_tool_name"))
                .and_then(|value| value.as_str()),
            Some("terminal")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_process_handoff_process_group"))
                .and_then(|value| value.as_u64()),
            Some(4242)
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_browser_handoff_action"))
                .and_then(|value| value.as_str()),
            Some("click")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_browser_handoff_target"))
                .and_then(|value| value.as_str()),
            Some("#submit")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_browser_session_action"))
                .and_then(|value| value.as_str()),
            Some("navigate")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_browser_session_page_url"))
                .and_then(|value| value.as_str()),
            Some("https://example.com/dashboard")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_mcp_handoff_tool_name"))
                .and_then(|value| value.as_str()),
            Some("mcp_resource_subscribe")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_mcp_handoff_requires_live_runtime"))
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_mcp_runtime_tool_name"))
                .and_then(|value| value.as_str()),
            Some("mcp_resource_subscribe")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(
                    |metadata| metadata.get("follow_target_mcp_runtime_active_subscription_count")
                )
                .and_then(|value| value.as_u64()),
            Some(1)
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_artifact_kind"))
                .and_then(|value| value.as_str()),
            Some("assistant_output")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_artifact_content_preview"))
                .and_then(|value| value.as_str()),
            Some("Recovered answer preview")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_recovery_decision"))
                .and_then(|value| value.as_str()),
            Some("manual_review")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_recovery_reason"))
                .and_then(|value| value.as_str()),
            Some("process_handoff_risk")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_process_handoff_risk"))
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_assessed_by_worker_id"))
                .and_then(|value| value.as_str()),
            Some("gw_leaf_eval")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_ownership_released_worker_id"))
                .and_then(|value| value.as_str()),
            Some("gw_takeover")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("follow_target_ownership_released_reason"))
                .and_then(|value| value.as_str()),
            Some("interrupted")
        );
    }

    #[tokio::test]
    async fn append_source_takeover_update_for_replay_child_dedupes_active_lineage() {
        let (_tmp, state, agent, version) = build_managed_test_state().await;
        let managed = state.managed.clone().unwrap();

        let mut source_run =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        source_run.status = ManagedRunStatus::Interrupted;
        source_run.prompt = "Resume this investigation".to_string();
        source_run.session_id = Some("managed_session_takeover".to_string());
        managed.store.create_run(&source_run).await.unwrap();

        let mut replay_child =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        replay_child.status = ManagedRunStatus::Running;
        replay_child.prompt = source_run.prompt.clone();
        replay_child.session_id = source_run.session_id.clone();
        replay_child.replay_of_run_id = Some(source_run.id.clone());
        managed.store.create_run(&replay_child).await.unwrap();
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": source_run.id.as_str(),
                        "replay_root_run_id": source_run.id.as_str(),
                        "replay_depth": 1,
                        "replay_trigger": "interrupted_auto_replay",
                        "replay_trigger_worker_id": "gw_eval",
                        "replay_source_status": "interrupted",
                        "replay_source_interruption_cause": "lease_expired",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                        "replay_source_boundary": "tool_results_checkpointed",
                    })),
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &replay_child.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunTakeoverEstablished,
                    message: Some("managed replay takeover established".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(
                        serde_json::to_value(&ManagedRunContinuationSummary {
                            source_run_id: source_run.id.clone(),
                            root_run_id: source_run.id.clone(),
                            replay_depth: 1,
                            trigger: hermes_managed::ManagedRunReplayTrigger::InterruptedAutoReplay,
                            takeover_lineage_id: Some(replay_child.id.clone()),
                            source_status: Some(ManagedRunStatus::Interrupted),
                            source_interruption_cause: Some(ManagedRunInterruptionCause::LeaseExpired),
                            reused_session_id: true,
                            resumed_existing_turn: true,
                            source_boundary: Some(
                                hermes_managed::ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed,
                            ),
                            evaluated_by_worker_id: Some("gw_eval".to_string()),
                            takeover_worker_id: Some("gw_takeover".to_string()),
                            note: Some("replay child took over".to_string()),
                        })
                        .unwrap(),
                    ),
                },
            )
            .await
            .unwrap();

        append_source_takeover_update_for_replay_child(
            managed.store.as_ref(),
            &replay_child,
            Some("gw_takeover"),
        )
        .await;
        append_source_takeover_update_for_replay_child(
            managed.store.as_ref(),
            &replay_child,
            Some("gw_takeover"),
        )
        .await;

        let source_events = managed
            .store
            .list_run_events(&source_run.id, 10)
            .await
            .unwrap();
        let takeover_updates: Vec<_> = source_events
            .iter()
            .filter(|event| event.kind == ManagedRunEventKind::RunTakeoverUpdated)
            .collect();
        assert_eq!(takeover_updates.len(), 1);
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_state"))
                .and_then(|value| value.as_str()),
            Some("active")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("current_owner_worker_id"))
                .and_then(|value| value.as_str()),
            Some("gw_takeover")
        );
        assert_eq!(
            takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_lineage_id"))
                .and_then(|value| value.as_str()),
            Some(replay_child.id.as_str())
        );
    }

    #[tokio::test]
    async fn append_source_takeover_update_for_replay_child_propagates_to_ancestor_sources() {
        let (_tmp, state, agent, version) = build_managed_test_state().await;
        let managed = state.managed.clone().unwrap();

        let mut root_source =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        root_source.status = ManagedRunStatus::Interrupted;
        root_source.prompt = "Resume the original task".to_string();
        root_source.session_id = Some("managed_session_takeover_root".to_string());
        managed.store.create_run(&root_source).await.unwrap();

        let mut child_source =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        child_source.status = ManagedRunStatus::Interrupted;
        child_source.prompt = root_source.prompt.clone();
        child_source.session_id = root_source.session_id.clone();
        child_source.replay_of_run_id = Some(root_source.id.clone());
        managed.store.create_run(&child_source).await.unwrap();

        let mut replay_leaf =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        replay_leaf.status = ManagedRunStatus::Running;
        replay_leaf.prompt = root_source.prompt.clone();
        replay_leaf.session_id = root_source.session_id.clone();
        replay_leaf.replay_of_run_id = Some(child_source.id.clone());
        managed.store.create_run(&replay_leaf).await.unwrap();
        managed
            .store
            .append_run_event(
                &replay_leaf.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": child_source.id.as_str(),
                        "replay_root_run_id": root_source.id.as_str(),
                        "replay_depth": 2,
                        "replay_trigger": "interrupted_auto_replay",
                        "replay_trigger_worker_id": "gw_leaf_eval",
                        "replay_source_status": "interrupted",
                        "replay_source_interruption_cause": "lease_expired",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                        "replay_source_boundary": "tool_results_checkpointed",
                    })),
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &replay_leaf.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunTakeoverEstablished,
                    message: Some("managed replay takeover established".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(
                        serde_json::to_value(&ManagedRunContinuationSummary {
                            source_run_id: child_source.id.clone(),
                            root_run_id: root_source.id.clone(),
                            replay_depth: 2,
                            trigger: hermes_managed::ManagedRunReplayTrigger::InterruptedAutoReplay,
                            takeover_lineage_id: Some(child_source.id.clone()),
                            source_status: Some(ManagedRunStatus::Interrupted),
                            source_interruption_cause: Some(ManagedRunInterruptionCause::LeaseExpired),
                            reused_session_id: true,
                            resumed_existing_turn: true,
                            source_boundary: Some(
                                hermes_managed::ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed,
                            ),
                            evaluated_by_worker_id: Some("gw_leaf_eval".to_string()),
                            takeover_worker_id: Some("gw_leaf_owner".to_string()),
                            note: Some("replay leaf took over".to_string()),
                        })
                        .unwrap(),
                    ),
                },
            )
            .await
            .unwrap();

        append_source_takeover_update_for_replay_child(
            managed.store.as_ref(),
            &replay_leaf,
            Some("gw_leaf_owner"),
        )
        .await;
        append_ancestor_follow_replay_decisions_for_replay_child(
            managed.store.as_ref(),
            &replay_leaf,
        )
        .await;
        append_source_takeover_update_for_replay_child(
            managed.store.as_ref(),
            &replay_leaf,
            Some("gw_leaf_owner"),
        )
        .await;
        append_ancestor_follow_replay_decisions_for_replay_child(
            managed.store.as_ref(),
            &replay_leaf,
        )
        .await;

        let child_events = managed
            .store
            .list_run_events(&child_source.id, 10)
            .await
            .unwrap();
        let child_takeover_updates: Vec<_> = child_events
            .iter()
            .filter(|event| event.kind == ManagedRunEventKind::RunTakeoverUpdated)
            .collect();
        assert_eq!(child_takeover_updates.len(), 1);
        assert_eq!(
            child_takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_run_id"))
                .and_then(|value| value.as_str()),
            Some(replay_leaf.id.as_str())
        );
        assert_eq!(
            child_takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("lineage_depth"))
                .and_then(|value| value.as_u64()),
            Some(1)
        );

        let root_events = managed
            .store
            .list_run_events(&root_source.id, 10)
            .await
            .unwrap();
        let root_takeover_updates: Vec<_> = root_events
            .iter()
            .filter(|event| event.kind == ManagedRunEventKind::RunTakeoverUpdated)
            .collect();
        assert_eq!(root_takeover_updates.len(), 1);
        assert_eq!(
            root_takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_run_id"))
                .and_then(|value| value.as_str()),
            Some(replay_leaf.id.as_str())
        );
        assert_eq!(
            root_takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("source_run_id"))
                .and_then(|value| value.as_str()),
            Some(root_source.id.as_str())
        );
        assert_eq!(
            root_takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("lineage_depth"))
                .and_then(|value| value.as_u64()),
            Some(2)
        );
        assert_eq!(
            root_takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_lineage_id"))
                .and_then(|value| value.as_str()),
            Some(child_source.id.as_str())
        );
        assert_eq!(
            root_takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("source_boundary"))
                .and_then(|value| value.as_str()),
            None
        );
        assert_eq!(
            root_takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("leaf_source_run_id"))
                .and_then(|value| value.as_str()),
            Some(child_source.id.as_str())
        );
        assert_eq!(
            root_takeover_updates[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("leaf_source_boundary"))
                .and_then(|value| value.as_str()),
            Some("tool_results_checkpointed")
        );
        assert!(
            root_takeover_updates[0]
                .message
                .as_deref()
                .is_some_and(|message| message.contains("replay descendant"))
        );

        let root_recovery_decisions: Vec<_> = root_events
            .iter()
            .filter(|event| event.kind == ManagedRunEventKind::RunRecoveryDecision)
            .collect();
        assert_eq!(root_recovery_decisions.len(), 1);
        assert_eq!(
            root_recovery_decisions[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("decision"))
                .and_then(|value| value.as_str()),
            Some("follow_replay")
        );
        assert_eq!(
            root_recovery_decisions[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_run_id"))
                .and_then(|value| value.as_str()),
            Some(child_source.id.as_str())
        );
        assert_eq!(
            root_recovery_decisions[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("active_follow_target_run_id"))
                .and_then(|value| value.as_str()),
            Some(replay_leaf.id.as_str())
        );
        assert_eq!(
            root_recovery_decisions[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("active_follow_target_lineage_depth"))
                .and_then(|value| value.as_u64()),
            Some(2)
        );
        assert_eq!(
            root_recovery_decisions[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("takeover_lineage_id"))
                .and_then(|value| value.as_str()),
            Some(child_source.id.as_str())
        );
    }

    #[tokio::test]
    async fn append_ancestor_follow_replay_decisions_for_replay_child_marks_terminal_leaf_without_active_reason()
     {
        let (_tmp, state, agent, version) = build_managed_test_state().await;
        let managed = state.managed.clone().unwrap();

        let mut root_source =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        root_source.status = ManagedRunStatus::Interrupted;
        root_source.prompt = "Resume the original task".to_string();
        root_source.session_id = Some("managed_session_takeover_root_terminal".to_string());
        managed.store.create_run(&root_source).await.unwrap();

        let mut child_source =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        child_source.status = ManagedRunStatus::Interrupted;
        child_source.prompt = root_source.prompt.clone();
        child_source.session_id = root_source.session_id.clone();
        child_source.replay_of_run_id = Some(root_source.id.clone());
        managed.store.create_run(&child_source).await.unwrap();

        let mut replay_leaf =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        replay_leaf.status = ManagedRunStatus::Failed;
        replay_leaf.prompt = root_source.prompt.clone();
        replay_leaf.session_id = root_source.session_id.clone();
        replay_leaf.replay_of_run_id = Some(child_source.id.clone());
        managed.store.create_run(&replay_leaf).await.unwrap();
        managed
            .store
            .append_run_event(
                &replay_leaf.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunCreated,
                    message: Some("managed run replayed".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(serde_json::json!({
                        "replay_of_run_id": child_source.id.as_str(),
                        "replay_root_run_id": root_source.id.as_str(),
                        "replay_depth": 2,
                        "replay_trigger": "interrupted_auto_replay",
                        "replay_trigger_worker_id": "gw_leaf_eval",
                        "replay_source_status": "interrupted",
                        "replay_source_interruption_cause": "lease_expired",
                        "reused_session_id": true,
                        "resumed_existing_turn": true,
                        "replay_source_boundary": "tool_results_checkpointed",
                    })),
                },
            )
            .await
            .unwrap();
        managed
            .store
            .append_run_event(
                &replay_leaf.id,
                &ManagedRunEventDraft {
                    kind: ManagedRunEventKind::RunTakeoverEstablished,
                    message: Some("managed replay takeover established".to_string()),
                    tool_name: None,
                    tool_call_id: None,
                    metadata: Some(
                        serde_json::to_value(&ManagedRunContinuationSummary {
                            source_run_id: child_source.id.clone(),
                            root_run_id: root_source.id.clone(),
                            replay_depth: 2,
                            trigger: hermes_managed::ManagedRunReplayTrigger::InterruptedAutoReplay,
                            takeover_lineage_id: Some(child_source.id.clone()),
                            source_status: Some(ManagedRunStatus::Interrupted),
                            source_interruption_cause: Some(ManagedRunInterruptionCause::LeaseExpired),
                            reused_session_id: true,
                            resumed_existing_turn: true,
                            source_boundary: Some(
                                hermes_managed::ManagedRunContinuationBoundaryKind::ToolResultsCheckpointed,
                            ),
                            evaluated_by_worker_id: Some("gw_leaf_eval".to_string()),
                            takeover_worker_id: Some("gw_leaf_owner".to_string()),
                            note: Some("replay leaf failed after takeover".to_string()),
                        })
                        .unwrap(),
                    ),
                },
            )
            .await
            .unwrap();

        append_ancestor_follow_replay_decisions_for_replay_child(
            managed.store.as_ref(),
            &replay_leaf,
        )
        .await;

        let root_events = managed
            .store
            .list_run_events(&root_source.id, 10)
            .await
            .unwrap();
        let root_recovery_decision = root_events
            .iter()
            .find(|event| event.kind == ManagedRunEventKind::RunRecoveryDecision)
            .expect("root recovery decision missing");
        assert_eq!(
            root_recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("replay_run_id"))
                .and_then(|value| value.as_str()),
            Some(child_source.id.as_str())
        );
        assert_eq!(
            root_recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("active_follow_target_run_id"))
                .and_then(|value| value.as_str()),
            Some(replay_leaf.id.as_str())
        );
        assert_eq!(
            root_recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("active_follow_target_status"))
                .and_then(|value| value.as_str()),
            Some("failed")
        );
        assert_eq!(
            root_recovery_decision
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("reason"))
                .and_then(|value| value.as_str()),
            None
        );
        assert!(
            root_recovery_decision
                .message
                .as_deref()
                .is_some_and(|message| message.contains("replay descendant"))
        );
    }

    #[tokio::test]
    async fn auto_replay_interrupted_runs_skips_unresolved_process_handoff() {
        let app_config = AppConfig {
            managed: ManagedConfigYaml {
                recovery: ManagedRecoveryPolicyYaml {
                    auto_replay_interrupted: true,
                    max_auto_replays_per_root_run: 3,
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(app_config, |_| {}).await;
        let managed = state.managed.clone().unwrap();

        let mut interrupted =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        interrupted.status = ManagedRunStatus::Interrupted;
        interrupted.prompt = "leaf".to_string();
        interrupted.session_id = Some("managed_session_123".to_string());
        managed.store.create_run(&interrupted).await.unwrap();
        managed
            .store
            .append_run_event(
                &interrupted.id,
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

        let summary = maybe_auto_replay_interrupted_runs(
            Arc::clone(&managed.shared),
            managed.app_config.clone(),
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            managed.worker_id.clone(),
            10,
        )
        .await
        .unwrap();

        assert_eq!(summary.candidates, 1);
        assert!(summary.replayed_run_ids.is_empty());
        assert_eq!(summary.skipped_handoff_risk, 1);
        assert!(summary.failures.is_empty());
        assert_eq!(managed.store.list_runs(10).await.unwrap().len(), 1);
        let repeated = maybe_auto_replay_interrupted_runs(
            Arc::clone(&managed.shared),
            managed.app_config.clone(),
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            managed.worker_id.clone(),
            10,
        )
        .await
        .unwrap();
        assert_eq!(repeated.skipped_handoff_risk, 1);
        let events = managed
            .store
            .list_run_events(&interrupted.id, 10)
            .await
            .unwrap();
        let takeover_assessments = events
            .iter()
            .filter(|event| event.kind == ManagedRunEventKind::RunTakeoverAssessed)
            .collect::<Vec<_>>();
        let recovery_decisions = events
            .iter()
            .filter(|event| event.kind == ManagedRunEventKind::RunRecoveryDecision)
            .collect::<Vec<_>>();
        assert_eq!(takeover_assessments.len(), 1);
        assert_eq!(
            takeover_assessments[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("process_handoff_risk"))
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            takeover_assessments[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("evaluated_by_worker_id"))
                .and_then(|value| value.as_str()),
            Some(managed.worker_id.as_str())
        );
        assert_eq!(recovery_decisions.len(), 1);
        assert_eq!(
            recovery_decisions[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("decision"))
                .and_then(|value| value.as_str()),
            Some("manual_review")
        );
        assert_eq!(
            recovery_decisions[0]
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("reason"))
                .and_then(|value| value.as_str()),
            Some("process_handoff_risk")
        );
    }

    #[tokio::test]
    async fn auto_replay_interrupted_runs_skips_unresolved_risky_browser_handoff() {
        let app_config = AppConfig {
            managed: ManagedConfigYaml {
                recovery: ManagedRecoveryPolicyYaml {
                    auto_replay_interrupted: true,
                    max_auto_replays_per_root_run: 3,
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(app_config, |_| {}).await;
        let managed = state.managed.clone().unwrap();

        let mut interrupted =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        interrupted.status = ManagedRunStatus::Interrupted;
        interrupted.prompt = "leaf".to_string();
        interrupted.session_id = Some("managed_session_123".to_string());
        managed.store.create_run(&interrupted).await.unwrap();
        managed
            .store
            .append_run_event(
                &interrupted.id,
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

        let summary = maybe_auto_replay_interrupted_runs(
            Arc::clone(&managed.shared),
            managed.app_config.clone(),
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            managed.worker_id.clone(),
            10,
        )
        .await
        .unwrap();

        assert_eq!(summary.candidates, 1);
        assert!(summary.replayed_run_ids.is_empty());
        assert_eq!(summary.skipped_browser_handoff_risk, 1);
        assert!(summary.failures.is_empty());
        assert_eq!(managed.store.list_runs(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn auto_replay_interrupted_runs_skips_live_browser_session_checkpoint() {
        let app_config = AppConfig {
            managed: ManagedConfigYaml {
                recovery: ManagedRecoveryPolicyYaml {
                    auto_replay_interrupted: true,
                    max_auto_replays_per_root_run: 3,
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(app_config, |_| {}).await;
        let managed = state.managed.clone().unwrap();

        let mut interrupted =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        interrupted.status = ManagedRunStatus::Interrupted;
        interrupted.prompt = "leaf".to_string();
        interrupted.session_id = Some("managed_session_123".to_string());
        managed.store.create_run(&interrupted).await.unwrap();
        managed
            .store
            .append_run_event(
                &interrupted.id,
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

        let summary = maybe_auto_replay_interrupted_runs(
            Arc::clone(&managed.shared),
            managed.app_config.clone(),
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            managed.worker_id.clone(),
            10,
        )
        .await
        .unwrap();

        assert_eq!(summary.candidates, 1);
        assert!(summary.replayed_run_ids.is_empty());
        assert_eq!(summary.skipped_browser_session_state, 1);
        assert!(summary.failures.is_empty());
        assert_eq!(managed.store.list_runs(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn auto_replay_interrupted_runs_skips_risky_mcp_handoff() {
        let app_config = AppConfig {
            managed: ManagedConfigYaml {
                recovery: ManagedRecoveryPolicyYaml {
                    auto_replay_interrupted: true,
                    max_auto_replays_per_root_run: 3,
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(app_config, |_| {}).await;
        let managed = state.managed.clone().unwrap();

        let mut interrupted =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        interrupted.status = ManagedRunStatus::Interrupted;
        interrupted.prompt = "leaf".to_string();
        interrupted.session_id = Some("managed_session_123".to_string());
        managed.store.create_run(&interrupted).await.unwrap();
        managed
            .store
            .append_run_event(
                &interrupted.id,
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
                        "transport": "stdio",
                        "target": "uri:docs://guide",
                        "note": "MCP subscription state may already have changed before interruption",
                    })),
                },
            )
            .await
            .unwrap();

        let summary = maybe_auto_replay_interrupted_runs(
            Arc::clone(&managed.shared),
            managed.app_config.clone(),
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            managed.worker_id.clone(),
            10,
        )
        .await
        .unwrap();

        assert_eq!(summary.candidates, 1);
        assert!(summary.replayed_run_ids.is_empty());
        assert_eq!(summary.skipped_mcp_handoff_risk, 1);
        assert!(summary.failures.is_empty());
        assert_eq!(managed.store.list_runs(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn auto_replay_interrupted_runs_skips_live_mcp_runtime_checkpoint() {
        let app_config = AppConfig {
            managed: ManagedConfigYaml {
                recovery: ManagedRecoveryPolicyYaml {
                    auto_replay_interrupted: true,
                    max_auto_replays_per_root_run: 3,
                },
                ..ManagedConfigYaml::default()
            },
            ..AppConfig::default()
        };
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(app_config, |_| {}).await;
        let managed = state.managed.clone().unwrap();

        let mut interrupted =
            hermes_managed::ManagedRun::new(&agent.id, version.version, &version.model);
        interrupted.status = ManagedRunStatus::Interrupted;
        interrupted.prompt = "leaf".to_string();
        interrupted.session_id = Some("managed_session_123".to_string());
        managed.store.create_run(&interrupted).await.unwrap();
        managed
            .store
            .append_run_event(
                &interrupted.id,
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

        let summary = maybe_auto_replay_interrupted_runs(
            Arc::clone(&managed.shared),
            managed.app_config.clone(),
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            managed.worker_id.clone(),
            10,
        )
        .await
        .unwrap();

        assert_eq!(summary.candidates, 1);
        assert!(summary.replayed_run_ids.is_empty());
        assert_eq!(summary.skipped_mcp_runtime_state, 1);
        assert!(summary.failures.is_empty());
        assert_eq!(managed.store.list_runs(10).await.unwrap().len(), 1);
    }

    #[test]
    fn browser_session_checkpoint_from_tool_result_extracts_live_state() {
        let observation = ToolExecutionResultObservation {
            request: hermes_core::tool::ToolExecutionObservation {
                session_id: "run_123".to_string(),
                call_id: "call_browser_1".to_string(),
                tool_name: "browser".to_string(),
                toolset: Some("browser".to_string()),
                arguments: serde_json::json!({
                    "action": "extract_text",
                    "selector": "#status"
                }),
            },
            result: hermes_core::message::ToolResult::ok(
                serde_json::json!({
                    "url": "file:///status.html",
                    "title": "Status",
                    "format": "text",
                    "selector": "#status",
                    "content": "Ready",
                })
                .to_string(),
            ),
        };

        let summary = browser_session_checkpoint_from_tool_result(&observation)
            .expect("browser checkpoint should be derived from successful result");
        assert_eq!(summary.action, "extract_text");
        assert!(summary.session_open);
        assert_eq!(summary.target.as_deref(), Some("selector:#status"));
        assert_eq!(summary.page_url.as_deref(), Some("file:///status.html"));
        assert_eq!(summary.output_preview.as_deref(), Some("Ready"));
    }

    #[test]
    fn mcp_handoff_from_tool_result_extracts_read_only_state() {
        let observation = ToolExecutionResultObservation {
            request: hermes_core::tool::ToolExecutionObservation {
                session_id: "run_123".to_string(),
                call_id: "call_mcp_1".to_string(),
                tool_name: "mcp_resource_read".to_string(),
                toolset: Some("mcp".to_string()),
                arguments: serde_json::json!({
                    "server": "docs",
                    "uri": "docs://guide"
                }),
            },
            result: hermes_core::message::ToolResult::ok(
                serde_json::json!({
                    "server": "docs",
                    "result": {
                        "uri": "docs://guide",
                        "contents": [{ "text": "Guide body" }]
                    }
                })
                .to_string(),
            ),
        };

        let mut transports = BTreeMap::new();
        transports.insert("docs".to_string(), "http".to_string());
        let summary = mcp_handoff_from_tool_result(&observation, &transports)
            .expect("MCP handoff should be derived from successful result");
        assert_eq!(summary.tool_name, "mcp_resource_read");
        assert_eq!(summary.state, ManagedRunMcpHandoffState::Completed);
        assert_eq!(
            summary.replay_disposition,
            ManagedRunMcpReplayDisposition::SafeToReplay
        );
        assert!(summary.read_only);
        assert!(!summary.requires_live_runtime);
        assert_eq!(summary.server.as_deref(), Some("docs"));
        assert_eq!(summary.transport.as_deref(), Some("http"));
        assert_eq!(summary.target.as_deref(), Some("uri:docs://guide"));
    }

    #[test]
    fn mcp_handoff_from_unknown_mcp_tool_defaults_to_risky() {
        let observation = hermes_core::tool::ToolExecutionObservation {
            session_id: "run_123".to_string(),
            call_id: "call_mcp_dynamic_1".to_string(),
            tool_name: "docs_search".to_string(),
            toolset: Some("mcp".to_string()),
            arguments: serde_json::json!({ "query": "leases" }),
        };

        let summary = mcp_handoff_from_tool_call(&observation, &BTreeMap::new())
            .expect("unknown MCP tools should still emit a conservative handoff summary");
        assert_eq!(summary.tool_name, "docs_search");
        assert_eq!(summary.state, ManagedRunMcpHandoffState::Started);
        assert_eq!(
            summary.replay_disposition,
            ManagedRunMcpReplayDisposition::UnsafeSideEffectWindow
        );
        assert!(!summary.read_only);
        assert!(summary.requires_live_runtime);
    }

    #[test]
    fn mcp_runtime_checkpoint_from_tool_result_tracks_active_subscription_state() {
        let observation = ToolExecutionResultObservation {
            request: hermes_core::tool::ToolExecutionObservation {
                session_id: "run_123".to_string(),
                call_id: "call_mcp_runtime_1".to_string(),
                tool_name: "mcp_resource_subscribe".to_string(),
                toolset: Some("mcp".to_string()),
                arguments: serde_json::json!({
                    "server": "docs",
                    "uri": "docs://guide"
                }),
            },
            result: hermes_core::message::ToolResult::ok(
                serde_json::json!({
                    "server": "docs",
                    "subscribed": true
                })
                .to_string(),
            ),
        };

        let mut transports = BTreeMap::new();
        transports.insert("docs".to_string(), "http".to_string());
        let mut runtime_state = ManagedMcpRuntimeState::default();
        let summary =
            mcp_runtime_checkpoint_from_tool_result(&observation, &transports, &mut runtime_state)
                .expect("stateful MCP results should emit a runtime checkpoint summary");

        assert_eq!(summary.tool_name, "mcp_resource_subscribe");
        assert!(summary.live_runtime_required);
        assert_eq!(summary.active_subscription_count, 1);
        assert_eq!(summary.active_servers, vec!["docs".to_string()]);
        assert_eq!(summary.transport.as_deref(), Some("http"));
        assert_eq!(summary.target.as_deref(), Some("uri:docs://guide"));
    }

    #[tokio::test]
    async fn managed_session_checkpoint_persists_tool_safe_points_before_completion() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let Some((base_url, _audits, server_handle)) =
            spawn_mock_managed_tool_then_hold_server().await
        else {
            return;
        };
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.base_url = Some(base_url.clone());
                version.timeout_secs = 30;
            })
            .await;
        let managed = state.managed.clone().unwrap();

        let (run_id, session_id, _timeout_secs, _stream_rx, _outcome_rx) =
            spawn_managed_run_with_version(
                &managed,
                &agent,
                &version,
                "resume me".to_string(),
                None,
                None,
                false,
            )
            .await
            .unwrap();

        let session_id = session_id.expect("managed run should create durable session");
        let history = wait_for_session_history_len(
            managed.shared.session_store.as_ref().unwrap().as_ref(),
            &session_id,
            3,
        )
        .await;
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[0].content.as_text_lossy(), "resume me");
        assert_eq!(history[1].role, Role::Assistant);
        assert_eq!(history[1].tool_calls.len(), 1);
        assert_eq!(history[1].tool_calls[0].name, "unknown_tool");
        assert_eq!(history[2].role, Role::Tool);
        assert_eq!(history[2].tool_call_id.as_deref(), Some("call_resume"));
        let artifacts = managed.store.list_run_artifacts(&run_id, 10).await.unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].kind, ManagedRunArtifactKind::ToolOutput);
        assert_eq!(artifacts[0].tool_name.as_deref(), Some("unknown_tool"));
        assert_eq!(artifacts[0].tool_call_id.as_deref(), Some("call_resume"));
        assert_eq!(artifacts[0].content, "unknown tool: unknown_tool");

        terminate_managed_run(
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            run_id.clone(),
            ManagedRunStatus::Cancelled,
            Some("test cancelled".to_string()),
        )
        .await;
        wait_for_run_status(managed.store.as_ref(), &run_id, ManagedRunStatus::Cancelled).await;
        wait_for_run_eviction(managed.runs.as_ref(), &run_id).await;

        server_handle.abort();
    }

    #[tokio::test]
    async fn managed_session_checkpoint_persists_assistant_output_artifact() {
        let _env_guard = ENV_LOCK.lock().await;
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let store = Arc::new(
            ManagedStore::open_at(&home.path().join("managed.db"))
                .await
                .unwrap(),
        );
        let session_store = Arc::new(
            hermes_config::SqliteSessionStore::open_at(&home.path().join("state.db"))
                .await
                .unwrap(),
        );

        let agent = ManagedAgent::new("artifact-observer");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.session_id = Some("managed_artifact_session".to_string());
        store.create_run(&run).await.unwrap();
        ensure_managed_session(
            session_store.as_ref(),
            run.session_id.as_deref().unwrap(),
            &version,
            &std::env::current_dir().unwrap(),
        )
        .await
        .unwrap();

        let observer = ManagedSessionCheckpointObserver::new(
            Arc::clone(&store),
            run.id.clone(),
            session_store,
            run.session_id.clone().unwrap(),
            0,
            Arc::new(ManagedRunCheckpointState::default()),
        );

        let history = vec![Message::user("hello"), Message::assistant("Final answer")];
        observer.on_history_checkpoint(&history).await.unwrap();

        let artifacts = store.list_run_artifacts(&run.id, 10).await.unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].kind, ManagedRunArtifactKind::AssistantOutput);
        assert_eq!(artifacts[0].label, "assistant_output");
        assert_eq!(artifacts[0].content, "Final answer");
    }

    #[tokio::test]
    async fn auto_replay_interrupted_runs_resume_from_persisted_history_without_duplicate_prompt() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let Some((base_url, audits, server_handle)) =
            spawn_mock_managed_provider_server(MockManagedProviderProtocol::OpenAi, "Recovered")
                .await
        else {
            return;
        };
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.base_url = Some(base_url.clone());
                version.timeout_secs = 5;
            })
            .await;
        let managed = state.managed.clone().unwrap();
        let session_id = "managed-replay-session".to_string();
        let working_dir = std::env::current_dir().unwrap();
        let session_store = managed.shared.session_store.clone().unwrap();
        ensure_managed_session(session_store.as_ref(), &session_id, &version, &working_dir)
            .await
            .unwrap();
        session_store
            .append_message(&session_id, &Message::user("resume me"))
            .await
            .unwrap();
        let mut assistant = Message::assistant("");
        assistant.tool_calls = vec![hermes_core::message::ToolCall {
            id: "call_resume".to_string(),
            name: "unknown_tool".to_string(),
            arguments: serde_json::json!({}),
        }];
        session_store
            .append_message(&session_id, &assistant)
            .await
            .unwrap();
        session_store
            .append_message(
                &session_id,
                &Message {
                    role: Role::Tool,
                    content: Content::Text("unknown tool: unknown_tool".to_string()),
                    tool_calls: vec![],
                    reasoning: None,
                    name: Some("unknown_tool".to_string()),
                    tool_call_id: Some("call_resume".to_string()),
                },
            )
            .await
            .unwrap();

        let mut interrupted = ManagedRun::new(&agent.id, version.version, &version.model);
        interrupted.status = ManagedRunStatus::Interrupted;
        interrupted.updated_at = chrono::Utc::now();
        interrupted.prompt = "resume me".to_string();
        interrupted.session_id = Some(session_id.clone());
        managed.store.create_run(&interrupted).await.unwrap();

        let summary = maybe_auto_replay_interrupted_runs(
            Arc::clone(&managed.shared),
            managed.app_config.clone(),
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            managed.worker_id.clone(),
            8,
        )
        .await
        .unwrap();

        assert_eq!(summary.replayed_run_ids.len(), 1);
        let replayed_run_id = summary.replayed_run_ids[0].clone();
        let replayed = wait_for_run_status(
            managed.store.as_ref(),
            &replayed_run_id,
            ManagedRunStatus::Completed,
        )
        .await;
        assert_eq!(
            replayed.replay_of_run_id.as_deref(),
            Some(interrupted.id.as_str())
        );
        wait_for_run_eviction(managed.runs.as_ref(), &replayed_run_id).await;

        let audits = audits.lock().unwrap();
        assert_eq!(audits.len(), 1);
        assert_eq!(
            audits[0]
                .messages
                .iter()
                .filter(|message| message.as_str() == "resume me")
                .count(),
            1
        );
        assert!(
            audits[0]
                .messages
                .iter()
                .any(|message| message == "unknown tool: unknown_tool")
        );

        server_handle.abort();
    }

    #[tokio::test]
    async fn auto_replay_interrupted_runs_executes_pending_tool_calls_before_provider_resume() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let Some((base_url, audits, server_handle)) = spawn_mock_managed_provider_server(
            MockManagedProviderProtocol::OpenAi,
            "Recovered after tool",
        )
        .await
        else {
            return;
        };
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.base_url = Some(base_url.clone());
                version.timeout_secs = 5;
            })
            .await;
        let managed = state.managed.clone().unwrap();
        let session_id = "managed-replay-pending-tool-session".to_string();
        let working_dir = std::env::current_dir().unwrap();
        let session_store = managed.shared.session_store.clone().unwrap();
        ensure_managed_session(session_store.as_ref(), &session_id, &version, &working_dir)
            .await
            .unwrap();
        session_store
            .append_message(&session_id, &Message::user("resume me"))
            .await
            .unwrap();
        let mut assistant = Message::assistant("");
        assistant.tool_calls = vec![hermes_core::message::ToolCall {
            id: "call_resume".to_string(),
            name: "unknown_tool".to_string(),
            arguments: serde_json::json!({}),
        }];
        session_store
            .append_message(&session_id, &assistant)
            .await
            .unwrap();

        let mut interrupted = ManagedRun::new(&agent.id, version.version, &version.model);
        interrupted.status = ManagedRunStatus::Interrupted;
        interrupted.updated_at = chrono::Utc::now();
        interrupted.prompt = "resume me".to_string();
        interrupted.session_id = Some(session_id.clone());
        managed.store.create_run(&interrupted).await.unwrap();

        let summary = maybe_auto_replay_interrupted_runs(
            Arc::clone(&managed.shared),
            managed.app_config.clone(),
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            managed.worker_id.clone(),
            8,
        )
        .await
        .unwrap();

        assert_eq!(summary.replayed_run_ids.len(), 1);
        let replayed_run_id = summary.replayed_run_ids[0].clone();
        wait_for_run_status(
            managed.store.as_ref(),
            &replayed_run_id,
            ManagedRunStatus::Completed,
        )
        .await;
        wait_for_run_eviction(managed.runs.as_ref(), &replayed_run_id).await;

        let audits = audits.lock().unwrap();
        assert_eq!(audits.len(), 1);
        assert_eq!(
            audits[0]
                .messages
                .iter()
                .filter(|message| message.as_str() == "resume me")
                .count(),
            1
        );
        assert!(
            audits[0]
                .messages
                .iter()
                .any(|message| message == "unknown tool: unknown_tool")
        );

        server_handle.abort();
    }

    #[tokio::test]
    async fn auto_replay_interrupted_runs_complete_from_checkpointed_final_assistant_response() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let Some((base_url, audits, server_handle)) =
            spawn_mock_managed_provider_server(MockManagedProviderProtocol::OpenAi, "unused").await
        else {
            return;
        };
        let (_tmp, state, agent, version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.base_url = Some(base_url.clone());
                version.timeout_secs = 5;
            })
            .await;
        let managed = state.managed.clone().unwrap();
        let session_id = "managed-replay-final-assistant-session".to_string();
        let working_dir = std::env::current_dir().unwrap();
        let session_store = managed.shared.session_store.clone().unwrap();
        ensure_managed_session(session_store.as_ref(), &session_id, &version, &working_dir)
            .await
            .unwrap();
        session_store
            .append_message(&session_id, &Message::user("resume me"))
            .await
            .unwrap();
        session_store
            .append_message(&session_id, &Message::assistant("Recovered final"))
            .await
            .unwrap();

        let mut interrupted = ManagedRun::new(&agent.id, version.version, &version.model);
        interrupted.status = ManagedRunStatus::Interrupted;
        interrupted.updated_at = chrono::Utc::now();
        interrupted.prompt = "resume me".to_string();
        interrupted.session_id = Some(session_id.clone());
        managed.store.create_run(&interrupted).await.unwrap();

        let summary = maybe_auto_replay_interrupted_runs(
            Arc::clone(&managed.shared),
            managed.app_config.clone(),
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            managed.worker_id.clone(),
            8,
        )
        .await
        .unwrap();

        assert_eq!(summary.replayed_run_ids.len(), 1);
        let replayed_run_id = summary.replayed_run_ids[0].clone();
        let replayed = wait_for_run_status(
            managed.store.as_ref(),
            &replayed_run_id,
            ManagedRunStatus::Completed,
        )
        .await;
        assert_eq!(
            replayed.replay_of_run_id.as_deref(),
            Some(interrupted.id.as_str())
        );
        wait_for_run_eviction(managed.runs.as_ref(), &replayed_run_id).await;

        let audits = audits.lock().unwrap();
        assert!(audits.is_empty(), "provider should not be called");

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_managed_oai_chat_reuses_session_history_and_returns_session_header() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let Some((base_url, audits, server_handle)) = spawn_mock_managed_provider_server(
            MockManagedProviderProtocol::OpenAi,
            "Session reply",
        )
        .await
        else {
            return;
        };
        let (_tmp, state, _agent, _version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.base_url = Some(base_url.clone());
                version.timeout_secs = 5;
            })
            .await;
        let managed = state.managed.clone().unwrap();
        let app = build_stateful_app(state);

        let first_request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model": "agent:code-reviewer",
                    "messages": [{"role": "user", "content": "hello session"}]
                })
                .to_string(),
            ))
            .unwrap();

        let first_response = app.clone().oneshot(first_request).await.unwrap();
        assert_eq!(first_response.status(), StatusCode::OK);
        let first_session_id = first_response
            .headers()
            .get("x-hermes-session-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
            .expect("managed response should include session id");
        let first_run_id = first_response
            .headers()
            .get("x-hermes-run-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
            .expect("managed response should include run id");
        let _ = first_response.into_body().collect().await.unwrap();

        let first_run = wait_for_run_status(
            managed.store.as_ref(),
            &first_run_id,
            ManagedRunStatus::Completed,
        )
        .await;
        assert_eq!(
            first_run.session_id.as_deref(),
            Some(first_session_id.as_str())
        );
        wait_for_run_eviction(managed.runs.as_ref(), &first_run_id).await;

        let second_request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "model": "agent:code-reviewer",
                    "session_id": first_session_id,
                    "messages": [{"role": "user", "content": "follow up"}]
                })
                .to_string(),
            ))
            .unwrap();

        let second_response = app.clone().oneshot(second_request).await.unwrap();
        assert_eq!(second_response.status(), StatusCode::OK);
        assert_eq!(
            second_response
                .headers()
                .get("x-hermes-session-id")
                .and_then(|value| value.to_str().ok()),
            Some(first_run.session_id.as_deref().unwrap())
        );
        let second_run_id = second_response
            .headers()
            .get("x-hermes-run-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
            .expect("managed response should include run id");
        let _ = second_response.into_body().collect().await.unwrap();

        let second_run = wait_for_run_status(
            managed.store.as_ref(),
            &second_run_id,
            ManagedRunStatus::Completed,
        )
        .await;
        assert_eq!(
            second_run.session_id.as_deref(),
            first_run.session_id.as_deref()
        );
        wait_for_run_eviction(managed.runs.as_ref(), &second_run_id).await;

        let audits = audits.lock().unwrap();
        assert_eq!(audits.len(), 2);
        assert!(
            audits[0]
                .messages
                .iter()
                .any(|message| message == "hello session")
        );
        assert!(
            audits[1]
                .messages
                .iter()
                .any(|message| message == "hello session")
        );
        assert!(
            audits[1]
                .messages
                .iter()
                .any(|message| message == "Session reply")
        );
        assert!(
            audits[1]
                .messages
                .iter()
                .any(|message| message == "follow up")
        );

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_oai_chat_no_router_returns_503() {
        let (tx, _rx) = mpsc::channel(8);
        let app = build_app(tx);

        let body = serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}]
        })
        .to_string();

        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_oai_chat_empty_messages_returns_400() {
        let (tx, _rx) = mpsc::channel(8);
        let state = ApiState {
            event_tx: tx,
            pending: Arc::new(DashMap::new()),
            api_key: None,
            model_name: "test".into(),
            router: None,
            managed: None,
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(handle_oai_chat))
            .with_state(state);

        // Empty messages array — message validation fires before router check
        let body = serde_json::json!({"messages": []}).to_string();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_managed_oai_chat_without_runtime_returns_503() {
        let (tx, _rx) = mpsc::channel(8);
        let app = build_app(tx);

        let body = serde_json::json!({
            "model": "agent:code-reviewer",
            "messages": [{"role": "user", "content": "hello"}]
        })
        .to_string();

        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_managed_oai_stream_disconnect_cancels_run() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let Some((base_url, started_rx, server_handle)) =
            spawn_mock_openai_server(MockOpenAiBehavior::DelayedText {
                delay_ms: 200,
                text: "hello",
            })
            .await
        else {
            return;
        };
        let (_tmp, state, _agent, _version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.base_url = Some(base_url.clone());
                version.timeout_secs = 5;
            })
            .await;
        let managed = state.managed.clone().unwrap();
        let app = build_stateful_app(state);

        let body = serde_json::json!({
            "model": "agent:code-reviewer",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        })
        .to_string();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        let run = wait_for_created_run(managed.store.as_ref()).await;

        drop(response);

        let stored =
            wait_for_run_status(managed.store.as_ref(), &run.id, ManagedRunStatus::Cancelled).await;
        assert_eq!(stored.last_error.as_deref(), Some("client disconnected"));
        assert!(stored.cancel_requested_at.is_some());
        wait_for_run_eviction(managed.runs.as_ref(), &run.id).await;

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_managed_oai_completed_run_records_lifecycle_events() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let Some((base_url, _audits, server_handle)) = spawn_mock_managed_provider_server(
            MockManagedProviderProtocol::OpenAi,
            "Hello from OpenAI",
        )
        .await
        else {
            return;
        };
        let (_tmp, state, _agent, _version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.base_url = Some(base_url.clone());
                version.timeout_secs = 5;
            })
            .await;
        let managed = state.managed.clone().unwrap();
        let app = build_stateful_app(state);

        let body = serde_json::json!({
            "model": "agent:code-reviewer",
            "messages": [{"role": "user", "content": "hello"}]
        })
        .to_string();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let run = wait_for_created_run(managed.store.as_ref()).await;
        let _stored =
            wait_for_run_status(managed.store.as_ref(), &run.id, ManagedRunStatus::Completed).await;
        wait_for_run_eviction(managed.runs.as_ref(), &run.id).await;

        let events_request = Request::builder()
            .method(Method::GET)
            .uri(format!("/v1/runs/{}/events", run.id))
            .body(Body::empty())
            .unwrap();
        let events_response = app.clone().oneshot(events_request).await.unwrap();
        assert_eq!(events_response.status(), StatusCode::OK);
        let events_bytes = events_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let events_json: serde_json::Value = serde_json::from_slice(&events_bytes).unwrap();
        let kinds = events_json["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["kind"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(kinds.contains(&"run.created".to_string()));
        assert!(kinds.contains(&"run.ownership_claimed".to_string()));
        assert!(kinds.contains(&"run.started".to_string()));
        assert!(kinds.contains(&"run.completed".to_string()));
        assert!(kinds.contains(&"run.ownership_released".to_string()));

        let limited_events_request = Request::builder()
            .method(Method::GET)
            .uri(format!("/v1/runs/{}/events?limit=2", run.id))
            .body(Body::empty())
            .unwrap();
        let limited_events_response = app.oneshot(limited_events_request).await.unwrap();
        assert_eq!(limited_events_response.status(), StatusCode::OK);
        let limited_events_bytes = limited_events_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let limited_events_json: serde_json::Value =
            serde_json::from_slice(&limited_events_bytes).unwrap();
        let limited_kinds = limited_events_json["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["kind"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            limited_kinds,
            vec![
                "run.completed".to_string(),
                "run.ownership_released".to_string()
            ]
        );

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_managed_oai_cancel_during_provider_wait_preserves_cancelled_run() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let Some((base_url, started_rx, server_handle)) =
            spawn_mock_openai_server(MockOpenAiBehavior::HoldOpen).await
        else {
            return;
        };
        let (_tmp, state, _agent, _version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.base_url = Some(base_url.clone());
                version.timeout_secs = 30;
            })
            .await;
        let managed = state.managed.clone().unwrap();
        let app = build_stateful_app(state);

        let body = serde_json::json!({
            "model": "agent:code-reviewer",
            "messages": [{"role": "user", "content": "hello"}]
        })
        .to_string();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let request_task = {
            let app = app.clone();
            tokio::spawn(async move { app.oneshot(request).await.unwrap() })
        };

        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .unwrap()
            .unwrap();
        let run = wait_for_created_run(managed.store.as_ref()).await;

        let cancel_request = Request::builder()
            .method(Method::DELETE)
            .uri(format!("/v1/runs/{}", run.id))
            .body(Body::empty())
            .unwrap();
        let cancel_response = app.clone().oneshot(cancel_request).await.unwrap();
        assert_eq!(cancel_response.status(), StatusCode::OK);

        let _response = tokio::time::timeout(Duration::from_secs(2), request_task)
            .await
            .unwrap()
            .unwrap();

        let stored =
            wait_for_run_status(managed.store.as_ref(), &run.id, ManagedRunStatus::Cancelled).await;
        assert_eq!(stored.last_error.as_deref(), Some("cancelled via API"));
        assert!(stored.cancel_requested_at.is_some());
        wait_for_run_eviction(managed.runs.as_ref(), &run.id).await;

        let events_request = Request::builder()
            .method(Method::GET)
            .uri(format!("/v1/runs/{}/events", run.id))
            .body(Body::empty())
            .unwrap();
        let events_response = app.oneshot(events_request).await.unwrap();
        assert_eq!(events_response.status(), StatusCode::OK);
        let events_bytes = events_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let events_json: serde_json::Value = serde_json::from_slice(&events_bytes).unwrap();
        let terminal_event = events_json["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["kind"] == "run.cancelled")
            .cloned()
            .expect("cancelled event missing");
        assert_eq!(terminal_event["message"], "cancelled via API");

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_managed_oai_failure_returns_500_and_preserves_failed_run() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let Some((base_url, _started_rx, server_handle)) =
            spawn_mock_openai_server(MockOpenAiBehavior::ServerError {
                message: "provider exploded",
            })
            .await
        else {
            return;
        };
        let (_tmp, state, _agent, _version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.base_url = Some(base_url.clone());
                version.timeout_secs = 5;
            })
            .await;
        let managed = state.managed.clone().unwrap();
        let app = build_stateful_app(state);

        let body = serde_json::json!({
            "model": "agent:code-reviewer",
            "messages": [{"role": "user", "content": "hello"}]
        })
        .to_string();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("provider exploded")
        );

        let run = wait_for_created_run(managed.store.as_ref()).await;
        let stored =
            wait_for_run_status(managed.store.as_ref(), &run.id, ManagedRunStatus::Failed).await;
        assert!(
            stored
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("provider exploded")
        );
        wait_for_run_eviction(managed.runs.as_ref(), &run.id).await;

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_managed_oai_mcp_admission_rejection_persists_event() {
        let _env_guard = ENV_LOCK.lock().await;
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let (_tmp, state, _agent, _version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.allowed_tools = vec!["mcp_resource_read".to_string()];
            })
            .await;
        let managed = state.managed.clone().unwrap();
        let app = build_stateful_app(state);

        let body = serde_json::json!({
            "model": "agent:code-reviewer",
            "messages": [{"role": "user", "content": "hello"}]
        })
        .to_string();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("managed MCP tools are disabled by operator policy")
        );
        assert_eq!(
            json["error"]["code"].as_str(),
            Some("disabled_by_operator_policy")
        );

        let run = wait_for_created_run(managed.store.as_ref()).await;
        let stored =
            wait_for_run_status(managed.store.as_ref(), &run.id, ManagedRunStatus::Failed).await;
        assert!(
            stored
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("managed MCP tools are disabled by operator policy")
        );

        let events = managed.store.list_run_events(&run.id, 10).await.unwrap();
        let rejection = events
            .iter()
            .find(|event| event.kind == ManagedRunEventKind::RunMcpAdmissionRejected)
            .expect("managed MCP admission rejection event missing");
        assert_eq!(
            rejection
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("code"))
                .and_then(|value| value.as_str()),
            Some("disabled_by_operator_policy")
        );
        assert_eq!(
            rejection
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("requested_tools"))
                .and_then(|value| value.as_array())
                .and_then(|values| values.first())
                .and_then(|value| value.as_str()),
            Some("mcp_resource_read")
        );
        assert!(
            events
                .iter()
                .any(|event| event.kind == ManagedRunEventKind::RunFailed),
            "terminal run.failed event missing"
        );
    }

    #[tokio::test]
    async fn test_managed_oai_stream_failure_omits_stop_and_done_markers() {
        let _env_guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");
        let home = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &home.path().to_string_lossy());

        let Some((base_url, _started_rx, server_handle)) =
            spawn_mock_openai_server(MockOpenAiBehavior::ServerError {
                message: "stream provider exploded",
            })
            .await
        else {
            return;
        };
        let (_tmp, state, _agent, _version) =
            build_managed_test_state_with_version(AppConfig::default(), |version| {
                version.base_url = Some(base_url.clone());
                version.timeout_secs = 5;
            })
            .await;
        let managed = state.managed.clone().unwrap();
        let app = build_stateful_app(state);

        let body = serde_json::json!({
            "model": "agent:code-reviewer",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        })
        .to_string();
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains("[DONE]"));
        assert!(!text.contains("\"finish_reason\":\"stop\""));

        let run = wait_for_created_run(managed.store.as_ref()).await;
        let stored =
            wait_for_run_status(managed.store.as_ref(), &run.id, ManagedRunStatus::Failed).await;
        assert!(
            stored
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("stream provider exploded")
        );
        wait_for_run_eviction(managed.runs.as_ref(), &run.id).await;

        server_handle.abort();
    }

    #[test]
    fn test_run_event_from_delta_maps_tool_events() {
        let started = run_event_from_delta(&StreamDelta::ToolCallStart {
            id: "call_123".to_string(),
            name: "read_file".to_string(),
        })
        .expect("tool call start should map");
        assert_eq!(started.kind, ManagedRunEventKind::ToolCallStarted);
        assert_eq!(started.tool_name.as_deref(), Some("read_file"));
        assert_eq!(started.tool_call_id.as_deref(), Some("call_123"));
        assert_eq!(started.metadata, None);

        let progress = run_event_from_delta(&StreamDelta::ToolProgress {
            tool: "read_file".to_string(),
            status: "reading README.md".to_string(),
        })
        .expect("tool progress should map");
        assert_eq!(progress.kind, ManagedRunEventKind::ToolProgress);
        assert_eq!(progress.message.as_deref(), Some("reading README.md"));
        assert_eq!(progress.metadata, None);

        let signet_event = run_event_from_delta(&StreamDelta::ToolEvent {
            kind: "tool.request_signed".to_string(),
            tool: "read_file".to_string(),
            call_id: Some("call_123".to_string()),
            message: Some("Signet request receipt appended".to_string()),
            metadata: Some(serde_json::json!({
                "receipt_id": "rec_123",
                "record_hash": "sha256:abc",
            })),
        })
        .expect("tool event should map");
        assert_eq!(signet_event.kind, ManagedRunEventKind::ToolRequestSigned);
        assert_eq!(
            signet_event.message.as_deref(),
            Some("Signet request receipt appended")
        );
        assert_eq!(signet_event.tool_call_id.as_deref(), Some("call_123"));
        assert_eq!(
            signet_event.metadata.as_ref().unwrap()["receipt_id"],
            "rec_123"
        );

        assert!(run_event_from_delta(&StreamDelta::TextDelta("hello".to_string())).is_none());
    }

    #[tokio::test]
    async fn test_managed_provider_matrix_openai() {
        run_managed_provider_case(
            MockManagedProviderProtocol::OpenAi,
            "openai/gpt-4o-mini",
            "OPENAI_API_KEY",
            "test-openai-key",
            "gpt-4o-mini",
            "Hello from OpenAI",
        )
        .await;
    }

    #[tokio::test]
    async fn test_managed_provider_matrix_openrouter() {
        run_managed_provider_case(
            MockManagedProviderProtocol::OpenAi,
            "openrouter/meta-llama/llama-3.1-8b-instruct",
            "OPENROUTER_API_KEY",
            "test-openrouter-key",
            "meta-llama/llama-3.1-8b-instruct",
            "Hello from OpenRouter",
        )
        .await;
    }

    #[tokio::test]
    async fn test_managed_provider_matrix_anthropic() {
        run_managed_provider_case(
            MockManagedProviderProtocol::Anthropic,
            "anthropic/claude-sonnet-4-20250514",
            "ANTHROPIC_API_KEY",
            "test-anthropic-key",
            "claude-sonnet-4-20250514",
            "Hello from Anthropic",
        )
        .await;
    }

    #[tokio::test]
    async fn test_managed_provider_matrix_responses() {
        run_managed_provider_case(
            MockManagedProviderProtocol::Responses,
            "openai-responses/gpt-5",
            "OPENAI_API_KEY",
            "test-openai-key",
            "gpt-5",
            "Hello from Responses",
        )
        .await;
    }

    #[test]
    fn test_oai_request_deserialize() {
        let json = r#"{"messages":[{"role":"user","content":"hi"}],"stream":true}"#;
        let req: OaiChatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
        assert_eq!(req.messages[0].content, "hi");
        assert!(req.stream);
        assert!(req.model.is_none());
    }

    #[test]
    fn test_oai_response_serialize() {
        let resp = OaiChatResponse {
            id: "chatcmpl-123".into(),
            object: "chat.completion",
            created: 1700000000,
            model: "hermes".into(),
            choices: vec![OaiChoice {
                index: 0,
                message: OaiMessage {
                    role: "assistant".into(),
                    content: "Hello!".into(),
                },
                finish_reason: "stop",
            }],
            usage: OaiUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["choices"][0]["message"]["role"], "assistant");
        assert_eq!(json["choices"][0]["message"]["content"], "Hello!");
        assert_eq!(json["choices"][0]["finish_reason"], "stop");
        assert_eq!(json["usage"]["total_tokens"], 15);
    }

    #[test]
    fn test_build_oai_prompt_single_user() {
        let msgs = vec![OaiMessage {
            role: "user".into(),
            content: "Hello".into(),
        }];
        assert_eq!(build_oai_prompt(&msgs), "Hello");
    }

    #[test]
    fn test_build_oai_prompt_multi_turn() {
        let msgs = vec![
            OaiMessage {
                role: "system".into(),
                content: "You are helpful.".into(),
            },
            OaiMessage {
                role: "user".into(),
                content: "What is Rust?".into(),
            },
            OaiMessage {
                role: "assistant".into(),
                content: "A systems language.".into(),
            },
            OaiMessage {
                role: "user".into(),
                content: "Tell me more.".into(),
            },
        ];
        let prompt = build_oai_prompt(&msgs);
        assert!(prompt.contains("<conversation-history>"));
        assert!(prompt.contains("[system]: You are helpful."));
        assert!(prompt.contains("[user]: What is Rust?"));
        assert!(prompt.contains("[assistant]: A systems language."));
        assert!(prompt.contains("</conversation-history>"));
        assert!(prompt.ends_with("Tell me more."));
    }

    #[test]
    fn test_build_oai_prompt_system_only() {
        let msgs = vec![
            OaiMessage {
                role: "system".into(),
                content: "Be concise.".into(),
            },
            OaiMessage {
                role: "user".into(),
                content: "Hi".into(),
            },
        ];
        let prompt = build_oai_prompt(&msgs);
        assert!(prompt.contains("[system]: Be concise."));
        assert!(prompt.ends_with("Hi"));
    }
}

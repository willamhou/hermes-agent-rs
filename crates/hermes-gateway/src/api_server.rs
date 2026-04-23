//! API server adapter — REST + OpenAI-compatible `/v1/chat/completions` with SSE.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use dashmap::DashMap;
use hermes_config::config::{ApiServerGatewayConfig, AppConfig};
use hermes_core::{
    error::Result,
    platform::{ChatType, MessageEvent, PlatformAdapter, PlatformEvent},
    stream::StreamDelta,
};
use hermes_managed::{
    ManagedAgent, ManagedAgentVersion, ManagedAgentVersionDraft, ManagedApprovalPolicy,
    ManagedRunEvent, ManagedRunEventDraft, ManagedRunEventKind, ManagedRunStatus, ManagedStore,
    ResolvedManagedVersionDefaults, RunRegistry, build_filtered_skill_manager,
    build_managed_runtime, preflight_managed_model, resolve_managed_version_defaults,
    validate_managed_agent_name, validate_managed_beta_tools,
};
use hermes_tools::session_cleanup;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};
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
struct ManagedApiState {
    shared: Arc<SharedState>,
    app_config: AppConfig,
    store: Arc<ManagedStore>,
    runs: Arc<RunRegistry>,
}

enum ManagedRunOutcome {
    Completed(String),
    Failed(String),
}

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
}

#[derive(Serialize)]
struct ManagedRunListResponse {
    object: &'static str,
    data: Vec<hermes_managed::ManagedRun>,
}

#[derive(Serialize)]
struct ManagedRunEventListResponse {
    object: &'static str,
    data: Vec<ManagedRunEvent>,
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
    ) {
        self.managed
            .lock()
            .expect("managed lock poisoned")
            .replace(ManagedApiState {
                shared,
                app_config,
                store,
                runs,
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

    let data = runs
        .into_iter()
        .map(|run| apply_run_snapshot(run.clone(), managed.runs.snapshot(&run.id)))
        .collect();

    Json(ManagedRunListResponse {
        object: "list",
        data,
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

    Json(ManagedRunEnvelope {
        run: apply_run_snapshot(run, managed.runs.snapshot(&run_id)),
    })
    .into_response()
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
        Ok(events) => Json(ManagedRunEventListResponse {
            object: "list",
            data: events,
        })
        .into_response(),
        Err(e) => managed_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to list managed run events: {e}"),
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

    let current = match managed.store.get_run(&run_id).await {
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

    let final_run = if current.status.is_terminal() {
        current
    } else if managed.runs.snapshot(&run_id).is_some() {
        terminate_managed_run(
            Arc::clone(&managed.store),
            Arc::clone(&managed.runs),
            run_id.clone(),
            ManagedRunStatus::Cancelled,
            Some("cancelled via API".to_string()),
        )
        .await;
        match managed.store.get_run(&run_id).await {
            Ok(Some(run)) => apply_run_snapshot(run, managed.runs.snapshot(&run_id)),
            Ok(None) => {
                return managed_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("managed run disappeared after cancellation: {run_id}"),
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
                &run_id,
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
                &run_id,
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
        match managed.store.get_run(&run_id).await {
            Ok(Some(run)) => run,
            Ok(None) => {
                return managed_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("managed run disappeared after cancellation: {run_id}"),
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

    Json(ManagedRunEnvelope { run: final_run }).into_response()
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

fn oai_error_response(
    status: StatusCode,
    message: impl Into<String>,
    error_type: &'static str,
) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": {
                "message": message.into(),
                "type": error_type
            }
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

fn clamp_agents_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(100).clamp(1, 1000)
}

fn terminal_run_event_kind(status: &ManagedRunStatus) -> Option<ManagedRunEventKind> {
    match status {
        ManagedRunStatus::Completed => Some(ManagedRunEventKind::RunCompleted),
        ManagedRunStatus::Failed => Some(ManagedRunEventKind::RunFailed),
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

async fn run_is_terminal(store: &ManagedStore, runs: &RunRegistry, run_id: &str) -> bool {
    if let Some(snapshot) = runs.snapshot(run_id) {
        return snapshot.status.is_terminal();
    }

    match store.get_run(run_id).await {
        Ok(Some(run)) => run.status.is_terminal(),
        Ok(None) | Err(_) => false,
    }
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

    if status_to_store == ManagedRunStatus::Failed {
        let _ = store
            .record_run_terminal_intent(&run_id, status_to_store.clone(), error_to_store.as_deref())
            .await;
    }

    let _ = store
        .update_run_status(&run_id, status_to_store.clone(), error_to_store.as_deref())
        .await;
    if !was_terminal {
        append_terminal_run_event(store.as_ref(), &run_id, &status_to_store, error_to_store).await;
    }
    log_managed_cleanup(&run_id, session_cleanup::cleanup_session(&run_id));
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

    if matches!(
        status_to_store,
        ManagedRunStatus::Cancelled | ManagedRunStatus::Failed | ManagedRunStatus::TimedOut
    ) {
        let _ = store
            .record_run_terminal_intent(&run_id, status_to_store.clone(), error_to_store.as_deref())
            .await;
    }

    let _ = store
        .update_run_status(&run_id, status_to_store.clone(), error_to_store.as_deref())
        .await;
    if !was_terminal {
        append_terminal_run_event(store.as_ref(), &run_id, &status_to_store, error_to_store).await;
    }
    log_managed_cleanup(&run_id, session_cleanup::cleanup_session(&run_id));
    let _ = runs.remove(&run_id);
}

fn log_managed_cleanup(run_id: &str, summary: session_cleanup::SessionCleanupSummary) {
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
    }
}

async fn spawn_managed_run(
    managed: &ManagedApiState,
    agent_name: &str,
    prompt: String,
) -> std::result::Result<
    (
        String,
        u64,
        broadcast::Receiver<StreamDelta>,
        oneshot::Receiver<ManagedRunOutcome>,
    ),
    Response,
> {
    let (agent, version) = load_managed_agent_version(managed, agent_name).await?;
    spawn_managed_run_with_version(managed, &agent, &version, prompt, None).await
}

async fn spawn_managed_run_with_version(
    managed: &ManagedApiState,
    agent: &ManagedAgent,
    version: &ManagedAgentVersion,
    prompt: String,
    replay_of_run_id: Option<String>,
) -> std::result::Result<
    (
        String,
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

    let runtime = build_managed_runtime(
        agent,
        version,
        managed.shared.registry.as_ref(),
        managed.shared.skills.as_ref(),
        &managed.app_config,
        working_dir,
    )
    .await
    .map_err(|e| {
        oai_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to build managed runtime: {e}"),
            "server_error",
        )
    })?;

    let mut run = runtime.run.clone();
    run.prompt = prompt.clone();
    run.replay_of_run_id = replay_of_run_id.clone();
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
            message: Some(match &replay_of_run_id {
                Some(source_run_id) => format!(
                    "managed run replayed from {source_run_id} for {}@{}",
                    agent.name, version.version
                ),
                None => format!("managed run created for {}@{}", agent.name, version.version),
            }),
            tool_name: None,
            tool_call_id: None,
            metadata: Some(serde_json::json!({
                "prompt_chars": run.prompt.chars().count(),
                "replay_of_run_id": replay_of_run_id,
            })),
        },
    )
    .await;

    let timeout_secs = u64::from(runtime.timeout_secs.max(1));
    let run_id = run.id.clone();
    let (delta_tx, mut delta_rx) = mpsc::channel::<StreamDelta>(64);
    let (broadcast_tx, broadcast_rx) = broadcast::channel::<StreamDelta>(64);
    let (outcome_tx, outcome_rx) = oneshot::channel::<ManagedRunOutcome>();
    let store = Arc::clone(&managed.store);
    let runs = Arc::clone(&managed.runs);
    let run_id_for_task = run_id.clone();
    let prompt_for_task = prompt;
    let mut agent_runner = runtime.agent;

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

        let mut history = Vec::new();
        let result = agent_runner
            .run_conversation(&prompt_for_task, &mut history, delta_tx)
            .await;
        let _ = relay_handle.await;

        match result {
            Ok(text) => {
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

    managed
        .runs
        .register(&run, runtime.timeout_secs, task)
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

    Ok((run_id, timeout_secs, broadcast_rx, outcome_rx))
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

    let source_run = match managed.store.get_run(&run_id).await {
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

    if source_run.prompt.trim().is_empty() {
        return managed_error_response(
            StatusCode::CONFLICT,
            format!("managed run is not replayable yet: {run_id}"),
        );
    }

    let (agent, version) = match load_managed_agent_version_for_run(&managed, &source_run).await {
        Ok(value) => value,
        Err(response) => return response,
    };

    let new_run_id = match spawn_managed_run_with_version(
        &managed,
        &agent,
        &version,
        source_run.prompt.clone(),
        Some(source_run.id.clone()),
    )
    .await
    {
        Ok((run_id, _, _, _)) => run_id,
        Err(response) => return response,
    };

    match managed.store.get_run(&new_run_id).await {
        Ok(Some(run)) => (
            StatusCode::CREATED,
            Json(ManagedRunEnvelope {
                run: apply_run_snapshot(run, managed.runs.snapshot(&new_run_id)),
            }),
        )
            .into_response(),
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

    let prompt = build_oai_prompt(&req.messages);
    let (run_id, timeout_secs, mut stream_rx, outcome_rx) =
        match spawn_managed_run(&managed, &agent_name, prompt).await {
            Ok(value) => value,
            Err(response) => return response,
        };

    if req.stream {
        let rid = request_id.clone();
        let model = model_for_resp.clone();
        let created = epoch_secs();
        let store = Arc::clone(&managed.store);
        let runs = Arc::clone(&managed.runs);
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
                        run_id.clone(),
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
                                run_id.clone(),
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
                    run_id.clone(),
                    ManagedRunStatus::TimedOut,
                    Some(format!("managed run timed out after {timeout_secs}s")),
                )
                .await;
            }
        });

        Sse::new(ReceiverStream::new(rx))
            .keep_alive(
                axum::response::sse::KeepAlive::new()
                    .interval(Duration::from_secs(15))
                    .text(""),
            )
            .into_response()
    } else {
        match tokio::time::timeout(Duration::from_secs(timeout_secs), outcome_rx).await {
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
        }
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
        io,
        sync::{Arc as StdArc, LazyLock, Mutex as StdMutex, OnceLock},
    };

    use super::*;
    use axum::body::Body;
    use hermes_core::{
        error::HermesError,
        provider::{
            ChatRequest as ProviderChatRequest, ChatResponse, ModelInfo, ModelPricing, Provider,
        },
        tool::ToolConfig,
    };
    use hermes_tools::ToolRegistry;
    use http::{Method, Request};
    use http_body_util::BodyExt;
    use tempfile::TempDir;
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

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct MockProviderRequestAudit {
        auth_header: Option<String>,
        api_key_header: Option<String>,
        anthropic_version: Option<String>,
        model: Option<String>,
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
                    adapters: HashMap::new(),
                }),
                app_config,
                store,
                runs: Arc::new(RunRegistry::new()),
            }),
        };

        (tmp, state, agent, version)
    }

    async fn build_managed_test_state() -> (TempDir, ApiState, ManagedAgent, ManagedAgentVersion) {
        build_managed_test_state_with_version(AppConfig::default(), |_| {}).await
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
        run.status = ManagedRunStatus::Running;
        managed.store.create_run(&run).await.unwrap();

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
        assert_eq!(list_json["data"][0]["id"], run.id);

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
        assert_eq!(get_json["run"]["status"], "running");
    }

    #[tokio::test]
    async fn test_managed_run_events_list() {
        let (_tmp, state, agent, _version) = build_managed_test_state().await;
        let managed = state.managed.clone().unwrap();

        let mut run = hermes_managed::ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        managed.store.create_run(&run).await.unwrap();
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
        assert_eq!(json["data"][0]["kind"], "run.created");
        assert_eq!(json["data"][1]["kind"], "tool.progress");
        assert_eq!(json["data"][1]["tool_name"], "read_file");
        assert_eq!(json["data"][1]["message"], "reading README.md");
    }

    #[tokio::test]
    async fn test_managed_run_cancel_endpoint_cancels_active_run() {
        let (_tmp, state, agent, _version) = build_managed_test_state().await;
        let managed = state.managed.clone().unwrap();

        let mut run = hermes_managed::ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;
        managed.store.create_run(&run).await.unwrap();
        let mut child = tokio::process::Command::new("bash")
            .args(["-lc", "sleep 30"])
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let _cleanup_registration =
            hermes_tools::session_cleanup::register_pid(&run.id, pid, "managed cancel test")
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
        assert!(kinds.contains(&"run.started".to_string()));
        assert!(kinds.contains(&"run.completed".to_string()));

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
            vec!["run.started".to_string(), "run.completed".to_string()]
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

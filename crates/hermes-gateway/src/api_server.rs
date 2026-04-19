//! API server adapter — REST + OpenAI-compatible `/v1/chat/completions` with SSE.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Sse, sse::Event},
    routing::{get, post},
};
use dashmap::DashMap;
use hermes_config::config::ApiServerGatewayConfig;
use hermes_core::{
    error::Result,
    platform::{ChatType, MessageEvent, PlatformAdapter, PlatformEvent},
    stream::StreamDelta,
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info};

use crate::session::SessionRouter;

// ─── Adapter ──────────────────────────────────────────────────────────────────

pub struct ApiServerAdapter {
    config: ApiServerGatewayConfig,
    pending: Arc<DashMap<String, oneshot::Sender<String>>>,
    router: std::sync::Mutex<Option<SessionRouter>>,
}

impl ApiServerAdapter {
    pub fn new(config: ApiServerGatewayConfig) -> Self {
        Self {
            config,
            pending: Arc::new(DashMap::new()),
            router: std::sync::Mutex::new(None),
        }
    }
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

// ─── PlatformAdapter impl ─────────────────────────────────────────────────────

impl ApiServerAdapter {
    /// Set the session router for streaming endpoints (called by GatewayRunner after construction).
    pub fn set_router(&self, router: SessionRouter) {
        self.router
            .lock()
            .expect("router lock poisoned")
            .replace(router);
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
        };

        let app = Router::new()
            .route("/api/chat", post(handle_chat))
            .route("/v1/chat/completions", post(handle_oai_chat))
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

async fn handle_oai_chat(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(req): Json<OaiChatRequest>,
) -> impl IntoResponse {
    if let Err(e) = check_bearer_auth(&headers, &state.api_key) {
        return e.into_response();
    }

    // Validate messages before checking router (better error for malformed requests)
    let prompt = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default();

    if prompt.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": {"message": "no user message found", "type": "invalid_request_error"}})),
        )
            .into_response();
    }

    let Some(router) = state.router else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": {"message": "router not available", "type": "server_error"}})),
        )
            .into_response();
    };

    let request_id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());
    let user_id = req.user.unwrap_or_else(|| "api-user".into());
    let model_name = req.model.unwrap_or_else(|| state.model_name.clone());
    let model_for_resp = if model_name.is_empty() {
        "hermes".to_string()
    } else {
        model_name
    };

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
    Json(OaiModelList {
        object: "list",
        data: vec![OaiModel {
            id: model_id,
            object: "model",
            owned_by: "hermes",
        }],
    })
    .into_response()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::{Method, Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn build_app(event_tx: mpsc::Sender<PlatformEvent>) -> Router {
        let state = ApiState {
            event_tx,
            pending: Arc::new(DashMap::new()),
            api_key: None,
            model_name: "test-model".into(),
            router: None,
        };
        Router::new()
            .route("/api/chat", post(handle_chat))
            .route("/v1/chat/completions", post(handle_oai_chat))
            .route("/v1/models", get(handle_oai_models))
            .route("/health", get(handle_health))
            .with_state(state)
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
}

//! API server adapter — exposes a REST interface for synchronous agent interaction.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use dashmap::DashMap;
use hermes_config::config::ApiServerGatewayConfig;
use hermes_core::{
    error::Result,
    platform::{ChatType, MessageEvent, PlatformAdapter, PlatformEvent},
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info};

// ─── Adapter ──────────────────────────────────────────────────────────────────

pub struct ApiServerAdapter {
    config: ApiServerGatewayConfig,
    pending: Arc<DashMap<String, oneshot::Sender<String>>>,
}

impl ApiServerAdapter {
    pub fn new(config: ApiServerGatewayConfig) -> Self {
        Self {
            config,
            pending: Arc::new(DashMap::new()),
        }
    }
}

// ─── Axum state ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ApiState {
    event_tx: mpsc::Sender<PlatformEvent>,
    pending: Arc<DashMap<String, oneshot::Sender<String>>>,
    api_key: Option<String>,
}

// ─── Request / response types ─────────────────────────────────────────────────

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

// ─── PlatformAdapter impl ─────────────────────────────────────────────────────

#[async_trait]
impl PlatformAdapter for ApiServerAdapter {
    fn platform_name(&self) -> &str {
        "api"
    }

    async fn run(self: Arc<Self>, event_tx: mpsc::Sender<PlatformEvent>) -> Result<()> {
        let state = ApiState {
            event_tx,
            pending: Arc::clone(&self.pending),
            api_key: self.config.api_key.clone(),
        };

        let app = Router::new()
            .route("/api/chat", post(handle_chat))
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
            Some(key) if key == expected_key => {}
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
        };
        Router::new()
            .route("/api/chat", post(handle_chat))
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
}

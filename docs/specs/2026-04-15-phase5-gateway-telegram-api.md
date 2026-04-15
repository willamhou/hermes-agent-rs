# Phase 5: Gateway — Telegram + API Server — Design Spec (v2)

**Date**: 2026-04-15
**Status**: Revised after self-review
**Depends on**: Phase 4 (complete)
**Validates**: tokio multi-task, per-session agents, cross-platform adapter trait

---

## 0. Review Fixes (v1 → v2)

| # | Issue | Fix |
|---|-------|-----|
| 1 | AgentConfig has 14 fields, no guidance on shared vs per-session | Added `build_session_agent()` with explicit sharing strategy |
| 2 | `PlatformAdapter::start(&self)` borrows self, can't spawn as 'static | Changed trait to `async fn run(self: Arc<Self>, event_tx)` |
| 3 | API server oneshot has no timeout for long agent runs | Added 300s HTTP request timeout + 504 on expiry |
| 4 | Telegram poll loop has no error recovery | Added exponential backoff (5s → 60s cap) |
| 5 | split_message doesn't account for Telegram UTF-16 counting | Use 4000 char limit (safety margin) |
| 6 | GatewayConfig not Optional in AppConfig | Made `Option<GatewayConfig>` with serde default |

---

## 1. Scope

### In Scope
- GatewayRunner (adapter orchestration, event loop, graceful shutdown)
- SessionRouter (per-session tokio tasks, DashMap, idle cleanup)
- TelegramAdapter (long-polling, message splitting, MarkdownV2, authorization)
- ApiServerAdapter (axum REST endpoints, optional API key auth)
- MessageEvent expansion (chat_type, user_name, thread_id)
- GatewayConfig (YAML section, env var overrides)
- Gateway approval policy (AutoAllow for Phase 5)
- CLI `hermes gateway` subcommand to start

### Out of Scope
- Pairing code system (deferred)
- Image/voice/media handling
- Webhook mode (polling only for Telegram)
- Discord, Slack, other platforms
- SSE streaming responses
- Cross-platform session continuity
- DeliveryRouter for cron output
- Session idle/daily reset policies (simple timeout only)

---

## 2. Architecture

```
                      ┌────────────────────────────────────┐
                      │           GatewayRunner             │
  TelegramAdapter ───►│                                    │
    (long-poll)       │   event_rx ──► SessionRouter       │
                      │                 │                   │
  ApiServerAdapter ──►│                 ├─ "tg:dm:123"     │
    (axum HTTP)       │                 │   └─ Agent task   │
                      │                 ├─ "api:sess_abc"  │
                      │                 │   └─ Agent task   │
                      │                 └─ cleanup task     │
                      │                                    │
                      │   response ◄── Agent ──► Tools     │
                      │     │                              │
                      │     └──► adapter.send_response()   │
                      └────────────────────────────────────┘
```

### Data Flow

1. Platform adapter receives raw message → converts to `PlatformEvent::Message(MessageEvent)`
2. GatewayRunner receives from `event_rx` channel
3. SessionRouter looks up or creates session by key
4. Session task receives message → runs `agent.run_conversation()`
5. Agent response → `adapter.send_response()` back to platform

### Session Lifecycle

```
Message arrives → derive session_key
  → DashMap lookup:
    Found → send to existing session task
    Not found → spawn new session task (Agent + history + store)
  → session task processes message sequentially
  → idle cleanup task periodically evicts stale sessions
```

---

## 3. MessageEvent Expansion

Current (hermes-core/src/platform.rs):
```rust
pub struct MessageEvent {
    pub platform: String,
    pub chat_id: String,
    pub user_id: String,
    pub text: String,
    pub reply_to: Option<String>,
}
```

Phase 5 additions:
```rust
#[derive(Debug, Clone)]
pub struct MessageEvent {
    pub platform: String,
    pub chat_id: String,
    pub user_id: String,
    pub user_name: Option<String>,
    pub text: String,
    pub reply_to: Option<String>,
    pub chat_type: ChatType,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatType {
    DirectMessage,
    Group,
    Channel,
}
```

### PlatformAdapter Trait (Revised)

The trait's `start()` method needs to own `self` for spawning as 'static tokio task.
Change from `&self` to `self: Arc<Self>`:

```rust
#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    fn platform_name(&self) -> &str;
    async fn run(self: Arc<Self>, event_tx: mpsc::Sender<PlatformEvent>) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn send_response(&self, event: &MessageEvent, response: &str) -> Result<()>;
}
```

`run()` replaces `start()` — it takes ownership via Arc and runs until shutdown.
`stop()` and `send_response()` use `&self` (called from outside the run loop via Arc clone).

---

## 4. GatewayRunner

```rust
// hermes-gateway/src/runner.rs
pub struct GatewayRunner {
    config: Arc<GatewayConfig>,
    app_config: AppConfig,
}

impl GatewayRunner {
    pub async fn run(&self) -> Result<()> {
        let (event_tx, mut event_rx) = mpsc::channel::<PlatformEvent>(256);
        let mut adapters: HashMap<String, Arc<dyn PlatformAdapter>> = HashMap::new();

        // Start adapters (Arc<Self> for 'static spawn)
        let mut adapter_handles = Vec::new();
        if let Some(tg_config) = &self.config.telegram {
            let adapter = Arc::new(TelegramAdapter::new(tg_config.clone())?);
            adapters.insert("telegram".into(), Arc::clone(&adapter) as Arc<dyn PlatformAdapter>);
            let tx = event_tx.clone();
            adapter_handles.push(tokio::spawn(async move {
                adapter.run(tx).await  // adapter: Arc<TelegramAdapter>
            }));
        }
        if let Some(api_config) = &self.config.api_server {
            let adapter = Arc::new(ApiServerAdapter::new(api_config.clone()));
            adapters.insert("api".into(), Arc::clone(&adapter) as Arc<dyn PlatformAdapter>);
            let tx = event_tx.clone();
            adapter_handles.push(tokio::spawn(async move {
                adapter.run(tx).await
            }));
        }
        drop(event_tx); // only adapters hold senders

        // Session router
        let router = SessionRouter::new(
            Arc::clone(&self.config),
            self.app_config.clone(),
        );

        // Spawn idle cleanup task
        let router_ref = router.clone();
        let cleanup_handle = tokio::spawn(async move {
            router_ref.cleanup_loop().await;
        });

        // Main event loop
        while let Some(event) = event_rx.recv().await {
            match event {
                PlatformEvent::Message(msg_event) => {
                    router.route(msg_event).await;
                }
                PlatformEvent::Shutdown => break,
            }
        }

        // Graceful shutdown
        cleanup_handle.abort();
        for handle in adapter_handles {
            handle.abort();
        }
        router.shutdown().await;
        Ok(())
    }
}
```

---

## 5. SessionRouter

```rust
// hermes-gateway/src/session.rs
pub struct SessionRouter {
    sessions: Arc<DashMap<String, SessionHandle>>,
    config: Arc<GatewayConfig>,
    app_config: AppConfig,
}

struct SessionHandle {
    msg_tx: mpsc::Sender<RoutedMessage>,
    last_active: Arc<AtomicU64>,  // epoch seconds
}

struct RoutedMessage {
    event: MessageEvent,
    response_tx: oneshot::Sender<String>,  // for API server sync response
}
```

### Session Key Derivation

```rust
fn session_key(event: &MessageEvent) -> String {
    match event.chat_type {
        ChatType::DirectMessage => format!("{}:dm:{}", event.platform, event.user_id),
        ChatType::Group => format!("{}:group:{}:{}", event.platform, event.chat_id, event.user_id),
        ChatType::Channel => format!("{}:chan:{}", event.platform, event.chat_id),
    }
}
```

Groups get per-user isolation (each user gets own agent context).

### build_session_agent — Sharing Strategy

AgentConfig has 14 fields. In gateway mode, some are **shared across all sessions** (Arc clone) and some are **created per-session**:

```rust
/// Shared state across all sessions (created once at gateway startup)
struct SharedGatewayState {
    provider: Arc<dyn Provider>,         // Arc clone — stateless, reqwest::Client is Arc internally
    registry: Arc<ToolRegistry>,         // Arc clone — read-only after init
    tool_config: Arc<ToolConfig>,        // Arc clone — immutable
    skills: Option<Arc<RwLock<SkillManager>>>,  // Arc clone — shared skill discovery
    adapters: HashMap<String, Arc<dyn PlatformAdapter>>,
}

/// Per-session state (created fresh for each new session)
fn build_session_agent(
    session_id: &str,
    shared: &SharedGatewayState,
    app_config: &AppConfig,
) -> Agent {
    // Per-session: independent memory, budget, approval, compression
    let memory = MemoryManager::new(hermes_home().join("memories"), None).unwrap();
    let (approval_tx, mut approval_rx) = mpsc::channel(8);
    tokio::spawn(async move {
        while let Some(req) = approval_rx.recv().await {
            // Gateway: auto-allow (no interactive UI)
            let _ = req.response_tx.send(ApprovalDecision::Allow);
        }
    });

    Agent::new(AgentConfig {
        provider: Arc::clone(&shared.provider),        // shared
        registry: Arc::clone(&shared.registry),        // shared
        tool_config: Arc::clone(&shared.tool_config),  // shared
        skills: shared.skills.clone(),                  // shared
        max_iterations: app_config.max_iterations,
        system_prompt: "You are Hermes, a helpful AI assistant.".into(),
        session_id: session_id.into(),
        working_dir: std::env::current_dir().unwrap_or_default(),
        approval_tx,                                    // per-session
        memory,                                         // per-session
        compression: CompressionConfig::default(),      // per-session
        delegation_depth: 0,
        clarify_tx: None,  // no clarify in gateway
    })
}
```

### Session Task

```rust
async fn session_task(
    session_id: String,
    mut msg_rx: mpsc::Receiver<RoutedMessage>,
    shared: Arc<SharedGatewayState>,
    app_config: AppConfig,
) {
    let mut agent = build_session_agent(&session_id, &shared, &app_config);
    let mut history = Vec::new();

    while let Some(routed) = msg_rx.recv().await {
        // Gateway discards streaming deltas (no terminal to render to)
        let (delta_tx, _delta_rx) = mpsc::channel(64);
        let result = agent.run_conversation(&routed.event.text, &mut history, delta_tx).await;

        let response = match result {
            Ok(text) => text,
            Err(e) => format!("Error: {e}"),
        };

        // Send back to originating platform
        if let Some(adapter) = shared.adapters.get(&routed.event.platform) {
            let _ = adapter.send_response(&routed.event, &response).await;
        }

        // For API server: send sync response via oneshot
        let _ = routed.response_tx.send(response);
    }
}
```

### Idle Cleanup

```rust
async fn cleanup_loop(&self) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        let now = epoch_secs();
        let timeout = self.config.session_idle_timeout_secs;
        self.sessions.retain(|_key, handle| {
            now - handle.last_active.load(Ordering::Relaxed) < timeout
        });
    }
}
```

---

## 6. TelegramAdapter

```rust
// hermes-gateway/src/telegram.rs
pub struct TelegramAdapter {
    config: TelegramConfig,
    client: reqwest::Client,
}
```

### Long-Polling Loop with Error Recovery

```rust
async fn run(self: Arc<Self>, event_tx: mpsc::Sender<PlatformEvent>) -> Result<()> {
    let mut offset: i64 = 0;
    let mut backoff_secs: u64 = 0;

    loop {
        // Backoff after errors
        if backoff_secs > 0 {
            tracing::warn!(backoff_secs, "telegram: reconnecting after error");
            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        }

        match self.get_updates(offset, 30).await {
            Ok(updates) => {
                backoff_secs = 0; // reset on success
                for update in updates {
                    offset = update.update_id + 1;
                    if let Some(msg) = update.message {
                        if !self.is_authorized(&msg) { continue; }
                        let event = self.to_message_event(&msg);
                        let _ = event_tx.send(PlatformEvent::Message(event)).await;
                    }
                }
            }
            Err(e) => {
                tracing::error!("telegram poll error: {e}");
                backoff_secs = (backoff_secs * 2).max(5).min(60); // 5s → 10s → 20s → 40s → 60s cap
            }
        }
    }
}
```

### Authorization

Simple allowlist (no pairing codes in Phase 5):
```rust
fn is_authorized(&self, msg: &TelegramMessage) -> bool {
    if self.config.allow_all { return true; }
    let user_id = msg.from.as_ref().map(|u| u.id.to_string()).unwrap_or_default();
    self.config.allowed_users.contains(&user_id)
}
```

### Response Sending

```rust
async fn send_response(&self, event: &MessageEvent, response: &str) -> Result<()> {
    let chunks = split_message(response, 4096);
    for (i, chunk) in chunks.iter().enumerate() {
        let suffix = if chunks.len() > 1 {
            format!(" ({}/{})", i + 1, chunks.len())
        } else {
            String::new()
        };
        let text = format!("{chunk}{suffix}");
        self.send_message(&event.chat_id, &text, event.thread_id.as_deref()).await?;
    }
    Ok(())
}
```

`split_message()`: split at 4000 chars (safety margin for Telegram's UTF-16 counting) at newline boundaries when possible, not mid-word. Use `floor_char_boundary` for UTF-8 safety.

### Telegram API Types (minimal)

```rust
#[derive(Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Deserialize)]
struct TelegramMessage {
    message_id: i64,
    from: Option<TelegramUser>,
    chat: TelegramChat,
    text: Option<String>,
    message_thread_id: Option<i64>,
}

#[derive(Deserialize)]
struct TelegramUser { id: i64, first_name: String, username: Option<String> }

#[derive(Deserialize)]
struct TelegramChat { id: i64, #[serde(rename = "type")] chat_type: String }
```

Only handle `text` messages for Phase 5 (ignore photos, stickers, etc.).

---

## 7. ApiServerAdapter

Uses `axum` for HTTP:

```rust
// hermes-gateway/src/api_server.rs
pub struct ApiServerAdapter {
    config: ApiServerConfig,
}
```

### Endpoints

```
POST /api/chat
  Headers: Authorization: Bearer <api_key> (if configured)
  Body: { "message": "string", "session_id": "optional", "user_id": "optional" }
  Response: { "response": "string", "session_id": "string" }

GET /api/sessions
  Response: [{ "id": "string", "last_active": "string" }]

DELETE /api/sessions/{id}
  Response: { "ok": true }

GET /health
  Response: { "status": "ok" }
```

### Implementation

```rust
async fn start(&self, event_tx: mpsc::Sender<PlatformEvent>) -> Result<()> {
    let app = Router::new()
        .route("/api/chat", post(handle_chat))
        .route("/api/sessions", get(handle_list_sessions))
        .route("/api/sessions/:id", delete(handle_delete_session))
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .layer(/* auth middleware if api_key configured */);

    let listener = TcpListener::bind(&self.config.bind_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

The `/api/chat` handler:
1. Parse request body
2. Create `MessageEvent` with platform="api", chat_type=DM
3. Send to `event_tx`
4. Wait for response via `oneshot` channel with **300s timeout**
5. On timeout: return HTTP 504 Gateway Timeout
6. On success: return JSON response

```rust
let response = tokio::time::timeout(
    Duration::from_secs(300),
    response_rx,
).await;

match response {
    Ok(Ok(text)) => Json(json!({"response": text, "session_id": session_id})),
    Ok(Err(_)) => /* 500 Internal Error */,
    Err(_) => /* 504 Gateway Timeout */,
}
```

---

## 8. GatewayConfig

Added to `AppConfig`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GatewayConfig {
    #[serde(default)]
    pub telegram: Option<TelegramConfig>,
    #[serde(default)]
    pub api_server: Option<ApiServerConfig>,
    #[serde(default = "default_session_idle_timeout")]
    pub session_idle_timeout_secs: u64,  // default 1800
    #[serde(default = "default_max_sessions")]
    pub max_concurrent_sessions: usize,  // default 100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    pub token: String,  // from env: TELEGRAM_BOT_TOKEN
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default)]
    pub allow_all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiServerConfig {
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,  // "0.0.0.0:8080"
    pub api_key: Option<String>,  // from env: HERMES_API_KEY
}
```

Added to AppConfig as Optional:
```rust
// In AppConfig:
#[serde(default)]
pub gateway: Option<GatewayConfig>,
```

Config YAML example:
```yaml
gateway:
  session_idle_timeout_secs: 1800
  telegram:
    token: "${TELEGRAM_BOT_TOKEN}"
    allow_all: false
    allowed_users: ["123456"]
  api_server:
    bind_addr: "0.0.0.0:8080"
```

Telegram token loaded from env var if starts with `$`.

---

## 9. CLI Entry Point

Add `gateway` subcommand to hermes CLI:

```rust
// main.rs
#[derive(Subcommand)]
enum Commands {
    /// Start the gateway server
    Gateway,
}
```

`hermes gateway` starts the GatewayRunner with config from `~/.hermes/config.yaml`.

---

## 10. File Structure

### New files
```
crates/hermes-gateway/src/runner.rs         # GatewayRunner
crates/hermes-gateway/src/session.rs        # SessionRouter, session_task
crates/hermes-gateway/src/telegram.rs       # TelegramAdapter
crates/hermes-gateway/src/api_server.rs     # ApiServerAdapter (axum)
crates/hermes-gateway/src/message_split.rs  # split_message utility
```

### Modified files
```
crates/hermes-core/src/platform.rs          # MessageEvent expansion (ChatType, etc.)
crates/hermes-config/src/config.rs          # Add GatewayConfig to AppConfig
crates/hermes-gateway/src/lib.rs            # Wire modules
crates/hermes-gateway/Cargo.toml            # Add axum, dashmap deps
crates/hermes-cli/src/main.rs              # Add gateway subcommand
Cargo.toml                                  # Add axum to workspace deps
```

### New dependencies
```toml
axum = "0.8"
tower = "0.5"
tower-http = { version = "0.6", features = ["cors"] }
```

---

## 11. Testing Strategy

| Component | Tests |
|-----------|-------|
| SessionRouter | session_key derivation (DM, group, channel), session creation, idle cleanup |
| message_split | split at boundaries, exact 4096, short messages no-op, multi-chunk numbering |
| TelegramAdapter | parse Update JSON, authorization check, message event conversion |
| ApiServerAdapter | /health endpoint, /api/chat roundtrip, auth middleware, missing session |
| GatewayConfig | YAML parsing, defaults, env var token |
| MessageEvent | ChatType derive, new fields serde |
| Integration | Full gateway with mock adapters: message → session → agent → response |

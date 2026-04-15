# Phase 5: Gateway (Telegram + API Server) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a multi-platform gateway that runs Telegram and HTTP API adapters, routing messages to per-session agent instances.

**Architecture:** Revise PlatformAdapter trait for Arc-based spawning. Build GatewayRunner (adapter orchestration), SessionRouter (DashMap per-session tasks), TelegramAdapter (long-poll with backoff), ApiServerAdapter (axum REST with timeout). Add `hermes gateway` CLI subcommand.

**Tech Stack:** axum 0.8, tokio, dashmap, reqwest (Telegram API), serde

**Review fixes applied:** Arc-based trait, 300s API timeout, exponential backoff for Telegram, 4000 char message split, build_session_agent with shared/per-session strategy, GatewayConfig as Option.

---

## File Structure

### New files
```
crates/hermes-gateway/src/runner.rs         # GatewayRunner
crates/hermes-gateway/src/session.rs        # SessionRouter + session_task + build_session_agent
crates/hermes-gateway/src/telegram.rs       # TelegramAdapter (long-poll)
crates/hermes-gateway/src/api_server.rs     # ApiServerAdapter (axum)
crates/hermes-gateway/src/message_split.rs  # split_message utility
```

### Modified files
```
crates/hermes-core/src/platform.rs          # MessageEvent + ChatType + revised PlatformAdapter
crates/hermes-config/src/config.rs          # GatewayConfig, TelegramConfig, ApiServerConfig
crates/hermes-gateway/src/lib.rs            # Wire modules, re-exports
crates/hermes-gateway/Cargo.toml            # Add axum + deps
crates/hermes-cli/src/main.rs              # Add gateway subcommand
Cargo.toml                                  # Add axum to workspace deps
```

---

## Task 1: MessageEvent Expansion + PlatformAdapter Trait Revision

Expand MessageEvent with ChatType and revise PlatformAdapter for Arc-based spawning.

**Files:**
- Modify: `crates/hermes-core/src/platform.rs`

- [ ] **Step 1: Revise platform.rs**

Read the current file first. Replace with expanded version:

```rust
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatType {
    DirectMessage,
    Group,
    Channel,
}

impl Default for ChatType {
    fn default() -> Self { Self::DirectMessage }
}

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

#[derive(Debug)]
pub enum PlatformEvent {
    Message(MessageEvent),
    Shutdown,
}

/// Platform adapter trait. Implementors are wrapped in Arc for spawning.
/// `run()` takes `self: Arc<Self>` so it can be spawned as a 'static task.
#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    fn platform_name(&self) -> &str;
    /// Run the adapter's event loop. Takes Arc<Self> for 'static spawning.
    async fn run(self: Arc<Self>, event_tx: mpsc::Sender<PlatformEvent>) -> Result<()>;
    async fn send_response(&self, event: &MessageEvent, response: &str) -> Result<()>;
}
```

Note: removed `stop()` — shutdown via dropping the event_tx sender (adapter's `run()` loop exits when channel closes).

- [ ] **Step 2: Verify compilation**

Run: `cargo check --workspace`

Compilation should succeed since no code currently calls PlatformAdapter methods (gateway is a stub).

- [ ] **Step 3: Commit**

Commit: `feat: expand MessageEvent with ChatType and revise PlatformAdapter trait`

---

## Task 2: GatewayConfig + Message Split Utility

Add gateway configuration to AppConfig and implement message splitting.

**Files:**
- Modify: `crates/hermes-config/src/config.rs`
- Create: `crates/hermes-gateway/src/message_split.rs`
- Modify: `crates/hermes-gateway/src/lib.rs`
- Modify: `crates/hermes-gateway/Cargo.toml`
- Modify: `Cargo.toml` (workspace deps)

- [ ] **Step 1: Add GatewayConfig to AppConfig**

In `crates/hermes-config/src/config.rs`, add:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub telegram: Option<TelegramGatewayConfig>,
    #[serde(default)]
    pub api_server: Option<ApiServerGatewayConfig>,
    #[serde(default = "default_session_idle_timeout")]
    pub session_idle_timeout_secs: u64,
    #[serde(default = "default_max_sessions")]
    pub max_concurrent_sessions: usize,
}

fn default_session_idle_timeout() -> u64 { 1800 }
fn default_max_sessions() -> usize { 100 }

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            telegram: None,
            api_server: None,
            session_idle_timeout_secs: default_session_idle_timeout(),
            max_concurrent_sessions: default_max_sessions(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramGatewayConfig {
    pub token: String,
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default)]
    pub allow_all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiServerGatewayConfig {
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    #[serde(default)]
    pub api_key: Option<String>,
}

fn default_bind_addr() -> String { "0.0.0.0:8080".into() }

impl Default for ApiServerGatewayConfig {
    fn default() -> Self {
        Self { bind_addr: default_bind_addr(), api_key: None }
    }
}
```

Add to AppConfig:
```rust
#[serde(default)]
pub gateway: Option<GatewayConfig>,
```

Add config test: `test_gateway_config_parsing` — verify YAML with telegram section parses correctly.

- [ ] **Step 2: Add axum to workspace deps**

Add to root `Cargo.toml` [workspace.dependencies]:
```toml
axum = "0.8"
```

Add to `crates/hermes-gateway/Cargo.toml` [dependencies]:
```toml
axum.workspace = true
hermes-skills.workspace = true
hermes-memory.workspace = true
```

Add [dev-dependencies]:
```toml
[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 3: Implement message_split.rs**

Create `crates/hermes-gateway/src/message_split.rs`:

```rust
/// Split a message into chunks that fit within Telegram's character limit.
/// Uses 4000 chars (safety margin for UTF-16 counting).
/// Prefers splitting at newline boundaries.
const MAX_CHUNK_CHARS: usize = 4000;

pub fn split_message(text: &str, max_chars: usize) -> Vec<String> {
    if text.chars().count() <= max_chars {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.chars().count() <= max_chars {
            chunks.push(remaining.to_string());
            break;
        }

        // Find a split point at a newline boundary within max_chars
        let char_boundary = remaining
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());

        let split_at = remaining[..char_boundary]
            .rfind('\n')
            .map(|i| i + 1) // include the newline in current chunk
            .unwrap_or(char_boundary); // no newline found, split at char boundary

        let (chunk, rest) = remaining.split_at(split_at);
        chunks.push(chunk.to_string());
        remaining = rest;
    }

    // Add numbering if multi-chunk
    if chunks.len() > 1 {
        let total = chunks.len();
        chunks = chunks
            .into_iter()
            .enumerate()
            .map(|(i, chunk)| format!("{chunk}\n({}/{})", i + 1, total))
            .collect();
    }

    chunks
}

pub fn split_telegram(text: &str) -> Vec<String> {
    split_message(text, MAX_CHUNK_CHARS)
}
```

8 tests:
- `test_short_message_no_split` — under limit, returns single chunk
- `test_exact_limit` — exactly 4000 chars, no split
- `test_split_at_newline` — prefers newline boundary
- `test_split_no_newline` — falls back to char boundary
- `test_multi_chunk_numbering` — chunks get (1/N) suffix
- `test_empty_message` — returns single empty chunk
- `test_unicode_safety` — Chinese text, no panic on boundaries
- `test_very_long_message` — 20000 chars, verify 5 chunks

- [ ] **Step 4: Wire up lib.rs**

Replace `crates/hermes-gateway/src/lib.rs`:
```rust
pub mod message_split;
```

- [ ] **Step 5: Run tests + commit**

Commit: `feat: add GatewayConfig and message splitting utility`

---

## Task 3: SessionRouter

Per-session agent management with DashMap and idle cleanup.

**Files:**
- Create: `crates/hermes-gateway/src/session.rs`
- Modify: `crates/hermes-gateway/src/lib.rs`

- [ ] **Step 1: Implement session.rs**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use hermes_agent::{Agent, AgentConfig};
use hermes_agent::compressor::CompressionConfig;
use hermes_config::config::{AppConfig, GatewayConfig, hermes_home};
use hermes_core::message::Message;
use hermes_core::platform::{ChatType, MessageEvent, PlatformAdapter, PlatformEvent};
use hermes_core::stream::StreamDelta;
use hermes_core::tool::ApprovalDecision;
use hermes_memory::MemoryManager;
use hermes_provider::create_provider;
use hermes_tools::ToolRegistry;
use secrecy::SecretString;
use tokio::sync::{mpsc, oneshot};

/// Message routed to a session task.
pub struct RoutedMessage {
    pub event: MessageEvent,
    pub response_tx: oneshot::Sender<String>,
}

/// Handle to a running session.
struct SessionHandle {
    msg_tx: mpsc::Sender<RoutedMessage>,
    last_active: Arc<AtomicU64>,
}

/// Shared state across all gateway sessions.
pub struct SharedState {
    pub provider: Arc<dyn hermes_core::provider::Provider>,
    pub registry: Arc<ToolRegistry>,
    pub tool_config: Arc<hermes_core::tool::ToolConfig>,
    pub skills: Option<Arc<tokio::sync::RwLock<hermes_skills::SkillManager>>>,
    pub adapters: HashMap<String, Arc<dyn PlatformAdapter>>,
}

pub struct SessionRouter {
    sessions: Arc<DashMap<String, SessionHandle>>,
    shared: Arc<SharedState>,
    gateway_config: Arc<GatewayConfig>,
    app_config: AppConfig,
}
```

Key methods:
- `new(shared, gateway_config, app_config) -> Self`
- `route(&self, event: MessageEvent) -> String` — derive session key, get-or-create session, send message, await response
- `session_key(event: &MessageEvent) -> String` — `{platform}:{chat_type}:{chat_id}:{user_id}`
- `cleanup_stale(&self)` — remove sessions idle > timeout
- `shutdown(&self)` — drop all session handles

`build_session_agent()` as a free function:
```rust
fn build_session_agent(
    session_id: &str,
    shared: &SharedState,
    app_config: &AppConfig,
) -> Agent {
    let memory = MemoryManager::new(hermes_home().join("memories"), None)
        .expect("failed to create memory");
    let (approval_tx, mut approval_rx) = mpsc::channel(8);
    tokio::spawn(async move {
        while let Some(req) = approval_rx.recv().await {
            let _ = req.response_tx.send(ApprovalDecision::Allow);
        }
    });
    Agent::new(AgentConfig {
        provider: Arc::clone(&shared.provider),
        registry: Arc::clone(&shared.registry),
        tool_config: Arc::clone(&shared.tool_config),
        skills: shared.skills.clone(),
        max_iterations: app_config.max_iterations,
        system_prompt: "You are Hermes, a helpful AI assistant.".into(),
        session_id: session_id.into(),
        working_dir: std::env::current_dir().unwrap_or_default(),
        approval_tx,
        memory,
        compression: CompressionConfig::default(),
        delegation_depth: 0,
        clarify_tx: None,
    })
}
```

`session_task()`:
```rust
async fn session_task(
    session_id: String,
    mut msg_rx: mpsc::Receiver<RoutedMessage>,
    shared: Arc<SharedState>,
    app_config: AppConfig,
    last_active: Arc<AtomicU64>,
) {
    let mut agent = build_session_agent(&session_id, &shared, &app_config);
    let mut history = Vec::new();

    while let Some(routed) = msg_rx.recv().await {
        last_active.store(epoch_secs(), Ordering::Relaxed);
        let (delta_tx, _) = mpsc::channel::<StreamDelta>(64);
        let result = agent.run_conversation(&routed.event.text, &mut history, delta_tx).await;
        let response = match result {
            Ok(text) => text,
            Err(e) => format!("Error: {e}"),
        };
        // Send to platform
        if let Some(adapter) = shared.adapters.get(&routed.event.platform) {
            let _ = adapter.send_response(&routed.event, &response).await;
        }
        // Send sync response (for API server)
        let _ = routed.response_tx.send(response);
    }
}
```

6 tests:
- `test_session_key_dm` — DM → `{platform}:dm:{user_id}`
- `test_session_key_group` — Group → per-user isolation
- `test_session_key_channel` — Channel → shared
- `test_build_session_agent` — verify agent construction doesn't panic
- `test_cleanup_stale_removes_old` — create handle with old timestamp, cleanup removes it
- `test_cleanup_stale_keeps_active` — recent handle survives cleanup

- [ ] **Step 2: Wire up + commit**

Add `pub mod session;` to lib.rs.

Commit: `feat: implement SessionRouter with per-session agent tasks`

---

## Task 4: TelegramAdapter

Long-polling Telegram bot with authorization and error recovery.

**Files:**
- Create: `crates/hermes-gateway/src/telegram.rs`
- Modify: `crates/hermes-gateway/src/lib.rs`

- [ ] **Step 1: Implement telegram.rs**

Telegram API types (minimal, serde Deserialize):
```rust
#[derive(Deserialize)]
struct TgResponse<T> { ok: bool, result: Option<T> }

#[derive(Deserialize)]
struct TgUpdate { update_id: i64, message: Option<TgMessage> }

#[derive(Deserialize)]
struct TgMessage {
    message_id: i64,
    from: Option<TgUser>,
    chat: TgChat,
    text: Option<String>,
    message_thread_id: Option<i64>,
}

#[derive(Deserialize)]
struct TgUser { id: i64, first_name: String, username: Option<String> }

#[derive(Deserialize)]
struct TgChat { id: i64, #[serde(rename = "type")] chat_type: String }
```

TelegramAdapter:
```rust
pub struct TelegramAdapter {
    token: String,
    client: reqwest::Client,
    allowed_users: HashSet<String>,
    allow_all: bool,
}
```

Methods:
- `new(config: TelegramGatewayConfig) -> Self`
- `impl PlatformAdapter`:
  - `platform_name()` → "telegram"
  - `run(self: Arc<Self>, event_tx)` — long-poll loop with exponential backoff (5s → 60s cap on error, reset on success)
  - `send_response(event, response)` — split via `split_telegram()`, send via `sendMessage` API, retry 3 times on failure
- `get_updates(&self, offset, timeout) -> Result<Vec<TgUpdate>>`
- `send_message(&self, chat_id, text, thread_id) -> Result<()>`
- `is_authorized(&self, msg: &TgMessage) -> bool`
- `to_message_event(&self, msg: &TgMessage) -> MessageEvent` — map chat.type to ChatType

5 tests:
- `test_telegram_parse_update` — deserialize sample JSON
- `test_telegram_authorization_allow_all` — allow_all=true
- `test_telegram_authorization_allowlist` — specific user allowed
- `test_telegram_authorization_denied` — user not in list
- `test_to_message_event` — verify ChatType mapping (private→DM, group→Group, supergroup→Group)

- [ ] **Step 2: Wire up + commit**

Commit: `feat: implement TelegramAdapter with long-polling and authorization`

---

## Task 5: ApiServerAdapter

Axum-based REST API with timeout and optional auth.

**Files:**
- Create: `crates/hermes-gateway/src/api_server.rs`
- Modify: `crates/hermes-gateway/src/lib.rs`

- [ ] **Step 1: Implement api_server.rs**

```rust
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router, Json,
    extract::State,
    http::StatusCode,
    routing::{get, post, delete},
};
use hermes_config::config::ApiServerGatewayConfig;
use hermes_core::platform::{ChatType, MessageEvent, PlatformAdapter, PlatformEvent};
use tokio::sync::{mpsc, oneshot};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct ApiServerAdapter {
    config: ApiServerGatewayConfig,
}

#[derive(Deserialize)]
struct ChatRequest {
    message: String,
    session_id: Option<String>,
    user_id: Option<String>,
}

#[derive(Serialize)]
struct ChatResponse {
    response: String,
    session_id: String,
}
```

`impl PlatformAdapter`:
- `platform_name()` → "api"
- `run(self: Arc<Self>, event_tx)` — start axum server on bind_addr
- `send_response()` — no-op (API server uses oneshot for sync response)

Axum handlers:
- `POST /api/chat` — create MessageEvent, send to event_tx, await oneshot with 300s timeout, return JSON or 504
- `GET /health` — return `{"status": "ok"}`

Auth middleware: if `api_key` configured, check `Authorization: Bearer {key}` header, return 401 on mismatch.

The handler needs access to event_tx. Use axum State:
```rust
#[derive(Clone)]
struct ApiState {
    event_tx: mpsc::Sender<PlatformEvent>,
    api_key: Option<String>,
}
```

3 tests:
- `test_health_endpoint` — start server in test, GET /health → 200
- `test_chat_endpoint_format` — verify request/response JSON shapes
- `test_auth_middleware` — with api_key set, verify 401 without header

For integration testing: use `axum::Router` directly with `axum_test` or `tower::ServiceExt` without starting a real server.

- [ ] **Step 2: Wire up + commit**

Commit: `feat: implement ApiServerAdapter with axum REST endpoints`

---

## Task 6: GatewayRunner

Orchestrates adapters and routes messages through SessionRouter.

**Files:**
- Create: `crates/hermes-gateway/src/runner.rs`
- Modify: `crates/hermes-gateway/src/lib.rs`

- [ ] **Step 1: Implement runner.rs**

```rust
pub struct GatewayRunner {
    gateway_config: GatewayConfig,
    app_config: AppConfig,
}

impl GatewayRunner {
    pub fn new(gateway_config: GatewayConfig, app_config: AppConfig) -> Self {
        Self { gateway_config, app_config }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        // 1. Build shared state (provider, registry, tool_config, skills)
        let api_key = self.app_config.api_key()
            .ok_or_else(|| anyhow::anyhow!("No API key configured"))?;
        let provider = create_provider(&self.app_config.model, SecretString::new(api_key.into()), None)?;
        let registry = Arc::new(build_registry(&self.app_config).await);
        let tool_config = Arc::new(self.app_config.tool_config(std::env::current_dir()?));
        // Skills setup (if skills dir exists)
        let skills = /* ... same as repl.rs pattern ... */;

        // 2. Create adapters
        let mut adapters: HashMap<String, Arc<dyn PlatformAdapter>> = HashMap::new();
        let (event_tx, mut event_rx) = mpsc::channel::<PlatformEvent>(256);
        let mut adapter_handles = Vec::new();

        if let Some(ref tg_config) = self.gateway_config.telegram {
            let adapter = Arc::new(TelegramAdapter::new(tg_config.clone()));
            adapters.insert("telegram".into(), adapter.clone() as _);
            let tx = event_tx.clone();
            adapter_handles.push(tokio::spawn(async move { adapter.run(tx).await }));
        }
        if let Some(ref api_config) = self.gateway_config.api_server {
            let adapter = Arc::new(ApiServerAdapter::new(api_config.clone()));
            adapters.insert("api".into(), adapter.clone() as _);
            let tx = event_tx.clone();
            adapter_handles.push(tokio::spawn(async move { adapter.run(tx).await }));
        }
        drop(event_tx);

        // 3. Build session router
        let shared = Arc::new(SharedState { provider, registry, tool_config, skills, adapters });
        let router = SessionRouter::new(shared, Arc::new(self.gateway_config.clone()), self.app_config.clone());

        // 4. Spawn cleanup task
        let router_clone = router.clone();
        let cleanup = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop { interval.tick().await; router_clone.cleanup_stale(); }
        });

        // 5. Main loop
        tracing::info!("gateway started");
        while let Some(event) = event_rx.recv().await {
            match event {
                PlatformEvent::Message(msg) => {
                    let router = router.clone();
                    tokio::spawn(async move { router.route(msg).await; });
                }
                PlatformEvent::Shutdown => break,
            }
        }

        cleanup.abort();
        router.shutdown();
        tracing::info!("gateway stopped");
        Ok(())
    }
}
```

- [ ] **Step 2: Wire up lib.rs**

```rust
pub mod api_server;
pub mod message_split;
pub mod runner;
pub mod session;
pub mod telegram;

pub use runner::GatewayRunner;
```

- [ ] **Step 3: Compile check + commit**

Run: `cargo check --workspace`

Commit: `feat: implement GatewayRunner with adapter orchestration`

---

## Task 7: CLI Gateway Subcommand

Add `hermes gateway` subcommand.

**Files:**
- Modify: `crates/hermes-cli/src/main.rs`
- Modify: `crates/hermes-cli/Cargo.toml`

- [ ] **Step 1: Add hermes-gateway dependency to CLI**

Add to `crates/hermes-cli/Cargo.toml` [dependencies]:
```toml
hermes-gateway.workspace = true
```

- [ ] **Step 2: Add gateway subcommand**

In `main.rs`, change from flat args to subcommands:

```rust
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "hermes", about = "Interactive AI agent powered by Hermes")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    // Keep existing flat args for backward compat
    #[arg(short, long)]
    message: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    resume: Option<Option<String>>,
    #[arg(long)]
    list_sessions: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the gateway server
    Gateway,
}
```

In main():
```rust
if let Some(Commands::Gateway) = cli.command {
    return run_gateway().await;
}
// ... existing flag handling ...
```

```rust
async fn run_gateway() -> anyhow::Result<()> {
    let config = hermes_config::config::AppConfig::load();
    let gateway_config = config.gateway.clone().unwrap_or_default();

    if gateway_config.telegram.is_none() && gateway_config.api_server.is_none() {
        anyhow::bail!(
            "No gateway adapters configured. Add [gateway.telegram] or [gateway.api_server] to ~/.hermes/config.yaml"
        );
    }

    let runner = hermes_gateway::GatewayRunner::new(gateway_config, config);
    runner.run().await
}
```

- [ ] **Step 3: Verify compilation**

Run: `cargo build -p hermes-cli`

- [ ] **Step 4: Commit**

Commit: `feat: add 'hermes gateway' CLI subcommand`

---

## Task 8: Full Build Verification + Smoke Test

- [ ] **Step 1: Run full checks**

```bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo build --release -p hermes-cli
```

Fix any issues.

- [ ] **Step 2: Smoke test API server**

Create a minimal config:
```bash
mkdir -p ~/.hermes
cat >> ~/.hermes/config.yaml << 'EOF'
gateway:
  api_server:
    bind_addr: "127.0.0.1:8080"
EOF
```

Start gateway:
```bash
OPENAI_API_KEY=<key> ./target/release/hermes gateway &
sleep 2
```

Test health:
```bash
curl http://127.0.0.1:8080/health
# Expected: {"status":"ok"}
```

Test chat:
```bash
curl -X POST http://127.0.0.1:8080/api/chat \
  -H "Content-Type: application/json" \
  -d '{"message": "What is 2+2?"}'
# Expected: {"response":"...","session_id":"..."}
```

- [ ] **Step 3: Smoke test Telegram (if token available)**

Add to config:
```yaml
gateway:
  telegram:
    token: "${TELEGRAM_BOT_TOKEN}"
    allow_all: true
```

Start gateway, send message to bot, verify response.

- [ ] **Step 4: Commit any fixes**

Commit: `chore: fix Phase 5 build issues`

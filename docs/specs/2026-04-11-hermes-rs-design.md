# Hermes-RS: Rust Rewrite Design Spec

**Date**: 2026-04-11
**Status**: Draft
**Scope**: Full port of Hermes Agent (v0.8.0) with Rust-native architecture redesign

## 1. Goals & Decisions

### Objective

Full-feature port of the Python Hermes Agent to Rust, redesigning modules with Rust idioms (traits, ownership, async runtime) while preserving all capabilities.

### Key Architectural Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Build order | Core-first (6 phases) | Dependencies flow downward; agent loop is foundation |
| Plugin model | Compile-time (inventory) + MCP protocol | Zero-cost for builtins, MCP for external extensibility |
| Python interop | Process isolation | Pure Rust for Phase 1-5; RL worker as subprocess in Phase 6 |
| Async runtime | tokio | De facto standard, reqwest/axum ecosystem |
| Error handling | thiserror (libraries) + anyhow (binaries) | Structured errors in crates, flexible at top level |

### Phase Plan

| Phase | Content | Validates |
|-------|---------|-----------|
| 1 | Agent loop + LLM provider + Tool registry + minimal CLI | Core trait hierarchy, ownership model |
| 2 | Config system + Session storage + basic tools (terminal, file) | Persistence, serde patterns |
| 3 | Memory system + Context compression + Prompt caching | Async prefetch, cache mechanics |
| 4 | Skills system + full tool suite (40+) | Plugin registry at scale |
| 5 | Gateway + platform adapters (Telegram, Discord, Slack, etc.) | tokio multi-task, cross-platform |
| 6 | Cron + Batch processing + RL integration (process isolation) | Scheduled automation, Python IPC |

---

## 2. Workspace Structure

```
hermes-rs/
├── Cargo.toml                  # workspace root
├── crates/
│   ├── hermes-core/            # Core types, traits, errors (deps: serde, serde_json, async-trait, tokio sync primitives)
│   ├── hermes-provider/        # LLM provider trait + OpenAI/Anthropic/OpenRouter impl
│   ├── hermes-tools/           # Tool trait, Registry, 40+ built-in tools
│   ├── hermes-mcp/             # MCP client (JSON-RPC over stdio/HTTP)
│   ├── hermes-memory/          # Memory provider trait + built-in/external impl
│   ├── hermes-agent/           # Agent loop, iteration budget, context compression
│   ├── hermes-config/          # Config loading, session storage (SQLite)
│   ├── hermes-skills/          # Skill discovery, parsing, injection
│   ├── hermes-cli/             # REPL (rustyline + crossterm)
│   ├── hermes-gateway/         # Multi-platform gateway + adapters
│   └── hermes-cron/            # Scheduled task scheduler
├── tools/                      # Optional: large tools as separate crates
└── tests/                      # Integration tests
```

### Dependency Graph

```
cli ──→ agent ──→ tools ──→ core
gateway ┘    ├──→ provider ──→ core
cron ───┘    ├──→ memory ────→ core
             ├──→ mcp ──────→ core
             ├──→ skills ───→ core
             └──→ config ───→ core
```

**Principle**: Each crate has a single responsibility. Dependencies flow strictly downward. `hermes-core` has minimal dependencies (serde, serde_json, async-trait, tokio sync primitives) and serves as the shared foundation.

---

## 3. Core Types (hermes-core)

### Message Model

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Role { System, User, Assistant, Tool }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Content,
    pub tool_calls: Vec<ToolCall>,       // empty vec, not Option (simplifies downstream)
    pub reasoning: Option<String>,        // Anthropic extended thinking tokens
    pub name: Option<String>,             // tool name when role=Tool
    pub tool_call_id: Option<String>,     // correlates tool result to call
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Content {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentPart {
    Text(String),
    Image { data: ImageData },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,   // JSON Schema
}
```

### Four Core Traits

```rust
// 1. Tool
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> ToolSchema;
    fn toolset(&self) -> &str;                     // grouping: "web", "terminal", "file"
    fn is_available(&self) -> bool { true }         // gate on env/credentials
    fn is_read_only(&self) -> bool { false }        // parallel dispatch decision
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult>;
}

// 2. Provider
#[async_trait]
pub trait Provider: Send + Sync {
    async fn chat(
        &self,
        request: &ChatRequest<'_>,
        delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) -> Result<ChatResponse>;

    fn supports_tool_calling(&self) -> bool { true }
    fn supports_reasoning(&self) -> bool { false }
    fn supports_caching(&self) -> bool { false }
    fn model_info(&self) -> &ModelInfo;
}

// 3. MemoryProvider
// Use Arc<dyn MemoryProvider> (not Box) so cloning is trivial reference counting.
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    fn system_prompt_block(&self) -> Option<String>;
    async fn prefetch(&self, query: &str, session_id: &str) -> Result<String>;
    async fn sync_turn(&self, user: &str, assistant: &str, session_id: &str) -> Result<()>;
    async fn on_turn_start(&self, turn: u32, message: &str) -> Result<()>;
    async fn on_turn_end(&self, user: &str, assistant: &str) -> Result<()>;
    async fn on_session_end(&self, messages: &[Message]) -> Result<()>;
    async fn on_pre_compress(&self, messages: &[Message]) -> Result<Option<String>>;
    async fn on_memory_write(&self, action: &str, target: &str, content: &str) -> Result<()>;
    async fn on_delegation(&self, task: &str, result: &str, child_session_id: &str) -> Result<()>;
    async fn shutdown(&self) -> Result<()>;
}

// 4. PlatformAdapter
#[async_trait]
pub trait PlatformAdapter: Send + Sync {
    fn platform_name(&self) -> &str;
    async fn start(&self, event_tx: mpsc::Sender<PlatformEvent>) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn send_response(&self, event: &MessageEvent, response: &str) -> Result<()>;
}
```

### ToolContext (designed for parallel spawning)

```rust
#[derive(Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub config: Arc<AppConfig>,
    pub approval_tx: mpsc::Sender<ApprovalRequest>,
    pub working_dir: PathBuf,
}
```

All owned/Arc types so it can be `Clone`d into `tokio::spawn` tasks. Cost: one String + two Arc clones + one PathBuf per tool call, negligible.

### Stream Deltas (channel-based, not callback)

```rust
#[derive(Debug, Clone)]
pub enum StreamDelta {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStart { id: String, name: String },
    ToolCallArgsDelta { id: String, delta: String },
    ToolProgress { tool: String, status: String },
    Done,
}
```

Agent sends deltas through `mpsc::Sender<StreamDelta>`, CLI/gateway receives and renders. Decouples agent from display concerns.

### Error Types

```rust
// hermes-core: structured errors via thiserror
#[derive(Debug, thiserror::Error)]
pub enum HermesError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("tool error: {name}: {message}")]
    Tool { name: String, message: String },
    #[error("config error: {0}")]
    Config(#[from] ConfigError),
    #[error("memory error: {0}")]
    Memory(String),
    #[error("mcp error: {0}")]
    Mcp(String),
    #[error("approval denied")]
    ApprovalDenied,
}

// hermes-cli / hermes-gateway: anyhow at top level
fn main() -> anyhow::Result<()> { /* ... */ }
```

### Technology Choices

| Capability | Crate | Replaces (Python) |
|-----------|-------|-------------------|
| Async runtime | **tokio** | asyncio / sync |
| HTTP client | **reqwest** | httpx |
| JSON | **serde_json** | json |
| Config | **serde_yaml_ng** + **dotenvy** | pyyaml + python-dotenv |
| SQLite | **tokio-rusqlite** (FTS5) | sqlite3 |
| CLI REPL | **rustyline** + **crossterm** | prompt_toolkit + Rich |
| Arg parsing | **clap** | argparse |
| Logging | **tracing** | logging |
| Compile-time registration | **inventory** | import side-effects |
| MCP | **rmcp** or custom | mcp-python |
| Markdown rendering | **termimad** or **comrak** | Rich.Markdown |
| Token counting | **tokenizers** (HuggingFace) | tiktoken |
| Secrets | **secrecy** | N/A |
| Concurrent map | **dashmap** | N/A |
| Cron parsing | **cron** | APScheduler |
| Directory walking | **walkdir** | os.walk |
| UUID | **uuid** | uuid |
| DateTime | **chrono** | datetime |

---

## 4. Agent Loop

### Ownership Model

```rust
pub struct Agent {
    provider: Arc<dyn Provider>,       // Arc: subagents share same provider
    router: ModelRouter,               // primary + summary + vision providers
    registry: Arc<ToolRegistry>,       // Arc: subagents share tool set
    memory: MemoryManager,             // Each agent owns independently
    compressor: ContextCompressor,     // context compression (uses summary provider)
    cache_manager: PromptCacheManager, // prompt cache state for this session
    config: Arc<AppConfig>,            // Arc: shared globally
    budget: IterationBudget,           // Each agent owns independently (NOT shared)
    session_id: String,
}
```

**Rationale**:
- `Arc<dyn Provider>`: Provider is stateless (reqwest::Client is internally Arc), sharing is free
- `Arc<ToolRegistry>`: Read-only after init, subagent just clones the reference
- `MemoryManager` owned: Each agent has independent prefetch cache and sync queue
- `IterationBudget` owned: Each agent (parent and subagent) gets its own fresh budget. Subagents do NOT share parent's budget — total iterations across parent + subagents can exceed the parent's cap (matches Python behavior)
- `ContextCompressor` + `PromptCacheManager`: owned per agent, interact during compression

### Core Loop

```rust
impl Agent {
    pub async fn run_conversation(
        &self,
        user_message: &str,
        system_prompt: &str,
        history: &mut Vec<Message>,        // caller owns, agent borrows mutably
        delta_tx: &mpsc::Sender<StreamDelta>,
    ) -> Result<String> {
        history.push(Message::user(user_message));

        // 1. Take prefetched memory (from background task spawned at end of last turn)
        let memory_ctx = self.memory.take_prefetched(&self.session_id).await;

        // 2. Build system prompt (first turn: freeze, subsequent: reuse for cache)
        let system = self.build_system_prompt(system_prompt, memory_ctx.as_deref());

        let mut final_response = String::new();

        // 3. Main loop
        while self.budget.try_consume()? {
            let request = ChatRequest {
                system: &system,
                messages: history.as_slice(),
                tools: self.registry.available_schemas(),
                // ...
            };

            // 4. LLM call (streaming deltas sent via delta_tx)
            let response = self.provider.chat(&request, Some(delta_tx)).await?;

            // 5. Append assistant message
            history.push(Message::assistant_from_response(&response));

            // 6. No tool calls -> done
            if response.tool_calls.is_empty() {
                final_response = response.text_content();
                break;
            }

            // 7. Execute tools (possibly parallel)
            let results = self.execute_tools(&response.tool_calls, delta_tx).await?;
            for result in results {
                history.push(Message::tool_result(result));
            }

            // 8. Context compression check
            if self.should_compress(history) {
                self.compress(history).await?;
            }
        }

        // 9. Async memory sync + prefetch for next turn
        self.memory.sync_turn(user_message, &final_response, &self.session_id);
        self.memory.queue_prefetch(&final_response, &self.session_id);

        Ok(final_response)
    }
}
```

### Tool Parallel Execution

```rust
impl Agent {
    async fn execute_tools(
        &self,
        calls: &[ToolCall],
        delta_tx: &mpsc::Sender<StreamDelta>,
    ) -> Result<Vec<ToolCallResult>> {
        if self.should_parallelize(calls) {
            self.execute_parallel(calls, delta_tx).await
        } else {
            self.execute_sequential(calls, delta_tx).await
        }
    }

    fn should_parallelize(&self, calls: &[ToolCall]) -> bool {
        if calls.len() <= 1 { return false; }
        // Any interactive tool -> sequential
        if calls.iter().any(|c| c.name == "clarify") { return false; }
        // Collect write paths, check for conflicts
        let write_paths: Vec<&str> = calls.iter()
            .filter(|c| !self.registry.get(&c.name).map_or(false, |t| t.is_read_only()))
            .filter_map(|c| c.arguments.get("path").and_then(|v| v.as_str()))
            .collect();
        // No write path conflicts -> parallel
        !has_path_conflicts(&write_paths)
    }

    async fn execute_parallel(
        &self,
        calls: &[ToolCall],
        delta_tx: &mpsc::Sender<StreamDelta>,
    ) -> Result<Vec<ToolCallResult>> {
        let mut set = JoinSet::new();

        for call in calls {
            let registry = Arc::clone(&self.registry);
            let ctx = self.make_tool_context();
            let call = call.clone();
            let tx = delta_tx.clone();

            set.spawn(async move {
                tx.send(StreamDelta::ToolProgress {
                    tool: call.name.clone(),
                    status: "running".into(),
                }).await.ok();

                let result = match registry.get(&call.name) {
                    Some(tool) => tool.execute(call.arguments, &ctx).await,
                    None => Err(HermesError::Tool {
                        name: call.name.clone(),
                        message: "unknown tool".into(),
                    }),
                };

                ToolCallResult {
                    call_id: call.id,
                    tool_name: call.name,
                    result: result.unwrap_or_else(|e| ToolResult::error(e.to_string())),
                }
            });
        }

        let mut results = Vec::with_capacity(calls.len());
        while let Some(res) = set.join_next().await {
            results.push(res??);
        }
        // IMPORTANT: JoinSet returns in completion order, but LLM APIs require
        // tool results in the same order as tool_calls. Re-sort by original index.
        let call_order: HashMap<&str, usize> = calls.iter()
            .enumerate()
            .map(|(i, c)| (c.id.as_str(), i))
            .collect();
        results.sort_by_key(|r| call_order.get(r.call_id.as_str()).copied().unwrap_or(usize::MAX));
        Ok(results)
    }
}
```

### Dangerous Command Approval (oneshot channel pattern)

```rust
pub struct ApprovalRequest {
    pub tool_name: String,
    pub command: String,
    pub reason: String,
    pub response_tx: oneshot::Sender<ApprovalDecision>,
}

pub enum ApprovalDecision {
    Allow,
    AllowSession,    // remember for this session
    AllowAlways,     // remember permanently
    Deny,
}
```

Data flow: Tool -> `approval_tx` -> CLI/Gateway renders approval UI -> `oneshot` returns decision -> Tool continues or aborts. Agent loop itself does not participate in approval logic.

### Iteration Budget

Budget is owned (not shared), so plain `u32` with `&mut self` — no atomics needed.

```rust
pub struct IterationBudget {
    remaining: u32,
    max: u32,
}

impl IterationBudget {
    pub fn new(max: u32) -> Self {
        Self { remaining: max, max }
    }

    pub fn try_consume(&mut self) -> bool {
        if self.remaining == 0 { return false; }
        self.remaining -= 1;
        true
    }

    /// execute_code etc. refund iterations
    pub fn refund(&mut self, n: u32) {
        self.remaining = self.remaining.saturating_add(n).min(self.max);
    }
}
```

### Subagent Delegation

```rust
impl Agent {
    pub async fn delegate(
        &self,
        task: &str,
        tool_subset: Option<&[String]>,
        delta_tx: &mpsc::Sender<StreamDelta>,
    ) -> Result<String> {
        let sub_session = Uuid::new_v4().to_string();
        let sub = Agent {
            provider: Arc::clone(&self.provider),
            router: self.router.clone(),
            registry: Arc::clone(&self.registry),
            memory: self.memory.new_child(),
            compressor: ContextCompressor::new(self.router.for_task(TaskType::Summary), &self.config),
            cache_manager: PromptCacheManager::new(),
            config: Arc::clone(&self.config),
            budget: IterationBudget::new(50),   // fresh, independent budget
            session_id: sub_session.clone(),
        };

        let mut sub_history = Vec::new();
        let result = sub.run_conversation(
            task, &self.system_prompt(), &mut sub_history, delta_tx,
        ).await?;

        self.memory.on_delegation(task, &result, &sub.session_id);
        Ok(result)
    }
}
```

---

## 5. Configuration & Storage

### Directory Structure (Profile Isolated)

```
~/.hermes/
├── config.yaml                    # all settings
├── .env                           # API keys (0600 permissions)
├── SOUL.md                        # agent persona
├── sessions/{uuid}.db             # conversation data (SQLite + FTS5)
├── memories/
│   ├── MEMORY.md                  # structured notes
│   └── USER.md                    # user profile
├── skills/{name}.md               # skill files
├── cron/
│   ├── jobs.json                  # scheduled job definitions
│   └── output/{job_id}/{ts}.md    # execution output
├── cache/                         # model cache, audio samples
├── logs/                          # tracing logs
├── skins/{name}.yaml              # themes
└── profiles/{name}/               # isolated profiles (same structure)
```

```rust
/// All crates use this, never hardcode ~/.hermes
pub fn hermes_home() -> PathBuf {
    env::var("HERMES_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap().join(".hermes"))
}
```

### Config Types

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub config_version: u32,                      // current v5, for migrations
    pub model: ModelConfig,
    pub toolsets: BTreeMap<String, bool>,
    pub terminal: TerminalConfig,
    pub memory: MemoryConfig,
    pub display: DisplayConfig,
    pub security: SecurityConfig,
    pub platforms: PlatformConfigs,
    pub mcp_servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub default: String,                          // "anthropic/claude-sonnet-4-20250514"
    pub vision: Option<String>,
    pub summary: Option<String>,
    pub reasoning: bool,
    pub max_tokens: u32,
    pub temperature: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalBackend {
    Local,
    Docker { image: String, volumes: Vec<String> },
    Ssh { host: String, user: String, key_path: Option<PathBuf> },
    Modal { app_name: String },
    Daytona { workspace: String },
}
```

### Config Loading (layered: YAML -> .env -> env vars)

```rust
impl AppConfig {
    pub fn load() -> Result<Self> {
        let path = hermes_home().join("config.yaml");

        if !path.exists() {
            let config = Self::default();
            config.save(&path)?;
            return Ok(config);
        }

        // 1. Parse as dynamic Value
        let raw = fs::read_to_string(&path)?;
        let mut value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&raw)?;

        // 2. Version migrations
        let version = value.get("config_version")
            .and_then(|v| v.as_u64()).unwrap_or(1) as u32;
        migrate(&mut value, version, CURRENT_VERSION)?;

        // 3. Deserialize to strong types
        let mut config: AppConfig = serde_yaml_ng::from_value(value)?;

        // 4. .env -> env vars
        let env_path = hermes_home().join(".env");
        if env_path.exists() { dotenvy::from_path(&env_path)?; }

        // 5. Env var overrides
        config.apply_env_overrides();
        Ok(config)
    }
}

fn migrate(value: &mut serde_yaml_ng::Value, from: u32, to: u32) -> Result<()> {
    for v in from..to {
        match v {
            1 => { /* v1->v2: rename keys */ }
            2 => { /* v2->v3: add mcp_servers */ }
            3 => { /* v3->v4: restructure terminal */ }
            4 => { /* v4->v5: profile support */ }
            _ => {}
        }
        value["config_version"] = serde_yaml_ng::Value::from(v + 1);
    }
    Ok(())
}
```

### Session Storage (tokio-rusqlite + FTS5)

**Design choice**: Per-session `.db` files (not the Python's single `state.db`). This is an intentional redesign: per-session files enable easy branching (just copy file), simpler cleanup (delete old sessions), and avoid lock contention in the gateway where multiple sessions are active concurrently. The Python's single-file approach with `session_id` foreign keys is a valid alternative but creates more contention.

Uses `tokio-rusqlite` which wraps `rusqlite::Connection` in an internal thread pool, since `Connection` is `!Send` and cannot be held across `.await` points.

```rust
pub struct SessionStore {
    conn: tokio_rusqlite::Connection,  // !Send Connection wrapped in internal thread
    session_id: String,
}

impl SessionStore {
    pub async fn open(session_id: &str) -> Result<Self> {
        let path = hermes_home().join("sessions").join(format!("{session_id}.db"));
        fs::create_dir_all(path.parent().expect("session path always has parent"))?;
        let conn = tokio_rusqlite::Connection::open(&path).await?;

        conn.call(|conn| {
            conn.execute_batch("
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS messages (
                id        INTEGER PRIMARY KEY,
                role      TEXT NOT NULL,
                name      TEXT,               -- tool name when role=Tool
                content   TEXT,
                tool_calls    TEXT,            -- JSON
                reasoning     TEXT,
                tool_call_id  TEXT,
                created_at    TEXT DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS checkpoints (
                id         INTEGER PRIMARY KEY,
                message_id INTEGER NOT NULL,
                label      TEXT,
                created_at TEXT DEFAULT (datetime('now'))
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts
                USING fts5(content, content=messages, content_rowid=id);

            CREATE TRIGGER IF NOT EXISTS fts_insert AFTER INSERT ON messages BEGIN
                INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
            END;
            CREATE TRIGGER IF NOT EXISTS fts_delete AFTER DELETE ON messages BEGIN
                INSERT INTO messages_fts(messages_fts, rowid, content)
                    VALUES('delete', old.id, old.content);
            END;
        ")?;
            Ok(())
        }).await?;

        Ok(Self { conn, session_id: session_id.to_string() })
    }

    pub async fn append(&self, msg: &Message) -> Result<i64> { /* conn.call(|c| ...) */ }
    pub async fn load_history(&self) -> Result<Vec<Message>> { /* conn.call(|c| ...) */ }
    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>> { /* FTS5 MATCH */ }
    pub async fn checkpoint(&self, label: Option<&str>) -> Result<i64> { /* ... */ }
    pub async fn rollback_to(&self, checkpoint_id: i64) -> Result<()> { /* ... */ }
    pub async fn branch(&self) -> Result<String> { /* copy db to new session_id */ }
}

/// Cross-session search: iterate all .db files, FTS5 query each, merge by rank
pub fn search_all_sessions(query: &str, limit: usize) -> Result<Vec<GlobalSearchHit>> {
    let dir = hermes_home().join("sessions");
    let mut all_hits = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension() != Some("db".as_ref()) { continue; }
        let session_id = path.file_stem().unwrap().to_string_lossy().to_string();
        if let Ok(store) = SessionStore::open(&session_id) {
            if let Ok(hits) = store.search(query) {
                all_hits.extend(hits.into_iter().map(|h| GlobalSearchHit {
                    session_id: session_id.clone(), hit: h,
                }));
            }
        }
    }
    all_hits.sort_by(|a, b| a.hit.rank.partial_cmp(&b.hit.rank).unwrap());
    all_hits.truncate(limit);
    Ok(all_hits)
}
```

---

## 6. Memory System

### MemoryManager (orchestrator)

```rust
pub struct MemoryManager {
    builtin: BuiltinMemory,
    external: Option<Arc<dyn MemoryProvider>>,    // Arc (not Box) — cheap to clone for spawned tasks
    prefetch_cache: Arc<Mutex<HashMap<String, String>>>,  // std::sync::Mutex, never held across .await
}

impl MemoryManager {
    pub fn new(config: &MemoryConfig) -> Result<Self> {
        let external: Option<Arc<dyn MemoryProvider>> = match &config.external_provider {
            Some(ExternalMemoryProvider::Honcho { api_url, app_id }) => {
                Some(Arc::new(HonchoMemory::new(api_url, app_id)?))
            }
            None => None,
        };
        Ok(Self {
            builtin: BuiltinMemory::new(hermes_home().join("memories"))?,
            external,
            prefetch_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Aggregate all provider system prompt blocks, fenced against injection
    pub fn system_prompt_blocks(&self) -> String {
        let mut blocks = Vec::new();
        if let Some(b) = self.builtin.system_prompt_block() { blocks.push(b); }
        if let Some(ext) = &self.external {
            if let Some(b) = ext.system_prompt_block() { blocks.push(b); }
        }
        blocks.iter()
            .map(|b| format!("<memory-context>\n{b}\n</memory-context>"))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Non-blocking: spawn background task, result stored in cache
    pub fn queue_prefetch(&self, hint: &str, session_id: &str) {
        let provider = self.external.clone();     // Arc::clone — cheap
        let cache = Arc::clone(&self.prefetch_cache);
        let hint = hint.to_string();
        let sid = session_id.to_string();
        tokio::spawn(async move {
            if let Some(ext) = provider {
                if let Ok(result) = ext.prefetch(&hint, &sid).await {
                    cache.lock().expect("prefetch cache poisoned").insert(sid, result);
                }
            }
        });
    }

    /// Turn start: take cached result (O(1)), or sync prefetch if no cache.
    /// IMPORTANT: Mutex guard is dropped BEFORE any .await to avoid blocking tokio.
    pub async fn take_prefetched(&self, session_id: &str) -> Option<String> {
        // Scope the lock so guard is dropped before any .await
        let cached = {
            self.prefetch_cache.lock().expect("prefetch cache poisoned").remove(session_id)
        };
        if cached.is_some() {
            return cached;
        }
        // Fallback: sync prefetch (first turn or cache miss). Empty query = "no hint" (cold start).
        if let Some(ext) = &self.external {
            ext.prefetch("", session_id).await.ok()
        } else {
            None
        }
    }

    /// Non-blocking turn-end sync
    pub fn sync_turn(&self, user: &str, assistant: &str, session_id: &str) {
        let builtin = self.builtin.clone();
        let external = self.external.clone();     // Arc::clone
        let (user, assistant, sid) = (user.to_string(), assistant.to_string(), session_id.to_string());
        tokio::spawn(async move {
            let _ = builtin.on_turn_end(&user, &assistant).await;
            if let Some(ext) = external {
                let _ = ext.sync_turn(&user, &assistant, &sid).await;
            }
        });
    }

    /// Create child for subagent: independent cache, no external writes
    pub fn new_child(&self) -> Self {
        Self {
            builtin: self.builtin.clone(),
            external: None,   // subagents don't directly write external memory
            prefetch_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}
```

### Built-in Memory (file system)

```rust
#[derive(Clone)]
pub struct BuiltinMemory {
    dir: PathBuf,    // ~/.hermes/memories/
}

impl BuiltinMemory {
    pub fn system_prompt_block(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Ok(content) = fs::read_to_string(self.dir.join("MEMORY.md")) {
            if !content.trim().is_empty() { parts.push(format!("## Notes\n{content}")); }
        }
        if let Ok(content) = fs::read_to_string(self.dir.join("USER.md")) {
            if !content.trim().is_empty() { parts.push(format!("## User Profile\n{content}")); }
        }
        if parts.is_empty() { None } else { Some(parts.join("\n\n")) }
    }

    pub fn write(&self, key: &str, content: &str) -> Result<()> { /* ... */ }
    pub fn read(&self, key: &str) -> Result<Option<String>> { /* ... */ }
}
```

### SOUL.md Persona

```rust
pub struct Persona { path: PathBuf }

impl Persona {
    pub fn load(&self) -> Option<String> {
        fs::read_to_string(&self.path).ok().filter(|s| !s.trim().is_empty())
    }

    pub fn inject(&self, system_prompt: &mut String) {
        if let Some(soul) = self.load() {
            system_prompt.push_str("\n\n## Your Persona\n");
            system_prompt.push_str(&soul);
        }
    }
}
```

---

## 7. LLM Provider Layer

### Shared SSE Stream Parser

```rust
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

pub struct SseStream {
    reader: BufReader<StreamReader<BytesStream, io::Error>>,
}

impl SseStream {
    pub fn new(response: reqwest::Response) -> Self { /* ... */ }

    pub async fn next_event(&mut self) -> Result<Option<SseEvent>> {
        // Parse SSE lines: event:, data:, empty line = event boundary
        // Returns None on EOF or data: [DONE]
    }
}
```

### Tool Call Assembler

Tool call arguments arrive as JSON fragments that must be concatenated:

```rust
pub struct ToolCallAssembler {
    pending: HashMap<usize, PendingToolCall>,
}

struct PendingToolCall {
    id: String,
    name: String,
    arguments_buf: String,
}

impl ToolCallAssembler {
    pub fn start(&mut self, index: usize, id: String, name: String) { /* ... */ }
    pub fn append_arguments(&mut self, index: usize, delta: &str) { /* ... */ }
    pub fn finish(self) -> Result<Vec<ToolCall>> { /* parse accumulated JSON */ }
}
```

### OpenAI-Compatible Provider

One implementation covers OpenAI, OpenRouter, Azure, Mistral, local models:

```rust
pub struct OpenAiProvider {
    client: reqwest::Client,
    config: OpenAiConfig,
    info: ModelInfo,
}

pub struct OpenAiConfig {
    pub base_url: String,              // varies per provider
    pub api_key: Secret<String>,       // secrecy crate: prevents accidental logging
    pub model: String,
    pub org_id: Option<String>,
    pub auth_style: AuthStyle,         // Bearer | AzureApiKey | None
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn chat(&self, request: &ChatRequest<'_>, delta_tx: Option<&mpsc::Sender<StreamDelta>>) -> Result<ChatResponse> {
        let body = self.build_request(request);
        let http_req = self.client.post(format!("{}/chat/completions", self.config.base_url))
            .headers(self.auth_headers())
            .json(&body);

        if let Some(tx) = delta_tx {
            self.stream_response(http_req, tx).await
        } else {
            self.blocking_response(http_req).await
        }
    }
    // ...
}
```

Stream response processes SSE events:
- `choices[0].delta.content` -> `StreamDelta::TextDelta`
- `choices[0].delta.tool_calls[i]` -> `ToolCallAssembler`
- `usage` (final chunk) -> `TokenUsage`

### Anthropic Provider

Different API format and SSE event structure:

```rust
pub struct AnthropicProvider {
    client: reqwest::Client,
    config: AnthropicConfig,
    info: ModelInfo,
}

pub struct AnthropicConfig {
    pub base_url: String,
    pub api_key: Secret<String>,       // secrecy crate: prevents accidental logging
    pub model: String,
    pub api_version: String,
    pub max_thinking_tokens: Option<u32>,
}
```

Key differences from OpenAI:
- System prompt is separate from messages, supports `cache_control` annotations per block
- Messages must strictly alternate user/assistant (tool results are `user` messages with `tool_result` content blocks)
- SSE events: `message_start`, `content_block_start`, `content_block_delta`, `message_delta`, `message_stop`
- Extended thinking: `thinking` content blocks with `thinking_delta` events
- Cache metadata in `usage`: `cache_creation_input_tokens`, `cache_read_input_tokens`

### Message Format Conversion (Anthropic)

```rust
impl AnthropicProvider {
    fn convert_messages(&self, messages: &[Message]) -> Vec<serde_json::Value> {
        // Role::System -> skip (handled separately)
        // Role::User -> { "role": "user", "content": text }
        // Role::Assistant -> { "role": "assistant", "content": [thinking?, text?, tool_use*] }
        // Role::Tool -> { "role": "user", "content": [{ "type": "tool_result", ... }] }
        // Then merge adjacent same-role messages (Anthropic requires strict alternation)
    }
}
```

### Retry Policy

```rust
pub struct RetryPolicy {
    pub max_retries: u32,           // default: 3
    pub initial_backoff: Duration,  // default: 500ms
    pub max_backoff: Duration,      // default: 30s
    pub retryable_statuses: &'static [u16],  // [429, 500, 502, 503, 529]
}
```

- 429 (rate limit): use `Retry-After` header if present, else exponential backoff + jitter
- 500/502/503: exponential backoff
- 400 (bad request): do not retry
- Network/timeout errors: retry with backoff

### Provider Factory

The Python has three distinct API modes: `chat_completions` (OpenAI-compatible), `anthropic_messages` (native Anthropic), and `codex_responses` (OpenAI Responses API at `/v1/responses`). All three must be supported.

```rust
/// Three API modes, matching Python's api_mode
pub enum ApiMode {
    ChatCompletions,       // OpenAI /v1/chat/completions — OpenAI, OpenRouter, Azure, Mistral, local
    AnthropicMessages,     // Anthropic /v1/messages — native Anthropic SDK, also MiniMax/Alibaba /anthropic endpoints
    CodexResponses,        // OpenAI /v1/responses — OpenAI Responses API (different streaming/tool format)
}

pub fn create_provider(config: &AppConfig) -> Result<Arc<dyn Provider>> {
    let (provider_name, model_id) = config.model.default.split_once('/')
        .unwrap_or(("openai", &config.model.default));

    match provider_name {
        "anthropic"     => Ok(Arc::new(AnthropicProvider { /* AnthropicMessages mode */ })),
        "openai"        => Ok(Arc::new(OpenAiProvider { /* ChatCompletions mode */ })),
        "openai-codex"  => Ok(Arc::new(CodexResponsesProvider { /* CodexResponses mode */ })),
        "openrouter"    => Ok(Arc::new(OpenAiProvider { base_url: "https://openrouter.ai/api/v1", .. })),
        "azure"         => Ok(Arc::new(OpenAiProvider { base_url: azure_endpoint, auth: AzureApiKey, .. })),
        _ => {
            // Auto-detect: if URL ends in /anthropic, use AnthropicMessages
            let base_url = env::var("CUSTOM_LLM_BASE_URL").unwrap_or_default();
            if base_url.ends_with("/anthropic") {
                Ok(Arc::new(AnthropicProvider { /* ... */ }))
            } else {
                Ok(Arc::new(OpenAiProvider { /* ChatCompletions */ }))
            }
        }
    }
}
```

`CodexResponsesProvider` implements `Provider` with the OpenAI Responses API format (different streaming events, different tool call structure). It shares the same `OpenAiConfig` but uses `/v1/responses` endpoint.

### Model Info & Pricing

```rust
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub max_context: usize,
    pub max_output: usize,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub supports_reasoning: bool,
    pub supports_caching: bool,
    pub pricing: ModelPricing,
}

pub struct ModelPricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: f64,
    pub cache_create_per_mtok: f64,
}
```

### Smart Model Routing

```rust
pub struct ModelRouter {
    primary: Arc<dyn Provider>,
    summary: Option<Arc<dyn Provider>>,   // cheap model for summaries
    vision: Option<Arc<dyn Provider>>,    // cheap model for vision
}

pub enum TaskType { Conversation, Summary, Vision }

impl ModelRouter {
    pub fn for_task(&self, task: TaskType) -> &dyn Provider {
        match task {
            TaskType::Summary => self.summary.as_deref().unwrap_or(self.primary.as_ref()),
            TaskType::Vision  => self.vision.as_deref().unwrap_or(self.primary.as_ref()),
            TaskType::Conversation => self.primary.as_ref(),
        }
    }
}
```

---

## 8. Context Compression

### Token Counter

Uses the HuggingFace `tokenizers` crate, not `tiktoken-rs` (which only supports OpenAI models). For Claude, load the actual Claude tokenizer vocabulary from HuggingFace. For OpenAI, load `cl100k_base` or `o200k_base`.

```rust
pub struct TokenCounter {
    tokenizer: tokenizers::Tokenizer,   // HuggingFace tokenizers crate
}

impl TokenCounter {
    pub fn for_model(model: &str) -> Result<Self> {
        let tokenizer = if model.contains("claude") || model.contains("anthropic") {
            // Load Claude tokenizer from HuggingFace or bundled vocab
            tokenizers::Tokenizer::from_pretrained("Xenova/claude-tokenizer", None)?
        } else {
            // OpenAI models: cl100k_base or o200k_base
            tokenizers::Tokenizer::from_pretrained("Xenova/gpt-4o", None)?
        };
        Ok(Self { tokenizer })
    }

    pub fn count_text(&self, text: &str) -> usize {
        self.tokenizer.encode(text, false)
            .map(|enc| enc.get_ids().len())
            .unwrap_or(text.len() / 4)  // fallback heuristic
    }

    pub fn count_message(&self, msg: &Message) -> usize { /* content + tool_calls + reasoning + 4 overhead */ }
    pub fn count_messages(&self, msgs: &[Message]) -> usize { /* sum */ }
}
```

### Compressor

```rust
pub struct ContextCompressor {
    summary_provider: Arc<dyn Provider>,   // cheap model (haiku)
    counter: TokenCounter,
    config: CompressionConfig,
}

pub struct CompressionConfig {
    pub max_context_tokens: usize,           // e.g., 200_000
    pub pressure_threshold: f32,              // default: 0.50 (matches Python)
    pub target_after_compression: f32,        // default: 0.20 (matches Python)
    pub preserve_recent_messages: usize,      // default: 20 (matches Python's protect_last_n)
}
```

### Compression Strategy (layered, not all-or-nothing)

1. **Find split point**: Protect last N messages. Never split tool call chains (assistant tool_calls and subsequent tool results must stay together).

2. **Classify compressible messages** by information density:
   - `KeyDecision`: preserve verbatim
   - `ToolResult`: keep tool name + result summary
   - `Conversation`: extract key points only
   - `BulkToolOutput` (>500 tokens): extreme compression, keep metadata only

3. **Let memory providers contribute** via `on_pre_compress()` hook.

4. **Generate summary** via cheap model with classification hints.

5. **Rebuild history**: `[summary_message] + protected_messages`.

```rust
pub enum CompressionResult {
    NotNeeded,
    Compressed {
        before_tokens: usize,
        after_tokens: usize,
        messages_compressed: usize,
        messages_kept: usize,
    },
}
```

---

## 9. Prompt Caching

### Core Mechanism

Anthropic prompt cache:
- Add `cache_control: { type: "ephemeral" }` to system prompt blocks
- First request: full send, API caches (cache creation, normal billing)
- Subsequent requests: prefix matches exactly -> **cache hit**, 90% token discount
- Cache TTL: 5 minutes (refreshed on each hit)
- **Any prefix change** -> cache miss -> recreate

### Cache Manager

```rust
pub struct PromptCacheManager {
    frozen_system: Option<FrozenSystemPrompt>,
    stats: CacheStats,
}

struct FrozenSystemPrompt {
    content: String,
    hash: u64,
    segments: Vec<CacheSegment>,
}

pub struct CacheSegment {
    pub text: String,
    pub label: &'static str,     // "base_instructions", "memory", "persona"
    pub cache_control: bool,
}
```

### Lifecycle

```
Turn 1: freeze_system_prompt() -> hash=A -> CACHE MISS (creation)
Turn 2-N: get_or_rebuild() -> frozen(hash=A) -> CACHE HIT (90% savings)
  (memory writes go to disk but DON'T update frozen system prompt)
Turn K: Compression triggers
  -> invalidate_and_rebuild() -> hash=B -> CACHE MISS (one-time cost)
Turn K+1: get_or_rebuild() -> frozen(hash=B) -> CACHE HIT
```

**Key insight**: Memory writes and system prompt updates are **decoupled**. Writes persist to disk immediately, but the frozen system prompt only refreshes on compression or new session. This ensures cache hits during normal conversation turns.

### Anthropic API Integration

System prompt is split into independently cacheable segments:

```rust
fn build_api_request(&self, req: &ChatRequest, cache: &FrozenSystemPrompt) -> serde_json::Value {
    let system_blocks: Vec<Value> = cache.segments.iter()
        .filter(|s| !s.text.is_empty())
        .map(|seg| {
            let mut block = json!({"type": "text", "text": seg.text});
            if seg.cache_control {
                block["cache_control"] = json!({"type": "ephemeral"});
            }
            block
        })
        .collect();
    // ...
}
```

### Usage Tracking

```rust
pub struct UsageTracker {
    pub turns: usize,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cache_read_tokens: usize,
    pub cache_creation_tokens: usize,
    pub compression_tokens: usize,
}

impl UsageTracker {
    pub fn estimated_cost(&self, pricing: &ModelPricing) -> f64 { /* ... */ }
    pub fn cache_savings(&self, pricing: &ModelPricing) -> f64 { /* ... */ }
}
```

---

## 10. CLI

### Entry Point

```rust
#[derive(Parser)]
#[command(name = "hermes", version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<SubCommand>,
    #[arg(short, long)]
    pub message: Option<String>,
    #[arg(long)]
    pub profile: Option<String>,
}

#[derive(Subcommand)]
pub enum SubCommand {
    Gateway,
    Batch { dataset: PathBuf, output: PathBuf },
    Setup,
}
```

### REPL Architecture

`rustyline` for input (history, multiline, autocomplete) + `crossterm` for streaming output. Not a full TUI (ratatui) because Hermes is fundamentally a REPL, not a full-screen app.

- **Input**: rustyline with Emacs keybindings, slash command autocomplete via `HermesHelper`
- **IMPORTANT**: `rustyline::Editor::readline()` is blocking. Must run via `tokio::task::spawn_blocking` to avoid starving the tokio executor. The main REPL loop runs readline on a blocking thread, sends input to the async agent task via channel.
- **Terminal ownership**: crossterm raw mode must be disabled before handing control to rustyline (which manages its own raw mode). Coordinate via explicit enable/disable around streaming vs. input phases.
- **Output**: crossterm colored streaming, spinner during tool execution
- **Agent runs in background**: `tokio::spawn`, communicates via `mpsc::channel<StreamDelta>`
- **Approval UI**: blocking readline prompt when `ApprovalRequest` arrives (also via `spawn_blocking`)

### Slash Command Registry

Static compile-time array (fixed set, no need for inventory):

```rust
pub struct CommandDef {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub category: Category,
    pub gateway_available: bool,
    pub handler: fn(&mut CommandContext) -> Pin<Box<dyn Future<Output = Result<CommandResult>> + Send + '_>>,
    // async fn pointer pattern: handlers can do async I/O (config reload, network, etc.)
}

pub static COMMANDS: &[CommandDef] = &[
    CommandDef { name: "new",    aliases: &["n"],       category: Session, handler: cmd_new, ... },
    CommandDef { name: "model",  aliases: &["m"],       category: Config,  handler: cmd_model, ... },
    CommandDef { name: "tools",  aliases: &["t"],       category: Tools,   handler: cmd_tools, ... },
    CommandDef { name: "help",   aliases: &["h", "?"],  category: Info,    handler: cmd_help, ... },
    // ...
];
```

One definition drives: CLI dispatch, autocomplete, gateway routing, Telegram menu.

### Skin Engine (data-driven theming)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skin {
    pub name: String,
    pub colors: SkinColors,
    pub prompt_symbol: String,
    pub spinner_frames: Vec<String>,
    pub response_label: String,
}
```

Loaded from `~/.hermes/skins/{name}.yaml`. No code changes needed for new themes.

---

## 11. Gateway

### Architecture

```
                    ┌──────────────────────────────────────────┐
                    │              GatewayRunner                │
  Telegram ────────►│  event_rx ──► SessionRouter              │
  Discord ─────────►│              ├─ Session A ──► Agent A    │
  Slack ───────────►│              ├─ Session B ──► Agent B    │
  WhatsApp ────────►│              └─ Session C ──► Agent C    │
                    │              DeliveryRouter ◄─────────┘  │
                    └──────────────────────────────────────────┘
```

### Per-Session Task Model

Each session is an independent tokio task with its own message channel. No mutexes needed for session state.

Uses `DashMap` for atomic entry operations (avoids TOCTOU race between "session exists?" check and "create session" insert that could spawn duplicate tasks with `RwLock`).

```rust
pub struct SessionRouter {
    active: DashMap<String, mpsc::Sender<IncomingMessage>>,
    config: Arc<AppConfig>,
}

impl SessionRouter {
    pub async fn route(&self, msg: MessageEvent, adapters: &HashMap<String, Arc<dyn PlatformAdapter>>) -> Result<()> {
        let session_id = self.resolve_session_id(&msg).await;

        // Atomic: either get existing sender or create new session
        let tx = self.active.entry(session_id.clone()).or_insert_with(|| {
            let (tx, rx) = mpsc::channel(32);
            let config = Arc::clone(&self.config);
            let delivery = DeliveryRouter::new(adapters);
            let sid = session_id.clone();

            tokio::spawn(async move {
                session_task(sid, rx, config, delivery).await;
            });

            tx
        }).clone();

        tx.send(IncomingMessage::from(msg)).await.ok();
        Ok(())
    }
}
```

### Gateway Approval Handling

In CLI mode, `ApprovalRequest` is consumed by the readline thread. In the gateway, there's no interactive terminal. Gateway sessions use an **auto-policy** configurable per security level:

```rust
pub enum GatewayApprovalPolicy {
    AutoDeny,                          // reject all dangerous commands (default, safest)
    AutoAllowSession,                  // allow if previously approved in this session
    PlatformPrompt,                    // send approval request to user via platform (e.g., Telegram inline buttons)
}
```

The gateway session task spawns a dedicated approval handler that consumes the `approval_rx` channel and applies the configured policy. For `PlatformPrompt`, it sends an interactive message to the user and waits for their response via the platform adapter.
```

### Cross-Platform Session Continuity

Same user on Telegram and Discord can continue the same conversation:

```rust
async fn resolve_session_id(&self, msg: &MessageEvent) -> String {
    // Check pairing table (user /pair'd across platforms)
    if let Some(paired) = self.lookup_pairing(&msg.user_id, &msg.platform).await {
        return paired.session_id;
    }
    // Default: platform:user_id
    format!("{}:{}", msg.platform, msg.user_id)
}
```

### Platform Adapter Example (Telegram)

```rust
pub struct TelegramAdapter {
    token: Secret<String>,             // secrecy crate
    mode: TelegramMode,               // Polling | Webhook
    client: reqwest::Client,
}

#[async_trait]
impl PlatformAdapter for TelegramAdapter {
    fn platform_name(&self) -> &str { "telegram" }
    async fn start(&self, event_tx: mpsc::Sender<PlatformEvent>) -> Result<()> { /* ... */ }
    async fn send_response(&self, event: &MessageEvent, response: &str) -> Result<()> {
        // Markdown -> Telegram MarkdownV2
        // Split long messages (4096 char limit)
    }
    async fn stop(&self) -> Result<()> { /* ... */ }
}
```

---

## 12. Skills System

### Skill Format (Markdown + YAML frontmatter)

```markdown
---
name: git-rebase
title: Interactive Git Rebase
description: Guide for complex rebase operations
conditions:
  - tool_required: terminal
  - platform: linux
---
## Instructions
1. First check current branch status...
```

### SkillManager

```rust
pub struct SkillManager {
    skills: Vec<Skill>,
    dirs: Vec<PathBuf>,
}

impl SkillManager {
    pub fn new() -> Result<Self> { /* discover from ~/.hermes/skills/ */ }
    pub fn match_for(&self, ctx: &MatchContext) -> Vec<&Skill> { /* condition filtering */ }
    pub fn reload(&mut self) -> Result<()> { /* re-scan directories */ }
}
```

### Injection Strategy

Skills are injected as **user messages** (not system prompt) to preserve prompt cache:

```rust
pub fn inject_into_history(&self, ctx: &MatchContext, history: &mut Vec<Message>) {
    let matched = self.match_for(ctx);
    if matched.is_empty() { return; }
    let combined = matched.iter()
        .map(|s| format!("<skill name=\"{}\">\n{}\n</skill>", s.name, s.body))
        .collect::<Vec<_>>().join("\n\n");
    history.insert(0, Message::user(&format!("[Active skills]\n\n{combined}")));
}
```

### Auto-Creation

After complex interactions (5+ tool calls), the agent suggests creating a skill. The agent calls `skill_manage(action='create')` tool to save it.

### Skill Tools

Registered via inventory like any other tool: `skill_list`, `skill_view`, `skill_manage` (create/update/patch/delete).

---

## 13. Cron System

### Job Model

```rust
pub struct CronJob {
    pub id: Uuid,
    pub name: String,
    pub schedule: JobSchedule,       // Cron("0 9 * * *") | Interval(1800) | Once(DateTime)
    pub prompt: String,
    pub delivery: Vec<DeliveryTarget>,
    pub enabled: bool,
}
```

### Storage

`~/.hermes/cron/jobs.json` (JSON file). Output stored in `~/.hermes/cron/output/{job_id}/{timestamp}.md`.

### Scheduler

Each enabled job gets its own tokio task. Jobs run as full Agent instances (complete tool access, clean context per execution). Results are saved to disk and optionally delivered to platforms via `DeliveryRouter`.

### Cron Tool

Agent-callable tool for managing jobs: `cron(action='list|create|delete|toggle')`.

---

## 14. Compile-Time Tool Registration

### Registration Pattern

```rust
// hermes-tools/src/registry.rs
pub struct ToolRegistration {
    pub factory: fn() -> Box<dyn Tool>,
}
inventory::collect!(ToolRegistration);

// hermes-tools/src/web_search.rs
inventory::submit! {
    ToolRegistration { factory: || Box::new(WebSearchTool::new()) }
}

// At startup
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub async fn new() -> Result<Self> {
        let mut tools = HashMap::new();
        // 1. Compile-time registered tools
        for reg in inventory::iter::<ToolRegistration> {
            let tool = (reg.factory)();
            if tool.is_available() {
                tools.insert(tool.name().to_string(), tool);
            }
        }
        // 2. MCP server discovery (runtime)
        // discover_mcp(&mut tools, &config.mcp_servers).await?;
        Ok(Self { tools })
    }
}
```

### MCP Integration

External tools discovered at startup from `config.yaml` MCP server entries. Each MCP tool gets an `McpToolAdapter` that implements `Tool` by proxying JSON-RPC calls to the server process.

---

## 15. Security

### Dangerous Command Detection

Terminal commands scanned for: `rm`, `rmdir`, `dd`, `shred`, `git reset`, `git clean`, `chmod`, `chown`, `>` redirection, package management.

### Approval Levels

1. **Default**: Every dangerous command requires interactive approval
2. **YOLO mode**: Skip all approvals (user opt-in)
3. **Approval memory**: AllowSession / AllowAlways remembered

### Credential Security

- `.env` file: 0600 permissions (owner-only)
- HERMES_HOME directory: 0700 permissions
- Profile token locks: prevent two profiles from using the same platform token
- All API keys/tokens stored as `Secret<String>` (secrecy crate) — prevents accidental logging/Debug printing at the type level
- `cargo audit` in CI pipeline for dependency vulnerability scanning
- `cargo clippy` + `rustfmt` enforced pre-commit

### Toolchain Requirements

The following must be enforced in CI (per CLAUDE.md rules):
- `rustfmt` — format check
- `cargo clippy` — lint
- `cargo audit` — dependency vulnerability scan
- `cargo test` — unit + integration tests with `#[cfg(test)]` modules per crate and `tests/` for integration tests
- Target 80%+ coverage on new code

---

## 16. Data Flow Summary

### CLI Flow

```
User types message
  -> Slash command check (COMMAND_REGISTRY)
  -> Agent::run_conversation(user_msg, system_prompt, &mut history, delta_tx)
     ├─ Prefetch memory (from cache)
     ├─ Build/reuse frozen system prompt (cache control)
     ├─ Provider::chat() (streaming SSE -> delta_tx)
     ├─ Process tool calls (parallel if safe)
     │   └─ Registry::dispatch() -> Tool::execute()
     ├─ Iterate until no tool calls
     └─ Return final response
  -> Render response (crossterm colors, markdown)
  -> Sync memory + queue prefetch (tokio::spawn, non-blocking)
```

### Gateway Flow

```
Platform message arrives (Telegram/Discord/Slack)
  -> SessionRouter::route() -> per-session mpsc channel
  -> session_task (owns Agent, processes sequentially)
     └─ Agent::run_conversation() (same as CLI)
  -> DeliveryRouter::send() -> format for platform -> send
```

### Compression + Cache Interaction

```
Normal turns: frozen system prompt -> CACHE HIT (90% savings)
Memory writes: persist to disk, don't touch frozen prompt -> CACHE HIT
Compression: summarize old messages, rebuild system prompt -> CACHE MISS (one-time)
Next turns: new frozen prompt -> CACHE HIT
```

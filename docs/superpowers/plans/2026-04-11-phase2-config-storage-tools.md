# Phase 2: Config, Session Storage & Basic Tools — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add persistence (session storage + config expansion) and four foundational tools (terminal, read_file, write_file, search_files) that make the agent actually useful.

**Architecture:** First extend hermes-core with ToolConfig types, is_exclusive trait method, and SessionStore trait. Then expand hermes-config with .env loading, TerminalConfig/FileConfig, and SqliteSessionStore. Next implement tools in hermes-tools with path sandboxing and dangerous command detection. Finally wire everything through the CLI with session persistence, approval handler, and resume support.

**Tech Stack:** tokio-rusqlite, dotenvy, walkdir, regex, tokio::process

---

## Task 1: ToolConfig + is_exclusive in hermes-core

Add ToolConfig types and `is_exclusive()` method to the Tool trait. These are the foundation other tasks build on.

**Files:**
- Modify: `crates/hermes-core/src/tool.rs`

- [ ] **Step 1: Add ToolConfig types and is_exclusive to tool.rs**

Add these types BEFORE the ToolContext struct, and add `tool_config: Arc<ToolConfig>` to ToolContext. Add `is_exclusive()` default method to the Tool trait.

```rust
// Add at top of file, after existing imports:
use std::sync::Arc;

// Add BEFORE ToolContext:
#[derive(Debug, Clone)]
pub struct ToolConfig {
    pub terminal: TerminalToolConfig,
    pub file: FileToolConfig,
    pub workspace_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct TerminalToolConfig {
    pub timeout: u64,
    pub max_timeout: u64,
    pub output_max_chars: usize,
}

impl Default for TerminalToolConfig {
    fn default() -> Self {
        Self {
            timeout: 180,
            max_timeout: 600,
            output_max_chars: 50_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileToolConfig {
    pub read_max_chars: usize,
    pub read_max_lines: usize,
    pub blocked_prefixes: Vec<PathBuf>,
}

impl Default for FileToolConfig {
    fn default() -> Self {
        Self {
            read_max_chars: 100_000,
            read_max_lines: 2000,
            blocked_prefixes: vec![
                PathBuf::from("/etc/"),
                PathBuf::from("/boot/"),
                PathBuf::from("/usr/lib/systemd/"),
            ],
        }
    }
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            terminal: TerminalToolConfig::default(),
            file: FileToolConfig::default(),
            workspace_root: PathBuf::from("."),
        }
    }
}
```

Modify `ToolContext` to include `tool_config`:
```rust
#[derive(Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub working_dir: PathBuf,
    pub approval_tx: mpsc::Sender<ApprovalRequest>,
    pub delta_tx: mpsc::Sender<StreamDelta>,
    pub tool_config: Arc<ToolConfig>,  // NEW
}
```

Add `is_exclusive()` to the Tool trait:
```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> ToolSchema;
    fn toolset(&self) -> &str;
    fn is_available(&self) -> bool { true }
    fn is_read_only(&self) -> bool { false }
    fn is_exclusive(&self) -> bool { false }  // NEW
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult>;
}
```

- [ ] **Step 2: Fix all compilation errors from ToolContext change**

Every place that constructs a `ToolContext` now needs `tool_config`. Update:

1. `crates/hermes-agent/src/loop_runner.rs` — add `tool_config: Arc::new(ToolConfig::default())` to the ToolContext construction in `run_conversation`, and add `use hermes_core::tool::ToolConfig;` import.

2. `crates/hermes-tools/src/registry.rs` — update the test's ToolContext construction to include `tool_config: Arc::new(ToolConfig::default())`, add `use std::sync::Arc; use hermes_core::tool::ToolConfig;`.

3. `crates/hermes-agent/src/parallel.rs` — add `is_exclusive()` check in `should_parallelize`:
```rust
// Add after the NEVER_PARALLEL check:
if calls.iter().any(|c| {
    registry.get(&c.name).map_or(false, |t| t.is_exclusive())
}) {
    return false;
}
```

- [ ] **Step 3: Run tests + clippy + fmt**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt`

Expected: All 78 tests pass, no clippy warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/hermes-core/src/tool.rs crates/hermes-agent/src/loop_runner.rs crates/hermes-agent/src/parallel.rs crates/hermes-tools/src/registry.rs
git commit -m "feat: add ToolConfig types and is_exclusive to Tool trait"
```

---

## Task 2: Config Expansion

Expand AppConfig with TerminalConfig, FileConfig, .env loading. Add `tool_config()` builder method.

**Files:**
- Modify: `crates/hermes-config/src/config.rs`
- Modify: `crates/hermes-config/Cargo.toml`

- [ ] **Step 1: Add dotenvy dependency**

Add to `crates/hermes-config/Cargo.toml` [dependencies]:
```toml
dotenvy.workspace = true
```

- [ ] **Step 2: Expand AppConfig with serde-compatible config structs + .env loading**

In `crates/hermes-config/src/config.rs`:

Add serde-compatible config structs (separate from hermes-core's ToolConfig since these are for YAML deserialization):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalConfigYaml {
    #[serde(default = "default_terminal_timeout")]
    pub timeout: u64,
    #[serde(default = "default_terminal_max_timeout")]
    pub max_timeout: u64,
    #[serde(default = "default_terminal_output_max_chars")]
    pub output_max_chars: usize,
}

fn default_terminal_timeout() -> u64 { 180 }
fn default_terminal_max_timeout() -> u64 { 600 }
fn default_terminal_output_max_chars() -> usize { 50_000 }

impl Default for TerminalConfigYaml {
    fn default() -> Self {
        Self {
            timeout: default_terminal_timeout(),
            max_timeout: default_terminal_max_timeout(),
            output_max_chars: default_terminal_output_max_chars(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileConfigYaml {
    #[serde(default = "default_file_read_max_chars")]
    pub read_max_chars: usize,
    #[serde(default = "default_file_read_max_lines")]
    pub read_max_lines: usize,
}

fn default_file_read_max_chars() -> usize { 100_000 }
fn default_file_read_max_lines() -> usize { 2000 }

impl Default for FileConfigYaml {
    fn default() -> Self {
        Self {
            read_max_chars: default_file_read_max_chars(),
            read_max_lines: default_file_read_max_lines(),
        }
    }
}
```

Add terminal and file fields to AppConfig:
```rust
pub struct AppConfig {
    // ... existing fields ...
    #[serde(default)]
    pub terminal: TerminalConfigYaml,
    #[serde(default)]
    pub file: FileConfigYaml,
}
```

Update `Default for AppConfig` to include the new fields.

Modify `AppConfig::load()` to load .env before config:
```rust
pub fn load() -> Self {
    // 1. Load .env for secrets
    let env_path = hermes_home().join(".env");
    if env_path.exists() {
        let _ = dotenvy::from_path(&env_path);
    }
    // 2. Load config.yaml (existing code)
    // ...
}
```

Add `tool_config()` method that converts YAML config to hermes-core ToolConfig:
```rust
use hermes_core::tool::{ToolConfig, TerminalToolConfig, FileToolConfig};

impl AppConfig {
    pub fn tool_config(&self, workspace_root: PathBuf) -> ToolConfig {
        ToolConfig {
            terminal: TerminalToolConfig {
                timeout: self.terminal.timeout,
                max_timeout: self.terminal.max_timeout,
                output_max_chars: self.terminal.output_max_chars,
            },
            file: FileToolConfig {
                read_max_chars: self.file.read_max_chars,
                read_max_lines: self.file.read_max_lines,
                blocked_prefixes: vec![
                    PathBuf::from("/etc/"),
                    PathBuf::from("/boot/"),
                    PathBuf::from("/usr/lib/systemd/"),
                ],
            },
            workspace_root,
        }
    }
}
```

- [ ] **Step 3: Add tests for new config fields**

Add tests in the existing `#[cfg(test)] mod tests`:
```rust
#[test]
fn config_with_terminal_section() {
    let yaml = r#"
model: "openai/gpt-4o"
terminal:
  timeout: 300
  max_timeout: 900
"#;
    let cfg: AppConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.terminal.timeout, 300);
    assert_eq!(cfg.terminal.max_timeout, 900);
    assert_eq!(cfg.terminal.output_max_chars, 50_000); // default
}

#[test]
fn config_defaults_when_sections_missing() {
    let yaml = "model: openai/gpt-4o\n";
    let cfg: AppConfig = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(cfg.terminal.timeout, 180);
    assert_eq!(cfg.file.read_max_chars, 100_000);
}

#[test]
fn tool_config_conversion() {
    let cfg = AppConfig::default();
    let tc = cfg.tool_config(PathBuf::from("/home/user/project"));
    assert_eq!(tc.workspace_root, PathBuf::from("/home/user/project"));
    assert_eq!(tc.terminal.timeout, 180);
    assert_eq!(tc.file.read_max_chars, 100_000);
    assert!(!tc.file.blocked_prefixes.is_empty());
}
```

- [ ] **Step 4: Run tests + clippy + fmt, commit**

Run: `cargo test -p hermes-config && cargo clippy -p hermes-config -- -D warnings && cargo fmt -p hermes-config`

Commit: `feat: expand config with terminal/file sections and .env loading`

---

## Task 3: SessionStore Trait in hermes-core

Define the SessionStore trait and SessionMeta type.

**Files:**
- Create: `crates/hermes-core/src/session.rs`
- Modify: `crates/hermes-core/src/lib.rs`

- [ ] **Step 1: Create session.rs with trait + types**

Create `crates/hermes-core/src/session.rs`:

```rust
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::message::Message;
use crate::provider::TokenUsage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub source: String,
    pub model: String,
    pub system_prompt: String,
    pub cwd: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub message_count: u32,
    pub tool_call_count: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub title: Option<String>,
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create_session(&self, meta: &SessionMeta) -> Result<()>;
    async fn end_session(&self, session_id: &str) -> Result<()>;
    async fn append_message(&self, session_id: &str, msg: &Message) -> Result<i64>;
    async fn load_history(&self, session_id: &str) -> Result<Vec<Message>>;
    async fn get_session(&self, session_id: &str) -> Result<Option<SessionMeta>>;
    async fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMeta>>;
    async fn update_usage(&self, session_id: &str, usage: &TokenUsage) -> Result<()>;
}
```

- [ ] **Step 2: Add to lib.rs**

Add `pub mod session;` to `crates/hermes-core/src/lib.rs`.

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p hermes-core`

- [ ] **Step 4: Commit**

Commit: `feat: add SessionStore trait and SessionMeta type`

---

## Task 4: SqliteSessionStore

Implement SessionStore with tokio-rusqlite. Single `state.db` file.

**Files:**
- Create: `crates/hermes-config/src/sqlite_store.rs`
- Modify: `crates/hermes-config/src/lib.rs`
- Modify: `crates/hermes-config/Cargo.toml`

- [ ] **Step 1: Add SQLite dependencies**

Add to `crates/hermes-config/Cargo.toml` [dependencies]:
```toml
tokio.workspace = true
tokio-rusqlite.workspace = true
rusqlite.workspace = true
async-trait.workspace = true
anyhow.workspace = true
```

And dev-dependencies:
```toml
[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 2: Implement SqliteSessionStore**

Create `crates/hermes-config/src/sqlite_store.rs` with:

- `SqliteSessionStore` struct wrapping `tokio_rusqlite::Connection`
- `open()` — opens `hermes_home()/state.db`, runs schema migration
- Schema: `sessions` table (id, source, model, system_prompt, cwd, started_at, ended_at, message_count, tool_call_count, input_tokens, output_tokens, title) + `messages` table (id, session_id, role, content, tool_calls JSON, tool_call_id, tool_name, reasoning, created_at) + indexes
- `PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;`
- impl `SessionStore` for `SqliteSessionStore`:
  - `create_session` — INSERT into sessions
  - `end_session` — UPDATE ended_at
  - `append_message` — INSERT into messages, serialize tool_calls as JSON
  - `load_history` — SELECT from messages ORDER BY id, reconstruct Message structs
  - `get_session` — SELECT from sessions WHERE id=?
  - `list_sessions` — SELECT from sessions ORDER BY started_at DESC LIMIT ?
  - `update_usage` — UPDATE sessions SET input_tokens += ?, output_tokens += ?

Message serialization:
- `content` → `Content::as_text_lossy()` for storage, `Content::Text(stored)` on load
- `tool_calls` → `serde_json::to_string(&msg.tool_calls)` if non-empty, NULL otherwise
- On load: parse tool_calls JSON back to `Vec<ToolCall>`

- [ ] **Step 3: Write tests**

Test in `#[cfg(test)]` module:
- `test_create_and_get_session` — create, retrieve, verify fields
- `test_append_and_load_messages` — append user+assistant+tool messages, load back, verify order and content
- `test_message_with_tool_calls_roundtrip` — append message with tool_calls, load, verify JSON roundtrip
- `test_list_sessions` — create 3 sessions, list with limit=2, verify order
- `test_end_session` — create, end, verify ended_at is set

Use `tempfile` or a temp directory for test databases.

- [ ] **Step 4: Wire up lib.rs**

Add to `crates/hermes-config/src/lib.rs`:
```rust
pub mod sqlite_store;
pub use sqlite_store::SqliteSessionStore;
```

- [ ] **Step 5: Run tests + clippy + fmt, commit**

Run: `cargo test -p hermes-config && cargo clippy -p hermes-config -- -D warnings && cargo fmt -p hermes-config`

Commit: `feat: implement SqliteSessionStore with message persistence`

---

## Task 5: Approval Handler Injection

Move approval channel ownership from Agent internals to AgentConfig. The caller (CLI/gateway) provides the `approval_tx`.

**Files:**
- Modify: `crates/hermes-agent/src/loop_runner.rs`

- [ ] **Step 1: Add approval_tx to AgentConfig, remove internal spawn**

In `loop_runner.rs`:

Add `approval_tx` to `AgentConfig`:
```rust
pub struct AgentConfig {
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<ToolRegistry>,
    pub max_iterations: u32,
    pub system_prompt: String,
    pub session_id: String,
    pub working_dir: PathBuf,
    pub approval_tx: mpsc::Sender<ApprovalRequest>,  // NEW: caller provides
    pub tool_config: Arc<ToolConfig>,                  // NEW: for ToolContext
}
```

Simplify `Agent`:
```rust
pub struct Agent {
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    budget: IterationBudget,
    system_prompt: String,
    session_id: String,
    working_dir: PathBuf,
    approval_tx: mpsc::Sender<ApprovalRequest>,
    tool_config: Arc<ToolConfig>,
    // REMOVED: _approval_task
}
```

Simplify `Agent::new` — just store fields, no spawn:
```rust
impl Agent {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            provider: config.provider,
            registry: config.registry,
            budget: IterationBudget::new(config.max_iterations),
            system_prompt: config.system_prompt,
            session_id: config.session_id,
            working_dir: config.working_dir,
            approval_tx: config.approval_tx,
            tool_config: config.tool_config,
        }
    }
}
```

In `run_conversation`, use `self.tool_config` for ToolContext:
```rust
let ctx = ToolContext {
    session_id: self.session_id.clone(),
    working_dir: self.working_dir.clone(),
    approval_tx: self.approval_tx.clone(),
    delta_tx: delta_tx.clone(),
    tool_config: Arc::clone(&self.tool_config),
};
```

- [ ] **Step 2: Fix tests**

Update `make_agent` in tests to provide `approval_tx` and `tool_config`:
```rust
fn make_agent(responses: Vec<ChatResponse>, max_iterations: u32) -> (Agent, mpsc::Receiver<ApprovalRequest>) {
    let provider = Arc::new(MockProvider::new(responses));
    let registry = Arc::new(ToolRegistry::new());
    let (approval_tx, approval_rx) = mpsc::channel(8);
    let agent = Agent::new(AgentConfig {
        provider,
        registry,
        max_iterations,
        system_prompt: "You are a helpful assistant.".to_string(),
        session_id: "test-session".to_string(),
        working_dir: std::env::temp_dir(),
        approval_tx,
        tool_config: Arc::new(ToolConfig::default()),
    });
    (agent, approval_rx)
}
```

Update all test call sites from `make_agent(...)` to destructure `(agent, _rx)`.

- [ ] **Step 3: Fix CLI compilation**

Update `crates/hermes-cli/src/repl.rs` — create approval channel and spawn handler:
```rust
// After creating tool_config:
let tool_config = Arc::new(config.tool_config(working_dir.clone()));

// Create approval channel — CLI spawns the handler
let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalRequest>(8);
tokio::spawn(async move {
    while let Some(req) = approval_rx.recv().await {
        tracing::warn!(tool = %req.tool_name, command = %req.command, "auto-allowing tool");
        let _ = req.response_tx.send(hermes_core::tool::ApprovalDecision::Allow);
    }
});

let agent_config = AgentConfig {
    provider,
    registry,
    max_iterations: config.max_iterations,
    system_prompt: "You are Hermes, a helpful AI assistant.".to_string(),
    session_id,
    working_dir,
    approval_tx,
    tool_config,
};
```

Do the same for `crates/hermes-cli/src/oneshot.rs`.

- [ ] **Step 4: Run tests + clippy + fmt, commit**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt`

Commit: `refactor: move approval handler ownership to caller via AgentConfig`

---

## Task 6: Path Utilities

Shared path resolution, sandbox checking, and binary detection for file tools.

**Files:**
- Create: `crates/hermes-tools/src/path_utils.rs`
- Modify: `crates/hermes-tools/src/lib.rs`
- Modify: `crates/hermes-tools/Cargo.toml`

- [ ] **Step 1: Add regex dependency**

Add to `crates/hermes-tools/Cargo.toml` [dependencies]:
```toml
regex = "1"
```

Add to root `Cargo.toml` [workspace.dependencies]:
```toml
regex = "1"
```

- [ ] **Step 2: Implement path_utils.rs**

Create `crates/hermes-tools/src/path_utils.rs` with:

```rust
use std::path::{Path, PathBuf};
use hermes_core::tool::ToolConfig;

/// Resolve a path relative to working_dir. Handles ~/ expansion.
pub fn resolve_path(path: &str, working_dir: &Path) -> PathBuf {
    let path = if let Some(stripped) = path.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(stripped)
    } else {
        PathBuf::from(path)
    };
    if path.is_absolute() {
        path
    } else {
        working_dir.join(path)
    }
}

/// Check if the resolved path is within the workspace root (sandbox).
/// Returns Err with message if path escapes sandbox.
pub fn check_sandbox(resolved: &Path, workspace_root: &Path) -> Result<(), String> {
    let canonical = std::fs::canonicalize(resolved)
        .or_else(|_| {
            // File may not exist yet (write_file). Check parent.
            if let Some(parent) = resolved.parent() {
                std::fs::canonicalize(parent).map(|p| p.join(resolved.file_name().unwrap_or_default()))
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no parent"))
            }
        })
        .map_err(|e| format!("cannot resolve path {}: {e}", resolved.display()))?;

    let canonical_root = std::fs::canonicalize(workspace_root)
        .unwrap_or_else(|_| workspace_root.to_path_buf());

    if !canonical.starts_with(&canonical_root) {
        return Err(format!(
            "path {} is outside workspace root {}",
            canonical.display(),
            canonical_root.display()
        ));
    }
    Ok(())
}

/// Check if the path is a blocked device path.
pub fn is_blocked_device(path: &Path) -> bool {
    let s = path.to_string_lossy();
    const BLOCKED: &[&str] = &[
        "/dev/zero", "/dev/random", "/dev/urandom", "/dev/full",
        "/dev/stdin", "/dev/tty", "/dev/console",
        "/dev/stdout", "/dev/stderr",
        "/dev/fd/0", "/dev/fd/1", "/dev/fd/2",
    ];
    BLOCKED.iter().any(|b| s.as_ref() == *b)
        || (s.starts_with("/proc/") && s.contains("/fd/"))
}

/// Check if a file has a binary extension.
pub fn has_binary_extension(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    const BINARY_EXTS: &[&str] = &[
        "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp",
        "mp3", "mp4", "wav", "avi", "mkv", "mov", "flac",
        "zip", "tar", "gz", "bz2", "xz", "7z", "rar",
        "exe", "dll", "so", "dylib", "o", "a",
        "pyc", "pyo", "class", "wasm",
        "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
        "db", "sqlite", "sqlite3",
    ];
    BINARY_EXTS.contains(&ext.as_str())
}

/// Check if a path is in the blocked write prefixes.
pub fn is_blocked_write_path(path: &Path, config: &ToolConfig) -> bool {
    let s = path.to_string_lossy();
    config.file.blocked_prefixes.iter().any(|prefix| {
        s.starts_with(&prefix.to_string_lossy().as_ref())
    }) || s.as_ref() == "/var/run/docker.sock"
      || s.as_ref() == "/run/docker.sock"
}
```

Add 10+ unit tests covering: resolve relative/absolute/tilde paths, sandbox pass/escape, blocked devices, binary extensions, blocked write paths.

- [ ] **Step 3: Wire up lib.rs**

Add to `crates/hermes-tools/src/lib.rs`:
```rust
pub mod path_utils;
```

- [ ] **Step 4: Run tests + clippy + fmt, commit**

Commit: `feat: add path utilities with sandbox checking and binary detection`

---

## Task 7: Terminal Tool

Shell command execution with timeout, output truncation, and dangerous command approval.

**Files:**
- Create: `crates/hermes-tools/src/terminal.rs`
- Modify: `crates/hermes-tools/src/lib.rs`

- [ ] **Step 1: Implement terminal tool**

Create `crates/hermes-tools/src/terminal.rs` with:

**Dangerous command patterns** (13 regex patterns from spec):
```rust
use once_cell::sync::Lazy;  // or std::sync::LazyLock on edition 2024
use regex::Regex;

static DANGEROUS_PATTERNS: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| vec![
    (Regex::new(r"(?i)\brm\s+(-[^\s]*\s+)*/").unwrap(), "delete in root path"),
    (Regex::new(r"(?i)\brm\s+-[^\s]*r").unwrap(), "recursive delete"),
    (Regex::new(r"(?i)\bchmod\s+.*\b(777|666)\b").unwrap(), "world-writable permissions"),
    (Regex::new(r"(?i)\bmkfs\b").unwrap(), "format filesystem"),
    (Regex::new(r"(?i)\bdd\s+.*if=").unwrap(), "disk copy"),
    (Regex::new(r"(?i)\bDROP\s+(TABLE|DATABASE)\b").unwrap(), "SQL DROP"),
    (Regex::new(r"(?i)\bDELETE\s+FROM\b(?!.*\bWHERE\b)").unwrap(), "SQL DELETE without WHERE"),
    (Regex::new(r"(?i)>\s*/etc/").unwrap(), "overwrite system config"),
    (Regex::new(r"(?i)\bkill\s+-9\s+-1\b").unwrap(), "kill all processes"),
    (Regex::new(r"(?i)\b(curl|wget)\b.*\|\s*(ba)?sh\b").unwrap(), "pipe remote to shell"),
    (Regex::new(r"(?i)\bgit\s+reset\s+--hard\b").unwrap(), "git reset --hard"),
    (Regex::new(r"(?i)\bgit\s+push\b.*(-f|--force)\b").unwrap(), "git force push"),
    (Regex::new(r"(?i)\bgit\s+clean\s+-[^\s]*f").unwrap(), "git clean with force"),
]);

fn detect_dangerous(command: &str) -> Option<&'static str> {
    for (pattern, description) in DANGEROUS_PATTERNS.iter() {
        if pattern.is_match(command) {
            return Some(description);
        }
    }
    None
}
```

**TerminalTool** struct implementing Tool:
- `name()` → "terminal"
- `toolset()` → "terminal"
- `is_exclusive()` → true
- Schema: command (required), timeout (optional), workdir (optional)
- `execute()`:
  1. Parse args (command, timeout, workdir)
  2. Clamp timeout to config max_timeout
  3. Check dangerous patterns → if dangerous, send ApprovalRequest, await response
  4. Resolve workdir (default to ctx.working_dir)
  5. Spawn `tokio::process::Command::new("bash").args(["-lc", &command])`
  6. Wait with `tokio::time::timeout`
  7. Capture stdout+stderr combined
  8. Truncate output if exceeds output_max_chars (40% head + 60% tail)
  9. Return JSON: `{"output": "...", "exit_code": N, "error": null}`

**Output truncation helper**:
```rust
fn truncate_output(output: &str, max_chars: usize) -> String {
    if output.len() <= max_chars { return output.to_string(); }
    let head_len = max_chars * 40 / 100;
    let tail_len = max_chars * 60 / 100;
    let head = &output[..head_len];
    let tail = &output[output.len() - tail_len..];
    let omitted = output.len() - head_len - tail_len;
    format!("{head}\n\n[...truncated {omitted} chars...]\n\n{tail}")
}
```

**inventory registration**:
```rust
inventory::submit! {
    crate::ToolRegistration { factory: || Box::new(TerminalTool) }
}
```

- [ ] **Step 2: Write tests**

Tests in `#[cfg(test)]`:
- `test_detect_dangerous_rm_rf` — `rm -rf /` is dangerous
- `test_detect_dangerous_git_force_push` — `git push --force` is dangerous
- `test_detect_safe_command` — `ls -la` is not dangerous
- `test_truncate_output_short` — no truncation needed
- `test_truncate_output_long` — verify head/tail split
- `test_terminal_execute_echo` — actually run `echo hello`, verify output+exit_code (integration test)
- `test_terminal_execute_false` — run `false`, verify exit_code=1
- `test_terminal_timeout` — run `sleep 10` with 1s timeout, verify exit_code=124

- [ ] **Step 3: Wire up and commit**

Add `pub mod terminal;` to lib.rs.

Run: `cargo test -p hermes-tools && cargo clippy -p hermes-tools -- -D warnings && cargo fmt -p hermes-tools`

Commit: `feat: implement terminal tool with dangerous command detection`

---

## Task 8: read_file Tool

File reading with line numbers, pagination, binary detection, and sandbox checking.

**Files:**
- Create: `crates/hermes-tools/src/file_read.rs`
- Modify: `crates/hermes-tools/src/lib.rs`

- [ ] **Step 1: Implement read_file tool**

Create `crates/hermes-tools/src/file_read.rs`:

**ReadFileTool** implementing Tool:
- `name()` → "read_file", `toolset()` → "file", `is_read_only()` → true
- Schema: path (required), offset (default 1, min 1), limit (default 500, max 2000)
- `execute()`:
  1. Parse args
  2. Resolve path via `path_utils::resolve_path`
  3. Check `is_blocked_device`
  4. Check `has_binary_extension` → return error suggesting appropriate tool
  5. Read file content (`std::fs::read_to_string`)
  6. Split into lines, get total_lines and file_size
  7. Check total chars against `config.file.read_max_chars` → error if exceeded without offset/limit
  8. Apply offset (1-indexed) and limit
  9. Format as `"{line_num}|{line}\n"` for each line
  10. Return JSON: `{"content": "...", "path": "...", "file_size": N, "total_lines": N, "showing": "lines X-Y of Z"}`

- [ ] **Step 2: Write tests + inventory registration**

Tests:
- `test_read_file_basic` — create temp file, read it, verify line-numbered content
- `test_read_file_offset_limit` — verify pagination
- `test_read_file_binary_blocked` — .png path returns error
- `test_read_file_device_blocked` — /dev/zero returns error
- `test_read_file_not_found` — returns error

Inventory: `inventory::submit! { crate::ToolRegistration { factory: || Box::new(ReadFileTool) } }`

- [ ] **Step 3: Wire up and commit**

Add `pub mod file_read;` to lib.rs.

Commit: `feat: implement read_file tool with line numbers and safety checks`

---

## Task 9: write_file Tool

File writing with path validation and atomic writes.

**Files:**
- Create: `crates/hermes-tools/src/file_write.rs`
- Modify: `crates/hermes-tools/src/lib.rs`

- [ ] **Step 1: Implement write_file tool**

Create `crates/hermes-tools/src/file_write.rs`:

**WriteFileTool** implementing Tool:
- `name()` → "write_file", `toolset()` → "file"
- Schema: path (required), content (required)
- `execute()`:
  1. Resolve path, check sandbox
  2. Check `is_blocked_write_path` → error if blocked
  3. Create parent directories (`fs::create_dir_all`)
  4. Check if file already exists (for `created` field)
  5. Write content to file (`fs::write`)
  6. Return JSON: `{"path": "...", "bytes_written": N, "created": bool}`

- [ ] **Step 2: Write tests + inventory**

Tests:
- `test_write_file_create` — write to new file, verify content
- `test_write_file_overwrite` — write, overwrite, verify
- `test_write_file_creates_parent_dirs` — write to nested path
- `test_write_file_blocked_path` — /etc/foo returns error

- [ ] **Step 3: Wire up and commit**

Commit: `feat: implement write_file tool with path validation`

---

## Task 10: search_files Tool

Regex content search and file glob search.

**Files:**
- Create: `crates/hermes-tools/src/file_search.rs`
- Modify: `crates/hermes-tools/src/lib.rs`
- Modify: `crates/hermes-tools/Cargo.toml`

- [ ] **Step 1: Add walkdir dependency**

walkdir is already in workspace deps and hermes-tools Cargo.toml — verify it's there. If not, add:
```toml
walkdir.workspace = true
```

- [ ] **Step 2: Implement search_files tool**

Create `crates/hermes-tools/src/file_search.rs`:

**SearchFilesTool** implementing Tool:
- `name()` → "search_files", `toolset()` → "file", `is_read_only()` → true
- Schema: pattern (required), target (content|files, default content), path (default "."), file_glob (optional), limit (default 50), context (default 0)
- `execute()` for target="content":
  1. Resolve search root path
  2. Compile regex pattern
  3. Walk directory tree (walkdir), skip hidden dirs and binary files
  4. For each file: read lines, match pattern, collect matches with line numbers
  5. Apply context lines if context > 0
  6. Truncate to limit
  7. Return JSON: `{"matches": [...], "total_matches": N, "truncated": bool}`
- `execute()` for target="files":
  1. Walk directory tree
  2. Match filenames against glob pattern
  3. Return file paths

File glob matching: use pattern matching on the file name component.

- [ ] **Step 3: Write tests + inventory**

Tests:
- `test_search_content_basic` — create temp files, search for pattern, verify matches
- `test_search_content_with_glob` — filter by *.rs
- `test_search_content_limit` — verify truncation
- `test_search_files_mode` — target=files, find files by glob
- `test_search_no_results` — pattern matches nothing

- [ ] **Step 4: Wire up and commit**

Commit: `feat: implement search_files tool with regex and glob support`

---

## Task 11: CLI Session Persistence & Resume

Wire session storage into the REPL. Persist conversations and add `--resume` / `--list-sessions` flags.

**Files:**
- Modify: `crates/hermes-cli/src/main.rs`
- Modify: `crates/hermes-cli/src/repl.rs`
- Modify: `crates/hermes-cli/src/oneshot.rs`

- [ ] **Step 1: Add CLI flags**

In `main.rs`, add to the Cli struct:
```rust
/// Resume a previous session (most recent if no ID given).
#[arg(long)]
resume: Option<Option<String>>,  // --resume or --resume=<id>

/// List recent sessions.
#[arg(long)]
list_sessions: bool,
```

Handle in main:
```rust
if cli.list_sessions {
    return list_sessions_cmd().await;
}
if let Some(resume_id) = cli.resume {
    return repl::run_repl_with_resume(resume_id).await;
}
```

Implement `list_sessions_cmd()`: open SqliteSessionStore, list_sessions(20), print table.

- [ ] **Step 2: Add session persistence to repl.rs**

In `run_repl()`:
1. Open `SqliteSessionStore::open().await`
2. Create session via `store.create_session(&meta)`
3. After each `agent.run_conversation()` call, persist messages:
   - Before the call: persist the user message via `store.append_message()`
   - After the call: persist all new messages (assistant + tool results)
4. On `/new`: end current session, create new one
5. On exit: end session

- [ ] **Step 3: Implement resume**

Add `run_repl_with_resume(resume_id: Option<String>)`:
1. Open store
2. If resume_id is None: `store.list_sessions(1)` to get most recent
3. Load session metadata
4. Load message history via `store.load_history()`
5. Reconstruct Agent with same model/system_prompt
6. Print resume banner with session info
7. Continue normal REPL loop

- [ ] **Step 4: Run tests + clippy + fmt, commit**

Run: `cargo build -p hermes-cli && cargo clippy --workspace -- -D warnings && cargo fmt`

Commit: `feat: add session persistence and --resume support to CLI`

---

## Task 12: Full Build Verification

Ensure everything compiles, passes clippy, and all tests pass.

**Files:**
- No new files

- [ ] **Step 1: Run full verification**

```bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo build --release -p hermes-cli
```

Fix any issues found.

- [ ] **Step 2: Smoke test with real API**

```bash
OPENAI_API_KEY=<key> ./target/release/hermes --message "Create a file called /tmp/hermes-test.txt with the content 'hello from hermes' using the write_file tool, then read it back with read_file." --model "openai/gemini-3.1-pro-preview" --base-url "http://34.60.178.0:3000/v1"
```

Verify:
- Terminal and file tools are available to the LLM
- Tool execution works (file created and read back)
- Session is persisted to state.db

- [ ] **Step 3: Commit any fixes**

Commit: `chore: fix Phase 2 build issues`

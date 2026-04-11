# Phase 2: Config, Session Storage & Basic Tools — Design Spec

**Date**: 2026-04-11
**Status**: Draft
**Depends on**: Phase 1 (complete)
**Validates**: Persistence patterns, serde, tool execution model

---

## 1. Scope

### In Scope
- Config expansion: terminal config, .env loading, tool config injection
- Session storage: single `state.db`, message persistence, session resume
- Approval handler: move ownership from Agent to caller (CLI/gateway)
- Four tools: `terminal`, `read_file`, `write_file`, `search_files`
- Workspace root path policy for file tool sandboxing

### Out of Scope (deferred)
- FTS5 full-text search (Phase 3)
- Docker/SSH/Modal terminal backends (Phase 5)
- Config sections: browser, compression, auxiliary models, smart routing, display
- Patch tool, list_directory, file dedup/staleness tracking
- Background task execution
- `AllowAlways` approval persistence
- Cross-session search, session chaining

---

## 2. Architectural Changes to Existing Code

### 2.1 Approval Handler Injection

**Problem**: Agent currently creates its own approval channel internally and auto-allows everything. CLI/gateway cannot provide custom approval UI.

**Fix**: Accept an `approval_tx` through `AgentConfig` instead of creating one internally.

```rust
// hermes-agent/src/loop_runner.rs — MODIFIED
pub struct AgentConfig {
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<ToolRegistry>,
    pub max_iterations: u32,
    pub system_prompt: String,
    pub session_id: String,
    pub working_dir: PathBuf,
    pub approval_tx: mpsc::Sender<ApprovalRequest>,  // NEW: caller provides
}

pub struct Agent {
    // ... existing fields ...
    approval_tx: mpsc::Sender<ApprovalRequest>,  // from config, no internal spawn
    // REMOVE: _approval_task: JoinHandle<()>
}
```

The CLI spawns the approval handler task and passes `approval_tx` into `AgentConfig`. The Agent just uses it. Gateway can provide a different handler (auto-deny, platform prompt, etc.).

### 2.2 ToolContext Config Injection

**Problem**: Tools need access to configuration (timeouts, path policies, read limits) but ToolContext only has session_id, working_dir, channels.

**Fix**: Add `Arc<ToolConfig>` to ToolContext. `ToolConfig` is defined in hermes-core (not hermes-config) to avoid a circular dependency. hermes-config constructs it from AppConfig.

```rust
// hermes-core/src/tool.rs — MODIFIED
#[derive(Debug, Clone)]
pub struct ToolConfig {
    pub terminal: TerminalToolConfig,
    pub file: FileToolConfig,
    pub workspace_root: PathBuf,  // canonicalized, used as sandbox boundary
}

#[derive(Debug, Clone)]
pub struct TerminalToolConfig {
    pub timeout: u64,           // default 180s
    pub max_timeout: u64,       // hard cap 600s
    pub output_max_chars: usize, // default 50_000
}

#[derive(Debug, Clone)]
pub struct FileToolConfig {
    pub read_max_chars: usize,  // default 100_000
    pub read_max_lines: usize,  // default 2000
    pub blocked_prefixes: Vec<PathBuf>,  // /etc/, /boot/, etc.
}

#[derive(Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub working_dir: PathBuf,
    pub approval_tx: mpsc::Sender<ApprovalRequest>,
    pub delta_tx: mpsc::Sender<StreamDelta>,
    pub tool_config: Arc<ToolConfig>,  // NEW
}
```

### 2.3 Terminal Tool Parallelization Guard

**Problem**: Terminal commands mutate workspace but are invisible to path-based conflict detection.

**Fix**: Terminal tool returns `is_read_only() = false` (already correct) but add a new trait method to explicitly block parallelization:

```rust
// hermes-core/src/tool.rs — ADD method to Tool trait
#[async_trait]
pub trait Tool: Send + Sync {
    // ... existing methods ...
    fn is_exclusive(&self) -> bool { false }  // NEW: if true, never parallelize
}
```

```rust
// hermes-agent/src/parallel.rs — MODIFIED
pub fn should_parallelize(calls: &[ToolCall], registry: &ToolRegistry) -> bool {
    // ... existing checks ...
    // Any exclusive tool -> sequential
    if calls.iter().any(|c| registry.get(&c.name).map_or(false, |t| t.is_exclusive())) {
        return false;
    }
    // ... rest unchanged ...
}
```

---

## 3. Config Expansion

### 3.1 AppConfig Structure

```rust
// hermes-config/src/config.rs — EXPANDED
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default)]
    pub terminal: TerminalConfig,  // NEW
    #[serde(default)]
    pub file: FileConfig,          // NEW
}
```

### 3.2 .env Loading

Use `dotenvy` to load `~/.hermes/.env` before config parsing. This provides API keys and secrets without putting them in YAML.

```rust
impl AppConfig {
    pub fn load() -> Self {
        // 1. Load .env file (secrets)
        let env_path = hermes_home().join(".env");
        if env_path.exists() {
            let _ = dotenvy::from_path(&env_path);
        }

        // 2. Load config.yaml
        let config_path = hermes_home().join("config.yaml");
        // ... existing YAML loading ...
    }
}
```

Add `dotenvy` back to hermes-config Cargo.toml dependencies.

### 3.3 ToolConfig Construction

`ToolConfig` is built from `AppConfig` + runtime state (workspace_root from CWD):

```rust
impl AppConfig {
    pub fn tool_config(&self, workspace_root: PathBuf) -> ToolConfig {
        ToolConfig {
            terminal: self.terminal.clone(),
            file: self.file.clone(),
            workspace_root,
        }
    }
}
```

---

## 4. Session Storage

### 4.1 Design Choice: Single state.db

Per Codex review recommendation: use a single `~/.hermes/state.db` instead of per-session files. Simpler for resume, listing, migrations, and future cross-session features.

### 4.2 Schema

```sql
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS sessions (
    id            TEXT PRIMARY KEY,
    source        TEXT NOT NULL DEFAULT 'cli',
    model         TEXT,
    system_prompt TEXT,
    cwd           TEXT,
    started_at    TEXT NOT NULL DEFAULT (datetime('now')),
    ended_at      TEXT,
    message_count INTEGER DEFAULT 0,
    tool_call_count INTEGER DEFAULT 0,
    input_tokens  INTEGER DEFAULT 0,
    output_tokens INTEGER DEFAULT 0,
    title         TEXT
);

CREATE INDEX IF NOT EXISTS idx_sessions_started ON sessions(started_at DESC);

CREATE TABLE IF NOT EXISTS messages (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id    TEXT NOT NULL REFERENCES sessions(id),
    role          TEXT NOT NULL,
    content       TEXT,
    tool_calls    TEXT,           -- JSON array
    tool_call_id  TEXT,
    tool_name     TEXT,
    reasoning     TEXT,
    created_at    TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, id);
```

Deliberately simpler than Python's v6 schema:
- No billing/cost columns (not needed yet)
- No parent_session_id (no session chaining yet)
- No FTS5 (deferred to Phase 3)
- `datetime('now')` text format instead of REAL timestamps

### 4.3 SessionStore Trait + SQLite Implementation

```rust
// hermes-config/src/session.rs — NEW (trait only, for abstraction)
// Actually: put the trait in hermes-core, impl in hermes-config

// hermes-core/src/session.rs — NEW
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create_session(&self, meta: SessionMeta) -> Result<()>;
    async fn end_session(&self, session_id: &str) -> Result<()>;
    async fn append_message(&self, session_id: &str, msg: &Message) -> Result<i64>;
    async fn load_history(&self, session_id: &str) -> Result<Vec<Message>>;
    async fn get_session(&self, session_id: &str) -> Result<Option<SessionMeta>>;
    async fn list_sessions(&self, limit: usize) -> Result<Vec<SessionMeta>>;
    async fn update_usage(&self, session_id: &str, usage: &TokenUsage) -> Result<()>;
}

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
```

```rust
// hermes-config/src/sqlite_store.rs — NEW
pub struct SqliteSessionStore {
    conn: tokio_rusqlite::Connection,
}

impl SqliteSessionStore {
    pub async fn open() -> Result<Self> {
        let path = hermes_home().join("state.db");
        fs::create_dir_all(path.parent().unwrap())?;
        let conn = tokio_rusqlite::Connection::open(&path).await?;
        // Run schema migration
        conn.call(|c| { c.execute_batch(SCHEMA_SQL)?; Ok(()) }).await?;
        Ok(Self { conn })
    }
}

#[async_trait]
impl SessionStore for SqliteSessionStore {
    // Each method uses conn.call(|c| { ... }) for rusqlite operations
    // Messages are serialized: tool_calls as JSON, content as text
}
```

### 4.4 Message Serialization

Messages are stored with these mappings:
- `role` → `Role` enum as lowercase string
- `content` → `Content::as_text_lossy()` (text representation)
- `tool_calls` → `serde_json::to_string(&msg.tool_calls)` (JSON array or NULL)
- `tool_call_id`, `tool_name`, `reasoning` → direct Option fields

On load, reconstruct `Message` from stored fields. `Content` is always loaded as `Content::Text` (multipart images are not persisted).

### 4.5 CLI Resume

```
hermes --resume              # resume most recent session
hermes --resume <session_id> # resume specific session
hermes --list-sessions       # list recent sessions
```

Resume flow:
1. Open `state.db`
2. Load session metadata
3. Load message history
4. Reconstruct Agent with same model/system_prompt
5. Continue conversation

---

## 5. Tools

### 5.1 Terminal Tool

**Name**: `terminal`
**Toolset**: `terminal`
**Exclusive**: `true` (never parallelized)

**Schema**:
```json
{
  "name": "terminal",
  "description": "Execute a shell command. Returns JSON with output, exit_code, and error fields.",
  "parameters": {
    "type": "object",
    "properties": {
      "command": { "type": "string", "description": "Shell command to execute" },
      "timeout": { "type": "integer", "description": "Max seconds (default: 180, max: 600)", "minimum": 1 },
      "workdir": { "type": "string", "description": "Working directory (absolute path)" }
    },
    "required": ["command"]
  }
}
```

Phase 2 omits: `background`, `pty`, `check_interval`, `notify_on_complete`, `force`.

**Execution**:
```rust
pub struct TerminalTool;

impl TerminalTool {
    async fn execute_command(
        command: &str,
        timeout: Duration,
        workdir: &Path,
    ) -> TerminalResult {
        // 1. Spawn: tokio::process::Command::new("bash").args(["-lc", command])
        // 2. Set cwd, capture stdout+stderr
        // 3. Wait with timeout (tokio::time::timeout)
        // 4. On timeout: kill process, return exit_code=124
        // 5. Return TerminalResult { stdout, stderr, exit_code }
    }
}
```

**Structured Output**:
```rust
#[derive(Serialize)]
struct TerminalOutput {
    output: String,        // stdout + stderr combined, truncated
    exit_code: i32,
    error: Option<String>, // set on timeout or spawn failure
}
```

Output truncation: max `output_max_chars` (default 50,000). Truncation keeps 40% head + 60% tail with `[...truncated N chars...]` marker.

**Dangerous Command Approval**:

Before execution, check command against pattern list. If dangerous:
1. Send `ApprovalRequest` through `ctx.approval_tx`
2. Wait for response via oneshot channel
3. `Allow` / `AllowSession` → execute
4. `Deny` → return error "Command denied by user"

Pattern list (subset of Python's 40+ patterns, covering the most critical ones):

```rust
const DANGEROUS_PATTERNS: &[(&str, &str)] = &[
    (r"\brm\s+(-[^\s]*\s+)*/", "delete in root path"),
    (r"\brm\s+-[^\s]*r", "recursive delete"),
    (r"\bchmod\s+.*\b(777|666)\b", "world-writable permissions"),
    (r"\bmkfs\b", "format filesystem"),
    (r"\bdd\s+.*if=", "disk copy"),
    (r"\bDROP\s+(TABLE|DATABASE)\b", "SQL DROP"),
    (r"\bDELETE\s+FROM\b(?!.*\bWHERE\b)", "SQL DELETE without WHERE"),
    (r">\s*/etc/", "overwrite system config"),
    (r"\bkill\s+-9\s+-1\b", "kill all processes"),
    (r"\b(curl|wget)\b.*\|\s*(ba)?sh\b", "pipe remote content to shell"),
    (r"\bgit\s+reset\s+--hard\b", "git reset --hard"),
    (r"\bgit\s+push\b.*(-f|--force)\b", "git force push"),
    (r"\bgit\s+clean\s+-[^\s]*f", "git clean with force"),
];
```

### 5.2 read_file Tool

**Name**: `read_file`
**Toolset**: `file`
**Read-only**: `true`

**Schema**:
```json
{
  "name": "read_file",
  "description": "Read a text file with line numbers. Returns paginated content.",
  "parameters": {
    "type": "object",
    "properties": {
      "path": { "type": "string", "description": "File path (absolute or relative to workspace)" },
      "offset": { "type": "integer", "description": "Line to start from (1-indexed)", "default": 1, "minimum": 1 },
      "limit": { "type": "integer", "description": "Max lines to read", "default": 500, "maximum": 2000 }
    },
    "required": ["path"]
  }
}
```

**Path Resolution**:
1. Resolve relative paths against `ctx.working_dir`
2. Canonicalize via `std::fs::canonicalize`
3. Verify result is under `ctx.tool_config.workspace_root` (sandbox check)
4. Block device paths: `/dev/*`, `/proc/*/fd/*`

**Binary Detection**:
Extension-based check (no I/O needed):
```rust
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "svg",
    "mp3", "mp4", "wav", "avi", "mkv", "mov", "flac",
    "zip", "tar", "gz", "bz2", "xz", "7z", "rar",
    "exe", "dll", "so", "dylib", "o", "a",
    "pyc", "pyo", "class", "wasm",
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
    "db", "sqlite", "sqlite3",
];
```

**Output**: Line-numbered content (`{line_num}|{content}`) with metadata:
```rust
#[derive(Serialize)]
struct ReadFileOutput {
    content: String,       // "1|first line\n2|second line\n..."
    path: String,
    file_size: u64,
    total_lines: usize,
    showing: String,       // "lines 1-500 of 1234"
}
```

**Size Guard**: If total characters exceed `read_max_chars` (default 100K), return error suggesting offset/limit.

### 5.3 write_file Tool

**Name**: `write_file`
**Toolset**: `file`
**Read-only**: `false`

**Schema**:
```json
{
  "name": "write_file",
  "description": "Write content to a file, replacing existing content. Creates parent directories if needed.",
  "parameters": {
    "type": "object",
    "properties": {
      "path": { "type": "string", "description": "File path" },
      "content": { "type": "string", "description": "Complete file content" }
    },
    "required": ["path", "content"]
  }
}
```

**Path Validation**:
1. Same resolution as read_file (canonicalize, sandbox check)
2. Block sensitive prefixes: `/etc/`, `/boot/`, `/usr/lib/systemd/`
3. Block exact paths: `/var/run/docker.sock`, `/run/docker.sock`

**Execution**:
1. Create parent directories (`fs::create_dir_all`)
2. Write atomically: write to temp file, then rename (prevents partial writes)
3. Return success with bytes written

**Output**:
```rust
#[derive(Serialize)]
struct WriteFileOutput {
    path: String,
    bytes_written: usize,
    created: bool,  // true if file didn't exist before
}
```

### 5.4 search_files Tool

**Name**: `search_files`
**Toolset**: `file`
**Read-only**: `true`

**Schema**:
```json
{
  "name": "search_files",
  "description": "Search file contents using regex or find files by glob pattern.",
  "parameters": {
    "type": "object",
    "properties": {
      "pattern": { "type": "string", "description": "Regex pattern (content search) or glob (file search)" },
      "target": { "type": "string", "enum": ["content", "files"], "default": "content" },
      "path": { "type": "string", "description": "Directory to search in", "default": "." },
      "file_glob": { "type": "string", "description": "Filter files by glob (e.g., '*.rs')" },
      "limit": { "type": "integer", "description": "Max results", "default": 50 },
      "context": { "type": "integer", "description": "Context lines around matches", "default": 0 }
    },
    "required": ["pattern"]
  }
}
```

**Implementation**:
- Content search: use `grep` crate or shell out to `rg` (ripgrep) if available, fall back to `grep -rn`
- File search: use `walkdir` + glob matching
- Path resolution: relative to `ctx.working_dir`, sandboxed under `workspace_root`

**Output**:
```rust
#[derive(Serialize)]
struct SearchOutput {
    matches: Vec<SearchMatch>,
    total_matches: usize,
    truncated: bool,
}

#[derive(Serialize)]
struct SearchMatch {
    path: String,
    line: Option<usize>,     // None for file-only mode
    content: Option<String>, // None for file-only mode
}
```

---

## 6. File Structure

### New files
```
crates/hermes-core/src/session.rs          # SessionStore trait, SessionMeta
crates/hermes-config/src/sqlite_store.rs   # SqliteSessionStore implementation
crates/hermes-config/src/tool_config.rs    # ToolConfig, TerminalConfig, FileConfig
crates/hermes-tools/src/terminal.rs        # TerminalTool + dangerous command detection
crates/hermes-tools/src/file_read.rs       # ReadFileTool
crates/hermes-tools/src/file_write.rs      # WriteFileTool
crates/hermes-tools/src/file_search.rs     # SearchFilesTool
crates/hermes-tools/src/path_utils.rs      # Path resolution, sandbox check, binary detection
```

### Modified files
```
crates/hermes-core/src/lib.rs              # add pub mod session
crates/hermes-core/src/tool.rs             # add is_exclusive() to Tool trait, Arc<ToolConfig> to ToolContext
crates/hermes-config/src/lib.rs            # add pub mod sqlite_store, tool_config
crates/hermes-config/src/config.rs         # expand AppConfig, add .env loading
crates/hermes-config/Cargo.toml            # add dotenvy, tokio-rusqlite, rusqlite back
crates/hermes-tools/src/lib.rs             # add tool modules
crates/hermes-tools/Cargo.toml             # add walkdir, regex, tokio process deps
crates/hermes-agent/src/loop_runner.rs     # approval_tx from AgentConfig, remove internal spawn
crates/hermes-agent/src/parallel.rs        # add is_exclusive() check
crates/hermes-cli/src/main.rs              # add --resume, --list-sessions
crates/hermes-cli/src/repl.rs              # session persistence, approval handler spawn
```

### Dependency graph addition
```
cli ──→ agent ──→ tools ──→ core
               ──→ config ──→ core   (config now has SQLite)
```

---

## 7. Testing Strategy

| Component | Test Type | Key Scenarios |
|-----------|-----------|---------------|
| ToolConfig serde | Unit | defaults, YAML roundtrip, partial override |
| .env loading | Unit | file exists, missing, env var override |
| SessionStore | Integration | create/append/load/list/resume, concurrent access |
| Message serialization | Unit | roundtrip through SQLite (tool_calls JSON) |
| Path resolution | Unit | relative, absolute, sandbox escape attempts, symlinks |
| Binary detection | Unit | known extensions, edge cases |
| Dangerous command detection | Unit | all 13 patterns, false positives, normalization |
| Terminal execution | Integration | echo, timeout, exit codes, workdir |
| read_file | Integration | normal, large file, binary, device path, offset/limit |
| write_file | Integration | create, overwrite, blocked paths, parent dir creation |
| search_files | Integration | content match, file glob, limit, no results |
| Approval flow | Integration | allow, deny, timeout |
| is_exclusive parallelism | Unit | terminal blocks parallel, read_file allows |

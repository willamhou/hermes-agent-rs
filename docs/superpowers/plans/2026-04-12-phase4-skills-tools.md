# Phase 4: Skills System + Extended Tools — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add 6 new tools (patch, memory, web_search, web_extract, vision, execute_code) and a full skills system (discovery, matching, injection, management) to the agent.

**Architecture:** First extend ToolContext with three new trait-based access handles (MemoryAccess, SkillAccess, aux_provider). Then implement tools independently. Then build the skills system in hermes-skills. Finally wire everything into the agent loop and CLI. Tasks 3 and 10 modify loop_runner.rs — they must be sequential with each other.

**Tech Stack:** reqwest (web tools), base64 (vision), serde_yaml_ng (skill frontmatter), inventory (tool registration)

---

## File Structure

### New files
```
crates/hermes-tools/src/patch.rs            # Find-and-replace file editing
crates/hermes-tools/src/memory_tools.rs     # memory_read + memory_write
crates/hermes-tools/src/web_search.rs       # Tavily web search
crates/hermes-tools/src/web_extract.rs      # URL content extraction + SSRF guard
crates/hermes-tools/src/vision.rs           # Image analysis via provider
crates/hermes-tools/src/execute_code.rs     # Python code execution (opt-in)
crates/hermes-skills/src/skill.rs           # Skill struct + YAML frontmatter parser
crates/hermes-skills/src/manager.rs         # SkillManager (discovery, matching)
crates/hermes-skills/src/tools.rs           # skill_list, skill_view, skill_manage tools
```

### Modified files
```
crates/hermes-core/src/tool.rs              # MemoryAccess, SkillAccess traits + ToolContext expansion
crates/hermes-core/src/lib.rs               # re-export new traits
crates/hermes-tools/src/lib.rs              # wire new modules
crates/hermes-tools/Cargo.toml              # add base64, reqwest deps
crates/hermes-skills/src/lib.rs             # wire modules
crates/hermes-skills/Cargo.toml             # add deps
crates/hermes-agent/src/loop_runner.rs      # memory refresh + skill injection + pass new ToolContext fields
crates/hermes-agent/Cargo.toml              # add hermes-skills dep
crates/hermes-cli/src/repl.rs               # construct SkillManager
crates/hermes-cli/src/oneshot.rs            # same
crates/hermes-cli/Cargo.toml                # add hermes-skills dep
```

---

## Task 1: ToolContext Expansion (Core Traits)

Add MemoryAccess, SkillAccess traits and expand ToolContext with three new Optional fields. This is the foundation all new tools build on.

**Files:**
- Modify: `crates/hermes-core/src/tool.rs`

- [ ] **Step 1: Add traits and expand ToolContext**

Add to `crates/hermes-core/src/tool.rs`, BEFORE the ToolContext struct:

```rust
use crate::provider::Provider;

// ── Memory access for memory tools ──
#[async_trait]
pub trait MemoryAccess: Send + Sync {
    fn read_live(&self, key: &str) -> Result<Option<String>>;
    fn write_live(&self, key: &str, content: &str) -> Result<()>;
    fn refresh_snapshot(&self) -> Result<()>;
    async fn on_memory_write(&self, action: &str, target: &str, content: &str) -> Result<()>;
}

// ── Skill access for skill tools ──
#[derive(Debug, Clone)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct SkillDoc {
    pub name: String,
    pub description: String,
    pub body: String,
}

pub trait SkillAccess: Send + Sync {
    fn list(&self) -> Vec<SkillSummary>;
    fn get(&self, name: &str) -> Option<SkillDoc>;
    fn create(&self, name: &str, content: &str) -> Result<()>;
    fn edit(&self, name: &str, content: &str) -> Result<()>;
    fn delete(&self, name: &str) -> Result<()>;
    fn reload(&self) -> Result<()>;
}
```

Add three new fields to ToolContext:
```rust
#[derive(Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub working_dir: PathBuf,
    pub approval_tx: mpsc::Sender<ApprovalRequest>,
    pub delta_tx: mpsc::Sender<StreamDelta>,
    pub tool_config: Arc<ToolConfig>,
    pub memory: Option<Arc<dyn MemoryAccess>>,      // NEW
    pub aux_provider: Option<Arc<dyn Provider>>,     // NEW
    pub skills: Option<Arc<dyn SkillAccess>>,        // NEW
}
```

- [ ] **Step 2: Fix ALL compilation errors**

Every ToolContext construction site needs three new `None` fields. Files to update:
- `crates/hermes-agent/src/loop_runner.rs` — ToolContext in run_conversation
- `crates/hermes-tools/src/registry.rs` — test ToolContext
- `crates/hermes-agent/tests/e2e_test.rs` — test ToolContext

Add `memory: None, aux_provider: None, skills: None` to each.

- [ ] **Step 3: Run tests + clippy + fmt, commit**

Commit: `feat: add MemoryAccess, SkillAccess traits and expand ToolContext`

---

## Task 2: Patch Tool

Find-and-replace file editing with whitespace-aware fallback matching.

**Files:**
- Create: `crates/hermes-tools/src/patch.rs`
- Modify: `crates/hermes-tools/src/lib.rs`

- [ ] **Step 1: Implement patch tool**

Create `crates/hermes-tools/src/patch.rs`:

**PatchTool** implementing Tool:
- `name()` → "patch", `toolset()` → "file", `is_read_only()` → false
- Schema: path (required), old_string (required), new_string (required), replace_all (default false)
- `execute()`:
  1. Parse args
  2. Resolve path, check sandbox (reuse path_utils)
  3. Read file content
  4. **Exact match first**: `content.matches(old_string).count()`
  5. If no exact match, **whitespace-aware fallback**:
     - Split `old_string` into alternating literal and whitespace segments
     - Build a regex where literal parts are `regex::escape`d and whitespace runs become `\s+`
     - Search in original content, collect byte ranges
  6. If 0 matches → error "old_string not found"
  7. If >1 matches and !replace_all → error "N matches found, set replace_all=true"
  8. Replace (from end to start to preserve offsets if replace_all)
  9. Write back via `std::fs::write`
  10. Return `{ "path": "...", "replacements": N }`

**Whitespace-aware matching helper**:
```rust
fn whitespace_aware_find(content: &str, pattern: &str) -> Vec<std::ops::Range<usize>> {
    // Split pattern into segments: alternating literal/whitespace
    // Build regex: literal parts escaped, whitespace → \s+
    // Find all matches, return byte ranges
}
```

Register: `inventory::submit! { crate::ToolRegistration { factory: || Box::new(PatchTool) } }`

**7 tests**:
- `test_patch_exact_match` — exact string found and replaced
- `test_patch_whitespace_aware` — old_string has 2 spaces, file has tab → still matches
- `test_patch_no_match` — returns error
- `test_patch_multiple_matches_no_replace_all` — returns error with count
- `test_patch_replace_all` — all occurrences replaced
- `test_patch_sandbox_blocked` — path outside workspace
- `test_patch_preserves_rest` — only matched portion changed, rest intact

- [ ] **Step 2: Wire up, test, commit**

Add `pub mod patch;` to lib.rs.

Commit: `feat: implement patch tool with whitespace-aware matching`

---

## Task 3: Memory Tools + Agent Memory Refresh

Memory read/write tools plus the agent loop changes to refresh snapshot after writes.

**Files:**
- Create: `crates/hermes-tools/src/memory_tools.rs`
- Modify: `crates/hermes-tools/src/lib.rs`
- Modify: `crates/hermes-memory/src/manager.rs` (implement MemoryAccess)
- Modify: `crates/hermes-agent/src/loop_runner.rs` (pass memory handle + post-write refresh)

- [ ] **Step 1: Implement MemoryAccess on MemoryManager**

In `crates/hermes-memory/src/manager.rs`, add:

```rust
use hermes_core::tool::MemoryAccess;

impl MemoryAccess for MemoryManager {
    fn read_live(&self, key: &str) -> hermes_core::error::Result<Option<String>> {
        self.builtin.read_live(key)
    }
    fn write_live(&self, key: &str, content: &str) -> hermes_core::error::Result<()> {
        self.builtin.write(key, content)
    }
    fn refresh_snapshot(&self) -> hermes_core::error::Result<()> {
        // Need interior mutability — BuiltinMemory::refresh_snapshot takes &mut self
        // Solution: wrap builtin in Arc<Mutex> or use RefCell
        // For now: this is called from the agent loop which has &mut Agent
        // Actually MemoryAccess::refresh_snapshot takes &self, so we need Mutex
        todo!("needs interior mutability refactor")
    }
    async fn on_memory_write(&self, action: &str, target: &str, content: &str) -> hermes_core::error::Result<()> {
        if let Some(ext) = &self.external {
            ext.on_memory_write(action, target, content).await?;
        }
        Ok(())
    }
}
```

**Interior mutability issue**: `BuiltinMemory::refresh_snapshot(&mut self)` but `MemoryAccess::refresh_snapshot(&self)`. Fix: wrap `builtin` in `std::sync::Mutex<BuiltinMemory>` inside MemoryManager. Update all BuiltinMemory accesses to lock first.

- [ ] **Step 2: Create memory_tools.rs**

Two tools: `MemoryReadTool` and `MemoryWriteTool`.

**MemoryReadTool**:
- `name()` → "memory_read", `toolset()` → "memory", `is_read_only()` → true
- Schema: target (required, enum ["memory", "user"])
- `execute()`: get `ctx.memory`, call `read_live(target)`, return content or "no content"

**MemoryWriteTool**:
- `name()` → "memory_write", `toolset()` → "memory", `is_read_only()` → false
- Schema: action (required, enum ["add", "replace", "remove"]), target (required), content (optional), old_text (optional)
- `execute()`:
  - `add`: read current, append with `§` separator, write back
  - `replace`: read current, find old_text, replace with content, write back
  - `remove`: read current, find entry containing old_text, remove it, write back
  - After success: call `on_memory_write(action, target, content)` for external providers

Both registered via inventory.

**6 tests**: add entry, replace, remove, read existing, read missing, char limit

- [ ] **Step 3: Agent loop — pass memory handle and post-write refresh**

In `loop_runner.rs`, when constructing ToolContext:
```rust
memory: Some(Arc::new(self.memory.clone())),  // or Arc from shared handle
```

After tool execution, check if any tool was "memory_write" and succeeded:
```rust
// Check if memory was written — refresh snapshot and rebuild system prompt
let memory_written = tool_results.iter().any(|r| {
    r.tool_name == "memory_write" && !r.result.is_error
});
if memory_written {
    let _ = self.memory.refresh_snapshot();
    let memory_block = self.memory.system_prompt_blocks();
    full_system = if memory_block.is_empty() {
        self.system_prompt.clone()
    } else {
        format!("{}\n\n{}", self.system_prompt, memory_block)
    };
    self.cache_manager.invalidate();
    if self.provider.supports_caching() {
        segments = Some(self.cache_manager.get_or_freeze(&self.system_prompt, &memory_block));
    }
}
```

- [ ] **Step 4: Run tests + commit**

Commit: `feat: implement memory tools with agent loop refresh integration`

---

## Task 4: Web Search Tool

**Files:**
- Create: `crates/hermes-tools/src/web_search.rs`
- Modify: `crates/hermes-tools/src/lib.rs`
- Modify: `crates/hermes-tools/Cargo.toml`

- [ ] **Step 1: Add reqwest to hermes-tools deps**

Add to `crates/hermes-tools/Cargo.toml` [dependencies]:
```toml
reqwest.workspace = true
```

- [ ] **Step 2: Implement web_search.rs**

**WebSearchTool**:
- `name()` → "web_search", `toolset()` → "web", `is_read_only()` → true
- `is_available()` → `std::env::var("TAVILY_API_KEY").is_ok()`
- Schema: query (required string)
- `execute()`:
  1. Get TAVILY_API_KEY from env
  2. Build reqwest client, POST to `https://api.tavily.com/search`
  3. Body: `{ "api_key": key, "query": query, "max_results": 5 }`
  4. Parse response JSON: `results[].{ title, url, content }`
  5. Format as readable text
  6. Return JSON result

Register via inventory.

**4 tests** (mock HTTP not needed — test parsing and formatting):
- `test_web_search_unavailable_without_key` — no env var → is_available() false
- `test_web_search_parse_response` — test the response parsing function with sample JSON
- `test_web_search_format_results` — verify formatted output
- `test_web_search_empty_results` — empty results array

- [ ] **Step 3: Wire up, commit**

Commit: `feat: implement web_search tool with Tavily backend`

---

## Task 5: Web Extract Tool (with SSRF protection)

**Files:**
- Create: `crates/hermes-tools/src/web_extract.rs`
- Modify: `crates/hermes-tools/src/lib.rs`

- [ ] **Step 1: Implement web_extract.rs**

**SSRF validation helper**:
```rust
fn validate_url(url: &str) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        s => return Err(format!("scheme '{s}' not allowed")),
    }
    if let Some(host) = parsed.host_str() {
        if host == "localhost" || host == "169.254.169.254" || host.ends_with(".internal") {
            return Err(format!("blocked host: {host}"));
        }
        // Check for private IPs
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            if ip.is_loopback() || is_private_ip(ip) {
                return Err(format!("private IP blocked: {ip}"));
            }
        }
    }
    Ok(parsed)
}

fn is_private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => v6.is_loopback(),
    }
}
```

**html_to_text helper**:
```rust
fn html_to_text(html: &str) -> String {
    // Remove <script>...</script> and <style>...</style>
    // Strip all HTML tags
    // Decode &amp; &lt; &gt; &quot; &#NNN;
    // Collapse whitespace
}
```

**WebExtractTool**:
- `name()` → "web_extract", `toolset()` → "web", `is_read_only()` → true
- Schema: url (required string)
- `execute()`:
  1. Validate URL (SSRF check)
  2. Build reqwest client with redirect policy (max 5, re-validate each)
  3. GET with 60s timeout
  4. Check content-type: text/html → html_to_text, text/plain → as-is, else error
  5. Truncate to 50,000 chars
  6. Return `{ "url": "...", "title": "...", "content": "..." }`

Register via inventory.

**8 tests**:
- `test_validate_url_http_ok`
- `test_validate_url_blocked_localhost`
- `test_validate_url_blocked_private_ip`
- `test_validate_url_blocked_metadata`
- `test_validate_url_blocked_scheme`
- `test_html_to_text_basic`
- `test_html_to_text_script_removal`
- `test_html_to_text_entity_decode`

- [ ] **Step 2: Wire up, commit**

Commit: `feat: implement web_extract tool with SSRF protection`

---

## Task 6: Vision + Execute Code Tools

Two simpler tools batched together.

**Files:**
- Create: `crates/hermes-tools/src/vision.rs`
- Create: `crates/hermes-tools/src/execute_code.rs`
- Modify: `crates/hermes-tools/src/lib.rs`
- Modify: `crates/hermes-tools/Cargo.toml`

- [ ] **Step 1: Add base64 dep**

Add to root `Cargo.toml` [workspace.dependencies]:
```toml
base64 = "0.22"
```

Add to `crates/hermes-tools/Cargo.toml` [dependencies]:
```toml
base64.workspace = true
```

- [ ] **Step 2: Implement vision.rs**

**VisionTool**:
- `name()` → "vision_analyze", `toolset()` → "vision", `is_read_only()` → true
- Schema: image_path (required), question (required)
- `execute()`:
  1. Check ctx.aux_provider is Some, else error
  2. If URL (starts with http): validate URL (reuse SSRF check), download with reqwest
  3. If local path: resolve via path_utils, check sandbox, read file
  4. Detect MIME from extension (png→image/png, jpg/jpeg→image/jpeg, gif→image/gif, webp→image/webp)
  5. Base64 encode
  6. Build ChatRequest with Content::Parts (image + text question)
  7. Call aux_provider.chat() with no streaming
  8. Return `{ "analysis": response.content }`

Register via inventory. 4 tests (MIME detection, base64 encoding, missing provider error).

- [ ] **Step 3: Implement execute_code.rs**

**ExecuteCodeTool**:
- `name()` → "execute_code", `toolset()` → "code", `is_exclusive()` → true
- `is_available()` → `std::env::var("HERMES_ENABLE_EXECUTE_CODE").is_ok()`
- Schema: code (required string), timeout (optional integer, default 30, max 300)
- `execute()`:
  1. Write code to temp file (`.py` extension)
  2. Send ApprovalRequest (same pattern as terminal tool)
  3. Run `python3 <temp_file>` via tokio::process::Command, cwd = ctx.working_dir
  4. Timeout + output truncation (reuse terminal's truncate_output)
  5. Return `{ "stdout": "...", "stderr": "...", "exit_code": N }`
  6. Cleanup temp file

Register via inventory. 4 tests (disabled by default, simple print, timeout, exit code).

- [ ] **Step 4: Wire up, commit**

Commit: `feat: implement vision_analyze and execute_code tools`

---

## Task 7: Skills System — Skill Struct + SkillManager

**Files:**
- Create: `crates/hermes-skills/src/skill.rs`
- Create: `crates/hermes-skills/src/manager.rs`
- Modify: `crates/hermes-skills/src/lib.rs`
- Modify: `crates/hermes-skills/Cargo.toml`

- [ ] **Step 1: Add deps to hermes-skills**

Verify/add to `crates/hermes-skills/Cargo.toml`:
```toml
[dependencies]
hermes-core.workspace = true
serde.workspace = true
serde_json.workspace = true
serde_yaml_ng.workspace = true
anyhow.workspace = true
walkdir.workspace = true
tracing.workspace = true

[dev-dependencies]
tempfile.workspace = true
```

- [ ] **Step 2: Implement skill.rs**

```rust
use std::path::PathBuf;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub platforms: Vec<String>,
    pub category: Option<String>,
    pub dir: PathBuf,
}

#[derive(Deserialize)]
struct SkillFrontmatter {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    platforms: Vec<String>,
}

impl Skill {
    pub fn from_file(path: &Path, category: Option<String>) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let (frontmatter, body) = parse_frontmatter(&content)?;
        Ok(Self {
            name: frontmatter.name,
            description: frontmatter.description,
            body,
            platforms: frontmatter.platforms,
            category,
            dir: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        })
    }
}

fn parse_frontmatter(content: &str) -> Result<(SkillFrontmatter, String)> {
    // Split on --- delimiters, parse YAML, return body
}
```

5 tests: parse valid, missing description defaults, platform filtering, malformed YAML error, no frontmatter error.

- [ ] **Step 3: Implement manager.rs**

```rust
pub struct SkillManager {
    skills: Vec<Skill>,
    dirs: Vec<PathBuf>,
}

impl SkillManager {
    pub fn new(dirs: Vec<PathBuf>) -> Result<Self>;
    pub fn discover(&mut self) -> Result<()>;  // walk dirs, find SKILL.md files
    pub fn list(&self) -> &[Skill];
    pub fn get(&self, name: &str) -> Option<&Skill>;
    pub fn reload(&mut self) -> Result<()>;
    pub fn match_for_turn(&self, user_message: &str, history: &[Message], max_skills: usize) -> Vec<&Skill>;
    pub fn inject_active_into_history(active: &[&Skill], history: &mut Vec<Message>);
    pub fn primary_dir(&self) -> Option<&Path>;  // first writable dir for mutations
}
```

**match_for_turn**:
1. Filter by current platform
2. Check explicit mentions (skill name in user_message)
3. Score by lexical overlap (word intersection between user_message and skill name+description)
4. Take top `max_skills`, cap total body chars at 12000
5. Return matched skills

**inject_active_into_history**: insert `<skill>` blocks as Message::user at position 0.

8 tests: discover from temp dir, list, get by name, match explicit mention, match lexical overlap, inject format, platform filter, empty dir.

- [ ] **Step 4: Wire up lib.rs**

```rust
pub mod skill;
pub mod manager;

pub use manager::SkillManager;
pub use skill::Skill;
```

- [ ] **Step 5: Commit**

Commit: `feat: implement Skill struct and SkillManager with discovery and matching`

---

## Task 8: Skill Tools

Three tools for listing, viewing, and managing skills.

**Files:**
- Create: `crates/hermes-skills/src/tools.rs`
- Modify: `crates/hermes-skills/src/lib.rs`
- Modify: `crates/hermes-skills/Cargo.toml` (add inventory, async-trait, tokio)

- [ ] **Step 1: Implement SkillAccess on SkillManager**

In `crates/hermes-skills/src/manager.rs`, implement the SkillAccess trait:

```rust
use hermes_core::tool::{SkillAccess, SkillSummary, SkillDoc};
use std::sync::RwLock;

// SkillManager needs to be wrapped in RwLock for SkillAccess (which takes &self)
pub struct SharedSkillManager {
    inner: RwLock<SkillManager>,
}

impl SkillAccess for SharedSkillManager {
    fn list(&self) -> Vec<SkillSummary> { ... }
    fn get(&self, name: &str) -> Option<SkillDoc> { ... }
    fn create(&self, name: &str, content: &str) -> Result<()> {
        // Write SKILL.md to primary_dir/name/SKILL.md
        // Then reload
    }
    fn edit(&self, name: &str, content: &str) -> Result<()> { ... }
    fn delete(&self, name: &str) -> Result<()> { ... }
    fn reload(&self) -> Result<()> { ... }
}
```

- [ ] **Step 2: Implement tools.rs**

Three tools in one file, all registered via inventory:

**SkillListTool**: `name()` → "skill_list", read_only, returns list from ctx.skills
**SkillViewTool**: `name()` → "skill_view", read_only, returns full content
**SkillManageTool**: `name()` → "skill_manage", not read_only, create/edit/delete via ctx.skills

Register all three via inventory. hermes-skills needs inventory dep.

5 tests: list, view existing, view missing, create+list, delete.

- [ ] **Step 3: Wire up + deps**

Add to hermes-skills Cargo.toml:
```toml
inventory.workspace = true
async-trait.workspace = true
tokio.workspace = true
```

Add `pub mod tools;` to lib.rs. Also export `SharedSkillManager`.

Note: hermes-tools registers tools via inventory in its own crate. hermes-skills also registers tools via inventory. For inventory to collect from BOTH crates, hermes-cli must depend on BOTH (it already depends on hermes-tools).

- [ ] **Step 4: Commit**

Commit: `feat: implement skill tools (list, view, manage) with SharedSkillManager`

---

## Task 9: Agent + CLI Integration

Wire everything into the agent loop and CLI. Skill injection into request-local history. Pass memory/provider/skills handles in ToolContext.

**Files:**
- Modify: `crates/hermes-agent/src/loop_runner.rs`
- Modify: `crates/hermes-agent/Cargo.toml`
- Modify: `crates/hermes-cli/src/repl.rs`
- Modify: `crates/hermes-cli/src/oneshot.rs`
- Modify: `crates/hermes-cli/Cargo.toml`

- [ ] **Step 1: Add hermes-skills dep**

Add to `crates/hermes-agent/Cargo.toml`:
```toml
hermes-skills.workspace = true
```

Add to `crates/hermes-cli/Cargo.toml`:
```toml
hermes-skills.workspace = true
```

- [ ] **Step 2: Add skills + ToolContext fields to AgentConfig/Agent**

In loop_runner.rs:

```rust
pub struct AgentConfig {
    // ... existing ...
    pub skills: Option<Arc<dyn SkillAccess>>,
}

pub struct Agent {
    // ... existing ...
    skills: Option<Arc<dyn SkillAccess>>,
}
```

In Agent::new, store `config.skills`.

- [ ] **Step 3: Modify run_conversation for skill injection + full ToolContext**

Skill injection: before each provider call, clone history and inject active skills:

```rust
// Inside the loop, before building ChatRequest:
let mut request_messages = history.clone();
if let Some(ref skills) = self.skills {
    // Simple: inject all skills for now. Matching can use user_message.
    // Actually: use match_for_turn if SkillAccess exposes it
    // For Phase 4: inject skills from the SkillAccess list
    // ... skill injection logic ...
}

let request = ChatRequest {
    system: &full_system,
    system_segments: segments.as_deref(),
    messages: &request_messages,  // use request-local clone
    // ...
};
```

ToolContext construction — pass all handles:
```rust
let ctx = ToolContext {
    // ... existing fields ...
    memory: Some(Arc::new(/* MemoryAccess impl */)),
    aux_provider: Some(Arc::clone(&self.provider)),  // use primary as aux
    skills: self.skills.clone(),
};
```

- [ ] **Step 4: Update CLI (repl.rs + oneshot.rs)**

Construct SharedSkillManager:
```rust
use hermes_skills::{SkillManager, SharedSkillManager};

let skill_dirs = vec![hermes_config::hermes_home().join("skills")];
let skill_manager = Arc::new(SharedSkillManager::new(skill_dirs)?);

// Pass to AgentConfig:
skills: Some(skill_manager as Arc<dyn SkillAccess>),
```

- [ ] **Step 5: Fix all test AgentConfig sites**

Update `make_agent` in loop_runner tests and e2e_test.rs:
```rust
skills: None,
```

- [ ] **Step 6: Run tests + commit**

Commit: `feat: integrate skills and extended ToolContext into agent loop`

---

## Task 10: Full Build Verification

- [ ] **Step 1: Run full checks**

```bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo build --release -p hermes-cli
```

- [ ] **Step 2: Smoke test**

```bash
OPENAI_API_KEY=<key> ./target/release/hermes --message "Use the patch tool to add a comment '# Phase 4 works!' at the top of /tmp/hermes-test.txt, then read it back." --model "openai/gemini-3.1-pro-preview" --base-url "http://34.60.178.0:3000/v1"
```

- [ ] **Step 3: Commit fixes if any**

Commit: `chore: fix Phase 4 build issues`

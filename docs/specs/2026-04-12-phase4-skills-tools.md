# Phase 4: Skills System + Extended Tools — Design Spec

**Date**: 2026-04-12
**Status**: Draft
**Depends on**: Phase 3 (complete)
**Validates**: Plugin registry at scale, skill injection, tool diversity

---

## 1. Scope

### In Scope (Tier 1 Tools + Skills System)
- **patch tool** — find-and-replace file editing (mode=replace only, skip V4A for now)
- **memory tools** — memory_read, memory_write (live memory access with snapshot refresh semantics)
- **web_search tool** — internet search via configurable backend
- **web_extract tool** — web page content extraction with SSRF protection
- **vision_analyze tool** — image analysis via auxiliary provider
- **execute_code tool** — opt-in Python execution, disabled by default, reusing terminal safety controls
- **Skills system** — Skill struct, SkillManager (discovery, matching, request-local injection), skill_list/skill_view/skill_manage tools

### Out of Scope (deferred)
- Browser automation tools (10 sub-tools) — Phase 5
- TTS/transcription/voice — Phase 5+
- MCP integration — Phase 5
- Messaging/delegation tools — Phase 5+
- RL training tools — Phase 6
- V4A patch format — later
- Skills Hub (remote skill marketplace) — later

---

## 2. Patch Tool (File Edit)

Find-and-replace editing — more precise than write_file for targeted changes.

### Schema
```json
{
  "name": "patch",
  "parameters": {
    "type": "object",
    "properties": {
      "path": { "type": "string", "description": "File path" },
      "old_string": { "type": "string", "description": "Text to find" },
      "new_string": { "type": "string", "description": "Replacement text" },
      "replace_all": { "type": "boolean", "default": false }
    },
    "required": ["path", "old_string", "new_string"]
  }
}
```

### Behavior
1. Resolve path, check sandbox (reuse path_utils)
2. Read file content
3. Find `old_string` in content:
   - Exact byte-for-byte match first
   - If not found, try whitespace-aware matching over the original file bytes
   - Split `old_string` into alternating literal runs and whitespace runs
   - Literal runs must match exactly
   - Each whitespace run in `old_string` matches one or more ASCII whitespace chars in the file (` `, `\t`, `\r`, `\n`)
   - The matcher must return concrete byte ranges in the original file; do **not** search a fully normalized copy and then try to map offsets back
   - If still not found, return error with "old_string not found"
4. If multiple concrete matches are found and `replace_all=false`: return error "multiple matches, set replace_all=true"
5. Replace and write back (reuse atomic write pattern from write_file)
6. Return `{ "path": "...", "replacements": N }`

### Toolset: "file", is_read_only: false

---

## 3. Memory Tools

Expose the existing memory system (Phase 3) to the agent as callable tools.

### memory_write Schema
```json
{
  "name": "memory_write",
  "parameters": {
    "type": "object",
    "properties": {
      "action": { "type": "string", "enum": ["add", "replace", "remove"] },
      "target": { "type": "string", "enum": ["memory", "user"] },
      "content": { "type": "string", "description": "New content (for add/replace)" },
      "old_text": { "type": "string", "description": "Text to find (for replace/remove)" }
    },
    "required": ["action", "target"]
  }
}
```

### memory_read Schema
```json
{
  "name": "memory_read",
  "parameters": {
    "type": "object",
    "properties": {
      "target": { "type": "string", "enum": ["memory", "user"] }
    },
    "required": ["target"]
  }
}
```

### Behavior
- **memory_read**: returns current disk state (NOT frozen snapshot)
- **memory_write(add)**: appends entry with `§` separator, enforces char limits, truncates oldest if over
- **memory_write(replace)**: finds `old_text` substring, replaces with `content`
- **memory_write(remove)**: finds entry containing `old_text`, removes it
- Successful writes are best-effort mirrored to external providers via `on_memory_write(...)` when configured
- Successful writes do **not** mutate the already-sent prompt for the current provider request; instead the agent refreshes the frozen snapshot before the **next** provider call

### Access to MemoryManager
Memory tools need more than raw `BuiltinMemory`: they must read live disk state, write through the builtin store, and coordinate snapshot refresh semantics with the agent loop.

**Solution**: Add a `MemoryAccess` handle to `ToolContext`. The implementation is backed by `MemoryManager` (or a thin wrapper around it), not by `BuiltinMemory` alone.

```rust
// hermes-core/src/tool.rs
#[async_trait]
pub trait MemoryAccess: Send + Sync {
    fn read_live(&self, key: &str) -> Result<Option<String>>;
    fn write_live(&self, key: &str, content: &str) -> Result<()>;
    fn refresh_snapshot(&self) -> Result<()>;
    async fn on_memory_write(&self, action: &str, target: &str, content: &str) -> Result<()>;
}

// Add to ToolContext:
pub memory: Option<Arc<dyn MemoryAccess>>,
```

### Agent Loop Requirement
After any successful `memory_write` tool call:
1. Refresh the builtin memory snapshot
2. Rebuild `full_system` from `memory.system_prompt_blocks()`
3. Invalidate prompt cache segments and rebuild them if caching is enabled

This makes updated memory visible on subsequent model turns in the same conversation loop and on future user turns.

### Toolset: "memory", is_read_only: false (write), true (read)

---

## 4. Web Search Tool

Internet search via configurable backend API.

### Schema
```json
{
  "name": "web_search",
  "parameters": {
    "type": "object",
    "properties": {
      "query": { "type": "string", "description": "Search query" }
    },
    "required": ["query"]
  }
}
```

### Backend Selection
Phase 4 supports one backend: **Tavily** (simplest API, good results).

Env var: `TAVILY_API_KEY`. If not set, tool reports unavailable (`is_available() = false`).

### Behavior
1. POST to `https://api.tavily.com/search` with `{ "query": query, "max_results": 5 }`
2. Parse response: `results[].{ title, url, content }`
3. Format as readable text with URLs
4. Return `{ "results": [...], "query": "..." }`

### Config
```rust
// In AppConfig, later:
pub struct WebConfig {
    pub search_backend: String,  // "tavily" | "firecrawl" | etc.
    pub tavily_api_key: Option<String>,  // from env
}
```

Phase 4: just check `TAVILY_API_KEY` env var directly.

### Toolset: "web", is_read_only: true

---

## 5. Web Extract Tool

Extract text content from web pages.

### Schema
```json
{
  "name": "web_extract",
  "parameters": {
    "type": "object",
    "properties": {
      "url": { "type": "string", "description": "URL to extract content from" }
    },
    "required": ["url"]
  }
}
```

### Behavior
1. Validate URL: only `http` and `https` are allowed
2. Resolve the host and reject loopback, private, link-local, multicast, and unspecified IP ranges
3. Reject `localhost` and common metadata targets
4. Follow redirects with a max of 5 hops; re-run the same validation on every redirect target
5. Fetch URL with reqwest (60s timeout)
6. If `text/html`: strip tags, extract text content (basic HTML-to-text)
7. If `text/plain`: return body as-is
8. Otherwise return "unsupported content type"
9. Truncate to 50,000 chars
10. Return `{ "url": "...", "title": "...", "content": "..." }`

### HTML-to-Text
Simple approach: regex strip tags + decode entities. No full DOM parsing needed for Phase 4.

```rust
fn html_to_text(html: &str) -> String {
    // Remove script/style blocks
    // Strip HTML tags
    // Decode common entities (&amp; &lt; &gt; &quot;)
    // Collapse whitespace
}
```

### Toolset: "web", is_read_only: true

---

## 6. Vision Analyze Tool

Image analysis using an auxiliary LLM provider.

### Schema
```json
{
  "name": "vision_analyze",
  "parameters": {
    "type": "object",
    "properties": {
      "image_path": { "type": "string", "description": "Local file path or HTTP URL" },
      "question": { "type": "string", "description": "What to analyze" }
    },
    "required": ["image_path", "question"]
  }
}
```

### Behavior
1. If HTTP URL: download to temp file (60s timeout, SSRF check)
2. If local path: resolve via path_utils, check sandbox
3. Read file, base64 encode
4. Detect MIME type from extension (png, jpg, gif, webp)
5. Build message with `Content::Parts` containing image + question
6. Call provider.chat() with the image message
7. Return `{ "analysis": "..." }`

### Provider Access
Vision tool needs access to a Provider. Add `Option<Arc<dyn Provider>>` to ToolContext for auxiliary model access.

```rust
// hermes-core/src/tool.rs — add to ToolContext:
pub aux_provider: Option<Arc<dyn Provider>>,
```

Phase 4: use the primary provider for vision (most modern models support it).

### Toolset: "vision", is_read_only: true

---

## 7. Execute Code Tool

Run Python code in a tightly-scoped, opt-in mode. This is intentionally **not** a general sandbox in Phase 4.

### Schema
```json
{
  "name": "execute_code",
  "parameters": {
    "type": "object",
    "properties": {
      "code": { "type": "string", "description": "Python code to execute" },
      "timeout": { "type": "integer", "default": 30, "maximum": 300 }
    },
    "required": ["code"]
  }
}
```

### Availability / Safety
- Disabled by default in Phase 4
- Enabled only when `HERMES_ENABLE_EXECUTE_CODE=1` is present in the environment
- Reuses the same approval flow as `terminal`
- Reuses terminal timeout and output-cap settings from `ToolConfig`
- Runs with `cwd` pinned to the agent working directory
- Not a security boundary: if stronger isolation is required, defer to a future sandboxed execution design

### Phase 4 Simplification
- Execute Python via `python3 <temp_file>` subprocess (no UDS/RPC)
- No tool stubs in Phase 4
- Stdout/stderr capture, timeout, exit code
- Essentially a specialized terminal tool restricted to Python
- Exclusive: never parallelize with other tools

### Behavior
1. If `HERMES_ENABLE_EXECUTE_CODE` is not set, tool is unavailable (`is_available() = false`)
2. Write code to a temp file
3. Issue an approval request using the same mechanism as `terminal`
4. Run `python3 <temp_file>` via `tokio::process::Command` with `cwd = ctx.working_dir`
5. Apply timeout and output truncation using terminal tool config
6. Return `{ "stdout": "...", "stderr": "...", "exit_code": N }`

### Toolset: "code", is_exclusive: true

---

## 8. Skills System

### 8.1 Skill Struct

```rust
// hermes-skills/src/skill.rs
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,           // markdown content after frontmatter
    pub platforms: Vec<String>, // empty = all platforms
    pub category: Option<String>,
    pub dir: PathBuf,           // directory containing SKILL.md
}
```

### 8.2 SKILL.md Format

```markdown
---
name: skill-name
description: One-line description
platforms: [linux, macos]
---

# Skill Title

Instructions body...
```

Parse with `serde_yaml_ng` for frontmatter extraction. Split on first `---`...`---` boundary.

### 8.3 SkillManager

```rust
// hermes-skills/src/manager.rs
pub struct SkillManager {
    skills: Vec<Skill>,
    dirs: Vec<PathBuf>,
}

impl SkillManager {
    pub fn new(dirs: Vec<PathBuf>) -> Result<Self>;  // discover from dirs
    pub fn discover(&mut self) -> Result<()>;         // scan dirs for SKILL.md files
    pub fn list(&self) -> &[Skill];                   // all skills
    pub fn get(&self, name: &str) -> Option<&Skill>;  // by name
    pub fn reload(&mut self) -> Result<()>;           // re-scan dirs
    pub fn match_for_turn(&self, user_message: &str, history: &[Message], max_skills: usize) -> Vec<Skill>;
    pub fn inject_active_into_history(&self, active: &[Skill], history: &mut Vec<Message>);
}
```

### 8.4 Skill Matching

Phase 4 uses a deterministic, lightweight matcher:
- Filter out skills whose `platforms` do not include the current OS
- Explicit mention wins: `$skill-name`, `use skill-name`, or exact skill name mention in the user turn
- Otherwise score by lexical overlap between the current user message and the skill's `name` + `description`
- Limit to the top 3 matched skills
- Cap total injected skill body size to ~12k chars; keep explicit matches first, then highest-score matches
- If nothing matches, inject nothing

No "inject all skills" behavior in Phase 4.

### 8.5 Skill Injection

Matched skills are injected into a **request-local clone** of history, not the persisted conversation history and not the session store.

This means:
- Skills affect the current model request only
- Synthetic skill messages are not stored in `history`
- Skill injection does **not** currently benefit provider prompt caching, because the cache today only covers `system` / `system_segments`

Injection format:

```rust
pub fn inject_active_into_history(&self, active: &[Skill], history: &mut Vec<Message>) {
    if active.is_empty() { return; }
    let combined = active.iter()
        .map(|s| format!("<skill name=\"{}\">\n{}\n</skill>", s.name, s.body))
        .collect::<Vec<_>>()
        .join("\n\n");
    history.insert(0, Message::user(&format!("[Active skills for this turn]\n\n{combined}")));
}
```

### 8.6 Skill Tools

Three tools registered via inventory:

**skill_list**: List all skills with name + description. Toolset: "skills", read_only: true.

**skill_view**: View full SKILL.md content by name. Toolset: "skills", read_only: true.

**skill_manage**: Create/edit/delete skills. Toolset: "skills", read_only: false.
- Operates on the shared `SkillManager`, not on a hard-coded path independent of the active manager
- Uses the first configured writable skill directory as the mutation target
- Calls `reload()` after every successful mutation so the current session sees the new state on the next turn
- `create(name, content)` — creates `<primary_skill_dir>/{name}/SKILL.md`
- `edit(name, content)` — replaces SKILL.md
- `delete(name)` — removes skill directory

### 8.7 Agent Integration

SkillManager is constructed in the CLI and passed to AgentConfig as a shared handle. The agent performs matching once per user turn and injects only the active skills into a request-local message vector before each provider call.

```rust
pub struct AgentConfig {
    // ... existing fields ...
    pub skills: Option<Arc<tokio::sync::RwLock<hermes_skills::SkillManager>>>,
}
```

High-level flow per user turn:
1. User message is appended to persisted `history`
2. Agent computes `active_skills = skill_manager.match_for_turn(user_message, history, 3)`
3. Before each provider call, agent clones `history` into `request_history`
4. Agent injects `active_skills` into `request_history`
5. Provider sees `request_history`, but persisted `history` remains skill-free

To avoid a `hermes-core -> hermes-skills` dependency, skill tools access the shared manager via a trait in `hermes-core`:

```rust
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

pub struct SkillDoc {
    pub name: String,
    pub description: String,
    pub body: String,
}

pub trait SkillAccess: Send + Sync {
    fn list(&self) -> Vec<SkillSummary>;
    fn get(&self, name: &str) -> Option<SkillDoc>;
    fn match_for_turn(&self, user_message: &str, history: &[Message], max_skills: usize) -> Vec<SkillDoc>;
    fn create(&self, name: &str, content: &str) -> Result<()>;
    fn edit(&self, name: &str, content: &str) -> Result<()>;
    fn delete(&self, name: &str) -> Result<()>;
    fn reload(&self) -> Result<()>;
}
```

---

## 9. ToolContext Expansion

Phase 4 adds three optional fields to ToolContext:

```rust
#[derive(Clone)]
pub struct ToolContext {
    // ... existing fields (session_id, working_dir, approval_tx, delta_tx, tool_config) ...
    pub memory: Option<Arc<dyn MemoryAccess>>,    // for memory tools
    pub aux_provider: Option<Arc<dyn Provider>>,   // for vision tool
    pub skills: Option<Arc<dyn SkillAccess>>,      // for skill tools
}
```

All three are `Option` so existing tools don't need them. New tools check and use them when available.

---

## 10. File Structure

### New files
```
crates/hermes-tools/src/patch.rs           # Patch tool (find-and-replace)
crates/hermes-tools/src/memory_tools.rs    # memory_read + memory_write
crates/hermes-tools/src/web_search.rs      # Web search (Tavily)
crates/hermes-tools/src/web_extract.rs     # Web content extraction
crates/hermes-tools/src/vision.rs          # Vision analyze
crates/hermes-tools/src/execute_code.rs    # Python code execution
crates/hermes-skills/src/skill.rs          # Skill struct + YAML parsing
crates/hermes-skills/src/manager.rs        # SkillManager
crates/hermes-skills/src/tools.rs          # skill_list, skill_view, skill_manage tools
```

### Modified files
```
crates/hermes-core/src/tool.rs             # ToolContext: add memory + aux_provider + skills access
crates/hermes-tools/src/lib.rs             # wire new tool modules
crates/hermes-tools/Cargo.toml             # add reqwest for web tools
crates/hermes-skills/src/lib.rs            # wire modules
crates/hermes-skills/Cargo.toml            # add deps
crates/hermes-agent/src/loop_runner.rs     # pass memory/provider/skills to ToolContext, refresh memory after memory_write, inject request-local skills
crates/hermes-cli/src/repl.rs              # construct shared SkillManager
crates/hermes-cli/src/oneshot.rs           # same
```

---

## 11. Testing Strategy

| Component | Test Type | Key Scenarios |
|-----------|-----------|---------------|
| Patch | Unit | exact match, whitespace-aware fallback, concrete-range mapping, multiple matches, no match, replace_all |
| memory_write | Unit | add entry, replace, remove, char limit, truncation, external mirror hook |
| memory_read | Unit | read existing, read missing |
| web_search | Unit | mock HTTP response parsing |
| web_extract | Unit | HTML-to-text stripping, truncation, private-IP rejection, redirect re-validation |
| vision | Unit | MIME detection, base64 encoding |
| execute_code | Integration | disabled-by-default gating, approval request, simple print, timeout, output truncation |
| Skill parsing | Unit | frontmatter extraction, missing fields, platform filtering |
| SkillManager | Unit | discovery from temp dir, list, get by name, explicit match, lexical match, size cap, inject_active_into_history |
| skill_manage | Integration | create/edit/delete in temp dir through shared manager, reload visible next turn |
| Agent loop + memory | Integration | successful memory_write refreshes snapshot, rebuilds system prompt, invalidates cache |
| ToolContext expansion | Unit | existing tools still work with None memory/provider/skills |

# Phase 4: Skills System + Extended Tools — Design Spec

**Date**: 2026-04-12
**Status**: Draft
**Depends on**: Phase 3 (complete)
**Validates**: Plugin registry at scale, skill injection, tool diversity

---

## 1. Scope

### In Scope (Tier 1 Tools + Skills System)
- **patch tool** — find-and-replace file editing (mode=replace only, skip V4A for now)
- **memory tools** — memory_read, memory_write (expose BuiltinMemory to agent via tools)
- **web_search tool** — internet search via configurable backend
- **web_extract tool** — web page content extraction
- **vision_analyze tool** — image analysis via auxiliary provider
- **execute_code tool** — Python code execution with tool stubs (simplified)
- **Skills system** — Skill struct, SkillManager (discovery, matching, injection), skill_list/skill_view/skill_manage tools

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
   - Exact match first
   - If not found, try whitespace-normalized match (collapse runs of whitespace)
   - If still not found, return error with "old_string not found"
4. If multiple matches and `replace_all=false`: return error "multiple matches, set replace_all=true"
5. Replace and write back (reuse atomic write pattern from write_file)
6. Return `{ "path": "...", "replacements": N }`

### Toolset: "file", is_read_only: false

---

## 3. Memory Tools

Expose the existing BuiltinMemory (Phase 3) to the agent as callable tools.

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
- **memory_read**: calls `BuiltinMemory::read_live(target)` — returns current disk state (NOT frozen snapshot)
- **memory_write(add)**: appends entry with § separator, enforces char limits, truncates oldest if over
- **memory_write(replace)**: finds `old_text` substring, replaces with `content`
- **memory_write(remove)**: finds entry containing `old_text`, removes it

### Access to BuiltinMemory
Memory tools need access to the BuiltinMemory instance. Since BuiltinMemory is inside MemoryManager which is inside Agent, the tools can't directly access it.

**Solution**: Add `Arc<BuiltinMemory>` to `ToolContext` (or a new `MemoryHandle` wrapper). When constructing ToolContext in the agent loop, provide a reference to the builtin memory.

```rust
// hermes-core/src/tool.rs — add to ToolContext:
pub memory: Option<Arc<dyn MemoryAccess>>,

// hermes-core/src/tool.rs — new trait:
pub trait MemoryAccess: Send + Sync {
    fn read_live(&self, key: &str) -> Result<Option<String>>;
    fn write(&self, key: &str, content: &str) -> Result<()>;
}
```

BuiltinMemory implements MemoryAccess.

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
1. Fetch URL with reqwest (follow redirects, 60s timeout)
2. If HTML: strip tags, extract text content (basic HTML-to-text)
3. Truncate to 50,000 chars
4. Return `{ "url": "...", "title": "...", "content": "..." }`

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

Run Python code with access to tool stubs. Simplified for Phase 4.

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

### Phase 4 Simplification
- Execute Python via `python3 -c <code>` subprocess (no UDS/RPC)
- No tool stubs in Phase 4 (just raw Python execution)
- Stdout/stderr capture, timeout, exit code
- Essentially a specialized terminal tool restricted to Python

### Behavior
1. Write code to temp file
2. Run `python3 <temp_file>` via tokio::process::Command
3. Capture stdout/stderr with timeout
4. Return `{ "stdout": "...", "stderr": "...", "exit_code": N }`

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
    pub fn inject_into_history(&self, history: &mut Vec<Message>);  // add skills as user message
}
```

### 8.4 Skill Injection

Skills are injected as a user message at `history[0]` (before the actual user message) to preserve prompt cache:

```rust
pub fn inject_into_history(&self, history: &mut Vec<Message>) {
    if self.skills.is_empty() { return; }
    let combined = self.skills.iter()
        .map(|s| format!("<skill name=\"{}\">\n{}\n</skill>", s.name, s.body))
        .collect::<Vec<_>>()
        .join("\n\n");
    history.insert(0, Message::user(&format!("[Active skills]\n\n{combined}")));
}
```

### 8.5 Skill Tools

Three tools registered via inventory:

**skill_list**: List all skills with name + description. Toolset: "skills", read_only: true.

**skill_view**: View full SKILL.md content by name. Toolset: "skills", read_only: true.

**skill_manage**: Create/edit/delete skills. Toolset: "skills", read_only: false.
- `create(name, content)` — creates `~/.hermes/skills/{name}/SKILL.md`
- `edit(name, content)` — replaces SKILL.md
- `delete(name)` — removes skill directory

### 8.6 Agent Integration

SkillManager is constructed in the CLI, passed to AgentConfig, and skills are injected before the first LLM call.

```rust
pub struct AgentConfig {
    // ... existing fields ...
    pub skills: Option<hermes_skills::SkillManager>,
}
```

---

## 9. ToolContext Expansion

Phase 4 adds two optional fields to ToolContext:

```rust
#[derive(Clone)]
pub struct ToolContext {
    // ... existing fields (session_id, working_dir, approval_tx, delta_tx, tool_config) ...
    pub memory: Option<Arc<dyn MemoryAccess>>,    // for memory tools
    pub aux_provider: Option<Arc<dyn Provider>>,   // for vision tool
}
```

Both are `Option` so existing tools don't need them. New tools check and use them when available.

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
crates/hermes-core/src/tool.rs             # ToolContext: add memory + aux_provider fields
crates/hermes-tools/src/lib.rs             # wire new tool modules
crates/hermes-tools/Cargo.toml             # add reqwest for web tools
crates/hermes-skills/src/lib.rs            # wire modules
crates/hermes-skills/Cargo.toml            # add deps
crates/hermes-agent/src/loop_runner.rs     # pass memory/provider to ToolContext, inject skills
crates/hermes-cli/src/repl.rs              # construct SkillManager
crates/hermes-cli/src/oneshot.rs           # same
```

---

## 11. Testing Strategy

| Component | Test Type | Key Scenarios |
|-----------|-----------|---------------|
| Patch | Unit | exact match, whitespace-normalized, multiple matches, no match, replace_all |
| memory_write | Unit | add entry, replace, remove, char limit, truncation |
| memory_read | Unit | read existing, read missing |
| web_search | Unit | mock HTTP response parsing |
| web_extract | Unit | HTML-to-text stripping, truncation |
| vision | Unit | MIME detection, base64 encoding |
| execute_code | Integration | simple print, timeout, exit code |
| Skill parsing | Unit | frontmatter extraction, missing fields, platform filtering |
| SkillManager | Unit | discovery from temp dir, list, get by name, inject_into_history |
| skill_manage | Integration | create/edit/delete in temp dir |
| ToolContext expansion | Unit | existing tools still work with None memory/provider |

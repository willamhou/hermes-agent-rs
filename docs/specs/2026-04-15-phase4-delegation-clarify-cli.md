# Phase 4: Delegation, Clarify & CLI Commands — Design Spec

**Date**: 2026-04-15
**Status**: Draft
**Depends on**: Phase 3 (complete)
**Validates**: Subagent isolation, interactive UI, command dispatch

---

## 1. Scope

### In Scope
- **Delegation**: Subagent spawning with isolated context, restricted toolsets, independent budget
- **Clarify tool**: Interactive multiple-choice / open-ended question tool with timeout
- **CLI commands**: 13 slash commands with registry, prefix matching, aliases

### Out of Scope
- Batch/parallel delegation (single task only for Phase 4)
- ACP transport override (delegate always uses parent's provider)
- Gateway-specific clarify UI (CLI only)
- 30+ remaining slash commands
- Cron job executor (only /cron list for now)

---

## 2. Delegation System

### Tool Schema

```json
{
  "name": "delegate_task",
  "description": "Spawn a focused subagent for a specific task. The subagent gets its own context and budget.",
  "parameters": {
    "type": "object",
    "properties": {
      "goal": { "type": "string", "description": "Clear, specific task description" },
      "context": { "type": "string", "description": "Background info (files, errors, constraints)" },
      "toolsets": {
        "type": "array", "items": { "type": "string" },
        "description": "Tool categories to enable (e.g., ['terminal', 'file']). Default: all except blocked."
      }
    },
    "required": ["goal"]
  }
}
```

### Child Agent Creation

```rust
pub struct DelegationTool;

impl DelegationTool {
    async fn spawn_child(
        goal: &str,
        context: Option<&str>,
        toolsets: Option<&[String]>,
        parent_ctx: &ToolContext,
        parent_provider: &Arc<dyn Provider>,
        parent_registry: &Arc<ToolRegistry>,
    ) -> Result<DelegationResult> {
        // 1. Build restricted registry
        // 2. Build child system prompt
        // 3. Create child Agent with fresh budget
        // 4. Run conversation
        // 5. Return summary
    }
}
```

**Isolated context**:
- Fresh conversation history (empty)
- Own session_id (UUID)
- Independent IterationBudget (default 50, from config `delegation.max_iterations`)
- Shares parent's Provider (Arc clone — stateless)
- Shares parent's working_dir and workspace_root

**Blocked tools** (hardcoded):
```rust
const DELEGATION_BLOCKED_TOOLS: &[&str] = &[
    "delegate_task",  // no recursive delegation
    "clarify",        // no user interaction
];
```

**Toolset restriction**: If `toolsets` provided, child registry = parent tools filtered to those toolsets. Blocked tools always removed regardless.

**Depth limit**: MAX_DEPTH = 1 (no grandchildren). Track depth via a field in ToolContext.

**System prompt for child**:
```
You are a focused AI assistant working on a specific task.

## Task
{goal}

## Context
{context}

## Working Directory
{working_dir}

Complete the task efficiently. Be concise in your response.
```

### Result Format

```rust
#[derive(Debug, Serialize)]
pub struct DelegationResult {
    pub status: String,        // "completed" | "failed" | "budget_exhausted"
    pub summary: String,       // child's final response
    pub iterations_used: u32,
    pub duration_ms: u64,
}
```

The parent receives ONLY the result — no intermediate tool calls or messages.

### ToolContext Changes

Add `delegation_depth: u32` to ToolContext:
```rust
pub struct ToolContext {
    // ... existing fields ...
    pub delegation_depth: u32,  // 0 for parent, 1 for child
}
```

### Implementation Location

`crates/hermes-tools/src/delegate.rs` — the tool itself.

The tool needs access to Provider and ToolRegistry, which are not in ToolContext. Two options:
- **A**: Add `Arc<dyn Provider>` and `Arc<ToolRegistry>` to ToolContext
- **B**: Store them in the DelegationTool struct at registration time

**Choice: A** — Add to ToolContext. It's cleaner because other tools might need provider access in the future (e.g., for sub-queries). Add:
```rust
pub struct ToolContext {
    // ... existing fields ...
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<ToolRegistry>,
    pub delegation_depth: u32,
}
```

This changes the ToolContext API — all construction sites need updating (like Phase 3 changes).

---

## 3. Clarify Tool

### Tool Schema

```json
{
  "name": "clarify",
  "description": "Ask the user a question. Use when you need clarification before proceeding.",
  "parameters": {
    "type": "object",
    "properties": {
      "question": { "type": "string", "description": "The question to ask" },
      "choices": {
        "type": "array", "items": { "type": "string" },
        "description": "Up to 4 predefined choices. Omit for open-ended."
      }
    },
    "required": ["question"]
  }
}
```

### Communication Architecture

The clarify tool needs to send a question to the CLI and wait for an answer. Use the same channel pattern as approval:

```rust
pub struct ClarifyRequest {
    pub question: String,
    pub choices: Vec<String>,
    pub response_tx: oneshot::Sender<ClarifyResponse>,
}

pub enum ClarifyResponse {
    Answer(String),
    Timeout,
}
```

Add `clarify_tx: mpsc::Sender<ClarifyRequest>` to ToolContext. The CLI spawns a handler that renders the UI and sends back the response.

### CLI Rendering

When a ClarifyRequest arrives:

**Multiple-choice mode** (choices non-empty):
```
╭─ Clarify ────────────────────────────╮
│ What language should I use?          │
│                                      │
│  1) Python                           │
│  2) Rust                             │
│  3) Go                               │
│  4) Other (type your answer)         │
╰──────────────────────────────────────╯
> 
```

**Open-ended mode** (no choices):
```
╭─ Clarify ────────────────────────────╮
│ What's the target directory?         │
╰──────────────────────────────────────╯
> 
```

**Timeout**: 120 seconds. If no response, send `ClarifyResponse::Timeout`. The tool returns:
```json
{"question": "...", "response": null, "timed_out": true}
```

### Implementation

- Tool: `crates/hermes-tools/src/clarify.rs`
- CLI handler: in `crates/hermes-cli/src/repl.rs` — spawn a task that receives ClarifyRequests and interacts with the readline thread
- Blocked in delegation: child agents cannot use clarify

---

## 4. CLI Command System

### Command Registry

```rust
// crates/hermes-cli/src/commands.rs
pub struct CommandDef {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub usage: &'static str,
}

pub const COMMANDS: &[CommandDef] = &[
    CommandDef { name: "help",     aliases: &["h", "?"],  description: "Show available commands",       usage: "/help" },
    CommandDef { name: "quit",     aliases: &["q", "exit"], description: "Exit Hermes",                usage: "/quit" },
    CommandDef { name: "new",      aliases: &["reset"],    description: "Start new conversation",       usage: "/new" },
    CommandDef { name: "clear",    aliases: &[],           description: "Clear terminal screen",        usage: "/clear" },
    CommandDef { name: "model",    aliases: &["m"],        description: "Switch model",                 usage: "/model [provider/model]" },
    CommandDef { name: "tools",    aliases: &["t"],        description: "List or toggle tools",         usage: "/tools [list|enable|disable] [name]" },
    CommandDef { name: "status",   aliases: &[],           description: "Show session info",            usage: "/status" },
    CommandDef { name: "retry",    aliases: &[],           description: "Re-run last user message",     usage: "/retry" },
    CommandDef { name: "undo",     aliases: &[],           description: "Remove last turn from history", usage: "/undo" },
    CommandDef { name: "compress", aliases: &[],           description: "Manually trigger compression", usage: "/compress" },
    CommandDef { name: "skills",   aliases: &[],           description: "List available skills",        usage: "/skills [list|reload]" },
    CommandDef { name: "save",     aliases: &[],           description: "Save conversation to file",    usage: "/save [path]" },
    CommandDef { name: "cron",     aliases: &[],           description: "List scheduled jobs",          usage: "/cron list" },
];
```

### Dispatch

```rust
pub fn resolve_command(input: &str) -> Option<&'static CommandDef> {
    let word = input.split_whitespace().next()?.strip_prefix('/')?;
    // Exact match on name or alias
    if let Some(cmd) = COMMANDS.iter().find(|c| c.name == word || c.aliases.contains(&word)) {
        return Some(cmd);
    }
    // Prefix match (unambiguous only)
    let matches: Vec<_> = COMMANDS.iter().filter(|c| c.name.starts_with(word)).collect();
    if matches.len() == 1 { Some(matches[0]) } else { None }
}
```

### Command Implementations

| Command | Handler | Complexity |
|---------|---------|------------|
| `/help` | Print formatted command table | Simple |
| `/quit` | Break loop | Already exists |
| `/new` | Clear history, end session, start new | Already exists |
| `/clear` | `crossterm::execute!(stdout, Clear(All))` | Simple |
| `/model` | Parse arg, rebuild provider, print confirmation | Medium |
| `/tools` | List from registry, enable/disable via config | Medium |
| `/status` | Print session_id, model, message count, token estimate | Simple |
| `/retry` | Pop last user+assistant from history, re-send user message | Medium |
| `/undo` | Pop last turn (user + assistant + tool results) | Medium |
| `/compress` | Call `compressor.compress()` directly | Medium |
| `/skills` | List skills from SkillManager, optionally reload | Simple |
| `/save` | Serialize history to file (JSON or markdown) | Simple |
| `/cron` | List cron jobs from config (read-only for Phase 4) | Simple |

---

## 5. File Structure

### New files
```
crates/hermes-core/src/clarify.rs          # ClarifyRequest, ClarifyResponse
crates/hermes-tools/src/delegate.rs        # DelegationTool
crates/hermes-tools/src/clarify.rs         # ClarifyTool
crates/hermes-cli/src/commands.rs          # CommandDef registry + dispatch
crates/hermes-cli/src/handlers.rs          # Command handler implementations
```

### Modified files
```
crates/hermes-core/src/tool.rs             # ToolContext: add provider, registry, delegation_depth, clarify_tx
crates/hermes-core/src/lib.rs              # add pub mod clarify
crates/hermes-agent/src/loop_runner.rs     # pass provider+registry into ToolContext
crates/hermes-tools/src/lib.rs             # add delegate, clarify modules
crates/hermes-cli/src/main.rs              # (minor: no changes needed if commands handled in repl)
crates/hermes-cli/src/repl.rs              # command dispatch, clarify handler, model switching
```

---

## 6. ToolContext Changes Summary

```rust
#[derive(Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub working_dir: PathBuf,
    pub approval_tx: mpsc::Sender<ApprovalRequest>,
    pub delta_tx: mpsc::Sender<StreamDelta>,
    pub tool_config: Arc<ToolConfig>,
    // NEW in Phase 4:
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<ToolRegistry>,
    pub delegation_depth: u32,
    pub clarify_tx: mpsc::Sender<ClarifyRequest>,
}
```

This is a larger API change than previous phases. Every ToolContext construction site needs 4 new fields. However, this is the same pattern as Phase 3 (memory field added to AgentConfig) — mechanical but necessary.

---

## 7. Testing Strategy

| Component | Tests |
|-----------|-------|
| DelegationTool | spawn child with MockProvider, verify isolation (fresh history), verify blocked tools filtered, verify depth limit enforced, verify result format |
| ClarifyTool | send request via channel, verify question/choices transmitted, verify timeout handling, verify blocked in delegation |
| Command registry | resolve by name, resolve by alias, resolve by prefix, ambiguous prefix returns None |
| Command handlers | /status output format, /retry pops and re-sends, /undo removes last turn, /compress triggers compression |
| ToolContext | verify all new fields propagated through agent loop |

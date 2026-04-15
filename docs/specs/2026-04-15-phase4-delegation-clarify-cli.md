# Phase 4: Delegation, Clarify & CLI Commands — Design Spec (v2)

**Date**: 2026-04-15
**Status**: Revised after code review against actual codebase
**Depends on**: Phase 3 + MCP/Skills/Browser work (all on main)

---

## 1. Scope

### In Scope
- **Delegation**: Subagent spawning with isolated context, restricted toolsets, independent budget
- **Clarify tool**: Interactive multiple-choice / open-ended questions with timeout
- **CLI commands**: 13 slash commands with registry, prefix matching, aliases

### Out of Scope
- Batch/parallel delegation (single task only)
- ACP transport override
- Gateway-specific clarify UI
- Full /model switching (read-only display for Phase 4)
- Cron job executor (only /cron list placeholder)

---

## 2. Review-Driven Changes (v1 → v2)

| Issue | v1 | v2 |
|-------|-----|-----|
| Provider in ToolContext | Add new `provider` field | **Use existing `aux_provider`** — already `Arc<dyn Provider>` |
| Registry access | Add `registry` to ToolContext | Add `registry: Arc<ToolRegistry>` to ToolContext |
| /model command | Full provider rebuild | **Read-only**: show current model, suggest --model flag |
| Clarify blocking | Not addressed | Mark `is_exclusive() = true`, document async safety |
| delegation_depth | New field in ToolContext | Keep — add `delegation_depth: u32` |
| clarify_tx | New field in ToolContext | Keep — add `clarify_tx: Option<mpsc::Sender<ClarifyRequest>>` (Option because delegation children don't get one) |

---

## 3. Current ToolContext (actual code)

```rust
pub struct ToolContext {
    pub session_id: String,
    pub working_dir: PathBuf,
    pub approval_tx: mpsc::Sender<ApprovalRequest>,
    pub delta_tx: mpsc::Sender<StreamDelta>,
    pub tool_config: Arc<ToolConfig>,
    pub memory: Option<Arc<dyn MemoryAccess>>,
    pub aux_provider: Option<Arc<dyn Provider>>,
    pub skills: Option<Arc<dyn SkillAccess>>,
}
```

### Phase 4 additions (3 new fields):
```rust
pub struct ToolContext {
    // ... existing 8 fields ...
    pub registry: Arc<ToolRegistry>,                         // NEW: for delegation filtering
    pub delegation_depth: u32,                               // NEW: 0=parent, 1=child, max=1
    pub clarify_tx: Option<mpsc::Sender<ClarifyRequest>>,   // NEW: None in delegation children
}
```

---

## 4. Delegation System

### Tool Schema
```json
{
  "name": "delegate_task",
  "description": "Spawn a focused subagent for a specific task.",
  "parameters": {
    "type": "object",
    "properties": {
      "goal": { "type": "string" },
      "context": { "type": "string" },
      "toolsets": { "type": "array", "items": { "type": "string" } }
    },
    "required": ["goal"]
  }
}
```

### Implementation: `crates/hermes-tools/src/delegate.rs`

Uses `ctx.aux_provider` (the parent's provider) and `ctx.registry` (parent's tool registry).

**Child construction:**
1. Filter parent's registry: if `toolsets` given, keep only matching; always remove `BLOCKED_TOOLS`
2. Build child system prompt from goal + context + working_dir
3. Create child Agent with:
   - provider: `ctx.aux_provider.clone()` (Arc clone)
   - registry: filtered copy
   - budget: 50 iterations (configurable)
   - memory: parent's MemoryManager.new_child()
   - Fresh ToolContext with `delegation_depth = parent_depth + 1`
   - `clarify_tx: None` (children can't ask users)
4. Call `child.run_conversation(goal, &mut Vec::new(), delta_tx)`
5. Return DelegationResult { status, summary, iterations_used, duration_ms }

**Blocked tools:**
```rust
const BLOCKED: &[&str] = &["delegate_task", "clarify"];
```

**Depth limit:** If `ctx.delegation_depth >= 1`, return error "delegation depth limit reached".

### hermes-agent dependency

DelegationTool needs to construct an `Agent`. This means hermes-tools depends on hermes-agent — but hermes-agent already depends on hermes-tools (circular!).

**Solution:** Move DelegationTool to hermes-agent crate (not hermes-tools). It's the only tool that needs Agent. Register it from hermes-agent or hermes-cli.

Actually better: **Don't register via inventory.** Instead, register it manually in the CLI's `build_registry()` function (in `crates/hermes-cli/src/tooling.rs`). The tool struct lives in hermes-agent.

```rust
// crates/hermes-agent/src/delegate.rs
pub struct DelegationTool;
impl Tool for DelegationTool { ... }

// crates/hermes-cli/src/tooling.rs
registry.register(Box::new(hermes_agent::delegate::DelegationTool));
```

---

## 5. Clarify Tool

### Types: `crates/hermes-core/src/clarify.rs`
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

### Tool: `crates/hermes-tools/src/clarify.rs`

- `name()` → "clarify"
- `is_exclusive()` → true (blocks parallel execution)
- `execute()`:
  1. Check `ctx.delegation_depth > 0` → error "clarify not available in delegation"
  2. Check `ctx.clarify_tx.is_some()` → error if None
  3. Parse question + choices from args
  4. Send ClarifyRequest via channel
  5. Await oneshot response (async — does NOT block tokio)
  6. Return response as JSON

### CLI handler: in `crates/hermes-cli/src/repl.rs`

Spawn a task that receives ClarifyRequests and uses the readline input:
- Multiple-choice: print numbered choices, wait for number or text
- Open-ended: print question, wait for text
- Timeout: 120 seconds

---

## 6. CLI Commands

### Registry: `crates/hermes-cli/src/commands.rs`

```rust
pub struct CommandDef {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub usage: &'static str,
}
```

13 commands:

| Command | Aliases | Implementation |
|---------|---------|---------------|
| help | h, ? | Print command table |
| quit | q, exit | Break loop (exists) |
| new | reset | End session, clear history (exists) |
| clear | | Clear terminal |
| model | m | Show current model (read-only) |
| tools | t | List tools from registry |
| status | | Session info (id, model, messages, token estimate) |
| retry | | Pop last turn, re-send |
| undo | | Pop last turn |
| compress | | Manually trigger compression |
| skills | | List skills, /skills reload |
| save | | Export history to JSON file |
| cron | | Placeholder: print "cron not yet implemented" |

### Dispatch in repl_loop

Replace the current `match input.as_str()` with:
```rust
if input.starts_with('/') {
    if let Some(cmd) = resolve_command(&input) {
        handle_command(cmd, &input, &mut agent, &mut history, ...)?;
        continue;
    } else {
        println!("Unknown command. Type /help for list.");
        continue;
    }
}
```

---

## 7. File Structure

### New files
```
crates/hermes-core/src/clarify.rs          # ClarifyRequest, ClarifyResponse types
crates/hermes-agent/src/delegate.rs        # DelegationTool (lives in hermes-agent to avoid circular dep)
crates/hermes-tools/src/clarify.rs         # ClarifyTool
crates/hermes-cli/src/commands.rs          # CommandDef registry + resolve
crates/hermes-cli/src/handlers.rs          # Command handler implementations
```

### Modified files
```
crates/hermes-core/src/tool.rs             # ToolContext: add registry, delegation_depth, clarify_tx
crates/hermes-core/src/lib.rs              # add pub mod clarify
crates/hermes-agent/src/loop_runner.rs     # pass registry + delegation_depth + clarify_tx into ToolContext
crates/hermes-agent/src/lib.rs             # add pub mod delegate
crates/hermes-tools/src/lib.rs             # add clarify module
crates/hermes-cli/src/repl.rs              # command dispatch, clarify handler
crates/hermes-cli/src/tooling.rs           # register DelegationTool
```

---

## 8. Testing Strategy

| Component | Tests |
|-----------|-------|
| DelegationTool | MockProvider child, verify isolation (empty history), blocked tools filtered, depth limit, result format |
| ClarifyTool | Channel request/response roundtrip, timeout, blocked in delegation (depth>0) |
| Command registry | Resolve by name, alias, prefix, ambiguous returns None |
| /retry | Pop last turn, re-send same message, verify history |
| /undo | Pop last turn, verify history shorter |
| /compress | Trigger compression manually |
| /status | Print correct stats |
| ToolContext fields | All 3 new fields propagated through agent loop |

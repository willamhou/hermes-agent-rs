# Phase 4: Delegation, Clarify & CLI Commands — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add subagent delegation, interactive clarify tool, and 13 CLI slash commands.

**Architecture:** Three sub-phases: 4a (ToolContext expansion + Clarify types), 4b (Delegation + Clarify tools), 4c (CLI command system). Delegation lives in hermes-agent (not hermes-tools) to avoid circular dependency.

**Tech Stack:** tokio channels (mpsc, oneshot), crossterm (clear screen), inventory

**Implementation order:** Tasks must be sequential — each builds on the previous.

**Review fixes applied:** Use existing `aux_provider` (not new provider field), DelegationTool in hermes-agent (avoids circular dep), /model as read-only, clarify is_exclusive=true.

---

## Task 1: ToolContext Expansion + Clarify Types

Add 3 new fields to ToolContext and create ClarifyRequest/Response types.

**Files:**
- Create: `crates/hermes-core/src/clarify.rs`
- Modify: `crates/hermes-core/src/tool.rs`
- Modify: `crates/hermes-core/src/lib.rs`

- [ ] **Step 1: Create clarify types**

Create `crates/hermes-core/src/clarify.rs`:
```rust
use tokio::sync::oneshot;

pub struct ClarifyRequest {
    pub question: String,
    pub choices: Vec<String>,
    pub response_tx: oneshot::Sender<ClarifyResponse>,
}

#[derive(Debug, Clone)]
pub enum ClarifyResponse {
    Answer(String),
    Timeout,
}
```

Add `pub mod clarify;` to `crates/hermes-core/src/lib.rs`.

- [ ] **Step 2: Add 3 fields to ToolContext**

In `crates/hermes-core/src/tool.rs`, add to ToolContext:
```rust
pub registry: Arc<hermes_tools_registry_placeholder>,  // see note below
pub delegation_depth: u32,
pub clarify_tx: Option<mpsc::Sender<crate::clarify::ClarifyRequest>>,
```

**IMPORTANT:** ToolContext can't directly reference `hermes_tools::ToolRegistry` because hermes-core can't depend on hermes-tools (circular). Two options:
- A: Use `Arc<dyn Any>` and downcast (ugly)
- B: Add a trait `ToolRegistryAccess` to hermes-core

**Choice B:** Add a simple trait:
```rust
// In hermes-core/src/tool.rs
pub trait ToolRegistryAccess: Send + Sync {
    fn available_tool_names(&self) -> Vec<String>;
    fn filter_by_toolsets(&self, toolsets: &[String], blocked: &[String]) -> Box<dyn ToolRegistryAccess>;
}
```

Actually this adds too much complexity for Phase 4. **Simpler approach:** The DelegationTool is in hermes-agent which already depends on hermes-tools. It can access the registry from AgentConfig. Pass registry info through ToolContext as `registry_names: Arc<Vec<String>>` (just the tool names, not the full registry). The DelegationTool constructs the child's registry by cloning and filtering the parent registry that it receives at registration time.

**Simplest approach:** Just add `delegation_depth: u32` and `clarify_tx: Option<...>` to ToolContext. The DelegationTool gets the registry from a captured `Arc<ToolRegistry>` at registration time (like inventory but manual), NOT from ToolContext.

```rust
#[derive(Clone)]
pub struct ToolContext {
    // ... existing 8 fields ...
    pub delegation_depth: u32,
    pub clarify_tx: Option<mpsc::Sender<crate::clarify::ClarifyRequest>>,
}
```

- [ ] **Step 3: Fix all ToolContext construction sites**

Every place that builds ToolContext needs the 2 new fields:
- `crates/hermes-agent/src/loop_runner.rs` — add `delegation_depth: 0, clarify_tx: None` (we'll wire clarify_tx in Task 5)
- `crates/hermes-tools/src/registry.rs` (test) — add fields
- `crates/hermes-agent/tests/e2e_test.rs` — add fields
- Any other test files that construct ToolContext

- [ ] **Step 4: Run tests + commit**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt`

Commit: `feat: add clarify types and expand ToolContext with delegation_depth`

---

## Task 2: ClarifyTool

Interactive question tool that communicates with CLI via channel.

**Files:**
- Create: `crates/hermes-tools/src/clarify.rs`
- Modify: `crates/hermes-tools/src/lib.rs`

- [ ] **Step 1: Implement ClarifyTool**

```rust
pub struct ClarifyTool;

#[async_trait]
impl Tool for ClarifyTool {
    fn name(&self) -> &str { "clarify" }
    fn toolset(&self) -> &str { "interaction" }
    fn is_exclusive(&self) -> bool { true }

    fn schema(&self) -> ToolSchema { /* question: string required, choices: array optional */ }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        // 1. Check delegation depth
        if ctx.delegation_depth > 0 {
            return Ok(ToolResult::error("clarify is not available in delegated tasks"));
        }
        // 2. Check clarify channel
        let tx = ctx.clarify_tx.as_ref()
            .ok_or_else(|| HermesError::Tool { name: "clarify".into(), message: "no clarify handler".into() })?;
        // 3. Parse question + choices
        // 4. Send ClarifyRequest, await oneshot
        // 5. Match response: Answer(s) → ToolResult::ok, Timeout → ToolResult with timeout note
    }
}

inventory::submit! { crate::ToolRegistration { factory: || Box::new(ClarifyTool) } }
```

- [ ] **Step 2: Tests**

4 tests:
- `test_clarify_blocked_in_delegation` — depth=1, verify error
- `test_clarify_no_handler` — clarify_tx=None, verify error
- `test_clarify_answer` — send Answer through channel, verify result
- `test_clarify_timeout` — send Timeout, verify result contains timeout note

- [ ] **Step 3: Wire up + commit**

Add `pub mod clarify;` to lib.rs.
Commit: `feat: implement clarify tool with channel-based user interaction`

---

## Task 3: DelegationTool

Subagent spawning tool. Lives in hermes-agent (not hermes-tools) to avoid circular dependency.

**Files:**
- Create: `crates/hermes-agent/src/delegate.rs`
- Modify: `crates/hermes-agent/src/lib.rs`
- Modify: `crates/hermes-cli/src/tooling.rs` (register the tool)

- [ ] **Step 1: Implement DelegationTool**

```rust
// crates/hermes-agent/src/delegate.rs
use std::sync::Arc;
use std::time::Instant;

pub struct DelegationTool {
    pub provider: Arc<dyn Provider>,
    pub parent_registry: Arc<ToolRegistry>,
    pub tool_config: Arc<ToolConfig>,
    pub memory_manager: ???,  // Need to figure out how to pass this
}
```

**Problem:** DelegationTool needs MemoryManager to create child's memory (new_child()). But MemoryManager is owned by Agent, not available at tool registration time.

**Solution:** DelegationTool captures `Arc<dyn Provider>` and `Arc<ToolRegistry>` at registration. For memory, the child gets a fresh MemoryManager (no external provider, just builtin clone). Pass the memory dir path instead:

```rust
pub struct DelegationTool {
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    tool_config: Arc<ToolConfig>,
    memory_dir: PathBuf,
}
```

Or simpler: just use ToolContext's existing fields:
- `ctx.aux_provider` → child's provider
- registry → captured at construction
- memory → child gets fresh MemoryManager

```rust
impl DelegationTool {
    pub fn new(
        registry: Arc<ToolRegistry>,
        memory_dir: PathBuf,
    ) -> Self {
        Self { registry, memory_dir }
    }
}

#[async_trait]
impl Tool for DelegationTool {
    fn name(&self) -> &str { "delegate_task" }
    fn toolset(&self) -> &str { "delegation" }
    fn is_exclusive(&self) -> bool { true }

    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        // 1. Check depth limit
        if ctx.delegation_depth >= 1 {
            return Ok(ToolResult::error("delegation depth limit reached"));
        }

        // 2. Parse args (goal, context, toolsets)
        let goal = args.get("goal").and_then(|v| v.as_str())
            .ok_or_else(|| HermesError::Tool { name: "delegate_task".into(), message: "goal required".into() })?;

        // 3. Get provider from ctx
        let provider = ctx.aux_provider.as_ref()
            .ok_or_else(|| HermesError::Tool { name: "delegate_task".into(), message: "no provider".into() })?
            .clone();

        // 4. Build filtered registry
        let mut child_registry = ToolRegistry::new();
        // Copy tools from self.registry, filtering by toolsets and removing blocked
        for name in self.registry.tool_names() {
            if BLOCKED.contains(&name) { continue; }
            // if toolsets specified, check tool's toolset matches
            if let Some(tool) = self.registry.get(name) {
                // Can't move Box<dyn Tool> out of registry...
                // Need ToolRegistry to support cloning tools or filtering
            }
        }

        // PROBLEM: ToolRegistry stores Box<dyn Tool> which can't be cloned.
        // Solution: child reuses parent's registry (Arc clone) but DelegationTool
        // checks blocked tools at execution time, not at registry level.
        // Simpler: child uses same registry, just relies on BLOCKED check in execute.

        // Actually simplest: child uses FULL parent registry. The child's own
        // execute() will check delegation_depth and block "delegate_task" + "clarify"
        // at the tool level, not the registry level.

        // 5. Build child Agent
        let child_memory = MemoryManager::new(self.memory_dir.clone(), None)?;
        let (child_approval_tx, mut child_approval_rx) = mpsc::channel(8);
        // Auto-allow for children
        tokio::spawn(async move {
            while let Some(req) = child_approval_rx.recv().await {
                let _ = req.response_tx.send(ApprovalDecision::Allow);
            }
        });

        let system_prompt = build_child_system_prompt(goal, context, &ctx.working_dir);

        let mut child = Agent::new(AgentConfig {
            provider,
            registry: Arc::clone(&self.registry),
            max_iterations: 50,
            system_prompt,
            session_id: Uuid::new_v4().to_string(),
            working_dir: ctx.working_dir.clone(),
            approval_tx: child_approval_tx,
            tool_config: Arc::clone(&ctx.tool_config),
            memory: child_memory,
            skills: None,  // children don't get skills
            compression: CompressionConfig::default(),
        });

        // 6. Run child
        let start = Instant::now();
        let (delta_tx, _rx) = mpsc::channel(64); // discard child's deltas
        let mut child_history = Vec::new();
        let result = child.run_conversation(goal, &mut child_history, delta_tx).await;

        let duration_ms = start.elapsed().as_millis() as u64;
        let iterations_used = 50 - child.remaining_budget();

        // 7. Format result
        match result {
            Ok(summary) => Ok(ToolResult::ok(serde_json::json!({
                "status": "completed",
                "summary": summary,
                "iterations_used": iterations_used,
                "duration_ms": duration_ms,
            }).to_string())),
            Err(e) => Ok(ToolResult::ok(serde_json::json!({
                "status": "failed",
                "summary": e.to_string(),
                "iterations_used": iterations_used,
                "duration_ms": duration_ms,
            }).to_string())),
        }
    }
}
```

- [ ] **Step 2: Register in CLI tooling.rs**

In `crates/hermes-cli/src/tooling.rs`, after building registry from inventory:
```rust
use hermes_agent::delegate::DelegationTool;

registry.register(Box::new(DelegationTool::new(
    Arc::clone(&registry_arc),
    hermes_config::hermes_home().join("memories"),
)));
```

Wait — this is tricky because we need to register AFTER creating the Arc. Check how tooling.rs currently works.

- [ ] **Step 3: Tests**

5 tests in `crates/hermes-agent/tests/`:
- `test_delegation_basic` — child runs echo command, returns summary
- `test_delegation_depth_limit` — depth=1, verify error
- `test_delegation_blocked_tools` — child can't use delegate_task (depth check)
- `test_delegation_result_format` — verify JSON has status, summary, iterations, duration
- `test_delegation_child_history_isolated` — parent history unchanged after delegation

- [ ] **Step 4: Commit**

Commit: `feat: implement delegation tool for subagent spawning`

---

## Task 4: CLI Command Registry

Static command registry with resolve-by-name/alias/prefix.

**Files:**
- Create: `crates/hermes-cli/src/commands.rs`

- [ ] **Step 1: Implement command registry**

```rust
pub struct CommandDef {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub usage: &'static str,
}

pub const COMMANDS: &[CommandDef] = &[
    CommandDef { name: "help",     aliases: &["h", "?"],    description: "Show commands",        usage: "/help" },
    CommandDef { name: "quit",     aliases: &["q", "exit"],  description: "Exit",                usage: "/quit" },
    CommandDef { name: "new",      aliases: &["reset"],      description: "New conversation",    usage: "/new" },
    CommandDef { name: "clear",    aliases: &[],             description: "Clear screen",        usage: "/clear" },
    CommandDef { name: "model",    aliases: &["m"],          description: "Show current model",  usage: "/model" },
    CommandDef { name: "tools",    aliases: &["t"],          description: "List tools",          usage: "/tools" },
    CommandDef { name: "status",   aliases: &[],             description: "Session info",        usage: "/status" },
    CommandDef { name: "retry",    aliases: &[],             description: "Re-run last message", usage: "/retry" },
    CommandDef { name: "undo",     aliases: &[],             description: "Remove last turn",    usage: "/undo" },
    CommandDef { name: "compress", aliases: &[],             description: "Compress context",    usage: "/compress" },
    CommandDef { name: "skills",   aliases: &[],             description: "List/reload skills",  usage: "/skills [reload]" },
    CommandDef { name: "save",     aliases: &[],             description: "Save to file",        usage: "/save [path]" },
    CommandDef { name: "cron",     aliases: &[],             description: "Scheduled jobs",      usage: "/cron" },
];

pub fn resolve_command(input: &str) -> Option<&'static CommandDef> {
    let word = input.split_whitespace().next()?.strip_prefix('/')?;
    // Exact match
    if let Some(cmd) = COMMANDS.iter().find(|c| c.name == word || c.aliases.contains(&word)) {
        return Some(cmd);
    }
    // Unambiguous prefix
    let matches: Vec<_> = COMMANDS.iter().filter(|c| c.name.starts_with(word)).collect();
    if matches.len() == 1 { Some(matches[0]) } else { None }
}
```

5 tests: resolve by name, alias, prefix, ambiguous prefix → None, unknown → None.

Commit: `feat: add CLI command registry with prefix matching`

---

## Task 5: CLI Command Handlers + Clarify Wiring

Implement all 13 command handlers and wire clarify channel into the REPL.

**Files:**
- Create: `crates/hermes-cli/src/handlers.rs`
- Modify: `crates/hermes-cli/src/repl.rs`

- [ ] **Step 1: Implement handlers**

Each handler is a function. Complex ones (retry, undo, compress) need mutable access to agent/history.

```rust
// handlers.rs
pub fn handle_help() { /* print COMMANDS table */ }
pub fn handle_clear() { /* crossterm Clear(All) */ }
pub fn handle_model(config: &AppConfig) { /* print current model */ }
pub fn handle_tools(registry: &ToolRegistry) { /* list tool names */ }
pub fn handle_status(session_id: &str, history: &[Message], config: &AppConfig) { /* print stats */ }
pub fn handle_retry(history: &mut Vec<Message>) -> Option<String> { /* pop last turn, return user msg to re-send */ }
pub fn handle_undo(history: &mut Vec<Message>) { /* pop last turn */ }
pub async fn handle_compress(agent: &mut Agent, history: &mut Vec<Message>) { /* call compressor */ }
pub fn handle_skills(skills: &Option<...>) { /* list skills */ }
pub fn handle_save(history: &[Message], path: Option<&str>) { /* serialize to file */ }
pub fn handle_cron() { println!("Cron scheduling not yet implemented."); }
```

- [ ] **Step 2: Integrate into repl_loop**

Replace the current `match input.as_str()` block with command dispatch:
```rust
if input.starts_with('/') {
    match commands::resolve_command(&input) {
        Some(cmd) => {
            match cmd.name {
                "quit" | "exit" => { ... break; }
                "new" => { ... }
                "help" => handlers::handle_help(),
                "clear" => handlers::handle_clear(),
                // ... etc
                "retry" => {
                    if let Some(msg) = handlers::handle_retry(&mut history) {
                        // Re-send the message (fall through to normal message handling)
                        pending_retry = Some(msg);
                    }
                }
                _ => {}
            }
            continue;
        }
        None => {
            println!("Unknown command. /help for list.");
            continue;
        }
    }
}
```

- [ ] **Step 3: Wire clarify channel**

In repl_loop, create clarify channel and pass to Agent/ToolContext:
```rust
let (clarify_tx, mut clarify_rx) = mpsc::channel::<ClarifyRequest>(4);

// Spawn clarify handler
tokio::spawn(async move {
    while let Some(req) = clarify_rx.recv().await {
        // Render question + choices to stdout
        // Read response from readline (via another channel to the readline thread)
        // Send response back via req.response_tx
    }
});
```

Pass `clarify_tx` to AgentConfig or to the ToolContext construction in loop_runner.rs.

**Note:** The clarify handler needs to interact with the readline thread. This requires a secondary channel: clarify handler → readline thread → user response → clarify handler → oneshot response. For Phase 4, simplify: use `tokio::task::spawn_blocking` for the readline call inside the clarify handler.

- [ ] **Step 4: Tests + commit**

Commit: `feat: implement 13 CLI commands and wire clarify handler`

---

## Task 6: Full Build Verification

- [ ] Run `cargo fmt && cargo clippy --workspace -- -D warnings && cargo test --workspace`
- [ ] Build release: `cargo build --release -p hermes-cli`
- [ ] Smoke test: verify /help, /status, /tools, /undo work
- [ ] Smoke test: verify clarify tool (if model calls it)
- [ ] Commit any fixes

Commit: `chore: fix Phase 4 build issues`

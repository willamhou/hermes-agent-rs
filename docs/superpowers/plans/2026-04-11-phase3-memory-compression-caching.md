# Phase 3: Memory, Compression & Prompt Caching — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a memory system (file-backed with prefetch/sync lifecycle), context compression (layered: prune → protect → summarize → sanitize), and prompt caching (Anthropic cache segments) to the agent loop.

**Architecture:** Three sub-phases implemented sequentially. 3a: memory (BuiltinMemory + MemoryManager + agent integration). 3b: prompt cache (PromptCacheManager with provider gating). 3c: compression (TokenCounter + ContextCompressor with tool pair sanitization). Each sub-phase produces testable, working software independently.

**Tech Stack:** tokio (spawn, Mutex), std::fs, std::collections::hash_map::DefaultHasher

**Review Fixes Applied** (from Claude + Codex review):
1. Add `hermes-memory` dep to hermes-cli Cargo.toml (not just hermes-agent)
2. Fix ALL AgentConfig construction sites (loop_runner tests + e2e_test.rs + CLI)
3. Use `crate::compressor::` not `hermes_agent::compressor::` inside hermes-agent
4. Make `full_system` mutable from Task 4 (compression needs to reassign)
5. Add async prefetch/take tests to MemoryManager
6. Add rebuild-detection test to CacheManager (not just output equality)
7. Compressor MockProvider must capture and verify ChatRequest content
8. BuiltinMemory tests must respect frozen-snapshot timing
9. Tasks 4/6/8 must be sequential (all modify loop_runner.rs)
10. Make `segments` mutable from Task 6 (compression needs to reassign)

---

## File Structure

### New files
```
crates/hermes-agent/src/token_counter.rs    # Heuristic token counter
crates/hermes-agent/src/cache_manager.rs    # PromptCacheManager
crates/hermes-agent/src/compressor.rs       # ContextCompressor + tool pair sanitization
crates/hermes-memory/src/builtin.rs         # BuiltinMemory (file-backed)
crates/hermes-memory/src/manager.rs         # MemoryManager (orchestrator)
```

### Modified files
```
crates/hermes-core/src/memory.rs            # trim trait, add defaults
crates/hermes-memory/src/lib.rs             # wire modules
crates/hermes-memory/Cargo.toml             # add deps
crates/hermes-agent/src/lib.rs              # add modules
crates/hermes-agent/src/loop_runner.rs      # integrate memory + caching + compression
crates/hermes-agent/Cargo.toml              # add hermes-memory dep
crates/hermes-cli/src/repl.rs               # construct MemoryManager
crates/hermes-cli/src/oneshot.rs            # same
```

---

## Phase 3a: Memory System

### Task 1: Trim MemoryProvider Trait + Token Counter

Simplify the MemoryProvider trait (add default no-op implementations) and add the heuristic token counter.

**Files:**
- Modify: `crates/hermes-core/src/memory.rs`
- Create: `crates/hermes-agent/src/token_counter.rs`
- Modify: `crates/hermes-agent/src/lib.rs`

- [ ] **Step 1: Revise MemoryProvider trait with defaults**

Replace `crates/hermes-core/src/memory.rs` — keep only the hooks Phase 3 needs as required, add default no-ops for the rest:

```rust
use async_trait::async_trait;

use crate::error::Result;
use crate::message::Message;

#[async_trait]
pub trait MemoryProvider: Send + Sync {
    fn system_prompt_block(&self) -> Option<String>;

    async fn prefetch(&self, query: &str, session_id: &str) -> Result<String> {
        let _ = (query, session_id);
        Ok(String::new())
    }

    async fn sync_turn(&self, user: &str, assistant: &str, session_id: &str) -> Result<()> {
        let _ = (user, assistant, session_id);
        Ok(())
    }

    async fn on_pre_compress(&self, messages: &[Message]) -> Result<Option<String>> {
        let _ = messages;
        Ok(None)
    }

    async fn on_memory_write(&self, action: &str, target: &str, content: &str) -> Result<()> {
        let _ = (action, target, content);
        Ok(())
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
```

- [ ] **Step 2: Create token_counter.rs**

Create `crates/hermes-agent/src/token_counter.rs`:

```rust
use hermes_core::message::Message;

pub struct TokenCounter;

impl TokenCounter {
    pub fn count_text(text: &str) -> usize {
        (text.len() + 3) / 4
    }

    pub fn count_message(msg: &Message) -> usize {
        let mut count = 4;
        count += Self::count_text(&msg.content.as_text_lossy());
        for tc in &msg.tool_calls {
            count += Self::count_text(&tc.name);
            count += Self::count_text(&tc.arguments.to_string());
        }
        if let Some(ref r) = msg.reasoning {
            count += Self::count_text(r);
        }
        count
    }

    pub fn count_messages(msgs: &[Message]) -> usize {
        msgs.iter().map(Self::count_message).sum()
    }

    pub fn estimate_request(system: &str, msgs: &[Message], tool_count: usize) -> usize {
        let mut total = Self::count_text(system);
        total += Self::count_messages(msgs);
        total += tool_count * 50;
        total
    }
}
```

Add 5 tests in `#[cfg(test)]`:
- `test_count_text_empty` — 0 for ""
- `test_count_text_short` — "hello" → (5+3)/4 = 2
- `test_count_text_long` — 400 chars → ~100
- `test_count_message_with_tool_calls` — message with tool calls counted
- `test_estimate_request_includes_system` — system prompt adds to total

- [ ] **Step 3: Wire up lib.rs**

Add `pub mod token_counter;` to `crates/hermes-agent/src/lib.rs`.

- [ ] **Step 4: Run tests + clippy + fmt, commit**

Run: `cargo test --workspace && cargo clippy --workspace -- -D warnings && cargo fmt`

Commit: `feat: trim MemoryProvider trait and add heuristic token counter`

---

### Task 2: BuiltinMemory

File-backed memory with frozen snapshot pattern.

**Files:**
- Create: `crates/hermes-memory/src/builtin.rs`
- Modify: `crates/hermes-memory/src/lib.rs`
- Modify: `crates/hermes-memory/Cargo.toml`

- [ ] **Step 1: Add deps to hermes-memory Cargo.toml**

Add to [dependencies]:
```toml
dirs.workspace = true
```

Add [dev-dependencies]:
```toml
[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
tempfile.workspace = true
```

- [ ] **Step 2: Implement BuiltinMemory**

Create `crates/hermes-memory/src/builtin.rs`:

```rust
use std::path::{Path, PathBuf};
use hermes_core::error::{HermesError, Result};

const MEMORY_MAX_CHARS: usize = 2200;
const USER_MAX_CHARS: usize = 1375;
const ENTRY_SEPARATOR: &str = "\n§\n";

#[derive(Clone)]
pub struct BuiltinMemory {
    dir: PathBuf,
    memory_snapshot: String,
    user_snapshot: String,
}

impl BuiltinMemory {
    pub fn load(dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&dir)
            .map_err(|e| HermesError::Memory(format!("create memory dir: {e}")))?;
        let memory = Self::read_file(&dir.join("MEMORY.md"));
        let user = Self::read_file(&dir.join("USER.md"));
        Ok(Self {
            dir,
            memory_snapshot: memory,
            user_snapshot: user,
        })
    }

    pub fn system_prompt_block(&self) -> Option<String> {
        let mut parts = Vec::new();
        if !self.memory_snapshot.trim().is_empty() {
            parts.push(format!("## Notes\n{}", self.memory_snapshot));
        }
        if !self.user_snapshot.trim().is_empty() {
            parts.push(format!("## User Profile\n{}", self.user_snapshot));
        }
        if parts.is_empty() { None } else { Some(parts.join("\n\n")) }
    }

    pub fn read_live(&self, key: &str) -> Result<Option<String>> {
        let path = self.key_path(key)?;
        if !path.exists() { return Ok(None); }
        std::fs::read_to_string(&path)
            .map(Some)
            .map_err(|e| HermesError::Memory(format!("read {key}: {e}")))
    }

    pub fn write(&self, key: &str, content: &str) -> Result<()> {
        let path = self.key_path(key)?;
        let max_chars = match key {
            "MEMORY" => MEMORY_MAX_CHARS,
            "USER" => USER_MAX_CHARS,
            _ => return Err(HermesError::Memory(format!("unknown key: {key}"))),
        };
        let truncated = Self::truncate_entries(content, max_chars);
        std::fs::write(&path, &truncated)
            .map_err(|e| HermesError::Memory(format!("write {key}: {e}")))
    }

    pub fn refresh_snapshot(&mut self) -> Result<()> {
        self.memory_snapshot = Self::read_file(&self.dir.join("MEMORY.md"));
        self.user_snapshot = Self::read_file(&self.dir.join("USER.md"));
        Ok(())
    }

    fn key_path(&self, key: &str) -> Result<PathBuf> {
        match key {
            "MEMORY" => Ok(self.dir.join("MEMORY.md")),
            "USER" => Ok(self.dir.join("USER.md")),
            _ => Err(HermesError::Memory(format!("unknown key: {key}"))),
        }
    }

    fn read_file(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    fn truncate_entries(content: &str, max_chars: usize) -> String {
        if content.len() <= max_chars {
            return content.to_string();
        }
        let entries: Vec<&str> = content.split(ENTRY_SEPARATOR).collect();
        let mut result: Vec<&str> = Vec::new();
        let mut total = 0;
        // Keep entries from the end (newest first)
        for entry in entries.iter().rev() {
            let entry_len = entry.len() + ENTRY_SEPARATOR.len();
            if total + entry_len > max_chars && !result.is_empty() {
                break;
            }
            result.push(entry);
            total += entry_len;
        }
        result.reverse();
        result.join(ENTRY_SEPARATOR)
    }
}
```

8 tests:
- `test_load_empty_dir` — fresh dir, empty snapshots
- `test_write_and_read_live` — write MEMORY, read back
- `test_snapshot_frozen_after_write` — write doesn't change snapshot
- `test_refresh_snapshot` — refresh updates snapshot from disk
- `test_system_prompt_block_empty` — no files → None
- `test_system_prompt_block_with_content` — files exist → Some(block)
- `test_truncate_entries_under_limit` — no truncation
- `test_truncate_entries_over_limit` — oldest entries dropped

- [ ] **Step 3: Wire up lib.rs**

Replace `crates/hermes-memory/src/lib.rs`:
```rust
pub mod builtin;
```

- [ ] **Step 4: Run tests + commit**

Commit: `feat: implement BuiltinMemory with frozen snapshot pattern`

---

### Task 3: MemoryManager

Orchestrator for builtin + optional external memory provider.

**Files:**
- Create: `crates/hermes-memory/src/manager.rs`
- Modify: `crates/hermes-memory/src/lib.rs`

- [ ] **Step 1: Implement MemoryManager**

> **REVIEW FIX #5**: Tests MUST include async prefetch/take behavior. Add:
> - `test_queue_prefetch_and_take` — queue a prefetch (requires a mock MemoryProvider), then take_prefetched, verify the cached value is returned
> - `test_sync_turn_does_not_block` — verify sync_turn returns immediately (timeout-guarded)
> Frozen snapshot tests must write files BEFORE calling `MemoryManager::new()` or call `refresh_snapshot()` after writing.

Create `crates/hermes-memory/src/manager.rs`:

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hermes_core::error::Result;
use hermes_core::memory::MemoryProvider;
use hermes_core::message::Message;

use crate::builtin::BuiltinMemory;

pub struct MemoryManager {
    builtin: BuiltinMemory,
    external: Option<Arc<dyn MemoryProvider>>,
    prefetch_cache: Arc<Mutex<HashMap<String, String>>>,
}

impl MemoryManager {
    pub fn new(memory_dir: PathBuf, external: Option<Arc<dyn MemoryProvider>>) -> Result<Self> {
        Ok(Self {
            builtin: BuiltinMemory::load(memory_dir)?,
            external,
            prefetch_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn system_prompt_blocks(&self) -> String {
        let mut blocks = Vec::new();
        if let Some(b) = self.builtin.system_prompt_block() {
            blocks.push(b);
        }
        if let Some(ext) = &self.external {
            if let Some(b) = ext.system_prompt_block() {
                blocks.push(b);
            }
        }
        if blocks.is_empty() {
            return String::new();
        }
        blocks
            .iter()
            .map(|b| format!("<memory-context>\n{b}\n</memory-context>"))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn queue_prefetch(&self, hint: &str, session_id: &str) {
        let external = self.external.clone();
        let cache = Arc::clone(&self.prefetch_cache);
        let hint = hint.to_string();
        let sid = session_id.to_string();
        tokio::spawn(async move {
            if let Some(ext) = external {
                if let Ok(result) = ext.prefetch(&hint, &sid).await {
                    if !result.is_empty() {
                        cache.lock().expect("prefetch cache").insert(sid, result);
                    }
                }
            }
        });
    }

    pub async fn take_prefetched(&self, session_id: &str) -> Option<String> {
        let cached = {
            self.prefetch_cache.lock().expect("prefetch cache").remove(session_id)
        };
        if cached.is_some() {
            return cached;
        }
        if let Some(ext) = &self.external {
            ext.prefetch("", session_id).await.ok().filter(|s| !s.is_empty())
        } else {
            None
        }
    }

    pub fn sync_turn(&self, user: &str, assistant: &str, session_id: &str) {
        let external = self.external.clone();
        let (user, assistant, sid) = (user.to_string(), assistant.to_string(), session_id.to_string());
        tokio::spawn(async move {
            if let Some(ext) = external {
                let _ = ext.sync_turn(&user, &assistant, &sid).await;
            }
        });
    }

    pub async fn on_pre_compress(&self, messages: &[Message]) -> Option<String> {
        if let Some(ext) = &self.external {
            ext.on_pre_compress(messages).await.ok().flatten()
        } else {
            None
        }
    }

    pub fn refresh_snapshot(&mut self) -> Result<()> {
        self.builtin.refresh_snapshot()
    }

    pub fn builtin(&self) -> &BuiltinMemory {
        &self.builtin
    }

    pub fn new_child(&self) -> Result<Self> {
        Ok(Self {
            builtin: self.builtin.clone(),
            external: None,
            prefetch_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }
}
```

6 tests:
- `test_system_prompt_blocks_empty` — no files → empty string
- `test_system_prompt_blocks_with_content` — write memory, verify `<memory-context>` tags
- `test_new_child_isolation` — child has independent cache, no external
- `test_take_prefetched_cache_miss` — returns None with no external
- `test_manager_new_creates_dir` — fresh temp dir works
- `test_refresh_snapshot_updates` — write to disk, refresh, verify system_prompt_blocks changes

- [ ] **Step 2: Wire up lib.rs**

Update `crates/hermes-memory/src/lib.rs`:
```rust
pub mod builtin;
pub mod manager;

pub use manager::MemoryManager;
```

- [ ] **Step 3: Run tests + commit**

Commit: `feat: implement MemoryManager with prefetch and sync lifecycle`

---

### Task 4: Agent Memory Integration

Wire MemoryManager into AgentConfig and the conversation loop.

**Files:**
- Modify: `crates/hermes-agent/Cargo.toml`
- Modify: `crates/hermes-agent/src/loop_runner.rs`
- Modify: `crates/hermes-cli/src/repl.rs`
- Modify: `crates/hermes-cli/src/oneshot.rs`

- [ ] **Step 1: Add hermes-memory dependency**

Add to `crates/hermes-agent/Cargo.toml` [dependencies]:
```toml
hermes-memory.workspace = true
```

> **REVIEW FIX #1**: ALSO add to `crates/hermes-cli/Cargo.toml` [dependencies]:
> ```toml
> hermes-memory.workspace = true
> ```
> The CLI files (repl.rs, oneshot.rs) directly `use hermes_memory::MemoryManager`.

- [ ] **Step 2: Modify AgentConfig and Agent**

In `loop_runner.rs`:

Add `memory` to AgentConfig:
```rust
pub struct AgentConfig {
    // ... existing fields ...
    pub memory: hermes_memory::MemoryManager,
}
```

Add `memory` to Agent:
```rust
pub struct Agent {
    // ... existing fields ...
    memory: hermes_memory::MemoryManager,
}
```

In `Agent::new`, store `config.memory`.

- [ ] **Step 3: Modify run_conversation to use memory**

In `run_conversation`:

Before the loop:
```rust
// Take prefetched memory context (currently unused — external providers will use this in Phase 4)
let _memory_ctx = self.memory.take_prefetched(&self.session_id).await;

// Build system prompt with memory blocks
// REVIEW FIX #4: use `let mut` so compression (Task 8) can reassign
let memory_block = self.memory.system_prompt_blocks();
let mut full_system = if memory_block.is_empty() {
    self.system_prompt.clone()
} else {
    format!("{}\n\n{}", self.system_prompt, memory_block)
};
```

In the ChatRequest, use `&full_system` instead of `&self.system_prompt`.

After the loop (before return):
```rust
self.memory.sync_turn(user_message, &final_response, &self.session_id);
self.memory.queue_prefetch(&final_response, &self.session_id);
```

Store `final_response` properly — current code returns early from inside the loop. Need to capture it before break.

- [ ] **Step 4: Fix ALL tests that construct AgentConfig**

> **REVIEW FIX #2**: Update BOTH `loop_runner.rs` tests AND `crates/hermes-agent/tests/e2e_test.rs`.
> Every place that constructs `AgentConfig` must include the new `memory` field.

In `loop_runner.rs` test helper `make_agent`:
```rust
use hermes_memory::MemoryManager;

let memory = MemoryManager::new(std::env::temp_dir().join("hermes-test-memory"), None).unwrap();
// Add to AgentConfig: memory,
```

In `crates/hermes-agent/tests/e2e_test.rs` helper:
```rust
use hermes_memory::MemoryManager;

let memory = MemoryManager::new(workspace.path().join(".hermes-memory"), None).unwrap();
// Add to AgentConfig: memory,
```

- [ ] **Step 5: Update CLI (repl.rs + oneshot.rs)**

In both files, construct MemoryManager:
```rust
use hermes_memory::MemoryManager;

let memory_dir = hermes_config::hermes_home().join("memories");
let memory = MemoryManager::new(memory_dir, None)
    .context("failed to initialize memory")?;

// Add to AgentConfig:
memory,
```

- [ ] **Step 6: Run tests + clippy + fmt, commit**

Commit: `feat: integrate MemoryManager into agent loop with prefetch/sync lifecycle`

---

## Phase 3b: Prompt Caching

### Task 5: PromptCacheManager

Manages frozen system prompt for Anthropic cache optimization.

**Files:**
- Create: `crates/hermes-agent/src/cache_manager.rs`
- Modify: `crates/hermes-agent/src/lib.rs`

- [ ] **Step 1: Implement cache_manager.rs**

Create `crates/hermes-agent/src/cache_manager.rs`:

```rust
use std::hash::{DefaultHasher, Hash, Hasher};
use hermes_core::provider::CacheSegment;

pub struct PromptCacheManager {
    frozen: Option<FrozenSystemPrompt>,
}

struct FrozenSystemPrompt {
    hash: u64,
    segments: Vec<CacheSegment>,
}

impl PromptCacheManager {
    pub fn new() -> Self {
        Self { frozen: None }
    }

    pub fn get_or_freeze(
        &mut self,
        system_prompt: &str,
        memory_block: &str,
    ) -> Vec<CacheSegment> {
        let hash = Self::compute_hash(system_prompt, memory_block);
        if let Some(ref frozen) = self.frozen {
            if frozen.hash == hash {
                return frozen.segments.clone();
            }
        }
        let segments = Self::build_segments(system_prompt, memory_block);
        self.frozen = Some(FrozenSystemPrompt {
            hash,
            segments: segments.clone(),
        });
        segments
    }

    pub fn invalidate(&mut self) {
        self.frozen = None;
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.is_some()
    }

    fn compute_hash(system: &str, memory: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        system.hash(&mut hasher);
        memory.hash(&mut hasher);
        hasher.finish()
    }

    fn build_segments(system: &str, memory: &str) -> Vec<CacheSegment> {
        let mut segments = Vec::new();
        if !system.is_empty() {
            segments.push(CacheSegment {
                text: system.to_string(),
                label: "base",
                cache_control: true,
            });
        }
        if !memory.is_empty() {
            segments.push(CacheSegment {
                text: memory.to_string(),
                label: "memory",
                cache_control: true,
            });
        }
        segments
    }
}

impl Default for PromptCacheManager {
    fn default() -> Self { Self::new() }
}
```

8 tests:
- `test_new_is_not_frozen` — fresh manager, is_frozen() = false
- `test_freeze_creates_segments` — first call creates segments, is_frozen() = true
- `test_get_or_freeze_returns_cached` — same input twice, verify is_frozen() stayed true between calls (no invalidation happened)
- `test_get_or_freeze_rebuilds_on_change` — different input → is_frozen() was true, call with new input → verify segments changed (compare segment text content)
- `test_invalidate_clears` — invalidate → is_frozen() = false → next call rebuilds with is_frozen() = true again
- `test_segments_have_cache_control` — all segments have cache_control = true
- `test_empty_memory_skips_segment` — empty memory → only base segment
- `test_rebuild_count` — **REVIEW FIX #6**: Track rebuild by comparing segment Vec identity: call get_or_freeze twice with same input, verify the hash didn't change (is_frozen stayed true). Call with different input, verify hash changed (segments differ).

- [ ] **Step 2: Wire up lib.rs + CacheSegment Clone**

Add `pub mod cache_manager;` to `crates/hermes-agent/src/lib.rs`.

`CacheSegment` needs `Clone` derive. Check `crates/hermes-core/src/provider.rs` — if CacheSegment doesn't have Clone, add it.

- [ ] **Step 3: Run tests + commit**

Commit: `feat: implement PromptCacheManager with frozen system prompt`

---

### Task 6: Integrate Cache into Agent Loop

Wire PromptCacheManager into the agent with provider gating.

**Files:**
- Modify: `crates/hermes-agent/src/loop_runner.rs`

- [ ] **Step 1: Add cache_manager to Agent**

```rust
use crate::cache_manager::PromptCacheManager;

pub struct Agent {
    // ... existing fields ...
    cache_manager: PromptCacheManager,
}
```

In `Agent::new`, add `cache_manager: PromptCacheManager::new()`.

- [ ] **Step 2: Use cache in run_conversation**

After building `full_system` (from Task 4), add caching.

> **REVIEW FIX #10**: Use `let mut` for `segments` so Task 8 (compression) can reassign after invalidation.

```rust
// REVIEW FIX #10: `let mut` — compression (Task 8) will reassign after invalidation
let mut segments = if self.provider.supports_caching() {
    let memory_block = self.memory.system_prompt_blocks();
    Some(self.cache_manager.get_or_freeze(&self.system_prompt, &memory_block))
} else {
    None
};
```

In the ChatRequest:
```rust
system_segments: segments.as_deref(),
```

- [ ] **Step 3: Run tests + commit**

Commit: `feat: integrate prompt caching into agent loop with provider gating`

---

## Phase 3c: Context Compression

### Task 7: ContextCompressor Core

Implement the compression algorithm with tool pair sanitization.

**Files:**
- Create: `crates/hermes-agent/src/compressor.rs`
- Modify: `crates/hermes-agent/src/lib.rs`

- [ ] **Step 1: Implement compressor.rs**

Create `crates/hermes-agent/src/compressor.rs` with:

**CompressionConfig** struct with defaults (max_context_tokens: 200_000, threshold: 0.50, target: 0.20, protect_head: 3).

**CompressionResult** enum: NotNeeded, Compressed { before/after tokens, messages removed/kept }.

**ContextCompressor** struct:
```rust
pub struct ContextCompressor {
    config: CompressionConfig,
    previous_summary: Option<String>,
}
```

**should_compress(system, messages, tool_count) -> bool**: uses TokenCounter::estimate_request.

**compress(messages, provider, memory_contribution) -> Result<CompressionResult>**:

Phase 1 — `prune_tool_results(messages)`:
- Walk messages from start to end (skip tail-protected zone)
- Replace `Role::Tool` messages with content >200 chars with `"[Previous tool output cleared]"`

Phase 2 — `find_boundaries(messages) -> (head_end, tail_start)`:
- head_end = protect_head_messages
- tail_start: walk backward from end, accumulate tokens until target budget
- Align tail_start to tool pair boundaries using `find_tool_group_start(messages, index)`

`find_tool_group_start`: from a given index, walk backward. If the message is `Role::Tool`, find its parent assistant message (by matching `tool_call_id` against `ToolCall.id`s). Return the assistant message index.

Phase 3 — `summarize(compressible, provider, memory_contribution) -> String`:
- Serialize messages to text (truncate tool results to 6000, tool args to 1500)
- Build prompt with structured template + previous_summary if exists
- Call `provider.chat()` with prompt (no streaming, no tools)
- Store result in `self.previous_summary`
- On error: return fallback `"[Context compressed - summary unavailable]"`

Phase 4 — `rebuild(messages, head_end, tail_start, summary)`:
- `messages.drain(head_end..tail_start)`
- Insert `Message::user("<context-summary>\n{summary}\n</context-summary>")` at head_end

Phase 5 — `sanitize_tool_pairs(messages)`:
- Collect expected IDs from assistant tool_calls
- Collect result IDs from tool messages
- Remove orphan results (reference non-existent calls)
- Add stub results for calls without results

- [ ] **Step 2: Write tests**

14 tests:
- `test_should_compress_below_threshold` — returns false
- `test_should_compress_above_threshold` — returns true
- `test_should_compress_includes_system_prompt` — system prompt tokens push over threshold
- `test_prune_tool_results_long` — long tool result replaced
- `test_prune_tool_results_short_preserved` — short result kept
- `test_find_boundaries_basic` — head=3, correct tail start
- `test_find_boundaries_tool_pair_alignment` — tail start moves to include full tool group
- `test_sanitize_orphan_result_removed` — tool result with no matching call removed
- `test_sanitize_missing_result_stub_added` — call without result gets stub
- `test_sanitize_valid_pairs_unchanged` — matched pairs left intact
- `test_compress_full_with_mock` — full compression with MockProvider, verify history rebuilt correctly (summary as Message::user with `<context-summary>` tag)
- `test_compress_iterative_summary` — compress twice, verify previous_summary included in second prompt
- `test_compress_not_needed` — below threshold, returns NotNeeded
- `test_compress_summary_request_shape` — **REVIEW FIX #7**: MockProvider captures the ChatRequest and verifies: tools is empty, system is non-empty, messages contain the serialized conversation. Use `Arc<Mutex<Option<ChatRequest>>>` capture pattern (store a clone of the request fields, not the reference).

> **REVIEW FIX #7**: The MockProvider for compression tests MUST inspect the ChatRequest sent for summarization.
> Create a `CapturingMockProvider` that stores the request content in an `Arc<Mutex<Vec<String>>>` so tests can verify
> the summary prompt includes the right messages, previous_summary, and memory contribution.

For the full compression test, use the same MockProvider pattern from e2e tests, extended with request capture.

- [ ] **Step 3: Wire up lib.rs**

Add `pub mod compressor;` to `crates/hermes-agent/src/lib.rs`.

- [ ] **Step 4: Run tests + commit**

Commit: `feat: implement ContextCompressor with layered compression and tool pair sanitization`

---

### Task 8: Integrate Compression into Agent Loop

Wire compression check into the agent loop, coordinate with cache invalidation and memory refresh.

**Files:**
- Modify: `crates/hermes-agent/src/loop_runner.rs`

- [ ] **Step 1: Add compressor to AgentConfig and Agent**

```rust
use crate::compressor::{CompressionConfig, ContextCompressor};

pub struct AgentConfig {
    // ... existing fields ...
    pub compression: CompressionConfig,
}

pub struct Agent {
    // ... existing fields ...
    compressor: ContextCompressor,
}
```

In `Agent::new`:
```rust
compressor: ContextCompressor::new(config.compression),
```

- [ ] **Step 2: Add compression check in run_conversation**

After tool execution, before the next loop iteration:

```rust
// Compression check
let tool_count = self.registry.available_schemas().len();
if self.compressor.should_compress(&full_system, history, tool_count) {
    tracing::info!("context compression triggered");
    let contrib = self.memory.on_pre_compress(history).await;
    // REVIEW FIX #3: use `crate::` not `hermes_agent::` inside the crate
    match self.compressor.compress(history, self.provider.as_ref(), contrib.as_deref()).await {
        Ok(result) => {
            if let crate::compressor::CompressionResult::Compressed { before_tokens, after_tokens, .. } = &result {
                tracing::info!(before = before_tokens, after = after_tokens, "compression complete");
            }
            self.cache_manager.invalidate();
            let _ = self.memory.refresh_snapshot();
            // Rebuild full_system and segments for next iteration
            let memory_block = self.memory.system_prompt_blocks();
            full_system = if memory_block.is_empty() {
                self.system_prompt.clone()
            } else {
                format!("{}\n\n{}", self.system_prompt, memory_block)
            };
            if self.provider.supports_caching() {
                segments = Some(self.cache_manager.get_or_freeze(&self.system_prompt, &memory_block));
            }
        }
        Err(e) => {
            tracing::warn!("compression failed: {e}");
        }
    }
}
```

> Note: `full_system` was already made `let mut` in Task 4 (REVIEW FIX #4).
> `segments` was already made `let mut` in Task 6 (REVIEW FIX #10).

- [ ] **Step 3: Fix ALL tests that construct AgentConfig**

> **REVIEW FIX #2 (continued)**: Update ALL call sites — not just loop_runner tests.

Update `make_agent` in `loop_runner.rs` tests:
```rust
use crate::compressor::CompressionConfig;
// Add to AgentConfig: compression: CompressionConfig::default(),
```

Update `crates/hermes-agent/tests/e2e_test.rs`:
```rust
use hermes_agent::compressor::CompressionConfig;
// Add to AgentConfig: compression: CompressionConfig::default(),
```

Update CLI (`repl.rs`, `oneshot.rs`):
```rust
use hermes_agent::compressor::CompressionConfig;
// Add to AgentConfig: compression: CompressionConfig::default(),
```

- [ ] **Step 4: Run tests + commit**

Commit: `feat: integrate context compression into agent loop`

---

### Task 9: Full Build Verification

**Files:** None

- [ ] **Step 1: Run full checks**

```bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo build --release -p hermes-cli
```

Fix any issues.

- [ ] **Step 2: Smoke test**

```bash
OPENAI_API_KEY=<key> ./target/release/hermes --message "What is Rust?" --model "openai/gemini-3.1-pro-preview" --base-url "http://34.60.178.0:3000/v1"
```

Verify response and no panics.

- [ ] **Step 3: Commit fixes if any**

Commit: `chore: fix Phase 3 build issues`

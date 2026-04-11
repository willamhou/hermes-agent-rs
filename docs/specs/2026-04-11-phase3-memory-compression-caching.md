# Phase 3: Memory System, Context Compression & Prompt Caching — Design Spec (v2)

**Date**: 2026-04-11
**Status**: Revised after Codex review
**Depends on**: Phase 2 (complete)
**Validates**: Async prefetch, cache mechanics, context window management

**Implementation order**: 3a (Memory) → 3b (Prompt Caching) → 3c (Compression)

---

## 1. Scope

### In Scope
- Token counter (heuristic chars/4, swappable later)
- BuiltinMemory (file-based MEMORY.md + USER.md with frozen snapshot)
- MemoryManager (orchestrator: builtin + optional external, prefetch, async sync)
- PromptCacheManager (frozen system prompt, cache segments for Anthropic, provider-gated)
- ContextCompressor (layered: tool pruning → boundary detection → LLM summary → tool pair repair)
- Agent loop integration (memory lifecycle, cache management, compression check)

### Out of Scope (deferred)
- External memory providers (Honcho, etc.) — Phase 4+
- Memory tools (memory_read/write as agent-callable tools) — Phase 4
- HuggingFace tokenizers (precise counting) — Phase 4+
- ModelRouter / summary provider switching — Phase 4+
- SOUL.md persona injection — Phase 4

---

## 2. Review-Driven Changes (from Codex review)

| Issue | Original | Revised |
|-------|----------|---------|
| Summary as Message::system | Anthropic drops Role::System from messages | Summary inserted as `Message::user` with `<context-summary>` wrapper |
| MemoryProvider trait bloated | 10 hooks, spec uses ~5 | Trim trait to Phase 3 needed hooks only; keep others as no-op defaults |
| CacheManager returns `&[CacheSegment]` | Borrow across async boundary | Returns `Vec<CacheSegment>` (owned) |
| Token count ignores system prompt | Under-counts, compression triggers late | `should_compress()` takes system prompt + tool count as params |
| Tool pair fix by adjacency | Misses parallel multi-tool turns | ID-based grouping: collect all `tool_call_id`s, match against `ToolCall.id` |
| No provider gate on caching | CacheManager always active | Check `provider.supports_caching()` before freezing |
| Prefetch cache race | Concurrent overwrites | Acceptable for Phase 3 (no external provider = no concurrent prefetch) |

---

## 3. MemoryProvider Trait (Revised)

Trim the existing trait to have meaningful defaults for hooks not used in Phase 3. The trait stays in `hermes-core/src/memory.rs` but hooks that won't be called yet get default no-op implementations.

```rust
#[async_trait]
pub trait MemoryProvider: Send + Sync {
    /// Static system prompt block (frozen at session start).
    fn system_prompt_block(&self) -> Option<String>;

    /// Pre-turn recall. Returns context relevant to the upcoming turn.
    async fn prefetch(&self, query: &str, session_id: &str) -> Result<String> {
        Ok(String::new())
    }

    /// Post-turn write. Persists turn data to backend.
    async fn sync_turn(&self, user: &str, assistant: &str, session_id: &str) -> Result<()> {
        Ok(())
    }

    /// Called before compression. Provider contributes context to summary.
    async fn on_pre_compress(&self, messages: &[Message]) -> Result<Option<String>> {
        Ok(None)
    }

    /// Called on memory tool writes. Mirrors to external backends.
    async fn on_memory_write(&self, action: &str, target: &str, content: &str) -> Result<()> {
        Ok(())
    }

    /// Clean shutdown.
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
```

Removed from required (were never called): `on_turn_start`, `on_turn_end`, `on_session_end`, `on_delegation`. These can be added back as needed in later phases.

---

## Phase 3a: Memory System

### 3a.1 Token Counter

Heuristic estimation in `hermes-agent`. Designed for easy replacement.

```rust
// hermes-agent/src/token_counter.rs
pub struct TokenCounter;

impl TokenCounter {
    pub fn count_text(text: &str) -> usize {
        (text.len() + 3) / 4
    }

    pub fn count_message(msg: &Message) -> usize {
        let mut count = 4; // per-message overhead
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

    /// Estimate total request tokens (messages + system + tool schemas).
    pub fn estimate_request(system: &str, msgs: &[Message], tool_count: usize) -> usize {
        let mut total = Self::count_text(system);
        total += Self::count_messages(msgs);
        total += tool_count * 50; // ~50 tokens per tool schema (rough)
        total
    }
}
```

### 3a.2 BuiltinMemory

File-backed memory with frozen snapshot pattern.

```rust
// hermes-memory/src/builtin.rs
#[derive(Clone)]
pub struct BuiltinMemory {
    dir: PathBuf,
    memory_snapshot: String,  // frozen at load time
    user_snapshot: String,    // frozen at load time
}
```

**Storage**: `~/.hermes/memories/MEMORY.md` and `USER.md`. Entries separated by `§`. Character limits: MEMORY 2200, USER 1375.

**Key methods**:
- `load(dir)` → read files, store snapshots
- `system_prompt_block()` → returns frozen snapshot (not live state)
- `read_live(key)` → reads current file from disk (for tool responses)
- `write(key, content)` → writes to disk only, does NOT update snapshot
- `refresh_snapshot()` → re-reads files into snapshot (called after compression)

**Truncation on write**: When content exceeds limit, remove oldest entries (from top of file) until under limit.

### 3a.3 MemoryManager

```rust
// hermes-memory/src/manager.rs
pub struct MemoryManager {
    builtin: BuiltinMemory,
    external: Option<Arc<dyn MemoryProvider>>,
    prefetch_cache: Arc<Mutex<HashMap<String, String>>>,
}
```

**Methods**:
- `new(dir, external)` → load builtin, init cache
- `system_prompt_blocks()` → aggregate blocks wrapped in `<memory-context>` tags
- `queue_prefetch(hint, session_id)` → tokio::spawn background task, stores in cache
- `take_prefetched(session_id)` → take from cache (lock scoped, dropped before await)
- `sync_turn(user, assistant, session_id)` → tokio::spawn fire-and-forget
- `on_pre_compress(messages)` → delegate to providers
- `refresh_snapshot()` → delegate to builtin
- `new_child()` → independent cache, read-only builtin clone, no external

### 3a.4 Agent Integration (Memory Only)

Add `MemoryManager` to `AgentConfig` and `Agent`. Wire lifecycle:

```rust
pub struct AgentConfig {
    // ... existing ...
    pub memory: MemoryManager,
}
```

In `run_conversation`:
- Before loop: `take_prefetched()`, build system prompt with memory blocks
- After loop: `sync_turn()`, `queue_prefetch()`

---

## Phase 3b: Prompt Caching

### 3b.1 PromptCacheManager

```rust
// hermes-agent/src/cache_manager.rs
pub struct PromptCacheManager {
    frozen: Option<FrozenSystemPrompt>,
}

struct FrozenSystemPrompt {
    hash: u64,
    segments: Vec<CacheSegment>,
}
```

**Returns owned data** (Vec, not &slice) to avoid borrow issues across async:

```rust
impl PromptCacheManager {
    pub fn new() -> Self { Self { frozen: None } }

    /// Get cached segments or freeze new ones. Returns owned Vec.
    pub fn get_or_freeze(
        &mut self,
        system_prompt: &str,
        memory_block: &str,
    ) -> Vec<CacheSegment> {
        let hash = hash_content(system_prompt, memory_block);
        if let Some(ref frozen) = self.frozen {
            if frozen.hash == hash {
                return frozen.segments.clone();
            }
        }
        let segments = build_segments(system_prompt, memory_block);
        self.frozen = Some(FrozenSystemPrompt { hash, segments: segments.clone() });
        segments
    }

    /// Force invalidation (after compression).
    pub fn invalidate(&mut self) {
        self.frozen = None;
    }
}
```

### 3b.2 Provider Gating

Only use cache segments when the provider supports caching:

```rust
// In Agent::run_conversation:
let segments = if self.provider.supports_caching() {
    Some(self.cache_manager.get_or_freeze(&full_system, &memory_block))
} else {
    None
};

let request = ChatRequest {
    system: &full_system,
    system_segments: segments.as_deref(),
    // ...
};
```

### 3b.3 Segment Layout

```
Segment 1: "base" — system prompt text — cache_control: true
Segment 2: "memory" — memory block — cache_control: true
```

---

## Phase 3c: Context Compression

### 3c.1 Configuration

```rust
// hermes-agent/src/compressor.rs
pub struct CompressionConfig {
    pub max_context_tokens: usize,       // e.g., 200_000
    pub pressure_threshold: f32,          // default: 0.50
    pub target_after_compression: f32,    // default: 0.20
    pub protect_head_messages: usize,     // default: 3
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            max_context_tokens: 200_000,
            pressure_threshold: 0.50,
            target_after_compression: 0.20,
            protect_head_messages: 3,
        }
    }
}
```

### 3c.2 Trigger

```rust
pub fn should_compress(
    &self,
    system_prompt: &str,
    messages: &[Message],
    tool_count: usize,
) -> bool {
    let total = TokenCounter::estimate_request(system_prompt, messages, tool_count);
    let threshold = (self.config.max_context_tokens as f32 * self.config.pressure_threshold) as usize;
    total >= threshold
}
```

Takes system prompt + tool count to avoid under-counting.

### 3c.3 Compression Algorithm

```rust
pub async fn compress(
    &mut self,
    messages: &mut Vec<Message>,
    provider: &dyn Provider,
    memory_contribution: Option<&str>,
) -> Result<CompressionResult>
```

**Phase 1 — Tool Result Pruning** (no LLM):
- Walk backward, skip tail protection zone
- Tool results with `content.len() > 200` → replace with `"[Previous tool output cleared]"`

**Phase 2 — Boundary Detection**:
- Head: protect first `protect_head_messages`
- Tail: walk backward accumulating tokens until `target_after_compression * max_context_tokens`
- Minimum 3 messages in tail
- **Tool pair alignment**: scan for assistant messages with non-empty `tool_calls`; find ALL subsequent `Role::Tool` messages whose `tool_call_id` matches any `ToolCall.id` in the assistant message. Keep the entire group together.

**Phase 3 — LLM Summarization**:
- Serialize compressible messages (between head and tail)
- Truncate tool results to 6000 chars, tool args to 1500 chars
- Call `provider.chat()` with structured summary template
- Summary budget: 20% of compressed content tokens, min 2000, max 12000

**Phase 4 — Rebuild**:
- `history = protected_head + [summary_message] + protected_tail`
- Summary inserted as `Message::user("<context-summary>\n{summary}\n</context-summary>")` (NOT Message::system — Anthropic drops system messages from history)

**Phase 5 — Tool Pair Sanitization** (ID-based):
```rust
fn sanitize_tool_pairs(messages: &mut Vec<Message>) {
    // 1. Collect all tool_call IDs from assistant messages
    let mut expected_ids: HashSet<String> = HashSet::new();
    for msg in messages.iter() {
        if msg.role == Role::Assistant {
            for tc in &msg.tool_calls {
                expected_ids.insert(tc.id.clone());
            }
        }
    }

    // 2. Collect all tool_call_ids from tool result messages
    let mut result_ids: HashSet<String> = HashSet::new();
    for msg in messages.iter() {
        if msg.role == Role::Tool {
            if let Some(ref id) = msg.tool_call_id {
                result_ids.insert(id.clone());
            }
        }
    }

    // 3. Remove tool results that reference non-existent calls
    messages.retain(|msg| {
        if msg.role == Role::Tool {
            msg.tool_call_id.as_ref().map_or(true, |id| expected_ids.contains(id))
        } else {
            true
        }
    });

    // 4. Add stub results for calls without results
    let missing: Vec<String> = expected_ids.difference(&result_ids).cloned().collect();
    for id in missing {
        messages.push(Message {
            role: Role::Tool,
            content: Content::Text("[Tool result removed during compression]".into()),
            tool_calls: vec![],
            reasoning: None,
            name: None,
            tool_call_id: Some(id),
        });
    }
}
```

### 3c.4 CompressionResult

```rust
pub enum CompressionResult {
    NotNeeded,
    Compressed {
        before_tokens: usize,
        after_tokens: usize,
        messages_removed: usize,
        messages_kept: usize,
    },
}
```

### 3c.5 Iterative Summary

On re-compression, the previous summary (if found as `<context-summary>` in history) is included in the prompt. The model merges new progress into the existing structure.

### 3c.6 Agent Integration

In `run_conversation`, after each tool execution round:

```rust
if self.compressor.should_compress(&full_system, history, tool_count) {
    let contrib = self.memory.on_pre_compress(history).await;
    self.compressor.compress(history, self.provider.as_ref(), contrib.as_deref()).await?;
    self.cache_manager.invalidate();
    self.memory.refresh_snapshot()?;
}
```

---

## 4. File Structure

### New files
```
crates/hermes-memory/src/builtin.rs        # BuiltinMemory
crates/hermes-memory/src/manager.rs        # MemoryManager
crates/hermes-agent/src/token_counter.rs   # Heuristic token counter
crates/hermes-agent/src/cache_manager.rs   # PromptCacheManager
crates/hermes-agent/src/compressor.rs      # ContextCompressor
```

### Modified files
```
crates/hermes-core/src/memory.rs           # trim MemoryProvider trait (add defaults)
crates/hermes-memory/src/lib.rs            # wire modules
crates/hermes-memory/Cargo.toml            # add deps
crates/hermes-agent/src/lib.rs             # add modules
crates/hermes-agent/src/loop_runner.rs     # integrate memory + caching + compression
crates/hermes-agent/Cargo.toml             # add hermes-memory dep
crates/hermes-cli/src/repl.rs              # construct MemoryManager, pass to AgentConfig
crates/hermes-cli/src/oneshot.rs           # same
```

---

## 5. Testing Strategy

### Phase 3a Tests
| Component | Scenarios |
|-----------|-----------|
| TokenCounter | text counting, message counting, request estimation, empty input |
| BuiltinMemory | load/write/read_live, snapshot freeze vs live, char limits, truncation, § parsing |
| MemoryManager | system_prompt_blocks assembly, context fencing tags |
| MemoryManager async | prefetch queue/take, sync_turn non-blocking, new_child isolation |

### Phase 3b Tests
| Component | Scenarios |
|-----------|-----------|
| PromptCacheManager | freeze/get_or_freeze/invalidate lifecycle |
| PromptCacheManager | hash stability (same input → same hash → no rebuild) |
| PromptCacheManager | invalidation forces rebuild on next call |
| Provider gating | segments=None when supports_caching()=false |

### Phase 3c Tests
| Component | Scenarios |
|-----------|-----------|
| should_compress | below/at/above threshold, with system prompt overhead |
| Tool result pruning | long results truncated, short preserved, tail protected |
| Boundary detection | head/tail protection, tool pair alignment |
| Tool pair sanitization | orphan removal, stub insertion, multi-tool parallel turns |
| Full compression | MockProvider summary, history rebuilt correctly |
| Iterative summary | existing summary merged on re-compression |

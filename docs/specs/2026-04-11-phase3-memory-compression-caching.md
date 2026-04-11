# Phase 3: Memory System, Context Compression & Prompt Caching — Design Spec

**Date**: 2026-04-11
**Status**: Draft
**Depends on**: Phase 2 (complete)
**Validates**: Async prefetch, cache mechanics, context window management

---

## 1. Scope

### In Scope
- Token counter (heuristic chars/4, swappable later)
- BuiltinMemory (file-based MEMORY.md + USER.md with frozen snapshot)
- MemoryManager (orchestrator: builtin + optional external, prefetch cache, async sync)
- ContextCompressor (layered: tool pruning → boundary detection → LLM summarization → tool pair fix)
- PromptCacheManager (frozen system prompt, cache segments for Anthropic)
- Agent loop integration (compression check, cache management, memory prefetch/sync lifecycle)

### Out of Scope (deferred)
- External memory providers (Honcho, etc.) — Phase 4+
- Memory tools (memory_read/write as agent-callable tools) — Phase 4
- HuggingFace tokenizers crate (precise counting) — Phase 4+
- ModelRouter / summary provider switching — Phase 4+
- SOUL.md persona injection — Phase 4

---

## 2. Token Counter

Heuristic estimation, designed for easy replacement later.

```rust
// hermes-agent/src/token_counter.rs
pub struct TokenCounter;

impl TokenCounter {
    /// Estimate token count for text. ~4 chars per token.
    pub fn count_text(text: &str) -> usize {
        (text.len() + 3) / 4
    }

    /// Estimate tokens for a single message (content + tool_calls + reasoning + overhead).
    pub fn count_message(msg: &Message) -> usize {
        let mut count = 4; // per-message overhead (role, formatting)
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

    /// Estimate tokens for a message sequence.
    pub fn count_messages(msgs: &[Message]) -> usize {
        msgs.iter().map(Self::count_message).sum()
    }
}
```

---

## 3. BuiltinMemory

File-backed memory with two stores: MEMORY.md (agent notes) and USER.md (user profile).

### Storage Format

```
~/.hermes/memories/
├── MEMORY.md   # agent notes, § separated entries, max 2200 chars
└── USER.md     # user profile, § separated entries, max 1375 chars
```

Entry format within each file:
```
First entry content here
§
Second entry content here
§
Third entry content here
```

### Frozen Snapshot Pattern

- **Session start**: `load()` reads files, stores snapshot for system prompt
- **Writes**: `write()` updates files on disk immediately (durable)
- **System prompt**: `system_prompt_block()` returns the frozen snapshot (not live disk state)
- **Rationale**: Changing the system prompt mid-session invalidates prompt cache. By freezing, we keep cache hits for the entire session until compression forces a rebuild.

### API

```rust
// hermes-memory/src/builtin.rs
#[derive(Clone)]
pub struct BuiltinMemory {
    dir: PathBuf,
    memory_snapshot: Arc<Mutex<String>>,
    user_snapshot: Arc<Mutex<String>>,
}

impl BuiltinMemory {
    pub fn load(dir: PathBuf) -> Result<Self>;
    pub fn system_prompt_block(&self) -> Option<String>;
    pub fn read(&self, key: &str) -> Result<Option<String>>;  // live from disk
    pub fn write(&self, key: &str, content: &str) -> Result<()>;  // write to disk only
    pub fn refresh_snapshot(&self) -> Result<()>;  // re-read files into snapshot
}
```

### Character Limits

- MEMORY.md: 2200 chars max
- USER.md: 1375 chars max
- On write exceeding limit: truncate oldest entries (from top)

---

## 4. MemoryManager

Orchestrator that manages builtin memory + optional external provider.

```rust
// hermes-memory/src/manager.rs
pub struct MemoryManager {
    builtin: BuiltinMemory,
    external: Option<Arc<dyn MemoryProvider>>,
    prefetch_cache: Arc<Mutex<HashMap<String, String>>>,
}
```

### Lifecycle

```
Session start:
  builtin.load() → snapshot frozen
  external.prefetch("", session_id) → warm up

Turn start:
  take_prefetched(session_id) → cached result or sync fallback

Turn end:
  sync_turn(user, assistant, session_id) → tokio::spawn non-blocking
  queue_prefetch(hint, session_id) → tokio::spawn background

Compression:
  on_pre_compress(messages) → providers contribute context
  builtin.refresh_snapshot() → update frozen snapshot post-compression

Session end:
  external.shutdown()
```

### Methods

```rust
impl MemoryManager {
    pub fn new(dir: PathBuf, external: Option<Arc<dyn MemoryProvider>>) -> Result<Self>;

    /// Aggregate all provider system prompt blocks, wrapped in <memory-context> tags.
    pub fn system_prompt_blocks(&self) -> String;

    /// Spawn background prefetch task. Result stored in cache.
    pub fn queue_prefetch(&self, hint: &str, session_id: &str);

    /// Take cached prefetch result (O(1)), or sync fallback if cache miss.
    pub async fn take_prefetched(&self, session_id: &str) -> Option<String>;

    /// Non-blocking turn-end sync. Spawns tokio task.
    pub fn sync_turn(&self, user: &str, assistant: &str, session_id: &str);

    /// Called before compression. Returns provider contributions.
    pub async fn on_pre_compress(&self, messages: &[Message]) -> Option<String>;

    /// Refresh builtin snapshot (after compression invalidates cache).
    pub fn refresh_snapshot(&self) -> Result<()>;

    /// Create child for subagent (independent cache, no external writes).
    pub fn new_child(&self) -> Self;
}
```

### Context Fencing

System prompt blocks are wrapped to prevent injection:
```
<memory-context>
## Notes
{MEMORY.md content}

## User Profile
{USER.md content}
</memory-context>
```

---

## 5. ContextCompressor

Layered compression strategy that reduces conversation history while preserving key context.

### Configuration

```rust
// hermes-agent/src/compressor.rs
pub struct CompressionConfig {
    pub max_context_tokens: usize,      // e.g., 200_000
    pub pressure_threshold: f32,         // default: 0.50
    pub target_after_compression: f32,   // default: 0.20
    pub protect_head_messages: usize,    // default: 3
    pub summary_max_tokens: usize,       // default: 12_000
}
```

### Trigger

```rust
impl ContextCompressor {
    pub fn should_compress(&self, messages: &[Message]) -> bool {
        let token_count = TokenCounter::count_messages(messages);
        let threshold = (self.config.max_context_tokens as f32 * self.config.pressure_threshold) as usize;
        token_count >= threshold
    }
}
```

### Compression Algorithm

```rust
pub async fn compress(
    &mut self,
    messages: &mut Vec<Message>,
    provider: &dyn Provider,
    memory_contribution: Option<&str>,
) -> Result<CompressionResult>
```

**Phase 1 — Tool Result Pruning** (no LLM call):
- Walk backward from end
- Old tool results with content >200 chars → replace with `"[Previous tool output cleared]"`
- Stop when within tail protection zone

**Phase 2 — Boundary Detection**:
- Protect head: first `protect_head_messages` messages
- Protect tail: walk backward accumulating tokens until `target_after_compression * max_context_tokens` budget
- Minimum: always keep at least 3 messages in tail
- Align to tool pairs: never split assistant(tool_calls) from its tool results

**Phase 3 — LLM Summarization**:
- Serialize compressible messages (between head and tail) into a structured prompt
- Truncate tool results to 6000 chars, tool arguments to 1500 chars
- Call provider.chat() with summary template
- Summary budget: 20% of compressed content tokens, min 2000, max 12000

**Summary template**:
```
Summarize this conversation context. Preserve:
- Goal: What the user is trying to accomplish
- Progress: What's been done, in progress, blocked
- Key Decisions: Technical choices and rationale
- Relevant Files: Paths mentioned with context
- Next Steps: What to do next
- Critical Context: Specific values, error messages, config

{previous_summary if exists}
{memory_contribution if exists}

--- MESSAGES TO COMPRESS ---
{serialized messages}
```

**Phase 4 — Rebuild History**:
- `history = [summary_message] + protected_tail_messages`
- Summary inserted as `Message::system("## Previous Context Summary\n{summary}")`

**Phase 5 — Tool Pair Sanitization**:
- Remove tool results that reference deleted tool_calls
- Add stub results for tool_calls whose results were dropped
- Prevents API rejection from orphaned references

### CompressionResult

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

### Iterative Summaries

On re-compression, the existing summary is included in the prompt as "previous context". The model merges new progress into the existing structure rather than regenerating from scratch. This preserves information across multiple compressions.

---

## 6. PromptCacheManager

Manages frozen system prompt for Anthropic prompt caching.

### Core Idea

Anthropic caches the system prompt prefix. If it stays identical between requests, subsequent calls get 90% token discount. Any change → cache miss → full price.

### API

```rust
// hermes-agent/src/cache_manager.rs
pub struct PromptCacheManager {
    frozen: Option<FrozenSystemPrompt>,
}

struct FrozenSystemPrompt {
    content: String,
    hash: u64,
    segments: Vec<CacheSegment>,
}

impl PromptCacheManager {
    pub fn new() -> Self;

    /// Freeze the system prompt on first call. Returns segments with cache_control.
    pub fn freeze(&mut self, system_prompt: &str, memory_block: &str) -> &[CacheSegment];

    /// Get existing frozen prompt, or rebuild if hash changed.
    pub fn get_or_rebuild(
        &mut self,
        system_prompt: &str,
        memory_block: &str,
    ) -> &[CacheSegment];

    /// Force invalidation (after compression). Next get_or_rebuild will re-freeze.
    pub fn invalidate(&mut self);

    /// Whether the prompt is currently frozen (cache active).
    pub fn is_frozen(&self) -> bool;
}
```

`CacheSegment` is already defined in `hermes-core/src/provider.rs`:
```rust
pub struct CacheSegment {
    pub text: String,
    pub label: &'static str,
    pub cache_control: bool,
}
```

### Segment Layout

```
Segment 1: base_instructions (system prompt) — cache_control: true
Segment 2: memory (frozen memory block) — cache_control: true
```

### Lifecycle

```
Turn 1: freeze(system, memory) → hash=A → segments with cache_control
Turn 2-N: get_or_rebuild() → hash unchanged → return same segments (CACHE HIT)
Memory write: disk only, no invalidation → CACHE HIT continues
Compression: invalidate() → hash cleared
Turn K+1: get_or_rebuild() → re-freeze → hash=B → new segments (CACHE MISS)
Turn K+2: hash=B unchanged → CACHE HIT again
```

---

## 7. Agent Loop Integration

### Modified run_conversation Flow

```
pub async fn run_conversation(...) -> Result<String> {
    // 1. Take prefetched memory
    let memory_ctx = self.memory.take_prefetched(&self.session_id).await;

    // 2. Build/reuse frozen system prompt
    let memory_block = self.memory.system_prompt_blocks();
    let segments = self.cache_manager.get_or_rebuild(&self.system_prompt, &memory_block);

    // 3. Main loop
    while self.budget.try_consume() {
        let request = ChatRequest {
            system: &self.system_prompt,
            system_segments: Some(segments),
            messages: history,
            // ...
        };

        let response = self.provider.chat(&request, Some(&delta_tx)).await?;
        // ... handle response, tool calls ...

        // 4. Compression check after each iteration
        if self.compressor.should_compress(history) {
            let memory_contrib = self.memory.on_pre_compress(history).await;
            self.compressor.compress(history, self.provider.as_ref(), memory_contrib.as_deref()).await?;
            self.cache_manager.invalidate();
            self.memory.refresh_snapshot()?;
            // Re-freeze on next iteration via get_or_rebuild
            segments = self.cache_manager.get_or_rebuild(&self.system_prompt, &self.memory.system_prompt_blocks());
        }
    }

    // 5. Turn-end memory lifecycle
    self.memory.sync_turn(user_message, &final_response, &self.session_id);
    self.memory.queue_prefetch(&final_response, &self.session_id);

    Ok(final_response)
}
```

### AgentConfig Changes

```rust
pub struct AgentConfig {
    // ... existing fields ...
    pub memory: MemoryManager,           // NEW
    pub compression: CompressionConfig,  // NEW
}
```

The Agent owns: MemoryManager, ContextCompressor, PromptCacheManager.

---

## 8. File Structure

### New files
```
crates/hermes-memory/src/builtin.rs        # BuiltinMemory (file-backed)
crates/hermes-memory/src/manager.rs        # MemoryManager (orchestrator)
crates/hermes-agent/src/token_counter.rs   # Heuristic token counter
crates/hermes-agent/src/compressor.rs      # ContextCompressor
crates/hermes-agent/src/cache_manager.rs   # PromptCacheManager
```

### Modified files
```
crates/hermes-memory/src/lib.rs            # wire up modules
crates/hermes-memory/Cargo.toml            # add deps
crates/hermes-agent/src/lib.rs             # add modules
crates/hermes-agent/src/loop_runner.rs     # integrate memory + compression + caching
crates/hermes-agent/Cargo.toml             # add hermes-memory dep
crates/hermes-cli/src/repl.rs              # construct MemoryManager, pass to AgentConfig
crates/hermes-cli/src/oneshot.rs           # same
```

### Dependency additions
```
hermes-agent → hermes-memory (NEW)
hermes-memory → hermes-core (existing)
```

---

## 9. Testing Strategy

| Component | Test Type | Key Scenarios |
|-----------|-----------|---------------|
| TokenCounter | Unit | text counting, message counting, empty input |
| BuiltinMemory | Unit | load/write/read, snapshot freezing, char limits, truncation |
| BuiltinMemory | Unit | injection pattern detection |
| MemoryManager | Unit | system_prompt_blocks assembly, context fencing |
| MemoryManager | Async | prefetch queue/take, sync_turn non-blocking, new_child isolation |
| ContextCompressor | Unit | should_compress threshold, tool result pruning |
| ContextCompressor | Unit | boundary detection (head/tail protection, tool pair alignment) |
| ContextCompressor | Integration | full compression with MockProvider for summary |
| ContextCompressor | Unit | tool pair sanitization (orphan removal, stub insertion) |
| PromptCacheManager | Unit | freeze/get_or_rebuild/invalidate lifecycle, hash stability |
| Agent integration | Integration | multi-turn with compression trigger, memory prefetch |

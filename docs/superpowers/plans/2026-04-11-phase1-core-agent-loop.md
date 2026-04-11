# Phase 1: Core Agent Loop Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a working agent that can hold a multi-turn conversation with an LLM, execute tools, and render streaming output in a CLI REPL.

**Architecture:** Core types are already scaffolded in `hermes-core`. We build upward: tool registry with `inventory` compile-time registration, provider implementations (OpenAI-compatible + Anthropic) with SSE streaming, agent loop with iteration budget and parallel tool execution, and a minimal CLI REPL using rustyline + crossterm. Each layer only depends on layers below it.

**Tech Stack:** Rust 1.85+, tokio, reqwest, serde, inventory, rustyline, crossterm, thiserror/anyhow

---

## File Structure

### hermes-core (existing, needs test coverage + minor additions)
```
crates/hermes-core/
├── src/
│   ├── lib.rs           # module exports (exists)
│   ├── message.rs       # Message, Role, Content, ToolCall, ToolResult (exists)
│   ├── provider.rs      # Provider trait, ChatRequest/Response, ModelInfo (exists)
│   ├── tool.rs          # Tool trait, ToolSchema, ToolContext, ApprovalRequest (exists)
│   ├── error.rs         # HermesError, ProviderError (exists)
│   ├── stream.rs        # StreamDelta (exists)
│   ├── memory.rs        # MemoryProvider trait (exists)
│   └── platform.rs      # PlatformAdapter trait (exists)
└── tests/
    └── message_test.rs  # CREATE: serde roundtrip, constructors, Content methods
```

### hermes-tools
```
crates/hermes-tools/src/
├── lib.rs               # MODIFY: ToolRegistration, ToolRegistry, re-exports
└── registry.rs          # CREATE: ToolRegistry impl, dispatch, schema retrieval
```

### hermes-provider
```
crates/hermes-provider/src/
├── lib.rs               # MODIFY: module exports, create_provider factory
├── sse.rs               # CREATE: SSE stream parser (shared by OpenAI + Anthropic)
├── tool_assembler.rs    # CREATE: streaming tool call JSON assembler
├── retry.rs             # CREATE: retry policy with exponential backoff + jitter
├── openai.rs            # CREATE: OpenAI-compatible provider (chat/completions)
└── anthropic.rs         # CREATE: Anthropic provider (messages API)
```

### hermes-agent
```
crates/hermes-agent/src/
├── lib.rs               # MODIFY: module exports, re-exports
├── budget.rs            # CREATE: IterationBudget
├── loop_runner.rs       # CREATE: Agent struct, run_conversation, tool dispatch
└── parallel.rs          # CREATE: should_parallelize, execute_parallel/sequential
```

### hermes-config (minimal for Phase 1)
```
crates/hermes-config/src/
├── lib.rs               # MODIFY: module exports
└── config.rs            # CREATE: AppConfig, ModelConfig, hermes_home()
```

### hermes-cli
```
crates/hermes-cli/src/
├── main.rs              # MODIFY: clap args, tokio main, REPL entry
├── repl.rs              # CREATE: REPL loop, input handling, command dispatch
└── render.rs            # CREATE: streaming output renderer (crossterm colors)
```

---

## Task 1: hermes-core Test Coverage

Add tests for existing scaffolded types to verify serde behavior and constructor ergonomics. This locks down the API before other crates build on it.

**Files:**
- Create: `crates/hermes-core/tests/message_test.rs`
- Modify: `crates/hermes-core/Cargo.toml` (add dev-dependencies)

- [ ] **Step 1: Add dev-dependencies to hermes-core**

Add tokio dev-dependency for async test support:

```toml
# append to crates/hermes-core/Cargo.toml
[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt"] }
```

- [ ] **Step 2: Write Message constructor and serde tests**

Create `crates/hermes-core/tests/message_test.rs`:

```rust
use hermes_core::message::*;

#[test]
fn test_user_message_constructor() {
    let msg = Message::user("hello");
    assert_eq!(msg.role, Role::User);
    assert_eq!(msg.content.as_text(), Some("hello"));
    assert!(msg.tool_calls.is_empty());
    assert!(msg.reasoning.is_none());
    assert!(msg.name.is_none());
    assert!(msg.tool_call_id.is_none());
}

#[test]
fn test_assistant_message_constructor() {
    let msg = Message::assistant("response");
    assert_eq!(msg.role, Role::Assistant);
    assert_eq!(msg.content.as_text(), Some("response"));
}

#[test]
fn test_system_message_constructor() {
    let msg = Message::system("you are helpful");
    assert_eq!(msg.role, Role::System);
    assert_eq!(msg.content.as_text(), Some("you are helpful"));
}

#[test]
fn test_content_text_serde_roundtrip() {
    let content = Content::Text("hello world".into());
    let json = serde_json::to_string(&content).unwrap();
    let back: Content = serde_json::from_str(&json).unwrap();
    assert_eq!(back.as_text(), Some("hello world"));
}

#[test]
fn test_content_parts_serde_roundtrip() {
    let content = Content::Parts(vec![
        ContentPart::Text { text: "describe this".into() },
        ContentPart::Image {
            data: "base64data".into(),
            media_type: "image/png".into(),
        },
    ]);
    let json = serde_json::to_string(&content).unwrap();
    let back: Content = serde_json::from_str(&json).unwrap();
    assert_eq!(back.as_text_lossy(), "describe this");
}

#[test]
fn test_content_as_text_lossy_concatenates_text_parts() {
    let content = Content::Parts(vec![
        ContentPart::Text { text: "hello ".into() },
        ContentPart::Image {
            data: "img".into(),
            media_type: "image/png".into(),
        },
        ContentPart::Text { text: "world".into() },
    ]);
    assert_eq!(content.as_text_lossy(), "hello world");
}

#[test]
fn test_message_serde_roundtrip() {
    let msg = Message {
        role: Role::Assistant,
        content: Content::Text("I'll help".into()),
        tool_calls: vec![ToolCall {
            id: "call_1".into(),
            name: "web_search".into(),
            arguments: serde_json::json!({"query": "rust"}),
        }],
        reasoning: Some("thinking...".into()),
        name: None,
        tool_call_id: None,
    };
    let json = serde_json::to_string(&msg).unwrap();
    let back: Message = serde_json::from_str(&json).unwrap();
    assert_eq!(back.role, Role::Assistant);
    assert_eq!(back.tool_calls.len(), 1);
    assert_eq!(back.tool_calls[0].name, "web_search");
    assert_eq!(back.reasoning, Some("thinking...".into()));
}

#[test]
fn test_message_serde_skips_none_fields() {
    let msg = Message::user("hi");
    let json = serde_json::to_string(&msg).unwrap();
    let val: serde_json::Value = serde_json::from_str(&json).unwrap();
    // Optional None fields should be omitted
    assert!(val.get("reasoning").is_none());
    assert!(val.get("name").is_none());
    assert!(val.get("tool_call_id").is_none());
}

#[test]
fn test_role_serde_lowercase() {
    let json = serde_json::to_string(&Role::Assistant).unwrap();
    assert_eq!(json, r#""assistant""#);
    let back: Role = serde_json::from_str(r#""tool""#).unwrap();
    assert_eq!(back, Role::Tool);
}

#[test]
fn test_tool_result_constructors() {
    let ok = ToolResult::ok("success");
    assert_eq!(ok.content, "success");
    assert!(!ok.is_error);

    let err = ToolResult::error("failed");
    assert_eq!(err.content, "failed");
    assert!(err.is_error);
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p hermes-core --test message_test -- --nocapture`

Expected: All 10 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/hermes-core/Cargo.toml crates/hermes-core/tests/message_test.rs
git commit -m "test: add hermes-core message serde and constructor tests"
```

---

## Task 2: Tool Registry with inventory

Implement the compile-time tool registration system using the `inventory` crate and a runtime `ToolRegistry` that collects registered tools and provides schema retrieval + dispatch.

**Files:**
- Create: `crates/hermes-tools/src/registry.rs`
- Modify: `crates/hermes-tools/src/lib.rs`
- Modify: `crates/hermes-tools/Cargo.toml` (add dev-dependencies)

- [ ] **Step 1: Write failing tests for ToolRegistry**

First add dev-dependencies to `crates/hermes-tools/Cargo.toml`:

```toml
# append to crates/hermes-tools/Cargo.toml
[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

Create `crates/hermes-tools/src/registry.rs` with tests at the bottom, implementation stubs that won't compile yet:

```rust
use std::collections::HashMap;

use hermes_core::error::Result;
use hermes_core::message::ToolResult;
use hermes_core::tool::{Tool, ToolContext, ToolSchema};

/// Compile-time tool registration entry.
/// Tools register themselves with `inventory::submit!`.
pub struct ToolRegistration {
    pub factory: fn() -> Box<dyn Tool>,
}

inventory::collect!(ToolRegistration);

/// Runtime tool registry. Collects all `inventory`-registered tools
/// and provides lookup by name.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    /// Build registry from all compile-time registered tools.
    /// Only includes tools where `is_available()` returns true.
    pub fn from_inventory() -> Self {
        let mut tools = HashMap::new();
        for reg in inventory::iter::<ToolRegistration> {
            let tool = (reg.factory)();
            if tool.is_available() {
                tools.insert(tool.name().to_string(), tool);
            }
        }
        Self { tools }
    }

    /// Create an empty registry (useful for tests).
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Manually register a tool (for MCP tools or testing).
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Get a tool by name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|b| b.as_ref())
    }

    /// Get all available tool schemas (for LLM API requests).
    pub fn available_schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|t| t.schema()).collect()
    }

    /// List all registered tool names.
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;

    /// A test tool that always succeeds.
    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str { "echo" }
        fn toolset(&self) -> &str { "test" }
        fn is_read_only(&self) -> bool { true }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "echo".into(),
                description: "Echoes the input".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"}
                    },
                    "required": ["text"]
                }),
            }
        }
        async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
            let text = args.get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("(empty)");
            Ok(ToolResult::ok(text))
        }
    }

    /// A tool that reports itself as unavailable.
    struct UnavailableTool;

    #[async_trait]
    impl Tool for UnavailableTool {
        fn name(&self) -> &str { "unavailable" }
        fn toolset(&self) -> &str { "test" }
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "unavailable".into(),
                description: "Never available".into(),
                parameters: json!({"type": "object"}),
            }
        }
        fn is_available(&self) -> bool { false }
        async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
            unreachable!()
        }
    }

    #[test]
    fn test_empty_registry() {
        let reg = ToolRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(reg.get("anything").is_none());
        assert!(reg.available_schemas().is_empty());
    }

    #[test]
    fn test_manual_register_and_lookup() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(EchoTool));

        assert_eq!(reg.len(), 1);
        assert!(reg.get("echo").is_some());
        assert!(reg.get("nonexistent").is_none());
        assert_eq!(reg.get("echo").unwrap().name(), "echo");
    }

    #[test]
    fn test_available_schemas() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(EchoTool));

        let schemas = reg.available_schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "echo");
        assert_eq!(schemas[0].description, "Echoes the input");
    }

    #[test]
    fn test_tool_names() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(EchoTool));
        let names = reg.tool_names();
        assert_eq!(names, vec!["echo"]);
    }

    #[tokio::test]
    async fn test_tool_execute() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(EchoTool));

        let tool = reg.get("echo").unwrap();

        // Create a minimal ToolContext for testing
        let (approval_tx, _approval_rx) = tokio::sync::mpsc::channel(1);
        let (delta_tx, _delta_rx) = tokio::sync::mpsc::channel(1);
        let ctx = ToolContext {
            session_id: "test-session".into(),
            working_dir: std::path::PathBuf::from("/tmp"),
            approval_tx,
            delta_tx,
        };

        let result = tool.execute(json!({"text": "hello"}), &ctx).await.unwrap();
        assert_eq!(result.content, "hello");
        assert!(!result.is_error);
    }

    #[test]
    fn test_from_inventory_runs_without_panic() {
        // With no tools registered via inventory::submit!, this should produce an empty registry
        // (or whatever tools other tests/crates have registered — but in unit test binary, likely empty)
        let reg = ToolRegistry::from_inventory();
        // Just verify it doesn't panic
        let _ = reg.len();
    }
}
```

- [ ] **Step 2: Wire up lib.rs**

Replace `crates/hermes-tools/src/lib.rs`:

```rust
pub mod registry;

pub use registry::{ToolRegistration, ToolRegistry};
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p hermes-tools -- --nocapture`

Expected: All 6 tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/hermes-tools/src/lib.rs crates/hermes-tools/src/registry.rs crates/hermes-tools/Cargo.toml
git commit -m "feat: implement ToolRegistry with inventory compile-time registration"
```

---

## Task 3: SSE Stream Parser

Shared SSE (Server-Sent Events) parser used by both OpenAI and Anthropic providers. Parses `event:`, `data:`, and blank-line event boundaries from an HTTP streaming response.

**Files:**
- Create: `crates/hermes-provider/src/sse.rs`
- Modify: `crates/hermes-provider/src/lib.rs`

- [ ] **Step 1: Write SSE parser with tests**

Create `crates/hermes-provider/src/sse.rs`:

```rust
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_stream::Stream;
use tokio_util::io::StreamReader;

/// A parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Async SSE event stream built from an HTTP response body.
pub struct SseStream<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    reader: BufReader<StreamReader<MapStream<S>, io::Error>>,
    current_event: Option<String>,
    data_buf: String,
}

/// Wrapper to map reqwest::Error to io::Error for StreamReader.
pub struct MapStream<S>(pub S);

impl<S> Stream for MapStream<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.0).poll_next(cx).map(|opt| {
            opt.map(|res| res.map_err(|e| io::Error::new(io::ErrorKind::Other, e)))
        })
    }
}

impl<S> SseStream<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    pub fn new(byte_stream: S) -> Self {
        let mapped = MapStream(byte_stream);
        let reader = BufReader::new(StreamReader::new(mapped));
        Self {
            reader,
            current_event: None,
            data_buf: String::new(),
        }
    }

    /// Read the next SSE event. Returns `None` on EOF or `data: [DONE]`.
    pub async fn next_event(&mut self) -> io::Result<Option<SseEvent>> {
        let mut line = String::new();

        loop {
            line.clear();
            let bytes_read = self.reader.read_line(&mut line).await?;

            if bytes_read == 0 {
                // EOF
                if !self.data_buf.is_empty() {
                    return Ok(Some(self.emit_event()));
                }
                return Ok(None);
            }

            let trimmed = line.trim_end_matches(['\r', '\n']);

            if trimmed.is_empty() {
                // Blank line = event boundary
                if !self.data_buf.is_empty() {
                    return Ok(Some(self.emit_event()));
                }
                continue;
            }

            if let Some(value) = trimmed.strip_prefix("event:") {
                self.current_event = Some(value.trim().to_string());
            } else if let Some(value) = trimmed.strip_prefix("data:") {
                let value = value.trim_start();
                if value == "[DONE]" {
                    return Ok(None);
                }
                if !self.data_buf.is_empty() {
                    self.data_buf.push('\n');
                }
                self.data_buf.push_str(value);
            }
            // Ignore `id:`, `retry:`, and comment lines (`:`)
        }
    }

    fn emit_event(&mut self) -> SseEvent {
        SseEvent {
            event: self.current_event.take(),
            data: std::mem::take(&mut self.data_buf),
        }
    }
}

/// Parse SSE events from raw bytes (useful for testing without HTTP).
pub fn parse_sse_events(raw: &[u8]) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let mut current_event: Option<String> = None;
    let mut data_buf = String::new();

    for line in raw.split(|&b| b == b'\n') {
        let line = std::str::from_utf8(line).unwrap_or("").trim_end_matches('\r');

        if line.is_empty() {
            if !data_buf.is_empty() {
                events.push(SseEvent {
                    event: current_event.take(),
                    data: std::mem::take(&mut data_buf),
                });
            }
            continue;
        }

        if let Some(value) = line.strip_prefix("event:") {
            current_event = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            let value = value.trim_start();
            if value == "[DONE]" {
                break;
            }
            if !data_buf.is_empty() {
                data_buf.push('\n');
            }
            data_buf.push_str(value);
        }
    }

    if !data_buf.is_empty() {
        events.push(SseEvent {
            event: current_event.take(),
            data: data_buf,
        });
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_data_events() {
        let raw = b"data: hello\n\ndata: world\n\n";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "hello");
        assert_eq!(events[0].event, None);
        assert_eq!(events[1].data, "world");
    }

    #[test]
    fn test_parse_event_with_type() {
        let raw = b"event: message_start\ndata: {\"type\":\"message\"}\n\n";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, Some("message_start".into()));
        assert_eq!(events[0].data, r#"{"type":"message"}"#);
    }

    #[test]
    fn test_parse_multiline_data() {
        let raw = b"data: line1\ndata: line2\n\n";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn test_parse_done_terminates() {
        let raw = b"data: hello\n\ndata: [DONE]\n\ndata: ignored\n\n";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn test_parse_skips_comments_and_empty() {
        let raw = b": comment\n\ndata: real\n\n";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn test_parse_multiple_events_with_types() {
        let raw = b"event: content_block_start\ndata: {\"index\":0}\n\nevent: content_block_delta\ndata: {\"delta\":\"Hi\"}\n\nevent: message_stop\ndata: {}\n\n";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event, Some("content_block_start".into()));
        assert_eq!(events[1].event, Some("content_block_delta".into()));
        assert_eq!(events[2].event, Some("message_stop".into()));
    }

    #[test]
    fn test_parse_empty_input() {
        let events = parse_sse_events(b"");
        assert!(events.is_empty());
    }

    #[test]
    fn test_parse_no_trailing_newline() {
        let raw = b"data: trailing";
        let events = parse_sse_events(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "trailing");
    }
}
```

- [ ] **Step 2: Add required dependencies to hermes-provider Cargo.toml**

Add `bytes`, `tokio-stream`, `tokio-util` to `crates/hermes-provider/Cargo.toml` dependencies:

```toml
# add to [dependencies] section
bytes = "1"
tokio-stream = "0.1"
tokio-util = { version = "0.7", features = ["io"] }
```

And add dev-dependencies:

```toml
[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

Also add these to `[workspace.dependencies]` in root `Cargo.toml`:

```toml
bytes = "1"
tokio-stream = "0.1"
tokio-util = { version = "0.7", features = ["io"] }
```

- [ ] **Step 3: Wire up lib.rs**

Replace `crates/hermes-provider/src/lib.rs`:

```rust
pub mod sse;
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p hermes-provider -- --nocapture`

Expected: All 8 SSE tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/hermes-provider/src/sse.rs crates/hermes-provider/src/lib.rs crates/hermes-provider/Cargo.toml Cargo.toml
git commit -m "feat: implement SSE stream parser for LLM provider responses"
```

---

## Task 4: Tool Call Assembler

Streaming tool call arguments arrive as JSON fragments. The assembler buffers them per tool call index and produces complete `ToolCall` structs when done.

**Files:**
- Create: `crates/hermes-provider/src/tool_assembler.rs`
- Modify: `crates/hermes-provider/src/lib.rs`

- [ ] **Step 1: Write tool call assembler with tests**

Create `crates/hermes-provider/src/tool_assembler.rs`:

```rust
use std::collections::HashMap;

use hermes_core::message::ToolCall;

/// Accumulates streaming tool call fragments into complete ToolCall structs.
///
/// During SSE streaming, tool call arguments arrive as small JSON fragments
/// (e.g., `{"qu`, then `ery":`, then `"rust"}`). This struct buffers them
/// per tool call index and produces complete ToolCalls when `finish()` is called.
pub struct ToolCallAssembler {
    pending: HashMap<usize, PendingToolCall>,
}

struct PendingToolCall {
    id: String,
    name: String,
    arguments_buf: String,
}

impl ToolCallAssembler {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }

    /// Start tracking a new tool call at the given stream index.
    pub fn start(&mut self, index: usize, id: String, name: String) {
        self.pending.insert(
            index,
            PendingToolCall {
                id,
                name,
                arguments_buf: String::new(),
            },
        );
    }

    /// Append a JSON fragment to the tool call at the given index.
    pub fn append_arguments(&mut self, index: usize, delta: &str) {
        if let Some(pending) = self.pending.get_mut(&index) {
            pending.arguments_buf.push_str(delta);
        }
    }

    /// Consume the assembler and produce complete ToolCall structs.
    /// Tool calls are sorted by index to preserve the original order.
    /// Invalid JSON arguments are wrapped in a JSON object with an "error" key.
    pub fn finish(self) -> Vec<ToolCall> {
        let mut entries: Vec<(usize, PendingToolCall)> = self.pending.into_iter().collect();
        entries.sort_by_key(|(idx, _)| *idx);

        entries
            .into_iter()
            .map(|(_, p)| {
                let arguments = serde_json::from_str(&p.arguments_buf).unwrap_or_else(|_| {
                    serde_json::json!({
                        "_raw": p.arguments_buf,
                        "_error": "invalid JSON in tool call arguments"
                    })
                });
                ToolCall {
                    id: p.id,
                    name: p.name,
                    arguments,
                }
            })
            .collect()
    }

    /// Whether any tool calls are being assembled.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

impl Default for ToolCallAssembler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_single_tool_call_assembled() {
        let mut asm = ToolCallAssembler::new();
        asm.start(0, "call_1".into(), "web_search".into());
        asm.append_arguments(0, r#"{"qu"#);
        asm.append_arguments(0, r#"ery": "rust"}"#);

        let calls = asm.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].arguments, json!({"query": "rust"}));
    }

    #[test]
    fn test_multiple_tool_calls_sorted_by_index() {
        let mut asm = ToolCallAssembler::new();
        // Insert out of order
        asm.start(1, "call_2".into(), "read_file".into());
        asm.start(0, "call_1".into(), "web_search".into());
        asm.append_arguments(0, r#"{"query": "rust"}"#);
        asm.append_arguments(1, r#"{"path": "/tmp/f.txt"}"#);

        let calls = asm.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[1].name, "read_file");
    }

    #[test]
    fn test_invalid_json_produces_error_wrapper() {
        let mut asm = ToolCallAssembler::new();
        asm.start(0, "call_1".into(), "broken".into());
        asm.append_arguments(0, r#"{"truncated"#);

        let calls = asm.finish();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].arguments.get("_error").is_some());
        assert_eq!(
            calls[0].arguments.get("_raw").unwrap().as_str().unwrap(),
            r#"{"truncated"#
        );
    }

    #[test]
    fn test_empty_assembler() {
        let asm = ToolCallAssembler::new();
        assert!(!asm.has_pending());
        let calls = asm.finish();
        assert!(calls.is_empty());
    }

    #[test]
    fn test_has_pending() {
        let mut asm = ToolCallAssembler::new();
        assert!(!asm.has_pending());
        asm.start(0, "call_1".into(), "test".into());
        assert!(asm.has_pending());
    }

    #[test]
    fn test_append_to_nonexistent_index_is_noop() {
        let mut asm = ToolCallAssembler::new();
        asm.append_arguments(99, "data");
        let calls = asm.finish();
        assert!(calls.is_empty());
    }

    #[test]
    fn test_empty_arguments() {
        let mut asm = ToolCallAssembler::new();
        asm.start(0, "call_1".into(), "no_args".into());
        // No append_arguments calls — empty string
        let calls = asm.finish();
        assert_eq!(calls.len(), 1);
        // Empty string is not valid JSON, so it should produce an error wrapper
        assert!(calls[0].arguments.get("_error").is_some());
    }
}
```

- [ ] **Step 2: Add to lib.rs**

Append to `crates/hermes-provider/src/lib.rs`:

```rust
pub mod sse;
pub mod tool_assembler;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p hermes-provider -- --nocapture`

Expected: All SSE + assembler tests PASS (15 total).

- [ ] **Step 4: Commit**

```bash
git add crates/hermes-provider/src/tool_assembler.rs crates/hermes-provider/src/lib.rs
git commit -m "feat: implement streaming tool call assembler"
```

---

## Task 5: Retry Policy

Exponential backoff with jitter for retryable HTTP errors. Used by both OpenAI and Anthropic providers.

**Files:**
- Create: `crates/hermes-provider/src/retry.rs`
- Modify: `crates/hermes-provider/src/lib.rs`

- [ ] **Step 1: Write retry policy with tests**

Create `crates/hermes-provider/src/retry.rs`:

```rust
use std::time::Duration;

use hermes_core::error::{HermesError, ProviderError};

/// Retry policy with exponential backoff and jitter.
pub struct RetryPolicy {
    pub max_retries: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// Classification of an HTTP error for retry decisions.
#[derive(Debug, PartialEq, Eq)]
pub enum RetryAction {
    /// Retry after the given delay.
    RetryAfter(Duration),
    /// Do not retry this error.
    DoNotRetry,
}

impl RetryPolicy {
    /// Determine if and how to retry based on the error and attempt number.
    /// `attempt` is 0-indexed (first retry = attempt 0).
    pub fn should_retry(
        &self,
        error: &HermesError,
        attempt: u32,
        retry_after_header: Option<f64>,
    ) -> RetryAction {
        if attempt >= self.max_retries {
            return RetryAction::DoNotRetry;
        }

        match error {
            HermesError::Provider(provider_err) => match provider_err {
                ProviderError::RateLimited { retry_after } => {
                    // Use header value, then error field, then computed backoff
                    let delay = retry_after_header
                        .or(*retry_after)
                        .map(Duration::from_secs_f64)
                        .unwrap_or_else(|| self.compute_backoff(attempt));
                    RetryAction::RetryAfter(delay.min(self.max_backoff))
                }
                ProviderError::Network(_) | ProviderError::Timeout(_) => {
                    RetryAction::RetryAfter(self.compute_backoff(attempt))
                }
                ProviderError::ApiError { status, .. } => {
                    if is_retryable_status(*status) {
                        RetryAction::RetryAfter(self.compute_backoff(attempt))
                    } else {
                        RetryAction::DoNotRetry
                    }
                }
                // Auth, model not found, context overflow, SSE parse: don't retry
                _ => RetryAction::DoNotRetry,
            },
            // Non-provider errors: don't retry
            _ => RetryAction::DoNotRetry,
        }
    }

    /// Exponential backoff with ±25% jitter.
    fn compute_backoff(&self, attempt: u32) -> Duration {
        let base = self.initial_backoff.as_millis() as u64 * 2u64.pow(attempt);
        let base = base.min(self.max_backoff.as_millis() as u64);

        // Deterministic jitter: ±25% based on attempt number
        // In production you'd use rand, but this avoids a dependency.
        let jitter_factor = match attempt % 4 {
            0 => 100,
            1 => 125,
            2 => 75,
            3 => 110,
            _ => 100,
        };
        let with_jitter = base * jitter_factor / 100;
        Duration::from_millis(with_jitter.min(self.max_backoff.as_millis() as u64))
    }
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 529)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate_limited_error(retry_after: Option<f64>) -> HermesError {
        HermesError::Provider(ProviderError::RateLimited { retry_after })
    }

    fn api_error(status: u16) -> HermesError {
        HermesError::Provider(ProviderError::ApiError {
            status,
            message: "error".into(),
        })
    }

    fn network_error() -> HermesError {
        HermesError::Provider(ProviderError::Network("connection reset".into()))
    }

    #[test]
    fn test_rate_limited_uses_retry_after_header() {
        let policy = RetryPolicy::default();
        let err = rate_limited_error(None);
        match policy.should_retry(&err, 0, Some(2.5)) {
            RetryAction::RetryAfter(d) => assert_eq!(d, Duration::from_secs_f64(2.5)),
            other => panic!("expected RetryAfter, got {other:?}"),
        }
    }

    #[test]
    fn test_rate_limited_uses_error_field_if_no_header() {
        let policy = RetryPolicy::default();
        let err = rate_limited_error(Some(5.0));
        match policy.should_retry(&err, 0, None) {
            RetryAction::RetryAfter(d) => assert_eq!(d, Duration::from_secs_f64(5.0)),
            other => panic!("expected RetryAfter, got {other:?}"),
        }
    }

    #[test]
    fn test_rate_limited_falls_back_to_backoff() {
        let policy = RetryPolicy::default();
        let err = rate_limited_error(None);
        match policy.should_retry(&err, 0, None) {
            RetryAction::RetryAfter(d) => assert!(d.as_millis() > 0),
            other => panic!("expected RetryAfter, got {other:?}"),
        }
    }

    #[test]
    fn test_network_error_retries() {
        let policy = RetryPolicy::default();
        let err = network_error();
        assert!(matches!(
            policy.should_retry(&err, 0, None),
            RetryAction::RetryAfter(_)
        ));
    }

    #[test]
    fn test_500_retries() {
        let policy = RetryPolicy::default();
        for status in [429, 500, 502, 503, 529] {
            let err = api_error(status);
            assert!(
                matches!(policy.should_retry(&err, 0, None), RetryAction::RetryAfter(_)),
                "status {status} should be retryable"
            );
        }
    }

    #[test]
    fn test_400_does_not_retry() {
        let policy = RetryPolicy::default();
        let err = api_error(400);
        assert_eq!(policy.should_retry(&err, 0, None), RetryAction::DoNotRetry);
    }

    #[test]
    fn test_auth_error_does_not_retry() {
        let policy = RetryPolicy::default();
        let err = HermesError::Provider(ProviderError::AuthError);
        assert_eq!(policy.should_retry(&err, 0, None), RetryAction::DoNotRetry);
    }

    #[test]
    fn test_max_retries_exceeded() {
        let policy = RetryPolicy::default(); // max_retries = 3
        let err = network_error();
        // Attempts 0, 1, 2 should retry
        assert!(matches!(policy.should_retry(&err, 0, None), RetryAction::RetryAfter(_)));
        assert!(matches!(policy.should_retry(&err, 1, None), RetryAction::RetryAfter(_)));
        assert!(matches!(policy.should_retry(&err, 2, None), RetryAction::RetryAfter(_)));
        // Attempt 3 = exceeded
        assert_eq!(policy.should_retry(&err, 3, None), RetryAction::DoNotRetry);
    }

    #[test]
    fn test_backoff_increases_with_attempts() {
        let policy = RetryPolicy {
            max_retries: 5,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(60),
        };
        let err = network_error();
        let d0 = match policy.should_retry(&err, 0, None) {
            RetryAction::RetryAfter(d) => d,
            _ => panic!(),
        };
        let d2 = match policy.should_retry(&err, 2, None) {
            RetryAction::RetryAfter(d) => d,
            _ => panic!(),
        };
        // With jitter, d2 should generally be larger than d0 (100ms base vs 400ms base)
        assert!(d2 > d0, "d2={d2:?} should be > d0={d0:?}");
    }

    #[test]
    fn test_non_provider_error_does_not_retry() {
        let policy = RetryPolicy::default();
        let err = HermesError::ApprovalDenied;
        assert_eq!(policy.should_retry(&err, 0, None), RetryAction::DoNotRetry);
    }
}
```

- [ ] **Step 2: Add to lib.rs**

Update `crates/hermes-provider/src/lib.rs`:

```rust
pub mod retry;
pub mod sse;
pub mod tool_assembler;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p hermes-provider -- --nocapture`

Expected: All tests PASS (~25 total).

- [ ] **Step 4: Commit**

```bash
git add crates/hermes-provider/src/retry.rs crates/hermes-provider/src/lib.rs
git commit -m "feat: implement retry policy with exponential backoff"
```

---

## Task 6: OpenAI-Compatible Provider

Implements the `Provider` trait for OpenAI `/v1/chat/completions` API. Also works with OpenRouter, Azure, and any OpenAI-compatible endpoint. Handles SSE streaming and tool call assembly.

**Files:**
- Create: `crates/hermes-provider/src/openai.rs`
- Modify: `crates/hermes-provider/src/lib.rs`

- [ ] **Step 1: Write OpenAI provider implementation**

Create `crates/hermes-provider/src/openai.rs`:

```rust
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use hermes_core::error::{HermesError, ProviderError, Result};
use hermes_core::message::{Content, ContentPart, Message, ToolCall};
use hermes_core::provider::*;
use hermes_core::stream::StreamDelta;
use hermes_core::tool::ToolSchema;

use crate::sse::SseStream;
use crate::tool_assembler::ToolCallAssembler;

/// Authentication style for the API endpoint.
#[derive(Debug, Clone)]
pub enum AuthStyle {
    /// Bearer token in Authorization header (OpenAI, OpenRouter).
    Bearer,
    /// Azure API key in `api-key` header.
    AzureApiKey,
}

/// Configuration for an OpenAI-compatible provider.
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: SecretString,
    pub model: String,
    pub org_id: Option<String>,
    pub auth_style: AuthStyle,
}

/// Provider for OpenAI-compatible `/v1/chat/completions` endpoints.
pub struct OpenAiProvider {
    client: reqwest::Client,
    config: OpenAiConfig,
    info: ModelInfo,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig, info: ModelInfo) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            config,
            info,
        }
    }

    fn auth_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let key = self.config.api_key.expose_secret();
        match self.config.auth_style {
            AuthStyle::Bearer => {
                if let Ok(v) = HeaderValue::from_str(&format!("Bearer {key}")) {
                    headers.insert(AUTHORIZATION, v);
                }
            }
            AuthStyle::AzureApiKey => {
                if let Ok(v) = HeaderValue::from_str(key) {
                    headers.insert("api-key", v);
                }
            }
        }

        if let Some(org) = &self.config.org_id {
            if let Ok(v) = HeaderValue::from_str(org) {
                headers.insert("OpenAI-Organization", v);
            }
        }

        headers
    }

    fn build_request_body(&self, request: &ChatRequest<'_>) -> Value {
        let messages = self.convert_messages(request.system, request.messages);
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "stream": true,
        });

        if !request.tools.is_empty() {
            let tools: Vec<Value> = request.tools.iter().map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            }).collect();
            body["tools"] = json!(tools);
        }

        if !request.stop_sequences.is_empty() {
            body["stop"] = json!(request.stop_sequences);
        }

        // Stream options to get usage in final chunk
        body["stream_options"] = json!({"include_usage": true});

        body
    }

    fn convert_messages(&self, system: &str, messages: &[Message]) -> Vec<Value> {
        let mut result = Vec::with_capacity(messages.len() + 1);

        if !system.is_empty() {
            result.push(json!({"role": "system", "content": system}));
        }

        for msg in messages {
            match msg.role {
                hermes_core::message::Role::System => {
                    // Already handled above
                }
                hermes_core::message::Role::User => {
                    let content = self.convert_content(&msg.content);
                    result.push(json!({"role": "user", "content": content}));
                }
                hermes_core::message::Role::Assistant => {
                    let mut m = json!({
                        "role": "assistant",
                        "content": msg.content.as_text_lossy(),
                    });
                    if !msg.tool_calls.is_empty() {
                        let tool_calls: Vec<Value> = msg.tool_calls.iter().map(|tc| {
                            json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": tc.arguments.to_string(),
                                }
                            })
                        }).collect();
                        m["tool_calls"] = json!(tool_calls);
                    }
                    result.push(m);
                }
                hermes_core::message::Role::Tool => {
                    result.push(json!({
                        "role": "tool",
                        "tool_call_id": msg.tool_call_id,
                        "content": msg.content.as_text_lossy(),
                    }));
                }
            }
        }

        result
    }

    fn convert_content(&self, content: &Content) -> Value {
        match content {
            Content::Text(s) => json!(s),
            Content::Parts(parts) => {
                let converted: Vec<Value> = parts.iter().map(|p| match p {
                    ContentPart::Text { text } => json!({"type": "text", "text": text}),
                    ContentPart::Image { data, media_type } => json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{media_type};base64,{data}"),
                        }
                    }),
                }).collect();
                json!(converted)
            }
        }
    }

    async fn stream_response(
        &self,
        http_response: reqwest::Response,
        delta_tx: &mpsc::Sender<StreamDelta>,
    ) -> Result<ChatResponse> {
        let status = http_response.status();
        if !status.is_success() {
            let body = http_response.text().await.unwrap_or_default();
            return Err(classify_http_error(status.as_u16(), &body));
        }

        let byte_stream = http_response.bytes_stream();
        let mut sse = SseStream::new(byte_stream);
        let mut assembler = ToolCallAssembler::new();
        let mut content_buf = String::new();
        let mut reasoning_buf = String::new();
        let mut finish_reason = FinishReason::Stop;
        let mut usage = TokenUsage::default();

        while let Some(event) = sse.next_event().await.map_err(|e| {
            HermesError::Provider(ProviderError::SseParse(e.to_string()))
        })? {
            let data: Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(e) => {
                    warn!("failed to parse SSE data: {e}");
                    continue;
                }
            };

            // Check for error in stream
            if let Some(err) = data.get("error") {
                let msg = err.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown stream error");
                return Err(HermesError::Provider(ProviderError::ApiError {
                    status: 0,
                    message: msg.to_string(),
                }));
            }

            // Usage (in final chunk)
            if let Some(u) = data.get("usage") {
                usage.input_tokens = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                usage.output_tokens = u.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            }

            let Some(choices) = data.get("choices").and_then(|c| c.as_array()) else {
                continue;
            };

            for choice in choices {
                // Finish reason
                if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                    finish_reason = match fr {
                        "stop" => FinishReason::Stop,
                        "tool_calls" => FinishReason::ToolUse,
                        "length" => FinishReason::MaxTokens,
                        "content_filter" => FinishReason::ContentFilter,
                        _ => FinishReason::Stop,
                    };
                }

                let Some(delta) = choice.get("delta") else { continue };

                // Text content delta
                if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                    content_buf.push_str(text);
                    let _ = delta_tx.send(StreamDelta::TextDelta(text.to_string())).await;
                }

                // Reasoning content (some models)
                if let Some(reasoning) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                    reasoning_buf.push_str(reasoning);
                    let _ = delta_tx.send(StreamDelta::ReasoningDelta(reasoning.to_string())).await;
                }

                // Tool call deltas
                if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tool_calls {
                        let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

                        if let Some(func) = tc.get("function") {
                            // New tool call start
                            if let (Some(id), Some(name)) = (
                                tc.get("id").and_then(|v| v.as_str()),
                                func.get("name").and_then(|v| v.as_str()),
                            ) {
                                assembler.start(index, id.to_string(), name.to_string());
                                let _ = delta_tx.send(StreamDelta::ToolCallStart {
                                    id: id.to_string(),
                                    name: name.to_string(),
                                }).await;
                            }

                            // Arguments delta
                            if let Some(args_delta) = func.get("arguments").and_then(|v| v.as_str()) {
                                assembler.append_arguments(index, args_delta);
                                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                                    let _ = delta_tx.send(StreamDelta::ToolCallArgsDelta {
                                        id: id.to_string(),
                                        delta: args_delta.to_string(),
                                    }).await;
                                }
                            }
                        }
                    }
                }
            }
        }

        let _ = delta_tx.send(StreamDelta::Done).await;

        let tool_calls = if assembler.has_pending() {
            assembler.finish()
        } else {
            vec![]
        };

        Ok(ChatResponse {
            content: content_buf,
            tool_calls,
            reasoning: if reasoning_buf.is_empty() { None } else { Some(reasoning_buf) },
            finish_reason,
            usage,
            cache_meta: None,
        })
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn chat(
        &self,
        request: &ChatRequest<'_>,
        delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) -> Result<ChatResponse> {
        let url = format!("{}/chat/completions", self.config.base_url);
        let body = self.build_request_body(request);

        debug!(url = %url, model = %self.config.model, "sending chat request");

        let http_response = self
            .client
            .post(&url)
            .headers(self.auth_headers())
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    HermesError::Provider(ProviderError::Timeout(300))
                } else {
                    HermesError::Provider(ProviderError::Network(e.to_string()))
                }
            })?;

        if let Some(tx) = delta_tx {
            self.stream_response(http_response, tx).await
        } else {
            // For non-streaming, we still parse SSE but use a dummy channel
            let (tx, _rx) = mpsc::channel(64);
            self.stream_response(http_response, &tx).await
        }
    }

    fn model_info(&self) -> &ModelInfo {
        &self.info
    }
}

fn classify_http_error(status: u16, body: &str) -> HermesError {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| body.chars().take(500).collect());

    HermesError::Provider(match status {
        401 | 403 => ProviderError::AuthError,
        404 => ProviderError::ModelNotFound(message),
        429 => ProviderError::RateLimited { retry_after: None },
        _ => ProviderError::ApiError { status, message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_http_error_401() {
        let err = classify_http_error(401, r#"{"error":{"message":"invalid key"}}"#);
        assert!(matches!(err, HermesError::Provider(ProviderError::AuthError)));
    }

    #[test]
    fn test_classify_http_error_429() {
        let err = classify_http_error(429, "");
        assert!(matches!(err, HermesError::Provider(ProviderError::RateLimited { .. })));
    }

    #[test]
    fn test_classify_http_error_500() {
        let err = classify_http_error(500, r#"{"error":{"message":"internal error"}}"#);
        match err {
            HermesError::Provider(ProviderError::ApiError { status, message }) => {
                assert_eq!(status, 500);
                assert_eq!(message, "internal error");
            }
            other => panic!("expected ApiError, got {other:?}"),
        }
    }

    #[test]
    fn test_convert_messages_system_first() {
        let config = OpenAiConfig {
            base_url: "https://api.openai.com/v1".into(),
            api_key: SecretString::from("test-key".to_string()),
            model: "gpt-4o".into(),
            org_id: None,
            auth_style: AuthStyle::Bearer,
        };
        let info = ModelInfo {
            id: "gpt-4o".into(),
            provider: "openai".into(),
            max_context: 128_000,
            max_output: 16_384,
            supports_tools: true,
            supports_vision: true,
            supports_reasoning: false,
            supports_caching: false,
            pricing: ModelPricing {
                input_per_mtok: 2.5,
                output_per_mtok: 10.0,
                cache_read_per_mtok: 0.0,
                cache_create_per_mtok: 0.0,
            },
        };
        let provider = OpenAiProvider::new(config, info);

        let messages = vec![Message::user("hello")];
        let converted = provider.convert_messages("you are helpful", &messages);

        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0]["role"], "system");
        assert_eq!(converted[0]["content"], "you are helpful");
        assert_eq!(converted[1]["role"], "user");
        assert_eq!(converted[1]["content"], "hello");
    }

    #[test]
    fn test_convert_messages_with_tool_calls_and_results() {
        let config = OpenAiConfig {
            base_url: "https://api.openai.com/v1".into(),
            api_key: SecretString::from("test-key".to_string()),
            model: "gpt-4o".into(),
            org_id: None,
            auth_style: AuthStyle::Bearer,
        };
        let info = ModelInfo {
            id: "gpt-4o".into(),
            provider: "openai".into(),
            max_context: 128_000,
            max_output: 16_384,
            supports_tools: true,
            supports_vision: true,
            supports_reasoning: false,
            supports_caching: false,
            pricing: ModelPricing {
                input_per_mtok: 2.5,
                output_per_mtok: 10.0,
                cache_read_per_mtok: 0.0,
                cache_create_per_mtok: 0.0,
            },
        };
        let provider = OpenAiProvider::new(config, info);

        let messages = vec![
            Message::user("search for rust"),
            Message {
                role: hermes_core::message::Role::Assistant,
                content: Content::Text("I'll search".into()),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "web_search".into(),
                    arguments: json!({"query": "rust"}),
                }],
                reasoning: None,
                name: None,
                tool_call_id: None,
            },
            Message {
                role: hermes_core::message::Role::Tool,
                content: Content::Text("Rust is a programming language".into()),
                tool_calls: vec![],
                reasoning: None,
                name: Some("web_search".into()),
                tool_call_id: Some("call_1".into()),
            },
        ];

        let converted = provider.convert_messages("", &messages);
        // No system message since system is empty
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0]["role"], "user");
        assert_eq!(converted[1]["role"], "assistant");
        assert!(converted[1].get("tool_calls").is_some());
        assert_eq!(converted[2]["role"], "tool");
        assert_eq!(converted[2]["tool_call_id"], "call_1");
    }
}
```

- [ ] **Step 2: Add to lib.rs**

Update `crates/hermes-provider/src/lib.rs`:

```rust
pub mod openai;
pub mod retry;
pub mod sse;
pub mod tool_assembler;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p hermes-provider -- --nocapture`

Expected: All tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/hermes-provider/src/openai.rs crates/hermes-provider/src/lib.rs
git commit -m "feat: implement OpenAI-compatible provider with SSE streaming"
```

---

## Task 7: Anthropic Provider

Implements the `Provider` trait for Anthropic's `/v1/messages` API. Different message format (strict alternation, system prompt separate, thinking blocks), different SSE events, prompt cache support.

**Files:**
- Create: `crates/hermes-provider/src/anthropic.rs`
- Modify: `crates/hermes-provider/src/lib.rs`

- [ ] **Step 1: Write Anthropic provider implementation**

Create `crates/hermes-provider/src/anthropic.rs`:

```rust
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use hermes_core::error::{HermesError, ProviderError, Result};
use hermes_core::message::{Content, ContentPart, Message, Role, ToolCall};
use hermes_core::provider::*;
use hermes_core::stream::StreamDelta;
use hermes_core::tool::ToolSchema;

use crate::sse::SseStream;

/// Configuration for the Anthropic provider.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub base_url: String,
    pub api_key: SecretString,
    pub model: String,
    pub api_version: String,
    pub max_thinking_tokens: Option<u32>,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.anthropic.com/v1".into(),
            api_key: SecretString::from(String::new()),
            model: "claude-sonnet-4-20250514".into(),
            api_version: "2023-06-01".into(),
            max_thinking_tokens: None,
        }
    }
}

/// Provider for Anthropic's `/v1/messages` API.
pub struct AnthropicProvider {
    client: reqwest::Client,
    config: AnthropicConfig,
    info: ModelInfo,
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig, info: ModelInfo) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("failed to build HTTP client");
        Self {
            client,
            config,
            info,
        }
    }

    fn auth_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let key = self.config.api_key.expose_secret();
        if key.starts_with("sk-ant-oat") {
            // OAuth token
            if let Ok(v) = HeaderValue::from_str(&format!("Bearer {key}")) {
                headers.insert("Authorization", v);
            }
        } else {
            // API key
            if let Ok(v) = HeaderValue::from_str(key) {
                headers.insert("x-api-key", v);
            }
        }

        if let Ok(v) = HeaderValue::from_str(&self.config.api_version) {
            headers.insert("anthropic-version", v);
        }

        headers
    }

    fn build_request_body(&self, request: &ChatRequest<'_>) -> Value {
        let messages = self.convert_messages(request.messages);

        // System prompt: either segmented (for cache control) or plain
        let system = if let Some(segments) = request.system_segments {
            let blocks: Vec<Value> = segments
                .iter()
                .filter(|s| !s.text.is_empty())
                .map(|seg| {
                    let mut block = json!({"type": "text", "text": seg.text});
                    if seg.cache_control {
                        block["cache_control"] = json!({"type": "ephemeral"});
                    }
                    block
                })
                .collect();
            json!(blocks)
        } else if !request.system.is_empty() {
            json!(request.system)
        } else {
            json!([])
        };

        let mut body = json!({
            "model": self.config.model,
            "system": system,
            "messages": messages,
            "max_tokens": request.max_tokens,
            "stream": true,
        });

        if request.temperature > 0.0 {
            body["temperature"] = json!(request.temperature);
        }

        if !request.tools.is_empty() {
            let tools: Vec<Value> = request.tools.iter().map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            }).collect();
            body["tools"] = json!(tools);
        }

        if !request.stop_sequences.is_empty() {
            body["stop_sequences"] = json!(request.stop_sequences);
        }

        // Extended thinking
        if request.reasoning {
            if let Some(budget) = self.config.max_thinking_tokens {
                body["thinking"] = json!({
                    "type": "enabled",
                    "budget_tokens": budget,
                });
            }
        }

        body
    }

    /// Convert internal messages to Anthropic format.
    /// Anthropic requires strict user/assistant alternation.
    /// Tool results are `user` messages with `tool_result` content blocks.
    fn convert_messages(&self, messages: &[Message]) -> Vec<Value> {
        let mut result: Vec<Value> = Vec::with_capacity(messages.len());

        for msg in messages {
            match msg.role {
                Role::System => {
                    // System is handled separately, skip
                }
                Role::User => {
                    let content = self.convert_content(&msg.content);
                    let m = json!({"role": "user", "content": content});
                    self.push_or_merge(&mut result, m);
                }
                Role::Assistant => {
                    let mut blocks = Vec::new();

                    // Reasoning/thinking block (if present)
                    if let Some(reasoning) = &msg.reasoning {
                        blocks.push(json!({
                            "type": "thinking",
                            "thinking": reasoning,
                        }));
                    }

                    // Text content
                    let text = msg.content.as_text_lossy();
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }

                    // Tool use blocks
                    for tc in &msg.tool_calls {
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.name,
                            "input": tc.arguments,
                        }));
                    }

                    if blocks.is_empty() {
                        blocks.push(json!({"type": "text", "text": ""}));
                    }

                    let m = json!({"role": "assistant", "content": blocks});
                    self.push_or_merge(&mut result, m);
                }
                Role::Tool => {
                    // Tool results become user messages with tool_result content
                    let block = json!({
                        "type": "tool_result",
                        "tool_use_id": msg.tool_call_id,
                        "content": msg.content.as_text_lossy(),
                    });
                    let m = json!({"role": "user", "content": [block]});
                    self.push_or_merge(&mut result, m);
                }
            }
        }

        result
    }

    fn convert_content(&self, content: &Content) -> Value {
        match content {
            Content::Text(s) => json!([{"type": "text", "text": s}]),
            Content::Parts(parts) => {
                let blocks: Vec<Value> = parts.iter().map(|p| match p {
                    ContentPart::Text { text } => json!({"type": "text", "text": text}),
                    ContentPart::Image { data, media_type } => json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": media_type,
                            "data": data,
                        }
                    }),
                }).collect();
                json!(blocks)
            }
        }
    }

    /// Merge adjacent same-role messages (Anthropic requires strict alternation).
    fn push_or_merge(&self, result: &mut Vec<Value>, msg: Value) {
        if let Some(last) = result.last_mut() {
            if last["role"] == msg["role"] {
                // Merge content arrays
                if let (Some(last_content), Some(new_content)) = (
                    last.get_mut("content").and_then(|c| c.as_array_mut()),
                    msg["content"].as_array(),
                ) {
                    last_content.extend(new_content.iter().cloned());
                    return;
                }
            }
        }
        result.push(msg);
    }

    async fn stream_response(
        &self,
        http_response: reqwest::Response,
        delta_tx: &mpsc::Sender<StreamDelta>,
    ) -> Result<ChatResponse> {
        let status = http_response.status();
        if !status.is_success() {
            let body = http_response.text().await.unwrap_or_default();
            return Err(classify_anthropic_error(status.as_u16(), &body));
        }

        let byte_stream = http_response.bytes_stream();
        let mut sse = SseStream::new(byte_stream);
        let mut content_buf = String::new();
        let mut reasoning_buf = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut current_tool: Option<PendingTool> = None;
        let mut finish_reason = FinishReason::Stop;
        let mut usage = TokenUsage::default();

        while let Some(event) = sse.next_event().await.map_err(|e| {
            HermesError::Provider(ProviderError::SseParse(e.to_string()))
        })? {
            let data: Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(e) => {
                    warn!("failed to parse Anthropic SSE data: {e}");
                    continue;
                }
            };

            let event_type = event.event.as_deref()
                .or_else(|| data.get("type").and_then(|v| v.as_str()));

            match event_type {
                Some("message_start") => {
                    if let Some(u) = data.get("message").and_then(|m| m.get("usage")) {
                        usage.input_tokens = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        usage.cache_creation_tokens = u.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        usage.cache_read_tokens = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    }
                }
                Some("content_block_start") => {
                    if let Some(block) = data.get("content_block") {
                        match block.get("type").and_then(|v| v.as_str()) {
                            Some("tool_use") => {
                                let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let _ = delta_tx.send(StreamDelta::ToolCallStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                }).await;
                                current_tool = Some(PendingTool {
                                    id,
                                    name,
                                    input_buf: String::new(),
                                });
                            }
                            _ => {}
                        }
                    }
                }
                Some("content_block_delta") => {
                    if let Some(delta) = data.get("delta") {
                        match delta.get("type").and_then(|v| v.as_str()) {
                            Some("text_delta") => {
                                if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                    content_buf.push_str(text);
                                    let _ = delta_tx.send(StreamDelta::TextDelta(text.to_string())).await;
                                }
                            }
                            Some("thinking_delta") => {
                                if let Some(thinking) = delta.get("thinking").and_then(|v| v.as_str()) {
                                    reasoning_buf.push_str(thinking);
                                    let _ = delta_tx.send(StreamDelta::ReasoningDelta(thinking.to_string())).await;
                                }
                            }
                            Some("input_json_delta") => {
                                if let Some(partial) = delta.get("partial_json").and_then(|v| v.as_str()) {
                                    if let Some(ref mut tool) = current_tool {
                                        tool.input_buf.push_str(partial);
                                        let _ = delta_tx.send(StreamDelta::ToolCallArgsDelta {
                                            id: tool.id.clone(),
                                            delta: partial.to_string(),
                                        }).await;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Some("content_block_stop") => {
                    if let Some(tool) = current_tool.take() {
                        let arguments = serde_json::from_str(&tool.input_buf)
                            .unwrap_or_else(|_| json!({"_raw": tool.input_buf, "_error": "invalid JSON"}));
                        tool_calls.push(ToolCall {
                            id: tool.id,
                            name: tool.name,
                            arguments,
                        });
                    }
                }
                Some("message_delta") => {
                    if let Some(delta) = data.get("delta") {
                        if let Some(reason) = delta.get("stop_reason").and_then(|v| v.as_str()) {
                            finish_reason = match reason {
                                "end_turn" | "stop_sequence" => FinishReason::Stop,
                                "tool_use" => FinishReason::ToolUse,
                                "max_tokens" => FinishReason::MaxTokens,
                                _ => FinishReason::Stop,
                            };
                        }
                    }
                    if let Some(u) = data.get("usage") {
                        usage.output_tokens = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    }
                }
                Some("message_stop") | Some("error") => {
                    if event_type == Some("error") {
                        let msg = data.get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(|m| m.as_str())
                            .unwrap_or("stream error");
                        return Err(HermesError::Provider(ProviderError::ApiError {
                            status: 0,
                            message: msg.to_string(),
                        }));
                    }
                }
                _ => {}
            }
        }

        let _ = delta_tx.send(StreamDelta::Done).await;

        Ok(ChatResponse {
            content: content_buf,
            tool_calls,
            reasoning: if reasoning_buf.is_empty() { None } else { Some(reasoning_buf) },
            finish_reason,
            usage,
            cache_meta: if usage.cache_creation_tokens > 0 || usage.cache_read_tokens > 0 {
                Some(CacheMeta {
                    cache_creation_tokens: usage.cache_creation_tokens,
                    cache_read_tokens: usage.cache_read_tokens,
                })
            } else {
                None
            },
        })
    }
}

struct PendingTool {
    id: String,
    name: String,
    input_buf: String,
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn chat(
        &self,
        request: &ChatRequest<'_>,
        delta_tx: Option<&mpsc::Sender<StreamDelta>>,
    ) -> Result<ChatResponse> {
        let url = format!("{}/messages", self.config.base_url);
        let body = self.build_request_body(request);

        debug!(url = %url, model = %self.config.model, "sending Anthropic chat request");

        let http_response = self
            .client
            .post(&url)
            .headers(self.auth_headers())
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    HermesError::Provider(ProviderError::Timeout(300))
                } else {
                    HermesError::Provider(ProviderError::Network(e.to_string()))
                }
            })?;

        if let Some(tx) = delta_tx {
            self.stream_response(http_response, tx).await
        } else {
            let (tx, _rx) = mpsc::channel(64);
            self.stream_response(http_response, &tx).await
        }
    }

    fn supports_reasoning(&self) -> bool {
        self.config.max_thinking_tokens.is_some()
    }

    fn supports_caching(&self) -> bool {
        true
    }

    fn model_info(&self) -> &ModelInfo {
        &self.info
    }
}

fn classify_anthropic_error(status: u16, body: &str) -> HermesError {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| body.chars().take(500).collect());

    let err_type = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("type"))
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        });

    HermesError::Provider(match status {
        401 => ProviderError::AuthError,
        429 => ProviderError::RateLimited { retry_after: None },
        529 => ProviderError::ApiError { status, message: "API overloaded".into() },
        _ => {
            if err_type.as_deref() == Some("invalid_request_error")
                && message.contains("context length")
            {
                ProviderError::ContextLengthExceeded { used: 0, max: 0 }
            } else {
                ProviderError::ApiError { status, message }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> AnthropicConfig {
        AnthropicConfig {
            api_key: SecretString::from("sk-ant-api-test".to_string()),
            ..Default::default()
        }
    }

    fn test_info() -> ModelInfo {
        ModelInfo {
            id: "claude-sonnet-4-20250514".into(),
            provider: "anthropic".into(),
            max_context: 200_000,
            max_output: 64_000,
            supports_tools: true,
            supports_vision: true,
            supports_reasoning: false,
            supports_caching: true,
            pricing: ModelPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cache_read_per_mtok: 0.3,
                cache_create_per_mtok: 3.75,
            },
        }
    }

    #[test]
    fn test_convert_messages_strict_alternation() {
        let provider = AnthropicProvider::new(test_config(), test_info());

        // Two consecutive tool results (role=Tool -> user) should be merged
        let messages = vec![
            Message::user("search for rust and python"),
            Message {
                role: Role::Assistant,
                content: Content::Text("I'll search for both".into()),
                tool_calls: vec![
                    ToolCall { id: "c1".into(), name: "search".into(), arguments: json!({"q": "rust"}) },
                    ToolCall { id: "c2".into(), name: "search".into(), arguments: json!({"q": "python"}) },
                ],
                reasoning: None,
                name: None,
                tool_call_id: None,
            },
            Message {
                role: Role::Tool,
                content: Content::Text("Rust results".into()),
                tool_calls: vec![],
                reasoning: None,
                name: Some("search".into()),
                tool_call_id: Some("c1".into()),
            },
            Message {
                role: Role::Tool,
                content: Content::Text("Python results".into()),
                tool_calls: vec![],
                reasoning: None,
                name: Some("search".into()),
                tool_call_id: Some("c2".into()),
            },
        ];

        let converted = provider.convert_messages(&messages);
        // user, assistant, user(merged tool results)
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0]["role"], "user");
        assert_eq!(converted[1]["role"], "assistant");
        assert_eq!(converted[2]["role"], "user");
        // The merged user message should have 2 tool_result blocks
        let content = converted[2]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[1]["type"], "tool_result");
    }

    #[test]
    fn test_convert_messages_with_thinking() {
        let provider = AnthropicProvider::new(test_config(), test_info());

        let messages = vec![
            Message::user("think about this"),
            Message {
                role: Role::Assistant,
                content: Content::Text("Here's my answer".into()),
                tool_calls: vec![],
                reasoning: Some("Let me reason through this...".into()),
                name: None,
                tool_call_id: None,
            },
        ];

        let converted = provider.convert_messages(&messages);
        assert_eq!(converted.len(), 2);
        let assistant_content = converted[1]["content"].as_array().unwrap();
        assert_eq!(assistant_content[0]["type"], "thinking");
        assert_eq!(assistant_content[1]["type"], "text");
    }

    #[test]
    fn test_classify_anthropic_error_overloaded() {
        let err = classify_anthropic_error(529, r#"{"error":{"type":"overloaded_error","message":"overloaded"}}"#);
        match err {
            HermesError::Provider(ProviderError::ApiError { status, .. }) => assert_eq!(status, 529),
            other => panic!("expected ApiError, got {other:?}"),
        }
    }

    #[test]
    fn test_classify_anthropic_error_context_length() {
        let body = r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: context length exceeded"}}"#;
        let err = classify_anthropic_error(400, body);
        assert!(matches!(err, HermesError::Provider(ProviderError::ContextLengthExceeded { .. })));
    }

    #[test]
    fn test_auth_header_api_key() {
        let provider = AnthropicProvider::new(test_config(), test_info());
        let headers = provider.auth_headers();
        assert!(headers.get("x-api-key").is_some());
        assert!(headers.get("Authorization").is_none());
    }

    #[test]
    fn test_auth_header_oauth() {
        let config = AnthropicConfig {
            api_key: SecretString::from("sk-ant-oat-test-token".to_string()),
            ..Default::default()
        };
        let provider = AnthropicProvider::new(config, test_info());
        let headers = provider.auth_headers();
        assert!(headers.get("Authorization").is_some());
        assert!(headers.get("x-api-key").is_none());
    }
}
```

- [ ] **Step 2: Add to lib.rs**

Update `crates/hermes-provider/src/lib.rs`:

```rust
pub mod anthropic;
pub mod openai;
pub mod retry;
pub mod sse;
pub mod tool_assembler;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p hermes-provider -- --nocapture`

Expected: All tests PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/hermes-provider/src/anthropic.rs crates/hermes-provider/src/lib.rs
git commit -m "feat: implement Anthropic provider with cache support and thinking blocks"
```

---

## Task 8: Provider Factory

Factory function that creates the right provider based on model string config. Also add `create_provider` to lib.rs.

**Files:**
- Modify: `crates/hermes-provider/src/lib.rs`

- [ ] **Step 1: Add provider factory with tests**

Replace `crates/hermes-provider/src/lib.rs`:

```rust
use std::sync::Arc;

use hermes_core::error::{HermesError, Result};
use hermes_core::provider::{ModelInfo, ModelPricing, Provider};
use secrecy::SecretString;

pub mod anthropic;
pub mod openai;
pub mod retry;
pub mod sse;
pub mod tool_assembler;

/// Create a provider from a model string like "anthropic/claude-sonnet-4-20250514"
/// or "openai/gpt-4o". Falls back to OpenAI-compatible if provider prefix is unknown.
pub fn create_provider(
    model_string: &str,
    api_key: SecretString,
    base_url: Option<&str>,
) -> Result<Arc<dyn Provider>> {
    let (provider_name, model_id) = model_string
        .split_once('/')
        .unwrap_or(("openai", model_string));

    match provider_name {
        "anthropic" => {
            let config = anthropic::AnthropicConfig {
                base_url: base_url
                    .unwrap_or("https://api.anthropic.com/v1")
                    .to_string(),
                api_key,
                model: model_id.to_string(),
                api_version: "2023-06-01".to_string(),
                max_thinking_tokens: None,
            };
            let info = anthropic_model_info(model_id);
            Ok(Arc::new(anthropic::AnthropicProvider::new(config, info)))
        }
        "openai" => {
            let config = openai::OpenAiConfig {
                base_url: base_url
                    .unwrap_or("https://api.openai.com/v1")
                    .to_string(),
                api_key,
                model: model_id.to_string(),
                org_id: None,
                auth_style: openai::AuthStyle::Bearer,
            };
            let info = openai_model_info(model_id);
            Ok(Arc::new(openai::OpenAiProvider::new(config, info)))
        }
        "openrouter" => {
            let config = openai::OpenAiConfig {
                base_url: base_url
                    .unwrap_or("https://openrouter.ai/api/v1")
                    .to_string(),
                api_key,
                model: model_id.to_string(),
                org_id: None,
                auth_style: openai::AuthStyle::Bearer,
            };
            let info = generic_model_info(provider_name, model_id);
            Ok(Arc::new(openai::OpenAiProvider::new(config, info)))
        }
        _ => {
            // Default: treat as OpenAI-compatible
            let url = base_url.unwrap_or("https://api.openai.com/v1");
            let config = openai::OpenAiConfig {
                base_url: url.to_string(),
                api_key,
                model: model_id.to_string(),
                org_id: None,
                auth_style: openai::AuthStyle::Bearer,
            };
            let info = generic_model_info(provider_name, model_id);
            Ok(Arc::new(openai::OpenAiProvider::new(config, info)))
        }
    }
}

fn anthropic_model_info(model: &str) -> ModelInfo {
    let (max_output, pricing) = if model.contains("opus") {
        (
            32_000,
            ModelPricing {
                input_per_mtok: 15.0,
                output_per_mtok: 75.0,
                cache_read_per_mtok: 1.5,
                cache_create_per_mtok: 18.75,
            },
        )
    } else if model.contains("haiku") {
        (
            8_192,
            ModelPricing {
                input_per_mtok: 0.25,
                output_per_mtok: 1.25,
                cache_read_per_mtok: 0.03,
                cache_create_per_mtok: 0.3,
            },
        )
    } else {
        // Sonnet or default
        (
            64_000,
            ModelPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cache_read_per_mtok: 0.3,
                cache_create_per_mtok: 3.75,
            },
        )
    };

    ModelInfo {
        id: model.to_string(),
        provider: "anthropic".to_string(),
        max_context: 200_000,
        max_output,
        supports_tools: true,
        supports_vision: true,
        supports_reasoning: model.contains("opus") || model.contains("sonnet"),
        supports_caching: true,
        pricing,
    }
}

fn openai_model_info(model: &str) -> ModelInfo {
    ModelInfo {
        id: model.to_string(),
        provider: "openai".to_string(),
        max_context: 128_000,
        max_output: 16_384,
        supports_tools: true,
        supports_vision: model.contains("gpt-4") || model.contains("o1") || model.contains("o3"),
        supports_reasoning: model.starts_with("o1") || model.starts_with("o3"),
        supports_caching: false,
        pricing: ModelPricing {
            input_per_mtok: 2.5,
            output_per_mtok: 10.0,
            cache_read_per_mtok: 0.0,
            cache_create_per_mtok: 0.0,
        },
    }
}

fn generic_model_info(provider: &str, model: &str) -> ModelInfo {
    ModelInfo {
        id: model.to_string(),
        provider: provider.to_string(),
        max_context: 128_000,
        max_output: 16_384,
        supports_tools: true,
        supports_vision: false,
        supports_reasoning: false,
        supports_caching: false,
        pricing: ModelPricing {
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
            cache_read_per_mtok: 0.0,
            cache_create_per_mtok: 0.0,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_anthropic_provider() {
        let provider = create_provider(
            "anthropic/claude-sonnet-4-20250514",
            SecretString::from("sk-ant-api-test".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(provider.model_info().provider, "anthropic");
        assert_eq!(provider.model_info().id, "claude-sonnet-4-20250514");
        assert!(provider.supports_caching());
    }

    #[test]
    fn test_create_openai_provider() {
        let provider = create_provider(
            "openai/gpt-4o",
            SecretString::from("sk-test".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(provider.model_info().provider, "openai");
        assert_eq!(provider.model_info().id, "gpt-4o");
    }

    #[test]
    fn test_create_openrouter_provider() {
        let provider = create_provider(
            "openrouter/meta-llama/llama-3-70b",
            SecretString::from("sk-or-test".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(provider.model_info().provider, "openrouter");
    }

    #[test]
    fn test_create_unknown_provider_defaults_to_openai() {
        let provider = create_provider(
            "custom/my-model",
            SecretString::from("key".to_string()),
            Some("http://localhost:8000/v1"),
        )
        .unwrap();
        assert_eq!(provider.model_info().provider, "custom");
    }

    #[test]
    fn test_no_slash_defaults_to_openai() {
        let provider = create_provider(
            "gpt-4o-mini",
            SecretString::from("key".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(provider.model_info().provider, "openai");
        assert_eq!(provider.model_info().id, "gpt-4o-mini");
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p hermes-provider -- --nocapture`

Expected: All tests PASS (factory + all prior tests).

- [ ] **Step 3: Commit**

```bash
git add crates/hermes-provider/src/lib.rs
git commit -m "feat: add provider factory for model string routing"
```

---

## Task 9: Iteration Budget

Simple owned iteration counter for the agent loop. No atomics needed since each agent owns its budget independently.

**Files:**
- Create: `crates/hermes-agent/src/budget.rs`
- Modify: `crates/hermes-agent/src/lib.rs`
- Modify: `crates/hermes-agent/Cargo.toml` (trim unnecessary deps for now)

- [ ] **Step 1: Write IterationBudget with tests**

Create `crates/hermes-agent/src/budget.rs`:

```rust
/// Controls how many LLM iterations an agent can perform.
/// Owned per agent (parent and subagent get independent budgets).
pub struct IterationBudget {
    remaining: u32,
    max: u32,
}

impl IterationBudget {
    pub fn new(max: u32) -> Self {
        Self { remaining: max, max }
    }

    /// Try to consume one iteration. Returns `true` if budget was available.
    pub fn try_consume(&mut self) -> bool {
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        true
    }

    /// Refund iterations (e.g., for execute_code tool).
    /// Cannot exceed the original max.
    pub fn refund(&mut self, n: u32) {
        self.remaining = self.remaining.saturating_add(n).min(self.max);
    }

    /// Remaining iterations.
    pub fn remaining(&self) -> u32 {
        self.remaining
    }

    /// Maximum iterations.
    pub fn max(&self) -> u32 {
        self.max
    }

    /// Whether the budget is exhausted.
    pub fn is_exhausted(&self) -> bool {
        self.remaining == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_budget() {
        let budget = IterationBudget::new(10);
        assert_eq!(budget.remaining(), 10);
        assert_eq!(budget.max(), 10);
        assert!(!budget.is_exhausted());
    }

    #[test]
    fn test_consume_decrements() {
        let mut budget = IterationBudget::new(3);
        assert!(budget.try_consume());
        assert_eq!(budget.remaining(), 2);
        assert!(budget.try_consume());
        assert_eq!(budget.remaining(), 1);
        assert!(budget.try_consume());
        assert_eq!(budget.remaining(), 0);
        assert!(budget.is_exhausted());
    }

    #[test]
    fn test_consume_returns_false_when_exhausted() {
        let mut budget = IterationBudget::new(1);
        assert!(budget.try_consume());
        assert!(!budget.try_consume());
        assert!(!budget.try_consume());
        assert_eq!(budget.remaining(), 0);
    }

    #[test]
    fn test_refund() {
        let mut budget = IterationBudget::new(5);
        budget.try_consume();
        budget.try_consume();
        assert_eq!(budget.remaining(), 3);

        budget.refund(1);
        assert_eq!(budget.remaining(), 4);
    }

    #[test]
    fn test_refund_capped_at_max() {
        let mut budget = IterationBudget::new(5);
        budget.try_consume();
        assert_eq!(budget.remaining(), 4);

        budget.refund(100);
        assert_eq!(budget.remaining(), 5); // capped at max
    }

    #[test]
    fn test_zero_budget() {
        let mut budget = IterationBudget::new(0);
        assert!(budget.is_exhausted());
        assert!(!budget.try_consume());
    }

    #[test]
    fn test_refund_from_zero() {
        let mut budget = IterationBudget::new(3);
        budget.try_consume();
        budget.try_consume();
        budget.try_consume();
        assert!(budget.is_exhausted());

        budget.refund(2);
        assert_eq!(budget.remaining(), 2);
        assert!(!budget.is_exhausted());
    }
}
```

- [ ] **Step 2: Update lib.rs**

Replace `crates/hermes-agent/src/lib.rs`:

```rust
pub mod budget;
```

- [ ] **Step 3: Trim Cargo.toml dependencies**

Replace `crates/hermes-agent/Cargo.toml` — remove deps we don't need yet to speed up compilation:

```toml
[package]
name = "hermes-agent"
description = "Agent loop, iteration budget, context compression, prompt caching"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
hermes-core.workspace = true
hermes-provider.workspace = true
hermes-tools.workspace = true
serde.workspace = true
serde_json.workspace = true
async-trait.workspace = true
anyhow.workspace = true
tokio.workspace = true
uuid.workspace = true
tracing.workspace = true
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p hermes-agent -- --nocapture`

Expected: All 7 budget tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/hermes-agent/src/budget.rs crates/hermes-agent/src/lib.rs crates/hermes-agent/Cargo.toml
git commit -m "feat: implement IterationBudget for agent loop iteration control"
```

---

## Task 10: Agent Loop

The core agent conversation loop. Takes a user message, calls the LLM, dispatches tool calls (parallel or sequential), iterates until no more tool calls or budget exhausted.

**Files:**
- Create: `crates/hermes-agent/src/parallel.rs`
- Create: `crates/hermes-agent/src/loop_runner.rs`
- Modify: `crates/hermes-agent/src/lib.rs`

- [ ] **Step 1: Write parallel execution logic with tests**

Create `crates/hermes-agent/src/parallel.rs`:

```rust
use std::collections::HashSet;
use std::sync::Arc;

use tokio::task::JoinSet;
use tokio::sync::mpsc;
use tracing::warn;

use hermes_core::error::HermesError;
use hermes_core::message::{ToolCall, ToolResult};
use hermes_core::stream::StreamDelta;
use hermes_core::tool::ToolContext;
use hermes_tools::ToolRegistry;

/// Result of executing a single tool call.
pub struct ToolCallResult {
    pub call_id: String,
    pub tool_name: String,
    pub result: ToolResult,
}

/// Tools that must never run in parallel (interactive tools).
const NEVER_PARALLEL: &[&str] = &["clarify"];

/// Decide whether a batch of tool calls can be parallelized.
pub fn should_parallelize(calls: &[ToolCall], registry: &ToolRegistry) -> bool {
    if calls.len() <= 1 {
        return false;
    }

    // Any interactive tool -> sequential
    if calls.iter().any(|c| NEVER_PARALLEL.contains(&c.name.as_str())) {
        return false;
    }

    // Collect write paths to check for conflicts
    let mut write_paths: Vec<&str> = Vec::new();
    for call in calls {
        let is_read_only = registry
            .get(&call.name)
            .map_or(false, |t| t.is_read_only());
        if !is_read_only {
            if let Some(path) = call.arguments.get("path").and_then(|v| v.as_str()) {
                write_paths.push(path);
            }
        }
    }

    !has_path_conflicts(&write_paths)
}

/// Check if any paths conflict (one is a prefix of another, or they are equal).
fn has_path_conflicts(paths: &[&str]) -> bool {
    let unique: HashSet<&str> = paths.iter().copied().collect();
    if unique.len() != paths.len() {
        return true; // Duplicate paths
    }
    for (i, a) in paths.iter().enumerate() {
        for b in &paths[i + 1..] {
            if a.starts_with(b) || b.starts_with(a) {
                return true;
            }
        }
    }
    false
}

/// Execute tool calls in parallel using a JoinSet.
pub async fn execute_parallel(
    calls: &[ToolCall],
    registry: &Arc<ToolRegistry>,
    ctx: &ToolContext,
) -> Vec<ToolCallResult> {
    let mut set = JoinSet::new();

    for (original_index, call) in calls.iter().enumerate() {
        let registry = Arc::clone(registry);
        let ctx = ctx.clone();
        let call = call.clone();

        set.spawn(async move {
            let _ = ctx.delta_tx.send(StreamDelta::ToolProgress {
                tool: call.name.clone(),
                status: "running".into(),
            }).await;

            let result = match registry.get(&call.name) {
                Some(tool) => {
                    tool.execute(call.arguments.clone(), &ctx).await
                        .unwrap_or_else(|e| ToolResult::error(e.to_string()))
                }
                None => ToolResult::error(format!("unknown tool: {}", call.name)),
            };

            (original_index, ToolCallResult {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                result,
            })
        });
    }

    let mut indexed_results = Vec::with_capacity(calls.len());
    while let Some(res) = set.join_next().await {
        match res {
            Ok((idx, result)) => indexed_results.push((idx, result)),
            Err(e) => {
                warn!("tool task panicked: {e}");
            }
        }
    }

    // Re-sort by original index (JoinSet returns in completion order)
    indexed_results.sort_by_key(|(idx, _)| *idx);
    indexed_results.into_iter().map(|(_, r)| r).collect()
}

/// Execute tool calls sequentially.
pub async fn execute_sequential(
    calls: &[ToolCall],
    registry: &Arc<ToolRegistry>,
    ctx: &ToolContext,
) -> Vec<ToolCallResult> {
    let mut results = Vec::with_capacity(calls.len());

    for call in calls {
        let _ = ctx.delta_tx.send(StreamDelta::ToolProgress {
            tool: call.name.clone(),
            status: "running".into(),
        }).await;

        let result = match registry.get(&call.name) {
            Some(tool) => {
                tool.execute(call.arguments.clone(), ctx).await
                    .unwrap_or_else(|e| ToolResult::error(e.to_string()))
            }
            None => ToolResult::error(format!("unknown tool: {}", call.name)),
        };

        results.push(ToolCallResult {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            result,
        });
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_call_not_parallel() {
        let reg = ToolRegistry::new();
        let calls = vec![ToolCall {
            id: "c1".into(),
            name: "search".into(),
            arguments: serde_json::json!({}),
        }];
        assert!(!should_parallelize(&calls, &reg));
    }

    #[test]
    fn test_clarify_blocks_parallelization() {
        let reg = ToolRegistry::new();
        let calls = vec![
            ToolCall { id: "c1".into(), name: "search".into(), arguments: serde_json::json!({}) },
            ToolCall { id: "c2".into(), name: "clarify".into(), arguments: serde_json::json!({}) },
        ];
        assert!(!should_parallelize(&calls, &reg));
    }

    #[test]
    fn test_no_conflicts_allows_parallel() {
        let reg = ToolRegistry::new();
        let calls = vec![
            ToolCall { id: "c1".into(), name: "search".into(), arguments: serde_json::json!({"query": "a"}) },
            ToolCall { id: "c2".into(), name: "search".into(), arguments: serde_json::json!({"query": "b"}) },
        ];
        assert!(should_parallelize(&calls, &reg));
    }

    #[test]
    fn test_has_path_conflicts_same_path() {
        assert!(has_path_conflicts(&["/tmp/a.txt", "/tmp/a.txt"]));
    }

    #[test]
    fn test_has_path_conflicts_prefix() {
        assert!(has_path_conflicts(&["/tmp", "/tmp/a.txt"]));
    }

    #[test]
    fn test_no_path_conflicts() {
        assert!(!has_path_conflicts(&["/tmp/a.txt", "/tmp/b.txt"]));
    }

    #[test]
    fn test_empty_paths_no_conflict() {
        assert!(!has_path_conflicts(&[]));
    }
}
```

- [ ] **Step 2: Write the Agent loop**

Create `crates/hermes-agent/src/loop_runner.rs`:

```rust
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use hermes_core::error::{HermesError, Result};
use hermes_core::message::{Content, Message, Role};
use hermes_core::provider::{ChatRequest, Provider};
use hermes_core::stream::StreamDelta;
use hermes_core::tool::ToolContext;
use hermes_tools::ToolRegistry;

use crate::budget::IterationBudget;
use crate::parallel::{self, ToolCallResult};

/// The agent that drives the LLM conversation loop.
pub struct Agent {
    provider: Arc<dyn Provider>,
    registry: Arc<ToolRegistry>,
    budget: IterationBudget,
    system_prompt: String,
    session_id: String,
}

/// Configuration for creating an Agent.
pub struct AgentConfig {
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<ToolRegistry>,
    pub max_iterations: u32,
    pub system_prompt: String,
    pub session_id: String,
}

impl Agent {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            provider: config.provider,
            registry: config.registry,
            budget: IterationBudget::new(config.max_iterations),
            system_prompt: config.system_prompt,
            session_id: config.session_id,
        }
    }

    /// Run a conversation turn: send the user message, loop until LLM returns text
    /// (no more tool calls) or budget is exhausted.
    pub async fn run_conversation(
        &mut self,
        user_message: &str,
        history: &mut Vec<Message>,
        delta_tx: &mpsc::Sender<StreamDelta>,
    ) -> Result<String> {
        history.push(Message::user(user_message));

        let tool_ctx = ToolContext {
            session_id: self.session_id.clone(),
            working_dir: std::env::current_dir().unwrap_or_default(),
            approval_tx: {
                // Placeholder: auto-deny channel (CLI will replace with real UI)
                let (tx, _rx) = mpsc::channel(1);
                tx
            },
            delta_tx: delta_tx.clone(),
        };

        let mut final_response = String::new();

        while self.budget.try_consume() {
            let schemas = self.registry.available_schemas();
            let request = ChatRequest {
                system: &self.system_prompt,
                system_segments: None,
                messages: history.as_slice(),
                tools: &schemas,
                max_tokens: self.provider.model_info().max_output as u32,
                temperature: 0.7,
                reasoning: self.provider.supports_reasoning(),
                stop_sequences: vec![],
            };

            debug!(
                remaining = self.budget.remaining(),
                "sending LLM request"
            );

            let response = self.provider.chat(&request, Some(delta_tx)).await?;

            // Build assistant message
            let assistant_msg = Message {
                role: Role::Assistant,
                content: Content::Text(response.content.clone()),
                tool_calls: response.tool_calls.clone(),
                reasoning: response.reasoning.clone(),
                name: None,
                tool_call_id: None,
            };
            history.push(assistant_msg);

            // No tool calls -> done
            if response.tool_calls.is_empty() {
                final_response = response.content;
                break;
            }

            // Execute tools
            info!(
                count = response.tool_calls.len(),
                "executing tool calls"
            );

            let results = if parallel::should_parallelize(&response.tool_calls, &self.registry) {
                parallel::execute_parallel(&response.tool_calls, &self.registry, &tool_ctx).await
            } else {
                parallel::execute_sequential(&response.tool_calls, &self.registry, &tool_ctx).await
            };

            // Append tool results to history
            for result in results {
                history.push(Message {
                    role: Role::Tool,
                    content: Content::Text(result.result.content),
                    tool_calls: vec![],
                    reasoning: None,
                    name: Some(result.tool_name),
                    tool_call_id: Some(result.call_id),
                });
            }
        }

        if final_response.is_empty() && self.budget.is_exhausted() {
            warn!("agent loop ended: iteration budget exhausted");
            final_response = "[iteration budget exhausted]".into();
        }

        Ok(final_response)
    }

    /// Remaining iteration budget.
    pub fn remaining_budget(&self) -> u32 {
        self.budget.remaining()
    }

    /// Refund iterations (for execute_code tool).
    pub fn refund_budget(&mut self, n: u32) {
        self.budget.refund(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use hermes_core::message::ToolCall;
    use hermes_core::provider::*;

    /// A mock provider that returns a fixed response.
    struct MockProvider {
        responses: std::sync::Mutex<Vec<ChatResponse>>,
    }

    impl MockProvider {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat(
            &self,
            _request: &ChatRequest<'_>,
            _delta_tx: Option<&mpsc::Sender<StreamDelta>>,
        ) -> Result<ChatResponse> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(ChatResponse {
                    content: "no more responses".into(),
                    tool_calls: vec![],
                    reasoning: None,
                    finish_reason: FinishReason::Stop,
                    usage: TokenUsage::default(),
                    cache_meta: None,
                })
            } else {
                Ok(responses.remove(0))
            }
        }

        fn model_info(&self) -> &ModelInfo {
            // Leaked for test convenience
            &ModelInfo {
                id: "mock".into(),
                provider: "mock".into(),
                max_context: 128_000,
                max_output: 4096,
                supports_tools: true,
                supports_vision: false,
                supports_reasoning: false,
                supports_caching: false,
                pricing: ModelPricing {
                    input_per_mtok: 0.0,
                    output_per_mtok: 0.0,
                    cache_read_per_mtok: 0.0,
                    cache_create_per_mtok: 0.0,
                },
            }
        }
    }

    #[tokio::test]
    async fn test_simple_conversation_no_tools() {
        let provider = Arc::new(MockProvider::new(vec![ChatResponse {
            content: "Hello! How can I help?".into(),
            tool_calls: vec![],
            reasoning: None,
            finish_reason: FinishReason::Stop,
            usage: TokenUsage::default(),
            cache_meta: None,
        }]));

        let registry = Arc::new(ToolRegistry::new());
        let (tx, _rx) = mpsc::channel(64);

        let mut agent = Agent::new(AgentConfig {
            provider,
            registry,
            max_iterations: 10,
            system_prompt: "You are helpful.".into(),
            session_id: "test".into(),
        });

        let mut history = Vec::new();
        let result = agent
            .run_conversation("Hi", &mut history, &tx)
            .await
            .unwrap();

        assert_eq!(result, "Hello! How can I help?");
        assert_eq!(history.len(), 2); // user + assistant
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[1].role, Role::Assistant);
    }

    #[tokio::test]
    async fn test_tool_call_then_response() {
        let provider = Arc::new(MockProvider::new(vec![
            // First response: tool call
            ChatResponse {
                content: "Let me search for that.".into(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "unknown_tool".into(),
                    arguments: serde_json::json!({"query": "rust"}),
                }],
                reasoning: None,
                finish_reason: FinishReason::ToolUse,
                usage: TokenUsage::default(),
                cache_meta: None,
            },
            // Second response: final text
            ChatResponse {
                content: "Rust is a systems programming language.".into(),
                tool_calls: vec![],
                reasoning: None,
                finish_reason: FinishReason::Stop,
                usage: TokenUsage::default(),
                cache_meta: None,
            },
        ]));

        let registry = Arc::new(ToolRegistry::new());
        let (tx, _rx) = mpsc::channel(64);

        let mut agent = Agent::new(AgentConfig {
            provider,
            registry,
            max_iterations: 10,
            system_prompt: "".into(),
            session_id: "test".into(),
        });

        let mut history = Vec::new();
        let result = agent
            .run_conversation("What is Rust?", &mut history, &tx)
            .await
            .unwrap();

        assert_eq!(result, "Rust is a systems programming language.");
        // user + assistant(tool_call) + tool(result) + assistant(final)
        assert_eq!(history.len(), 4);
        assert_eq!(history[2].role, Role::Tool);
        assert!(history[2].content.as_text_lossy().contains("unknown tool"));
    }

    #[tokio::test]
    async fn test_budget_exhaustion() {
        // Provider always returns tool calls, so budget should exhaust
        let provider = Arc::new(MockProvider::new(vec![
            ChatResponse {
                content: "".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "x".into(),
                    arguments: serde_json::json!({}),
                }],
                reasoning: None,
                finish_reason: FinishReason::ToolUse,
                usage: TokenUsage::default(),
                cache_meta: None,
            },
            ChatResponse {
                content: "".into(),
                tool_calls: vec![ToolCall {
                    id: "c2".into(),
                    name: "x".into(),
                    arguments: serde_json::json!({}),
                }],
                reasoning: None,
                finish_reason: FinishReason::ToolUse,
                usage: TokenUsage::default(),
                cache_meta: None,
            },
        ]));

        let registry = Arc::new(ToolRegistry::new());
        let (tx, _rx) = mpsc::channel(64);

        let mut agent = Agent::new(AgentConfig {
            provider,
            registry,
            max_iterations: 2,
            system_prompt: "".into(),
            session_id: "test".into(),
        });

        let mut history = Vec::new();
        let result = agent
            .run_conversation("do stuff", &mut history, &tx)
            .await
            .unwrap();

        assert!(result.contains("budget exhausted"));
        assert!(agent.remaining_budget() == 0);
    }
}
```

- [ ] **Step 3: Update lib.rs**

Replace `crates/hermes-agent/src/lib.rs`:

```rust
pub mod budget;
pub mod loop_runner;
pub mod parallel;

pub use loop_runner::{Agent, AgentConfig};
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p hermes-agent -- --nocapture`

Expected: All tests PASS (budget + parallel + loop tests).

- [ ] **Step 5: Commit**

```bash
git add crates/hermes-agent/src/parallel.rs crates/hermes-agent/src/loop_runner.rs crates/hermes-agent/src/lib.rs
git commit -m "feat: implement agent loop with parallel tool execution and budget control"
```

---

## Task 11: Minimal Config

Minimal config for Phase 1: just enough to get the CLI running. Model selection, API key from env var, hermes_home().

**Files:**
- Create: `crates/hermes-config/src/config.rs`
- Modify: `crates/hermes-config/src/lib.rs`
- Modify: `crates/hermes-config/Cargo.toml` (trim deps)

- [ ] **Step 1: Write minimal config with tests**

Create `crates/hermes-config/src/config.rs`:

```rust
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Resolve the Hermes home directory.
/// Priority: $HERMES_HOME > ~/.hermes
pub fn hermes_home() -> PathBuf {
    std::env::var("HERMES_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .expect("could not determine home directory")
                .join(".hermes")
        })
}

/// Top-level application config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Model string like "anthropic/claude-sonnet-4-20250514"
    #[serde(default = "default_model")]
    pub model: String,

    /// Max iterations per conversation turn
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,

    /// Temperature for LLM requests
    #[serde(default = "default_temperature")]
    pub temperature: f32,
}

fn default_model() -> String {
    "anthropic/claude-sonnet-4-20250514".into()
}

fn default_max_iterations() -> u32 {
    90
}

fn default_temperature() -> f32 {
    0.7
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            max_iterations: default_max_iterations(),
            temperature: default_temperature(),
        }
    }
}

impl AppConfig {
    /// Load config from ~/.hermes/config.yaml, falling back to defaults.
    pub fn load() -> Self {
        let path = hermes_home().join("config.yaml");

        if !path.exists() {
            return Self::default();
        }

        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_yaml_ng::from_str(&raw).unwrap_or_else(|e| {
                tracing::warn!("failed to parse config: {e}, using defaults");
                Self::default()
            }),
            Err(e) => {
                tracing::warn!("failed to read config: {e}, using defaults");
                Self::default()
            }
        }
    }

    /// Get the API key from environment variables.
    /// Checks provider-specific vars first, then generic HERMES_API_KEY.
    pub fn api_key(&self) -> Option<String> {
        let provider = self.model.split('/').next().unwrap_or("openai");
        let provider_var = match provider {
            "anthropic" => "ANTHROPIC_API_KEY",
            "openai" => "OPENAI_API_KEY",
            "openrouter" => "OPENROUTER_API_KEY",
            _ => "HERMES_API_KEY",
        };

        std::env::var(provider_var)
            .or_else(|_| std::env::var("HERMES_API_KEY"))
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert!(config.model.contains("claude"));
        assert_eq!(config.max_iterations, 90);
        assert!((config.temperature - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn test_config_serde_roundtrip() {
        let config = AppConfig {
            model: "openai/gpt-4o".into(),
            max_iterations: 50,
            temperature: 0.5,
        };
        let yaml = serde_yaml_ng::to_string(&config).unwrap();
        let back: AppConfig = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(back.model, "openai/gpt-4o");
        assert_eq!(back.max_iterations, 50);
    }

    #[test]
    fn test_hermes_home_default() {
        // Remove env var to test default
        std::env::remove_var("HERMES_HOME");
        let home = hermes_home();
        assert!(home.ends_with(".hermes"));
    }

    #[test]
    fn test_hermes_home_env_override() {
        std::env::set_var("HERMES_HOME", "/tmp/test-hermes");
        let home = hermes_home();
        assert_eq!(home, PathBuf::from("/tmp/test-hermes"));
        std::env::remove_var("HERMES_HOME");
    }
}
```

- [ ] **Step 2: Add dirs dependency**

Add to `[workspace.dependencies]` in root `Cargo.toml`:

```toml
dirs = "6"
```

Add to `crates/hermes-config/Cargo.toml` `[dependencies]`:

```toml
dirs = { workspace = true }
```

- [ ] **Step 3: Trim hermes-config Cargo.toml**

Replace `crates/hermes-config/Cargo.toml`:

```toml
[package]
name = "hermes-config"
description = "Configuration loading, session storage (SQLite + FTS5)"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
hermes-core.workspace = true
serde.workspace = true
serde_json.workspace = true
serde_yaml_ng.workspace = true
tracing.workspace = true
dirs.workspace = true
```

- [ ] **Step 4: Update lib.rs**

Replace `crates/hermes-config/src/lib.rs`:

```rust
pub mod config;

pub use config::{AppConfig, hermes_home};
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p hermes-config -- --nocapture`

Expected: All 4 tests PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/hermes-config/src/config.rs crates/hermes-config/src/lib.rs crates/hermes-config/Cargo.toml Cargo.toml
git commit -m "feat: implement minimal config with YAML loading and env var support"
```

---

## Task 12: CLI REPL

Minimal interactive REPL using rustyline for input and crossterm for colored streaming output. Connects to the agent loop and renders deltas in real time.

**Files:**
- Create: `crates/hermes-cli/src/render.rs`
- Create: `crates/hermes-cli/src/repl.rs`
- Modify: `crates/hermes-cli/src/main.rs`
- Modify: `crates/hermes-cli/Cargo.toml` (trim deps)

- [ ] **Step 1: Write the stream renderer**

Create `crates/hermes-cli/src/render.rs`:

```rust
use std::io::{self, Write};

use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use crossterm::ExecutableCommand;
use tokio::sync::mpsc;

use hermes_core::stream::StreamDelta;

/// Renders StreamDelta events to the terminal with colors.
pub async fn render_stream(mut rx: mpsc::Receiver<StreamDelta>) {
    let mut stdout = io::stdout();
    let mut in_tool = false;

    while let Some(delta) = rx.recv().await {
        match delta {
            StreamDelta::TextDelta(text) => {
                if in_tool {
                    // Close tool indicator before printing text
                    let _ = stdout.execute(ResetColor);
                    let _ = writeln!(stdout);
                    in_tool = false;
                }
                let _ = stdout.execute(Print(&text));
                let _ = stdout.flush();
            }
            StreamDelta::ReasoningDelta(text) => {
                let _ = stdout.execute(SetForegroundColor(Color::DarkGrey));
                let _ = stdout.execute(Print(&text));
                let _ = stdout.execute(ResetColor);
                let _ = stdout.flush();
            }
            StreamDelta::ToolCallStart { name, .. } => {
                let _ = stdout.execute(SetForegroundColor(Color::Yellow));
                let _ = write!(stdout, "\n[tool: {name}] ");
                let _ = stdout.flush();
                in_tool = true;
            }
            StreamDelta::ToolCallArgsDelta { .. } => {
                // Don't render arg fragments — too noisy
            }
            StreamDelta::ToolProgress { tool, status } => {
                let _ = stdout.execute(SetForegroundColor(Color::Cyan));
                let _ = write!(stdout, "[{tool}: {status}] ");
                let _ = stdout.execute(ResetColor);
                let _ = stdout.flush();
            }
            StreamDelta::Done => {
                if in_tool {
                    let _ = stdout.execute(ResetColor);
                    in_tool = false;
                }
                let _ = writeln!(stdout);
                let _ = stdout.flush();
                break;
            }
        }
    }
}
```

- [ ] **Step 2: Write the REPL loop**

Create `crates/hermes-cli/src/repl.rs`:

```rust
use std::sync::Arc;

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use secrecy::SecretString;
use tokio::sync::mpsc;

use hermes_agent::{Agent, AgentConfig};
use hermes_config::AppConfig;
use hermes_core::message::Message;
use hermes_provider::create_provider;
use hermes_tools::ToolRegistry;

use crate::render::render_stream;

/// Run the interactive REPL.
pub async fn run_repl() -> anyhow::Result<()> {
    let config = AppConfig::load();

    let api_key = config.api_key().ok_or_else(|| {
        anyhow::anyhow!(
            "No API key found. Set ANTHROPIC_API_KEY, OPENAI_API_KEY, or HERMES_API_KEY"
        )
    })?;

    let provider = create_provider(
        &config.model,
        SecretString::from(api_key),
        None,
    )?;

    let registry = Arc::new(ToolRegistry::from_inventory());

    let system_prompt = "You are Hermes, a helpful AI assistant. \
        Answer questions concisely and accurately."
        .to_string();

    let mut agent = Agent::new(AgentConfig {
        provider,
        registry,
        max_iterations: config.max_iterations,
        system_prompt,
        session_id: uuid::Uuid::new_v4().to_string(),
    });

    let mut history: Vec<Message> = Vec::new();

    // Print banner
    let model = &config.model;
    println!("Hermes Agent (Rust) — model: {model}");
    println!("Type /quit to exit.\n");

    // rustyline is blocking — run on a blocking thread
    let (input_tx, mut input_rx) = mpsc::channel::<String>(1);

    // Spawn readline on blocking thread
    tokio::task::spawn_blocking(move || {
        let mut rl = DefaultEditor::new().expect("failed to create editor");
        let history_path = hermes_config::hermes_home().join("cli_history.txt");
        let _ = rl.load_history(&history_path);

        loop {
            match rl.readline(">>> ") {
                Ok(line) => {
                    let trimmed = line.trim().to_string();
                    if trimmed.is_empty() {
                        continue;
                    }
                    rl.add_history_entry(&trimmed).ok();
                    if input_tx.blocking_send(trimmed).is_err() {
                        break;
                    }
                }
                Err(ReadlineError::Interrupted | ReadlineError::Eof) => {
                    let _ = input_tx.blocking_send("/quit".into());
                    break;
                }
                Err(e) => {
                    eprintln!("readline error: {e}");
                    break;
                }
            }
        }

        let _ = rl.save_history(&history_path);
    });

    // Main loop: receive input, run agent, render output
    while let Some(input) = input_rx.recv().await {
        if input == "/quit" || input == "/exit" || input == "/q" {
            println!("Goodbye!");
            break;
        }

        if input == "/new" {
            history.clear();
            println!("--- new conversation ---\n");
            continue;
        }

        if input == "/help" {
            println!("Commands:");
            println!("  /quit, /q, /exit  — exit");
            println!("  /new              — clear history");
            println!("  /help             — show this");
            println!();
            continue;
        }

        let (delta_tx, delta_rx) = mpsc::channel(256);

        // Spawn renderer
        let render_handle = tokio::spawn(render_stream(delta_rx));

        // Run agent
        match agent.run_conversation(&input, &mut history, &delta_tx).await {
            Ok(_response) => {
                // Response was already streamed
            }
            Err(e) => {
                eprintln!("\nerror: {e}\n");
            }
        }

        // Drop sender to signal Done to renderer, then wait
        drop(delta_tx);
        let _ = render_handle.await;
    }

    Ok(())
}
```

- [ ] **Step 3: Write main.rs**

Replace `crates/hermes-cli/src/main.rs`:

```rust
use clap::Parser;

mod render;
mod repl;

/// Hermes Agent — AI assistant CLI
#[derive(Parser)]
#[command(name = "hermes", version)]
struct Cli {
    /// Single message (non-interactive mode)
    #[arg(short, long)]
    message: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("hermes=info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();

    if let Some(_msg) = cli.message {
        // TODO: single-message mode
        eprintln!("Single message mode not yet implemented. Use interactive mode.");
        return Ok(());
    }

    repl::run_repl().await
}
```

- [ ] **Step 4: Trim CLI Cargo.toml**

Replace `crates/hermes-cli/Cargo.toml`:

```toml
[package]
name = "hermes-cli"
description = "Interactive REPL CLI for Hermes Agent"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[[bin]]
name = "hermes"
path = "src/main.rs"

[dependencies]
hermes-core.workspace = true
hermes-agent.workspace = true
hermes-provider.workspace = true
hermes-tools.workspace = true
hermes-config.workspace = true
serde.workspace = true
serde_json.workspace = true
anyhow.workspace = true
tokio.workspace = true
clap.workspace = true
rustyline.workspace = true
crossterm.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
uuid.workspace = true
secrecy.workspace = true
```

- [ ] **Step 5: Verify it compiles**

Run: `cargo build -p hermes-cli 2>&1`

Expected: Compiles without errors. (We can't run tests for the REPL since it's interactive, but it must compile.)

- [ ] **Step 6: Commit**

```bash
git add crates/hermes-cli/src/main.rs crates/hermes-cli/src/render.rs crates/hermes-cli/src/repl.rs crates/hermes-cli/Cargo.toml
git commit -m "feat: implement minimal CLI REPL with streaming output"
```

---

## Task 13: Full Build Verification & Clippy

Ensure the entire workspace compiles, passes clippy, and all tests pass.

**Files:**
- No new files

- [ ] **Step 1: Run cargo clippy on entire workspace**

Run: `cargo clippy --workspace -- -D warnings 2>&1`

Fix any warnings. This may require minor adjustments to imports, unused variables, etc.

- [ ] **Step 2: Run cargo fmt check**

Run: `cargo fmt --check 2>&1`

Fix any formatting issues.

- [ ] **Step 3: Run all tests**

Run: `cargo test --workspace 2>&1`

Expected: All tests PASS across all crates.

- [ ] **Step 4: Commit any fixes**

```bash
git add -A
git commit -m "chore: fix clippy warnings and formatting"
```

---

## Task 14: Smoke Test with Real API (Manual)

Manual verification that the full pipeline works end-to-end.

**Files:**
- No files changed

- [ ] **Step 1: Build in release mode**

Run: `cargo build --release -p hermes-cli 2>&1`

- [ ] **Step 2: Run with API key**

Run: `ANTHROPIC_API_KEY=<key> ./target/release/hermes`

Expected:
- Banner prints with model info
- Prompt `>>> ` appears
- Type "What is 2+2?" — get streamed response
- Type `/new` — clears history
- Type `/quit` — exits cleanly

- [ ] **Step 3: Commit tag**

```bash
git tag v0.1.0-phase1
```

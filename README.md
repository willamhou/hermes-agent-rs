# Hermes Agent RS

A high-performance AI agent framework in Rust. Single binary, 26 MB, 0 dependencies to install.

Features streaming tool-calling agents with 17 built-in tools, multi-platform gateway, OpenAI-compatible API, cron scheduling, MCP integration, and 78 bundled skills.

## Quick Start

```bash
# Prerequisites: Rust 1.85+, one API key
export ANTHROPIC_API_KEY=sk-...  # or OPENAI_API_KEY / OPENROUTER_API_KEY

# Interactive REPL
cargo run --release -p hermes-cli

# One-shot
cargo run --release -p hermes-cli -- -m "Explain this repo"

# With a specific model
cargo run --release -p hermes-cli -- -m "Hello" --model openai/gpt-4o
```

## What It Does

```
User message
    ↓
┌─────────────────────────────────────────────┐
│  Agent Loop                                 │
│  ┌───────┐   ┌──────────┐   ┌───────────┐  │
│  │Prompt │──▶│ Provider │──▶│ Tool Exec │  │
│  │Builder│   │(stream)  │   │(parallel) │  │
│  └───────┘   └──────────┘   └───────────┘  │
│       ↑                           │         │
│       └───────────────────────────┘         │
│  Context compression when token limit hit   │
└─────────────────────────────────────────────┘
    ↓
Streaming response (CLI / SSE / Telegram / ...)
```

## Features

### Agent Core
- **Streaming tool-calling loop** with iteration budget and parallel tool execution
- **Providers**: Anthropic, OpenAI Chat Completions, OpenAI Responses, OpenRouter, any OpenAI-compatible endpoint
- **5-phase context compression**: tool pruning → boundary detection → LLM summarization → history rebuild → tool-pair sanitization
- **Prompt caching** (Anthropic) with hash-based freeze/invalidate
- **Delegation**: spawn child agents with restricted tools and depth limit

### 17 Built-in Tools

| Category | Tools |
|----------|-------|
| Terminal | `terminal` (shell exec, dangerous command detection, approval flow) |
| File | `read_file`, `write_file`, `search_files`, `patch` (find-and-replace) |
| Web | `web_search` (Tavily), `web_extract` (URL → text) |
| Browser | `browser` (CDP headless: navigate, click, type, snapshot) |
| Vision | `vision_analyze` (image analysis via provider) |
| Memory | `memory_read`, `memory_write` (persistent MEMORY.md / USER.md) |
| Skills | `skill_list`, `skill_view`, `skill_manage` |
| Code | `execute_code` (Python, opt-in via `HERMES_ENABLE_EXECUTE_CODE=1`) |
| Interaction | `clarify` (ask user questions with multiple choice) |
| Scheduling | `cron` (create/list/remove/pause/resume/trigger jobs) |
| Delegation | `delegate_task` (spawn sub-agent) |
| MCP | Dynamic tools from configured MCP servers |

### Gateway
- **Telegram** — long-polling with user allowlist, message splitting, exponential backoff
- **API Server** — OpenAI-compatible `/v1/chat/completions` (SSE streaming), `/v1/models`, plus legacy REST
- Per-session agent isolation with idle cleanup
- Cron scheduler (60s tick loop, file-locked)

### CLI (21 Commands)

```
/help       /quit       /new        /clear      /model
/tools      /status     /retry      /undo       /compress
/skills     /save       /cron       /search     /config
/provider   /usage      /yolo       /title      /toolsets
/verbose
```

### Additional Systems
- **Memory**: file-backed MEMORY.md + USER.md with frozen snapshot pattern, prefetch/sync
- **Skills**: 78 bundled skills, auto-discovery, lexical matching, context injection
- **Sessions**: SQLite persistence + FTS5 full-text search, resume support
- **MCP Client**: stdio + HTTP transports, JSON-RPC 2.0, capabilities negotiation

## Architecture

```
crates/
├── hermes-core       # Traits: Tool, Provider, SessionStore, PlatformAdapter, MemoryAccess
├── hermes-provider   # Anthropic, OpenAI, OpenRouter — SSE streaming, retry, tool-call assembly
├── hermes-tools      # 13 tools + ToolRegistry (inventory compile-time registration)
├── hermes-agent      # Agent loop, compression, delegation, prompt caching, token counter
├── hermes-memory     # BuiltinMemory, MemoryManager, prefetch cache
├── hermes-skills     # SkillManager, matching, skill tools
├── hermes-config     # YAML config, SQLite session store (WAL + FTS5)
├── hermes-mcp        # MCP client (stdio + HTTP), tool/prompt/resource bridges
├── hermes-cron       # CronJob, JobStore, CronScheduler, CronTool
├── hermes-gateway    # GatewayRunner, SessionRouter, Telegram + API Server adapters
└── hermes-cli        # Binary: REPL, oneshot, session management, approval UI
```

## Configuration

Config file: `~/.hermes/config.yaml` (or `$HERMES_HOME/config.yaml`)

```yaml
model: anthropic/claude-sonnet-4-20250514
max_iterations: 90
temperature: 0.7

terminal:
  timeout: 180
  max_timeout: 600
  output_max_chars: 50000

file:
  read_max_chars: 100000
  read_max_lines: 2000

browser:
  headless: true
  sandbox: true

approval:
  policy: ask  # ask | yolo | deny

mcp_servers:
  - name: filesystem
    command: npx
    args: ["-y", "@modelcontextprotocol/server-filesystem", "."]
  - name: remote
    transport: http
    url: https://mcp.example.com
    headers:
      Authorization: "Bearer ${MCP_TOKEN}"

gateway:
  session_idle_timeout_secs: 1800
  max_concurrent_sessions: 100
  telegram:
    token: "${TELEGRAM_BOT_TOKEN}"
    allowed_users: ["user1", "12345678"]
    # allow_all: true
  api_server:
    bind_addr: "127.0.0.1:8080"
    api_key: "${HERMES_API_KEY}"
```

## OpenAI-Compatible API

Start the gateway, then use any OpenAI-compatible client:

```bash
# Start
cargo run --release -p hermes-cli -- gateway

# Use with curl
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer $HERMES_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "messages": [{"role": "user", "content": "Hello"}],
    "stream": true
  }'

# Use with Open WebUI, LobeChat, LibreChat, ChatBox, etc.
# Set base URL to http://localhost:8080/v1
```

## Gateway (Telegram)

```bash
# Set up in config.yaml, then:
cargo run --release -p hermes-cli -- gateway
```

The bot responds to allowed users. Each user gets an isolated agent session with its own conversation history. Dangerous commands are auto-denied in gateway mode.

## Security Model

| Context | Dangerous Commands | Rationale |
|---------|-------------------|-----------|
| CLI | User prompted (y/s/a/n) | Interactive, user in control |
| Gateway | Auto-denied | Non-interactive, no human in loop |
| Cron | Auto-denied | Scheduled, no human in loop |
| Delegation | Auto-denied | Sub-agent, no human in loop |

Additional protections:
- **Path sandbox**: all file operations checked against workspace root
- **SecretString**: API keys never logged or serialized
- **Constant-time auth**: API key comparison resists timing attacks
- **Session cap**: 500 message hard limit with forced compression
- **MCP**: tool discovery gated on server capability negotiation

## Development

```bash
# Format
cargo fmt

# Lint
cargo clippy --workspace -- -D warnings

# Test (341 tests)
cargo test --workspace

# Build release binary (~26 MB)
cargo build --release -p hermes-cli
```

## License

MIT

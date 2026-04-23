# Hermes Agent RS

A high-performance AI agent framework in Rust. Single binary, 26 MB, 0 dependencies to install.

Features streaming tool-calling agents with 17 built-in tools, multi-platform gateway, OpenAI-compatible API, managed-agents beta control plane, cron scheduling, MCP integration, and 78 bundled skills.

[![Demo](https://asciinema.org/a/6QEKM06EO9iwn30j.svg)](https://asciinema.org/a/6QEKM06EO9iwn30j)

## Quick Start

```bash
# Prerequisites: Rust 1.86+, one API key
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
- **API Server** — OpenAI-compatible `/v1/chat/completions` (SSE streaming), `/v1/models`, managed `/v1/agents` and `/v1/runs`, plus legacy REST
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
├── hermes-tools      # 17 tools + ToolRegistry (inventory compile-time registration)
├── hermes-agent      # Agent loop, compression, delegation, prompt caching, token counter
├── hermes-memory     # BuiltinMemory, MemoryManager, prefetch cache
├── hermes-skills     # SkillManager, matching, skill tools
├── hermes-config     # YAML config, SQLite session store (WAL + FTS5)
├── hermes-mcp        # MCP client (stdio + HTTP), tool/prompt/resource bridges
├── hermes-cron       # CronJob, JobStore, CronScheduler, CronTool
├── hermes-managed    # Managed agents domain, store, run registry, runtime filtering
├── hermes-gateway    # GatewayRunner, SessionRouter, Telegram + API Server adapters
└── hermes-cli        # Binary: REPL, oneshot, session management, approval UI
```

## Configuration

Config file: `~/.hermes/config.yaml` (or `$HERMES_HOME/config.yaml`)

Hermes also loads `~/.hermes/.env` before parsing config. `HERMES_MODEL` and
`HERMES_BASE_URL` can override the YAML values, and provider keys can live there too.

For OpenAI-compatible endpoints, including NewAPI-style gateways, the simplest setup is:

```bash
cat >> ~/.hermes/.env <<'EOF'
HERMES_MODEL=openai/gpt-4.1-mini
HERMES_BASE_URL=https://your-openai-compatible-host/v1
HERMES_API_KEY=sk-...
EOF
```

`HERMES_MODEL` may also be a bare model name such as `gpt-4.1-mini` when you want
OpenAI-compatible routing with a custom `HERMES_BASE_URL`.

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

Managed agents use the same endpoint with `model: "agent:<name>"`.
When publishing managed-agent versions, omitted `model` and `base_url` fields inherit the
current global Hermes config, including `HERMES_MODEL` and `HERMES_BASE_URL` from `~/.hermes/.env`.

## Managed Agents Beta

The current managed-agents surface is a beta control plane, not hosted-platform parity.

What exists today:
- agent CRUD plus immutable versions
- invocation through `model: "agent:<name>"`
- per-agent tool and skill allowlists
- persisted run timelines via `GET /v1/runs/{id}/events`
- durable run replay via `POST /v1/runs/{id}/replay`
- startup reconciliation that marks runs left active during process exit as `failed` or `cancelled`
- optional Signet receipts for managed tool calls, appended to a local audit chain
- best-effort run cancellation through `DELETE /v1/runs/:id`
- CLI `hermes agents ...` commands plus YAML `diff` / `sync`

Current non-goals:
- hard real-time cancellation guarantees
- MCP in managed mode
- hosted control plane, remote KMS, or managed audit pipeline
- restart-safe in-flight resume, RBAC, or multi-tenant namespaces

Example YAML:

```yaml
name: code-reviewer
system_prompt: |
  Review code carefully and explain concrete risks.
allowed_tools:
  - read_file
  - search_files
  - patch
allowed_skills: []
max_iterations: 90
temperature: 0.0
approval_policy: ask
timeout_secs: 300
```

If you omit `model` or `base_url` in YAML or version-create requests, Hermes resolves and stores
the current global defaults at publish time. That keeps each immutable version pinned even if your
global `.env` changes later.

The same sample lives at [examples/code-reviewer.yaml](examples/code-reviewer.yaml).

Example flow:

```bash
# Sync one or more YAML specs from ~/.hermes/agents
cargo run --release -p hermes-cli -- agents sync --dry-run
cargo run --release -p hermes-cli -- agents sync --yes

# Or create directly from the CLI. `--model` and `--base-url` are optional;
# when omitted they inherit the current ~/.hermes/.env values.
cargo run --release -p hermes-cli -- agents create code-reviewer
cargo run --release -p hermes-cli -- agents versions create code-reviewer \
  --system-prompt "Review code carefully and explain concrete risks." \
  --tool read_file \
  --tool search_files \
  --tool patch

# Inspect managed runs and follow persisted timeline events.
cargo run --release -p hermes-cli -- runs list
cargo run --release -p hermes-cli -- runs list --json
cargo run --release -p hermes-cli -- runs get <run-id>
cargo run --release -p hermes-cli -- runs get <run-id> --json
cargo run --release -p hermes-cli -- runs replay <run-id>
cargo run --release -p hermes-cli -- runs events <run-id> --json
cargo run --release -p hermes-cli -- runs events <run-id> --follow
cargo run --release -p hermes-cli -- runs verify <run-id>
cargo run --release -p hermes-cli -- runs verify <run-id> --json
cargo run --release -p hermes-cli -- runs verify <run-id> --strict
cargo run --release -p hermes-cli -- runs verify <run-id> --quiet --strict

# CI-friendly Signet verification helpers
# Signet verification is meaningful only for runs that actually executed at least one managed tool.
bash examples/verify-managed-run.sh --latest
bash examples/verify-managed-run.sh --run <run-id> --wait
bash examples/verify-managed-run.sh --agent code-reviewer --json
bash examples/replay-managed-run.sh --agent code-reviewer
```

Optional Signet integration for managed runtimes:

```yaml
signet:
  enabled: true
  key_name: hermes-managed
  owner: hermes
  # dir: /absolute/path/to/signet-data
```

When enabled, managed tool calls emit extra `tool.progress` timeline entries such as
structured `tool.request_signed` / `tool.response_signed` timeline events with receipt metadata,
and Hermes appends Signet audit files under
`~/.hermes/signet/audit` by default. Keys live under `~/.hermes/signet/keys`.

Minimal API walkthrough:

```bash
# Assumes gateway is already running and HERMES_API_KEY is set
# Use HERMES_GATEWAY_BASE_URL for the local gateway; keep HERMES_BASE_URL for the upstream model API.
# Override MANAGED_USER_PROMPT for a short deterministic smoke prompt when needed.
bash examples/managed-agents-beta.sh
```

That script walks through:
- `POST /v1/agents`
- `POST /v1/agents/{id}/versions`
- `POST /v1/chat/completions` with `model: "agent:<name>"`
- `GET /v1/runs`
- `GET /v1/runs/{id}/events`
- optional `DELETE /v1/runs/{id}` cancellation

The managed run API also supports `POST /v1/runs/{id}/replay` to enqueue a new run from the
original persisted prompt and immutable agent version.

The end-to-end API example lives at [examples/managed-agents-beta.sh](examples/managed-agents-beta.sh).
The CI-oriented verification helper lives at [examples/verify-managed-run.sh](examples/verify-managed-run.sh).
The replay helper lives at [examples/replay-managed-run.sh](examples/replay-managed-run.sh).
The repository workflow lives at [.github/workflows/managed-run-verify.yml](.github/workflows/managed-run-verify.yml).
It is bound to the GitHub Actions environment `managed-verify`, which should define
`HERMES_MODEL`, `HERMES_BASE_URL`, and `HERMES_API_KEY` as environment secrets.
The mirrored example file lives at [examples/github-actions-managed-run-verify.yml](examples/github-actions-managed-run-verify.yml).

See [docs/specs/2026-04-22-managed-agents-v1-beta-plan.md](docs/specs/2026-04-22-managed-agents-v1-beta-plan.md) for the current beta contract and non-goals.

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

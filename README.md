# Hermes Agent RS

Rust workspace for an agentic CLI assistant with provider abstraction, local tools, memory, context compression, and a growing skills system.

## Status

Current implementation includes:

- Interactive CLI and one-shot mode in `crates/hermes-cli`
- Agent loop with streaming, tool execution, prompt caching, and context compression
- Provider support for Anthropic, OpenAI, and OpenRouter-compatible endpoints
- Built-in tools for files, terminal, patching, memory, web search/extract, vision, and opt-in code execution
- Local memory snapshots plus request-local skill matching/injection
- SQLite-backed session history and resume support

Still in progress:

- MCP integration
- Multi-platform gateway adapters
- Interactive approval UI and approval memory
- Browser automation, delegation, and voice-related tools

## Workspace Layout

- `crates/hermes-cli`: REPL and one-shot binary (`hermes`)
- `crates/hermes-agent`: agent loop, budget, compression, cache handling
- `crates/hermes-provider`: model provider implementations
- `crates/hermes-tools`: built-in tool registry and tool implementations
- `crates/hermes-memory`: local memory manager and provider hook surface
- `crates/hermes-skills`: skill discovery, matching, and skill tools
- `crates/hermes-config`: config loading and SQLite session storage

## Quick Start

### Prerequisites

- Rust `1.85` or newer
- One provider API key:
  - `ANTHROPIC_API_KEY`
  - `OPENAI_API_KEY`
  - `OPENROUTER_API_KEY`
  - or `HERMES_API_KEY` as a fallback

### Run the CLI

```bash
cargo run -p hermes-cli
```

### Run a single prompt

```bash
cargo run -p hermes-cli -- \
  --message "Summarize this repository" \
  --model openai/gpt-4o-mini
```

### Resume or inspect sessions

```bash
cargo run -p hermes-cli -- --list-sessions
cargo run -p hermes-cli -- --resume
```

## Configuration

Hermes loads config from `$HERMES_HOME/config.yaml`, defaulting to `~/.hermes/config.yaml`.

Minimal example:

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
```

Optional environment setup:

```bash
export ANTHROPIC_API_KEY=...
# or OPENAI_API_KEY / OPENROUTER_API_KEY / HERMES_API_KEY
export HERMES_HOME="$HOME/.hermes"
```

## Testing

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Notes

- Dangerous tool approvals are still auto-allowed in the current CLI until an interactive approval flow lands.
- `execute_code` is disabled by default and is only exposed when `HERMES_ENABLE_EXECUTE_CODE=1`.
- Phase-by-phase design notes live under [`docs/specs`](docs/specs).

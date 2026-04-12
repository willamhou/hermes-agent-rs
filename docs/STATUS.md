# Status

## What Exists

- CLI REPL and one-shot execution
- Durable SQLite session history and resume support
- Provider adapters for Anthropic, OpenAI chat-compatible, OpenAI Responses, and OpenRouter-compatible endpoints
- Built-in tools for files, terminal, patching, memory, web search/extract, vision, and opt-in code execution
- Approval UI plus approval memory with `ask | yolo | deny` policy
- Local memory snapshots, context compression, prompt caching, and request-local skill injection
- MCP support for:
  - stdio transport
  - HTTP transport
  - tool discovery/execution
  - prompt bridge tools
  - resource bridge tools

## Current Priorities

- MCP notifications and runtime refresh for changing tool/prompt/resource lists
- MCP resource subscriptions
- Better documentation and usage examples for MCP workflows

## Still Missing From The Broader Design

- Multi-platform gateway adapters
- Browser automation
- Delegation / child-agent workflows
- Voice and transcription-related tools
- More complete product/docs polish around non-CLI entrypoints

## How To Read The Docs

- `README.md`: public/project entrypoint and current quick start
- `docs/specs/`: design history and phase-by-phase implementation notes
- `AGENTS.md`: repository working rules and architecture guardrails

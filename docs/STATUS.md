# Status

## What Exists

- CLI REPL and one-shot execution
- Durable SQLite transcript sessions with resume and FTS5 search
- Provider adapters for Anthropic, OpenAI chat-compatible, OpenAI Responses, and OpenRouter-compatible endpoints
- Built-in tools for files, terminal, patching, browser automation, memory, web search/extract, vision, delegation, cron, and opt-in code execution
- Approval UI plus approval memory with `ask | yolo | deny` policy
- Local memory snapshots, context compression, prompt caching, and request-local skill injection
- MCP support for stdio and HTTP transports, tool discovery/execution, prompt bridges, and resource bridges
- Gateway support for Telegram and an OpenAI-compatible API server
- Managed-agents beta control plane:
  - agent CRUD plus immutable versions
  - invocation through `model: "agent:<name>"`
  - per-agent tool and skill allowlists
  - `/v1/runs` list/get/cancel with best-effort task abort
  - persisted run events via `/v1/runs/{id}/events`
  - startup reconciliation for runs left `pending` / `running` during process exit
  - CLI `hermes agents ...` commands plus YAML `diff` / `sync`
  - CLI `hermes runs ...` inspection and Signet verification commands
  - optional Signet request/response receipts for managed tool calls
  - example scripts for managed API smoke tests, Signet verification, and a repository GitHub Actions workflow

## Current Priorities

- Final managed-agents beta doc sweep and release positioning

## Still Missing From The Managed Beta Roadmap

- Durable run replay or restart recovery that resumes in-flight runs
- MCP-backed tools in managed mode
- Vault / KMS or a hosted audit pipeline
- Multi-tenant namespaces and RBAC
- Web UI
- Stronger cancellation cleanup for external-process tools

## How To Read The Docs

- [README.md](../README.md): public project entrypoint and quick start
- [docs/specs/2026-04-22-managed-agents-v1-beta-plan.md](./specs/2026-04-22-managed-agents-v1-beta-plan.md): current managed-agents beta contract
- [AGENTS.md](../AGENTS.md): repository working rules and architecture guardrails
- `docs/specs/`: design history and phase-by-phase implementation notes

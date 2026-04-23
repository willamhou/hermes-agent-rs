# Hermes Managed Agents — Archived Draft

**Date**: 2026-04-22
**Status**: Archived / superseded
**Superseded by**: [2026-04-22-managed-agents-v1-beta-plan.md](./2026-04-22-managed-agents-v1-beta-plan.md)

This document is no longer the source of truth for managed agents.

It described a larger v1 than the current codebase can honestly support and relied on assumptions that do not match the Rust runtime, especially around:
- hosted-platform parity positioning
- session-shaped managed APIs instead of run-shaped APIs
- universal cancellation semantics
- connection-drop cancellation through request-extractor lifecycle
- vault / KMS / audit work in the first beta cut

## Current Source Of Truth

Use the beta plan for product scope, API shape, and delivery sequencing:

- [2026-04-22-managed-agents-v1-beta-plan.md](./2026-04-22-managed-agents-v1-beta-plan.md)

## Current Beta Contract

The managed-agents beta in this repository currently targets:

- agent CRUD plus immutable versions
- invocation through `model: "agent:<name>"`
- per-agent tool and skill allowlists
- managed runs tracked separately from transcript sessions
- `/v1/runs` list/get/cancel with best-effort task abort
- CLI `hermes agents ...` commands plus YAML `diff` / `sync`

## Explicit Non-Goals For The Current Beta

- hard real-time cancellation guarantees
- MCP in managed mode
- vault / KMS / audit log
- persistent run replay
- multi-tenant namespaces or RBAC
- web UI

## Implementation Pointers

- managed types, store, run registry, and runtime filtering live in [`crates/hermes-managed`](../../crates/hermes-managed)
- gateway managed API resolution lives in [`crates/hermes-gateway/src/api_server.rs`](../../crates/hermes-gateway/src/api_server.rs)
- CLI control-plane commands and YAML sync live in [`crates/hermes-cli/src/agents.rs`](../../crates/hermes-cli/src/agents.rs)

Keep this file only as a breadcrumb for older links. For any product or implementation decision, use the beta plan instead.

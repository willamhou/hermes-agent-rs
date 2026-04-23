# Managed Run Cancellation Cleanup

**Date**: 2026-04-23  
**Status**: In progress  
**Purpose**: Add a reusable cleanup boundary for session-scoped resources so managed run termination can reclaim external resources instead of only aborting the top-level task.

---

## Problem

Managed runs already support best-effort task abort, but task abort alone is not enough once tools create resources that outlive the immediate future:

- foreground child processes
- background child processes
- browser sessions / handler tasks
- future MCP client sessions or subscriptions

The current managed beta tool allowlist avoids the riskiest toolsets, but that only pushes the cleanup problem out in time. If we later widen the allowlist without a cleanup boundary, termination will remain incomplete.

---

## Decision

Introduce **session-scoped cleanup** as the common boundary.

Key points:

- resources are owned by `session_id`
- tools register long-lived external resources under the current session
- managed run termination always attempts cleanup for the run's session id
- cleanup is best-effort and additive; it does not block the existing run status model

This keeps the cleanup model aligned with the existing agent runtime shape because managed runs already use `run.id` as the agent `session_id`.

---

## First Slice

The first slice focuses on **process-like resources** because they are the simplest to verify and already have concrete kill semantics:

- foreground `terminal` child processes
- foreground `execute_code` child processes
- `terminal` background processes tracked through `process_registry`

Implementation:

- add a global `session_cleanup` registry in `hermes-tools`
- allow tools to register:
  - raw child PIDs
  - background process ids
- call `cleanup_session(run_id)` from managed terminal transitions

This gives managed runs a real cleanup hook even though the current beta allowlist still excludes these toolsets.

---

## Out Of Scope For This Slice

- browser session cleanup
- MCP transport/session cleanup
- resource-specific terminal process-group handling
- restart-safe restoration of cleanup registrations after process death
- changing managed beta tool policy

These remain follow-up work.

---

## Follow-Up Order

1. Extend session cleanup to browser session ownership and explicit close-on-termination.
2. Add process-group aware termination for shell-like tools where a single PID is not enough.
3. Define MCP cleanup semantics for server processes, HTTP sessions, subscriptions, and refresh tasks.
4. Persist enough cleanup metadata to make restart-safe reclamation possible, if we later need that.

---

## Why This Shape

This approach deliberately avoids two bad options:

- tool-specific cancellation hacks spread across the gateway
- pretending task abort is equivalent to resource cleanup

Instead, cleanup stays:

- explicit
- session-scoped
- reusable across tools
- compatible with the existing managed run ownership model

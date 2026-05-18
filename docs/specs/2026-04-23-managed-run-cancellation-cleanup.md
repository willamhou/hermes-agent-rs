# Managed Run Cancellation Cleanup

**Date**: 2026-04-23  
**Status**: Partially implemented; process-like cleanup and browser session cleanup run on managed termination, and process-like resources, browser session state (`process_group/root_pid + user_data_dir`), MCP HTTP sessions/resource subscriptions, plus shared MCP stdio/HTTP runtime manifests now have durable restart-time boundaries
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

## Implemented Slices

The first implemented slice focused on **process-like resources** because they are the simplest to verify and already have concrete kill semantics:

- foreground `terminal` child processes
- foreground `execute_code` child processes
- `terminal` background processes tracked through `process_registry`

Implemented:

- add a global `session_cleanup` registry in `hermes-tools`
- allow tools to register:
  - raw child PIDs
  - background process ids
  - async cleanup hooks
- call `cleanup_session(run_id)` from managed terminal transitions

The next implemented slice extends the same cleanup boundary to browser ownership:

- `browser` sessions register a session-scoped async cleanup hook on first launch
- managed run termination closes browser sessions and aborts their handler task
- explicit browser `close` unregisters the cleanup hook so the session is not double-cleaned

This gives managed runs a real cleanup hook even though the current beta allowlist still excludes these toolsets.

The latest slices add restart-safe durability for the process-like subset, the browser session boundary, and the first MCP-owned HTTP resource classes:

- `session_cleanup` can persist durable process-like cleanup resources through a recorder hook
- managed runs store those resources in SQLite keyed by `run_id` and cleanup entry id
- gateway startup and periodic recovery sweeps reclaim persisted process-like resources for terminal managed runs
- `browser` sessions now launch with per-session `user_data_dir` paths instead of a shared chromiumoxide temp directory
- `browser` cleanup now attaches a durable `browser_session` manifest (`process_group/root_pid + user_data_dir`) to the existing async browser-session cleanup entry
- browser launch now goes through a lightweight `setsid` wrapper on Unix so restart-time reclaim can kill the whole browser process group instead of only the top-level browser PID
- normal browser close removes the user-data dir after the browser process exits, and restart-time reclaim deletes that dir even when the original process is already gone
- HTTP MCP usage now registers a session-scoped cleanup hook, and HTTP sessions persist a durable `server + session_id + protocol_version` manifest for restart-time `DELETE`
- `mcp_resource_subscribe` now registers session-scoped cleanup, and HTTP subscriptions persist a durable `server + session_id + protocol_version + uri` manifest for restart-time unsubscribe
- successful reclaim removes the durable manifest row; failed reclaim leaves it in place for later retry
- cleanup failures are surfaced in persisted managed run events as `run.cleanup_failed` during terminal cleanup and recovery reclaim
- shared MCP stdio runtimes now own their stdout/stderr reader tasks explicitly and shut down via process-group-aware teardown instead of killing only the parent server PID
- shared MCP HTTP runtimes now explicitly own their notification-stream task instead of relying on a detached background spawn
- shared MCP stdio runtimes now persist worker leases plus per-runtime process-group manifests, and startup/periodic recovery sweeps reclaim stale server process groups after owner lease expiry
- shared MCP HTTP runtimes now persist worker leases plus per-runtime session manifests, and startup/periodic recovery sweeps reclaim stale shared HTTP sessions after owner lease expiry

---

## Still Out Of Scope

- MCP transport/session cleanup beyond HTTP sessions, resource-subscription cleanup, and shared HTTP runtime-session cleanup
- restart-safe browser session recovery beyond the current `process_group/root_pid + user_data_dir` boundary
- restart-safe MCP cleanup restoration beyond HTTP sessions, resource-subscription cleanup, and shared stdio/HTTP runtime manifests
- changing managed beta tool policy

These remain follow-up work.

---

## Follow-Up Order

1. Persist richer browser session metadata beyond the current `process_group/root_pid + user_data_dir` cleanup boundary, plus deeper MCP cleanup metadata, in a durable, restart-safe form.
2. Extend MCP cleanup semantics beyond current managed HTTP manifests plus shared stdio/HTTP runtime manifests into richer restart-safe MCP session/runtime ownership.
3. Decide whether repeated recovery-sweep cleanup failures should be deduplicated or rate-limited in `run.cleanup_failed` telemetry.

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

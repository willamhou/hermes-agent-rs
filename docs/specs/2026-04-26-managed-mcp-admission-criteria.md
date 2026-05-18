# Managed MCP Admission Criteria

**Date**: 2026-04-26  
**Status**: Partially implemented admission contract  
**Purpose**: Define the runtime, cleanup, and operator guarantees for managed MCP admission, and record the currently admitted slice versus what remains blocked.

---

## 1. Why This Spec Exists

Managed mode no longer rejects every MCP-backed tool uniformly.

That is no longer because MCP is entirely unmanaged. The repository now already has:

- session-scoped cleanup for managed HTTP MCP sessions and resource subscriptions
- durable restart-time reclaim for those managed HTTP MCP resources
- explicit shared MCP runtime ownership for both `stdio` and HTTP transports
- persisted worker leases plus shared `stdio` process-group manifests and shared HTTP session manifests
- persisted shared MCP runtime audit events for startup / periodic reclaim outcomes
- explicit operator policy fields for transport/server/side-effect gates
- explicit `managed.mcp.stdio` candidate policy fields for allowlisted stdio servers plus redacted env-key audit metadata
- an operator-facing `hermes mcp inspect` surface that aggregates current policy, recent admission rejections with structured read-only capability attribution, and shared-runtime reclaim audits
- stronger managed MCP preflight rejections for stdio-only bridge tools, model-callable MCP tools that currently resolve only to stdio servers, read-only bridge requests when operator policy leaves only stdio candidates, and allowlisted HTTP read-only requests whose servers either lack prompt/resource capability or are shadowed by non-allowlisted HTTP servers that do expose it
- a managed-local HTTP read-only bridge slice for:
  - `mcp_prompt_list`
  - `mcp_prompt_get`
  - `mcp_resource_list`
  - `mcp_resource_template_list`
  - `mcp_resource_read`
- persisted `run.mcp_admission_rejected` events with structured rejection metadata emitted directly from managed MCP build / preflight failures

What is still missing is the **broader admission contract** for capabilities beyond that initial slice.

Without one, “enable MCP in managed mode” would collapse multiple unresolved questions into a single allowlist toggle:

- which MCP transports are safe enough
- which MCP capabilities are safe enough
- what isolation boundary a managed run actually gets
- how secrets, cleanup failures, and runtime ownership are surfaced to operators

This spec separates “cleanup building blocks now exist” from “managed MCP is ready to ship.”

---

## 2. Current Ground Truth

Today:

- managed mode admits a narrow HTTP read-only bridge slice when:
  - `managed.mcp.enabled = true`
  - `managed.mcp.allowed_transports` includes `http`
  - `managed.mcp.allowed_servers` references enabled HTTP MCP servers
- managed runs can durably clean up:
  - managed HTTP MCP sessions
  - managed HTTP MCP resource subscriptions
- shared MCP runtimes can durably reclaim:
  - stale `stdio` server process groups
  - stale HTTP sessions
- operators can inspect persisted shared-runtime reclaim history via `hermes mcp audits`
- managed MCP admission rejection metadata now includes a redacted stdio policy snapshot when such candidate policy is configured
- operators can inspect the current policy and recent rejection/reclaim signals together via `hermes mcp inspect`, including whether read-only capability failures are true gaps or are shadowed by non-allowlisted HTTP servers
- managed mode now distinguishes generic MCP rejection from stdio-specific preflight rejection for the current stdio-only capability slice, including the current read-only path when operator policy leaves only stdio candidates
- managed mode still rejects:
  - dynamic model-callable MCP tools discovered from `tools/list`
  - side-effecting MCP bridge tools (`mcp_resource_subscribe`, `mcp_resource_unsubscribe`, `mcp_resource_updates`)
  - all `stdio` MCP admission

That means Hermes now has a **real Gate A foothold**, but not yet the broader contract needed for side-effecting or `stdio` MCP admission.

---

## 3. Admission Principles

An MCP capability may be admitted into managed mode only if all of the following are true:

1. **Ownership is explicit**
   Each long-lived MCP-side effect must have a clear owner:
   - per-run
   - per-session
   - shared runtime

2. **Cleanup semantics are explicit**
   Cancellation and terminal cleanup must define what happens to:
   - server processes
   - HTTP sessions
   - subscriptions
   - refresh / notification tasks
   - any transport-specific background state

3. **Restart-time behavior is truthful**
   If a resource cannot be durably reclaimed after process death, that limitation must be explicit and must block admission for capabilities that depend on it.

4. **Operator policy is enforceable**
   Operators must be able to say:
   - which MCP servers are eligible for managed use
   - which agents may use them
   - which transports are allowed
   - whether read-only vs side-effecting usage is permitted

5. **Failure visibility exists**
   Cleanup and reclaim failures must not disappear into debug logs only.

6. **Tests prove the lifecycle**
   Admission requires targeted tests for:
   - normal completion
   - cancel / disconnect
   - restart-time reclaim
   - cleanup failure reporting

---

## 4. Transport-Specific Bar

### 4.1 HTTP MCP

HTTP MCP is the first realistic managed candidate because it already has:

- managed run-scoped session cleanup
- managed run-scoped subscription cleanup
- durable session/subscription manifests
- shared runtime session ownership and reclaim

HTTP MCP can be considered for managed admission only when:

- the admitted server set is explicitly allowlisted
- server auth / headers are sourced from an operator-controlled config path
- each admitted capability has tested cleanup semantics
- operator-visible failure telemetry exists for the managed run path

### 4.2 `stdio` MCP

`stdio` MCP is a higher bar even with shared runtime worker leases and process-group reclaim.

It is still blocked on at least these concerns:

- command / argv / env are deployment-sensitive and need explicit policy
- process spawning is a stronger side effect than HTTP session creation
- shared runtime ownership is not yet a managed per-run isolation boundary
- operator-visible process audit exists, but the managed `stdio` policy / secrets boundary is still not strong enough
- explicit stdio candidate policy now exists, but it is still descriptive / auditable rather than an admission grant

As a result, `stdio` MCP should remain out of managed mode until it has:

- explicit command/env admission policy
- clear secrets handling
- operator-visible runtime/process auditability
- tested cancel and restart semantics that are strong enough for managed use

---

## 5. Capability Gates

Managed MCP should not be admitted all at once. It should land in gates.

### Gate A: Read-oriented HTTP MCP

Eligible shape:

- HTTP transport only
- prompt / resource read / tool calls that do not create subscriptions
- no browser-like spawned local processes

Required guarantees:

- managed session cleanup
- restart-time HTTP session reclaim
- per-server allowlist
- cleanup failure visibility

### Gate B: Subscription-capable HTTP MCP

Eligible shape:

- HTTP transport with `resources/subscribe`

Additional guarantees:

- durable subscription manifests
- tested unsubscribe semantics on cancel and restart reclaim
- clear operator guidance for servers that do not fully support cleanup

### Gate C: `stdio` MCP

Eligible shape:

- explicitly allowlisted `stdio` servers only

Additional guarantees:

- process-spawn policy
- env / secret policy
- operator-visible audit semantics
- restart-time stale runtime reclaim already in place
- managed admission tests that prove cancel and cleanup behavior under failure

---

## 6. What Still Blocks Admission Today

As of this spec, broader managed MCP is still blocked by:

- no operator-facing secrets / auth policy for managed MCP servers
- no richer managed browser cleanup yet, which is relevant for any MCP server that indirectly creates browser-like external state
- no finalized managed `stdio` runtime enforcement over the new process/env/secrets policy surface

So the right next step is **not** “treat all MCP as managed-safe now that Gate A exists.”

The right next step is:

1. keep hardening cleanup and ownership boundaries
2. extend operator-facing telemetry / audit semantics from the current baseline of admission-rejection, cleanup-failure, and shared-runtime reclaim events into a stronger managed `stdio` policy surface
3. expand admission only capability-by-capability

---

## 7. Recommended Next PRs

1. Broaden operator-visible managed MCP lifecycle telemetry beyond the current baseline:
   - cleanup failure events
   - admission rejection reasons
   - shared-runtime reclaim history / operator surfaces
   - stronger `stdio` runtime/process audit and secrets-policy signals

2. Broaden runtime preflight enforcement beyond the current stdio-specific rejection semantics before considering Gate C admission.

3. Expand the HTTP-only managed MCP slice beyond fixed read-only bridge tools only when lifecycle tests cover the new capability.

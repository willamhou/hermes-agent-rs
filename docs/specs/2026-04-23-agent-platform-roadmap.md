# Hermes Agent Platform Roadmap

**Date**: 2026-04-23  
**Status**: Working roadmap  
**Purpose**: Describe how Hermes moves from a strong local agent runtime plus a managed-agents beta into an open-source "Agent-era AWS" style platform.

---

## 1. What This Roadmap Means

This roadmap does **not** mean "copy every hosted vendor surface."

It means Hermes should become a credible open-source platform with:

- a reliable **agent execution plane**
- a reusable **managed control plane**
- durable **platform services** for identity, secrets, audit, policy, and operations

In short:

> local agent runtime + managed runs + operator services = open-source Agent platform

Today the repository is closer to:

- a feature-rich local agent runtime in `hermes-agent`
- a beta managed control plane in `hermes-managed` and `hermes-gateway`

It is **not yet** a full platform for large-scale multi-agent operations.

---

## 2. Two Planning Lenses

### 2.1 `hermes-agent` Function Parity

Question:

> Which agent capabilities work locally today, and how do we make them safely available in managed mode?

This lens is about closing the gap between:

- what a local Hermes agent can do
- what a managed Hermes agent can safely do

The main parity gaps are:

- `browser`
- `terminal` / `execute_code`
- MCP-backed tools
- `delegate_task`
- `cron`
- richer artifact handling
- workflow-like multi-step orchestration

### 2.2 `managed-agents` Platform Expansion

Question:

> Which new platform layers are required so managed agents become a dependable shared service rather than just an API wrapper around a run?

This lens is about building:

- execution reliability
- tenancy and policy
- secrets and audit
- scheduling and triggers
- observability and operations

---

## 3. Platform Thesis

Hermes becomes "Agent-era AWS" only when it offers all three of these layers together.

### 3.1 Agent Runtime Layer

Primary crates:

- `crates/hermes-agent`
- `crates/hermes-tools`
- `crates/hermes-skills`
- `crates/hermes-mcp`
- `crates/hermes-memory`

Responsibilities:

- tool calling
- skills and memory
- browser / terminal / MCP execution
- delegation and orchestration
- context and artifacts

### 3.2 Managed Execution Layer

Primary crates:

- `crates/hermes-managed`
- `crates/hermes-gateway`
- `crates/hermes-cli`

Responsibilities:

- agent and version lifecycle
- run lifecycle
- filtering and policy enforcement
- replay / verification / auditability
- cancellation / cleanup / recovery
- worker ownership of long-running execution

### 3.3 Platform Services Layer

Primary future scope across current crates or new crates:

- namespaces / organizations / projects
- RBAC / IAM-style policy
- secrets / vault / KMS integration
- quotas / budgets / cost attribution
- scheduler / queue / triggers / webhooks
- observability / traces / run search / UI

Without this third layer, Hermes remains a strong runtime and beta control plane, but not a full platform.

---

## 4. Current Starting Point

What already exists:

- local runtime with broad built-in tool coverage
- multi-provider support
- gateway and OpenAI-compatible API
- managed agents with immutable versions
- managed runs with events, replay, verification, best-effort cancellation, and ownership/lease-based interruption recovery
- session-scoped cleanup for process-like resources and browser sessions
- durable cleanup manifests plus restart-time reclaim for process-like resources, browser session state (`process_group/root_pid + user_data_dir`), managed MCP HTTP sessions/resource subscriptions, and shared MCP stdio/HTTP runtime boundaries

What still blocks the "platform" story:

- no richer browser restart recovery beyond the current `process_group/root_pid + user_data_dir` cleanup boundary
- no deeper MCP runtime/session restart recovery beyond current managed HTTP and shared stdio/HTTP boundaries
- only a narrow allowlisted HTTP read-only managed MCP bridge slice exists; dynamic tools, side-effecting bridge tools, and `stdio` MCP remain out
- no tenancy or RBAC
- no secrets / KMS / hosted audit pipeline
- no distributed queue / worker / trigger model
- no operator UI or broad observability surface

---

## 5. Core Workstreams

This roadmap should be executed as three parallel workstreams.

### 5.1 Runtime Safety

Goal:

Make advanced agent capabilities safe enough to expose in managed mode.

Main themes:

- richer browser cleanup manifests
- deeper MCP runtime/session cleanup semantics
- managed-safe browser / broader MCP / delegation / cron boundaries
- artifact model for files, snapshots, receipts, and structured outputs
- workflow-safe tool lifecycle ownership

### 5.2 Managed Platform

Goal:

Turn managed runs from a beta API surface into a reliable execution plane.

Main themes:

- restart-safe recovery
- resumable run ownership
- worker queue and lease model
- retries, concurrency caps, budgets, and deadlines
- event triggers, scheduled triggers, and webhooks

### 5.3 Operator Surface

Goal:

Make the system governable by teams, not just usable by one developer.

Main themes:

- namespaces / orgs / projects
- RBAC and policy
- secrets and KMS
- audit, search, metrics, traces
- web UI and infrastructure-facing APIs

---

## 6. Sequenced Roadmap

### P0. Safe Managed Runtime Foundations

Purpose:

Remove the biggest trust gaps in managed execution.

Current state:

- process-group-aware cleanup for shell-like tools is implemented
- managed termination has explicit cleanup semantics for currently admitted long-lived resource classes
- shared MCP stdio/HTTP runtime ownership is explicit, with restart-time reclaim for current persisted boundaries

Priority work:

- richer browser durable cleanup beyond the current `process_group/root_pid + user_data_dir` boundary
- deeper MCP cleanup and lifecycle ownership beyond current managed HTTP plus shared stdio/HTTP runtime manifests
- broaden managed MCP beyond the current HTTP read-only bridge slice without weakening ownership/cleanup guarantees
- artifact model design for managed runs

Exit criteria:

- managed termination has explicit cleanup semantics for every currently supported long-lived resource class
- there is a documented lifecycle contract for future managed-safe tool admission

### P1. Restart-Safe Recovery

Purpose:

Make managed runs survive process death in a principled way.

Priority work:

- extend the implemented ownership/lease model with optional replay policies
- persist enough execution ownership and cleanup metadata for remaining resource classes
- define provider/tool boundaries for any future resumability beyond interrupted-then-replay

Exit criteria:

- process restart no longer reduces active runs to generic terminal guesses
- recovery behavior is explicit, tested, and documented

### P2. Managed Capability Parity

Purpose:

Bring the most important local agent capabilities into managed-safe execution.

Priority work:

- managed `browser`
- managed MCP-backed tools
- managed delegation model
- managed cron / scheduled execution alignment

Exit criteria:

- managed mode supports a materially larger subset of the local runtime
- each newly admitted capability has cleanup, audit, and failure semantics

### P3. Execution Plane

Purpose:

Move from "run API" to "agent compute plane."

Priority work:

- worker queue
- lease / claim model
- retries and retry policy
- concurrency controls
- deadlines and budget enforcement
- webhooks and event triggers

Exit criteria:

- managed execution can be distributed and scheduled
- run ownership is explicit even beyond a single gateway process

### P4. Identity, Tenancy, and Policy

Purpose:

Make Hermes usable by teams and organizations.

Priority work:

- namespaces / projects / organizations
- RBAC / IAM-style permissions
- API token scopes
- quotas and spend / usage limits
- policy hooks for toolsets, models, and environments

Exit criteria:

- users and teams can share one deployment safely
- policy is no longer encoded only in agent version config

### P5. Secrets, Audit, and Observability

Purpose:

Add the operator-grade services that make the platform trustworthy.

Priority work:

- vault / secret refs / KMS
- remote audit pipeline
- structured run traces and metrics
- run search and timeline inspection
- cost attribution and budget reporting

Exit criteria:

- operators can answer who ran what, with which credentials, at what cost, and with what outcome

### P6. Product Surface

Purpose:

Make the platform discoverable, operable, and extensible.

Priority work:

- web UI
- agent registry
- managed skill / MCP catalog
- SDK / Terraform / Helm style operator surface
- deployment guides for self-hosted environments

Exit criteria:

- Hermes is usable as a platform by operators, not only by repository contributors

---

## 7. Near-Term Priorities

The next concrete priorities should be:

1. extend durable cleanup beyond the current browser-session (`process_group/root_pid + user_data_dir`) boundary and current MCP runtime/session manifests
2. implement managed MCP admission policy on top of the documented cleanup and ownership model
3. design the execution-plane model for queue / leases / retries
4. design the artifact model for managed runs

These four items unlock nearly every later platform feature.

---

## 8. Architectural Guardrails

As the roadmap expands, keep these constraints:

- `hermes-core` stays provider-neutral
- managed concerns stay out of generic local runtime APIs unless they are truly reusable
- policy must be explicit and inspectable
- long-running work must have owned lifecycle and cleanup semantics
- persisted state changes should remain additive and migration-conscious

---

## 9. How To Use This Roadmap

Use this file for:

- deciding what comes after the managed beta
- checking whether a proposal closes local/runtime parity or platform capability
- sequencing PRs so runtime hardening lands before platform promises

Use the beta plan for:

- the current managed beta contract
- current non-goals
- already-shipped managed API surface

Use follow-on specs for:

- recovery and cleanup subproblems
- managed MCP admission policy details
- narrower platform workstreams that need their own contract

Source of truth for the current beta:

- [2026-04-22-managed-agents-v1-beta-plan.md](./2026-04-22-managed-agents-v1-beta-plan.md)

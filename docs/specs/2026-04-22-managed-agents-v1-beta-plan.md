# Hermes Managed Agents v1 Beta — Reality-Based Implementation Plan

**Date**: 2026-04-22
**Status**: In progress, with the core beta surface implemented
**Purpose**: Turn the current managed-agents draft into a deliverable v1 beta plan that fits the actual codebase and avoids fake guarantees.

---

## Progress Snapshot

Implemented already:
- `crates/hermes-managed` with managed agents, versions, runs, store, run registry, tool filtering, and skill filtering
- gateway resolution for `model: "agent:<name>"`
- `/v1/agents`, `/v1/agents/:id/versions`, `/v1/runs`, and managed entries in `/v1/models`
- CLI `hermes agents ...` CRUD plus YAML `diff` / `sync`
- canonical YAML hashing plus `# hermes-synced: sha256=...` metadata
- persisted managed run events via `/v1/runs/:id/events` and `hermes runs events`
- startup reconciliation that marks runs left active during process exit as terminal
- managed run inspection via `hermes runs list|get|events`
- CLI Signet verification via `hermes runs verify [--json] [--strict] [--quiet]`
- optional Signet request/response receipts for managed tool calls, stored in the local audit chain
- managed disconnect / cancel behavior tests
- managed provider matrix tests for OpenAI chat-compatible, OpenRouter-compatible, Anthropic, and Responses paths
- end-to-end examples for managed API usage, Signet verification, and a repository GitHub Actions workflow

Still open before the beta story is fully closed:
- final public docs / release sweep so the beta story is consistent everywhere

---

## 0. Product Contract

v1 beta is **not** "Anthropic Managed Agents parity."

v1 beta **is**:
- Agent CRUD
- Immutable `AgentVersion`
- OpenAI-compatible invocation via `model: "agent:<name>"`
- Per-agent built-in tool allowlist
- Best-effort cancellable runs
- API disconnect aborts the run task
- Per-agent timeout / max iterations / approval policy
- YAML `diff` / `sync` with dry-run and metadata hash
- Persisted run status and event records
- Optional local Signet receipts and CLI verification for managed tool calls
- Basic examples for API usage, CLI verification, and CI wiring

v1 beta is **not**:
- Persistent run replay
- Restart-safe recovery that resumes runs which were still active during process exit
- Full session management parity with hosted platforms
- Multi-tenant namespaces or RBAC
- MCP in managed mode
- Vault / KMS or a remote managed audit pipeline in the first cut
- Guaranteed sub-500ms cancel for every possible tool

The public positioning for v1 beta should be:

> Self-hosted multi-provider agent control plane with managed runs for a restricted toolset.

---

## 1. Hard Decisions

### 1.1 Use `run`, not `session`, in the managed API

The repository already has durable transcript sessions in [`crates/hermes-core/src/session.rs`](../../crates/hermes-core/src/session.rs) and [`crates/hermes-config/src/sqlite_store.rs`](../../crates/hermes-config/src/sqlite_store.rs). Managed execution should not overload that term.

Decision:
- Managed API surface uses `run`
- Existing CLI transcript storage keeps `session`
- Avoid broad internal renames in beta; keep the naming split at the new managed boundary

### 1.2 Cancel is "best-effort," not "hard real-time"

Current code cannot truthfully promise that every run stops within 500 ms:
- The agent loop waits directly on provider futures in [`crates/hermes-agent/src/loop_runner.rs`](../../crates/hermes-agent/src/loop_runner.rs)
- Gateway session tasks currently have no abort handle in [`crates/hermes-gateway/src/session.rs`](../../crates/hermes-gateway/src/session.rs)
- Some tools spawn external resources, including `terminal`, `execute_code`, and `browser`

Decision:
- v1 beta promises run-task abort, not universal kill semantics
- Managed mode excludes tools that leak external resources on abort
- Documentation must say "best-effort cancellation"

### 1.3 No MCP in managed mode for v1 beta

Current MCP integration dynamically mutates the whole `mcp` toolset and owns background refresh tasks in [`crates/hermes-mcp/src/lib.rs`](../../crates/hermes-mcp/src/lib.rs).

Decision:
- Managed mode rejects MCP-backed tools in v1 beta
- Vault / secret resolution / audit follow after the control-plane core is stable

### 1.4 Do not start with provider or tool trait churn

The current providers execute request and stream handling inside a single awaited future:
- [`crates/hermes-provider/src/openai.rs`](../../crates/hermes-provider/src/openai.rs)
- [`crates/hermes-provider/src/anthropic.rs`](../../crates/hermes-provider/src/anthropic.rs)
- [`crates/hermes-provider/src/responses.rs`](../../crates/hermes-provider/src/responses.rs)

Decision:
- First implement cancellation through execution ownership and task abort
- Only add deeper trait-level cancellation if beta testing shows the task-abort model is insufficient

### 1.5 Keep beta storage additive and simple

The repository currently uses additive SQLite schema setup rather than a full migration framework.

Decision:
- Reuse the existing `state.db` pattern for beta
- Add managed tables additively
- Defer refinery or larger migration tooling until after beta proves the model

---

## 2. Managed Mode Tool Policy

Managed mode needs a strict beta tool policy from day one.

### 2.1 Allowed built-in tools for v1 beta

Initial allowlist target:
- `read_file`
- `search_files`
- `write_file`
- `patch`
- `memory_read`
- `memory_write`
- `web_search`
- `web_extract`
- `vision_analyze`
- `skill_list`
- `skill_view`

### 2.2 Explicitly disallowed in managed mode

These are out for v1 beta:
- `terminal`
- `execute_code`
- `browser`
- `clarify`
- `skill_manage`
- `delegate_task`
- `cron`
- all MCP tools

Reason:
- They either require interactive UX, spawn external resources, or introduce background lifecycle complexity that weakens cancellation guarantees

### 2.3 Enforcement points

Allowlist enforcement must happen in all of these places:
- Tool schema exposure to the model
- Tool execution lookup
- Skill matching / injection
- Delegation entry points
- Managed API validation

Beta does **not** ship until those paths are aligned.

---

## 3. Target Architecture

### 3.1 New crate

Add a new crate:

`crates/hermes-managed`

Responsibilities:
- Managed domain types
- Managed SQLite store
- Filtered runtime builders
- Run registry
- Managed API request resolution helpers

It should not own:
- CLI presentation logic
- Generic provider implementations
- Generic tool implementations

### 3.2 New domain objects

Core beta objects:
- `ManagedAgent`
- `ManagedAgentVersion`
- `ManagedRun`
- `ManagedRunStatus`
- `ManagedRuntimePlan`

`ManagedRuntimePlan` is the key boundary:
- model
- base_url
- system_prompt
- allowed_tools
- allowed_skills
- max_iterations
- approval_policy
- timeout_secs
- working_dir policy

The goal is to convert:

`ManagedAgentVersion -> ManagedRuntimePlan -> AgentConfig`

without polluting `hermes-core` with managed-only concerns.

### 3.3 New execution ownership model

Managed runs must have an owner that can stop them.

Add:
- `RunRegistry`
- `RunHandle`
- `RunStatusSnapshot`

`RunHandle` should own:
- run id
- agent/version identity
- started timestamp
- timeout
- `JoinHandle`
- abort handle or equivalent

For beta, the primary cancel path is:
- abort the spawned run task
- mark run status as `cancelled`
- stop streaming

### 3.4 Separate managed invocation path

Do not bolt managed behavior onto the existing shared gateway session router.

Instead:
- keep the current generic path for plain model calls
- add a separate managed path when `model` matches `agent:<name>`

That path should:
- resolve agent version
- build filtered provider / registry / skills
- register the run
- spawn the run task
- stream output
- update final status

---

## 4. PR Sequence

The plan below is intentionally narrow and reviewable. Each PR should land independently.

### PR1: Scaffold `hermes-managed` and additive storage

Goal:
- Create the new crate and the managed data model

Likely file touch set:
- `Cargo.toml`
- `crates/hermes-managed/Cargo.toml`
- `crates/hermes-managed/src/lib.rs`
- `crates/hermes-managed/src/types.rs`
- `crates/hermes-managed/src/store.rs`

Tasks:
- Define `ManagedAgent`, `ManagedAgentVersion`, `ManagedRun`, `ManagedRunStatus`
- Add a managed SQLite store API
- Reuse additive schema creation in `state.db`
- Add tables for agents, agent_versions, and runs

Acceptance:
- Can create/list/get agents and versions from tests
- Can create/update runs and statuses from tests
- No changes yet to live gateway behavior

Verification:
- unit tests in `hermes-managed`
- `cargo check --workspace`

### PR2: Filtered tool and skill wrappers

Goal:
- Make per-agent allowlists real

Likely file touch set:
- `crates/hermes-managed/src/filtered_registry.rs`
- `crates/hermes-managed/src/filtered_skills.rs`
- `crates/hermes-tools/src/registry.rs`
- `crates/hermes-skills/src/manager.rs`
- small tests near the wrappers

Tasks:
- Add a filtered tool registry wrapper that controls both schema exposure and execution lookup
- Add a filtered skill access wrapper that only exposes configured skills
- Add a managed beta tool policy helper that rejects disallowed tools up front

Acceptance:
- A blocked tool does not appear in exposed schemas
- A blocked tool cannot execute even if the model names it directly
- A blocked skill is never injected for managed runs

Verification:
- unit tests for schema filtering
- unit tests for execution denial
- unit tests for skill filtering

### PR3: Run registry and cancellable managed executor

Goal:
- Introduce owned managed runs with best-effort abort

Likely file touch set:
- `crates/hermes-managed/src/run_registry.rs`
- `crates/hermes-managed/src/executor.rs`
- `crates/hermes-agent/src/loop_runner.rs`
- `crates/hermes-gateway/src/session.rs`

Tasks:
- Create `RunRegistry`
- Spawn managed execution in a task owned by the registry
- Add `cancel_run(run_id)` that aborts the run task
- Keep generic gateway sessions unchanged for now

Acceptance:
- Cancelling a managed run flips status to cancelled
- The managed run task does not continue to stream after cancellation
- Disconnecting the client from the managed path aborts the managed run task

Verification:
- integration test with a fake provider that sleeps
- test that run status changes from `running` to `cancelled`

### PR4: Managed API resolution and `agent:*` model support

Goal:
- Wire managed invocation into the OpenAI-compatible endpoint

Likely file touch set:
- `crates/hermes-gateway/src/api_server.rs`
- `crates/hermes-gateway/src/runner.rs`
- `crates/hermes-managed/src/runtime_factory.rs`
- `crates/hermes-provider/src/lib.rs`

Tasks:
- Detect `model: "agent:<name>"`
- Resolve latest active agent version
- Build provider from agent version model/base_url
- Build filtered registry and filtered skills
- Create and track a managed run
- Keep non-managed requests on the old path

Acceptance:
- `model: "agent:<name>"` runs through the managed path
- a plain provider model like `openai/gpt-4o` still works unchanged
- managed and generic paths can coexist cleanly

Verification:
- API test for managed path
- API test for generic path regression

### PR5: Agent CRUD plus YAML sync CLI

Goal:
- Make the control plane operable without a UI

Likely file touch set:
- `crates/hermes-cli/src/main.rs`
- `crates/hermes-cli/src/commands.rs`
- `crates/hermes-cli/src/handlers.rs`
- `crates/hermes-managed/src/yaml.rs`
- `crates/hermes-managed/src/store.rs`

Tasks:
- Add create/list/get/version CLI commands
- Add YAML load / diff / dry-run sync
- Canonicalize YAML before hashing
- Reject invalid beta toolset entries during sync

Acceptance:
- Can create an agent from YAML
- `sync --dry-run` shows planned changes without mutating DB
- `diff` shows drift against the latest stored version

Verification:
- CLI-focused integration tests
- golden tests for YAML normalization and diff output

### PR6: Managed-mode hardening and final beta docs

Goal:
- Make the beta story honest and stable

Likely file touch set:
- `README.md`
- `docs/specs/2026-04-22-managed-agents.md`
- `docs/STATUS.md`
- tests across gateway / managed / provider boundaries

Tasks:
- Update docs to say "best-effort cancellation"
- Document the managed beta toolset explicitly
- Document current non-goals
- Add provider matrix tests with managed runs
- Add disconnect / cancel behavior tests

Acceptance:
- No doc still promises full managed-agents parity
- No doc still promises guaranteed 500 ms universal cancel
- Test suite covers the managed beta contract

Verification:
- `cargo test --workspace`
- targeted gateway / managed integration tests

Status:
- PR1 through PR6 are effectively implemented for the current beta contract
- Remaining work is mostly release hardening, restart semantics, and documentation polish rather than missing control-plane surface

---

## 5. Implementation Notes by Area

### 5.1 `hermes-agent`

Keep the agent loop mostly intact in beta.

Only touch it when needed to:
- support clean managed execution wrapping
- expose enough hooks for final run status and usage

Do **not** start beta by redesigning the whole loop around cancellation tokens.

### 5.2 `hermes-gateway`

This crate needs the most careful boundary work.

Current issue:
- it builds a single global provider and registry at startup in [`crates/hermes-gateway/src/runner.rs`](../../crates/hermes-gateway/src/runner.rs)

Beta target:
- generic invocation path stays global
- managed invocation path builds a runtime per agent version

### 5.3 `hermes-tools`

The main beta work is policy and filtering, not mass rewrites.

Only revisit concrete tool implementations when a managed-mode leak is proven.

For beta:
- exclude cancel-unsafe tools rather than pretending they are supported

### 5.4 `hermes-mcp`

Do not thread managed allowlists through the current MCP refresh model in beta.

Managed mode should reject MCP usage up front.

This is a scope-control decision, not a permanent product limitation.

### 5.5 `hermes-config`

Prefer additive config:
- managed API enable flag if needed
- default managed working directory policy

Do not bury managed runtime logic in config loading.

---

## 6. Cancel Semantics for v1 Beta

Public contract:
- `DELETE /v1/runs/:id` is best-effort
- API disconnect aborts the owned run task
- managed beta only supports tools whose lifecycle we can keep inside that run task boundary

Internal contract:
- run task abort must stop provider streaming for the managed beta toolset
- run task abort must not leave managed-mode background workers behind
- cancel-unsafe tools stay out of managed mode until proven safe

Open question for implementation:
- for streaming responses, whether to emit a final cancel marker before close or simply terminate the stream

Beta acceptance should test:
- non-streaming cancel during provider wait
- streaming disconnect during provider wait
- cancellation before tool execution starts

Beta should **not** claim:
- safe abort for spawned shells
- safe abort for spawned Python processes
- safe abort for browser sessions
- safe abort for MCP child processes

---

## 7. Deferred Work After Beta

These are real follow-on tracks, but they should not block beta:
- MCP in managed mode
- Vault / secret refs / KMS
- Audit log
- Run event replay
- Process-restart recovery for in-flight runs
- Multi-tenant namespaces
- RBAC
- Trait-level cancellation if task abort proves insufficient
- Resource-specific cleanup guards for external-process tools

---

## 8. Distance From A Fuller Managed-Agents Product

The repository is now close to a usable **managed-agents beta**, but still meaningfully short of a broader managed-agents product.

What is already true:
- control plane exists
- immutable versions exist
- invocation through the public API exists
- run records and event timelines exist
- best-effort cancel exists
- local auditability via Signet exists for managed tool calls

What still separates this from a more complete managed-agents offering:
- no durable replay / resume of an interrupted run
- no restart recovery for active runs after process death
- no multi-tenant isolation model
- no RBAC or org-level policy layer
- no managed MCP story
- no hosted audit / secrets / KMS story
- no web UI or operations dashboard

Practical estimate:
- to finish the current beta story cleanly: roughly another few focused engineering sessions
- to turn this into a stronger self-hosted managed-agents product: roughly several more weeks of focused systems work
- to approach hosted-platform parity: likely a multi-month project, mostly in runtime recovery, tenancy, policy, and operations layers
---

## 9. Exit Criteria

v1 beta is ready when all of the following are true:
- An agent can be created, versioned, and invoked through `model: "agent:<name>"`
- Managed runs have their own status records
- Managed runs can be cancelled through a dedicated API
- Client disconnect aborts the managed run task
- Managed mode only exposes the documented beta toolset
- Blocked tools cannot execute even if the model requests them directly
- Docs describe the product honestly as a beta control plane, not full hosted-platform parity

---

## 10. Recommended First PR

Start with PR1 plus the policy constants from PR2.

Reason:
- it creates the managed control-plane foundation
- it forces the object model to become concrete
- it settles names early
- it lets later PRs stay additive instead of speculative

If PR1 lands cleanly, the rest of the beta path becomes straightforward.

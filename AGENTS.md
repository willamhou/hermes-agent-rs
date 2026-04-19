# AGENTS.md

## What this repository is

hermes-agent-rs is a Rust workspace for a model-agnostic agent runtime and developer-facing agent tooling.

Primary goals:
- Keep the core runtime model-agnostic.
- Support multiple model providers without leaking provider-specific behavior into core abstractions.
- Treat long-running agent execution as durable, observable, and resumable.
- Prefer explicit runtime events and state transitions over implicit control flow.

Primary crates:
- `crates/hermes-core`: shared core types and contracts
- `crates/hermes-provider`: model/provider adapters
- `crates/hermes-tools`: tool abstractions, registries, and implementations
- `crates/hermes-memory`: memory/session persistence abstractions and implementations
- `crates/hermes-agent`: agent loop, orchestration, and runtime-facing logic
- `crates/hermes-config`: config loading, persistence, and runtime settings
- `crates/hermes-skills`: skill loading and matching/injection
- `crates/hermes-cli`: local CLI entrypoint

## Working principles

- Prefer small, reviewable changes over broad rewrites.
- Preserve public API stability unless the task explicitly requires a breaking change.
- Keep behavior changes localized and easy to verify.
- Do not mix unrelated refactors into feature or bug-fix patches.
- When touching architecture-sensitive code, explain the tradeoffs in the final summary.
- Favor readability and explicitness over clever abstractions.
- Avoid introducing new dependencies unless clearly justified.

## Architecture rules

### Model-agnostic core
- `hermes-core` must remain provider-neutral.
- Do not introduce Anthropic-, OpenAI-, or vendor-specific types into core contracts.
- Provider-specific request/response formats must stay in `hermes-provider`.
- Normalize external model behavior into shared internal types before crossing crate boundaries.

### Runtime separation
- Keep runtime/orchestration concerns separate from CLI presentation concerns.
- Do not let `hermes-cli` become the de facto runtime layer.
- Prefer reusable execution primitives that can later serve CLI, gateway, cron, or server-style entrypoints.

### Explicit state and events
- Prefer explicit runtime events, phases, and state transitions.
- Avoid hidden coupling through ad hoc flags or loosely documented control flow.
- Long-running behavior should be inspectable, resumable, and testable.

### Tool boundaries
- Tool implementations should not smuggle provider-specific assumptions into generic tool interfaces.
- Tool side effects should be explicit.
- Dangerous operations must respect approval and safety controls.

### Session and memory
- Treat session history as durable execution context, not just UI chat history.
- Prefer append-style recording of important execution facts.
- Do not silently discard information needed for replay, resume, or debugging.

## Coding conventions

### Rust style
- Use idiomatic Rust.
- Prefer explicit types at important API boundaries.
- Use `thiserror` for domain errors where appropriate.
- Use `anyhow` mainly at application edges, not for core contracts.
- Avoid panics in library code except for true invariants.
- Propagate errors with useful context.

### Async
- Be careful with cancellation, task lifetimes, and spawned work.
- Do not spawn detached tasks unless the lifecycle is clearly owned.
- Preserve backpressure and streaming semantics where relevant.
- Avoid blocking work in async code paths.

### Config and defaults
- Keep defaults safe and unsurprising.
- Do not silently enable dangerous capabilities.
- New config should have clear names and predictable precedence.

### Logging and output
- Logs should help debug long-running agent behavior.
- Do not emit noisy logs for normal control flow unless there is strong value.
- User-facing CLI output should remain concise; detailed traces belong in debug/logging paths.

## Build and verification

When changing Rust code, prefer these checks:

1. Format:
   - `cargo fmt --all`

2. Fast compile validation:
   - `cargo check --workspace`

3. Lints:
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`

4. Tests:
   - `cargo test --workspace`

If a full workspace run is too expensive for the task, run the narrowest meaningful crate-level checks and say so clearly.

## Change guidelines by area

### If changing `hermes-core`
- Be conservative.
- Assume downstream crates depend on these contracts.
- Highlight any schema, trait, or type changes clearly.
- Prefer additive changes over breaking ones.

### If changing `hermes-provider`
- Keep vendor-specific logic isolated.
- Preserve normalized internal semantics.
- Call out differences in streaming, tool-calling, response shape, finish reasons, and error semantics.
- Verify behavior against at least one representative provider path.

### If changing `hermes-agent`
- Preserve resumability and runtime invariants.
- Be explicit about loop termination, retries, compression, and approval handling.
- Watch for hidden coupling between orchestration and presentation.

### If changing `hermes-tools`
- Verify argument validation and error handling.
- Be explicit about side effects, approvals, and failure modes.
- Avoid expanding tool scope accidentally.

### If changing `hermes-memory` or `hermes-config`
- Preserve backward compatibility where feasible.
- Treat persistence format changes as high-risk.
- Call out migration or compatibility concerns.

### If changing `hermes-cli`
- Keep UX predictable.
- Do not bury runtime decisions in CLI-only code if they belong in reusable layers.
- Preserve scriptability where possible.

## Review priorities

When reviewing or generating code, prioritize:

1. Correctness
   - Does the code do what the change claims?
   - Are edge cases and error paths handled?

2. Architectural integrity
   - Does the change preserve model-agnostic boundaries?
   - Does it keep runtime logic reusable?

3. Long-running execution safety
   - Are resume, retry, timeout, streaming, and cancellation semantics preserved?

4. API and config stability
   - Does the patch unintentionally break callers, config compatibility, or persisted state?

5. Testability
   - Are the changed behaviors verifiable with tests or at least narrow checks?

## Preferred task behavior for Codex

When working in this repository:

- First inspect the relevant crate and its nearby modules before editing.
- Prefer reading existing types and traits before introducing new ones.
- Before broad refactors, explain the reason and likely blast radius.
- For cross-crate changes, summarize dependency flow and compatibility implications.
- After changes, provide a concise summary:
  - what changed
  - why it changed
  - what was verified
  - any remaining risk or follow-up

## Avoid

- Large speculative refactors
- Renaming crates or major modules without strong need
- Provider-specific hacks in shared abstractions
- Silent behavioral changes to approvals, execution safety, or persistence
- Mixing style-only cleanup with semantic changes
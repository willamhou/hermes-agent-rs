# Managed Run Restart-Safe Recovery

**Date**: 2026-04-23  
**Status**: Partially implemented; ownership/lease recovery, safe-point managed transcript checkpointing, provider fences, process/browser action handoff summaries, MCP runtime/session checkpoints, opt-in interrupted auto replay, plus process-like, browser-session (`process_group/root_pid + user_data_dir`), and MCP HTTP session/resource-subscription durable cleanup manifests are in place
**Purpose**: Define how managed runs behave after gateway process death without pretending the current runtime can transparently resume in-flight work.

---

## Problem

Today managed runs are durable in storage but **not** durable in execution ownership.

What exists now:

- a run row is persisted before execution starts
- terminal intent hints are recorded before normal terminal transitions
- ownership/lease fields and heartbeat semantics exist for active managed runs
- durable `run.ownership_claimed` events now record when a worker successfully acquires execution ownership and starts a fresh lease
- durable `run.interrupted` events now carry structured interruption cause metadata (`lease_expired` vs `ownership_not_established`), and derived summaries surface that cause in operator-facing run views
- startup reconciliation turns leftover `pending` / `running` rows into ownership-aware terminal states
- replay already exists as a separate API path
- managed transcript history can be checkpointed incrementally at user / assistant / tool-result safe points when durable `session_id` storage is enabled
- persisted `run.continuation_checkpoint` events capture the latest safe continuation boundary (`user`, `assistant final`, `pending tool calls`, `tool results`)
- continuation from checkpointed history can now resume safe boundaries explicitly: execute missing pending tool results, continue after checkpointed tool results or user input, or complete directly from a checkpointed final assistant response
- persisted `run.provider_call_started` fences capture when execution has crossed provider dispatch but not yet reached a newer durable response checkpoint
- persisted `tool.process_started|completed|failed|timed_out` milestones capture `terminal` / `execute_code` process windows that crossed process start or completion before a newer durable tool-result checkpoint
- persisted `tool.browser_action_started|completed|failed` milestones capture browser action windows that crossed browser dispatch or completion before a newer durable tool-result checkpoint
- persisted `run.browser_session_checkpoint` events capture the last browser state that became durable only after a successful browser tool result was appended to transcript history
- persisted `tool.mcp_call_started|completed|failed` milestones capture MCP tool-call windows that crossed dispatch or completion before a newer durable tool-result checkpoint
- persisted `run.mcp_runtime_checkpoint` events capture when a successful MCP tool result still leaves the run depending on live MCP runtime/session continuity (for example active subscriptions)
- persisted run artifacts capture checkpointed assistant final outputs and tool outputs as additive recovery context beyond transcript/event summaries
- derived run summaries and recovery hints now surface the latest checkpointed artifact continuity from the current run or replay lineage
- replayed runs now persist structured replay/takeover provenance on `run.created`, making worker-triggered interrupted auto replay distinguishable from manual replay in operator surfaces
- replay provenance and replay-child summaries now record the source continuation boundary plus structured interruption cause, making it explicit whether takeover resumed from `user`, `pending tool calls`, `tool results`, or `assistant final`, and whether the source worker lost a lease or never finished ownership establishment
- source runs now emit durable `run.replayed` events when a replay child is created, making raw event streams show takeover/continuation directly
- interrupted source runs now derive replay-child replacement summaries, making the currently active continuation run explicit in operator surfaces and recovery hints
- interrupted source runs now emit durable `run.recovery_decided` events for auto-replay outcomes (`replay_started`, `manual_review`, `blocked`, `failed`), making periodic recovery-sweep decisions explicit in raw event streams and derived summaries
- interrupted source runs now emit durable `run.takeover_assessed` events before each replay/manual-review/block decision, capturing the evaluating worker, interruption cause, continuation boundary, provider-fence presence, blocking runtime risks, and stable takeover-lineage correlation as explicit takeover-assessment lineage
- source runs with an active replay child now persist a durable `follow_replay` recovery decision carrying replay-run plus explicit evaluator / takeover worker lineage, making takeover ownership explicit without re-deriving it from replay lineage alone
- derived `follow_replay` recovery decisions now also expose the active continuation leaf separately from the original direct replay child when takeover lineage deepens, so operator surfaces can point at the latest follow target without rewriting raw recovery-decision history
- deeper replay descendants now also append additive `follow_replay` recovery decisions back onto ancestor source runs, carrying the ancestor’s direct child alongside the current active/terminal leaf target so raw decision streams can follow lineage deepening without mutating earlier direct-child decisions
- replay provenance, replay-child summaries, continuation lineage, takeover views, source-side follow decisions, raw takeover updates, and replay-child ownership claims now all carry a stable `takeover_lineage_id`, so operators can correlate one continuation chain across hops without matching replay descendants heuristically
- derived run summaries now expose a dedicated takeover summary, folding replay-child state plus evaluator/takeover worker lineage into a single operator-facing view, and it follows the latest continuation leaf when replay descendants form a deeper lineage
- derived run summaries now also surface live ownership/lease snapshots for active runs, and takeover summaries include the replay child’s current owner/lease state when continuation is still actively leased
- source-run takeover summaries now also carry the latest continuation leaf's own recovery decision when that leaf later becomes interrupted or otherwise needs follow-up recovery, so the source side can show both "which leaf owns continuation" and "what that leaf currently recommends next"
- replay child runs now persist durable `run.takeover_established` events and derive explicit continuation-lineage summaries, so child-side operator surfaces can show the source run, replay depth, checkpoint boundary, and evaluator / takeover worker lineage even after later source-run recovery decisions evolve
- source runs now also persist durable `run.takeover_updated` events when a replay child first actively owns continuation and again when that continuation later becomes terminal, so raw event streams show both live takeover ownership and whether the lineage completed, failed, was cancelled, timed out, or was itself interrupted
- deeper replay descendants now propagate `run.takeover_updated` back through replay ancestry with relative `lineage_depth`, so ancestor source runs can observe the latest continuation leaf directly in raw event streams without treating intermediate-source checkpoint metadata as their own recovery boundary
- source-side `run.takeover_updated` events now also snapshot the latest continuation leaf's recovery decision when that leaf later needs manual review / replay / blocking, so raw event streams carry the same "which leaf + what next" protocol context as derived takeover summaries
- source-run takeover summaries now also carry the latest continuation leaf's ownership-release snapshot when that leaf has already gone terminal, so the source side can show who last owned the leaf and why ownership ended without switching to the leaf run
- source-run takeover summaries and source-side `run.takeover_updated` events now also carry the latest continuation leaf's takeover-assessment risk bits, so source-side operator surfaces can see the leaf's blocking runtime risks without switching to the leaf run
- source-run takeover summaries and source-side `run.takeover_updated` events now also carry the latest continuation leaf's durable continuation-checkpoint boundary, so source-side operator surfaces can see where the leaf last reached a replay-safe boundary without switching to the leaf run
- source-run takeover summaries and source-side `run.takeover_updated` events now also carry the latest continuation leaf's unresolved provider-call fence when it has already crossed provider dispatch after that safe boundary, so source-side operator surfaces can see ambiguous "last provider call may be reissued" windows without switching to the leaf run
- source-run takeover summaries and source-side `run.takeover_updated` events now also carry the latest continuation leaf's unresolved process-handoff summary, so source-side operator surfaces can see the leaf's last process tool/state boundary without switching to the leaf run
- source-run takeover summaries and source-side `run.takeover_updated` events now also carry the latest continuation leaf's unresolved browser-action handoff summary, so source-side operator surfaces can see the leaf's last browser action boundary without switching to the leaf run
- source-run takeover summaries and source-side `run.takeover_updated` events now also carry the latest continuation leaf's unresolved MCP tool-call handoff summary, so source-side operator surfaces can see the leaf's last MCP tool boundary without switching to the leaf run
- source-run takeover summaries and source-side `run.takeover_updated` events now also carry the latest continuation leaf's live MCP runtime checkpoint when it still depends on active subscriptions or runtime/session continuity, so source-side operator surfaces can see unresolved MCP runtime dependencies without switching to the leaf run
- source-run takeover summaries and source-side `run.takeover_updated` events now also carry the latest continuation leaf's artifact continuity, so source-side operator surfaces can see the leaf's newest durable assistant/tool output context without switching runs or querying `/artifacts`
- source-run takeover summaries and source-side `run.takeover_updated` events now also carry the latest continuation leaf's durable ownership-claim summary, so source-side operator surfaces can see which worker most recently claimed the leaf and what lease window it established without switching to the leaf run
- managed runs now also persist durable `run.ownership_released` events when an owned execution span ends in a terminal state or startup recovery interrupts an expired lease, making ownership end explicit without reconstructing it from cleared owner columns alone
- manual replay now also blocks when the requested run itself is still `pending`/`running`, recording a durable `blocked/run_still_active` recovery decision instead of allowing a second concurrent branch from a live execution span
- manual replay now blocks when a source run already has an active replay continuation, recording a durable `blocked/replay_child_active` recovery decision instead of creating a second concurrent takeover lineage
- manual replay from an ancestor run with a terminal replay/takeover leaf now follows that latest leaf as the continuity source, preventing stale-ancestor branching when the leaf holds the newest replay prompt/session context
- cancellation now follows active takeover lineage as well: deleting a source run that already points at an active replay continuation cancels the current active replay leaf and then refreshes ancestor takeover summaries
- opt-in recovery can auto-replay interrupted runs after durable cleanup reclaim, with configurable replay-depth caps
- process-like cleanup resources, browser session state (`process_group/root_pid + user_data_dir`), and MCP HTTP sessions/resource subscriptions can be durably persisted and reclaimed after restart

What does not exist:

- checkpointed provider execution state that could resume mid-provider-call
- fully durable tool side-effect checkpoints beyond the current transcript-safe-point, provider-fence, `terminal` / `execute_code` process-handoff, browser-action handoff, MCP tool-call handoff, and MCP runtime checkpoint boundaries
- durable cleanup manifests for browser session recovery beyond the current `process_group/root_pid + user_data_dir` boundary, deeper MCP runtime resources, or other non-process resource classes

As a result, process restart currently collapses active runs into generic terminal guesses. That is acceptable for beta hardening, but it is not a durable execution contract.

---

## Non-Goals

This design does **not** promise:

- transparent resumption of an in-flight provider request or SSE stream
- exact replay of partially completed tool side effects
- automatic reattachment to browser, MCP, terminal, or `execute_code` runtime state after process death
- a distributed worker queue in the first slice

Those need deeper runtime checkpoints and durable resource manifests. They should not be implied by a “restart-safe recovery” label.

---

## Decision

Hermes should adopt **ownership + lease + explicit interruption** semantics.

The key choice is:

> after process death, default recovery is **interrupted-then-replay**, not transparent resume

More specifically:

1. Active runs get a persistent owner and renewable lease.
2. Loss of owner lease does **not** become a generic `failed` run by default.
3. Runs orphaned by process death become an explicit terminal state: `interrupted`.
4. Existing stronger intent is preserved when available:
   - terminal intent hint wins
   - persisted cancel request becomes `cancelled`
   - otherwise the run becomes `interrupted`
5. Recovery continues through replay into a **new** run, not by reviving the old task in place.

This is the strongest truthful contract the current architecture can support.

---

## Why Not Transparent Resume

Transparent resume is the wrong first target for this codebase.

Reasons:

- provider calls are awaited as single futures and are not checkpointed at resumable boundaries
- tool execution can have irreversible side effects
- only process-like cleanup registrations, browser session (`process_group/root_pid + user_data_dir`) manifests, and MCP HTTP session/resource-subscription manifests are durably recoverable today
- `terminal` / `execute_code` now expose unresolved process-handoff windows, but they still do not support transparent process continuation after restart
- `browser` now exposes unresolved browser-action handoff windows, but it still does not support transparent browser runtime/session continuation after restart
- even after a successful browser tool-result checkpoint, Hermes does not currently recreate the live browser runtime/page/session automatically, so open browser session checkpoints remain `manual_review` recovery territory
- the agent loop uses `run.id` as its internal execution session id, so the in-memory turn state dies with the process
- persisted managed `session_id` is primarily chat-history continuity; current safe-point transcript checkpoints still do not recreate an in-flight provider/tool execution stack

Trying to call this “resume” would over-promise and make correctness worse.

---

## Public Prior Art (Inference)

This section is intentionally limited to **public official docs as of 2026-04-24**. It is an inference from published product semantics, not a claim about undocumented internals.

### Anthropic

Anthropic's public agent surface currently reads more like **session persistence plus application-owned execution** than a hosted managed-run control plane:

- The [Claude Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview) emphasizes resuming and forking sessions to keep context across exchanges.
- The [sessions guide](https://code.claude.com/docs/en/agent-sdk/sessions) says sessions persist the conversation history, not the filesystem, and explains that resuming across hosts requires moving the local session file or carrying forward application state into a fresh session.
- The [tool use overview](https://platform.claude.com/docs/en/agents-and-tools/tool-use/overview) makes a sharp distinction between client-executed tools and Anthropic-executed server tools.

The important implication for Hermes is:

- Anthropic's public docs strongly separate **conversation continuity** from **filesystem or external-runtime continuity**
- that is much closer to `session_id` semantics than to transparent revival of an in-flight managed run

### OpenAI

OpenAI's public agent/runtime surface currently reads more like **hosted response/conversation objects plus optional background execution**:

- The [Responses API create reference](https://developers.openai.com/api/reference/resources/responses/methods/create) exposes `background` and `conversation` directly on the response-creation surface.
- The same reference says response input/output items are automatically attached to the associated conversation, which is a stronger hosted-state model than a local transcript file.
- The [Agents SDK overview](https://developers.openai.com/api/docs/guides/agents) separates SDK-owned orchestration from the hosted `Agent Builder` and `ChatKit` workflow path, and it explicitly calls out `Results and state` as a first-class concept.

The important implication for Hermes is:

- OpenAI's public docs model execution as a platform object with hosted state surfaces
- that is closer to a durable managed execution plane than Anthropic's session-first SDK story
- but it still does **not** justify claiming Hermes can transparently resume a lost in-process execution stack

### Why Hermes Lands in the Middle

Taken together, these public docs suggest a useful split:

| Concern | Anthropic public emphasis | OpenAI public emphasis | Hermes design choice |
| --- | --- | --- | --- |
| Context continuity | Session resume/fork | Conversation state | Reuse `session_id` |
| Execution identity | SDK call / local session history | Hosted response/run-like object semantics | Distinct `run.id` |
| Runtime ownership | Largely application-owned for client tools | Platform-owned state surfaces are explicit | Persist `owner_worker_id` + lease |
| Restart recovery | Resume conversation, not filesystem/runtime | Background/stateful hosted execution surfaces | `interrupted` then replay |

That is why this design deliberately does **not** equate “resume the session” with “resume the run”.

In Hermes:

- `session_id` carries conversation continuity
- `run.id` carries one execution attempt
- owner/lease fields carry live execution ownership

If ownership disappears, the truthful state transition is `interrupted`, not fake in-place resume.

---

## Recovery Semantics

### Terminal outcomes after restart

When startup recovery inspects a run left in `pending` or `running`, the recovery result should be:

- `completed`, `failed`, `cancelled`, or `timed_out` if a persisted terminal intent already exists
- `cancelled` if `cancel_requested_at` was already recorded and no stronger terminal intent exists
- `interrupted` otherwise

`interrupted` is terminal and replayable.

It means:

- the control plane lost execution ownership before the run reached a normal terminal boundary
- Hermes cannot prove whether the last in-flight provider/tool action completed
- the correct next step is replay, not pretend-resume

### Replay semantics

Replay remains a separate run:

- source run stays terminal
- replay creates a new run with `replay_of_run_id = <source>`
- replay reuses the source `session_id` when one exists
- replay reuses the immutable agent version snapshot already referenced by the source run

This keeps replay explicit, auditable, and compatible with the existing API.

---

## State Model Changes

### New run status

Add a new `ManagedRunStatus`:

- `Interrupted`

Properties:

- terminal
- replayable
- distinct from `Failed`

Rationale:

- `failed` should mean the run reached a normal failure boundary in owned execution
- `interrupted` means ownership was lost and Hermes cannot truthfully classify the final in-flight step

### New run event

Add a new `ManagedRunEventKind`:

- `run.interrupted`

Suggested event message:

- `managed run interrupted after worker lease expired during execution`

Suggested metadata:

- `worker_id`
- `claim_token`
- `lease_expires_at`
- `recovery_reason`

---

## Persistent Ownership Model

### New fields on `runs`

Add additive columns:

- `owner_worker_id TEXT`
- `owner_claim_token TEXT`
- `owner_claimed_at TEXT`
- `owner_last_heartbeat_at TEXT`
- `owner_lease_expires_at TEXT`

Optional future-friendly column:

- `recovery_note TEXT`

The first five are the real contract. They let Hermes distinguish:

- an actively owned run
- an unowned run
- a stale run whose owner lease expired

### Worker identity

Each gateway process gets a boot-scoped worker id, for example:

- `gw_<uuid>`

Each claimed run also gets a claim token:

- `claim_<uuid>`

The claim token prevents stale owner updates from overwriting a recovered run if a process pauses and later resumes unexpectedly.

### Claim rules

When a run starts:

1. create the run row as `pending`
2. atomically claim it by filling ownership columns
3. transition it to `running`
4. start execution

Claim should succeed only when:

- the run is unowned, or
- the existing lease is expired

### Heartbeat rules

While a run is active:

- the worker renews `owner_last_heartbeat_at`
- the worker extends `owner_lease_expires_at`

Suggested first-cut values:

- heartbeat every `10s`
- lease duration `45s`

These values are operational defaults, not API contract.

### Terminal rules

Normal terminal paths must:

- record terminal intent as they do today
- clear owner columns
- stop heartbeating
- append the terminal run event

If a terminal update fails its `claim_token` compare-and-set, the worker has lost ownership and must stop writing further state.

---

## Startup Recovery Algorithm

On startup, the gateway should recover runs in this order:

1. find runs in `pending` or `running`
2. classify each run:
   - live owner lease: leave untouched
   - missing or expired owner lease: recover it
3. resolve recovered status:
   - terminal intent hint if present
   - else `cancelled` if `cancel_requested_at` exists
   - else `interrupted`
4. append the matching terminal event
5. clear owner fields

This replaces the current “generic reconcile everything active” behavior with an ownership-based rule.

---

## API and CLI Implications

### API

`/v1/runs` and `/v1/runs/{id}` must expose `interrupted` as a first-class status.

The existing replay endpoint remains correct:

- `POST /v1/runs/{id}/replay`

No new recovery endpoint is required in the first slice.

### CLI

`hermes runs list|get|replay` should:

- display `interrupted`
- treat it as replayable

Optional debug output can later expose ownership fields in `--json`, but that is not required for the first implementation.

---

## Interaction With Session History

Managed `session_id` already gives Hermes a clean recovery boundary.

Important distinction:

- `run.id` is execution identity
- `session_id` is durable conversation identity

That means:

- an interrupted run should **not** try to reuse its old execution identity
- replay **should** reuse the durable `session_id` when present

This matches the current replay shape and avoids inventing a fake in-place resume model.

---

## Interaction With Cleanup

This design intentionally separates **run-state recovery** from **resource reclamation**.

The implemented Phase 1 plus the first Phase 2 slice now guarantee:

- truthful run-state recovery
- explicit interruption semantics
- replayability
- persisted cleanup manifests for process-like resources
- persisted cleanup manifests for browser session state (`process_group/root_pid + user_data_dir`) attached to browser-session cleanup
- best-effort restart-time cleanup sweeps for terminal managed runs that still have persisted process-like resources
- runtime-owned MCP stdio server teardown that kills full server process groups and explicitly owns stdio reader/logger tasks
- runtime-owned MCP HTTP notification-stream tasks that are explicitly owner-tracked instead of detached
- persisted MCP runtime worker leases plus shared stdio runtime manifests and shared HTTP runtime session manifests, with startup/periodic reclaim of stale stdio server process groups and shared HTTP sessions

It does **not** yet guarantee reclaiming every resource created before the process died, because full browser session state and deeper MCP runtime cleanup still rely on in-memory registration.

Follow-on work should extend the durable resource manifest to the remaining resource classes that need restart-time reclamation:

- full browser session state beyond the root browser process
- deeper MCP runtime ownership beyond managed HTTP sessions/resource subscriptions plus shared stdio/HTTP runtime manifests

That follow-on work is adjacent, but it no longer blocks the base ownership/lease design or the current opt-in interrupted auto-replay slice.

---

## Rollout Plan

### Phase 1: Truthful Recovery Contract

Status: Implemented

Implement:

- `Interrupted` status and `run.interrupted` event
- persistent run owner + claim token + lease fields
- heartbeat loop for active runs
- ownership-based startup recovery
- replay support for interrupted runs

Exit criteria:

- process restart no longer turns active runs into generic `failed`
- ownership loss is explicit and testable
- interrupted runs can be replayed manually through the existing API

### Phase 2: Durable Cleanup Manifests

Status: Partially implemented for process-like resources and browser session state (`process_group/root_pid + user_data_dir`)

Implement:

- persisted cleanup metadata for restart-time reclamation
- recovery-time cleanup attempts for persisted terminal-run resources
- extend persistence beyond process-like resources and the current browser-session (`process_group/root_pid + user_data_dir`) boundary to richer browser and MCP ownership

Exit criteria:

- restart recovery can make a best-effort attempt to reclaim known external resources

### Phase 3: Optional Auto-Replay Policies

Implement only after Phase 1 proves stable:

- per-agent or per-request replay policy
- guarded auto-replay for explicitly replay-safe workloads

Out of scope for this phase:

- generic exactly-once semantics

### Phase 4: Real Resume, If Ever

Only pursue this if Hermes later gains:

- checkpointed provider boundaries
- durable tool-side effect journals
- resumable execution frames in the agent loop

Without those, “resume” remains marketing, not engineering.

---

## Open Questions

These should be resolved during implementation planning, not before writing the first ownership slice:

- should `pending` runs ever persist long enough to need leases, or should claim happen immediately after insert?
- should replay from `interrupted` be manual-only in the first release, or can a worker-triggered auto-replay be safely hidden behind config?
- do we want owner fields in public run JSON immediately, or only in debug/CLI JSON first?

---

## Recommended Next PR

The next implementation PR should be narrow:

1. add `Interrupted` status and `run.interrupted` event
2. add owner / lease columns to `runs`
3. replace startup generic reconciliation with ownership-based recovery
4. keep replay as the only recovery path

That gets Hermes from “best-effort guess” to a principled execution contract without pretending to have transparent resumability.

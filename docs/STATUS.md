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
  - durable run replay via `/v1/runs/{id}/replay`
  - persisted run events via `/v1/runs/{id}/events`
  - persisted run artifacts via `/v1/runs/{id}/artifacts`
  - optional managed session history inheritance via explicit `session_id`
  - ownership/lease-based startup reconciliation for runs left `pending` / `running` during process exit
  - explicit `interrupted` status for owner-loss recovery and replay
  - structured `run.interrupted` metadata plus derived interruption summaries, distinguishing ownership-not-established from worker-lease-expired interruptions across `/v1/runs` list/get/events and `hermes runs list|get|events`
  - durable `run.ownership_claimed` events when a worker successfully takes execution ownership of a managed run, so raw event streams expose claim time and lease start rather than only the latest ownership snapshot
  - incremental managed transcript checkpoints at user / assistant / tool-result safe points whenever durable `session_id` storage is enabled
  - persisted `run.continuation_checkpoint` events plus derived run summaries for safe continuation boundaries (`user`, `assistant final`, `pending tool calls`, `tool results`)
  - interrupted continuation can now resume from checkpointed safe boundaries: execute missing tool results, continue after checkpointed tool results/user input, or complete directly from a checkpointed final assistant response
  - persisted `run.provider_call_started` fences plus derived provider-fence summaries for the ambiguous window between provider dispatch and the next durable response checkpoint
  - persisted `tool.process_started|completed|failed|timed_out` milestones for `terminal` / `execute_code`, plus derived unresolved process-handoff summaries for execution windows that crossed process start or completion without reaching a newer durable tool-result checkpoint
  - persisted `tool.browser_action_started|completed|failed` milestones for `browser`, plus derived unresolved browser-handoff summaries for action windows that crossed browser dispatch or completion without reaching a newer durable tool-result checkpoint
  - persisted `run.browser_session_checkpoint` summaries for the last durably checkpointed browser state after successful browser tool results
  - persisted `tool.mcp_call_started|completed|failed` milestones plus derived unresolved MCP-handoff summaries for MCP tool-call windows that crossed dispatch or completion without reaching a newer durable tool-result checkpoint
  - persisted `run.mcp_runtime_checkpoint` summaries for checkpointed MCP runtime/session state that still depends on live subscriptions or unresolved runtime continuity after a successful MCP tool result
  - persisted run artifacts for checkpointed assistant final outputs and tool outputs, exposed through `/v1/runs/{id}/artifacts` and `hermes runs artifacts`, with optional replay-lineage inspection
  - derived managed run summaries and recovery hints now fold in latest artifact continuity from the current run or replay lineage, so operators can see checkpointed output context without querying the artifacts endpoint separately
  - replayed managed runs now persist structured replay/takeover provenance on `run.created`, including manual-vs-auto trigger, replay depth, root run id, source status when known, resumed-turn intent, and triggering worker id
  - replay provenance and replay-child summaries now record the source continuation boundary (`user`, `pending tool calls`, `tool results`, or `assistant final`) plus structured interruption cause when a replay child takes over from an interrupted source run
  - source runs now emit durable `run.replayed` events when a replay child is created, so raw event streams capture takeover/continuation without relying only on derived summaries
  - interrupted source runs now derive structured replay-child replacement summaries, so operators can see which replay child currently owns continuation and recovery hints can prefer `follow_replay` over issuing another replay
  - interrupted source runs now emit durable `run.recovery_decided` events for auto-replay outcomes (`replay_started`, `manual_review`, `blocked`, `failed`), and derived summaries surface the latest recovery decision across `/v1/runs` list/get/events and `hermes runs list|get|events`
  - source runs that already have a replay child now persist a durable `follow_replay` recovery decision with the active replay run id plus explicit evaluator / takeover worker lineage, so takeover state is explicit even before a later worker evaluates recovery hints
  - derived `follow_replay` recovery decisions now also expose the active continuation leaf separately from the original direct replay child when takeover lineage deepens, so operator surfaces can show the current follow target without re-deriving replay descendants
  - deeper replay descendants now also append additive `follow_replay` recovery decisions back onto ancestor source runs, carrying the direct child on that ancestor plus the current active/terminal leaf target, so raw decision history can follow lineage deepening without rewriting older direct-child decisions
  - replay provenance, replay-child summaries, continuation lineage, takeover views, source-side follow decisions, raw takeover updates, and replay-child ownership claims now all carry a stable `takeover_lineage_id`, so operator tooling can correlate one continuation chain across hops without matching descendants heuristically
  - interrupted source runs now persist durable `run.takeover_assessed` events before each replay/manual-review/block decision, making the evaluating worker, continuation boundary, interruption cause, provider-fence presence, blocking runtime risks, and stable takeover-lineage correlation explicit in raw event streams and derived summaries
  - derived run summaries now also expose a dedicated takeover summary, folding replay-child state plus evaluator/takeover worker lineage into a single operator-facing view across `/v1/runs` list/get/events and `hermes runs list|get|events`, and it now follows the latest continuation leaf when replay descendants form a deeper lineage
  - derived run summaries now surface live ownership/lease snapshots for active runs, and takeover summaries include the replay child’s current owner/lease state when continuation is still actively leased
  - replay child runs now persist durable `run.takeover_established` events and derive explicit continuation-lineage summaries, so the child side can show source run, replay depth, continuation boundary, and evaluator / takeover worker lineage without re-deriving it from source-run state
  - source runs now also persist durable `run.takeover_updated` events when a replay child first actively owns continuation and again when that continuation later becomes terminal, so raw event streams capture both active takeover ownership and how the continuation child ultimately ended
  - deeper replay descendants now propagate `run.takeover_updated` back through replay ancestry with relative `lineage_depth`, so ancestor source runs can see the latest active/terminal continuation leaf directly in raw event streams without mislabeling intermediate-source checkpoint metadata
  - ancestor/source takeover summaries now also surface the latest continuation leaf's own recovery decision when that leaf later becomes interrupted or otherwise needs a follow-up recovery action, so operators can see "which leaf to follow" and "what that leaf currently recommends" from the same source-run view
  - source-side `run.takeover_updated` events now also snapshot the latest continuation leaf's recovery decision when that leaf later needs manual review / replay / blocking, so raw event streams carry the same "which leaf + what next" protocol context as derived takeover summaries
  - source-run takeover summaries now also surface the latest continuation leaf's ownership-release snapshot when that leaf has already gone terminal, so operators can see who last owned the leaf and why ownership ended without switching to the leaf run
  - source-run takeover summaries and source-side `run.takeover_updated` events now also snapshot the latest continuation leaf's takeover-assessment risk bits, so source-side operator views can see the leaf's blocking runtime risks without switching to the leaf run
  - source-run takeover summaries and source-side `run.takeover_updated` events now also snapshot the latest continuation leaf's durable continuation-checkpoint boundary, so source-side operator views can see where the leaf last reached a replay-safe boundary without switching to the leaf run
  - source-run takeover summaries and source-side `run.takeover_updated` events now also snapshot the latest continuation leaf's unresolved provider-call fence when it has already crossed provider dispatch after that safe boundary, so source-side operator views can see ambiguous "last provider call may be reissued" windows without switching to the leaf run
  - source-run takeover summaries and source-side `run.takeover_updated` events now also snapshot the latest continuation leaf's unresolved process-handoff summary, so source-side operator views can see the leaf's last process tool/state boundary without switching to the leaf run
  - source-run takeover summaries and source-side `run.takeover_updated` events now also snapshot the latest continuation leaf's unresolved browser-action handoff summary, so source-side operator views can see the leaf's last browser action boundary without switching to the leaf run
  - source-run takeover summaries and source-side `run.takeover_updated` events now also snapshot the latest continuation leaf's unresolved MCP tool-call handoff summary, so source-side operator views can see the leaf's last MCP tool boundary without switching to the leaf run
  - source-run takeover summaries and source-side `run.takeover_updated` events now also snapshot the latest continuation leaf's live MCP runtime checkpoint when it still depends on active subscriptions or runtime/session continuity, so source-side operator views can see unresolved MCP runtime dependencies without switching to the leaf run
  - source-run takeover summaries and source-side `run.takeover_updated` events now also snapshot the latest continuation leaf's artifact continuity, so source-side operator views can see the leaf's newest durable assistant/tool output context without switching runs or querying `/artifacts`
  - source-run takeover summaries and source-side `run.takeover_updated` events now also snapshot the latest continuation leaf's durable ownership-claim summary, so source-side operator views can see which worker most recently claimed the leaf and what lease window it established without switching to the leaf run
  - managed runs now also persist durable `run.ownership_released` events when an owned execution span ends in a terminal state or startup recovery interrupts an expired lease, so ownership end is visible in raw event streams instead of only being inferred from status + cleared owner columns
  - manual replay now also blocks with `409` when the requested run itself is still `pending`/`running`, recording a durable `blocked/run_still_active` recovery decision instead of allowing a second concurrent branch off a live execution
  - manual replay now blocks with `409` when a source run already has an active replay continuation, and records a durable `blocked/replay_child_active` recovery decision instead of starting a second takeover lineage
  - manual replay from an ancestor run that already has a terminal replay/takeover leaf now follows that latest leaf as the continuity source, so retries do not branch again from a stale ancestor row or its missing prompt/session context
  - cancelling a source run with an active replay continuation now follows the active replay leaf, so operator `DELETE /v1/runs/{source}` cancels the current continuation owner instead of leaving the live leaf running
  - interrupted auto replay now refuses runs with unresolved process risk, risky browser handoff risk, checkpointed live browser session state, unresolved risky MCP handoff, or checkpointed live MCP runtime/session state, surfacing `manual_review` recovery hints instead of blindly replaying shell/code/browser/MCP side effects
  - opt-in interrupted-run auto replay with configurable depth caps, reusing persisted prompt and `session_id` after cleanup reclaim and continuing from checkpointed session history when available
  - session-scoped termination cleanup for process-like resources and browser sessions
  - browser sessions now self-clean on unexpected handler / process exit, unregistering durable manifests and removing per-session profile dirs during normal runtime
  - durable cleanup manifests for process-like resources, browser session state (`process_group/root_pid + user_data_dir`), MCP HTTP sessions, and MCP HTTP resource subscriptions, with restart-time reclamation sweeps
  - cleanup failure telemetry surfaced in persisted `run.cleanup_failed` events during terminal cleanup and recovery reclaim
  - managed MCP admission rejections now surface as persisted `run.mcp_admission_rejected` events with structured policy / requested-tool metadata emitted directly from managed runtime build / preflight paths
  - derived managed run summaries now surface across `/v1/runs` list/get/events and `hermes runs list|get|events`, covering MCP admission rejections, cleanup failure summaries, and interrupted-run recovery hints
  - runtime-owned MCP stdio server shutdown now kills the full server process group and explicitly owns stdio reader/logger tasks, and HTTP notification streams are explicitly owner-tracked instead of detached
  - persisted MCP runtime worker leases plus shared stdio runtime manifests and shared HTTP runtime session manifests, with startup/periodic reclaim of stale stdio server process groups and shared HTTP sessions
  - persisted shared MCP runtime audit events for startup / periodic reclaim outcomes, plus CLI inspection via `hermes mcp audits`
  - explicit operator policy plus managed-local MCP registries for an allowlisted HTTP read-only bridge slice: `mcp_prompt_list`, `mcp_prompt_get`, `mcp_resource_list`, `mcp_resource_template_list`, and `mcp_resource_read`
  - explicit `managed.mcp.stdio` candidate policy fields for allowlisted stdio servers and redacted env-key audit, surfaced in managed MCP admission rejection metadata while stdio admission remains blocked
  - operator-facing MCP inspection via `hermes mcp inspect`, aggregating current managed MCP policy, configured non-allowlisted HTTP servers, recent `run.mcp_admission_rejected` events with structured read-only capability attribution, and recent shared-runtime reclaim audits
  - stronger managed MCP preflight rejection semantics for stdio-only bridge tools, stdio-only dynamic MCP tools, read-only MCP bridge requests when operator policy leaves only stdio candidates, and allowlisted HTTP read-only requests whose servers either lack prompt/resource capability or are shadowed by non-allowlisted HTTP servers that do expose it
  - CLI `hermes agents ...` commands plus YAML `diff` / `sync`
  - CLI `hermes runs ...` inspection, replay, and Signet verification commands
  - CLI `hermes mcp audits` inspection for shared runtime reclaim history
  - optional Signet request/response receipts for managed tool calls
  - example scripts for managed API smoke tests, Signet verification, and a repository GitHub Actions workflow
  - dedicated CI coverage for live browser interaction, cleanup, and managed reclaim tests when a Chrome/Chromium executable is provisioned

## Current Priorities

- Deeper MCP runtime/session continuation beyond the current tool-call and runtime-checkpoint recovery boundaries
- Expand managed MCP beyond the current HTTP read-only bridge slice without weakening cleanup / ownership guarantees
- Broader runtime preflight enforcement beyond the current stdio-specific and read-only capability-level rejection semantics
- Broader replay / migration policies beyond the current opt-in interrupted auto replay contract

## Still Missing From The Managed Beta Roadmap

- Durable cleanup manifests for richer browser session recovery beyond the current browser-session (`process_group/root_pid + user_data_dir`) boundary
- Dynamic MCP tools, side-effecting MCP bridge tools, and `stdio` MCP in managed mode
- Vault / KMS or a hosted audit pipeline
- Multi-tenant namespaces and RBAC
- Web UI

## How To Read The Docs

- [README.md](../README.md): public project entrypoint and quick start
- [docs/specs/2026-04-22-managed-agents-v1-beta-plan.md](./specs/2026-04-22-managed-agents-v1-beta-plan.md): current managed-agents beta contract
- [docs/specs/2026-04-23-agent-platform-roadmap.md](./specs/2026-04-23-agent-platform-roadmap.md): roadmap from managed beta to a broader agent platform
- [docs/specs/2026-04-23-managed-run-restart-recovery.md](./specs/2026-04-23-managed-run-restart-recovery.md): proposed ownership/lease-based recovery contract for in-flight managed runs
- [docs/specs/2026-04-26-managed-mcp-admission-criteria.md](./specs/2026-04-26-managed-mcp-admission-criteria.md): admission bar for managed MCP-backed tools
- [AGENTS.md](../AGENTS.md): repository working rules and architecture guardrails
- `docs/specs/`: design history and phase-by-phase implementation notes

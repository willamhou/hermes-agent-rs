# Phase 6: Cron Scheduler — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a cron scheduling system that executes agent tasks on schedule, stores output, and integrates with the gateway.

**Architecture:** CronJob model + JSON file store + scheduler tick loop + agent-callable tool. Scheduler runs inside the gateway or standalone via `hermes cron tick`.

**Tech Stack:** cron crate (parsing), chrono (datetime), fs2 (file locking), serde_json

---

## Task 1: CronJob Model + Schedule Parsing

**Files:**
- Create: `crates/hermes-cron/src/job.rs`
- Modify: `crates/hermes-cron/src/lib.rs`
- Modify: `crates/hermes-cron/Cargo.toml`

- [ ] **Step 1: Add dependencies**

Add to `crates/hermes-cron/Cargo.toml` [dependencies]:
```toml
hermes-core.workspace = true
serde.workspace = true
serde_json.workspace = true
chrono.workspace = true
cron.workspace = true
uuid.workspace = true
tracing.workspace = true
anyhow.workspace = true
```

Add [dev-dependencies]:
```toml
[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
tempfile.workspace = true
```

- [ ] **Step 2: Implement job.rs**

CronJob struct, JobSchedule enum (tagged serde), `parse_schedule(input: &str) -> Result<JobSchedule>`, `compute_next_run(schedule, after) -> Option<DateTime<Utc>>`.

Parse rules: ends with "m"/"min" → Interval minutes, "h" → hours*60, "d" → days*1440, contains spaces and cron-like → Cron, otherwise try ISO 8601 → Once.

8 tests: parse interval, hours, days, cron expr, ISO timestamp, invalid input, next_run for each type, Once past returns None.

- [ ] **Step 3: Wire up lib.rs + commit**

Commit: `feat: implement CronJob model with schedule parsing`

---

## Task 2: JobStore (JSON CRUD)

**Files:**
- Create: `crates/hermes-cron/src/store.rs`
- Modify: `crates/hermes-cron/src/lib.rs`

- [ ] **Step 1: Implement store.rs**

JobStore with path to jobs.json. Methods: `open()`, `list()`, `get(id)`, `create(job)`, `update(job)`, `remove(id)`.

Atomic write: write to `.tmp`, rename. Create dirs on open.

JobsFile wrapper: `{ jobs: Vec<CronJob>, updated_at: String }`.

6 tests (use tempdir): create+get, list order, update fields, remove, empty file handling, atomic write survives crash (write then verify).

- [ ] **Step 2: Commit**

Commit: `feat: implement JobStore with atomic JSON persistence`

---

## Task 3: CronScheduler

**Files:**
- Create: `crates/hermes-cron/src/scheduler.rs`
- Modify: `crates/hermes-cron/src/lib.rs`
- Modify: `crates/hermes-cron/Cargo.toml` (add hermes-agent, hermes-provider, hermes-tools, hermes-memory, hermes-config, hermes-skills, tokio, secrecy)

- [ ] **Step 1: Implement scheduler.rs**

CronScheduler struct with store, output_dir, app_config.

`tick()` — find due jobs, run each, save output, mark completed.

`run_job(job)` — build fresh Agent (same pattern as gateway's build_session_agent but with cron-specific config: no skills, no clarify, no delegation, disabled cron tool to prevent recursion). Run `agent.run_conversation(&job.prompt, ...)`. Return JobRunResult.

`save_output(job, result)` — write markdown to output dir.

`mark_completed(job, result)` — update last_run_at, last_status, compute next next_run_at, store.update(). For Once jobs: set enabled=false after completion.

`build_cron_agent(app_config)` — simplified Agent constructor (shared provider from config, fresh registry from_inventory, no skills/clarify/delegation).

5 tests: tick with due job, tick skips non-due, tick skips disabled, output file created, Once job disabled after run.

For tests: use MockProvider (same pattern from agent tests).

- [ ] **Step 2: Commit**

Commit: `feat: implement CronScheduler with job execution and output storage`

---

## Task 4: CronTool (agent-callable)

**Files:**
- Create: `crates/hermes-cron/src/tool.rs`
- Modify: `crates/hermes-cron/src/lib.rs`
- Modify: `crates/hermes-cli/src/tooling.rs` (register manually)

- [ ] **Step 1: Implement tool.rs**

CronTool struct holding a shared JobStore (Arc<Mutex<JobStore>> or just PathBuf for the store path).

Implement Tool trait: name="cron", toolset="scheduling", is_exclusive=true.

Actions: create (parse schedule, create job), list (format table), remove, pause, resume, trigger (set next_run_at to now).

Since CronTool is in hermes-cron (not hermes-tools), register manually in CLI tooling.rs (same pattern as DelegationTool).

4 tests: create action returns job_id, list action returns JSON, remove action, pause/resume toggle.

- [ ] **Step 2: Register in tooling.rs**

```rust
use hermes_cron::CronTool;
registry.register(Box::new(CronTool::new(cron_store_path)));
```

- [ ] **Step 3: Commit**

Commit: `feat: implement agent-callable cron tool`

---

## Task 5: CLI + Gateway Integration

**Files:**
- Modify: `crates/hermes-cli/src/main.rs` (cron subcommand)
- Modify: `crates/hermes-cli/src/handlers.rs` (/cron upgrade)
- Modify: `crates/hermes-gateway/src/runner.rs` (spawn scheduler)
- Modify: `crates/hermes-cli/Cargo.toml` (add hermes-cron)

- [ ] **Step 1: Add CLI cron subcommand**

```rust
enum Commands {
    Gateway,
    #[command(subcommand)]
    Cron(CronAction),
}

enum CronAction {
    /// Run one scheduler tick
    Tick,
    /// List all scheduled jobs
    List,
}
```

Implement `run_cron_tick()` and `run_cron_list()`.

- [ ] **Step 2: Upgrade /cron handler**

Replace stub `handle_cron()` with real implementation that calls JobStore::list() and prints a table.

- [ ] **Step 3: Gateway scheduler integration**

In `runner.rs`, spawn the scheduler tick loop:
```rust
let scheduler = CronScheduler::new(store, output_dir, app_config);
tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        if let Err(e) = scheduler.tick().await { tracing::warn!("cron tick: {e}"); }
    }
});
```

- [ ] **Step 4: Commit**

Commit: `feat: add cron CLI subcommand and gateway scheduler integration`

---

## Task 6: Full Verification

- [ ] `cargo fmt && cargo clippy --workspace -- -D warnings && cargo test --workspace`
- [ ] `cargo build --release -p hermes-cli`
- [ ] Smoke test: `hermes cron list` → empty
- [ ] Smoke test with API: create a cron job via agent, then `hermes cron list` to verify

Commit: `chore: fix Phase 6 build issues`

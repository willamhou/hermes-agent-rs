# Phase 6: Cron Scheduler — Design Spec

**Date**: 2026-04-18
**Status**: Final
**Depends on**: Phase 5 (Gateway complete)
**Validates**: Scheduled automation, isolated job execution

---

## 1. Scope

### In Scope
- CronJob model (id, name, prompt, schedule, deliver, status tracking)
- JobStore (JSON file CRUD with atomic writes)
- CronScheduler (60s tick, spawn fresh Agent per job, file lock)
- Schedule parsing (interval "30m"/"2h", cron expression, once timestamp)
- Cron tool (agent-callable: create/list/remove/pause/resume/trigger)
- Job output storage (~/.hermes/cron/output/)
- Delivery to gateway adapters or local file
- CLI /cron command upgrade
- `hermes cron tick` CLI subcommand for manual tick

### Out of Scope
- Pre-run scripts, per-job model override, repeat limits
- Prompt injection scanning
- [SILENT] response suppression
- Media extraction from response
- Batch processing, RL integration

---

## 2. CronJob Model

```rust
// hermes-cron/src/job.rs
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,                     // 12-char hex (uuid prefix)
    pub name: String,
    pub prompt: String,
    pub schedule: JobSchedule,
    pub deliver: String,                // "local" | "telegram:chat_id" | "api:session_id"
    pub enabled: bool,
    pub created_at: String,             // ISO 8601
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub last_status: Option<String>,    // "success" | "failed"
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum JobSchedule {
    #[serde(rename = "once")]
    Once { run_at: String },            // ISO 8601 timestamp
    #[serde(rename = "interval")]
    Interval { minutes: u64 },          // recurring every N minutes
    #[serde(rename = "cron")]
    Cron { expr: String },              // "0 9 * * *"
}
```

### Schedule Parsing

User input string → JobSchedule:
- `"30m"` or `"30min"` → Interval { minutes: 30 }
- `"2h"` → Interval { minutes: 120 }
- `"1d"` → Interval { minutes: 1440 }
- `"0 9 * * *"` (contains spaces + looks like cron) → Cron { expr }
- ISO 8601 string → Once { run_at }

### Next Run Computation

```rust
fn compute_next_run(schedule: &JobSchedule, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match schedule {
        JobSchedule::Once { run_at } => {
            let dt = parse_datetime(run_at)?;
            if dt > after { Some(dt) } else { None } // expired
        }
        JobSchedule::Interval { minutes } => {
            Some(after + Duration::minutes(*minutes as i64))
        }
        JobSchedule::Cron { expr } => {
            // Use `cron` crate to find next occurrence after `after`
            let schedule = cron::Schedule::from_str(expr).ok()?;
            schedule.after(&after).next()
        }
    }
}
```

---

## 3. JobStore

```rust
// hermes-cron/src/store.rs
pub struct JobStore {
    path: PathBuf,  // ~/.hermes/cron/jobs.json
}

#[derive(Serialize, Deserialize)]
struct JobsFile {
    jobs: Vec<CronJob>,
    updated_at: String,
}
```

Methods:
- `open(path) -> Result<Self>` — create dir, load or create empty file
- `list() -> Result<Vec<CronJob>>` — read and parse
- `get(id) -> Result<Option<CronJob>>`
- `create(job) -> Result<()>` — append, compute next_run_at, write
- `update(job) -> Result<()>` — replace by id, write
- `remove(id) -> Result<()>` — filter out, write

**Atomic write**: write to `jobs.json.tmp`, then `std::fs::rename` to `jobs.json`.

---

## 4. CronScheduler

```rust
// hermes-cron/src/scheduler.rs
pub struct CronScheduler {
    store: JobStore,
    output_dir: PathBuf,     // ~/.hermes/cron/output/
    app_config: AppConfig,
}
```

### Tick Logic

```rust
pub async fn tick(&self) -> Result<Vec<JobRunResult>> {
    let jobs = self.store.list()?;
    let now = Utc::now();
    let mut results = Vec::new();

    for job in &jobs {
        if !job.enabled { continue; }
        let next = job.next_run_at.as_ref().and_then(|s| parse_datetime(s));
        if next.map_or(true, |dt| dt > now) { continue; } // not due

        let result = self.run_job(job).await;
        self.save_output(job, &result)?;
        self.mark_completed(job, &result)?;
        results.push(result);
    }

    Ok(results)
}
```

### Job Execution

Each job spawns a **fresh Agent** with:
- No conversation history
- No skills, no clarify, no delegation (disabled)
- Auto-allow approval
- System prompt: `"You are running a scheduled task. Complete the task described in the prompt. If there is nothing to report, respond with [SILENT]."`
- max_iterations: 50

```rust
async fn run_job(&self, job: &CronJob) -> JobRunResult {
    let agent = build_cron_agent(&self.app_config);
    let (delta_tx, _) = mpsc::channel(64);
    let mut history = Vec::new();

    let start = Instant::now();
    let result = agent.run_conversation(&job.prompt, &mut history, delta_tx).await;
    let duration = start.elapsed();

    JobRunResult {
        job_id: job.id.clone(),
        status: if result.is_ok() { "success" } else { "failed" },
        response: result.unwrap_or_else(|e| e.to_string()),
        duration,
    }
}
```

### Output Storage

Save to `~/.hermes/cron/output/{job_id}/{timestamp}.md`:
```markdown
# Cron Job: {name}
**ID:** {id}
**Schedule:** {schedule}
**Ran at:** {timestamp}
**Duration:** {duration}
**Status:** {status}

## Prompt
{prompt}

## Response
{response}
```

### Delivery

After execution, if `deliver != "local"`:
- Parse delivery target (e.g., "telegram:12345")
- If gateway adapters are available (shared via Arc), call `adapter.send_response()`
- If no adapter available, log warning and save to local only

For Phase 6: delivery is best-effort. Gateway adapters are optional (cron can run standalone without gateway).

### File Lock

Prevent multiple processes from ticking simultaneously:
```rust
fn try_lock(lock_path: &Path) -> Option<FileLock> {
    // Create/open lock file
    // Try exclusive flock (non-blocking)
    // Return None if already locked
}
```

Use `fs2` crate for cross-platform file locking, or `libc::flock` on Unix.

---

## 5. Cron Tool

```rust
// hermes-cron/src/tool.rs (or hermes-tools/src/cron_tool.rs)
```

Since CronTool needs JobStore (which is in hermes-cron), and hermes-tools can't depend on hermes-cron (would create circular dep with hermes-agent), the tool lives in **hermes-cron** and is registered manually in CLI tooling.rs (same pattern as DelegationTool).

### Schema

```json
{
  "name": "cron",
  "description": "Manage scheduled tasks. Create recurring or one-time jobs.",
  "parameters": {
    "type": "object",
    "properties": {
      "action": {
        "type": "string",
        "enum": ["create", "list", "remove", "pause", "resume", "trigger"]
      },
      "prompt": { "type": "string", "description": "Task prompt (for create)" },
      "schedule": { "type": "string", "description": "Schedule: '30m', '2h', '0 9 * * *', or ISO timestamp" },
      "name": { "type": "string", "description": "Job name (for create, optional)" },
      "deliver": { "type": "string", "description": "Delivery target (default: 'local')" },
      "job_id": { "type": "string", "description": "Job ID (for remove/pause/resume/trigger)" }
    },
    "required": ["action"]
  }
}
```

### Implementation

- `create`: parse schedule, compute next_run_at, store.create(), return job_id
- `list`: store.list(), format as JSON table
- `remove`: store.remove(job_id)
- `pause`: set enabled=false, store.update()
- `resume`: set enabled=true, recompute next_run_at, store.update()
- `trigger`: set next_run_at=now, store.update() (next tick will pick it up)

---

## 6. CLI Integration

### Upgrade /cron command

Replace stub with real implementation:
- `/cron` or `/cron list` → list all jobs
- `/cron create "prompt" schedule` → create job (interactive)

### Add cron tick subcommand

```rust
// main.rs
enum Commands {
    Gateway,
    Cron { #[command(subcommand)] action: CronAction },
}

enum CronAction {
    /// Run one scheduler tick (execute due jobs)
    Tick,
    /// List all jobs
    List,
}
```

`hermes cron tick` — run one tick manually (useful for testing/debugging).
`hermes cron list` — list jobs from CLI.

### Gateway Integration

In `GatewayRunner::run()`, spawn the scheduler tick loop alongside adapters:
```rust
let scheduler = CronScheduler::new(store, output_dir, app_config);
let sched_handle = tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        if let Err(e) = scheduler.tick().await {
            tracing::warn!("cron tick error: {e}");
        }
    }
});
```

---

## 7. File Structure

### New files
```
crates/hermes-cron/src/job.rs           # CronJob, JobSchedule, schedule parsing
crates/hermes-cron/src/store.rs         # JobStore (JSON CRUD)
crates/hermes-cron/src/scheduler.rs     # CronScheduler (tick, run_job, output, delivery)
crates/hermes-cron/src/tool.rs          # CronTool (agent-callable)
```

### Modified files
```
crates/hermes-cron/src/lib.rs           # Wire modules
crates/hermes-cron/Cargo.toml           # Add deps (chrono, cron, fs2)
crates/hermes-cli/src/main.rs           # Add cron subcommand
crates/hermes-cli/src/handlers.rs       # Upgrade /cron handler
crates/hermes-cli/src/tooling.rs        # Register CronTool
crates/hermes-gateway/src/runner.rs     # Spawn scheduler tick loop
Cargo.toml                              # Add fs2 to workspace deps
```

---

## 8. Testing Strategy

| Component | Tests |
|-----------|-------|
| JobSchedule parsing | "30m"→Interval, "2h"→Interval, cron expr, ISO timestamp, invalid |
| next_run computation | Once (future/past), Interval (advance), Cron (next occurrence) |
| JobStore CRUD | create/get/list/update/remove, atomic write, empty file |
| CronTool | create action, list action, remove action, pause/resume |
| Scheduler tick | due job fires, non-due skipped, disabled skipped |
| Output storage | file created with correct format |

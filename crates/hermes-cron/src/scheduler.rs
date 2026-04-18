//! CronScheduler: ticks every call, finds due jobs, spawns fresh Agent per job, saves output.

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hermes_config::config::AppConfig;

use crate::job::CronJob;
use crate::store::JobStore;

/// Tools that must never be available to cron-spawned agents.
/// Prevents recursive cron creation, subagent spawning, and interactive prompts.
/// TODO: add configurable cron tool allowlist for production deployments.
const CRON_BLOCKED_TOOLS: &[&str] = &[
    "cron",          // prevent recursive cron creation
    "delegate_task", // no subagent spawning from cron
    "clarify",       // no user interaction
];

// ─── Types ────────────────────────────────────────────────────────────────────

pub struct CronScheduler {
    store: JobStore,
    output_dir: PathBuf,
    app_config: AppConfig,
}

#[derive(Debug)]
pub struct JobRunResult {
    pub job_id: String,
    pub job_name: String,
    pub status: String,
    pub response: String,
    pub duration: Duration,
    /// Wall-clock time at which the job started running (RFC 3339).
    pub started_at: String,
}

// ─── impl CronScheduler ───────────────────────────────────────────────────────

impl CronScheduler {
    pub fn new(store: JobStore, output_dir: PathBuf, app_config: AppConfig) -> Self {
        Self {
            store,
            output_dir,
            app_config,
        }
    }

    /// Tick: find all due jobs, execute them, save output, update store.
    ///
    /// A per-process file lock prevents two concurrent ticks from running at the
    /// same time (e.g. if a tick takes longer than 60 s and the next fires before
    /// it finishes).
    pub async fn tick(&self) -> anyhow::Result<Vec<JobRunResult>> {
        // ── C1: exclusive file lock — one tick at a time ──────────────────────
        let lock_path = self
            .store
            .path()
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join(".tick.lock");
        let lock_file = File::create(&lock_path)?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = lock_file.as_raw_fd();
            // SAFETY: fd is valid for the duration of this function; flock is safe
            // to call on a valid file descriptor.
            let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if ret != 0 {
                tracing::debug!("cron tick skipped — another tick is already running");
                return Ok(vec![]);
            }
        }
        #[cfg(not(unix))]
        {
            // On non-Unix platforms the file is created but not locked; just keep
            // it alive until end of tick so callers can see the file is in use.
            let _ = &lock_file;
            tracing::warn!(
                "file locking not available on this platform — concurrent ticks possible"
            );
        }

        let jobs = self.store.list()?;
        let now = chrono::Utc::now();
        let mut results = Vec::new();

        for job in &jobs {
            if !job.enabled {
                continue;
            }

            // Check if due
            let next = job
                .next_run_at
                .as_ref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc));

            match next {
                // dt > now: still in the future — not due yet.
                // Fires on the tick where now >= dt (i.e. the condition is false).
                Some(dt) if dt > now => continue,
                None => continue, // no next_run scheduled
                _ => {}           // due!
            }

            tracing::info!(job_id = %job.id, name = %job.name, "executing cron job");

            let result = self.run_job(job).await;
            self.save_output(job, &result)?;
            self.mark_completed(job, &result)?;
            results.push(result);
        }

        Ok(results)
    }

    /// Build and run a fresh isolated Agent for the given job.
    async fn run_job(&self, job: &CronJob) -> JobRunResult {
        let start = std::time::Instant::now();
        let started_at = chrono::Utc::now().to_rfc3339();

        // Build provider from config
        let api_key = match self.app_config.api_key() {
            Some(key) => key,
            None => {
                return JobRunResult {
                    job_id: job.id.clone(),
                    job_name: job.name.clone(),
                    status: "failed".into(),
                    response: "No API key configured".into(),
                    duration: start.elapsed(),
                    started_at,
                };
            }
        };

        let provider = match hermes_provider::create_provider(
            &self.app_config.model,
            secrecy::SecretString::new(api_key.into()),
            None,
        ) {
            Ok(p) => p,
            Err(e) => {
                return JobRunResult {
                    job_id: job.id.clone(),
                    job_name: job.name.clone(),
                    status: "failed".into(),
                    response: format!("Provider error: {e}"),
                    duration: start.elapsed(),
                    started_at,
                };
            }
        };

        // ── C3: restrict registry — block tools that must not run in cron context ──
        let registry = hermes_tools::ToolRegistry::from_inventory();
        for blocked in CRON_BLOCKED_TOOLS {
            registry.remove(blocked);
        }
        let registry = Arc::new(registry);

        // ── H2: no unwrap on memory fallback ─────────────────────────────────
        let memory_dir = hermes_config::config::hermes_home().join("cron-memory");
        let memory = match hermes_memory::MemoryManager::new(memory_dir, None) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("primary cron memory unavailable ({e}), falling back to temp dir");
                match hermes_memory::MemoryManager::new(
                    std::env::temp_dir().join("hermes-cron-mem"),
                    None,
                ) {
                    Ok(m) => m,
                    Err(e) => {
                        return JobRunResult {
                            job_id: job.id.clone(),
                            job_name: job.name.clone(),
                            status: "failed".into(),
                            response: format!("Memory init failed: {e}"),
                            duration: start.elapsed(),
                            started_at,
                        };
                    }
                }
            }
        };

        let (approval_tx, mut approval_rx) =
            tokio::sync::mpsc::channel::<hermes_core::tool::ApprovalRequest>(8);
        tokio::spawn(async move {
            while let Some(req) = approval_rx.recv().await {
                let _ = req
                    .response_tx
                    .send(hermes_core::tool::ApprovalDecision::Allow);
            }
        });

        let tool_config = Arc::new(
            self.app_config
                .tool_config(std::env::current_dir().unwrap_or_default()),
        );

        let mut agent = hermes_agent::Agent::new(hermes_agent::AgentConfig {
            provider,
            registry,
            max_iterations: 50,
            system_prompt:
                "You are running a scheduled task. Complete the task described below. Be concise."
                    .into(),
            session_id: format!("cron-{}-{}", job.id, chrono::Utc::now().timestamp()),
            working_dir: std::env::current_dir().unwrap_or_default(),
            approval_tx,
            tool_config,
            memory,
            skills: None,
            compression: hermes_agent::compressor::CompressionConfig::default(),
            delegation_depth: 0,
            clarify_tx: None,
        });

        let (delta_tx, _) = tokio::sync::mpsc::channel(64);
        let mut history = Vec::new();

        let result = agent
            .run_conversation(&job.prompt, &mut history, delta_tx)
            .await;
        let duration = start.elapsed();

        match result {
            Ok(response) => JobRunResult {
                job_id: job.id.clone(),
                job_name: job.name.clone(),
                status: "success".into(),
                response,
                duration,
                started_at,
            },
            Err(e) => JobRunResult {
                job_id: job.id.clone(),
                job_name: job.name.clone(),
                status: "failed".into(),
                response: e.to_string(),
                duration,
                started_at,
            },
        }
    }

    /// Write a markdown file with job run output.
    fn save_output(&self, job: &CronJob, result: &JobRunResult) -> anyhow::Result<()> {
        let dir = self.output_dir.join(&job.id);
        std::fs::create_dir_all(&dir)?;

        // Include milliseconds in the filename to prevent collision when two jobs
        // complete within the same second.
        let timestamp = chrono::DateTime::parse_from_rfc3339(&result.started_at)
            .map(|dt| dt.format("%Y%m%d_%H%M%S%.3f").to_string())
            .unwrap_or_else(|_| chrono::Utc::now().format("%Y%m%d_%H%M%S%.3f").to_string());
        let path = dir.join(format!("{timestamp}.md"));

        let content = format!(
            "# Cron Job: {name}\n\
             **ID:** {id}\n\
             **Ran at:** {ran_at}\n\
             **Duration:** {duration:.1}s\n\
             **Status:** {status}\n\n\
             ## Prompt\n{prompt}\n\n\
             ## Response\n{response}\n",
            name = job.name,
            id = job.id,
            ran_at = result.started_at,
            duration = result.duration.as_secs_f64(),
            status = result.status,
            prompt = job.prompt,
            response = result.response,
        );

        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Update job metadata after a run and compute next scheduled time.
    fn mark_completed(&self, job: &CronJob, result: &JobRunResult) -> anyhow::Result<()> {
        let mut updated = job.clone();
        updated.last_run_at = Some(chrono::Utc::now().to_rfc3339());
        updated.last_status = Some(result.status.clone());
        updated.last_error = if result.status == "failed" {
            Some(result.response.clone())
        } else {
            None
        };

        // Compute next run.
        // For Interval schedules use the original scheduled time as the base to
        // prevent drift: if a job was supposed to run at T but finished at T+2min,
        // the next interval still starts from T, not T+2min.
        let now = chrono::Utc::now();
        let base_time = match &job.schedule {
            crate::job::JobSchedule::Interval { .. } => job
                .next_run_at
                .as_ref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .unwrap_or(now),
            _ => now,
        };
        updated.next_run_at =
            crate::job::compute_next_run(&job.schedule, &base_time).map(|dt| dt.to_rfc3339());

        // Once jobs: disable after completion
        if matches!(job.schedule, crate::job::JobSchedule::Once { .. }) {
            updated.enabled = false;
        }

        let found = self.store.update(updated)?;
        if !found {
            tracing::warn!(job_id = %job.id, "mark_completed: job not found in store");
        }
        Ok(())
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{CronJob, JobSchedule};
    use crate::store::JobStore;
    use tempfile::TempDir;

    fn make_store(dir: &TempDir) -> JobStore {
        JobStore::open(dir.path().join("jobs.json")).unwrap()
    }

    fn make_scheduler(store: JobStore, dir: &TempDir) -> CronScheduler {
        CronScheduler::new(store, dir.path().join("output"), AppConfig::default())
    }

    fn past_timestamp() -> String {
        // 1 hour in the past — always due
        (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339()
    }

    fn future_timestamp() -> String {
        // 1 hour in the future — not due yet
        (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()
    }

    fn make_job_with_next(name: &str, next_run_at: Option<String>, enabled: bool) -> CronJob {
        let mut job = CronJob::new(
            name.to_string(),
            "test prompt".to_string(),
            JobSchedule::Interval { minutes: 60 },
            "stdout".to_string(),
        );
        job.next_run_at = next_run_at;
        job.enabled = enabled;
        job
    }

    // ── test_tick_skips_disabled ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_tick_skips_disabled() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        // Job is disabled and overdue — must not be executed.
        let job = make_job_with_next("disabled-job", Some(past_timestamp()), false);
        store.create(job.clone()).unwrap();

        let scheduler = make_scheduler(make_store(&dir), &dir);
        let results = scheduler.tick().await.unwrap();

        assert!(results.is_empty(), "disabled job should be skipped");
    }

    // ── test_tick_skips_not_due ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_tick_skips_not_due() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        // Job is enabled but next_run_at is in the future.
        let job = make_job_with_next("future-job", Some(future_timestamp()), true);
        store.create(job.clone()).unwrap();

        let scheduler = make_scheduler(make_store(&dir), &dir);
        let results = scheduler.tick().await.unwrap();

        assert!(results.is_empty(), "future job should be skipped");
    }

    // ── test_save_output_creates_file ─────────────────────────────────────────

    #[test]
    fn test_save_output_creates_file() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let scheduler = make_scheduler(store, &dir);

        let job = make_job_with_next("output-job", None, true);
        let result = JobRunResult {
            job_id: job.id.clone(),
            job_name: job.name.clone(),
            status: "success".into(),
            response: "all good".into(),
            duration: Duration::from_millis(42),
            started_at: chrono::Utc::now().to_rfc3339(),
        };

        scheduler.save_output(&job, &result).unwrap();

        let job_output_dir = dir.path().join("output").join(&job.id);
        assert!(
            job_output_dir.exists(),
            "output directory should be created"
        );

        let entries: Vec<_> = std::fs::read_dir(&job_output_dir).unwrap().collect();
        assert_eq!(entries.len(), 1, "exactly one output file expected");

        let file_path = entries[0].as_ref().unwrap().path();
        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("# Cron Job: output-job"));
        assert!(content.contains("**Status:** success"));
        assert!(content.contains("all good"));
    }

    // ── test_mark_completed_updates_store ─────────────────────────────────────

    #[test]
    fn test_mark_completed_updates_store() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let job = make_job_with_next("mark-job", Some(past_timestamp()), true);
        store.create(job.clone()).unwrap();

        let scheduler = make_scheduler(make_store(&dir), &dir);
        let result = JobRunResult {
            job_id: job.id.clone(),
            job_name: job.name.clone(),
            status: "success".into(),
            response: "done".into(),
            duration: Duration::from_secs(1),
            started_at: chrono::Utc::now().to_rfc3339(),
        };

        scheduler.mark_completed(&job, &result).unwrap();

        // Re-open store to verify persistence
        let store2 = make_store(&dir);
        let updated = store2.get(&job.id).unwrap().unwrap();
        assert!(
            updated.last_run_at.is_some(),
            "last_run_at should be set after mark_completed"
        );
        assert_eq!(updated.last_status.as_deref(), Some("success"));
        assert!(updated.last_error.is_none());
    }

    // ── test_mark_completed_once_disables ─────────────────────────────────────

    #[test]
    fn test_mark_completed_once_disables() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);

        let run_at = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let mut job = CronJob::new(
            "once-job".to_string(),
            "do it once".to_string(),
            JobSchedule::Once {
                run_at: run_at.clone(),
            },
            "stdout".to_string(),
        );
        // Force next_run_at to a past time so tick would consider it due.
        job.next_run_at = Some(run_at);
        store.create(job.clone()).unwrap();

        let scheduler = make_scheduler(make_store(&dir), &dir);
        let result = JobRunResult {
            job_id: job.id.clone(),
            job_name: job.name.clone(),
            status: "success".into(),
            response: "once done".into(),
            duration: Duration::from_millis(100),
            started_at: chrono::Utc::now().to_rfc3339(),
        };

        scheduler.mark_completed(&job, &result).unwrap();

        let store2 = make_store(&dir);
        let updated = store2.get(&job.id).unwrap().unwrap();
        assert!(
            !updated.enabled,
            "Once job should be disabled after mark_completed"
        );
        assert!(
            updated.next_run_at.is_none(),
            "Once job should have no next_run_at after past run_at"
        );
    }
}

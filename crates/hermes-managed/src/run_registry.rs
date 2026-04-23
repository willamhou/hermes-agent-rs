use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};

use chrono::{DateTime, Utc};
use hermes_core::error::{HermesError, Result};
use tokio::task::JoinHandle;

use crate::types::{ManagedRun, ManagedRunStatus};

pub struct RunRegistry {
    runs: RwLock<HashMap<String, Arc<RunHandle>>>,
}

pub struct RunHandle {
    run_id: String,
    agent_id: String,
    agent_version: u32,
    model: String,
    started_at: DateTime<Utc>,
    timeout_secs: u32,
    state: Mutex<RunState>,
}

struct RunState {
    status: ManagedRunStatus,
    updated_at: DateTime<Utc>,
    ended_at: Option<DateTime<Utc>>,
    cancel_requested_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
    task: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStatusSnapshot {
    pub run_id: String,
    pub agent_id: String,
    pub agent_version: u32,
    pub model: String,
    pub status: ManagedRunStatus,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub cancel_requested_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub timeout_secs: u32,
}

impl RunRegistry {
    pub fn new() -> Self {
        Self {
            runs: RwLock::new(HashMap::new()),
        }
    }

    pub fn register(
        &self,
        run: &ManagedRun,
        timeout_secs: u32,
        task: JoinHandle<()>,
    ) -> Result<Arc<RunHandle>> {
        let handle = Arc::new(RunHandle::new(run, timeout_secs, task));
        let mut guard = self.runs.write().expect("run registry write lock poisoned");
        if guard.contains_key(&run.id) {
            return Err(HermesError::Config(format!(
                "managed run already registered: {}",
                run.id
            )));
        }
        guard.insert(run.id.clone(), Arc::clone(&handle));
        Ok(handle)
    }

    pub fn get(&self, run_id: &str) -> Option<Arc<RunHandle>> {
        self.runs
            .read()
            .expect("run registry read lock poisoned")
            .get(run_id)
            .cloned()
    }

    pub fn snapshot(&self, run_id: &str) -> Option<RunStatusSnapshot> {
        self.get(run_id).map(|handle| handle.snapshot())
    }

    pub fn list(&self) -> Vec<RunStatusSnapshot> {
        self.runs
            .read()
            .expect("run registry read lock poisoned")
            .values()
            .map(|handle| handle.snapshot())
            .collect()
    }

    pub fn cancel_run(&self, run_id: &str) -> Result<RunStatusSnapshot> {
        let handle = self
            .get(run_id)
            .ok_or_else(|| HermesError::Config(format!("managed run not found: {run_id}")))?;
        Ok(handle.cancel())
    }

    pub fn terminate_run(
        &self,
        run_id: &str,
        status: ManagedRunStatus,
        last_error: Option<String>,
    ) -> Result<RunStatusSnapshot> {
        let handle = self
            .get(run_id)
            .ok_or_else(|| HermesError::Config(format!("managed run not found: {run_id}")))?;
        Ok(handle.terminate(status, last_error))
    }

    pub fn update_status(
        &self,
        run_id: &str,
        status: ManagedRunStatus,
        last_error: Option<String>,
    ) -> Result<RunStatusSnapshot> {
        let handle = self
            .get(run_id)
            .ok_or_else(|| HermesError::Config(format!("managed run not found: {run_id}")))?;
        Ok(handle.update_status(status, last_error))
    }

    pub fn remove(&self, run_id: &str) -> Option<RunStatusSnapshot> {
        self.runs
            .write()
            .expect("run registry write lock poisoned")
            .remove(run_id)
            .map(|handle| handle.snapshot())
    }

    pub fn len(&self) -> usize {
        self.runs
            .read()
            .expect("run registry read lock poisoned")
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for RunRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RunHandle {
    fn new(run: &ManagedRun, timeout_secs: u32, task: JoinHandle<()>) -> Self {
        Self {
            run_id: run.id.clone(),
            agent_id: run.agent_id.clone(),
            agent_version: run.agent_version,
            model: run.model.clone(),
            started_at: run.started_at,
            timeout_secs,
            state: Mutex::new(RunState {
                status: run.status.clone(),
                updated_at: run.updated_at,
                ended_at: run.ended_at,
                cancel_requested_at: run.cancel_requested_at,
                last_error: run.last_error.clone(),
                task: Some(task),
            }),
        }
    }

    pub fn snapshot(&self) -> RunStatusSnapshot {
        let state = self.state.lock().expect("run handle lock poisoned");
        RunStatusSnapshot {
            run_id: self.run_id.clone(),
            agent_id: self.agent_id.clone(),
            agent_version: self.agent_version,
            model: self.model.clone(),
            status: state.status.clone(),
            started_at: self.started_at,
            updated_at: state.updated_at,
            ended_at: state.ended_at,
            cancel_requested_at: state.cancel_requested_at,
            last_error: state.last_error.clone(),
            timeout_secs: self.timeout_secs,
        }
    }

    pub fn cancel(&self) -> RunStatusSnapshot {
        self.terminate(ManagedRunStatus::Cancelled, None)
    }

    pub fn terminate(
        &self,
        status: ManagedRunStatus,
        last_error: Option<String>,
    ) -> RunStatusSnapshot {
        let now = Utc::now();
        let mark_cancel_requested = status == ManagedRunStatus::Cancelled;
        let mut state = self.state.lock().expect("run handle lock poisoned");
        if let Some(task) = state.task.as_ref() {
            task.abort();
        }
        if !state.status.is_terminal() {
            state.status = status;
            state.updated_at = now;
            state.ended_at = Some(now);
            state.last_error = last_error;
        }
        if mark_cancel_requested && state.cancel_requested_at.is_none() {
            state.cancel_requested_at = Some(now);
        }
        drop(state);
        self.snapshot()
    }

    pub fn update_status(
        &self,
        status: ManagedRunStatus,
        last_error: Option<String>,
    ) -> RunStatusSnapshot {
        let now = Utc::now();
        let mut state = self.state.lock().expect("run handle lock poisoned");
        if state.status.is_terminal() {
            drop(state);
            return self.snapshot();
        }
        state.status = status.clone();
        state.updated_at = now;
        state.last_error = last_error;
        if status.is_terminal() {
            state.ended_at = Some(now);
        }
        drop(state);
        self.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn register_and_snapshot_run() {
        let registry = RunRegistry::new();
        let mut run = ManagedRun::new("agent_123", 3, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;

        let handle = tokio::spawn(async {});
        registry.register(&run, 300, handle).unwrap();

        let snapshot = registry.snapshot(&run.id).unwrap();
        assert_eq!(snapshot.run_id, run.id);
        assert_eq!(snapshot.agent_id, "agent_123");
        assert_eq!(snapshot.agent_version, 3);
        assert_eq!(snapshot.status, ManagedRunStatus::Running);
        assert_eq!(snapshot.timeout_secs, 300);
    }

    #[tokio::test]
    async fn cancel_run_aborts_task_and_marks_cancelled() {
        let registry = RunRegistry::new();
        let mut run = ManagedRun::new("agent_123", 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;

        let task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let handle = registry.register(&run, 120, task).unwrap();

        let snapshot = registry.cancel_run(&run.id).unwrap();
        assert_eq!(snapshot.status, ManagedRunStatus::Cancelled);
        assert!(snapshot.cancel_requested_at.is_some());
        assert!(snapshot.ended_at.is_some());

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if handle
                    .state
                    .lock()
                    .expect("run handle lock poisoned")
                    .task
                    .as_ref()
                    .is_some_and(|task| task.is_finished())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn update_status_sets_terminal_metadata() {
        let registry = RunRegistry::new();
        let mut run = ManagedRun::new("agent_123", 2, "anthropic/claude-sonnet");
        run.status = ManagedRunStatus::Running;

        let handle = tokio::spawn(async {});
        registry.register(&run, 90, handle).unwrap();

        let snapshot = registry
            .update_status(
                &run.id,
                ManagedRunStatus::Failed,
                Some("provider failed".to_string()),
            )
            .unwrap();
        assert_eq!(snapshot.status, ManagedRunStatus::Failed);
        assert_eq!(snapshot.last_error.as_deref(), Some("provider failed"));
        assert!(snapshot.ended_at.is_some());
    }

    #[tokio::test]
    async fn terminate_run_marks_timeout_and_preserves_error() {
        let registry = RunRegistry::new();
        let mut run = ManagedRun::new("agent_123", 2, "anthropic/claude-sonnet");
        run.status = ManagedRunStatus::Running;

        registry.register(&run, 90, tokio::spawn(async {})).unwrap();
        let snapshot = registry
            .terminate_run(
                &run.id,
                ManagedRunStatus::TimedOut,
                Some("timed out".to_string()),
            )
            .unwrap();

        assert_eq!(snapshot.status, ManagedRunStatus::TimedOut);
        assert_eq!(snapshot.last_error.as_deref(), Some("timed out"));
        assert!(snapshot.cancel_requested_at.is_none());
    }

    #[tokio::test]
    async fn duplicate_run_registration_is_rejected() {
        let registry = RunRegistry::new();
        let run = ManagedRun::new("agent_123", 1, "openai/gpt-4o-mini");

        registry.register(&run, 30, tokio::spawn(async {})).unwrap();
        let err = registry
            .register(&run, 30, tokio::spawn(async {}))
            .err()
            .unwrap();

        assert!(err.to_string().contains("already registered"));
    }

    #[tokio::test]
    async fn terminal_status_is_not_overridden() {
        let registry = RunRegistry::new();
        let mut run = ManagedRun::new("agent_123", 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Running;

        registry.register(&run, 30, tokio::spawn(async {})).unwrap();
        registry.cancel_run(&run.id).unwrap();
        let snapshot = registry
            .update_status(&run.id, ManagedRunStatus::Completed, None)
            .unwrap();

        assert_eq!(snapshot.status, ManagedRunStatus::Cancelled);
    }
}

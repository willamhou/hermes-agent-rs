use std::{
    collections::HashMap,
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::process_registry::global_registry;

static REGISTRY: LazyLock<SessionCleanupRegistry> = LazyLock::new(SessionCleanupRegistry::new);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCleanupRegistration {
    session_id: String,
    entry_id: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCleanupSummary {
    pub session_id: String,
    pub attempted: usize,
    pub cleaned: usize,
    pub failures: Vec<String>,
}

#[derive(Debug)]
enum CleanupTarget {
    Pid { pid: u32, label: String },
    BackgroundProcess { process_id: String, label: String },
}

struct SessionCleanupRegistry {
    sessions: Mutex<HashMap<String, HashMap<u64, CleanupTarget>>>,
}

impl SessionCleanupRegistry {
    fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
        }
    }
}

pub fn register_pid(
    session_id: &str,
    pid: u32,
    label: impl Into<String>,
) -> Option<SessionCleanupRegistration> {
    if pid == 0 {
        return None;
    }

    let entry_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut guard = REGISTRY
        .sessions
        .lock()
        .expect("session cleanup registry lock poisoned");
    guard.entry(session_id.to_string()).or_default().insert(
        entry_id,
        CleanupTarget::Pid {
            pid,
            label: label.into(),
        },
    );
    Some(SessionCleanupRegistration {
        session_id: session_id.to_string(),
        entry_id,
    })
}

pub fn register_background_process(
    session_id: &str,
    process_id: impl Into<String>,
    label: impl Into<String>,
) -> SessionCleanupRegistration {
    let entry_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut guard = REGISTRY
        .sessions
        .lock()
        .expect("session cleanup registry lock poisoned");
    guard.entry(session_id.to_string()).or_default().insert(
        entry_id,
        CleanupTarget::BackgroundProcess {
            process_id: process_id.into(),
            label: label.into(),
        },
    );
    SessionCleanupRegistration {
        session_id: session_id.to_string(),
        entry_id,
    }
}

pub fn unregister(registration: &SessionCleanupRegistration) -> bool {
    let mut guard = REGISTRY
        .sessions
        .lock()
        .expect("session cleanup registry lock poisoned");
    let Some(entries) = guard.get_mut(&registration.session_id) else {
        return false;
    };
    let removed = entries.remove(&registration.entry_id).is_some();
    if entries.is_empty() {
        guard.remove(&registration.session_id);
    }
    removed
}

pub fn cleanup_session(session_id: &str) -> SessionCleanupSummary {
    let entries = REGISTRY
        .sessions
        .lock()
        .expect("session cleanup registry lock poisoned")
        .remove(session_id)
        .unwrap_or_default();

    let mut summary = SessionCleanupSummary {
        session_id: session_id.to_string(),
        attempted: entries.len(),
        cleaned: 0,
        failures: Vec::new(),
    };

    for target in entries.into_values() {
        match cleanup_target(&target) {
            Ok(()) => summary.cleaned += 1,
            Err(err) => summary.failures.push(err),
        }
    }

    summary
}

#[cfg(test)]
pub fn tracked_count(session_id: &str) -> usize {
    REGISTRY
        .sessions
        .lock()
        .expect("session cleanup registry lock poisoned")
        .get(session_id)
        .map(|entries| entries.len())
        .unwrap_or(0)
}

fn cleanup_target(target: &CleanupTarget) -> Result<(), String> {
    match target {
        CleanupTarget::Pid { pid, label } => kill_pid(*pid)
            .map_err(|err| format!("failed to clean pid resource '{label}' ({pid}): {err}")),
        CleanupTarget::BackgroundProcess { process_id, label } => {
            let registry = global_registry();
            if !registry.is_running(process_id) {
                return Ok(());
            }
            registry.kill(process_id).map_err(|err| {
                format!("failed to clean background process '{label}' ({process_id}): {err}")
            })
        }
    }
}

fn kill_pid(pid: u32) -> Result<(), String> {
    let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    if ret == 0 {
        return Ok(());
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn unique_session(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
    }

    #[test]
    fn unregister_removes_session_cleanup_entry() {
        let session_id = unique_session("cleanup-unregister");
        let registration = register_background_process(&session_id, "bg_123", "test background");
        assert_eq!(tracked_count(&session_id), 1);
        assert!(unregister(&registration));
        assert_eq!(tracked_count(&session_id), 0);
    }

    #[tokio::test]
    async fn cleanup_session_kills_background_processes() {
        let session_id = unique_session("cleanup-bg");
        let registry = global_registry();
        let process_id = registry
            .spawn("sleep 30", std::path::Path::new("/tmp"))
            .unwrap();
        register_background_process(&session_id, process_id.clone(), "sleep");

        let summary = cleanup_session(&session_id);
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 1);
        assert!(summary.failures.is_empty());

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!registry.is_running(&process_id));
    }

    #[tokio::test]
    async fn cleanup_session_kills_registered_pids() {
        let session_id = unique_session("cleanup-pid");
        let mut child = tokio::process::Command::new("bash")
            .args(["-lc", "sleep 30"])
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let _registration = register_pid(&session_id, pid, "foreground sleep").unwrap();

        let summary = cleanup_session(&session_id);
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 1);
        assert!(summary.failures.is_empty());

        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("child should exit after cleanup")
            .expect("child wait should succeed");
        assert!(!status.success());
    }
}

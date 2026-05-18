use std::{
    collections::HashMap,
    future::Future,
    path::Path,
    pin::Pin,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::process_registry::{global_registry, kill_process, kill_process_group};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

static REGISTRY: LazyLock<SessionCleanupRegistry> = LazyLock::new(SessionCleanupRegistry::new);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static DURABLE_RECORDER: LazyLock<Mutex<Option<Arc<dyn DurableCleanupRecorder>>>> =
    LazyLock::new(|| Mutex::new(None));
static DURABLE_EXECUTOR: LazyLock<Mutex<Option<Arc<dyn DurableCleanupExecutor>>>> =
    LazyLock::new(|| Mutex::new(None));
#[cfg(test)]
pub(crate) static DURABLE_RECORDER_TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));

type CleanupFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>;
type AsyncCleanupAction = Box<dyn Fn() -> CleanupFuture + Send + Sync + 'static>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurableCleanupResourceKind {
    Pid,
    ProcessGroup,
    BrowserSession,
    McpHttpResourceSubscription,
    McpHttpSession,
}

impl DurableCleanupResourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pid => "pid",
            Self::ProcessGroup => "process_group",
            Self::BrowserSession => "browser_session",
            Self::McpHttpResourceSubscription => "mcp_http_resource_subscription",
            Self::McpHttpSession => "mcp_http_session",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pid" => Some(Self::Pid),
            "process_group" => Some(Self::ProcessGroup),
            "browser_session" => Some(Self::BrowserSession),
            "mcp_http_resource_subscription" => Some(Self::McpHttpResourceSubscription),
            "mcp_http_session" => Some(Self::McpHttpSession),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurableCleanupResource {
    pub kind: DurableCleanupResourceKind,
    pub label: String,
    pub target_value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DurableBrowserSessionTarget {
    #[serde(default)]
    root_pid: Option<u32>,
    #[serde(default)]
    process_group: Option<u32>,
    user_data_dir: String,
}

fn durable_browser_session_target(
    root_pid: Option<u32>,
    process_group: Option<u32>,
    user_data_dir: impl AsRef<Path>,
) -> Result<DurableCleanupResource, String> {
    let target = DurableBrowserSessionTarget {
        root_pid,
        process_group,
        user_data_dir: user_data_dir.as_ref().display().to_string(),
    };
    Ok(DurableCleanupResource {
        kind: DurableCleanupResourceKind::BrowserSession,
        label: "browser session state".to_string(),
        target_value: serde_json::to_string(&target)
            .map_err(|err| format!("failed to serialize browser cleanup target: {err}"))?,
    })
}

#[async_trait]
pub trait DurableCleanupRecorder: Send + Sync {
    async fn register(
        &self,
        session_id: &str,
        entry_id: u64,
        resource: DurableCleanupResource,
    ) -> Result<(), String>;

    async fn unregister(&self, session_id: &str, entry_id: u64) -> Result<(), String>;
}

#[async_trait]
pub trait DurableCleanupExecutor: Send + Sync {
    async fn cleanup(&self, resource: &DurableCleanupResource) -> Result<bool, String>;
}

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

enum CleanupTarget {
    Pid {
        pid: u32,
        label: String,
    },
    ProcessGroup {
        process_group: u32,
        label: String,
    },
    BackgroundProcess {
        process_id: String,
        label: String,
    },
    AsyncAction {
        label: String,
        cleanup: AsyncCleanupAction,
        durable_resource: Option<DurableCleanupResource>,
    },
}

impl std::fmt::Debug for CleanupTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CleanupTarget::Pid { pid, label } => f
                .debug_struct("Pid")
                .field("pid", pid)
                .field("label", label)
                .finish(),
            CleanupTarget::ProcessGroup {
                process_group,
                label,
            } => f
                .debug_struct("ProcessGroup")
                .field("process_group", process_group)
                .field("label", label)
                .finish(),
            CleanupTarget::BackgroundProcess { process_id, label } => f
                .debug_struct("BackgroundProcess")
                .field("process_id", process_id)
                .field("label", label)
                .finish(),
            CleanupTarget::AsyncAction { label, .. } => {
                f.debug_struct("AsyncAction").field("label", label).finish()
            }
        }
    }
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

pub fn replace_durable_cleanup_recorder(
    recorder: Option<Arc<dyn DurableCleanupRecorder>>,
) -> Option<Arc<dyn DurableCleanupRecorder>> {
    std::mem::replace(
        &mut *DURABLE_RECORDER
            .lock()
            .expect("durable cleanup recorder lock poisoned"),
        recorder,
    )
}

pub fn replace_durable_cleanup_executor(
    executor: Option<Arc<dyn DurableCleanupExecutor>>,
) -> Option<Arc<dyn DurableCleanupExecutor>> {
    std::mem::replace(
        &mut *DURABLE_EXECUTOR
            .lock()
            .expect("durable cleanup executor lock poisoned"),
        executor,
    )
}

fn current_durable_cleanup_recorder() -> Option<Arc<dyn DurableCleanupRecorder>> {
    DURABLE_RECORDER
        .lock()
        .expect("durable cleanup recorder lock poisoned")
        .clone()
}

fn current_durable_cleanup_executor() -> Option<Arc<dyn DurableCleanupExecutor>> {
    DURABLE_EXECUTOR
        .lock()
        .expect("durable cleanup executor lock poisoned")
        .clone()
}

fn spawn_durable_cleanup_register(
    session_id: &str,
    entry_id: u64,
    resource: DurableCleanupResource,
) {
    let Some(recorder) = current_durable_cleanup_recorder() else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let session_id = session_id.to_string();
    handle.spawn(async move {
        let still_registered = REGISTRY
            .sessions
            .lock()
            .expect("session cleanup registry lock poisoned")
            .get(&session_id)
            .is_some_and(|entries| entries.contains_key(&entry_id));
        if !still_registered {
            return;
        }
        if let Err(err) = recorder.register(&session_id, entry_id, resource).await {
            tracing::warn!(
                session_id,
                entry_id,
                "failed to persist cleanup resource manifest: {err}"
            );
        }
    });
}

fn spawn_durable_cleanup_unregister(session_id: &str, entry_id: u64) {
    let Some(recorder) = current_durable_cleanup_recorder() else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let session_id = session_id.to_string();
    handle.spawn(async move {
        if let Err(err) = recorder.unregister(&session_id, entry_id).await {
            tracing::warn!(
                session_id,
                entry_id,
                "failed to remove cleanup resource manifest: {err}"
            );
        }
    });
}

fn durable_resource_for_target(target: &CleanupTarget) -> Option<DurableCleanupResource> {
    match target {
        CleanupTarget::Pid { pid, label } => Some(DurableCleanupResource {
            kind: DurableCleanupResourceKind::Pid,
            label: label.clone(),
            target_value: pid.to_string(),
        }),
        CleanupTarget::ProcessGroup {
            process_group,
            label,
        } => Some(DurableCleanupResource {
            kind: DurableCleanupResourceKind::ProcessGroup,
            label: label.clone(),
            target_value: process_group.to_string(),
        }),
        CleanupTarget::BackgroundProcess { process_id, label } => global_registry()
            .process_group_for(process_id)
            .map(|process_group| DurableCleanupResource {
                kind: DurableCleanupResourceKind::ProcessGroup,
                label: label.clone(),
                target_value: process_group.to_string(),
            }),
        CleanupTarget::AsyncAction {
            durable_resource, ..
        } => durable_resource.clone(),
    }
}

pub fn browser_session_durable_resource(
    root_pid: Option<u32>,
    process_group: Option<u32>,
    user_data_dir: impl AsRef<Path>,
) -> Result<DurableCleanupResource, String> {
    durable_browser_session_target(root_pid, process_group, user_data_dir)
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
    if let Some(target) = guard
        .get(session_id)
        .and_then(|entries| entries.get(&entry_id))
        .and_then(durable_resource_for_target)
    {
        spawn_durable_cleanup_register(session_id, entry_id, target);
    }
    Some(SessionCleanupRegistration {
        session_id: session_id.to_string(),
        entry_id,
    })
}

pub fn register_process_group(
    session_id: &str,
    process_group: u32,
    label: impl Into<String>,
) -> Option<SessionCleanupRegistration> {
    if process_group == 0 {
        return None;
    }

    let entry_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut guard = REGISTRY
        .sessions
        .lock()
        .expect("session cleanup registry lock poisoned");
    guard.entry(session_id.to_string()).or_default().insert(
        entry_id,
        CleanupTarget::ProcessGroup {
            process_group,
            label: label.into(),
        },
    );
    if let Some(target) = guard
        .get(session_id)
        .and_then(|entries| entries.get(&entry_id))
        .and_then(durable_resource_for_target)
    {
        spawn_durable_cleanup_register(session_id, entry_id, target);
    }
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
    if let Some(target) = guard
        .get(session_id)
        .and_then(|entries| entries.get(&entry_id))
        .and_then(durable_resource_for_target)
    {
        spawn_durable_cleanup_register(session_id, entry_id, target);
    }
    SessionCleanupRegistration {
        session_id: session_id.to_string(),
        entry_id,
    }
}

pub fn register_async_cleanup<F, Fut>(
    session_id: &str,
    label: impl Into<String>,
    cleanup: F,
) -> SessionCleanupRegistration
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    register_async_cleanup_with_resource(session_id, label, None, cleanup)
}

pub fn register_async_cleanup_with_durable_resource<F, Fut>(
    session_id: &str,
    label: impl Into<String>,
    durable_resource: DurableCleanupResource,
    cleanup: F,
) -> SessionCleanupRegistration
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    register_async_cleanup_with_resource(session_id, label, Some(durable_resource), cleanup)
}

fn register_async_cleanup_with_resource<F, Fut>(
    session_id: &str,
    label: impl Into<String>,
    durable_resource: Option<DurableCleanupResource>,
    cleanup: F,
) -> SessionCleanupRegistration
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    let entry_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut guard = REGISTRY
        .sessions
        .lock()
        .expect("session cleanup registry lock poisoned");
    guard.entry(session_id.to_string()).or_default().insert(
        entry_id,
        CleanupTarget::AsyncAction {
            label: label.into(),
            cleanup: Box::new(move || Box::pin(cleanup())),
            durable_resource,
        },
    );
    if let Some(target) = guard
        .get(session_id)
        .and_then(|entries| entries.get(&entry_id))
        .and_then(durable_resource_for_target)
    {
        spawn_durable_cleanup_register(session_id, entry_id, target);
    }
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
    if removed {
        spawn_durable_cleanup_unregister(&registration.session_id, registration.entry_id);
    }
    removed
}

pub async fn cleanup_session(session_id: &str) -> SessionCleanupSummary {
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

    for (entry_id, target) in entries {
        match cleanup_target(target).await {
            Ok(()) => {
                summary.cleaned += 1;
                spawn_durable_cleanup_unregister(session_id, entry_id);
            }
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

async fn cleanup_target(target: CleanupTarget) -> Result<(), String> {
    match target {
        CleanupTarget::Pid { pid, label } => kill_process(pid)
            .map_err(|err| format!("failed to clean pid resource '{label}' ({pid}): {err}")),
        CleanupTarget::ProcessGroup {
            process_group,
            label,
        } => kill_process_group(process_group).map_err(|err| {
            format!("failed to clean process-group resource '{label}' ({process_group}): {err}")
        }),
        CleanupTarget::BackgroundProcess { process_id, label } => {
            let registry = global_registry();
            if !registry.is_running(&process_id) {
                return Ok(());
            }
            registry.kill(&process_id).map_err(|err| {
                format!("failed to clean background process '{label}' ({process_id}): {err}")
            })
        }
        CleanupTarget::AsyncAction { label, cleanup, .. } => cleanup()
            .await
            .map_err(|err| format!("failed to clean async resource '{label}': {err}")),
    }
}

pub async fn cleanup_persisted_resource(resource: &DurableCleanupResource) -> Result<(), String> {
    match resource.kind {
        DurableCleanupResourceKind::Pid | DurableCleanupResourceKind::ProcessGroup => {
            let target = resource.target_value.parse::<u32>().map_err(|e| {
                format!(
                    "invalid durable cleanup target '{}': {e}",
                    resource.target_value
                )
            })?;
            match resource.kind {
                DurableCleanupResourceKind::Pid => kill_process(target).map_err(|err| {
                    format!(
                        "failed to clean durable pid resource '{}': {err}",
                        resource.label
                    )
                }),
                DurableCleanupResourceKind::ProcessGroup => {
                    kill_process_group(target).map_err(|err| {
                        format!(
                            "failed to clean durable process-group resource '{}': {err}",
                            resource.label
                        )
                    })
                }
                DurableCleanupResourceKind::BrowserSession
                | DurableCleanupResourceKind::McpHttpResourceSubscription
                | DurableCleanupResourceKind::McpHttpSession => unreachable!(),
            }
        }
        DurableCleanupResourceKind::BrowserSession => {
            let target: DurableBrowserSessionTarget = serde_json::from_str(&resource.target_value)
                .map_err(|e| {
                    format!(
                        "invalid durable cleanup target '{}': {e}",
                        resource.target_value
                    )
                })?;
            if let Some(process_group) = target.process_group {
                kill_process_group(process_group).map_err(|err| {
                    format!(
                        "failed to clean durable browser-session process group '{}': {err}",
                        resource.label
                    )
                })?;
            } else if let Some(root_pid) = target.root_pid {
                kill_process(root_pid).map_err(|err| {
                    format!(
                        "failed to clean durable browser-session process '{}': {err}",
                        resource.label
                    )
                })?;
            }
            remove_browser_user_data_dir(Path::new(&target.user_data_dir))
                .await
                .map_err(|err| {
                    format!(
                        "failed to clean durable browser-session directory '{}': {err}",
                        resource.label
                    )
                })
        }
        DurableCleanupResourceKind::McpHttpResourceSubscription
        | DurableCleanupResourceKind::McpHttpSession => {
            let Some(executor) = current_durable_cleanup_executor() else {
                return Err(format!(
                    "no durable cleanup executor installed for '{}'",
                    resource.kind.as_str()
                ));
            };
            match executor.cleanup(resource).await {
                Ok(true) => Ok(()),
                Ok(false) => Err(format!(
                    "no durable cleanup handler registered for '{}'",
                    resource.kind.as_str()
                )),
                Err(err) => Err(err),
            }
        }
    }
}

pub async fn remove_browser_user_data_dir(path: &Path) -> Result<(), String> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("{} ({})", err, path.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use super::*;
    use crate::process_registry::configure_child_process_group;
    use tempfile::{NamedTempFile, tempdir};

    #[derive(Default)]
    struct MockRecorder {
        registered: Mutex<Vec<(String, u64, DurableCleanupResource)>>,
        unregistered: Mutex<Vec<(String, u64)>>,
    }

    #[derive(Default)]
    struct MockExecutor {
        cleaned: Mutex<Vec<DurableCleanupResource>>,
    }

    #[async_trait::async_trait]
    impl DurableCleanupRecorder for MockRecorder {
        async fn register(
            &self,
            session_id: &str,
            entry_id: u64,
            resource: DurableCleanupResource,
        ) -> Result<(), String> {
            self.registered
                .lock()
                .expect("mock recorder registered lock poisoned")
                .push((session_id.to_string(), entry_id, resource));
            Ok(())
        }

        async fn unregister(&self, session_id: &str, entry_id: u64) -> Result<(), String> {
            self.unregistered
                .lock()
                .expect("mock recorder unregistered lock poisoned")
                .push((session_id.to_string(), entry_id));
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl DurableCleanupExecutor for MockExecutor {
        async fn cleanup(&self, resource: &DurableCleanupResource) -> Result<bool, String> {
            self.cleaned
                .lock()
                .expect("mock executor cleaned lock poisoned")
                .push(resource.clone());
            Ok(true)
        }
    }

    struct RecorderGuard(Option<Arc<dyn DurableCleanupRecorder>>);

    impl RecorderGuard {
        fn install(recorder: Arc<dyn DurableCleanupRecorder>) -> Self {
            Self(replace_durable_cleanup_recorder(Some(recorder)))
        }
    }

    impl Drop for RecorderGuard {
        fn drop(&mut self) {
            let _ = replace_durable_cleanup_recorder(self.0.take());
        }
    }

    struct ExecutorGuard(Option<Arc<dyn DurableCleanupExecutor>>);

    impl ExecutorGuard {
        fn install(executor: Arc<dyn DurableCleanupExecutor>) -> Self {
            Self(replace_durable_cleanup_executor(Some(executor)))
        }
    }

    impl Drop for ExecutorGuard {
        fn drop(&mut self) {
            let _ = replace_durable_cleanup_executor(self.0.take());
        }
    }

    fn unique_session(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
    }

    fn pid_is_alive(pid: u32) -> bool {
        let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if ret == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        err.raw_os_error() != Some(libc::ESRCH)
    }

    async fn wait_for_pid_file(path: &Path) -> u32 {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(contents) = std::fs::read_to_string(path) {
                    if let Ok(pid) = contents.trim().parse::<u32>() {
                        return pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }

    async fn wait_for_mock_recorder_entries<T, F>(extract: F) -> T
    where
        T: Clone + Send + 'static,
        F: Fn() -> Option<T>,
    {
        tokio::time::timeout(Duration::from_secs(5), async move {
            loop {
                if let Some(value) = extract() {
                    return value;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
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

        let summary = cleanup_session(&session_id).await;
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

        let summary = cleanup_session(&session_id).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 1);
        assert!(summary.failures.is_empty());

        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("child should exit after cleanup")
            .expect("child wait should succeed");
        assert!(!status.success());
    }

    #[tokio::test]
    async fn cleanup_session_kills_registered_process_groups() {
        let session_id = unique_session("cleanup-pgid");
        let pid_file = NamedTempFile::new().unwrap();
        let command = format!("sleep 30 & echo $! > {} && wait", pid_file.path().display());

        let mut cmd = tokio::process::Command::new("bash");
        cmd.args(["-lc", &command]);
        configure_child_process_group(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let process_group = child.id().unwrap();
        let descendant_pid = wait_for_pid_file(pid_file.path()).await;
        let _registration =
            register_process_group(&session_id, process_group, "foreground shell group").unwrap();

        let summary = cleanup_session(&session_id).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 1);
        assert!(summary.failures.is_empty());

        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("shell process should exit after cleanup")
            .expect("child wait should succeed");
        assert!(!status.success());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !pid_is_alive(descendant_pid),
            "descendant pid {descendant_pid} should be terminated with its process group"
        );
    }

    #[tokio::test]
    async fn cleanup_session_runs_async_cleanup_callbacks() {
        let session_id = unique_session("cleanup-async");
        let cleaned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cleaned_flag = std::sync::Arc::clone(&cleaned);
        register_async_cleanup(&session_id, "async test", move || {
            let cleaned_flag = std::sync::Arc::clone(&cleaned_flag);
            async move {
                cleaned_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
        });

        let summary = cleanup_session(&session_id).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 1);
        assert!(summary.failures.is_empty());
        assert!(cleaned.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn async_cleanup_with_durable_resource_persists_and_unregisters_manifest() {
        let _lock = DURABLE_RECORDER_TEST_LOCK.lock().await;
        let session_id = unique_session("cleanup-async-durable");
        let recorder = Arc::new(MockRecorder::default());
        let _guard = RecorderGuard::install(recorder.clone());

        let cleaned = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cleaned_flag = Arc::clone(&cleaned);
        register_async_cleanup_with_durable_resource(
            &session_id,
            "browser session",
            DurableCleanupResource {
                kind: DurableCleanupResourceKind::Pid,
                label: "browser root process".to_string(),
                target_value: "4242".to_string(),
            },
            move || {
                let cleaned_flag = Arc::clone(&cleaned_flag);
                async move {
                    cleaned_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                }
            },
        );

        let recorder_for_registered = Arc::clone(&recorder);
        let session_id_for_registered = session_id.clone();
        let session_registered: Vec<_> = wait_for_mock_recorder_entries(move || {
            let registered = recorder_for_registered
                .registered
                .lock()
                .expect("mock recorder registered lock poisoned")
                .clone();
            let session_registered: Vec<_> = registered
                .into_iter()
                .filter(|(registered_session_id, _, _)| {
                    registered_session_id == &session_id_for_registered
                })
                .collect();
            (!session_registered.is_empty()).then_some(session_registered)
        })
        .await;
        assert_eq!(session_registered.len(), 1);
        let entry_id = session_registered[0].1;
        assert_eq!(
            session_registered[0].2.kind,
            DurableCleanupResourceKind::Pid
        );
        assert_eq!(session_registered[0].2.target_value, "4242");

        let summary = cleanup_session(&session_id).await;
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 1);
        assert!(summary.failures.is_empty());
        assert!(cleaned.load(std::sync::atomic::Ordering::SeqCst));

        let recorder_for_unregistered = Arc::clone(&recorder);
        let session_id_for_unregistered = session_id.clone();
        let session_unregistered: Vec<_> = wait_for_mock_recorder_entries(move || {
            let unregistered = recorder_for_unregistered
                .unregistered
                .lock()
                .expect("mock recorder unregistered lock poisoned")
                .clone();
            let session_unregistered: Vec<_> = unregistered
                .into_iter()
                .filter(|(unregistered_session_id, _)| {
                    unregistered_session_id == &session_id_for_unregistered
                })
                .collect();
            (!session_unregistered.is_empty()).then_some(session_unregistered)
        })
        .await;
        assert_eq!(session_unregistered.len(), 1);
        assert_eq!(session_unregistered[0], (session_id, entry_id));
    }

    #[tokio::test]
    async fn cleanup_persisted_resource_delegates_to_executor_for_mcp_resources() {
        let _lock = DURABLE_RECORDER_TEST_LOCK.lock().await;
        let executor = Arc::new(MockExecutor::default());
        let _guard = ExecutorGuard::install(executor.clone());
        let resource = DurableCleanupResource {
            kind: DurableCleanupResourceKind::McpHttpResourceSubscription,
            label: "mcp subscription".to_string(),
            target_value: r#"{"server":"docs","session_id":"sid_123","uri":"file:///tmp/doc.txt"}"#
                .to_string(),
        };

        cleanup_persisted_resource(&resource).await.unwrap();

        let cleaned = executor
            .cleaned
            .lock()
            .expect("mock executor cleaned lock poisoned")
            .clone();
        assert_eq!(cleaned, vec![resource]);
    }

    #[tokio::test]
    async fn cleanup_persisted_browser_session_resource_removes_user_data_dir() {
        let dir = tempdir().unwrap();
        let user_data_dir = dir.path().join("browser-profile");
        std::fs::create_dir_all(&user_data_dir).unwrap();
        std::fs::write(user_data_dir.join("Preferences"), "{}").unwrap();

        let resource = browser_session_durable_resource(None, None, &user_data_dir).unwrap();
        cleanup_persisted_resource(&resource).await.unwrap();

        assert!(!user_data_dir.exists());
    }

    #[tokio::test]
    async fn cleanup_persisted_browser_session_resource_kills_process_group_descendants() {
        let dir = tempdir().unwrap();
        let user_data_dir = dir.path().join("browser-profile");
        std::fs::create_dir_all(&user_data_dir).unwrap();
        std::fs::write(user_data_dir.join("Preferences"), "{}").unwrap();

        let pid_file = NamedTempFile::new().unwrap();
        let command = format!("sleep 30 & echo $! > {} && wait", pid_file.path().display());
        let mut cmd = tokio::process::Command::new("bash");
        cmd.args(["-lc", &command]);
        configure_child_process_group(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let process_group = child.id().unwrap();
        let descendant_pid = wait_for_pid_file(pid_file.path()).await;
        assert!(pid_is_alive(descendant_pid));

        let resource = browser_session_durable_resource(
            Some(process_group),
            Some(process_group),
            &user_data_dir,
        )
        .unwrap();
        cleanup_persisted_resource(&resource).await.unwrap();

        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .expect("browser shell process should exit after cleanup")
            .expect("child wait should succeed");
        assert!(!status.success());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!pid_is_alive(descendant_pid));
        assert!(!user_data_dir.exists());
    }
}

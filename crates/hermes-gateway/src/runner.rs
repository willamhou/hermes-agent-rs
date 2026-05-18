//! GatewayRunner — orchestrates adapters and routes messages through SessionRouter.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use hermes_config::{
    SqliteSessionStore,
    config::{AppConfig, GatewayConfig, hermes_home},
};
use hermes_core::platform::{PlatformAdapter, PlatformEvent};
use hermes_managed::{
    ManagedRunCleanupResource, ManagedRunCleanupResourceKind, ManagedStore, RunRegistry,
};
use hermes_mcp::McpDurableCleanupExecutor;
use hermes_provider::create_provider;
use hermes_tools::{ToolRegistry, session_cleanup};
use tokio::sync::{RwLock, mpsc};

use crate::api_server::{
    ApiServerAdapter, append_ancestor_follow_replay_decisions_for_replay_child,
    append_source_takeover_update_for_replay_child, maybe_auto_replay_interrupted_runs,
};
use crate::discord::DiscordAdapter;
use crate::session::{SessionRouter, SharedState};
use crate::telegram::TelegramAdapter;

pub struct GatewayRunner {
    gateway_config: GatewayConfig,
    app_config: AppConfig,
}

const MANAGED_CLEANUP_RECOVERY_BATCH_SIZE: usize = 256;
const MANAGED_AUTO_REPLAY_BATCH_SIZE: usize = 64;

#[derive(Default)]
struct ManagedCleanupRecoverySummary {
    attempted: usize,
    cleaned: usize,
    failures: Vec<String>,
    run_failures: HashMap<String, Vec<ManagedCleanupRecoveryFailure>>,
}

#[derive(Debug, Clone)]
struct ManagedCleanupRecoveryFailure {
    entry_id: u64,
    stage: &'static str,
    kind: ManagedRunCleanupResourceKind,
    label: String,
    target_value: String,
    error: String,
}

fn durable_cleanup_resource_from_managed(
    resource: &ManagedRunCleanupResource,
) -> session_cleanup::DurableCleanupResource {
    session_cleanup::DurableCleanupResource {
        kind: match resource.kind {
            ManagedRunCleanupResourceKind::Pid => session_cleanup::DurableCleanupResourceKind::Pid,
            ManagedRunCleanupResourceKind::ProcessGroup => {
                session_cleanup::DurableCleanupResourceKind::ProcessGroup
            }
            ManagedRunCleanupResourceKind::BrowserSession => {
                session_cleanup::DurableCleanupResourceKind::BrowserSession
            }
            ManagedRunCleanupResourceKind::McpHttpResourceSubscription => {
                session_cleanup::DurableCleanupResourceKind::McpHttpResourceSubscription
            }
            ManagedRunCleanupResourceKind::McpHttpSession => {
                session_cleanup::DurableCleanupResourceKind::McpHttpSession
            }
        },
        label: resource.label.clone(),
        target_value: resource.target_value.clone(),
    }
}

async fn reclaim_terminal_managed_cleanup_resources(
    store: &ManagedStore,
) -> anyhow::Result<ManagedCleanupRecoverySummary> {
    let resources = store
        .list_terminal_run_cleanup_resources(MANAGED_CLEANUP_RECOVERY_BATCH_SIZE)
        .await?;
    let mut summary = ManagedCleanupRecoverySummary {
        attempted: resources.len(),
        ..ManagedCleanupRecoverySummary::default()
    };

    for resource in resources {
        let durable = durable_cleanup_resource_from_managed(&resource);
        match session_cleanup::cleanup_persisted_resource(&durable).await {
            Ok(()) => match store
                .delete_run_cleanup_resource(&resource.run_id, resource.entry_id)
                .await
            {
                Ok(_) => summary.cleaned += 1,
                Err(err) => {
                    let err = format!(
                        "failed to delete cleanup manifest for {}#{}: {err}",
                        resource.run_id, resource.entry_id
                    );
                    summary.failures.push(err.clone());
                    summary
                        .run_failures
                        .entry(resource.run_id.clone())
                        .or_default()
                        .push(ManagedCleanupRecoveryFailure {
                            entry_id: resource.entry_id,
                            stage: "delete_manifest",
                            kind: resource.kind.clone(),
                            label: resource.label.clone(),
                            target_value: resource.target_value.clone(),
                            error: err,
                        });
                }
            },
            Err(err) => {
                let err = format!(
                    "failed to reclaim durable cleanup resource for {}#{}: {err}",
                    resource.run_id, resource.entry_id
                );
                summary.failures.push(err.clone());
                summary
                    .run_failures
                    .entry(resource.run_id.clone())
                    .or_default()
                    .push(ManagedCleanupRecoveryFailure {
                        entry_id: resource.entry_id,
                        stage: "cleanup_resource",
                        kind: resource.kind.clone(),
                        label: resource.label.clone(),
                        target_value: resource.target_value.clone(),
                        error: err,
                    });
            }
        }
    }

    Ok(summary)
}

fn managed_cleanup_recovery_failure_event(
    failures: &[ManagedCleanupRecoveryFailure],
    context: &str,
) -> hermes_managed::ManagedRunEventDraft {
    hermes_managed::ManagedRunEventDraft {
        kind: hermes_managed::ManagedRunEventKind::RunCleanupFailed,
        message: Some(format!(
            "Managed durable cleanup recovery failed for {} resource(s)",
            failures.len()
        )),
        tool_name: None,
        tool_call_id: None,
        metadata: Some(serde_json::json!({
            "phase": "recovery_reclaim",
            "context": context,
            "failures": failures.iter().map(|failure| serde_json::json!({
                "entry_id": failure.entry_id,
                "stage": failure.stage,
                "kind": failure.kind.as_str(),
                "label": failure.label,
                "target_value": failure.target_value,
                "error": failure.error,
            })).collect::<Vec<_>>(),
        })),
    }
}

async fn log_terminal_managed_cleanup_recovery(
    store: &ManagedStore,
    summary: &ManagedCleanupRecoverySummary,
    context: &str,
) {
    if summary.attempted == 0 {
        return;
    }

    if summary.failures.is_empty() {
        tracing::warn!(
            attempted = summary.attempted,
            cleaned = summary.cleaned,
            context,
            "reclaimed durable cleanup resources for terminal managed runs"
        );
    } else {
        tracing::warn!(
            attempted = summary.attempted,
            cleaned = summary.cleaned,
            failures = ?summary.failures,
            context,
            "managed durable cleanup recovery completed with failures"
        );
        for (run_id, failures) in &summary.run_failures {
            let _ = store
                .append_run_event(
                    run_id,
                    &managed_cleanup_recovery_failure_event(failures, context),
                )
                .await;
        }
    }
}

fn log_managed_auto_replay_summary(
    summary: &crate::api_server::ManagedAutoReplaySummary,
    context: &str,
) {
    if summary.is_empty() {
        return;
    }

    if summary.failures.is_empty() {
        tracing::warn!(
            candidates = summary.candidates,
            replayed = summary.replayed_run_ids.len(),
            skipped_depth_limit = summary.skipped_depth_limit,
            replayed_run_ids = ?summary.replayed_run_ids,
            context,
            "processed interrupted managed run auto-replay sweep"
        );
    } else {
        tracing::warn!(
            candidates = summary.candidates,
            replayed = summary.replayed_run_ids.len(),
            skipped_depth_limit = summary.skipped_depth_limit,
            replayed_run_ids = ?summary.replayed_run_ids,
            failures = ?summary.failures,
            context,
            "managed interrupted auto-replay sweep completed with failures"
        );
    }
}

impl GatewayRunner {
    pub fn new(gateway_config: GatewayConfig, app_config: AppConfig) -> Self {
        Self {
            gateway_config,
            app_config,
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        // 1. Build shared state
        let api_key = self
            .app_config
            .api_key()
            .ok_or_else(|| anyhow::anyhow!("No API key configured"))?;
        let provider = create_provider(
            &self.app_config.model,
            api_key,
            self.app_config.base_url.as_deref(),
        )?;

        // Build tool registry — same tools as CLI, including inventory-registered ones
        let registry = Arc::new(ToolRegistry::from_inventory());

        let working_dir = std::env::current_dir()?;
        let tool_config = Arc::new(self.app_config.tool_config(working_dir));

        // Skills (optional — if skills dir exists)
        let skills = {
            let skills_dir = hermes_home().join("skills");
            if skills_dir.exists() {
                match hermes_skills::SkillManager::new(vec![skills_dir]) {
                    Ok(sm) => Some(Arc::new(RwLock::new(sm))),
                    Err(e) => {
                        tracing::warn!("failed to load skills: {e}");
                        None
                    }
                }
            } else {
                None
            }
        };

        let session_store = match SqliteSessionStore::open().await {
            Ok(store) => Some(Arc::new(store) as Arc<dyn hermes_core::session::SessionStore>),
            Err(e) => {
                tracing::warn!("gateway session persistence disabled — failed to open store: {e}");
                None
            }
        };

        // 2. Create adapters + event channel
        let (event_tx, mut event_rx) = mpsc::channel::<PlatformEvent>(256);
        let mut adapters: HashMap<String, Arc<dyn PlatformAdapter>> = HashMap::new();
        let mut adapter_handles = Vec::new();
        let mut managed_recovery_handle = None;

        if let Some(ref tg_config) = self.gateway_config.telegram {
            let adapter = Arc::new(TelegramAdapter::new(
                tg_config.token.clone(),
                tg_config.allowed_users.clone(),
                tg_config.allow_all,
            ));
            adapters.insert(
                "telegram".into(),
                adapter.clone() as Arc<dyn PlatformAdapter>,
            );
            let tx = event_tx.clone();
            adapter_handles.push(tokio::spawn(async move { adapter.run(tx).await }));
            tracing::info!("telegram adapter enabled");
        }

        if let Some(ref dc_config) = self.gateway_config.discord {
            let adapter = Arc::new(DiscordAdapter::new(
                dc_config.token.clone(),
                dc_config.allowed_users.clone(),
                dc_config.allow_all,
            ));
            adapters.insert(
                "discord".into(),
                adapter.clone() as Arc<dyn PlatformAdapter>,
            );
            let tx = event_tx.clone();
            adapter_handles.push(tokio::spawn(async move { adapter.run(tx).await }));
            tracing::info!("discord adapter enabled");
        }

        // API server adapter — constructed here but started after router is built
        let api_adapter = self.gateway_config.api_server.as_ref().map(|api_config| {
            let mut cfg = api_config.clone();
            // Populate model_name from AppConfig if not set explicitly
            if cfg.model_name.is_none() {
                cfg.model_name = Some(self.app_config.model.clone());
            }
            let adapter = Arc::new(ApiServerAdapter::new(cfg));
            adapters.insert("api".into(), adapter.clone() as Arc<dyn PlatformAdapter>);
            adapter
        });

        drop(event_tx); // only adapters hold senders now

        // 3. Build session router
        let shared = Arc::new(SharedState {
            provider,
            registry,
            tool_config,
            skills,
            session_store,
            adapters,
        });
        let router = SessionRouter::new(
            Arc::clone(&shared),
            self.gateway_config.session_idle_timeout_secs,
            self.gateway_config.max_concurrent_sessions,
            self.app_config.clone(),
        );

        // Start API server with router for streaming, plus event channel for legacy /api/chat
        if let Some(adapter) = api_adapter {
            adapter.set_router(router.clone());
            match ManagedStore::open().await {
                Ok(store) => {
                    let store = Arc::new(store);
                    let runs = Arc::new(RunRegistry::new());
                    let worker_id = format!("gw_{}", uuid::Uuid::new_v4().simple());
                    let cleanup_recorder: Arc<dyn session_cleanup::DurableCleanupRecorder> =
                        store.clone();
                    let _ =
                        session_cleanup::replace_durable_cleanup_recorder(Some(cleanup_recorder));
                    let cleanup_executor: Arc<dyn session_cleanup::DurableCleanupExecutor> =
                        Arc::new(McpDurableCleanupExecutor::new(
                            self.app_config.mcp_servers.clone(),
                        ));
                    let _ =
                        session_cleanup::replace_durable_cleanup_executor(Some(cleanup_executor));
                    match store.reconcile_incomplete_runs().await {
                        Ok(reconciled) if !reconciled.is_empty() => {
                            tracing::warn!(
                                count = reconciled.len(),
                                "reconciled managed runs left active by a previous process"
                            );
                            for run in &reconciled {
                                append_source_takeover_update_for_replay_child(
                                    store.as_ref(),
                                    run,
                                    None,
                                )
                                .await;
                                append_ancestor_follow_replay_decisions_for_replay_child(
                                    store.as_ref(),
                                    run,
                                )
                                .await;
                            }
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(
                            "managed runtime started without reconciling incomplete runs: {e}"
                        ),
                    }
                    match reclaim_terminal_managed_cleanup_resources(store.as_ref()).await {
                        Ok(summary) => {
                            log_terminal_managed_cleanup_recovery(
                                store.as_ref(),
                                &summary,
                                "startup",
                            )
                            .await;
                        }
                        Err(e) => tracing::warn!(
                            "managed runtime started without reclaiming persisted cleanup resources: {e}"
                        ),
                    }
                    match maybe_auto_replay_interrupted_runs(
                        Arc::clone(&shared),
                        self.app_config.clone(),
                        Arc::clone(&store),
                        Arc::clone(&runs),
                        worker_id.clone(),
                        MANAGED_AUTO_REPLAY_BATCH_SIZE,
                    )
                    .await
                    {
                        Ok(summary) => log_managed_auto_replay_summary(&summary, "startup"),
                        Err(e) => tracing::warn!(
                            "managed runtime started without sweeping interrupted auto-replay candidates: {e}"
                        ),
                    }
                    let recovery_shared = Arc::clone(&shared);
                    let recovery_app_config = self.app_config.clone();
                    let recovery_store = Arc::clone(&store);
                    let recovery_runs = Arc::clone(&runs);
                    let recovery_worker_id = worker_id.clone();
                    managed_recovery_handle = Some(tokio::spawn(async move {
                        let mut interval = tokio::time::interval(Duration::from_secs(15));
                        interval.tick().await;
                        loop {
                            interval.tick().await;
                            match recovery_store.reconcile_incomplete_runs().await {
                                Ok(reconciled) if !reconciled.is_empty() => {
                                    tracing::warn!(
                                        count = reconciled.len(),
                                        "reconciled managed runs after ownership lease expiry"
                                    );
                                    for run in &reconciled {
                                        append_source_takeover_update_for_replay_child(
                                            recovery_store.as_ref(),
                                            run,
                                            None,
                                        )
                                        .await;
                                        append_ancestor_follow_replay_decisions_for_replay_child(
                                            recovery_store.as_ref(),
                                            run,
                                        )
                                        .await;
                                    }
                                }
                                Ok(_) => {}
                                Err(e) => tracing::warn!(
                                    "managed recovery sweep failed while reconciling incomplete runs: {e}"
                                ),
                            }
                            match reclaim_terminal_managed_cleanup_resources(
                                recovery_store.as_ref(),
                            )
                            .await
                            {
                                Ok(summary) => {
                                    log_terminal_managed_cleanup_recovery(
                                        recovery_store.as_ref(),
                                        &summary,
                                        "periodic",
                                    )
                                    .await;
                                }
                                Err(e) => tracing::warn!(
                                    "managed recovery sweep failed while reclaiming persisted cleanup resources: {e}"
                                ),
                            }
                            match maybe_auto_replay_interrupted_runs(
                                Arc::clone(&recovery_shared),
                                recovery_app_config.clone(),
                                Arc::clone(&recovery_store),
                                Arc::clone(&recovery_runs),
                                recovery_worker_id.clone(),
                                MANAGED_AUTO_REPLAY_BATCH_SIZE,
                            )
                            .await
                            {
                                Ok(summary) => {
                                    log_managed_auto_replay_summary(&summary, "periodic");
                                }
                                Err(e) => tracing::warn!(
                                    "managed recovery sweep failed while auto-replaying interrupted runs: {e}"
                                ),
                            }
                        }
                    }));

                    adapter.set_managed_state(
                        Arc::clone(&shared),
                        self.app_config.clone(),
                        store,
                        runs,
                        worker_id,
                    )
                }
                Err(e) => tracing::warn!("managed runtime disabled — failed to open store: {e}"),
            }
            let (api_event_tx, mut api_event_rx) = mpsc::channel::<PlatformEvent>(256);
            let api_router = router.clone();
            tokio::spawn(async move {
                while let Some(event) = api_event_rx.recv().await {
                    match event {
                        PlatformEvent::Message(msg) => {
                            let r = api_router.clone();
                            tokio::spawn(async move {
                                r.route(msg).await;
                            });
                        }
                        PlatformEvent::Shutdown => break,
                    }
                }
            });
            adapter_handles.push(tokio::spawn(async move { adapter.run(api_event_tx).await }));
            if let Some(ref api_config) = self.gateway_config.api_server {
                tracing::info!(addr = %api_config.bind_addr, "api server enabled (OpenAI compatible)");
            }
        }

        // 4. Spawn idle cleanup task
        let cleanup_router = router.clone();
        let cleanup_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                cleanup_router.cleanup_stale();
            }
        });

        // Spawn cron scheduler tick loop
        let cron_store_path = hermes_config::config::hermes_home()
            .join("cron")
            .join("jobs.json");
        match hermes_cron::store::JobStore::open(cron_store_path) {
            Ok(cron_store) => {
                let output_dir = hermes_config::config::hermes_home()
                    .join("cron")
                    .join("output");
                let cron_config = self.app_config.clone();
                tokio::spawn(async move {
                    let scheduler = hermes_cron::scheduler::CronScheduler::new(
                        cron_store,
                        output_dir,
                        cron_config,
                    );
                    // H4: skip the immediate first tick — start 60 s from now.
                    let mut interval = tokio::time::interval_at(
                        tokio::time::Instant::now() + std::time::Duration::from_secs(60),
                        std::time::Duration::from_secs(60),
                    );
                    loop {
                        interval.tick().await;
                        if let Err(e) = scheduler.tick().await {
                            tracing::warn!("cron tick error: {e}");
                        }
                    }
                });
                tracing::info!("cron scheduler enabled (60s tick)");
            }
            Err(e) => {
                // H5: log the failure so operators can diagnose misconfiguration.
                tracing::warn!("cron scheduler disabled — failed to open job store: {e}");
            }
        }

        // 5. Main event loop
        tracing::info!("gateway started — waiting for messages");

        // Telegram/Discord adapters send through event_rx.
        // API server routes directly through SessionRouter (bypasses event channel).
        let has_event_adapters =
            self.gateway_config.telegram.is_some() || self.gateway_config.discord.is_some();

        if has_event_adapters {
            // Event-driven: process messages until all adapters disconnect
            while let Some(event) = event_rx.recv().await {
                match event {
                    PlatformEvent::Message(msg) => {
                        let r = router.clone();
                        tokio::spawn(async move {
                            r.route(msg).await;
                        });
                    }
                    PlatformEvent::Shutdown => break,
                }
            }
        } else {
            // API-only mode: wait for adapter handle (axum serve blocks)
            tracing::info!("api-only mode — press ctrl-c to stop");
            for handle in &mut adapter_handles {
                let _ = handle.await;
            }
        }

        // 6. Shutdown
        cleanup_handle.abort();
        if let Some(handle) = managed_recovery_handle {
            handle.abort();
        }
        for handle in adapter_handles {
            handle.abort();
        }
        router.shutdown();
        tracing::info!("gateway stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::{Arc, LazyLock, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use chrono::Utc;
    use hermes_core::{
        stream::StreamDelta,
        tool::{
            ApprovalRequest, BrowserToolConfig, FileToolConfig, TerminalToolConfig, Tool,
            ToolConfig, ToolContext,
        },
    };
    use hermes_managed::{
        ManagedAgent, ManagedAgentVersion, ManagedRun, ManagedRunEventKind, ManagedRunStatus,
    };
    use hermes_tools::{browser::BrowserTool, process_registry::global_registry};
    use tempfile::{NamedTempFile, TempDir};
    use tokio::sync::mpsc;

    use super::*;

    static CLEANUP_EXECUTOR_TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));
    static CLEANUP_RECORDER_TEST_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    #[derive(Default)]
    struct MockCleanupExecutor {
        cleaned: Mutex<Vec<session_cleanup::DurableCleanupResource>>,
    }

    #[async_trait]
    impl session_cleanup::DurableCleanupExecutor for MockCleanupExecutor {
        async fn cleanup(
            &self,
            resource: &session_cleanup::DurableCleanupResource,
        ) -> std::result::Result<bool, String> {
            self.cleaned
                .lock()
                .expect("mock cleanup executor lock poisoned")
                .push(resource.clone());
            Ok(true)
        }
    }

    struct CleanupExecutorGuard(Option<Arc<dyn session_cleanup::DurableCleanupExecutor>>);

    impl CleanupExecutorGuard {
        fn install(executor: Arc<dyn session_cleanup::DurableCleanupExecutor>) -> Self {
            Self(session_cleanup::replace_durable_cleanup_executor(Some(
                executor,
            )))
        }
    }

    impl Drop for CleanupExecutorGuard {
        fn drop(&mut self) {
            let _ = session_cleanup::replace_durable_cleanup_executor(self.0.take());
        }
    }

    struct CleanupRecorderGuard(Option<Arc<dyn session_cleanup::DurableCleanupRecorder>>);

    impl CleanupRecorderGuard {
        fn install(recorder: Arc<dyn session_cleanup::DurableCleanupRecorder>) -> Self {
            Self(session_cleanup::replace_durable_cleanup_recorder(Some(
                recorder,
            )))
        }
    }

    impl Drop for CleanupRecorderGuard {
        fn drop(&mut self) {
            let _ = session_cleanup::replace_durable_cleanup_recorder(self.0.take());
        }
    }

    fn temp_db() -> (TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        (dir, path)
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

    fn make_browser_test_ctx(workspace: &Path, session_id: &str) -> ToolContext {
        let (approval_tx, _) = mpsc::channel::<ApprovalRequest>(1);
        let (delta_tx, _) = mpsc::channel::<StreamDelta>(1);
        let browser_executable = std::env::var_os("CHROME").map(PathBuf::from);
        ToolContext {
            session_id: session_id.to_string(),
            working_dir: workspace.to_path_buf(),
            approval_tx,
            delta_tx,
            execution_observer: None,
            tool_config: Arc::new(ToolConfig {
                terminal: TerminalToolConfig::default(),
                file: FileToolConfig::default(),
                browser: BrowserToolConfig {
                    sandbox: false,
                    executable: browser_executable,
                    ..BrowserToolConfig::default()
                },
                workspace_root: workspace.to_path_buf(),
            }),
            memory: None,
            aux_provider: None,
            skills: None,
            delegation_depth: 0,
            clarify_tx: None,
        }
    }

    fn browser_env_unavailable(message: &str) -> bool {
        message.contains("failed to detect browser executable")
    }

    async fn wait_for_browser_cleanup_resource(
        store: &ManagedStore,
        run_id: &str,
    ) -> ManagedRunCleanupResource {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let resources = store.list_run_cleanup_resources(run_id).await.unwrap();
                if let Some(resource) = resources.into_iter().next() {
                    return resource;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("browser cleanup resource should be persisted")
    }

    #[tokio::test]
    async fn reclaim_terminal_cleanup_resources_kills_process_groups_and_clears_manifest() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("cleanup-recovery");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.ended_at = Some(Utc::now());
        store.create_run(&run).await.unwrap();

        let registry = global_registry();
        registry.remove_exited();
        let pid_file = NamedTempFile::new().unwrap();
        let command = format!("sleep 30 & echo $! > {} && wait", pid_file.path().display());
        let process_id = registry.spawn(&command, Path::new("/tmp")).unwrap();
        let process_group = registry.process_group_for(&process_id).unwrap();
        let descendant_pid = wait_for_pid_file(pid_file.path()).await;
        assert!(pid_is_alive(descendant_pid));

        store
            .upsert_run_cleanup_resource(
                &run.id,
                1,
                ManagedRunCleanupResourceKind::ProcessGroup,
                "shell worker",
                &process_group.to_string(),
            )
            .await
            .unwrap();

        let summary = reclaim_terminal_managed_cleanup_resources(&store)
            .await
            .unwrap();
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 1);
        assert!(summary.failures.is_empty());

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!pid_is_alive(descendant_pid));
        assert!(!registry.is_running(&process_id));
        assert!(
            store
                .list_run_cleanup_resources(&run.id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn reclaim_terminal_cleanup_resources_retains_manifest_on_failure() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("cleanup-recovery-failure");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.ended_at = Some(Utc::now());
        store.create_run(&run).await.unwrap();

        store
            .upsert_run_cleanup_resource(
                &run.id,
                9,
                ManagedRunCleanupResourceKind::ProcessGroup,
                "broken shell worker",
                "not-a-pgid",
            )
            .await
            .unwrap();

        let summary = reclaim_terminal_managed_cleanup_resources(&store)
            .await
            .unwrap();
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 0);
        assert_eq!(summary.failures.len(), 1);
        log_terminal_managed_cleanup_recovery(&store, &summary, "test").await;

        let resources = store.list_run_cleanup_resources(&run.id).await.unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].entry_id, 9);
        let events = store.list_run_events(&run.id, 32).await.unwrap();
        let cleanup_failed = events
            .iter()
            .find(|event| event.kind == ManagedRunEventKind::RunCleanupFailed)
            .expect("expected cleanup failure event");
        assert_eq!(
            cleanup_failed.metadata.as_ref().unwrap()["phase"],
            "recovery_reclaim"
        );
        assert_eq!(cleanup_failed.metadata.as_ref().unwrap()["context"], "test");
        assert_eq!(
            cleanup_failed.metadata.as_ref().unwrap()["failures"][0]["stage"],
            "cleanup_resource"
        );
    }

    #[tokio::test]
    async fn reclaim_terminal_cleanup_resources_removes_browser_session_dirs() {
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();

        let agent = ManagedAgent::new("cleanup-recovery-browser");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.ended_at = Some(Utc::now());
        store.create_run(&run).await.unwrap();

        let registry = global_registry();
        registry.remove_exited();
        let pid_file = NamedTempFile::new().unwrap();
        let command = format!("sleep 30 & echo $! > {} && wait", pid_file.path().display());
        let process_id = registry.spawn(&command, Path::new("/tmp")).unwrap();
        let process_group = registry.process_group_for(&process_id).unwrap();
        let descendant_pid = wait_for_pid_file(pid_file.path()).await;
        assert!(pid_is_alive(descendant_pid));

        let browser_dir = tempfile::tempdir().unwrap();
        let user_data_dir = browser_dir.path().join("profile");
        std::fs::create_dir_all(&user_data_dir).unwrap();
        std::fs::write(user_data_dir.join("Preferences"), "{}").unwrap();

        store
            .upsert_run_cleanup_resource(
                &run.id,
                10,
                ManagedRunCleanupResourceKind::BrowserSession,
                "browser session state",
                &format!(
                    r#"{{"root_pid":null,"process_group":{},"user_data_dir":"{}"}}"#,
                    process_group,
                    user_data_dir.display()
                ),
            )
            .await
            .unwrap();

        let summary = reclaim_terminal_managed_cleanup_resources(&store)
            .await
            .unwrap();
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 1);
        assert!(summary.failures.is_empty());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!pid_is_alive(descendant_pid));
        assert!(!registry.is_running(&process_id));
        assert!(!user_data_dir.exists());
        assert!(
            store
                .list_run_cleanup_resources(&run.id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn reclaim_terminal_cleanup_resources_reclaims_live_browser_session_manifests() {
        let _lock = CLEANUP_RECORDER_TEST_LOCK.lock().await;
        let workspace = tempfile::tempdir().unwrap();
        let (_dir, path) = temp_db();
        let store = Arc::new(ManagedStore::open_at(&path).await.unwrap());
        let _guard = CleanupRecorderGuard::install(store.clone());

        let agent = ManagedAgent::new("cleanup-recovery-live-browser");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.ended_at = Some(Utc::now());
        store.create_run(&run).await.unwrap();

        let html_path = workspace.path().join("index.html");
        std::fs::write(
            &html_path,
            "<html><body><h1>Live browser reclaim</h1></body></html>",
        )
        .unwrap();
        let url = format!("file://{}", html_path.display());
        let tool = BrowserTool::new();
        let ctx = make_browser_test_ctx(workspace.path(), &run.id);
        let result = tool
            .execute(
                serde_json::json!({ "action": "navigate", "url": url }),
                &ctx,
            )
            .await
            .unwrap();
        if result.is_error && browser_env_unavailable(&result.content) {
            eprintln!("skipping live browser runner test: {}", result.content);
            return;
        }
        assert!(!result.is_error, "{}", result.content);

        let resource = wait_for_browser_cleanup_resource(store.as_ref(), &run.id).await;
        assert_eq!(resource.kind, ManagedRunCleanupResourceKind::BrowserSession);
        let target: serde_json::Value = serde_json::from_str(&resource.target_value).unwrap();
        let user_data_dir = PathBuf::from(target["user_data_dir"].as_str().unwrap());
        assert!(user_data_dir.exists());

        let summary = reclaim_terminal_managed_cleanup_resources(store.as_ref())
            .await
            .unwrap();
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 1);
        assert!(summary.failures.is_empty());

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!user_data_dir.exists());
        assert!(
            store
                .list_run_cleanup_resources(&run.id)
                .await
                .unwrap()
                .is_empty()
        );

        let _ = session_cleanup::cleanup_session(&run.id).await;
    }

    #[tokio::test]
    async fn reclaim_terminal_cleanup_resources_delegates_mcp_resources_to_executor() {
        let _lock = CLEANUP_EXECUTOR_TEST_LOCK.lock().await;
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();
        let executor = Arc::new(MockCleanupExecutor::default());
        let _guard = CleanupExecutorGuard::install(executor.clone());

        let agent = ManagedAgent::new("cleanup-recovery-mcp");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.ended_at = Some(Utc::now());
        store.create_run(&run).await.unwrap();

        store
            .upsert_run_cleanup_resource(
                &run.id,
                3,
                ManagedRunCleanupResourceKind::McpHttpResourceSubscription,
                "mcp docs subscription",
                r#"{"server":"docs","session_id":"sid_123","uri":"file:///tmp/doc.txt"}"#,
            )
            .await
            .unwrap();

        let summary = reclaim_terminal_managed_cleanup_resources(&store)
            .await
            .unwrap();
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 1);
        assert!(summary.failures.is_empty());

        let cleaned = executor
            .cleaned
            .lock()
            .expect("mock cleanup executor lock poisoned")
            .clone();
        assert_eq!(cleaned.len(), 1);
        assert_eq!(
            cleaned[0].kind,
            session_cleanup::DurableCleanupResourceKind::McpHttpResourceSubscription
        );
        assert!(
            store
                .list_run_cleanup_resources(&run.id)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn reclaim_terminal_cleanup_resources_delegates_mcp_session_resources_to_executor() {
        let _lock = CLEANUP_EXECUTOR_TEST_LOCK.lock().await;
        let (_dir, path) = temp_db();
        let store = ManagedStore::open_at(&path).await.unwrap();
        let executor = Arc::new(MockCleanupExecutor::default());
        let _guard = CleanupExecutorGuard::install(executor.clone());

        let agent = ManagedAgent::new("cleanup-recovery-mcp-session");
        store.create_agent(&agent).await.unwrap();
        let version = ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "prompt");
        store.create_agent_version(&version).await.unwrap();

        let mut run = ManagedRun::new(&agent.id, 1, "openai/gpt-4o-mini");
        run.status = ManagedRunStatus::Interrupted;
        run.ended_at = Some(Utc::now());
        store.create_run(&run).await.unwrap();

        store
            .upsert_run_cleanup_resource(
                &run.id,
                4,
                ManagedRunCleanupResourceKind::McpHttpSession,
                "mcp docs session",
                r#"{"server":"docs","session_id":"sid_123","protocol_version":"2025-06-18"}"#,
            )
            .await
            .unwrap();

        let summary = reclaim_terminal_managed_cleanup_resources(&store)
            .await
            .unwrap();
        assert_eq!(summary.attempted, 1);
        assert_eq!(summary.cleaned, 1);
        assert!(summary.failures.is_empty());

        let cleaned = executor
            .cleaned
            .lock()
            .expect("mock cleanup executor lock poisoned")
            .clone();
        assert_eq!(cleaned.len(), 1);
        assert_eq!(
            cleaned[0].kind,
            session_cleanup::DurableCleanupResourceKind::McpHttpSession
        );
        assert!(
            store
                .list_run_cleanup_resources(&run.id)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

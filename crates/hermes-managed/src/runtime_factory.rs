use std::{path::PathBuf, sync::Arc};

use hermes_agent::{
    compressor::CompressionConfig,
    loop_runner::{Agent, AgentConfig},
};
use hermes_config::config::AppConfig;
use hermes_core::{
    error::{HermesError, Result},
    tool::ApprovalDecision,
};
use hermes_memory::MemoryManager;
use hermes_provider::create_provider;
use hermes_skills::SkillManager;
use hermes_tools::ToolRegistry;
use tokio::sync::{RwLock, mpsc};

use crate::{
    build_filtered_registry, build_filtered_skill_manager,
    signet::build_signet_observer,
    types::{
        ManagedAgent, ManagedAgentVersion, ManagedApprovalPolicy, ManagedRun, ManagedRunStatus,
    },
};

pub struct ManagedRuntime {
    pub agent: Agent,
    pub registry: Arc<ToolRegistry>,
    pub skills: Option<Arc<RwLock<SkillManager>>>,
    pub run: ManagedRun,
    pub timeout_secs: u32,
}

fn managed_base_url<'a>(
    version: &'a ManagedAgentVersion,
    app_config: &'a AppConfig,
) -> Option<&'a str> {
    version
        .base_url
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            app_config
                .base_url
                .as_deref()
                .filter(|value| !value.trim().is_empty())
        })
}

pub async fn build_managed_runtime(
    agent: &ManagedAgent,
    version: &ManagedAgentVersion,
    source_registry: &ToolRegistry,
    source_skills: Option<&Arc<RwLock<SkillManager>>>,
    app_config: &AppConfig,
    working_dir: PathBuf,
) -> Result<ManagedRuntime> {
    let api_key = app_config
        .api_key_for_model(&version.model)
        .ok_or_else(|| {
            HermesError::Config(format!(
                "no API key configured for managed model: {}",
                version.model
            ))
        })?;
    let provider = create_provider(
        &version.model,
        api_key,
        managed_base_url(version, app_config),
    )
    .map_err(|e| HermesError::Config(format!("failed to create provider: {e}")))?;

    let filtered_registry = Arc::new(build_filtered_registry(
        source_registry,
        &version.allowed_tools,
    )?);

    let filtered_skills = match (source_skills, version.allowed_skills.is_empty()) {
        (_, true) => None,
        (Some(skills), false) => {
            let guard = skills.read().await;
            let filtered = build_filtered_skill_manager(&guard, &version.allowed_skills)?;
            Some(Arc::new(RwLock::new(filtered)))
        }
        (None, false) => {
            return Err(HermesError::Config(
                "managed agent requires skills but no skill manager is loaded".to_string(),
            ));
        }
    };

    let memory_dir = hermes_config::config::hermes_home()
        .join("memories")
        .join("managed")
        .join(&agent.id);
    let memory = MemoryManager::new(memory_dir, None)
        .map_err(|e| HermesError::Config(format!("failed to create memory: {e}")))?;
    let execution_observer = build_signet_observer(app_config)?;

    let (approval_tx, mut approval_rx) = mpsc::channel::<hermes_core::tool::ApprovalRequest>(8);
    let approval_policy = version.approval_policy.clone();
    tokio::spawn(async move {
        while let Some(req) = approval_rx.recv().await {
            let decision = match approval_policy {
                ManagedApprovalPolicy::Yolo => ApprovalDecision::Allow,
                ManagedApprovalPolicy::Ask | ManagedApprovalPolicy::Deny => ApprovalDecision::Deny,
            };
            let _ = req.response_tx.send(decision);
        }
    });

    let mut run = ManagedRun::new(agent.id.clone(), version.version, version.model.clone());
    run.status = ManagedRunStatus::Running;
    run.updated_at = chrono::Utc::now();

    let agent = Agent::new(AgentConfig {
        provider,
        registry: Arc::clone(&filtered_registry),
        max_iterations: version.max_iterations,
        system_prompt: version.system_prompt.clone(),
        session_id: run.id.clone(),
        working_dir: working_dir.clone(),
        approval_tx,
        tool_config: Arc::new(app_config.tool_config(working_dir)),
        execution_observer,
        memory,
        skills: filtered_skills.clone(),
        compression: CompressionConfig::default(),
        delegation_depth: 0,
        clarify_tx: None,
    });

    Ok(ManagedRuntime {
        agent,
        registry: filtered_registry,
        skills: filtered_skills,
        run,
        timeout_secs: version.timeout_secs,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::LazyLock;

    use hermes_core::tool::Tool;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    static ENV_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));

    struct EnvVarGuard {
        name: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let previous = std::env::var(name).ok();
            unsafe {
                std::env::set_var(name, value);
            }
            Self { name, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.name, value) },
                None => unsafe { std::env::remove_var(self.name) },
            }
        }
    }

    struct MockReadTool;

    #[async_trait::async_trait]
    impl Tool for MockReadTool {
        fn name(&self) -> &str {
            "read_file"
        }

        fn schema(&self) -> hermes_core::tool::ToolSchema {
            hermes_core::tool::ToolSchema {
                name: "read_file".to_string(),
                description: "read".to_string(),
                parameters: json!({}),
            }
        }

        fn toolset(&self) -> &str {
            "file"
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &hermes_core::tool::ToolContext,
        ) -> Result<hermes_core::message::ToolResult> {
            Ok(hermes_core::message::ToolResult::ok("ok"))
        }
    }

    fn write_skill(dir: &std::path::Path, name: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                r#"---
name: {name}
description: {name} helper
platforms: [linux]
---

Use {name}
"#
            ),
        )
        .unwrap();
    }

    fn cfg() -> AppConfig {
        AppConfig::default()
    }

    #[test]
    fn managed_base_url_uses_version_override_before_app_config() {
        let app_config = AppConfig {
            base_url: Some("https://config.example/v1".to_string()),
            ..AppConfig::default()
        };

        let mut version =
            ManagedAgentVersion::new("agent_123", 1, "openai/gpt-4o-mini", "system prompt");
        version.base_url = Some("https://version.example/v1".to_string());

        assert_eq!(
            managed_base_url(&version, &app_config),
            Some("https://version.example/v1")
        );
    }

    #[test]
    fn managed_base_url_falls_back_to_app_config() {
        let app_config = AppConfig {
            base_url: Some("https://config.example/v1".to_string()),
            ..AppConfig::default()
        };

        let version =
            ManagedAgentVersion::new("agent_123", 1, "openai/gpt-4o-mini", "system prompt");

        assert_eq!(
            managed_base_url(&version, &app_config),
            Some("https://config.example/v1")
        );
    }

    #[test]
    fn managed_base_url_ignores_blank_version_override() {
        let app_config = AppConfig {
            base_url: Some("https://config.example/v1".to_string()),
            ..AppConfig::default()
        };

        let mut version =
            ManagedAgentVersion::new("agent_123", 1, "openai/gpt-4o-mini", "system prompt");
        version.base_url = Some("   ".to_string());

        assert_eq!(
            managed_base_url(&version, &app_config),
            Some("https://config.example/v1")
        );
    }

    #[tokio::test]
    async fn build_runtime_filters_tools_and_skills() {
        let _guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");

        let tmp = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &tmp.path().to_string_lossy());
        write_skill(tmp.path(), "deploy");
        write_skill(tmp.path(), "testing");

        let source_registry = ToolRegistry::new();
        source_registry.register(Box::new(MockReadTool));

        let source_skills = Arc::new(RwLock::new(
            SkillManager::new(vec![tmp.path().to_path_buf()]).unwrap(),
        ));

        let agent = ManagedAgent::new("code-reviewer");
        let mut version =
            ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "system prompt");
        version.allowed_tools = vec!["read_file".to_string()];
        version.allowed_skills = vec!["deploy".to_string()];

        let runtime = build_managed_runtime(
            &agent,
            &version,
            &source_registry,
            Some(&source_skills),
            &cfg(),
            std::env::temp_dir(),
        )
        .await
        .unwrap();

        assert_eq!(runtime.registry.tool_names(), vec!["read_file".to_string()]);
        let skills = runtime.skills.unwrap();
        let guard = skills.read().await;
        assert_eq!(guard.list().len(), 1);
        assert_eq!(guard.list()[0].name, "deploy");
    }

    #[tokio::test]
    async fn build_runtime_requires_loaded_skills_when_allowlist_is_nonempty() {
        let _guard = ENV_LOCK.lock().await;
        let _api_key_guard = EnvVarGuard::set("OPENAI_API_KEY", "test-openai-key");
        let tmp = TempDir::new().unwrap();
        let _home_guard = EnvVarGuard::set("HERMES_HOME", &tmp.path().to_string_lossy());

        let source_registry = ToolRegistry::new();
        source_registry.register(Box::new(MockReadTool));

        let agent = ManagedAgent::new("code-reviewer");
        let mut version =
            ManagedAgentVersion::new(&agent.id, 1, "openai/gpt-4o-mini", "system prompt");
        version.allowed_tools = vec!["read_file".to_string()];
        version.allowed_skills = vec!["deploy".to_string()];

        let err = build_managed_runtime(
            &agent,
            &version,
            &source_registry,
            None,
            &cfg(),
            std::env::temp_dir(),
        )
        .await
        .err()
        .unwrap();

        assert!(err.to_string().contains("requires skills"));
    }
}

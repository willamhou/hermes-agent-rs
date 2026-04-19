//! Single-message (non-interactive) mode.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use hermes_agent::{
    compressor::CompressionConfig,
    loop_runner::{Agent, AgentConfig},
};
use hermes_config::config::{AppConfig, hermes_home};
use hermes_core::{message::Message, stream::StreamDelta, tool::ApprovalRequest};
use hermes_memory::MemoryManager;
use hermes_provider::create_provider;
use hermes_skills::SkillManager;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

use crate::approval::{ApprovalManager, is_interactive_terminal};
use crate::render::render_stream;
use crate::tooling::build_registry;

/// Send a single message, stream the response, then exit.
pub async fn run_oneshot(
    message: &str,
    model_override: Option<&str>,
    base_url_override: Option<&str>,
) -> Result<()> {
    let mut config = AppConfig::load();
    if let Some(m) = model_override {
        config.model = m.to_string();
    }
    let model = &config.model;

    let api_key = config.api_key().with_context(|| {
        format!(
            "No API key found. Set ANTHROPIC_API_KEY, OPENAI_API_KEY, or HERMES_API_KEY.\n\
             Configured model: {model}"
        )
    })?;

    let provider =
        create_provider(model, api_key, base_url_override).context("failed to create provider")?;

    let registry = build_registry(&config).await;
    let working_dir = std::env::current_dir().context("failed to get current directory")?;

    let tool_config = Arc::new(config.tool_config(working_dir.clone()));

    let approval_manager = ApprovalManager::load_or_default();
    let (approval_tx, approval_rx) = mpsc::channel::<ApprovalRequest>(8);
    approval_manager.spawn_handler(
        approval_rx,
        config.approval.policy.clone(),
        is_interactive_terminal(),
    );

    let memory_dir = hermes_home().join("memories");
    let memory = MemoryManager::new(memory_dir, None).context("failed to initialize memory")?;
    let skills_dir = hermes_home().join("skills");
    let skills = Arc::new(RwLock::new(
        SkillManager::new(vec![skills_dir]).context("failed to initialize skills")?,
    ));

    let agent_config = AgentConfig {
        provider,
        registry,
        max_iterations: config.max_iterations,
        system_prompt: "You are Hermes, a helpful AI assistant. Be concise.".to_string(),
        session_id: Uuid::new_v4().to_string(),
        working_dir,
        approval_tx,
        tool_config,
        memory,
        skills: Some(skills),
        compression: CompressionConfig::default(),
        delegation_depth: 0,
        clarify_tx: None,
    };

    let mut agent = Agent::new(agent_config);
    let mut history: Vec<Message> = Vec::new();

    let (delta_tx, delta_rx) = mpsc::channel::<StreamDelta>(64);
    let render_handle = tokio::spawn(render_stream(delta_rx));

    let result = agent
        .run_conversation(message, &mut history, delta_tx)
        .await;
    let _ = render_handle.await;

    if let Err(e) = result {
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }

    Ok(())
}

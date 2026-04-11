//! Interactive REPL loop for the Hermes CLI.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use hermes_agent::loop_runner::{Agent, AgentConfig};
use hermes_config::{
    SqliteSessionStore,
    config::{AppConfig, hermes_home},
};
use hermes_core::{
    message::Message,
    session::{SessionMeta, SessionStore as _},
    stream::StreamDelta,
    tool::{ApprovalDecision, ApprovalRequest},
};
use hermes_provider::create_provider;
use hermes_tools::registry::ToolRegistry;
use secrecy::SecretString;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::render::render_stream;

/// Start the interactive REPL.
pub async fn run_repl() -> Result<()> {
    // ── Configuration ────────────────────────────────────────────────────────
    let config = AppConfig::load();
    let api_key = config.api_key().with_context(|| {
        format!(
            "No API key found. Set ANTHROPIC_API_KEY, OPENAI_API_KEY, or HERMES_API_KEY.\n\
             Configured model: {}",
            config.model
        )
    })?;

    // ── Provider + tools ─────────────────────────────────────────────────────
    let provider = create_provider(&config.model, SecretString::new(api_key.into()), None)
        .context("failed to create provider")?;
    let registry = Arc::new(ToolRegistry::from_inventory());

    // ── Agent ────────────────────────────────────────────────────────────────
    let session_id = Uuid::new_v4().to_string();
    let working_dir = std::env::current_dir().context("failed to get current directory")?;
    let tool_config = Arc::new(config.tool_config(working_dir.clone()));

    let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalRequest>(8);
    tokio::spawn(async move {
        while let Some(req) = approval_rx.recv().await {
            tracing::warn!(tool = %req.tool_name, command = %req.command, "auto-allowing tool (no approval UI)");
            let _ = req.response_tx.send(ApprovalDecision::Allow);
        }
    });

    let system_prompt = "You are Hermes, a helpful AI assistant.".to_string();

    let agent_config = AgentConfig {
        provider,
        registry,
        max_iterations: config.max_iterations,
        system_prompt: system_prompt.clone(),
        session_id: session_id.clone(),
        working_dir: working_dir.clone(),
        approval_tx,
        tool_config,
    };
    let mut agent = Agent::new(agent_config);
    let mut history: Vec<Message> = Vec::new();

    // ── Session store ─────────────────────────────────────────────────────────
    let store = SqliteSessionStore::open()
        .await
        .context("failed to open session store")?;

    let meta = SessionMeta {
        id: session_id.clone(),
        source: "cli".to_string(),
        model: config.model.clone(),
        system_prompt,
        cwd: working_dir.to_string_lossy().to_string(),
        started_at: chrono::Utc::now().to_rfc3339(),
        ended_at: None,
        message_count: 0,
        tool_call_count: 0,
        input_tokens: 0,
        output_tokens: 0,
        title: None,
    };
    let _ = store.create_session(&meta).await;

    // ── Banner ───────────────────────────────────────────────────────────────
    println!("Hermes — model: {}", config.model);
    println!("Session: {session_id}");
    println!("Type /help for commands, /quit to exit.\n");

    repl_loop(&mut agent, &mut history, &store, &session_id, &config).await?;

    let _ = store.end_session(&session_id).await;

    Ok(())
}

/// Resume an existing session by ID, or the most recent session if `resume_id` is None.
pub async fn run_repl_with_resume(resume_id: Option<String>) -> Result<()> {
    let store = SqliteSessionStore::open()
        .await
        .context("failed to open session store")?;

    // Find session to resume
    let session_id = if let Some(id) = resume_id {
        id
    } else {
        let sessions = store
            .list_sessions(1)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        sessions
            .first()
            .map(|s| s.id.clone())
            .ok_or_else(|| anyhow::anyhow!("no sessions to resume"))?
    };

    let meta = store
        .get_session(&session_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .ok_or_else(|| anyhow::anyhow!("session {session_id} not found"))?;

    // Load history
    let mut history = store
        .load_history(&session_id)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!(
        "Resuming session {} ({} messages)",
        session_id,
        history.len()
    );

    // ── Build agent with session's model ──────────────────────────────────────
    let config = AppConfig::load();
    let model = meta.model.clone();
    let api_key = config.api_key().with_context(|| "No API key")?;
    let provider = create_provider(&model, SecretString::new(api_key.into()), None)
        .context("failed to create provider")?;
    let registry = Arc::new(ToolRegistry::from_inventory());
    let working_dir = PathBuf::from(&meta.cwd);
    let tool_config = Arc::new(config.tool_config(working_dir.clone()));

    let (approval_tx, mut approval_rx) = mpsc::channel::<ApprovalRequest>(8);
    tokio::spawn(async move {
        while let Some(req) = approval_rx.recv().await {
            tracing::warn!(tool = %req.tool_name, "auto-allowing tool");
            let _ = req.response_tx.send(ApprovalDecision::Allow);
        }
    });

    let mut agent = Agent::new(AgentConfig {
        provider,
        registry,
        max_iterations: config.max_iterations,
        system_prompt: meta.system_prompt.clone(),
        session_id: session_id.clone(),
        working_dir,
        approval_tx,
        tool_config,
    });

    println!("Hermes — model: {model}");
    println!("Type /help for commands, /quit to exit.\n");

    repl_loop(&mut agent, &mut history, &store, &session_id, &config).await?;

    let _ = store.end_session(&session_id).await;

    Ok(())
}

/// Core REPL loop shared by `run_repl` and `run_repl_with_resume`.
async fn repl_loop(
    agent: &mut Agent,
    history: &mut Vec<Message>,
    store: &SqliteSessionStore,
    session_id: &str,
    _config: &AppConfig,
) -> Result<()> {
    // ── Readline channel ─────────────────────────────────────────────────────
    let (input_tx, mut input_rx) = mpsc::channel::<String>(32);

    let history_path = hermes_home().join("cli_history.txt");
    let history_path_clone = history_path.clone();

    tokio::task::spawn_blocking(move || {
        let mut rl = match rustyline::DefaultEditor::new() {
            Ok(editor) => editor,
            Err(e) => {
                eprintln!("Failed to initialise readline: {e}");
                return;
            }
        };

        // Load history (ignore errors — file may not exist yet).
        let _ = rl.load_history(&history_path_clone);

        loop {
            match rl.readline("> ") {
                Ok(line) => {
                    let trimmed = line.trim().to_string();
                    if !trimmed.is_empty() {
                        rl.add_history_entry(&trimmed).ok();
                    }
                    if input_tx.blocking_send(trimmed).is_err() {
                        // Receiver dropped; the async side has exited.
                        break;
                    }
                }
                Err(rustyline::error::ReadlineError::Interrupted)
                | Err(rustyline::error::ReadlineError::Eof) => {
                    let _ = input_tx.blocking_send("/quit".to_string());
                    break;
                }
                Err(_) => break,
            }
        }

        let _ = rl.save_history(&history_path_clone);
    });

    // ── Main async loop ───────────────────────────────────────────────────────
    while let Some(input) = input_rx.recv().await {
        match input.as_str() {
            "" => continue,
            "/quit" | "/exit" => {
                println!("Goodbye.");
                break;
            }
            "/new" => {
                // End current session and start fresh (caller will create new session if needed)
                let _ = store.end_session(session_id).await;
                history.clear();
                println!("Conversation reset.");
                continue;
            }
            "/help" => {
                println!("/quit — exit");
                println!("/new  — reset conversation history");
                println!("/help — show this message");
                continue;
            }
            _ => {}
        }

        // Regular user message — persist user message, then stream the response.
        let user_msg = Message::user(&input);
        let _ = store.append_message(session_id, &user_msg).await;

        // Track how many messages exist before this turn so we can persist new ones.
        let pre_len = history.len();

        let (delta_tx, delta_rx) = mpsc::channel::<StreamDelta>(64);
        let render_handle = tokio::spawn(render_stream(delta_rx));

        let result = agent.run_conversation(&input, history, delta_tx).await;

        // Await renderer (it terminates when the sender is dropped, which
        // happens as `delta_tx` goes out of scope above when run_conversation
        // returns; drop explicitly to be clear).
        let _ = render_handle.await;

        if let Err(e) = result {
            eprintln!("Error: {e:#}");
        } else {
            // Persist all messages added during this turn, skipping the user
            // message at pre_len (already persisted above).
            for msg in &history[pre_len + 1..] {
                let _ = store.append_message(session_id, msg).await;
            }
        }
    }

    // Ensure the home directory exists so subsequent runs can load readline history.
    if let Some(parent) = history_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    Ok(())
}

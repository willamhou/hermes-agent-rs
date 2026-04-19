//! Interactive REPL loop for the Hermes CLI.

use std::{
    path::PathBuf,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Context as _, Result};
use hermes_agent::{
    compressor::CompressionConfig,
    loop_runner::{Agent, AgentConfig},
};
use hermes_config::{
    SqliteSessionStore,
    config::{AppConfig, ApprovalPolicy, hermes_home},
};
use hermes_core::{
    clarify::ClarifyRequest,
    message::Message,
    session::{SessionMeta, SessionStore as _},
    stream::StreamDelta,
    tool::ApprovalRequest,
};
use hermes_memory::MemoryManager;
use hermes_provider::create_provider;
use hermes_skills::SkillManager;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

use crate::approval::{ApprovalManager, is_interactive_terminal};
use crate::render::render_stream;
use crate::tooling::build_registry;
use crate::{commands, handlers};

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
    let provider =
        create_provider(&config.model, api_key, None).context("failed to create provider")?;
    let registry = build_registry(&config).await;

    // ── Agent ────────────────────────────────────────────────────────────────
    let session_id = Uuid::new_v4().to_string();
    let working_dir = std::env::current_dir().context("failed to get current directory")?;
    let tool_config = Arc::new(config.tool_config(working_dir.clone()));

    let shared_policy = Arc::new(RwLock::new(config.approval.policy.clone()));
    let approval_manager = ApprovalManager::load_or_default();
    let (approval_tx, approval_rx) = mpsc::channel::<ApprovalRequest>(8);
    approval_manager.spawn_handler(
        approval_rx,
        Arc::clone(&shared_policy),
        is_interactive_terminal(),
    );

    let system_prompt = "You are Hermes, a helpful AI assistant.".to_string();

    let memory_dir = hermes_home().join("memories");
    let memory = MemoryManager::new(memory_dir, None).context("failed to initialize memory")?;
    let skills_dir = hermes_home().join("skills");
    let skills = Arc::new(RwLock::new(
        SkillManager::new(vec![skills_dir]).context("failed to initialize skills")?,
    ));

    let (clarify_tx, clarify_rx) = mpsc::channel::<ClarifyRequest>(4);
    let clarify_active = Arc::new(AtomicBool::new(false));
    spawn_clarify_handler(clarify_rx, Arc::clone(&clarify_active));

    let verbose = Arc::new(AtomicBool::new(false));

    let agent_config = AgentConfig {
        provider,
        registry,
        max_iterations: config.max_iterations,
        system_prompt: system_prompt.clone(),
        session_id: session_id.clone(),
        working_dir: working_dir.clone(),
        approval_tx,
        tool_config,
        memory,
        skills: Some(skills),
        compression: CompressionConfig::default(),
        delegation_depth: 0,
        clarify_tx: Some(clarify_tx),
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

    let repl_state = ReplState {
        clarify_active,
        approval_policy: shared_policy,
        verbose,
    };
    repl_loop(
        &mut agent,
        &mut history,
        &store,
        &session_id,
        &config,
        repl_state,
    )
    .await?;

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
    let provider = create_provider(&model, api_key, None).context("failed to create provider")?;
    let registry = build_registry(&config).await;
    let working_dir = PathBuf::from(&meta.cwd);
    let tool_config = Arc::new(config.tool_config(working_dir.clone()));

    let shared_policy = Arc::new(RwLock::new(config.approval.policy.clone()));
    let approval_manager = ApprovalManager::load_or_default();
    let (approval_tx, approval_rx) = mpsc::channel::<ApprovalRequest>(8);
    approval_manager.spawn_handler(
        approval_rx,
        Arc::clone(&shared_policy),
        is_interactive_terminal(),
    );

    let memory_dir = hermes_home().join("memories");
    let memory = MemoryManager::new(memory_dir, None).context("failed to initialize memory")?;
    let skills_dir = hermes_home().join("skills");
    let skills = Arc::new(RwLock::new(
        SkillManager::new(vec![skills_dir]).context("failed to initialize skills")?,
    ));

    let (clarify_tx, clarify_rx) = mpsc::channel::<ClarifyRequest>(4);
    let clarify_active = Arc::new(AtomicBool::new(false));
    spawn_clarify_handler(clarify_rx, Arc::clone(&clarify_active));

    let verbose = Arc::new(AtomicBool::new(false));

    let mut agent = Agent::new(AgentConfig {
        provider,
        registry,
        max_iterations: config.max_iterations,
        system_prompt: meta.system_prompt.clone(),
        session_id: session_id.clone(),
        working_dir,
        approval_tx,
        tool_config,
        memory,
        skills: Some(skills),
        compression: CompressionConfig::default(),
        delegation_depth: 0,
        clarify_tx: Some(clarify_tx),
    });

    println!("Hermes — model: {model}");
    println!("Type /help for commands, /quit to exit.\n");

    let repl_state = ReplState {
        clarify_active,
        approval_policy: shared_policy,
        verbose,
    };
    repl_loop(
        &mut agent,
        &mut history,
        &store,
        &session_id,
        &config,
        repl_state,
    )
    .await?;

    let _ = store.end_session(&session_id).await;

    Ok(())
}

/// Shared REPL state passed to the loop.
struct ReplState {
    clarify_active: Arc<AtomicBool>,
    approval_policy: Arc<RwLock<ApprovalPolicy>>,
    verbose: Arc<AtomicBool>,
}

/// Extract the argument portion after the first whitespace in a slash command.
fn cmd_args(input: &str) -> &str {
    input
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest)
        .unwrap_or("")
        .trim()
}

/// Core REPL loop shared by `run_repl` and `run_repl_with_resume`.
async fn repl_loop(
    agent: &mut Agent,
    history: &mut Vec<Message>,
    store: &SqliteSessionStore,
    session_id: &str,
    config: &AppConfig,
    state: ReplState,
) -> Result<()> {
    let history_path = hermes_home().join("cli_history.txt");

    // ── Main async loop ───────────────────────────────────────────────────────
    loop {
        let input =
            read_repl_input(history_path.clone(), Arc::clone(&state.clarify_active)).await?;

        if input.is_empty() {
            continue;
        }

        // ── Command dispatch ─────────────────────────────────────────────────
        if input.starts_with('/') {
            match commands::resolve_command(&input) {
                Some(cmd) => {
                    match cmd.name {
                        "quit" => {
                            println!("Goodbye.");
                            let _ = store.end_session(session_id).await;
                            break;
                        }
                        "new" => {
                            let _ = store.end_session(session_id).await;
                            history.clear();
                            println!("Conversation reset.");
                        }
                        "help" => handlers::handle_help(),
                        "clear" => handlers::handle_clear(),
                        "model" => handlers::handle_model(&config.model),
                        "tools" => handlers::handle_tools(agent.registry()),
                        "status" => handlers::handle_status(
                            session_id,
                            &config.model,
                            history,
                            agent.remaining_budget(),
                        ),
                        "retry" => {
                            if let Some(msg) = handlers::handle_retry(history) {
                                run_message(agent, history, store, session_id, &msg).await;
                            } else {
                                println!("Nothing to retry.");
                            }
                        }
                        "undo" => handlers::handle_undo(history),
                        "compress" => {
                            println!("Compressing context...");
                            match agent.manual_compress(history).await {
                                Ok(hermes_agent::compressor::CompressionResult::NotNeeded) => {
                                    println!("No compression needed.");
                                }
                                Ok(hermes_agent::compressor::CompressionResult::Compressed {
                                    before_tokens,
                                    after_tokens,
                                    ..
                                }) => {
                                    println!("Compressed: {before_tokens} → {after_tokens} tokens");
                                }
                                Err(e) => eprintln!("Compression failed: {e:#}"),
                            }
                        }
                        "skills" => handlers::handle_skills_list(),
                        "save" => {
                            let args = input.split_whitespace().nth(1);
                            handlers::handle_save(history, args);
                        }
                        "cron" => handlers::handle_cron(),
                        "config" => handlers::handle_config(config),
                        "provider" => handlers::handle_provider(&config.model),
                        "usage" => {
                            handlers::handle_usage(session_id, store).await;
                        }
                        "yolo" => {
                            handlers::handle_yolo(&state.approval_policy).await;
                        }
                        "title" => {
                            let title = cmd_args(&input);
                            if title.is_empty() {
                                println!("Usage: /title <text>");
                            } else {
                                handlers::handle_title(title, session_id, store).await;
                            }
                        }
                        "toolsets" => handlers::handle_toolsets(agent.registry()),
                        "verbose" => handlers::handle_verbose(&state.verbose),
                        "search" => {
                            let query = cmd_args(&input);
                            if query.is_empty() {
                                println!("Usage: /search <query>");
                            } else {
                                handlers::handle_search(query, store).await;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                None => {
                    println!("Unknown command. Type /help for list.");
                    continue;
                }
            }
        }

        // ── Regular user message ─────────────────────────────────────────────
        run_message(agent, history, store, session_id, &input).await;
    }

    Ok(())
}

/// Send `message` to the agent, stream the response, and persist new history entries.
///
/// Persistence happens AFTER the agent run completes so that an error mid-turn
/// leaves no orphaned messages in the DB. The trade-off is that a crash during
/// execution loses the entire turn, but that is preferable to inconsistent state.
async fn run_message(
    agent: &mut Agent,
    history: &mut Vec<Message>,
    store: &SqliteSessionStore,
    session_id: &str,
    message: &str,
) {
    let pre_len = history.len();

    let (delta_tx, delta_rx) = mpsc::channel::<StreamDelta>(64);
    let render_handle = tokio::spawn(render_stream(delta_rx));

    // run_conversation pushes the user message onto history before calling the provider.
    let result = agent.run_conversation(message, history, delta_tx).await;

    let _ = render_handle.await;

    if let Err(ref e) = result {
        eprintln!("Error: {e:#}");
        // Do not persist messages from a failed turn.
        return;
    }

    // Persist ALL messages added this turn (user message + assistant/tool turns).
    // Clamp: compression may shrink history below pre_len.
    // TODO: After compression fires mid-turn, the DB retains pre-compression messages
    // while in-memory history has been compressed. Session resume will load the full
    // uncompressed history. This is acceptable for Phase 4 — compression will re-trigger
    // naturally on the resumed session when the token count exceeds the threshold again.
    let persist_start = pre_len.min(history.len());
    for msg in &history[persist_start..] {
        let _ = store.append_message(session_id, msg).await;
    }
}

/// Spawn a task that receives `ClarifyRequest`s and prompts the user via stdin.
///
/// `clarify_active` is set to `true` while stdin is being read so that the
/// readline loop in `read_repl_input` knows to pause. Although the current
/// repl_loop design serializes readline and agent execution (so there is no
/// truly concurrent stdin read in normal flow), this guard makes the invariant
/// explicit and protects against future refactors that might introduce concurrency.
fn spawn_clarify_handler(
    mut clarify_rx: mpsc::Receiver<ClarifyRequest>,
    clarify_active: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        while let Some(req) = clarify_rx.recv().await {
            println!("\n--- Agent asks: ---");
            println!("{}", req.question);
            if !req.choices.is_empty() {
                for (i, choice) in req.choices.iter().enumerate() {
                    println!("  {}) {choice}", i + 1);
                }
                println!("  {}) Other (type your answer)", req.choices.len() + 1);
            }
            print!("> ");
            let _ = std::io::Write::flush(&mut std::io::stdout());

            let choices = req.choices.clone();
            let choices_len = choices.len();
            let flag = Arc::clone(&clarify_active);
            let answer = tokio::task::spawn_blocking(move || {
                flag.store(true, Ordering::Relaxed);
                let mut input = String::new();
                std::io::stdin().read_line(&mut input).ok();
                flag.store(false, Ordering::Relaxed);
                let trimmed = input.trim().to_string();
                // If user typed a number selecting a listed choice, expand it
                if let Ok(n) = trimmed.parse::<usize>() {
                    if n >= 1 && n <= choices_len {
                        return choices[n - 1].clone();
                    }
                }
                trimmed
            })
            .await
            .unwrap_or_default();

            let response = if answer.is_empty() {
                hermes_core::clarify::ClarifyResponse::Timeout
            } else {
                hermes_core::clarify::ClarifyResponse::Answer(answer)
            };
            let _ = req.response_tx.send(response);
        }
    });
}

async fn read_repl_input(history_path: PathBuf, clarify_active: Arc<AtomicBool>) -> Result<String> {
    tokio::task::spawn_blocking(move || -> Result<String> {
        // Wait while the clarify handler is reading stdin to avoid concurrent access.
        while clarify_active.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        let mut rl =
            rustyline::DefaultEditor::new().context("failed to initialise readline editor")?;

        let _ = rl.load_history(&history_path);

        let input = match rl.readline("> ") {
            Ok(line) => {
                let trimmed = line.trim().to_string();
                if !trimmed.is_empty() {
                    rl.add_history_entry(&trimmed).ok();
                }
                trimmed
            }
            Err(rustyline::error::ReadlineError::Interrupted)
            | Err(rustyline::error::ReadlineError::Eof) => "/quit".to_string(),
            Err(err) => return Err(anyhow::anyhow!("readline failed: {err}")),
        };

        if let Some(parent) = history_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = rl.save_history(&history_path);

        Ok(input)
    })
    .await
    .context("readline task failed")?
}

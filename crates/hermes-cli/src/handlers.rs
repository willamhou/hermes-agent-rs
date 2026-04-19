//! REPL command handler functions.

use std::sync::Arc;

use hermes_agent::token_counter::TokenCounter;
use hermes_config::config::AppConfig;
use hermes_core::message::{Message, Role};
use hermes_tools::ToolRegistry;
use tokio::sync::RwLock;

pub fn handle_help() {
    use crate::commands::COMMANDS;
    println!("\nAvailable commands:");
    for cmd in COMMANDS {
        let aliases = if cmd.aliases.is_empty() {
            String::new()
        } else {
            format!(" ({})", cmd.aliases.join(", "))
        };
        println!("  {:<14}{}", cmd.usage, aliases);
        println!("  {:<14}  {}", "", cmd.description);
    }
    println!();
}

pub fn handle_clear() {
    use crossterm::{
        cursor::MoveTo,
        execute,
        terminal::{Clear, ClearType},
    };
    let _ = execute!(std::io::stdout(), Clear(ClearType::All), MoveTo(0, 0));
}

pub fn handle_model(model: &str) {
    println!("Current model: {model}");
    println!("Use --model flag to change: hermes --model openai/gpt-4o");
}

pub fn handle_tools(registry: &ToolRegistry) {
    let mut names = registry.tool_names();
    names.sort_unstable();
    println!("\nRegistered tools ({}):", names.len());
    for name in &names {
        println!("  {name}");
    }
    println!();
}

pub fn handle_status(session_id: &str, model: &str, history: &[Message], budget_remaining: u32) {
    let token_est = TokenCounter::count_messages(history);
    println!("\nSession:          {session_id}");
    println!("Model:            {model}");
    println!("Messages:         {}", history.len());
    println!("Tokens (est):     ~{token_est}");
    println!("Budget remaining: {budget_remaining}");
    println!();
}

/// Pop the last user message and all subsequent messages, returning the user
/// message text so the caller can re-send it.
///
/// Note: `as_text_lossy()` drops non-text content (images, etc.) from multimodal
/// messages. This is acceptable for Phase 4 since the CLI does not support
/// multimodal input.
pub fn handle_retry(history: &mut Vec<Message>) -> Option<String> {
    let last_user_idx = history.iter().rposition(|m| m.role == Role::User)?;
    let user_msg = history[last_user_idx].content.as_text_lossy();
    history.truncate(last_user_idx);
    Some(user_msg)
}

/// Remove the last turn (last user message plus all following messages).
pub fn handle_undo(history: &mut Vec<Message>) {
    if let Some(last_user_idx) = history.iter().rposition(|m| m.role == Role::User) {
        let removed = history.len() - last_user_idx;
        history.truncate(last_user_idx);
        println!("Undid last turn ({removed} messages removed).");
    } else {
        println!("Nothing to undo.");
    }
}

/// Serialize history to JSON and write to `path` (default: "conversation.json").
pub fn handle_save(history: &[Message], args: Option<&str>) {
    let path = args.unwrap_or("conversation.json");
    match serde_json::to_string_pretty(history) {
        Ok(json) => match std::fs::write(path, &json) {
            Ok(()) => println!("Saved {} messages to {path}", history.len()),
            Err(e) => eprintln!("Failed to save: {e}"),
        },
        Err(e) => eprintln!("Serialization error: {e}"),
    }
}

pub fn handle_skills_list() {
    println!("Skills are automatically matched based on conversation context.");
    println!("Place skill files in ~/.hermes/skills/");
}

pub async fn handle_search(query: &str, store: &hermes_config::SqliteSessionStore) {
    match store.search_messages(query, 20).await {
        Ok(hits) if hits.is_empty() => println!("No results found."),
        Ok(hits) => {
            println!("\nSearch results ({}):\n", hits.len());
            for hit in &hits {
                let content_preview = if hit.content.chars().count() > 100 {
                    let truncated: String = hit.content.chars().take(100).collect();
                    format!("{truncated}...")
                } else {
                    hit.content.clone()
                };
                let sid_len = hit.session_id.len().min(8);
                println!(
                    "  [{}] {} | {} | {}",
                    &hit.session_id[..sid_len],
                    hit.role,
                    hit.created_at.get(..19).unwrap_or(&hit.created_at),
                    content_preview
                );
            }
            println!();
        }
        Err(e) => eprintln!("Search failed: {e}"),
    }
}

pub fn handle_config(config: &AppConfig) {
    println!("\nConfiguration:");
    println!("  model:          {}", config.model);
    println!("  max_iterations: {}", config.max_iterations);
    println!("  temperature:    {}", config.temperature);
    println!("  approval:       {:?}", config.approval.policy);
    println!("  terminal:");
    println!("    timeout:        {}s", config.terminal.timeout);
    println!("    max_timeout:    {}s", config.terminal.max_timeout);
    println!(
        "    output_max:     {} chars",
        config.terminal.output_max_chars
    );
    println!("  file:");
    println!("    read_max_chars: {}", config.file.read_max_chars);
    println!("    read_max_lines: {}", config.file.read_max_lines);
    println!("  browser:");
    println!(
        "    headless: {} | sandbox: {}",
        config.browser.headless, config.browser.sandbox
    );
    println!("  mcp_servers:    {}", config.mcp_servers.len());
    if !config.mcp_servers.is_empty() {
        for s in &config.mcp_servers {
            let status = if s.enabled { "on" } else { "off" };
            println!("    - {} ({})", s.name, status);
        }
    }
    println!(
        "\n  Config file: {}/config.yaml",
        hermes_config::config::hermes_home().display()
    );
    println!();
}

pub fn handle_provider(model: &str) {
    let parts: Vec<&str> = model.splitn(2, '/').collect();
    let (provider, model_name) = if parts.len() == 2 {
        (parts[0], parts[1])
    } else {
        ("openai", model)
    };
    println!("\nProvider: {provider}");
    println!("Model:    {model_name}");

    let key_var = match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" | "openai-codex" | "openai-responses" => "OPENAI_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        _ => "HERMES_API_KEY",
    };
    let key_status = if std::env::var(key_var).is_ok_and(|k| !k.is_empty()) {
        format!("{key_var} = ****")
    } else if std::env::var("HERMES_API_KEY").is_ok_and(|k| !k.is_empty()) {
        "HERMES_API_KEY = ****".to_string()
    } else {
        "not set".to_string()
    };
    println!("API key:  {key_status}");
    println!();
}

pub async fn handle_usage(session_id: &str, store: &hermes_config::SqliteSessionStore) {
    use hermes_core::session::SessionStore;
    match store.get_session(session_id).await {
        Ok(Some(meta)) => {
            println!(
                "\nToken usage (session {}):",
                &session_id[..session_id.len().min(8)]
            );
            println!("  Input tokens:  {}", meta.input_tokens);
            println!("  Output tokens: {}", meta.output_tokens);
            println!(
                "  Total tokens:  {}",
                meta.input_tokens + meta.output_tokens
            );
            println!("  Messages:      {}", meta.message_count);
            println!("  Tool calls:    {}", meta.tool_call_count);
            println!();
        }
        Ok(None) => println!("Session not found."),
        Err(e) => eprintln!("Failed to get usage: {e}"),
    }
}

pub async fn handle_yolo(approval_policy: &Arc<RwLock<hermes_config::config::ApprovalPolicy>>) {
    use hermes_config::config::ApprovalPolicy;
    let mut policy = approval_policy.write().await;
    *policy = match *policy {
        ApprovalPolicy::Deny => {
            println!("Cannot toggle YOLO: approval policy is set to Deny in configuration.");
            return;
        }
        ApprovalPolicy::Ask => {
            println!("YOLO mode ON — dangerous commands auto-approved.");
            println!("Toggle off with /yolo");
            ApprovalPolicy::Yolo
        }
        ApprovalPolicy::Yolo => {
            println!("YOLO mode OFF — dangerous commands require approval.");
            ApprovalPolicy::Ask
        }
    };
}

pub async fn handle_title(
    title: &str,
    session_id: &str,
    store: &hermes_config::SqliteSessionStore,
) {
    const MAX_TITLE_LEN: usize = 200;
    if title.len() > MAX_TITLE_LEN {
        println!("Title too long (max {MAX_TITLE_LEN} characters).");
        return;
    }
    if let Err(e) = store.update_title(session_id, title).await {
        eprintln!("Failed to set title: {e}");
    } else {
        println!("Session title set to: {title}");
    }
}

pub fn handle_toolsets(registry: &ToolRegistry) {
    let groups = registry.tools_by_toolset();
    println!("\nTools by category ({} total):\n", registry.len());
    for (toolset, names) in &groups {
        println!("  [{toolset}]");
        for name in names {
            println!("    {name}");
        }
    }
    println!();
}

pub fn handle_verbose(verbose: &Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    let was = verbose.fetch_xor(true, Ordering::Relaxed);
    let now = !was;
    if now {
        println!("Verbose mode ON (note: tracing level change not yet wired).");
    } else {
        println!("Verbose mode OFF.");
    }
}

pub fn handle_background(args: &str) {
    use hermes_tools::process_registry::global_registry;

    let parts: Vec<&str> = args.split_whitespace().collect();
    match parts.first().copied().unwrap_or("list") {
        "list" | "" => {
            let infos = global_registry().list();
            if infos.is_empty() {
                println!("No background processes.");
            } else {
                println!("\nBackground processes ({}):", infos.len());
                for p in &infos {
                    println!("  {} | {} | {}", p.id, p.status, p.command);
                }
                println!();
            }
        }
        "read" => {
            if let Some(id) = parts.get(1) {
                match global_registry().read_output(id) {
                    Some(output) if output.is_empty() => println!("(no output yet)"),
                    Some(output) => print!("{output}"),
                    None => println!("Process {id} not found."),
                }
            } else {
                println!("Usage: /bg read <id>");
            }
        }
        "kill" => {
            if let Some(id) = parts.get(1) {
                match global_registry().kill(id) {
                    Ok(()) => println!("Sent SIGKILL to process {id}."),
                    Err(e) => println!("{e}"),
                }
            } else {
                println!("Usage: /bg kill <id>");
            }
        }
        "clean" => {
            global_registry().remove_exited();
            println!("Removed exited processes.");
        }
        other => {
            println!("Unknown subcommand: {other}");
            println!("Usage: /bg [list|read <id>|kill <id>|clean]");
        }
    }
}

pub fn handle_cron() {
    let store_path = hermes_config::config::hermes_home()
        .join("cron")
        .join("jobs.json");
    match hermes_cron::store::JobStore::open(store_path) {
        Ok(store) => match store.list() {
            Ok(jobs) if jobs.is_empty() => {
                println!("No scheduled jobs. Use the cron tool to create one.")
            }
            Ok(jobs) => {
                println!("\nScheduled jobs ({}):", jobs.len());
                for j in &jobs {
                    let status = j.last_status.as_deref().unwrap_or("-");
                    let next = j
                        .next_run_at
                        .as_deref()
                        .map(|s| &s[..s.len().min(19)])
                        .unwrap_or("-");
                    println!("  {} | {} | {} | next: {}", j.id, j.name, status, next);
                }
                println!();
            }
            Err(e) => eprintln!("Failed to list jobs: {e}"),
        },
        Err(e) => eprintln!("Failed to open job store: {e}"),
    }
}

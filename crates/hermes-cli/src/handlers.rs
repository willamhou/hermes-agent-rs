//! REPL command handler functions.

use hermes_agent::token_counter::TokenCounter;
use hermes_core::message::{Message, Role};
use hermes_tools::ToolRegistry;

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

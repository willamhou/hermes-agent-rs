//! Hermes CLI entry point.

mod approval;
mod oneshot;
mod render;
mod repl;

use clap::Parser;
use hermes_core::session::SessionStore as _;

/// Hermes — AI agent CLI
#[derive(Parser, Debug)]
#[command(name = "hermes", about = "Interactive AI agent powered by Hermes")]
struct Cli {
    /// Send a single message and print the response (non-interactive).
    #[arg(short, long)]
    message: Option<String>,

    /// Override the model (e.g. "openai/gpt-4o").
    #[arg(long)]
    model: Option<String>,

    /// Override the provider base URL.
    #[arg(long)]
    base_url: Option<String>,

    /// Resume a previous session. Pass session ID, or omit for most recent.
    #[arg(long)]
    resume: Option<Option<String>>,

    /// List recent sessions.
    #[arg(long)]
    list_sessions: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    if cli.list_sessions {
        return list_sessions_cmd().await;
    }

    // --resume was provided (with or without value)
    if let Some(resume_id) = cli.resume {
        return repl::run_repl_with_resume(resume_id).await;
    }

    if let Some(msg) = cli.message {
        return oneshot::run_oneshot(&msg, cli.model.as_deref(), cli.base_url.as_deref()).await;
    }

    repl::run_repl().await
}

async fn list_sessions_cmd() -> anyhow::Result<()> {
    let store = hermes_config::SqliteSessionStore::open().await?;
    let sessions = store
        .list_sessions(20)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if sessions.is_empty() {
        println!("No sessions found.");
        return Ok(());
    }
    println!("{:<38} {:<20} {:<6} Model", "ID", "Started", "Msgs");
    println!("{}", "-".repeat(80));
    for s in &sessions {
        println!(
            "{:<38} {:<20} {:<6} {}",
            s.id,
            &s.started_at[..std::cmp::min(19, s.started_at.len())],
            s.message_count,
            s.model,
        );
    }
    Ok(())
}

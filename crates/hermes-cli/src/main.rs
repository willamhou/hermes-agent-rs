//! Hermes CLI entry point.

mod agents;
mod approval;
mod commands;
mod handlers;
mod oneshot;
mod render;
mod repl;
mod runs;
mod tooling;

use clap::{Parser, Subcommand};
use hermes_config::config::{AppConfig, hermes_home};
use hermes_core::session::SessionStore as _;

/// Hermes — AI agent CLI
#[derive(Parser, Debug)]
#[command(name = "hermes", about = "Interactive AI agent powered by Hermes")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

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

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the gateway server (Telegram, API)
    Gateway,
    /// Managed agent control-plane commands
    #[command(subcommand)]
    Agents(agents::AgentsAction),
    /// Managed run inspection commands
    #[command(subcommand)]
    Runs(runs::RunsAction),
    /// Cron job management
    #[command(subcommand)]
    Cron(CronAction),
}

#[derive(Subcommand, Debug)]
enum CronAction {
    /// Run one scheduler tick (execute due jobs)
    Tick,
    /// List all scheduled jobs
    List,
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

    if let Some(Commands::Gateway) = cli.command {
        return run_gateway().await;
    }

    if let Some(Commands::Agents(action)) = cli.command {
        return agents::run_agents(action).await;
    }

    if let Some(Commands::Runs(action)) = cli.command {
        return runs::run_runs(action).await;
    }

    if let Some(Commands::Cron(action)) = cli.command {
        return match action {
            CronAction::Tick => run_cron_tick().await,
            CronAction::List => run_cron_list().await,
        };
    }

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

async fn run_gateway() -> anyhow::Result<()> {
    let config = hermes_config::config::AppConfig::load();
    let gateway_config = config.gateway.clone().unwrap_or_default();

    if gateway_config.telegram.is_none() && gateway_config.api_server.is_none() {
        anyhow::bail!(
            "No gateway adapters configured.\n\
             Add 'gateway.telegram' or 'gateway.api_server' section to ~/.hermes/config.yaml"
        );
    }

    let runner = hermes_gateway::GatewayRunner::new(gateway_config, config);
    runner.run().await
}

async fn run_cron_tick() -> anyhow::Result<()> {
    let config = AppConfig::load();
    let store_path = hermes_home().join("cron").join("jobs.json");
    let store = hermes_cron::store::JobStore::open(store_path)?;
    let output_dir = hermes_home().join("cron").join("output");
    let scheduler = hermes_cron::scheduler::CronScheduler::new(store, output_dir, config);
    let results = scheduler.tick().await?;
    if results.is_empty() {
        println!("No jobs due.");
    } else {
        for r in &results {
            println!(
                "{}: {} ({}, {:.1}s)",
                r.job_id,
                r.job_name,
                r.status,
                r.duration.as_secs_f64()
            );
        }
    }
    Ok(())
}

async fn run_cron_list() -> anyhow::Result<()> {
    let store_path = hermes_home().join("cron").join("jobs.json");
    let store = hermes_cron::store::JobStore::open(store_path)?;
    let jobs = store.list()?;
    if jobs.is_empty() {
        println!("No scheduled jobs.");
        return Ok(());
    }
    println!(
        "{:<14} {:<20} {:<8} {:<20} Status",
        "ID", "Name", "Enabled", "Next Run"
    );
    println!("{}", "-".repeat(80));
    for j in &jobs {
        println!(
            "{:<14} {:<20} {:<8} {:<20} {}",
            j.id,
            &j.name[..j.name.len().min(18)],
            if j.enabled { "yes" } else { "no" },
            j.next_run_at
                .as_deref()
                .map(|s| &s[..s.len().min(19)])
                .unwrap_or("-"),
            j.last_status.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
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

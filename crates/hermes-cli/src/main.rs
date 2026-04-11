//! Hermes CLI entry point.

mod oneshot;
mod render;
mod repl;

use clap::Parser;

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

    if let Some(msg) = cli.message {
        return oneshot::run_oneshot(&msg, cli.model.as_deref(), cli.base_url.as_deref()).await;
    }

    repl::run_repl().await
}

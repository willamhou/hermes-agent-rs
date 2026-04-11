//! Hermes CLI entry point.

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

    if cli.message.is_some() {
        println!("not yet implemented");
        return Ok(());
    }

    repl::run_repl().await
}

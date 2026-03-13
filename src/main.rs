use anyhow::Result;
use clap::Parser;

mod cli;
mod engine;
mod fix_engine;
mod goose_client;
mod llm_client;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = cli::Cli::parse();

    match args.command {
        cli::Command::Analyze(opts) => cli::analyze::run(opts).await,
        cli::Command::Fix(opts) => cli::fix::run(opts).await,
        cli::Command::Serve(opts) => cli::serve::run(opts).await,
    }
}

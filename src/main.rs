use anyhow::Result;
use clap::Parser;

mod cli;

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
        cli::Command::Fix(opts) => cli::fix::run(opts).await,
        cli::Command::Plan(opts) => cli::plan::run(opts).await,
        cli::Command::Serve(opts) => cli::serve::run(opts).await,
    }
}

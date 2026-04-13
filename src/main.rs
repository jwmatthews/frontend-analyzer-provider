use anyhow::Result;
use clap::Parser;

mod cli;

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_stack_size(8 * 1024 * 1024) // 8 MiB — matches OS default, prevents stack overflow in deep AST walkers
        .enable_all()
        .build()?;
    runtime.block_on(async_main())
}

async fn async_main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = cli::Cli::parse();

    match args.command {
        cli::Command::Fix(opts) => cli::fix::run(opts).await,
        cli::Command::Serve(opts) => cli::serve::run(opts).await,
    }
}

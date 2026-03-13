use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct AnalyzeOpts {
    /// Path to the project to analyze.
    pub project: PathBuf,

    /// Path to rules directory or YAML file.
    #[arg(short, long)]
    pub rules: PathBuf,

    /// Output file path. Defaults to stdout.
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Output format.
    #[arg(long, default_value = "yaml")]
    pub output_format: OutputFormat,
}

#[derive(Clone, clap::ValueEnum)]
pub enum OutputFormat {
    Yaml,
    Json,
}

pub async fn run(opts: AnalyzeOpts) -> Result<()> {
    let project = opts.project.canonicalize()?;
    tracing::info!("Analyzing project: {}", project.display());
    tracing::info!("Loading rules from: {}", opts.rules.display());

    let output = crate::engine::run_analysis(&project, &opts.rules)?;

    let serialized = match opts.output_format {
        OutputFormat::Yaml => serde_yml::to_string(&output)?,
        OutputFormat::Json => serde_json::to_string_pretty(&output)?,
    };

    match opts.output {
        Some(path) => {
            std::fs::write(&path, &serialized)?;
            tracing::info!("Output written to: {}", path.display());
        }
        None => {
            println!("{}", serialized);
        }
    }

    // Print summary
    let total_violations: usize = output.iter().map(|rs| rs.violations.len()).sum();
    let total_incidents: usize = output
        .iter()
        .flat_map(|rs| rs.violations.values())
        .map(|v| v.incidents.len())
        .sum();
    let unmatched: usize = output.iter().map(|rs| rs.unmatched.len()).sum();

    eprintln!();
    eprintln!("Analysis complete:");
    eprintln!("  Rules matched: {}", total_violations);
    eprintln!("  Total incidents: {}", total_incidents);
    eprintln!("  Rules unmatched: {}", unmatched);

    Ok(())
}

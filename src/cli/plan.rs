use anyhow::Result;
use clap::Args;
use frontend_core::fix::StrategySources;
use frontend_fix_engine::report::{build_remediation_report, ReportBuildOptions};
use frontend_js_fix::JsFixProvider;
use std::path::PathBuf;

use super::plan_common::{build_fix_context_registry, prepare_plan_context};

#[derive(Args)]
pub struct PlanOpts {
    /// Path to the project to plan fixes for.
    pub project: PathBuf,

    /// Path to Konveyor analysis output (YAML or JSON).
    #[arg(short, long)]
    pub input: PathBuf,

    /// Output path for the remediation plan JSON.
    #[arg(short, long, default_value = "remediation-plan.json")]
    pub output: PathBuf,

    /// Only process fixes for specific rule IDs (comma-separated).
    #[arg(long)]
    pub rules: Option<String>,

    /// Path to external fix strategies JSON file.
    #[arg(long)]
    pub strategies: Option<PathBuf>,

    /// Path to rule-adjacent fix strategies JSON file.
    #[arg(long)]
    pub rules_strategies: Option<PathBuf>,

    /// Show detailed output.
    #[arg(short, long)]
    pub verbose: bool,
}

pub async fn run(opts: PlanOpts) -> Result<()> {
    let prepared = prepare_plan_context(
        &opts.project,
        &opts.input,
        opts.rules.as_deref(),
        opts.strategies.as_deref(),
        opts.rules_strategies.as_deref(),
    )?;

    eprintln!(
        "Loaded {} violations with {} incidents",
        prepared.total_violations, prepared.total_incidents
    );
    if prepared.total_errors > 0 {
        eprintln!("Provider errors: {}", prepared.total_errors);
    }

    let context_registry = build_fix_context_registry();
    let fix_context = context_registry.get(&prepared.selected_ruleset_name);
    let lang = JsFixProvider::new();
    let output_path = resolve_output_path(&opts.output)?;

    let report = build_remediation_report(
        &prepared.plan,
        &prepared.analysis,
        &prepared.project_root,
        &prepared.merged_strategies,
        &lang,
        fix_context,
        &ReportBuildOptions {
            analysis_input: opts.input.clone(),
            output_path: output_path.clone(),
            rules_filter: prepared.rules_filter.clone(),
            strategy_sources: StrategySources {
                rules_strategies: opts.rules_strategies.clone(),
                external_strategies: opts.strategies.clone(),
            },
            strategy_origins: prepared.strategy_origins.clone(),
        },
    )?;

    let json = serde_json::to_string_pretty(&report)?;
    std::fs::write(&output_path, json)?;

    eprintln!(
        "Wrote remediation plan to {}",
        output_path.as_path().display()
    );
    eprintln!(
        "  Pattern-based: {} fixes ({} edits) across {} files",
        report.summary.deterministic_fix_count,
        report.summary.deterministic_edit_count,
        report.summary.files_with_deterministic_edits
    );
    eprintln!(
        "  Manual review: {} items",
        report.summary.manual_item_count
    );
    eprintln!("  LLM-assisted:  {} items", report.summary.llm_item_count);

    if opts.verbose {
        eprintln!(
            "  Goose batches: {} | OpenAI previews: {}",
            report.llm_plan.goose_batches.len(),
            report.llm_plan.openai_requests.len()
        );
    }

    Ok(())
}

fn resolve_output_path(path: &std::path::Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::{Cli, Command};
    use clap::Parser;
    use std::path::PathBuf;

    #[test]
    fn test_plan_default_output_path() {
        let cli = Cli::parse_from(["bin", "plan", "/tmp/project", "--input", "analysis.json"]);
        match cli.command {
            Command::Plan(opts) => {
                assert_eq!(opts.output, PathBuf::from("remediation-plan.json"));
            }
            _ => panic!("expected plan command"),
        }
    }

    #[test]
    fn test_plan_output_override() {
        let cli = Cli::parse_from([
            "bin",
            "plan",
            "/tmp/project",
            "--input",
            "analysis.json",
            "--output",
            "custom.json",
        ]);
        match cli.command {
            Command::Plan(opts) => {
                assert_eq!(opts.output, PathBuf::from("custom.json"));
            }
            _ => panic!("expected plan command"),
        }
    }
}

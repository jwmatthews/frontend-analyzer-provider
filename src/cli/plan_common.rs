use anyhow::{anyhow, Result};
use frontend_core::fix::{FixStrategy, StrategyResolutionSource};
use frontend_fix_engine::engine as fix_engine;
use frontend_fix_engine::registry::FixContextRegistry;
use frontend_js_fix::JsFixProvider;
use patternfly_fix_context::PatternFlyV5ToV6Context;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub struct PreparedPlanContext {
    pub project_root: PathBuf,
    pub analysis: Vec<konveyor_core::report::RuleSet>,
    pub merged_strategies: BTreeMap<String, FixStrategy>,
    pub strategy_origins: BTreeMap<String, StrategyResolutionSource>,
    pub selected_ruleset_name: String,
    pub plan: frontend_core::fix::FixPlan,
    pub total_violations: usize,
    pub total_incidents: usize,
    pub total_errors: usize,
    pub rules_filter: Option<Vec<String>>,
}

pub fn build_fix_context_registry() -> FixContextRegistry {
    let mut context_registry = FixContextRegistry::new();
    context_registry.register(Box::new(PatternFlyV5ToV6Context::new()));
    context_registry
}

pub fn prepare_plan_context(
    project: &Path,
    input: &Path,
    rules: Option<&str>,
    strategies: Option<&Path>,
    rules_strategies: Option<&Path>,
) -> Result<PreparedPlanContext> {
    let project_root = project.canonicalize()?;
    let input_content = std::fs::read_to_string(input)?;

    let parsed_output: Vec<konveyor_core::report::RuleSet> = {
        let trimmed = input_content.trim_start();
        if trimmed.starts_with('[') || trimmed.starts_with('{') {
            serde_json::from_str(&input_content)?
        } else {
            yaml_serde::from_str::<Vec<konveyor_core::report::RuleSet>>(&input_content)?
        }
    };

    let rules_filter = parse_rules_filter(rules);
    let analysis = filter_rulesets(parsed_output, rules_filter.as_deref());

    let total_violations: usize = analysis.iter().map(|rs| rs.violations.len()).sum();
    let total_incidents: usize = analysis
        .iter()
        .flat_map(|rs| rs.violations.values())
        .map(|v| v.incidents.len())
        .sum();
    let total_errors: usize = analysis.iter().map(|rs| rs.errors.len()).sum();

    let mut merged_strategies = BTreeMap::new();
    let mut strategy_origins = BTreeMap::new();

    if let Some(path) = rules_strategies {
        let strats = frontend_core::fix::load_strategies_from_json(path).map_err(|err| {
            anyhow!(
                "failed to load rule-adjacent strategies from {}: {}",
                path.display(),
                err
            )
        })?;
        for (rule_id, strategy) in strats {
            merged_strategies.insert(rule_id.clone(), strategy);
            strategy_origins.insert(rule_id, StrategyResolutionSource::ExplicitRulesStrategies);
        }
    }

    if let Some(path) = strategies {
        let strats = frontend_core::fix::load_strategies_from_json(path).map_err(|err| {
            anyhow!(
                "failed to load external strategies from {}: {}",
                path.display(),
                err
            )
        })?;
        for (rule_id, strategy) in strats {
            merged_strategies.insert(rule_id.clone(), strategy);
            strategy_origins.insert(
                rule_id,
                StrategyResolutionSource::ExplicitExternalStrategies,
            );
        }
    }

    let lang = JsFixProvider::new();
    let plan = fix_engine::plan_fixes(&analysis, &project_root, &merged_strategies, &lang)?;
    let selected_ruleset_name = analysis
        .first()
        .map(|rs| rs.name.clone())
        .unwrap_or_default();

    Ok(PreparedPlanContext {
        project_root,
        analysis,
        merged_strategies,
        strategy_origins,
        selected_ruleset_name,
        plan,
        total_violations,
        total_incidents,
        total_errors,
        rules_filter,
    })
}

fn parse_rules_filter(rules: Option<&str>) -> Option<Vec<String>> {
    let values = rules
        .map(|csv| {
            csv.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn filter_rulesets(
    output: Vec<konveyor_core::report::RuleSet>,
    rules_filter: Option<&[String]>,
) -> Vec<konveyor_core::report::RuleSet> {
    let Some(filter) = rules_filter else {
        return output;
    };

    let allowed: BTreeSet<&str> = filter.iter().map(String::as_str).collect();
    output
        .into_iter()
        .map(|mut ruleset| {
            ruleset
                .violations
                .retain(|rule_id, _| allowed.contains(rule_id.as_str()));
            ruleset
                .insights
                .retain(|rule_id, _| allowed.contains(rule_id.as_str()));
            ruleset
                .errors
                .retain(|rule_id, _| allowed.contains(rule_id.as_str()));
            ruleset
                .unmatched
                .retain(|rule_id| allowed.contains(rule_id.as_str()));
            ruleset
                .skipped
                .retain(|rule_id| allowed.contains(rule_id.as_str()));
            ruleset
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use frontend_core::incident::Incident;
    use frontend_core::report::{RuleSet, Violation};
    use std::fs;

    fn write_analysis(path: &Path, rulesets: &[RuleSet]) {
        fs::write(path, serde_json::to_vec_pretty(rulesets).unwrap()).unwrap();
    }

    fn make_test_ruleset(file_path: &Path) -> RuleSet {
        let incident = Incident {
            file_uri: format!("file://{}", file_path.display()),
            line_number: Some(1),
            code_location: None,
            message: "test incident".to_string(),
            code_snip: Some("1  const view = <Chip />;".to_string()),
            variables: BTreeMap::new(),
            effort: None,
            links: Vec::new(),
            is_dependency_incident: false,
        };

        RuleSet {
            name: "patternfly-v5-to-v6".to_string(),
            description: "test ruleset".to_string(),
            tags: Vec::new(),
            violations: BTreeMap::from([(
                "rule-a".to_string(),
                Violation {
                    description: "test violation".to_string(),
                    category: Some(frontend_core::report::Category::Mandatory),
                    labels: vec!["change-type=dom-structure".to_string()],
                    incidents: vec![incident],
                    links: Vec::new(),
                    effort: Some(1),
                },
            )]),
            insights: BTreeMap::new(),
            errors: BTreeMap::new(),
            unmatched: Vec::new(),
            skipped: Vec::new(),
        }
    }

    #[test]
    fn test_parse_rules_filter_none() {
        assert!(parse_rules_filter(None).is_none());
    }

    #[test]
    fn test_parse_rules_filter_csv() {
        let filter = parse_rules_filter(Some("a,b , c")).unwrap();
        assert_eq!(filter, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_prepare_plan_context_errors_on_invalid_rules_strategies() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        fs::create_dir_all(project_root.join("src")).unwrap();
        let file_path = project_root.join("src/App.tsx");
        fs::write(&file_path, "const view = <Chip />;\n").unwrap();

        let analysis_path = dir.path().join("analysis.json");
        write_analysis(&analysis_path, &[make_test_ruleset(&file_path)]);

        let invalid_rules_strategies = dir.path().join("rules-strategies.json");
        fs::write(&invalid_rules_strategies, "{not-valid-json").unwrap();

        let err = match prepare_plan_context(
            &project_root,
            &analysis_path,
            None,
            None,
            Some(&invalid_rules_strategies),
        ) {
            Ok(_) => panic!("expected invalid rules strategies to fail"),
            Err(err) => err,
        };

        let msg = err.to_string();
        assert!(msg.contains("failed to load rule-adjacent strategies"));
        assert!(msg.contains(invalid_rules_strategies.to_str().unwrap()));
    }

    #[test]
    fn test_prepare_plan_context_errors_on_invalid_external_strategies() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        fs::create_dir_all(project_root.join("src")).unwrap();
        let file_path = project_root.join("src/App.tsx");
        fs::write(&file_path, "const view = <Chip />;\n").unwrap();

        let analysis_path = dir.path().join("analysis.json");
        write_analysis(&analysis_path, &[make_test_ruleset(&file_path)]);

        let invalid_external_strategies = dir.path().join("fix-strategies.json");
        fs::write(&invalid_external_strategies, "{not-valid-json").unwrap();

        let err = match prepare_plan_context(
            &project_root,
            &analysis_path,
            None,
            Some(&invalid_external_strategies),
            None,
        ) {
            Ok(_) => panic!("expected invalid external strategies to fail"),
            Err(err) => err,
        };

        let msg = err.to_string();
        assert!(msg.contains("failed to load external strategies"));
        assert!(msg.contains(invalid_external_strategies.to_str().unwrap()));
    }
}

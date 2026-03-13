//! Analysis engine that ties together rule loading, scanning, and output generation.

use anyhow::Result;
use frontend_core::capabilities::ProviderCondition;
use frontend_core::incident::Incident;
use frontend_core::report::*;
use frontend_core::rules::{self, LoadedRuleSet, Rule, WhenCondition};
use regex::Regex;
use std::collections::BTreeMap;
use std::path::Path;

/// Run analysis on a project with rules loaded from disk.
pub fn run_analysis(project: &Path, rules_path: &Path) -> Result<AnalysisOutput> {
    let ruleset = rules::load_rules(rules_path)?;
    let result = evaluate_ruleset(project, &ruleset)?;
    Ok(vec![result])
}

/// Evaluate a loaded ruleset against a project.
fn evaluate_ruleset(project: &Path, ruleset: &LoadedRuleSet) -> Result<RuleSet> {
    let mut violations = BTreeMap::new();
    let mut unmatched = Vec::new();
    let mut errors = BTreeMap::new();

    for rule in &ruleset.rules {
        tracing::debug!("Evaluating rule: {}", rule.rule_id);

        match evaluate_rule(project, rule) {
            Ok(incidents) => {
                if incidents.is_empty() {
                    unmatched.push(rule.rule_id.clone());
                } else {
                    let violation = Violation {
                        description: rule.description.clone(),
                        category: rule.category.as_deref().and_then(|c| match c {
                            "mandatory" => Some(Category::Mandatory),
                            "optional" => Some(Category::Optional),
                            "potential" => Some(Category::Potential),
                            _ => None,
                        }),
                        labels: rule.labels.clone(),
                        incidents: incidents
                            .iter()
                            .map(|inc| ViolationIncident {
                                uri: inc.file_uri.clone(),
                                message: rule.message.clone(),
                                code_snip: inc.code_snip.clone(),
                                line_number: Some(inc.line_number),
                                variables: inc.variables.clone(),
                            })
                            .collect(),
                        links: rule
                            .links
                            .iter()
                            .map(|l| Link {
                                url: l.url.clone(),
                                title: Some(l.title.clone()),
                            })
                            .collect(),
                        effort: rule.effort,
                    };
                    violations.insert(rule.rule_id.clone(), violation);
                }
            }
            Err(e) => {
                tracing::error!("Rule {} failed: {}", rule.rule_id, e);
                errors.insert(rule.rule_id.clone(), e.to_string());
            }
        }
    }

    Ok(RuleSet {
        name: ruleset.meta.name.clone(),
        description: ruleset.meta.description.clone(),
        tags: Vec::new(),
        violations,
        insights: BTreeMap::new(),
        errors,
        unmatched,
        skipped: Vec::new(),
    })
}

/// Evaluate a single rule against a project.
fn evaluate_rule(project: &Path, rule: &Rule) -> Result<Vec<Incident>> {
    evaluate_when(project, &rule.when)
}

/// Evaluate a `when` condition.
fn evaluate_when(project: &Path, when: &WhenCondition) -> Result<Vec<Incident>> {
    match when {
        WhenCondition::Provider(provider_when) => {
            let mut all_incidents = Vec::new();
            for (provider, capability, condition_value) in provider_when.parse_conditions() {
                if provider != "frontend" {
                    tracing::debug!(
                        "Skipping non-frontend provider condition: {}.{} (handled by kantra builtin provider)",
                        provider,
                        capability
                    );
                    continue;
                }

                let condition_yaml = serde_yml::to_string(condition_value)?;
                let condition = ProviderCondition::parse(capability, &condition_yaml)?;
                let incidents = evaluate_provider_condition(project, condition)?;
                all_incidents.extend(incidents);
            }
            Ok(all_incidents)
        }
        WhenCondition::And { and } => {
            // All conditions must match. Return incidents from the last matching condition.
            let mut last_incidents = Vec::new();
            for cond in and {
                let incidents = evaluate_when(project, cond)?;
                if incidents.is_empty() {
                    return Ok(Vec::new()); // AND fails
                }
                last_incidents = incidents;
            }
            Ok(last_incidents)
        }
        WhenCondition::Or { or } => {
            // Any condition matching is sufficient. Return all incidents.
            let mut all_incidents = Vec::new();
            for cond in or {
                let incidents = evaluate_when(project, cond)?;
                all_incidents.extend(incidents);
            }
            Ok(all_incidents)
        }
    }
}

/// Evaluate a provider condition and return incidents.
fn evaluate_provider_condition(
    project: &Path,
    condition: ProviderCondition,
) -> Result<Vec<Incident>> {
    match condition {
        ProviderCondition::Referenced(cond) => {
            let files =
                frontend_js_scanner::scanner::collect_files(project, cond.file_pattern.as_deref())?;
            let mut incidents = Vec::new();
            for file in files {
                let result =
                    frontend_js_scanner::scanner::scan_file_referenced(&file, project, &cond)?;
                incidents.extend(result);
            }
            Ok(incidents)
        }
        ProviderCondition::CssClass(cond) => {
            let pattern = Regex::new(&cond.pattern)?;
            let mut incidents = Vec::new();

            let css_files = frontend_css_scanner::scanner::collect_css_files(
                project,
                cond.file_pattern.as_deref(),
            )?;
            for file in &css_files {
                let result =
                    frontend_css_scanner::scanner::scan_css_file_classes(file, project, &pattern)?;
                incidents.extend(result);
            }

            let js_files =
                frontend_js_scanner::scanner::collect_files(project, cond.file_pattern.as_deref())?;
            for file in &js_files {
                let result =
                    frontend_js_scanner::scanner::scan_file_classnames(file, project, &pattern)?;
                incidents.extend(result);
            }

            Ok(incidents)
        }
        ProviderCondition::CssVar(cond) => {
            let pattern = Regex::new(&cond.pattern)?;
            let mut incidents = Vec::new();

            let css_files = frontend_css_scanner::scanner::collect_css_files(
                project,
                cond.file_pattern.as_deref(),
            )?;
            for file in &css_files {
                let result =
                    frontend_css_scanner::scanner::scan_css_file_vars(file, project, &pattern)?;
                incidents.extend(result);
            }

            let js_files =
                frontend_js_scanner::scanner::collect_files(project, cond.file_pattern.as_deref())?;
            for file in &js_files {
                let result =
                    frontend_js_scanner::scanner::scan_file_css_vars(file, project, &pattern)?;
                incidents.extend(result);
            }

            Ok(incidents)
        }
        ProviderCondition::Dependency(cond) => {
            frontend_js_scanner::dependency::check_dependencies(project, &cond)
        }
    }
}

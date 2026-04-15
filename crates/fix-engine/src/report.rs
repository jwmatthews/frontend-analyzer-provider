//! Structured remediation report generation.
//!
//! Builds a non-mutating artifact from Konveyor analysis output plus the
//! fix-engine's planning logic.

use crate::context::FixContext;
use crate::engine::{
    fix_strategy_name, infer_strategy_from_labels, plan_incident_action, uri_to_path,
    PlannedIncidentAction,
};
use crate::goose_client::build_goose_plan_batches;
use crate::language::LanguageFixProvider;
use crate::llm_client::build_openai_plan_request;
use anyhow::Result;
use chrono::Utc;
use frontend_core::fix::{
    FixPlan, IncidentSnapshot, LlmPlanPreview, ManualReason, ProviderErrorSummary, RemediationFile,
    RemediationItem, RemediationKind, RemediationPlanReport, RemediationSummary, RuleSummary,
    StrategyResolution, StrategyResolutionSource, StrategySources,
};
use frontend_core::report::RuleSet;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// Options that control report content and provenance metadata.
#[derive(Debug, Clone)]
pub struct ReportBuildOptions {
    pub analysis_input: PathBuf,
    pub output_path: PathBuf,
    pub rules_filter: Option<Vec<String>>,
    pub strategy_sources: StrategySources,
    pub strategy_origins: BTreeMap<String, StrategyResolutionSource>,
}

#[derive(Debug, Default)]
struct FileReportBuilder {
    file_uri: String,
    items: Vec<RemediationItem>,
}

#[derive(Debug)]
struct CachedFileInfo {
    exists: bool,
    size_bytes: Option<u64>,
    line_count: Option<u32>,
    sha256_before: Option<String>,
    source_before: Option<String>,
}

#[derive(Debug, Default)]
struct RuleSummaryBuilder {
    ruleset_name: String,
    rule_id: String,
    rule_description: String,
    rule_category: Option<frontend_core::report::Category>,
    rule_labels: Vec<String>,
    incident_count: usize,
    pattern_item_count: usize,
    llm_item_count: usize,
    manual_item_count: usize,
    files: BTreeSet<PathBuf>,
}

/// Build a rich remediation report from analysis output and planning logic.
pub fn build_remediation_report(
    plan: &FixPlan,
    output: &[RuleSet],
    project_root: &Path,
    strategies: &BTreeMap<String, frontend_core::fix::FixStrategy>,
    lang: &dyn LanguageFixProvider,
    ctx: &dyn FixContext,
    options: &ReportBuildOptions,
) -> Result<RemediationPlanReport> {
    let mut by_file: BTreeMap<PathBuf, FileReportBuilder> = BTreeMap::new();
    let mut by_rule: BTreeMap<(String, String), RuleSummaryBuilder> = BTreeMap::new();
    let mut file_cache: BTreeMap<PathBuf, CachedFileInfo> = BTreeMap::new();
    let mut openai_requests = Vec::new();
    let mut llm_requests = Vec::new();

    for ruleset in output {
        for (rule_id, violation) in &ruleset.violations {
            let strategy = strategies
                .get(rule_id.as_str())
                .cloned()
                .or_else(|| infer_strategy_from_labels(&violation.labels).cloned())
                .unwrap_or(frontend_core::fix::FixStrategy::Llm);

            let strategy_source = options
                .strategy_origins
                .get(rule_id)
                .copied()
                .unwrap_or_else(|| {
                    if infer_strategy_from_labels(&violation.labels).is_some() {
                        StrategyResolutionSource::LabelInference
                    } else {
                        StrategyResolutionSource::FallbackLlm
                    }
                });

            for incident in &violation.incidents {
                let file_path = uri_to_path(&incident.file_uri, project_root);
                if lang.should_skip_path(&file_path) {
                    continue;
                }

                let Some(action) = plan_incident_action(
                    rule_id,
                    incident,
                    &strategy,
                    &file_path,
                    lang,
                    &violation.labels,
                ) else {
                    continue;
                };

                let rule_key = (ruleset.name.clone(), rule_id.clone());
                let rule_entry = by_rule
                    .entry(rule_key)
                    .or_insert_with(|| RuleSummaryBuilder {
                        ruleset_name: ruleset.name.clone(),
                        rule_id: rule_id.clone(),
                        rule_description: violation.description.clone(),
                        rule_category: violation.category.clone(),
                        rule_labels: violation.labels.clone(),
                        ..RuleSummaryBuilder::default()
                    });
                rule_entry.incident_count += 1;
                rule_entry.files.insert(file_path.clone());

                let file_entry =
                    by_file
                        .entry(file_path.clone())
                        .or_insert_with(|| FileReportBuilder {
                            file_uri: incident.file_uri.clone(),
                            ..FileReportBuilder::default()
                        });

                let cache = file_cache
                    .entry(file_path.clone())
                    .or_insert_with(|| load_file_info(&file_path));

                let incident_snapshot = IncidentSnapshot::from(incident);
                let strategy_resolution = StrategyResolution {
                    chosen_strategy: fix_strategy_name(&strategy).to_string(),
                    source: strategy_source,
                    source_detail: strategy_source_detail(
                        rule_id,
                        &violation.labels,
                        strategy_source,
                        options,
                    ),
                };

                let mut item = RemediationItem {
                    kind: RemediationKind::Manual,
                    rule_id: rule_id.clone(),
                    ruleset_name: ruleset.name.clone(),
                    rule_description: violation.description.clone(),
                    rule_category: violation.category.clone(),
                    rule_labels: violation.labels.clone(),
                    rule_links: violation.links.clone(),
                    rule_effort: violation.effort,
                    strategy_resolution,
                    incident: incident_snapshot,
                    planned_fix: None,
                    llm_request: None,
                    manual_item: None,
                    manual_reason: None,
                };

                match action {
                    PlannedIncidentAction::Pattern(fix) => {
                        item.kind = RemediationKind::Pattern;
                        item.planned_fix = Some(fix.clone());
                        rule_entry.pattern_item_count += 1;
                    }
                    PlannedIncidentAction::Manual(manual) => {
                        item.kind = RemediationKind::Manual;
                        item.manual_item = Some(manual);
                        item.manual_reason = Some(manual_reason_for(strategy_source));
                        rule_entry.manual_item_count += 1;
                    }
                    PlannedIncidentAction::Llm(mut request) => {
                        request.source = Some(cache.source_before.clone().unwrap_or_default());
                        if let Ok(preview) = build_openai_plan_request(&request, ctx) {
                            openai_requests.push(preview);
                        }
                        llm_requests.push(request.clone());
                        item.kind = RemediationKind::Llm;
                        item.llm_request = Some(request);
                        rule_entry.llm_item_count += 1;
                    }
                }

                file_entry.items.push(item);
            }
        }
    }

    let mut files = Vec::new();
    for (file_path, mut builder) in by_file {
        builder
            .items
            .sort_by_key(|item| item.incident.line_number.unwrap_or(0));

        let cache = file_cache
            .entry(file_path.clone())
            .or_insert_with(|| load_file_info(&file_path));
        let file_plan = plan.files.get(&file_path).cloned().unwrap_or_default();

        let deterministic_diff = if file_plan.is_empty() {
            None
        } else {
            let mut files_map = BTreeMap::new();
            files_map.insert(file_path.clone(), file_plan);
            let single_file_plan = frontend_core::fix::FixPlan {
                files: files_map,
                manual: Vec::new(),
                pending_llm: Vec::new(),
            };
            let diff = crate::engine::preview_fixes(&single_file_plan, lang)?;
            if diff.is_empty() {
                None
            } else {
                Some(diff)
            }
        };

        files.push(RemediationFile {
            file_path,
            file_uri: builder.file_uri,
            exists: cache.exists,
            size_bytes: cache.size_bytes,
            line_count: cache.line_count,
            sha256_before: cache.sha256_before.clone(),
            source_before: cache.source_before.clone(),
            deterministic_diff,
            items: builder.items,
        });
    }

    let by_rule = by_rule
        .into_values()
        .map(|summary| RuleSummary {
            ruleset_name: summary.ruleset_name,
            rule_id: summary.rule_id,
            rule_description: summary.rule_description,
            rule_category: summary.rule_category,
            rule_labels: summary.rule_labels,
            incident_count: summary.incident_count,
            pattern_item_count: summary.pattern_item_count,
            llm_item_count: summary.llm_item_count,
            manual_item_count: summary.manual_item_count,
            files: summary.files.into_iter().collect(),
        })
        .collect::<Vec<_>>();

    let provider_errors = collect_provider_errors(output);
    let llm_plan = LlmPlanPreview {
        openai_requests,
        goose_batches: build_goose_plan_batches(&llm_requests, ctx),
    };

    let summary = RemediationSummary {
        ruleset_count: output.len(),
        violation_count: output.iter().map(|rs| rs.violations.len()).sum(),
        incident_count: output
            .iter()
            .flat_map(|rs| rs.violations.values())
            .map(|v| v.incidents.len())
            .sum(),
        provider_error_count: provider_errors.len(),
        report_file_count: files.len(),
        files_with_deterministic_edits: plan.files.len(),
        deterministic_fix_count: plan
            .files
            .values()
            .flat_map(|fixes| fixes.iter())
            .filter(|f| f.source == frontend_core::fix::FixSource::Pattern)
            .count(),
        deterministic_edit_count: plan
            .files
            .values()
            .flat_map(|fixes| fixes.iter())
            .filter(|f| f.source == frontend_core::fix::FixSource::Pattern)
            .flat_map(|f| f.edits.iter())
            .count(),
        llm_item_count: plan.pending_llm.len(),
        manual_item_count: plan.manual.len(),
    };

    Ok(RemediationPlanReport {
        schema_version: "1".to_string(),
        generated_at_utc: Utc::now().to_rfc3339(),
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        project_root: project_root.to_path_buf(),
        analysis_input: options.analysis_input.clone(),
        output_path: options.output_path.clone(),
        ruleset_names: output.iter().map(|rs| rs.name.clone()).collect(),
        rules_filter: options.rules_filter.clone(),
        strategy_sources: options.strategy_sources.clone(),
        summary,
        provider_errors,
        files,
        by_rule,
        llm_plan,
    })
}

fn manual_reason_for(source: StrategyResolutionSource) -> ManualReason {
    match source {
        StrategyResolutionSource::ExplicitRulesStrategies
        | StrategyResolutionSource::ExplicitExternalStrategies => {
            ManualReason::ExplicitManualStrategy
        }
        StrategyResolutionSource::LabelInference | StrategyResolutionSource::FallbackLlm => {
            ManualReason::LabelInferredManual
        }
    }
}

fn strategy_source_detail(
    _rule_id: &str,
    labels: &[String],
    source: StrategyResolutionSource,
    options: &ReportBuildOptions,
) -> Option<String> {
    match source {
        StrategyResolutionSource::ExplicitRulesStrategies => options
            .strategy_sources
            .rules_strategies
            .as_ref()
            .map(|path| path.display().to_string()),
        StrategyResolutionSource::ExplicitExternalStrategies => options
            .strategy_sources
            .external_strategies
            .as_ref()
            .map(|path| path.display().to_string()),
        StrategyResolutionSource::LabelInference => labels
            .iter()
            .find(|label| infer_strategy_from_labels(std::slice::from_ref(label)).is_some())
            .cloned(),
        StrategyResolutionSource::FallbackLlm => {
            Some("no explicit or inferred strategy".to_string())
        }
    }
}

fn collect_provider_errors(output: &[RuleSet]) -> Vec<ProviderErrorSummary> {
    let mut seen = HashSet::new();
    let mut errors = Vec::new();
    for ruleset in output {
        for (rule_id, message) in &ruleset.errors {
            if seen.insert(message.clone()) {
                errors.push(ProviderErrorSummary {
                    ruleset_name: ruleset.name.clone(),
                    rule_id: Some(rule_id.clone()),
                    message: message.clone(),
                });
            }
        }
    }
    errors
}

fn load_file_info(file_path: &Path) -> CachedFileInfo {
    let Ok(metadata) = std::fs::metadata(file_path) else {
        return CachedFileInfo {
            exists: false,
            size_bytes: None,
            line_count: None,
            sha256_before: None,
            source_before: None,
        };
    };

    let Ok(source) = std::fs::read_to_string(file_path) else {
        return CachedFileInfo {
            exists: true,
            size_bytes: Some(metadata.len()),
            line_count: None,
            sha256_before: None,
            source_before: None,
        };
    };

    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    let digest = hasher.finalize();
    let mut sha = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut sha, "{byte:02x}");
    }

    CachedFileInfo {
        exists: true,
        size_bytes: Some(metadata.len()),
        line_count: Some(source.lines().count() as u32),
        sha256_before: Some(sha),
        source_before: Some(source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::GenericFixContext;
    use crate::language::LanguageFixProvider;
    use frontend_core::fix::{FixStrategy, PlannedFix, RenameMapping, StrategySources};
    use frontend_core::report::Violation;
    use konveyor_core::incident::Incident;
    use std::fs;
    use std::path::{Path, PathBuf};

    struct TestLanguageProvider;

    impl LanguageFixProvider for TestLanguageProvider {
        fn should_skip_path(&self, _path: &Path) -> bool {
            false
        }

        fn post_process_lines(&self, _lines: &mut [String]) {}

        fn plan_remove_attribute(
            &self,
            _rule_id: &str,
            _incident: &Incident,
            _file_path: &Path,
        ) -> Option<PlannedFix> {
            None
        }

        fn get_matched_text(&self, incident: &Incident) -> String {
            incident
                .variables
                .values()
                .find_map(|value| value.as_str().map(ToOwned::to_owned))
                .unwrap_or_default()
        }

        fn get_matched_text_for_rename(
            &self,
            incident: &Incident,
            mappings: &[RenameMapping],
        ) -> String {
            incident
                .variables
                .values()
                .filter_map(|value| value.as_str())
                .find(|value| mappings.iter().any(|mapping| mapping.old == *value))
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| self.get_matched_text(incident))
        }

        fn is_whole_file_rename(&self, incident: &Incident) -> bool {
            incident.variables.contains_key("importedName")
                || incident.variables.contains_key("componentName")
        }
    }

    fn write_source_file(project_root: &Path, source: &str) -> PathBuf {
        let file_path = project_root.join("src/App.tsx");
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(&file_path, source).unwrap();
        file_path
    }

    fn make_incident(
        file_path: &Path,
        line_number: u32,
        message: &str,
        code_snip: &str,
        variables: BTreeMap<String, serde_json::Value>,
    ) -> Incident {
        Incident {
            file_uri: format!("file://{}", file_path.display()),
            line_number: Some(line_number),
            code_location: None,
            message: message.to_string(),
            code_snip: Some(code_snip.to_string()),
            variables,
            effort: None,
            links: Vec::new(),
            is_dependency_incident: false,
        }
    }

    fn make_ruleset(rule_id: &str, violation: Violation) -> Vec<RuleSet> {
        vec![RuleSet {
            name: "patternfly-v5-to-v6".to_string(),
            description: String::new(),
            tags: Vec::new(),
            violations: BTreeMap::from([(rule_id.to_string(), violation)]),
            insights: BTreeMap::new(),
            errors: BTreeMap::new(),
            unmatched: Vec::new(),
            skipped: Vec::new(),
        }]
    }

    #[test]
    fn test_build_remediation_report_for_pattern_fix() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        let file_path = write_source_file(
            &project_root,
            "import { Chip } from '@patternfly/react-core';\nconst x = <Chip />;\n",
        );

        let incident = make_incident(
            &file_path,
            1,
            "Rename Chip to Label",
            "1  import { Chip } from '@patternfly/react-core';\n",
            BTreeMap::from([(
                "importedName".to_string(),
                serde_json::Value::String("Chip".to_string()),
            )]),
        );

        let output = make_ruleset(
            "pfv6-rename-chip-to-label",
            frontend_core::report::Violation {
                description: "Chip renamed".to_string(),
                category: Some(frontend_core::report::Category::Mandatory),
                labels: vec!["change-type=component-rename".to_string()],
                incidents: vec![incident],
                links: Vec::new(),
                effort: Some(1),
            },
        );

        let strategies = BTreeMap::from([(
            "pfv6-rename-chip-to-label".to_string(),
            FixStrategy::Rename(vec![RenameMapping {
                old: "Chip".to_string(),
                new: "Label".to_string(),
            }]),
        )]);
        let options = ReportBuildOptions {
            analysis_input: project_root.join("analysis.json"),
            output_path: project_root.join("remediation-plan.json"),
            rules_filter: None,
            strategy_sources: StrategySources::default(),
            strategy_origins: BTreeMap::from([(
                "pfv6-rename-chip-to-label".to_string(),
                StrategyResolutionSource::ExplicitRulesStrategies,
            )]),
        };

        let lang = TestLanguageProvider;
        let plan = crate::engine::plan_fixes(&output, &project_root, &strategies, &lang).unwrap();
        let report = build_remediation_report(
            &plan,
            &output,
            &project_root,
            &strategies,
            &lang,
            &GenericFixContext,
            &options,
        )
        .unwrap();

        assert_eq!(report.summary.deterministic_fix_count, 1);
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].items.len(), 1);
        assert!(report.files[0].deterministic_diff.is_some());
        assert_eq!(report.files[0].items[0].kind, RemediationKind::Pattern);
    }

    #[test]
    fn test_build_remediation_report_for_llm_item() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        let source = "export const App = () => <LegacyThing />;\n";
        let file_path = write_source_file(&project_root, source);

        let incident = make_incident(
            &file_path,
            1,
            "Migrate LegacyThing composition",
            "1  export const App = () => <LegacyThing />;",
            BTreeMap::new(),
        );
        let output = make_ruleset(
            "llm-rule",
            frontend_core::report::Violation {
                description: "Requires structural migration".to_string(),
                category: Some(frontend_core::report::Category::Mandatory),
                labels: Vec::new(),
                incidents: vec![incident],
                links: Vec::new(),
                effort: Some(1),
            },
        );

        let strategies = BTreeMap::from([("llm-rule".to_string(), FixStrategy::Llm)]);
        let strategy_path = project_root.join("fix-strategies.json");
        let options = ReportBuildOptions {
            analysis_input: project_root.join("analysis.json"),
            output_path: project_root.join("remediation-plan.json"),
            rules_filter: None,
            strategy_sources: StrategySources {
                rules_strategies: None,
                external_strategies: Some(strategy_path.clone()),
            },
            strategy_origins: BTreeMap::from([(
                "llm-rule".to_string(),
                StrategyResolutionSource::ExplicitExternalStrategies,
            )]),
        };

        let lang = TestLanguageProvider;
        let plan = crate::engine::plan_fixes(&output, &project_root, &strategies, &lang).unwrap();
        let report = build_remediation_report(
            &plan,
            &output,
            &project_root,
            &strategies,
            &lang,
            &GenericFixContext,
            &options,
        )
        .unwrap();

        assert_eq!(report.summary.llm_item_count, 1);
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].source_before.as_deref(), Some(source));
        assert_eq!(report.files[0].items.len(), 1);
        assert_eq!(report.files[0].items[0].kind, RemediationKind::Llm);
        assert_eq!(
            report.files[0].items[0].strategy_resolution.source,
            StrategyResolutionSource::ExplicitExternalStrategies
        );
        assert_eq!(
            report.files[0].items[0]
                .strategy_resolution
                .source_detail
                .as_deref(),
            Some(strategy_path.to_str().unwrap())
        );
        assert_eq!(report.llm_plan.openai_requests.len(), 1);
        assert_eq!(report.llm_plan.goose_batches.len(), 1);
        assert_eq!(
            report.files[0].items[0]
                .llm_request
                .as_ref()
                .and_then(|request| request.source.as_deref()),
            Some(source)
        );
        assert!(report.llm_plan.openai_requests[0]
            .user_prompt
            .contains(source));
    }

    #[test]
    fn test_build_remediation_report_for_explicit_manual_item() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        let file_path =
            write_source_file(&project_root, "export const App = () => <LegacyThing />;\n");

        let incident = make_incident(
            &file_path,
            1,
            "Manual migration required",
            "1  export const App = () => <LegacyThing />;",
            BTreeMap::new(),
        );
        let output = make_ruleset(
            "manual-rule",
            frontend_core::report::Violation {
                description: "No automatic migration".to_string(),
                category: Some(frontend_core::report::Category::Mandatory),
                labels: Vec::new(),
                incidents: vec![incident],
                links: Vec::new(),
                effort: Some(1),
            },
        );

        let strategies = BTreeMap::from([("manual-rule".to_string(), FixStrategy::Manual)]);
        let rules_strategy_path = project_root.join("rules-strategies.json");
        let options = ReportBuildOptions {
            analysis_input: project_root.join("analysis.json"),
            output_path: project_root.join("remediation-plan.json"),
            rules_filter: None,
            strategy_sources: StrategySources {
                rules_strategies: Some(rules_strategy_path.clone()),
                external_strategies: None,
            },
            strategy_origins: BTreeMap::from([(
                "manual-rule".to_string(),
                StrategyResolutionSource::ExplicitRulesStrategies,
            )]),
        };

        let lang = TestLanguageProvider;
        let plan = crate::engine::plan_fixes(&output, &project_root, &strategies, &lang).unwrap();
        let report = build_remediation_report(
            &plan,
            &output,
            &project_root,
            &strategies,
            &lang,
            &GenericFixContext,
            &options,
        )
        .unwrap();

        let item = &report.files[0].items[0];
        assert_eq!(report.summary.manual_item_count, 1);
        assert_eq!(item.kind, RemediationKind::Manual);
        assert!(item.manual_item.is_some());
        assert_eq!(
            item.manual_reason,
            Some(ManualReason::ExplicitManualStrategy)
        );
        assert_eq!(
            item.strategy_resolution.source,
            StrategyResolutionSource::ExplicitRulesStrategies
        );
        assert_eq!(
            item.strategy_resolution.source_detail.as_deref(),
            Some(rules_strategy_path.to_str().unwrap())
        );
    }

    #[test]
    fn test_build_remediation_report_for_label_inferred_manual_item() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        let file_path =
            write_source_file(&project_root, "export const App = () => <LegacyThing />;\n");

        let incident = make_incident(
            &file_path,
            1,
            "Manual migration required",
            "1  export const App = () => <LegacyThing />;",
            BTreeMap::new(),
        );
        let output = make_ruleset(
            "manual-rule",
            frontend_core::report::Violation {
                description: "Structure changed".to_string(),
                category: Some(frontend_core::report::Category::Mandatory),
                labels: vec!["change-type=dom-structure".to_string()],
                incidents: vec![incident],
                links: Vec::new(),
                effort: Some(1),
            },
        );

        let strategies = BTreeMap::new();
        let options = ReportBuildOptions {
            analysis_input: project_root.join("analysis.json"),
            output_path: project_root.join("remediation-plan.json"),
            rules_filter: None,
            strategy_sources: StrategySources::default(),
            strategy_origins: BTreeMap::new(),
        };

        let lang = TestLanguageProvider;
        let plan = crate::engine::plan_fixes(&output, &project_root, &strategies, &lang).unwrap();
        let report = build_remediation_report(
            &plan,
            &output,
            &project_root,
            &strategies,
            &lang,
            &GenericFixContext,
            &options,
        )
        .unwrap();

        let item = &report.files[0].items[0];
        assert_eq!(report.summary.manual_item_count, 1);
        assert_eq!(item.kind, RemediationKind::Manual);
        assert_eq!(item.manual_reason, Some(ManualReason::LabelInferredManual));
        assert_eq!(
            item.strategy_resolution.source,
            StrategyResolutionSource::LabelInference
        );
        assert_eq!(
            item.strategy_resolution.source_detail.as_deref(),
            Some("change-type=dom-structure")
        );
    }

    #[test]
    fn test_build_remediation_report_preserves_provider_errors() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        fs::create_dir_all(&project_root).unwrap();

        let output = vec![RuleSet {
            name: "patternfly-v5-to-v6".to_string(),
            description: String::new(),
            tags: Vec::new(),
            violations: BTreeMap::new(),
            insights: BTreeMap::new(),
            errors: BTreeMap::from([
                ("rule-a".to_string(), "Parse failed".to_string()),
                ("rule-b".to_string(), "Parse failed".to_string()),
                ("rule-c".to_string(), "Other provider error".to_string()),
            ]),
            unmatched: Vec::new(),
            skipped: Vec::new(),
        }];
        let options = ReportBuildOptions {
            analysis_input: project_root.join("analysis.json"),
            output_path: project_root.join("remediation-plan.json"),
            rules_filter: None,
            strategy_sources: StrategySources::default(),
            strategy_origins: BTreeMap::new(),
        };

        let report = build_remediation_report(
            &frontend_core::fix::FixPlan::default(),
            &output,
            &project_root,
            &BTreeMap::new(),
            &TestLanguageProvider,
            &GenericFixContext,
            &options,
        )
        .unwrap();

        let messages = report
            .provider_errors
            .iter()
            .map(|error| error.message.as_str())
            .collect::<Vec<_>>();
        assert_eq!(report.summary.provider_error_count, 2);
        assert_eq!(report.provider_errors.len(), 2);
        assert!(messages.contains(&"Parse failed"));
        assert!(messages.contains(&"Other provider error"));
        assert!(report.files.is_empty());
    }
}

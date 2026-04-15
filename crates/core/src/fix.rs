//! Fix engine types.
//!
//! Defines the data model for planned fixes: text edits grouped by file,
//! with support for pattern-based (deterministic) and LLM-assisted fixes.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// Re-export shared types from konveyor-core so existing code continues to compile.
pub use konveyor_core::fix::{
    FixConfidence, FixSource, FixStrategyEntry, MappingEntry as StrategyMappingEntry,
    MemberMappingEntry,
};

/// A single text replacement within a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEdit {
    /// 1-indexed line number where the edit applies.
    pub line: u32,
    /// The original text to find on this line.
    pub old_text: String,
    /// The replacement text.
    pub new_text: String,
    /// Rule ID that generated this fix.
    pub rule_id: String,
    /// Human-readable description of what this fix does.
    pub description: String,
    /// When true, replace ALL occurrences of old_text on this line (not just the first).
    /// Used for prefix replacements (e.g. CssVariablePrefix) where a single line
    /// may contain multiple instances of the old prefix.
    #[serde(default)]
    pub replace_all: bool,
}

/// A planned fix for a single incident.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedFix {
    /// The text edits to apply.
    pub edits: Vec<TextEdit>,
    /// Confidence level.
    pub confidence: FixConfidence,
    /// How the fix was generated.
    pub source: FixSource,
    /// The rule ID this fix addresses.
    pub rule_id: String,
    /// File URI from the incident.
    pub file_uri: String,
    /// Line number from the incident.
    pub line: u32,
    /// Description of what this fix does.
    pub description: String,
}

/// A fix plan: all planned fixes grouped by file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FixPlan {
    /// Fixes grouped by file path.
    pub files: BTreeMap<PathBuf, Vec<PlannedFix>>,
    /// Incidents that could not be auto-fixed and need manual attention.
    pub manual: Vec<ManualFixItem>,
    /// Incidents pending LLM-assisted fix.
    pub pending_llm: Vec<LlmFixRequest>,
}

/// An incident that requires manual fixing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualFixItem {
    pub rule_id: String,
    pub file_uri: String,
    pub line: u32,
    pub message: String,
    pub code_snip: Option<String>,
}

/// A request to send to the LLM for fix generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmFixRequest {
    pub rule_id: String,
    pub file_uri: String,
    pub file_path: PathBuf,
    pub line: u32,
    pub message: String,
    pub code_snip: Option<String>,
    /// The full source content of the file (for context).
    pub source: Option<String>,
    /// Labels from the violation (e.g., "family=Modal", "change-type=prop-to-child").
    /// Used to coalesce related rules into coherent migration groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
}

/// Result of applying a fix plan.
#[derive(Debug, Default)]
pub struct FixResult {
    /// Number of files modified.
    pub files_modified: usize,
    /// Number of edits applied.
    pub edits_applied: usize,
    /// Number of edits skipped (already applied or conflict).
    pub edits_skipped: usize,
    /// Errors encountered.
    pub errors: Vec<String>,
}

/// A rename mapping: old name -> new name.
/// Used for prop renames, component renames, import renames, etc.
#[derive(Debug, Clone)]
pub struct RenameMapping {
    pub old: String,
    pub new: String,
}

/// Known fix strategies keyed by rule ID.
/// Each entry defines how to transform incidents from that rule into text edits.
#[derive(Debug, Clone)]
pub enum FixStrategy {
    /// Simple text replacement: rename the matched text.
    /// The mapping is propName/componentName/importedName old -> new.
    Rename(Vec<RenameMapping>),
    /// Remove the matched prop (delete the entire attribute from the JSX tag).
    RemoveProp,
    /// Replace an import source path.
    ImportPathChange { old_path: String, new_path: String },
    /// Replace a CSS variable/class prefix.
    CssVariablePrefix {
        old_prefix: String,
        new_prefix: String,
    },
    /// Update a dependency version in package.json.
    UpdateDependency {
        package: String,
        new_version: String,
    },
    /// No auto-fix available — flag for manual review.
    Manual,
    /// Send to LLM for fix generation.
    Llm,
}

/// Structured remediation artifact written by the `plan` subcommand.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationPlanReport {
    pub schema_version: String,
    pub generated_at_utc: String,
    pub tool_version: String,
    pub project_root: PathBuf,
    pub analysis_input: PathBuf,
    pub output_path: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ruleset_names: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules_filter: Option<Vec<String>>,
    pub strategy_sources: StrategySources,
    pub summary: RemediationSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_errors: Vec<ProviderErrorSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<RemediationFile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_rule: Vec<RuleSummary>,
    pub llm_plan: LlmPlanPreview,
}

/// Paths or sources used to resolve remediation strategies.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StrategySources {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules_strategies: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_strategies: Option<PathBuf>,
}

/// Top-level counts and metrics for a remediation report.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemediationSummary {
    pub ruleset_count: usize,
    pub violation_count: usize,
    pub incident_count: usize,
    pub provider_error_count: usize,
    pub report_file_count: usize,
    pub files_with_deterministic_edits: usize,
    pub deterministic_fix_count: usize,
    pub deterministic_edit_count: usize,
    pub llm_item_count: usize,
    pub manual_item_count: usize,
}

/// Provider-side evaluation/parsing errors surfaced alongside the plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderErrorSummary {
    pub ruleset_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    pub message: String,
}

/// All remediation items known for a single file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationFile {
    pub file_path: PathBuf,
    pub file_uri: String,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256_before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deterministic_diff: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<RemediationItem>,
}

/// One actionable or informative remediation entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationItem {
    pub kind: RemediationKind,
    pub rule_id: String,
    pub ruleset_name: String,
    pub rule_description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_category: Option<crate::report::Category>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_links: Vec<crate::report::Link>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_effort: Option<i32>,
    pub strategy_resolution: StrategyResolution,
    pub incident: IncidentSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planned_fix: Option<PlannedFix>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_request: Option<LlmFixRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual_item: Option<ManualFixItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual_reason: Option<ManualReason>,
}

/// The kind of remediation work represented by an item.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemediationKind {
    Pattern,
    Llm,
    Manual,
}

/// Normalized reason explaining why an item still needs manual handling.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManualReason {
    ExplicitManualStrategy,
    LabelInferredManual,
}

/// Snapshot of the original incident evidence that produced a remediation item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentSnapshot {
    pub file_uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_number: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_location: Option<crate::incident::Location>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_snip: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<crate::incident::ExternalLink>,
    pub is_dependency_incident: bool,
}

impl From<&konveyor_core::incident::Incident> for IncidentSnapshot {
    fn from(incident: &konveyor_core::incident::Incident) -> Self {
        Self {
            file_uri: incident.file_uri.clone(),
            line_number: incident.line_number,
            code_location: incident.code_location.clone(),
            message: incident.message.clone(),
            code_snip: incident.code_snip.clone(),
            variables: incident.variables.clone(),
            effort: incident.effort,
            links: incident.links.clone(),
            is_dependency_incident: incident.is_dependency_incident,
        }
    }
}

/// How the fix strategy for an item was chosen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyResolution {
    pub chosen_strategy: String,
    pub source: StrategyResolutionSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_detail: Option<String>,
}

/// Provenance of the selected strategy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StrategyResolutionSource {
    ExplicitRulesStrategies,
    ExplicitExternalStrategies,
    LabelInference,
    FallbackLlm,
}

/// Aggregated summary of how a rule appears in the plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleSummary {
    pub ruleset_name: String,
    pub rule_id: String,
    pub rule_description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_category: Option<crate::report::Category>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_labels: Vec<String>,
    pub incident_count: usize,
    pub pattern_item_count: usize,
    pub llm_item_count: usize,
    pub manual_item_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<PathBuf>,
}

/// Non-mutating previews of the LLM requests/prompts the system can construct.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmPlanPreview {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub openai_requests: Vec<PlannedOpenAiRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub goose_batches: Vec<PlannedGooseBatch>,
}

/// OpenAI-compatible chat request preview for one LLM fix request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedOpenAiRequest {
    pub rule_id: String,
    pub file_path: PathBuf,
    pub line: u32,
    pub model: String,
    pub temperature: f32,
    pub system_prompt: String,
    pub user_prompt: String,
    pub request_json: serde_json::Value,
}

/// Goose batch prompt preview for one file/chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedGooseBatch {
    pub file_path: PathBuf,
    pub chunk_index: usize,
    pub chunk_count: usize,
    pub max_turns: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rule_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub families: Vec<String>,
    pub prompt: String,
}

/// Convert a `FixStrategyEntry` (from the shared `konveyor-core` crate) to
/// a runtime `FixStrategy`.
///
/// When `mappings` is populated (consolidated rule), builds a multi-mapping
/// `FixStrategy::Rename` or extracts multiple `RemoveProp` targets.
pub fn strategy_entry_to_fix_strategy(entry: &FixStrategyEntry) -> FixStrategy {
    match entry.strategy.as_str() {
        "Rename" => {
            let mut renames: Vec<RenameMapping> = Vec::new();
            // Collect from mappings array (consolidated rule)
            for m in &entry.mappings {
                if let (Some(from), Some(to)) = (&m.from, &m.to) {
                    renames.push(RenameMapping {
                        old: from.clone(),
                        new: to.clone(),
                    });
                }
            }
            // Fall back to top-level from/to (single-rule strategy)
            if renames.is_empty() {
                if let (Some(from), Some(to)) = (&entry.from, &entry.to) {
                    renames.push(RenameMapping {
                        old: from.clone(),
                        new: to.clone(),
                    });
                }
            }
            if renames.is_empty() {
                FixStrategy::Manual
            } else {
                FixStrategy::Rename(renames)
            }
        }
        "RemoveProp" => FixStrategy::RemoveProp,
        "CssVariablePrefix" => {
            if let (Some(from), Some(to)) = (&entry.from, &entry.to) {
                FixStrategy::CssVariablePrefix {
                    old_prefix: from.clone(),
                    new_prefix: to.clone(),
                }
            } else {
                FixStrategy::Manual
            }
        }
        "ImportPathChange" => {
            if let (Some(from), Some(to)) = (&entry.from, &entry.to) {
                FixStrategy::ImportPathChange {
                    old_path: from.clone(),
                    new_path: to.clone(),
                }
            } else {
                FixStrategy::Manual
            }
        }
        "UpdateDependency" => {
            if let (Some(package), Some(new_version)) = (&entry.package, &entry.new_version) {
                FixStrategy::UpdateDependency {
                    package: package.clone(),
                    new_version: new_version.clone(),
                }
            } else {
                FixStrategy::Manual
            }
        }
        "PropValueChange" | "PropTypeChange" => FixStrategy::Llm,
        "LlmAssisted" => FixStrategy::Llm,
        // v2 SD-pipeline strategies — these require structural JSX
        // transformations that only the LLM can handle.
        "ChildToProp"
        | "PropToChild"
        | "PropToChildren"
        | "CompositionChange"
        | "DeprecatedMigration" => FixStrategy::Llm,
        _ => FixStrategy::Manual,
    }
}

/// Load fix strategies from a JSON file.
///
/// Returns a map of rule_id -> FixStrategy.
pub fn load_strategies_from_json(
    path: &Path,
) -> Result<BTreeMap<String, FixStrategy>, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    let entries: BTreeMap<String, FixStrategyEntry> = serde_json::from_str(&content)?;
    let strategies = entries
        .iter()
        .map(|(rule_id, entry)| (rule_id.clone(), strategy_entry_to_fix_strategy(entry)))
        .collect();
    Ok(strategies)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_strategy_entry(strategy: &str) -> FixStrategyEntry {
        FixStrategyEntry::new(strategy)
    }

    #[test]
    fn test_rename_with_top_level_from_to() {
        let mut entry = make_strategy_entry("Rename");
        entry.from = Some("Chip".to_string());
        entry.to = Some("Label".to_string());

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Rename(mappings) => {
                assert_eq!(mappings.len(), 1);
                assert_eq!(mappings[0].old, "Chip");
                assert_eq!(mappings[0].new, "Label");
            }
            other => panic!("Expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn test_rename_with_mappings_array() {
        let mut entry = make_strategy_entry("Rename");
        entry.mappings = vec![
            StrategyMappingEntry {
                from: Some("Chip".to_string()),
                to: Some("Label".to_string()),
                component: None,
                prop: None,
            },
            StrategyMappingEntry {
                from: Some("ChipGroup".to_string()),
                to: Some("LabelGroup".to_string()),
                component: None,
                prop: None,
            },
        ];

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Rename(mappings) => {
                assert_eq!(mappings.len(), 2);
                assert_eq!(mappings[0].old, "Chip");
                assert_eq!(mappings[0].new, "Label");
                assert_eq!(mappings[1].old, "ChipGroup");
                assert_eq!(mappings[1].new, "LabelGroup");
            }
            other => panic!("Expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn test_rename_mappings_take_precedence_over_top_level() {
        let mut entry = make_strategy_entry("Rename");
        entry.from = Some("TopLevel".to_string());
        entry.to = Some("ShouldBeIgnored".to_string());
        entry.mappings = vec![StrategyMappingEntry {
            from: Some("FromMapping".to_string()),
            to: Some("ToMapping".to_string()),
            component: None,
            prop: None,
        }];

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Rename(mappings) => {
                assert_eq!(mappings.len(), 1);
                assert_eq!(mappings[0].old, "FromMapping");
            }
            other => panic!("Expected Rename, got {:?}", other),
        }
    }

    #[test]
    fn test_css_variable_prefix() {
        let mut entry = make_strategy_entry("CssVariablePrefix");
        entry.from = Some("pf-v5-".to_string());
        entry.to = Some("pf-v6-".to_string());

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::CssVariablePrefix {
                old_prefix,
                new_prefix,
            } => {
                assert_eq!(old_prefix, "pf-v5-");
                assert_eq!(new_prefix, "pf-v6-");
            }
            other => panic!("Expected CssVariablePrefix, got {:?}", other),
        }
    }

    #[test]
    fn test_css_variable_prefix_missing_fields_falls_to_manual() {
        let entry = make_strategy_entry("CssVariablePrefix");
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Manual => {}
            other => panic!("Expected Manual, got {:?}", other),
        }
    }

    #[test]
    fn test_import_path_change() {
        let mut entry = make_strategy_entry("ImportPathChange");
        entry.from = Some("@patternfly/react-core/deprecated".to_string());
        entry.to = Some("@patternfly/react-core".to_string());

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::ImportPathChange { old_path, new_path } => {
                assert_eq!(old_path, "@patternfly/react-core/deprecated");
                assert_eq!(new_path, "@patternfly/react-core");
            }
            other => panic!("Expected ImportPathChange, got {:?}", other),
        }
    }

    #[test]
    fn test_import_path_change_missing_fields_falls_to_manual() {
        let mut entry = make_strategy_entry("ImportPathChange");
        entry.from = Some("something".to_string());
        // missing `to`
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Manual => {}
            other => panic!("Expected Manual, got {:?}", other),
        }
    }

    #[test]
    fn test_update_dependency() {
        let mut entry = make_strategy_entry("UpdateDependency");
        entry.package = Some("@patternfly/react-core".to_string());
        entry.new_version = Some("^6.0.0".to_string());

        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::UpdateDependency {
                package,
                new_version,
            } => {
                assert_eq!(package, "@patternfly/react-core");
                assert_eq!(new_version, "^6.0.0");
            }
            other => panic!("Expected UpdateDependency, got {:?}", other),
        }
    }

    #[test]
    fn test_update_dependency_missing_fields_falls_to_manual() {
        let mut entry = make_strategy_entry("UpdateDependency");
        entry.package = Some("something".to_string());
        // missing new_version
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Manual => {}
            other => panic!("Expected Manual, got {:?}", other),
        }
    }

    #[test]
    fn test_prop_value_change_maps_to_llm() {
        let entry = make_strategy_entry("PropValueChange");
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Llm => {}
            other => panic!("Expected Llm, got {:?}", other),
        }
    }

    #[test]
    fn test_prop_type_change_maps_to_llm() {
        let entry = make_strategy_entry("PropTypeChange");
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Llm => {}
            other => panic!("Expected Llm, got {:?}", other),
        }
    }

    #[test]
    fn test_llm_assisted_maps_to_llm() {
        let entry = make_strategy_entry("LlmAssisted");
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Llm => {}
            other => panic!("Expected Llm, got {:?}", other),
        }
    }

    #[test]
    fn test_unknown_strategy_maps_to_manual() {
        let entry = make_strategy_entry("SomethingUnknown");
        match strategy_entry_to_fix_strategy(&entry) {
            FixStrategy::Manual => {}
            other => panic!("Expected Manual, got {:?}", other),
        }
    }

    #[test]
    fn test_strategy_entry_json_deserialization() {
        let json = r#"{
            "strategy": "Rename",
            "mappings": [
                {"from": "Chip", "to": "Label"},
                {"from": "ChipGroup", "to": "LabelGroup"}
            ]
        }"#;
        let entry: FixStrategyEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.strategy, "Rename");
        assert_eq!(entry.mappings.len(), 2);
        assert_eq!(entry.mappings[0].from.as_deref(), Some("Chip"));
        assert_eq!(entry.mappings[0].to.as_deref(), Some("Label"));
    }

    #[test]
    fn test_strategy_entry_json_with_top_level_fields() {
        let json = r#"{
            "strategy": "ImportPathChange",
            "from": "@patternfly/react-core/deprecated",
            "to": "@patternfly/react-core"
        }"#;
        let entry: FixStrategyEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.strategy, "ImportPathChange");
        assert_eq!(
            entry.from.as_deref(),
            Some("@patternfly/react-core/deprecated")
        );
        assert_eq!(entry.to.as_deref(), Some("@patternfly/react-core"));
        assert!(entry.mappings.is_empty());
    }

    #[test]
    fn test_fix_plan_default_is_empty() {
        let plan = FixPlan::default();
        assert!(plan.files.is_empty());
        assert!(plan.manual.is_empty());
        assert!(plan.pending_llm.is_empty());
    }

    #[test]
    fn test_fix_result_default_is_zero() {
        let result = FixResult::default();
        assert_eq!(result.files_modified, 0);
        assert_eq!(result.edits_applied, 0);
        assert_eq!(result.edits_skipped, 0);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_fix_confidence_serde() {
        assert_eq!(
            serde_json::to_string(&FixConfidence::Exact).unwrap(),
            "\"exact\""
        );
        assert_eq!(
            serde_json::to_string(&FixConfidence::High).unwrap(),
            "\"high\""
        );
        assert_eq!(
            serde_json::to_string(&FixConfidence::Medium).unwrap(),
            "\"medium\""
        );
        assert_eq!(
            serde_json::to_string(&FixConfidence::Low).unwrap(),
            "\"low\""
        );
    }

    #[test]
    fn test_fix_source_serde() {
        assert_eq!(
            serde_json::to_string(&FixSource::Pattern).unwrap(),
            "\"pattern\""
        );
        assert_eq!(serde_json::to_string(&FixSource::Llm).unwrap(), "\"llm\"");
        assert_eq!(
            serde_json::to_string(&FixSource::Manual).unwrap(),
            "\"manual\""
        );
    }
}

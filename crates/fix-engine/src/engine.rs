//! Fix engine: maps analysis violations to concrete text edits.
//!
//! Two-tier approach:
//! 1. Pattern-based: deterministic renames/removals driven by incident variables
//! 2. LLM-assisted: complex structural changes sent to an LLM endpoint
//!
//! The engine is language-agnostic. Language-specific operations (attribute
//! removal, import deduplication, path skipping) are delegated to a
//! [`LanguageFixProvider`](crate::language::LanguageFixProvider) implementation.

use anyhow::Result;
use frontend_core::fix::*;
use konveyor_core::incident::Incident;
use konveyor_core::report::RuleSet;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use crate::language::LanguageFixProvider;

/// Build a fix plan from analysis output.
///
/// `strategies` is a merged map of rule ID → fix strategy, loaded from one or
/// more external JSON files (rule-adjacent and/or semver-analyzer generated).
/// When no strategy is found for a rule, label-based inference is attempted,
/// falling back to LLM-assisted fixes.
///
/// `lang` provides language-specific fix operations (attribute removal,
/// matched text extraction, path skipping).
pub fn plan_fixes(
    output: &[RuleSet],
    project_root: &std::path::Path,
    strategies: &BTreeMap<String, FixStrategy>,
    lang: &dyn LanguageFixProvider,
) -> Result<FixPlan> {
    let mut plan = FixPlan::default();

    for ruleset in output {
        for (rule_id, violation) in &ruleset.violations {
            // Lookup order: strategies map → label inference → LLM fallback
            let strategy = strategies
                .get(rule_id.as_str())
                .cloned()
                .or_else(|| infer_strategy_from_labels(&violation.labels).cloned())
                .unwrap_or(FixStrategy::Llm);

            for incident in &violation.incidents {
                let file_path = uri_to_path(&incident.file_uri, project_root);

                // Let the language provider decide which paths to skip
                // (e.g., node_modules for JS/TS projects).
                if lang.should_skip_path(&file_path) {
                    continue;
                }

                match &strategy {
                    FixStrategy::Rename(mappings) => {
                        if let Some(fix) =
                            plan_rename(rule_id, incident, mappings, &file_path, lang)
                        {
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::RemoveProp => {
                        if let Some(fix) = lang.plan_remove_attribute(rule_id, incident, &file_path)
                        {
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::ImportPathChange { old_path, new_path } => {
                        if let Some(fix) = plan_import_path_change(
                            rule_id, incident, old_path, new_path, &file_path,
                        ) {
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::CssVariablePrefix {
                        old_prefix,
                        new_prefix,
                    } => {
                        // Treat CSS prefix changes as renames
                        let mappings = vec![RenameMapping {
                            old: old_prefix.clone(),
                            new: new_prefix.clone(),
                        }];
                        if let Some(mut fix) =
                            plan_rename(rule_id, incident, &mappings, &file_path, lang)
                        {
                            // CSS prefix edits should replace ALL occurrences on a line,
                            // e.g. className="pf-v5-u-color-200 pf-v5-u-font-weight-light"
                            for edit in &mut fix.edits {
                                edit.replace_all = true;
                            }
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::UpdateDependency {
                        ref package,
                        ref new_version,
                    } => {
                        if let Some(fix) = plan_update_dependency(
                            rule_id,
                            incident,
                            package,
                            new_version,
                            &file_path,
                        ) {
                            plan.files.entry(file_path).or_default().push(fix);
                        }
                    }
                    FixStrategy::Manual => {
                        plan.manual.push(ManualFixItem {
                            rule_id: rule_id.clone(),
                            file_uri: incident.file_uri.clone(),
                            line: incident.line_number.unwrap_or(0),
                            message: incident.message.clone(),
                            code_snip: incident.code_snip.clone(),
                        });
                    }
                    FixStrategy::Llm => {
                        plan.pending_llm.push(LlmFixRequest {
                            rule_id: rule_id.clone(),
                            file_uri: incident.file_uri.clone(),
                            file_path: file_path.clone(),
                            line: incident.line_number.unwrap_or(0),
                            message: incident.message.clone(),
                            code_snip: incident.code_snip.clone(),
                            source: None, // filled lazily if LLM is invoked
                            labels: violation.labels.clone(),
                        });
                    }
                }
            }
        }
    }

    // Sort edits within each file by line number (descending) so we can apply bottom-up
    for fixes in plan.files.values_mut() {
        fixes.sort_by(|a, b| b.line.cmp(&a.line));
    }

    Ok(plan)
}

/// Apply a fix plan to disk.
///
/// `lang` provides language-specific post-processing (e.g., import deduplication).
pub fn apply_fixes(plan: &FixPlan, lang: &dyn LanguageFixProvider) -> Result<FixResult> {
    let mut result = FixResult::default();

    for (file_path, fixes) in &plan.files {
        let source = match std::fs::read_to_string(file_path) {
            Ok(s) => s,
            Err(e) => {
                result
                    .errors
                    .push(format!("{}: {}", file_path.display(), e));
                continue;
            }
        };

        let mut lines: Vec<String> = source.lines().map(String::from).collect();
        let mut any_changed = false;

        // Deduplicate edits: when multiple incidents generate the same whole-file
        // rename, we get duplicate (line, old_text, new_text) tuples. Only apply each once.
        let mut seen_edits: std::collections::HashSet<(u32, String, String)> =
            std::collections::HashSet::new();

        for fix in fixes {
            for edit in &fix.edits {
                let key = (edit.line, edit.old_text.clone(), edit.new_text.clone());
                if !seen_edits.insert(key) {
                    continue; // already applied this exact edit
                }
                let idx = (edit.line as usize).saturating_sub(1);
                if idx < lines.len() {
                    let line = &lines[idx];
                    if line.contains(&edit.old_text) {
                        lines[idx] = if edit.replace_all {
                            line.replace(&edit.old_text, &edit.new_text)
                        } else {
                            line.replacen(&edit.old_text, &edit.new_text, 1)
                        };
                        result.edits_applied += 1;
                        any_changed = true;
                    } else {
                        result.edits_skipped += 1;
                    }
                } else {
                    result.edits_skipped += 1;
                }
            }
        }

        if any_changed {
            // Language-specific post-processing (e.g., import deduplication)
            lang.post_process_lines(&mut lines);

            // Remove empty lines left by prop removal
            // keep empty lines for now
            lines.retain(|_l| true);

            // Preserve original trailing newline
            let mut output = lines.join("\n");
            if source.ends_with('\n') {
                output.push('\n');
            }
            std::fs::write(file_path, output)?;
            result.files_modified += 1;
        }
    }

    Ok(result)
}

/// Generate a unified diff preview of the planned changes.
///
/// `lang` provides language-specific post-processing (e.g., import deduplication).
pub fn preview_fixes(plan: &FixPlan, lang: &dyn LanguageFixProvider) -> Result<String> {
    let mut output = String::new();

    for (file_path, fixes) in &plan.files {
        let source = match std::fs::read_to_string(file_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let lines: Vec<&str> = source.lines().collect();
        let mut changed_lines: HashMap<usize, String> = HashMap::new();

        // Apply edits to get the "after" lines
        for fix in fixes {
            for edit in &fix.edits {
                let idx = (edit.line as usize).saturating_sub(1);
                if idx < lines.len() {
                    let current = changed_lines
                        .get(&idx)
                        .map(String::as_str)
                        .unwrap_or(lines[idx]);
                    if current.contains(&edit.old_text) {
                        let new_line = if edit.replace_all {
                            current.replace(&edit.old_text, &edit.new_text)
                        } else {
                            current.replacen(&edit.old_text, &edit.new_text, 1)
                        };
                        changed_lines.insert(idx, new_line);
                    }
                }
            }
        }

        if changed_lines.is_empty() {
            continue;
        }

        // Language-specific post-processing on changed lines
        for (_, line_content) in changed_lines.iter_mut() {
            let mut single = [line_content.clone()];
            lang.post_process_lines(&mut single);
            *line_content = single.into_iter().next().unwrap();
        }

        output.push_str(&format!(
            "--- a/{}\n+++ b/{}\n",
            file_path.display(),
            file_path.display()
        ));

        // Group consecutive changed lines into hunks
        let mut changed_indices: Vec<usize> = changed_lines.keys().copied().collect();
        changed_indices.sort();

        for &idx in &changed_indices {
            let context = 3;
            let start = idx.saturating_sub(context);
            let end = (idx + context + 1).min(lines.len());

            output.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                start + 1,
                end - start,
                start + 1,
                end - start
            ));

            for (i, line) in lines.iter().enumerate().take(end).skip(start) {
                if let Some(new_line) = changed_lines.get(&i) {
                    output.push_str(&format!("-{}\n", line));
                    output.push_str(&format!("+{}\n", new_line));
                } else {
                    output.push_str(&format!(" {}\n", line));
                }
            }
        }
    }

    Ok(output)
}

// ── Pattern-based fix generators ──────────────────────────────────────────

fn plan_rename(
    rule_id: &str,
    incident: &Incident,
    mappings: &[RenameMapping],
    file_path: &PathBuf,
    lang: &dyn LanguageFixProvider,
) -> Option<PlannedFix> {
    let line = incident.line_number?;

    // Determine what text to look for from incident variables.
    // For value-level renames (e.g., variant="light" → "secondary"),
    // the mapping's `old` is the value, not the prop name.
    let matched_text = lang.get_matched_text_for_rename(incident, mappings);

    // Check if this rename requires whole-file scanning (e.g., component/import
    // renames in JSX that affect tags, closing tags, and type references beyond
    // the incident line).
    let is_whole_file_rename = lang.is_whole_file_rename(incident);

    // Try to find a mapping that matches the incident's matched text
    let primary_mapping = mappings.iter().find(|m| m.old == matched_text);

    // Read the file — we always need it for value-level scans and whole-file renames
    let source = std::fs::read_to_string(file_path).ok()?;
    let mut edits = Vec::new();

    if is_whole_file_rename {
        // Whole-file rename: scan the ENTIRE file for all occurrences of
        // every mapping's old value. This catches all usage sites beyond the
        // incident line (e.g., tags, type references, other declarations).
        //
        // Sort mappings longest-first to avoid substring false matches:
        // e.g., "TextContent" should be matched before "Text" since "Text"
        // is a substring of "TextContent".
        let mut sorted_mappings: Vec<&RenameMapping> =
            mappings.iter().filter(|m| m.old != m.new).collect();
        sorted_mappings.sort_by(|a, b| b.old.len().cmp(&a.old.len()));

        for (idx, file_line) in source.lines().enumerate() {
            let line_num = (idx + 1) as u32;
            // Track which ranges on this line have been claimed by longer mappings
            // to prevent shorter substring matches from generating duplicate edits.
            let mut consumed: Vec<&str> = Vec::new();
            for m in &sorted_mappings {
                if file_line.contains(m.old.as_str()) {
                    // Skip if a longer mapping already covers this match.
                    // e.g., skip "Text" if "TextContent" already matched on this line.
                    let is_substring_of_consumed =
                        consumed.iter().any(|c| c.contains(m.old.as_str()));
                    if is_substring_of_consumed {
                        continue;
                    }
                    edits.push(TextEdit {
                        line: line_num,
                        old_text: m.old.clone(),
                        new_text: m.new.clone(),
                        rule_id: rule_id.to_string(),
                        description: format!("Rename '{}' to '{}'", m.old, m.new),
                        replace_all: false,
                    });
                    consumed.push(&m.old);
                }
            }
        }
    } else if let Some(mapping) = primary_mapping {
        // Standard rename: apply the primary mapping on the incident line
        if mapping.old == mapping.new {
            return None;
        }
        edits.push(TextEdit {
            line,
            old_text: mapping.old.clone(),
            new_text: mapping.new.clone(),
            rule_id: rule_id.to_string(),
            description: format!("Rename '{}' to '{}'", mapping.old, mapping.new),
            replace_all: false,
        });

        // Also scan the incident line for value-level renames from other mappings.
        // e.g., when the prop key `spacer` -> `gap` is the primary match, also
        // rename `spacerNone` -> `gapNone`, `spacerMd` -> `gapMd` etc. on the
        // same line or nearby lines in the same prop value expression.
        let line_idx = (line as usize).saturating_sub(1);
        // Scan a small window around the incident line to catch multi-line prop values
        let scan_start = line_idx.saturating_sub(3);
        let scan_end = (line_idx + 5).min(source.lines().count());
        for (idx, file_line) in source
            .lines()
            .enumerate()
            .skip(scan_start)
            .take(scan_end - scan_start)
        {
            let line_num = (idx + 1) as u32;
            for m in mappings {
                if m.old == m.new {
                    continue;
                }
                // Skip the primary mapping on the primary line (already added)
                if std::ptr::eq(m, mapping) && line_num == line {
                    continue;
                }
                if file_line.contains(&m.old) {
                    edits.push(TextEdit {
                        line: line_num,
                        old_text: m.old.clone(),
                        new_text: m.new.clone(),
                        rule_id: rule_id.to_string(),
                        description: format!("Rename '{}' to '{}'", m.old, m.new),
                        replace_all: false,
                    });
                }
            }
        }
    } else {
        // Fallback: no primary match found. Scan the incident line for any
        // applicable mappings (handles prop-value-change and CSS rules where
        // the incident captures the prop/class name but mappings are value-level).
        if let Some(file_line) = source.lines().nth((line as usize).saturating_sub(1)) {
            for m in mappings {
                if m.old == m.new {
                    continue;
                }
                if file_line.contains(&m.old) {
                    edits.push(TextEdit {
                        line,
                        old_text: m.old.clone(),
                        new_text: m.new.clone(),
                        rule_id: rule_id.to_string(),
                        description: format!("Rename '{}' to '{}'", m.old, m.new),
                        replace_all: false,
                    });
                }
            }
        }
    }

    if edits.is_empty() {
        return None;
    }

    let desc = edits
        .iter()
        .map(|e| format!("'{}' → '{}'", e.old_text, e.new_text))
        .collect::<Vec<_>>()
        .join(", ");

    Some(PlannedFix {
        edits,
        confidence: FixConfidence::Exact,
        source: FixSource::Pattern,
        rule_id: rule_id.to_string(),
        file_uri: incident.file_uri.clone(),
        line,
        description: format!("Rename {}", desc),
    })
}

fn plan_import_path_change(
    rule_id: &str,
    incident: &Incident,
    old_path: &str,
    new_path: &str,
    _file_path: &PathBuf,
) -> Option<PlannedFix> {
    let line = incident.line_number?;

    Some(PlannedFix {
        edits: vec![TextEdit {
            line,
            old_text: old_path.to_string(),
            new_text: new_path.to_string(),
            rule_id: rule_id.to_string(),
            description: format!("Change import path '{}' → '{}'", old_path, new_path),
            replace_all: false,
        }],
        confidence: FixConfidence::Exact,
        source: FixSource::Pattern,
        rule_id: rule_id.to_string(),
        file_uri: incident.file_uri.clone(),
        line,
        description: format!("Change import path to '{}'", new_path),
    })
}

fn plan_update_dependency(
    rule_id: &str,
    incident: &Incident,
    package: &str,
    new_version: &str,
    file_path: &PathBuf,
) -> Option<PlannedFix> {
    let source = std::fs::read_to_string(file_path).ok()?;

    // Find the line containing this package name, regardless of incident line number.
    // Kantra may not pass through the provider's line number correctly.
    let package_quoted = format!("\"{}\"", package);
    let version_re = regex::Regex::new(r#"("[\^~><=]*\d+\.\d+\.\d+[^"]*")"#).ok()?;

    for (idx, file_line) in source.lines().enumerate() {
        if !file_line.contains(&package_quoted) {
            continue;
        }
        if let Some(m) = version_re.find(file_line) {
            let line = (idx + 1) as u32;
            let old_version = m.as_str();
            let new_ver_quoted = format!("\"{}\"", new_version);

            return Some(PlannedFix {
                edits: vec![TextEdit {
                    line,
                    old_text: old_version.to_string(),
                    new_text: new_ver_quoted.clone(),
                    rule_id: rule_id.to_string(),
                    description: format!(
                        "Update {} from {} to {}",
                        package, old_version, new_ver_quoted
                    ),
                    replace_all: false,
                }],
                confidence: FixConfidence::Exact,
                source: FixSource::Pattern,
                rule_id: rule_id.to_string(),
                file_uri: incident.file_uri.clone(),
                line,
                description: format!("Update {} to {}", package, new_version),
            });
        }
    }

    None
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Convert a file:// URI to a filesystem path, relative to project root.
fn uri_to_path(uri: &str, project_root: &std::path::Path) -> PathBuf {
    let path_str = uri.strip_prefix("file://").unwrap_or(uri);

    let path = PathBuf::from(path_str);
    if path.is_absolute() {
        path
    } else {
        project_root.join(path)
    }
}

/// Try to infer a fix strategy from rule labels when no explicit mapping exists.
/// This is a fallback for rules not covered by any strategy file.
fn infer_strategy_from_labels(labels: &[String]) -> Option<&'static FixStrategy> {
    for label in labels {
        match label.as_str() {
            "change-type=prop-removal" => return Some(&FixStrategy::RemoveProp),
            "change-type=dom-structure"
            | "change-type=behavioral"
            | "change-type=accessibility"
            | "change-type=interface-removal"
            | "change-type=module-export"
            | "change-type=other" => return Some(&FixStrategy::Manual),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── uri_to_path tests ────────────────────────────────────────────────

    #[test]
    fn test_uri_to_path_absolute() {
        let path = uri_to_path(
            "file:///home/user/project/src/App.tsx",
            std::path::Path::new("/ignored"),
        );
        assert_eq!(path, PathBuf::from("/home/user/project/src/App.tsx"));
    }

    #[test]
    fn test_uri_to_path_relative() {
        let path = uri_to_path("src/App.tsx", std::path::Path::new("/home/user/project"));
        assert_eq!(path, PathBuf::from("/home/user/project/src/App.tsx"));
    }

    #[test]
    fn test_uri_to_path_no_file_prefix() {
        let path = uri_to_path("/absolute/path.tsx", std::path::Path::new("/root"));
        assert_eq!(path, PathBuf::from("/absolute/path.tsx"));
    }

    // ── infer_strategy_from_labels tests ─────────────────────────────────

    #[test]
    fn test_infer_prop_removal() {
        let labels = vec!["change-type=prop-removal".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::RemoveProp)));
    }

    #[test]
    fn test_infer_dom_structure_manual() {
        let labels = vec!["change-type=dom-structure".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::Manual)));
    }

    #[test]
    fn test_infer_behavioral_manual() {
        let labels = vec!["change-type=behavioral".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::Manual)));
    }

    #[test]
    fn test_infer_accessibility_manual() {
        let labels = vec!["change-type=accessibility".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::Manual)));
    }

    #[test]
    fn test_infer_interface_removal_manual() {
        let labels = vec!["change-type=interface-removal".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::Manual)));
    }

    #[test]
    fn test_infer_module_export_manual() {
        let labels = vec!["change-type=module-export".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::Manual)));
    }

    #[test]
    fn test_infer_other_manual() {
        let labels = vec!["change-type=other".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::Manual)));
    }

    #[test]
    fn test_infer_unknown_label_returns_none() {
        let labels = vec!["change-type=rename".to_string()];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(strategy.is_none());
    }

    #[test]
    fn test_infer_empty_labels_returns_none() {
        let labels: Vec<String> = Vec::new();
        let strategy = infer_strategy_from_labels(&labels);
        assert!(strategy.is_none());
    }

    #[test]
    fn test_infer_first_matching_label_wins() {
        let labels = vec![
            "framework=patternfly".to_string(),
            "change-type=prop-removal".to_string(),
            "change-type=dom-structure".to_string(), // would also match, but prop-removal comes first
        ];
        let strategy = infer_strategy_from_labels(&labels);
        assert!(matches!(strategy, Some(&FixStrategy::RemoveProp)));
    }
}

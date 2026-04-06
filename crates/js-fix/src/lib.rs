//! JS/TS/JSX/TSX language-specific fix operations.
//!
//! Implements [`LanguageFixProvider`] for the JavaScript/TypeScript ecosystem:
//! - Skips `node_modules/` paths
//! - Deduplicates ES import specifiers after renames
//! - Removes JSX attributes (props) using syntax-aware regex
//! - Extracts matched text from JSX/React incident variables

use frontend_core::fix::*;
use frontend_fix_engine::language::LanguageFixProvider;
use konveyor_core::incident::Incident;
use std::path::Path;

/// Language fix provider for JavaScript/TypeScript/JSX/TSX files.
pub struct JsFixProvider;

impl JsFixProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JsFixProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageFixProvider for JsFixProvider {
    fn should_skip_path(&self, path: &Path) -> bool {
        // Skip node_modules — these are updated via package.json
        // version bumps, not by patching source directly.
        // Note: src/vendor/ is NOT skipped — vendored source code
        // (e.g., forked libraries) is compiled as part of the project
        // and needs migration alongside the rest of the codebase.
        path.components().any(|c| c.as_os_str() == "node_modules")
    }

    fn post_process_lines(&self, lines: &mut [String]) {
        dedup_import_specifiers(lines);
    }

    fn plan_remove_attribute(
        &self,
        rule_id: &str,
        incident: &Incident,
        file_path: &Path,
    ) -> Option<PlannedFix> {
        plan_remove_prop(rule_id, incident, file_path)
    }

    fn get_matched_text(&self, incident: &Incident) -> String {
        get_matched_text_from_incident(incident)
    }

    fn get_matched_text_for_rename(
        &self,
        incident: &Incident,
        mappings: &[RenameMapping],
    ) -> String {
        get_matched_text_for_rename_from_incident(incident, mappings)
    }

    fn is_whole_file_rename(&self, incident: &Incident) -> bool {
        // Component/import renames (detected via importedName variable) need
        // whole-file scanning since JSX usage of the component appears on many
        // lines beyond the import: opening tags, closing tags, type references.
        incident.variables.contains_key("importedName")
    }
}

// ── JSX prop removal ─────────────────────────────────────────────────────

fn plan_remove_prop(rule_id: &str, incident: &Incident, file_path: &Path) -> Option<PlannedFix> {
    let line = incident.line_number?;
    let prop_name = incident
        .variables
        .get("propName")
        .and_then(|v| v.as_str())?;

    // Read the actual file line to construct a precise removal edit.
    let source = std::fs::read_to_string(file_path).ok()?;
    let all_lines: Vec<&str> = source.lines().collect();
    let line_idx = (line as usize).saturating_sub(1);
    let file_line = all_lines.get(line_idx)?;
    let trimmed = file_line.trim();

    // If the entire line is just the prop (common in formatted JSX), remove it.
    // Patterns: `propName`, `propName={...}`, `propName="..."`, `propName={true}`
    if trimmed.starts_with(prop_name) {
        // Check if the prop value is self-contained on this line by counting
        // bracket/brace depth. If the value spans multiple lines (e.g.,
        // `actions={[ <Button>...</Button> ]}`), we need to remove all of them.
        let depth = bracket_depth(file_line);
        if depth == 0 {
            // Single-line prop — safe to remove just this line
            Some(PlannedFix {
                edits: vec![TextEdit {
                    line,
                    old_text: file_line.to_string(),
                    new_text: String::new(),
                    rule_id: rule_id.to_string(),
                    description: format!("Remove prop '{}' (entire line)", prop_name),
                    replace_all: false,
                }],
                confidence: FixConfidence::High,
                source: FixSource::Pattern,
                rule_id: rule_id.to_string(),
                file_uri: incident.file_uri.clone(),
                line,
                description: format!("Remove prop '{}'", prop_name),
            })
        } else {
            // Multi-line prop value — scan forward to find where brackets balance.
            let mut cumulative_depth = depth;
            let mut end_idx = line_idx;
            for (i, subsequent_line) in all_lines.iter().enumerate().skip(line_idx + 1) {
                cumulative_depth += bracket_depth(subsequent_line);
                end_idx = i;
                if cumulative_depth <= 0 {
                    break;
                }
            }

            if cumulative_depth > 0 {
                // Could not find matching close bracket — bail to manual review
                return Some(PlannedFix {
                    edits: vec![],
                    confidence: FixConfidence::Low,
                    source: FixSource::Pattern,
                    rule_id: rule_id.to_string(),
                    file_uri: incident.file_uri.clone(),
                    line,
                    description: format!(
                        "Remove prop '{}' (unbalanced brackets, manual)",
                        prop_name
                    ),
                });
            }

            // Remove all lines from prop start through closing bracket
            let mut edits = Vec::new();
            for i in line_idx..=end_idx {
                if let Some(l) = all_lines.get(i) {
                    edits.push(TextEdit {
                        line: (i + 1) as u32,
                        old_text: l.to_string(),
                        new_text: String::new(),
                        rule_id: rule_id.to_string(),
                        description: format!(
                            "Remove prop '{}' (line {} of multi-line)",
                            prop_name,
                            i - line_idx + 1
                        ),
                        replace_all: false,
                    });
                }
            }

            Some(PlannedFix {
                edits,
                confidence: FixConfidence::High,
                source: FixSource::Pattern,
                rule_id: rule_id.to_string(),
                file_uri: incident.file_uri.clone(),
                line,
                description: format!(
                    "Remove prop '{}' ({} lines)",
                    prop_name,
                    end_idx - line_idx + 1
                ),
            })
        }
    } else {
        // Prop is inline with other content — try to remove just the prop fragment.
        // Match: ` propName={...}` or ` propName="..."` or ` propName`
        // Use a simple regex to find the prop and its value on a single line.
        let prop_re = regex::Regex::new(&format!(
            r#"\s+{prop_name}(?:=\{{[^}}]*\}}|="[^"]*"|='[^']*'|=\{{.*?\}})?"#
        ))
        .ok()?;

        if let Some(m) = prop_re.find(file_line) {
            // Verify the matched fragment has balanced brackets. If not, the value
            // spans multiple lines and a simple single-line removal would corrupt the file.
            if bracket_depth(m.as_str()) != 0 {
                return Some(PlannedFix {
                    edits: vec![],
                    confidence: FixConfidence::Low,
                    source: FixSource::Pattern,
                    rule_id: rule_id.to_string(),
                    file_uri: incident.file_uri.clone(),
                    line,
                    description: format!("Remove prop '{}' (multi-line inline, manual)", prop_name),
                });
            }

            Some(PlannedFix {
                edits: vec![TextEdit {
                    line,
                    old_text: m.as_str().to_string(),
                    new_text: String::new(),
                    rule_id: rule_id.to_string(),
                    description: format!("Remove prop '{}'", prop_name),
                    replace_all: false,
                }],
                confidence: FixConfidence::High,
                source: FixSource::Pattern,
                rule_id: rule_id.to_string(),
                file_uri: incident.file_uri.clone(),
                line,
                description: format!("Remove prop '{}'", prop_name),
            })
        } else {
            // Can't parse — flag for manual review
            Some(PlannedFix {
                edits: vec![],
                confidence: FixConfidence::Low,
                source: FixSource::Pattern,
                rule_id: rule_id.to_string(),
                file_uri: incident.file_uri.clone(),
                line,
                description: format!("Remove prop '{}' (manual)", prop_name),
            })
        }
    }
}

// ── Import deduplication ─────────────────────────────────────────────────

/// Deduplicate import specifiers on lines that look like ES import statements.
///
/// After renaming multiple symbols to the same name (e.g., TextContent/TextList → Content),
/// an import line may have duplicate specifiers: `import { Content, Content, Content }`.
/// This function deduplicates them to `import { Content }`.
fn dedup_import_specifiers(lines: &mut [String]) {
    let import_re = regex::Regex::new(r"^(\s*import\s+\{)([^}]+)(\}\s*from\s+.*)$").unwrap();

    for line in lines.iter_mut() {
        if let Some(caps) = import_re.captures(line) {
            let prefix = caps.get(1).unwrap().as_str();
            let specifiers_str = caps.get(2).unwrap().as_str();
            let suffix = caps.get(3).unwrap().as_str();

            let specifiers: Vec<&str> = specifiers_str
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();

            let mut seen = std::collections::HashSet::new();
            let deduped: Vec<&str> = specifiers
                .into_iter()
                .filter(|s| {
                    // Handle `Name as Alias` — dedup by the full specifier
                    seen.insert(s.to_string())
                })
                .collect();

            let new_specifiers = format!(" {} ", deduped.join(", "));
            let new_line = format!("{}{}{}", prefix, new_specifiers, suffix);

            if new_line != *line {
                *line = new_line;
            }
        }
    }
}

// ── Bracket depth ────────────────────────────────────────────────────────

/// Count net bracket/brace depth change for a line.
/// Returns positive if more openers than closers, negative if more closers.
/// Ignores brackets inside string literals (single/double quoted and
/// JS template literals).
fn bracket_depth(line: &str) -> i32 {
    let mut depth: i32 = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_backtick = false;
    let mut prev = '\0';
    for ch in line.chars() {
        match ch {
            '\'' if !in_double_quote && !in_backtick && prev != '\\' => {
                in_single_quote = !in_single_quote
            }
            '"' if !in_single_quote && !in_backtick && prev != '\\' => {
                in_double_quote = !in_double_quote
            }
            '`' if !in_single_quote && !in_double_quote && prev != '\\' => {
                in_backtick = !in_backtick
            }
            '(' | '{' | '[' if !in_single_quote && !in_double_quote && !in_backtick => depth += 1,
            ')' | '}' | ']' if !in_single_quote && !in_double_quote && !in_backtick => depth -= 1,
            _ => {}
        }
        prev = ch;
    }
    depth
}

// ── Incident variable extraction ─────────────────────────────────────────

/// Extract the matched text from incident variables.
/// Checks propName, componentName, importedName, className, variableName in that order.
fn get_matched_text_from_incident(incident: &Incident) -> String {
    for key in &[
        "propName",
        "componentName",
        "importedName",
        "className",
        "variableName",
    ] {
        if let Some(serde_json::Value::String(s)) = incident.variables.get(*key) {
            return s.clone();
        }
    }
    String::new()
}

/// Get the matched text, considering both prop names and prop values.
/// Used by `plan_rename` to find the correct mapping — for value-level
/// renames (e.g., variant="light" → "secondary"), the mapping's `old`
/// is the value, not the prop name.
fn get_matched_text_for_rename_from_incident(
    incident: &Incident,
    mappings: &[RenameMapping],
) -> String {
    let prop_name = get_matched_text_from_incident(incident);

    // If a mapping matches the prop name directly, use it
    if mappings.iter().any(|m| m.old == prop_name) {
        return prop_name;
    }

    // Check if a mapping matches the prop VALUE instead (enum value renames)
    if let Some(serde_json::Value::String(val)) = incident.variables.get("propValue") {
        if mappings.iter().any(|m| m.old == val.as_str()) {
            return val.clone();
        }
    }

    // Check propObjectValues (responsive breakpoint objects)
    if let Some(serde_json::Value::Array(vals)) = incident.variables.get("propObjectValues") {
        for v in vals {
            if let serde_json::Value::String(s) = v {
                if mappings.iter().any(|m| m.old == s.as_str()) {
                    return s.clone();
                }
            }
        }
    }

    prop_name
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Create a test Incident with just the fields the fix provider cares about.
    fn make_test_incident(
        uri: &str,
        line: u32,
        variables: BTreeMap<String, serde_json::Value>,
    ) -> Incident {
        Incident {
            file_uri: uri.to_string(),
            line_number: Some(line),
            code_location: None,
            message: String::new(),
            code_snip: None,
            variables,
            effort: None,
            links: Vec::new(),
            is_dependency_incident: false,
        }
    }

    // ── should_skip_path tests ───────────────────────────────────────────

    #[test]
    fn test_skip_node_modules() {
        let provider = JsFixProvider::new();
        assert!(provider.should_skip_path(Path::new(
            "/project/node_modules/@patternfly/react-core/index.js"
        )));
    }

    #[test]
    fn test_skip_nested_node_modules() {
        let provider = JsFixProvider::new();
        assert!(
            provider.should_skip_path(Path::new("/project/packages/app/node_modules/foo/bar.ts"))
        );
    }

    #[test]
    fn test_does_not_skip_src() {
        let provider = JsFixProvider::new();
        assert!(!provider.should_skip_path(Path::new("/project/src/App.tsx")));
    }

    #[test]
    fn test_does_not_skip_vendor() {
        let provider = JsFixProvider::new();
        assert!(!provider.should_skip_path(Path::new("/project/src/vendor/lib.ts")));
    }

    // ── is_whole_file_rename tests ───────────────────────────────────────

    #[test]
    fn test_whole_file_rename_with_imported_name() {
        let provider = JsFixProvider::new();
        let mut vars = BTreeMap::new();
        vars.insert(
            "importedName".to_string(),
            serde_json::Value::String("Chip".to_string()),
        );
        let incident = make_test_incident("file:///test.tsx", 1, vars);
        assert!(provider.is_whole_file_rename(&incident));
    }

    #[test]
    fn test_not_whole_file_rename_without_imported_name() {
        let provider = JsFixProvider::new();
        let mut vars = BTreeMap::new();
        vars.insert(
            "propName".to_string(),
            serde_json::Value::String("isActive".to_string()),
        );
        let incident = make_test_incident("file:///test.tsx", 1, vars);
        assert!(!provider.is_whole_file_rename(&incident));
    }

    // ── bracket_depth tests ──────────────────────────────────────────────

    #[test]
    fn test_bracket_depth_balanced() {
        assert_eq!(bracket_depth("{ foo: bar }"), 0);
        assert_eq!(bracket_depth("foo()"), 0);
        assert_eq!(bracket_depth("[1, 2, 3]"), 0);
        assert_eq!(bracket_depth("{ foo: [1, 2] }"), 0);
    }

    #[test]
    fn test_bracket_depth_open() {
        assert_eq!(bracket_depth("actions={["), 2); // { and [
        assert_eq!(bracket_depth("  <Button"), 0);
        assert_eq!(bracket_depth("foo(bar, {"), 2); // ( and {
    }

    #[test]
    fn test_bracket_depth_close() {
        assert_eq!(bracket_depth("]}"), -2);
        assert_eq!(bracket_depth(")"), -1);
    }

    #[test]
    fn test_bracket_depth_nested() {
        assert_eq!(bracket_depth("{ a: { b: [1] } }"), 0);
        assert_eq!(bracket_depth("f(g(h(x)))"), 0);
    }

    #[test]
    fn test_bracket_depth_ignores_string_literals() {
        assert_eq!(bracket_depth(r#"  foo="{not a bracket}""#), 0);
        assert_eq!(bracket_depth("  foo='[still not]'"), 0);
        assert_eq!(bracket_depth("  foo=`${`nested`}`"), 0);
    }

    #[test]
    fn test_bracket_depth_empty() {
        assert_eq!(bracket_depth(""), 0);
        assert_eq!(bracket_depth("   just text   "), 0);
    }

    #[test]
    fn test_bracket_depth_escaped_quotes() {
        // Escaped quote should not toggle string mode
        assert_eq!(bracket_depth(r#"  "escaped \" quote" "#), 0);
    }

    // ── dedup_import_specifiers tests ────────────────────────────────────

    #[test]
    fn test_dedup_import_no_duplicates() {
        let mut lines = vec!["import { Foo, Bar } from '@pkg';".to_string()];
        dedup_import_specifiers(&mut lines);
        assert!(lines[0].contains("Foo"));
        assert!(lines[0].contains("Bar"));
    }

    #[test]
    fn test_dedup_import_removes_duplicates() {
        let mut lines =
            vec!["import { Content, Content, Content } from '@patternfly/react-core';".to_string()];
        dedup_import_specifiers(&mut lines);
        // Should contain exactly one "Content"
        let count = lines[0].matches("Content").count();
        assert_eq!(
            count, 1,
            "Expected 1 occurrence of Content, got {}: {}",
            count, lines[0]
        );
    }

    #[test]
    fn test_dedup_import_preserves_different_specifiers() {
        let mut lines = vec!["import { Foo, Bar, Foo, Baz, Bar } from '@pkg';".to_string()];
        dedup_import_specifiers(&mut lines);
        assert_eq!(lines[0].matches("Foo").count(), 1);
        assert_eq!(lines[0].matches("Bar").count(), 1);
        assert_eq!(lines[0].matches("Baz").count(), 1);
    }

    #[test]
    fn test_dedup_import_preserves_aliases() {
        let mut lines = vec!["import { Foo as F, Foo as F } from '@pkg';".to_string()];
        dedup_import_specifiers(&mut lines);
        assert_eq!(lines[0].matches("Foo as F").count(), 1);
    }

    #[test]
    fn test_dedup_import_non_import_lines_unchanged() {
        let original = "const x = { Foo, Foo };".to_string();
        let mut lines = vec![original.clone()];
        dedup_import_specifiers(&mut lines);
        assert_eq!(lines[0], original);
    }

    // ── get_matched_text tests ───────────────────────────────────────────

    #[test]
    fn test_get_matched_text_prop_name_first() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "propName".to_string(),
            serde_json::Value::String("isActive".to_string()),
        );
        vars.insert(
            "componentName".to_string(),
            serde_json::Value::String("Button".to_string()),
        );
        let incident = make_test_incident("file:///test.tsx", 1, vars);
        assert_eq!(get_matched_text_from_incident(&incident), "isActive");
    }

    #[test]
    fn test_get_matched_text_component_name() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "componentName".to_string(),
            serde_json::Value::String("Button".to_string()),
        );
        let incident = make_test_incident("", 1, vars);
        assert_eq!(get_matched_text_from_incident(&incident), "Button");
    }

    #[test]
    fn test_get_matched_text_imported_name() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "importedName".to_string(),
            serde_json::Value::String("Chip".to_string()),
        );
        let incident = make_test_incident("", 1, vars);
        assert_eq!(get_matched_text_from_incident(&incident), "Chip");
    }

    #[test]
    fn test_get_matched_text_empty_when_no_known_vars() {
        let incident = make_test_incident("", 1, BTreeMap::new());
        assert_eq!(get_matched_text_from_incident(&incident), "");
    }

    #[test]
    fn test_get_matched_text_ignores_non_string_values() {
        let mut vars = BTreeMap::new();
        vars.insert("propName".to_string(), serde_json::Value::Bool(true));
        vars.insert(
            "componentName".to_string(),
            serde_json::Value::String("Fallback".to_string()),
        );
        let incident = make_test_incident("", 1, vars);
        assert_eq!(get_matched_text_from_incident(&incident), "Fallback");
    }

    // ── get_matched_text_for_rename tests ────────────────────────────────

    #[test]
    fn test_get_matched_text_for_rename_prefers_prop_name() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "propName".into(),
            serde_json::Value::String("spaceItems".into()),
        );
        let incident = make_test_incident("file:///test.tsx", 1, vars);
        let mappings = vec![RenameMapping {
            old: "spaceItems".into(),
            new: "gap".into(),
        }];
        assert_eq!(
            get_matched_text_for_rename_from_incident(&incident, &mappings),
            "spaceItems"
        );
    }

    #[test]
    fn test_get_matched_text_for_rename_falls_back_to_prop_value() {
        let mut vars = BTreeMap::new();
        vars.insert(
            "propName".into(),
            serde_json::Value::String("variant".into()),
        );
        vars.insert(
            "propValue".into(),
            serde_json::Value::String("light".into()),
        );
        let incident = make_test_incident("file:///test.tsx", 1, vars);
        let mappings = vec![RenameMapping {
            old: "light".into(),
            new: "secondary".into(),
        }];
        assert_eq!(
            get_matched_text_for_rename_from_incident(&incident, &mappings),
            "light"
        );
    }

    #[test]
    fn test_get_matched_text_for_rename_checks_object_values() {
        let mut vars = BTreeMap::new();
        vars.insert("propName".into(), serde_json::Value::String("gap".into()));
        vars.insert(
            "propObjectValues".into(),
            serde_json::json!(["spaceItemsMd", "spaceItemsNone"]),
        );
        let incident = make_test_incident("file:///test.tsx", 1, vars);
        let mappings = vec![RenameMapping {
            old: "spaceItemsMd".into(),
            new: "gapMd".into(),
        }];
        assert_eq!(
            get_matched_text_for_rename_from_incident(&incident, &mappings),
            "spaceItemsMd"
        );
    }

    // ── post_process_lines integration test ──────────────────────────────

    #[test]
    fn test_post_process_deduplicates_imports() {
        let provider = JsFixProvider::new();
        let mut lines = vec![
            "import { Content, Content } from '@patternfly/react-core';".to_string(),
            "const x = 1;".to_string(),
        ];
        provider.post_process_lines(&mut lines);
        assert_eq!(lines[0].matches("Content").count(), 1);
        assert_eq!(lines[1], "const x = 1;");
    }
}

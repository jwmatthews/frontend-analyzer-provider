//! CSS selector scanning using LightningCSS.
//!
//! Parses CSS files and finds class selectors matching a pattern.

use anyhow::Result;
use frontend_core::incident::{extract_code_snip, Incident, Location, Position};
use lightningcss::rules::CssRule;
use lightningcss::rules::Location as CssLocation;
use lightningcss::selector::{Component, Selector};
use lightningcss::stylesheet::{ParserOptions, StyleSheet};
use regex::Regex;
use std::path::Path;

/// Scan a CSS file for class selectors matching the pattern.
pub fn scan_css_selectors(file_path: &Path, root: &Path, pattern: &Regex) -> Result<Vec<Incident>> {
    let source = std::fs::read_to_string(file_path)?;
    let file_uri = crate::scanner::path_to_uri(file_path, root);

    let stylesheet = StyleSheet::parse(&source, ParserOptions::default());
    let stylesheet = match stylesheet {
        Ok(s) => s,
        Err(_) => {
            return crate::scss_fallback::scan_scss_classes(file_path, root, pattern);
        }
    };

    let mut incidents = Vec::new();

    for rule in &stylesheet.rules.0 {
        scan_rule_for_classes(rule, &source, pattern, &file_uri, &mut incidents);
    }

    Ok(incidents)
}

fn scan_rule_for_classes(
    rule: &CssRule,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    incidents: &mut Vec<Incident>,
) {
    match rule {
        CssRule::Style(style_rule) => {
            for selector in style_rule.selectors.0.iter() {
                check_selector_components(
                    selector,
                    source,
                    pattern,
                    file_uri,
                    &style_rule.loc,
                    incidents,
                );
            }
            for nested in &style_rule.rules.0 {
                scan_rule_for_classes(nested, source, pattern, file_uri, incidents);
            }
        }
        CssRule::Media(media) => {
            for r in &media.rules.0 {
                scan_rule_for_classes(r, source, pattern, file_uri, incidents);
            }
        }
        CssRule::Supports(supports) => {
            for r in &supports.rules.0 {
                scan_rule_for_classes(r, source, pattern, file_uri, incidents);
            }
        }
        CssRule::LayerBlock(layer) => {
            for r in &layer.rules.0 {
                scan_rule_for_classes(r, source, pattern, file_uri, incidents);
            }
        }
        _ => {}
    }
}

fn check_selector_components<'i>(
    selector: &Selector<'i>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    loc: &CssLocation,
    incidents: &mut Vec<Incident>,
) {
    for component in selector.iter_raw_match_order() {
        match component {
            Component::Class(class_name) => {
                let name = class_name.as_ref();
                if pattern.is_match(name) {
                    emit_class_incident(source, file_uri, loc, name, incidents);
                }
            }
            // Recurse into pseudo-class functions that contain nested selectors:
            // :not(.pf-v5-c-tabs), :is(.pf-v5-c-button), :where(...), :has(...)
            Component::Negation(selectors) => {
                for inner in selectors.iter() {
                    check_selector_components(inner, source, pattern, file_uri, loc, incidents);
                }
            }
            Component::Is(selectors) | Component::Where(selectors) => {
                for inner in selectors.iter() {
                    check_selector_components(inner, source, pattern, file_uri, loc, incidents);
                }
            }
            Component::Has(selectors) => {
                for inner in selectors.iter() {
                    check_selector_components(inner, source, pattern, file_uri, loc, incidents);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scan(css: &str, pattern: &str) -> Vec<Incident> {
        let re = Regex::new(pattern).unwrap();
        let root = PathBuf::from("/project");
        let _file_path = root.join("test.css");

        // Write to a temp approach: parse directly
        let stylesheet =
            StyleSheet::parse(css, ParserOptions::default()).expect("CSS should parse");
        let file_uri = "file:///project/test.css";
        let mut incidents = Vec::new();
        for rule in &stylesheet.rules.0 {
            scan_rule_for_classes(rule, css, &re, file_uri, &mut incidents);
        }
        incidents
    }

    #[test]
    fn test_single_line_selector() {
        let css = ".container .pf-v5-c-button { color: red; }\n";
        let incidents = scan(css, "pf-v5-");
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("className"),
            Some(&serde_json::Value::String("pf-v5-c-button".to_string()))
        );
        assert_eq!(incidents[0].line_number, Some(1));
    }

    #[test]
    fn test_multi_line_selector_correct_line_number() {
        // Bug: classes on continuation lines got the rule's start line
        let css = ".container\n  .pf-v5-c-file-upload\n  .pf-v5-c-code-editor__main\n  .pf-v5-c-code-editor__code {\n  height: 100%;\n}\n";
        let incidents = scan(css, "pf-v5-");
        assert_eq!(incidents.len(), 3, "Should find 3 pf-v5 classes");

        // Each class should report its ACTUAL line, not the rule start (line 1)
        let lines: Vec<u32> = incidents.iter().map(|i| i.line_number.unwrap()).collect();
        assert!(
            lines.contains(&2),
            "pf-v5-c-file-upload should be on line 2, got {:?}",
            lines
        );
        assert!(
            lines.contains(&3),
            "pf-v5-c-code-editor__main should be on line 3, got {:?}",
            lines
        );
        assert!(
            lines.contains(&4),
            "pf-v5-c-code-editor__code should be on line 4, got {:?}",
            lines
        );
    }

    #[test]
    fn test_negation_pseudo_class() {
        // Bug: classes inside :not() were not detected
        let css = ".container > :not(.pf-v5-c-tabs) {\n  flex-grow: 1;\n}\n";
        let incidents = scan(css, "pf-v5-");
        assert_eq!(
            incidents.len(),
            1,
            "Should find pf-v5 inside :not() pseudo-class"
        );
        assert_eq!(
            incidents[0].variables.get("className"),
            Some(&serde_json::Value::String("pf-v5-c-tabs".to_string()))
        );
    }

    #[test]
    fn test_is_pseudo_class() {
        let css = ":is(.pf-v5-c-button, .pf-v5-c-link) { color: blue; }\n";
        let incidents = scan(css, "pf-v5-");
        assert_eq!(incidents.len(), 2, "Should find both classes inside :is()");
    }

    #[test]
    fn test_nested_negation_in_multi_line() {
        let css = ".drawer-tabs-container\n  > :not(.pf-v5-c-tabs) {\n  flex-grow: 1;\n}\n";
        let incidents = scan(css, "pf-v5-");
        assert_eq!(incidents.len(), 1);
        // The class should be reported on line 2 where :not(.pf-v5-c-tabs) appears
        assert_eq!(incidents[0].line_number, Some(2));
    }

    #[test]
    fn test_media_query_nested() {
        let css = "@media (min-width: 768px) {\n  .pf-v5-c-page {\n    padding: 0;\n  }\n}\n";
        let incidents = scan(css, "pf-v5-");
        assert_eq!(incidents.len(), 1);
    }
}

/// Emit an incident for a matched class name, locating its actual position
/// in the source text rather than using the rule's start location.
///
/// LightningCSS only provides the rule-level `loc` (the start of the style
/// rule), not per-component positions. For multi-line selectors like:
///
/// ```css
/// .container
///   .pf-v5-c-button
///   .pf-v5-c-icon { ... }
/// ```
///
/// the rule `loc` points to `.container` (line 1), but the matched classes
/// are on lines 2 and 3. We search forward from the rule start to find the
/// actual `.className` occurrence.
fn emit_class_incident(
    source: &str,
    file_uri: &str,
    loc: &CssLocation,
    class_name: &str,
    incidents: &mut Vec<Incident>,
) {
    // Build the search needle: ".className" (with the dot prefix)
    let needle = format!(".{}", class_name);

    // Compute byte offset of the rule's start line in the source
    let rule_start_offset = source
        .lines()
        .take(loc.line as usize)
        .map(|l| l.len() + 1) // +1 for newline
        .sum::<usize>();

    // Search for the class from the rule start. Use the first occurrence
    // that hasn't already been claimed by a prior incident on the same
    // rule (tracked by searching from an advancing offset).
    if let Some(rel) = source[rule_start_offset..].find(&needle) {
        let class_offset = rule_start_offset + rel + 1; // +1 to skip the dot
        let actual_line = source[..class_offset].matches('\n').count() as u32 + 1;
        let line_start = source[..class_offset].rfind('\n').map_or(0, |p| p + 1);
        let actual_col = (class_offset - line_start) as u32;

        let mut incident = Incident::new(
            file_uri.to_string(),
            actual_line,
            Location {
                start: Position {
                    line: actual_line - 1,
                    character: actual_col,
                },
                end: Position {
                    line: actual_line - 1,
                    character: actual_col + class_name.len() as u32,
                },
            },
        );
        incident.variables.insert(
            "className".into(),
            serde_json::Value::String(class_name.to_string()),
        );
        incident.code_snip = Some(extract_code_snip(source, actual_line, 3));
        incidents.push(incident);
    }
}

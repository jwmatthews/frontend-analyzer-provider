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
        if let Component::Class(class_name) = component {
            let name = class_name.as_ref();
            if pattern.is_match(name) {
                // lightningcss Location: line is 0-indexed, column is 1-indexed
                let line = loc.line + 1;
                let col = loc.column;

                let mut incident = Incident::new(
                    file_uri.to_string(),
                    line,
                    Location {
                        start: Position {
                            line: loc.line,
                            character: col.saturating_sub(1),
                        },
                        end: Position {
                            line: loc.line,
                            character: col.saturating_sub(1) + name.len() as u32,
                        },
                    },
                );
                incident.variables.insert(
                    "className".into(),
                    serde_json::Value::String(name.to_string()),
                );
                incident.code_snip = Some(extract_code_snip(source, line, 3));
                incidents.push(incident);
            }
        }
    }
}

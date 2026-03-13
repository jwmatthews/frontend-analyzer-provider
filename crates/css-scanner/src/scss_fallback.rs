//! Regex-based fallback for SCSS/LESS files that LightningCSS cannot parse.
//!
//! Uses line-by-line regex scanning to find class names and CSS variables.

use anyhow::Result;
use frontend_core::incident::{extract_code_snip, Incident, Location, Position};
use regex::Regex;
use std::path::Path;

/// Scan a SCSS/LESS file for CSS class name usage via regex.
pub fn scan_scss_classes(file_path: &Path, root: &Path, pattern: &Regex) -> Result<Vec<Incident>> {
    let source = std::fs::read_to_string(file_path)?;
    let file_uri = crate::scanner::path_to_uri(file_path, root);

    // Build a regex that looks for the class name in selector context
    // e.g., `.pf-m-expandable` or `&.pf-m-expandable`
    let class_re = Regex::new(&format!(r"\.{}", pattern.as_str()))?;

    let mut incidents = Vec::new();

    for (i, line) in source.lines().enumerate() {
        let line_num = (i + 1) as u32;

        // Check for class selectors
        if class_re.is_match(line) {
            if let Some(m) = class_re.find(line) {
                let mut incident = Incident::new(
                    file_uri.clone(),
                    line_num,
                    Location {
                        start: Position {
                            line: line_num - 1,
                            character: m.start() as u32,
                        },
                        end: Position {
                            line: line_num - 1,
                            character: m.end() as u32,
                        },
                    },
                );
                // Extract just the class name (without the leading dot)
                let class_name = &m.as_str()[1..]; // skip the '.'
                incident.variables.insert(
                    "className".into(),
                    serde_json::Value::String(class_name.to_string()),
                );
                incident.code_snip = Some(extract_code_snip(&source, line_num, 3));
                incidents.push(incident);
            }
        }

        // Also check for the class name appearing in string context (without dot prefix)
        if pattern.is_match(line) && !class_re.is_match(line) {
            if let Some(m) = pattern.find(line) {
                let mut incident = Incident::new(
                    file_uri.clone(),
                    line_num,
                    Location {
                        start: Position {
                            line: line_num - 1,
                            character: m.start() as u32,
                        },
                        end: Position {
                            line: line_num - 1,
                            character: m.end() as u32,
                        },
                    },
                );
                incident.variables.insert(
                    "matchingText".into(),
                    serde_json::Value::String(m.as_str().to_string()),
                );
                incident.code_snip = Some(extract_code_snip(&source, line_num, 3));
                incidents.push(incident);
            }
        }
    }

    Ok(incidents)
}

/// Scan a SCSS/LESS file for CSS variable usage via regex.
pub fn scan_scss_vars(file_path: &Path, root: &Path, pattern: &Regex) -> Result<Vec<Incident>> {
    let source = std::fs::read_to_string(file_path)?;
    let file_uri = crate::scanner::path_to_uri(file_path, root);

    let mut incidents = Vec::new();

    for (i, line) in source.lines().enumerate() {
        let line_num = (i + 1) as u32;

        if pattern.is_match(line) {
            if let Some(m) = pattern.find(line) {
                let mut incident = Incident::new(
                    file_uri.clone(),
                    line_num,
                    Location {
                        start: Position {
                            line: line_num - 1,
                            character: m.start() as u32,
                        },
                        end: Position {
                            line: line_num - 1,
                            character: m.end() as u32,
                        },
                    },
                );
                incident.variables.insert(
                    "variableName".into(),
                    serde_json::Value::String(m.as_str().to_string()),
                );
                incident.code_snip = Some(extract_code_snip(&source, line_num, 3));
                incidents.push(incident);
            }
        }
    }

    Ok(incidents)
}

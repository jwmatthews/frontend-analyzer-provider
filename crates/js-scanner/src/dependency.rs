//! package.json dependency analysis.
//!
//! Checks if a project has specific dependencies at specific version ranges.

use anyhow::Result;
use frontend_core::capabilities::DependencyCondition;
use frontend_core::incident::{Incident, Location, Position};
use regex::Regex;
use std::path::Path;

/// Parsed dependency from package.json.
#[derive(Debug, Clone)]
pub struct PackageDependency {
    pub name: String,
    pub version: String,
    pub dep_type: String, // "dependencies", "devDependencies", "peerDependencies"
    pub line_number: u32,
}

/// Check package.json for dependencies matching the condition.
pub fn check_dependencies(root: &Path, condition: &DependencyCondition) -> Result<Vec<Incident>> {
    let pkg_path = root.join("package.json");
    if !pkg_path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(&pkg_path)?;
    let pkg: serde_json::Value = serde_json::from_str(&content)?;
    let file_uri = format!("file://{}", pkg_path.display());

    let mut incidents = Vec::new();

    let dep_sections = ["dependencies", "devDependencies", "peerDependencies"];
    for section in &dep_sections {
        if let Some(deps) = pkg.get(section).and_then(|v| v.as_object()) {
            for (name, version_val) in deps {
                let version = version_val.as_str().unwrap_or_default();

                let name_matches = match (&condition.name, &condition.nameregex) {
                    (Some(exact), _) => name == exact,
                    (_, Some(re_str)) => {
                        let re = Regex::new(re_str)?;
                        re.is_match(name)
                    }
                    (None, None) => true,
                };

                if !name_matches {
                    continue;
                }

                // Find line number of this dependency in the JSON
                let line_number = find_key_line(&content, name);

                let mut incident = Incident::new(
                    file_uri.clone(),
                    line_number,
                    Location {
                        start: Position {
                            line: line_number.saturating_sub(1),
                            character: 0,
                        },
                        end: Position {
                            line: line_number.saturating_sub(1),
                            character: 0,
                        },
                    },
                );
                incident.variables.insert(
                    "dependencyName".into(),
                    serde_json::Value::String(name.clone()),
                );
                incident.variables.insert(
                    "dependencyVersion".into(),
                    serde_json::Value::String(version.to_string()),
                );
                incident.variables.insert(
                    "dependencyType".into(),
                    serde_json::Value::String(section.to_string()),
                );

                // Add code snippet
                incident.code_snip = Some(frontend_core::incident::extract_code_snip(
                    &content,
                    line_number,
                    3,
                ));

                incidents.push(incident);
            }
        }
    }

    Ok(incidents)
}

/// Find the line number of a JSON key in the source text.
fn find_key_line(content: &str, key: &str) -> u32 {
    let search = format!("\"{}\"", key);
    for (i, line) in content.lines().enumerate() {
        if line.contains(&search) {
            return (i + 1) as u32;
        }
    }
    1
}

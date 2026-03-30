//! package.json dependency analysis.
//!
//! Checks if a project has specific dependencies at specific version ranges.
//! Supports npm workspaces by walking workspace package.json files.

use anyhow::Result;
use frontend_core::capabilities::DependencyCondition;
use frontend_core::incident::{Incident, Location, Position};
use regex::Regex;
use std::path::{Path, PathBuf};

/// Parsed dependency from package.json.
#[derive(Debug, Clone)]
pub struct PackageDependency {
    pub name: String,
    pub version: String,
    pub dep_type: String, // "dependencies", "devDependencies", "peerDependencies"
    pub line_number: u32,
}

/// Check package.json files for dependencies matching the condition.
///
/// Walks the root package.json and any workspace package.json files.
pub fn check_dependencies(root: &Path, condition: &DependencyCondition) -> Result<Vec<Incident>> {
    let mut incidents = Vec::new();

    // Collect all package.json paths to check (root + workspaces)
    let pkg_paths = find_package_jsons(root);

    for pkg_path in &pkg_paths {
        let results = check_single_package_json(pkg_path, condition)?;
        incidents.extend(results);
    }

    Ok(incidents)
}

/// Find all package.json files to check: root + workspace members.
fn find_package_jsons(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let root_pkg = root.join("package.json");

    if !root_pkg.exists() {
        return paths;
    }

    // Check for workspaces field in root package.json
    if let Ok(content) = std::fs::read_to_string(&root_pkg) {
        if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(workspaces) = pkg.get("workspaces") {
                // workspaces can be an array of globs or an object with "packages" array
                let workspace_globs = match workspaces {
                    serde_json::Value::Array(arr) => arr
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>(),
                    serde_json::Value::Object(obj) => obj
                        .get("packages")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };

                for glob_pattern in &workspace_globs {
                    // Simple glob expansion: handle "client", "client/*", "packages/*"
                    let pattern = root.join(glob_pattern).join("package.json");
                    if let Ok(entries) = glob::glob(&pattern.to_string_lossy()) {
                        for entry in entries.flatten() {
                            if entry.exists() && entry != root_pkg {
                                paths.push(entry);
                            }
                        }
                    } else {
                        // Fallback: try as a direct directory
                        let direct = root.join(glob_pattern).join("package.json");
                        if direct.exists() && direct != root_pkg {
                            paths.push(direct);
                        }
                    }
                }
            }
        }
    }

    // Always include root package.json (check it last so workspace-specific
    // deps are found first)
    paths.push(root_pkg);

    paths
}

/// Check a single package.json for dependencies matching the condition.
fn check_single_package_json(
    pkg_path: &Path,
    condition: &DependencyCondition,
) -> Result<Vec<Incident>> {
    if !pkg_path.exists() {
        return Ok(Vec::new());
    }

    let content = std::fs::read_to_string(pkg_path)?;
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

                // Check version bounds
                if !version_in_bounds(version, condition) {
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

/// Check if a version string falls within the condition's bounds.
///
/// Strips npm version range prefixes (^, ~, >=, etc.) to extract the
/// base semver for comparison.
fn version_in_bounds(version_str: &str, condition: &DependencyCondition) -> bool {
    // If no bounds specified, all versions match
    if condition.upperbound.is_none() && condition.lowerbound.is_none() {
        return true;
    }

    let base = strip_npm_prefix(version_str);
    let parsed = match parse_loose_semver(base) {
        Some(v) => v,
        None => return true, // Can't parse -> don't filter, let it match
    };

    if let Some(ref ub) = condition.upperbound {
        if let Some(bound) = parse_loose_semver(strip_npm_prefix(ub)) {
            if parsed > bound {
                return false;
            }
        }
    }

    if let Some(ref lb) = condition.lowerbound {
        if let Some(bound) = parse_loose_semver(strip_npm_prefix(lb)) {
            if parsed < bound {
                return false;
            }
        }
    }

    true
}

/// Strip npm version range prefixes: ^, ~, >=, <=, >, <, =
fn strip_npm_prefix(version: &str) -> &str {
    let v = version.trim();
    if let Some(rest) = v.strip_prefix(">=") {
        rest.trim()
    } else if let Some(rest) = v.strip_prefix("<=") {
        rest.trim()
    } else if let Some(rest) = v.strip_prefix('^') {
        rest.trim()
    } else if let Some(rest) = v.strip_prefix('~') {
        rest.trim()
    } else if let Some(rest) = v.strip_prefix('>') {
        rest.trim()
    } else if let Some(rest) = v.strip_prefix('<') {
        rest.trim()
    } else if let Some(rest) = v.strip_prefix('=') {
        rest.trim()
    } else {
        v
    }
}

/// Loose semver parsing: extracts (major, minor, patch) from a version string.
/// Handles versions like "5.4.2", "6.0.0-alpha.1", "5.4.2-rc.1".
fn parse_loose_semver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim();
    // Strip prerelease/build metadata for comparison
    let version_part = s.split('-').next().unwrap_or(s);
    let version_part = version_part.split('+').next().unwrap_or(version_part);

    let parts: Vec<&str> = version_part.split('.').collect();
    let major = parts.first()?.parse::<u64>().ok()?;
    let minor = parts
        .get(1)
        .and_then(|p| p.parse::<u64>().ok())
        .unwrap_or(0);
    let patch = parts
        .get(2)
        .and_then(|p| p.parse::<u64>().ok())
        .unwrap_or(0);

    Some((major, minor, patch))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_npm_prefix() {
        assert_eq!(strip_npm_prefix("^6.4.1"), "6.4.1");
        assert_eq!(strip_npm_prefix("~5.4.2"), "5.4.2");
        assert_eq!(strip_npm_prefix(">=1.0.0"), "1.0.0");
        assert_eq!(strip_npm_prefix("5.4.2"), "5.4.2");
    }

    #[test]
    fn test_parse_loose_semver() {
        assert_eq!(parse_loose_semver("5.4.2"), Some((5, 4, 2)));
        assert_eq!(parse_loose_semver("6.0.0-alpha.1"), Some((6, 0, 0)));
        assert_eq!(parse_loose_semver("6.4.1"), Some((6, 4, 1)));
    }

    #[test]
    fn test_version_in_bounds() {
        let cond = DependencyCondition {
            name: None,
            nameregex: None,
            upperbound: Some("5.99.99".into()),
            lowerbound: None,
        };
        // v5 should match (<=5.99.99)
        assert!(version_in_bounds("^5.4.14", &cond));
        assert!(version_in_bounds("5.4.2", &cond));
        // v6 should NOT match (>5.99.99)
        assert!(!version_in_bounds("^6.4.1", &cond));
        assert!(!version_in_bounds("6.0.0", &cond));
    }
}

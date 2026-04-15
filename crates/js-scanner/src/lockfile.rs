//! Lockfile parsing for npm and yarn (berry).
//!
//! Parses `package-lock.json` (npm v2/v3) and `yarn.lock` (berry/v2+) to
//! extract resolved dependency entries. Used by the dependency scanner to
//! find packages whose transitive dependencies conflict with the project's
//! declared versions.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use tracing;

/// A resolved dependency entry from a lockfile.
#[derive(Debug, Clone)]
pub struct LockfileEntry {
    /// The npm package name (e.g., `@patternfly/react-topology`).
    pub name: String,
    /// The resolved version (e.g., `5.2.1`).
    pub version: String,
    /// Direct dependencies declared by this package: name → version constraint.
    pub dependencies: HashMap<String, String>,
}

/// Parse the project's lockfile and return all resolved entries.
///
/// Automatically detects the lockfile format by checking for `yarn.lock`
/// (berry) and `package-lock.json` (npm) in order.
pub fn parse_lockfile(root: &Path) -> Result<Vec<LockfileEntry>> {
    let yarn_lock = root.join("yarn.lock");
    let npm_lock = root.join("package-lock.json");

    if yarn_lock.exists() {
        let content = std::fs::read_to_string(&yarn_lock)
            .with_context(|| format!("Failed to read {}", yarn_lock.display()))?;
        parse_yarn_lock_berry(&content)
    } else if npm_lock.exists() {
        let content = std::fs::read_to_string(&npm_lock)
            .with_context(|| format!("Failed to read {}", npm_lock.display()))?;
        parse_package_lock_json(&content)
    } else {
        tracing::debug!("No lockfile found in {}", root.display());
        Ok(Vec::new())
    }
}

/// Find all lockfile entries that depend on `target_package` at a version
/// whose major component is within the given bounds.
///
/// Returns entries that are dependents of the target, NOT the target itself.
pub fn find_dependents<'a>(
    entries: &'a [LockfileEntry],
    target_package: &str,
    upperbound: Option<&str>,
) -> Vec<&'a LockfileEntry> {
    entries
        .iter()
        .filter(|entry| {
            // Don't return the target package itself as a dependent
            if entry.name == target_package {
                return false;
            }

            // Check if this entry depends on the target package
            if let Some(constraint) = entry.dependencies.get(target_package) {
                // Check if the constraint falls within bounds
                if let Some(ub) = upperbound {
                    constraint_within_upperbound(constraint, ub)
                } else {
                    true
                }
            } else {
                false
            }
        })
        .collect()
}

/// Check if a version constraint's base version is within the upperbound.
///
/// For example, constraint `^5.1.1` with upperbound `5.99.99` → true
/// because 5.1.1 <= 5.99.99. Constraint `^6.0.0` → false.
fn constraint_within_upperbound(constraint: &str, upperbound: &str) -> bool {
    let constraint_base = strip_prefix(constraint);
    let bound_base = strip_prefix(upperbound);

    match (parse_semver(constraint_base), parse_semver(bound_base)) {
        (Some(c), Some(b)) => c <= b,
        _ => false,
    }
}

fn strip_prefix(s: &str) -> &str {
    let s = s.trim();
    // Strip yarn's "npm:" prefix (e.g., "npm:^5.1.1")
    let s = s.strip_prefix("npm:").unwrap_or(s);
    // Strip semver range operators
    if let Some(rest) = s.strip_prefix(">=") {
        rest.trim()
    } else if let Some(rest) = s.strip_prefix("<=") {
        rest.trim()
    } else if let Some(rest) = s.strip_prefix('^') {
        rest.trim()
    } else if let Some(rest) = s.strip_prefix('~') {
        rest.trim()
    } else if let Some(rest) = s.strip_prefix('>') {
        rest.trim()
    } else if let Some(rest) = s.strip_prefix('<') {
        rest.trim()
    } else if let Some(rest) = s.strip_prefix('=') {
        rest.trim()
    } else {
        s
    }
}

fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim();
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

// ── package-lock.json (npm v2/v3) ────────────────────────────────────────

/// Parse npm's `package-lock.json` (lockfileVersion 2 or 3).
///
/// The `packages` object has keys like `node_modules/@patternfly/react-core`
/// with `version` and optional `dependencies` fields.
fn parse_package_lock_json(content: &str) -> Result<Vec<LockfileEntry>> {
    let lock: serde_json::Value =
        serde_json::from_str(content).context("Failed to parse package-lock.json")?;

    let mut entries = Vec::new();

    // lockfileVersion 2/3 uses "packages"
    if let Some(packages) = lock.get("packages").and_then(|v| v.as_object()) {
        for (key, value) in packages {
            // Keys are like "node_modules/@patternfly/react-topology"
            // or "" (the root project) or nested node_modules
            let name = extract_package_name_from_npm_key(key);
            if name.is_empty() {
                continue; // Skip root entry
            }

            let version = value
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            let mut dependencies = HashMap::new();

            // Collect from both "dependencies" and "peerDependencies"
            for dep_section in &["dependencies", "peerDependencies"] {
                if let Some(deps) = value.get(*dep_section).and_then(|v| v.as_object()) {
                    for (dep_name, dep_ver) in deps {
                        let ver = dep_ver.as_str().unwrap_or_default().to_string();
                        dependencies.entry(dep_name.clone()).or_insert(ver);
                    }
                }
            }

            if !version.is_empty() {
                entries.push(LockfileEntry {
                    name,
                    version,
                    dependencies,
                });
            }
        }
    }

    tracing::debug!(entries = entries.len(), "Parsed package-lock.json");

    Ok(entries)
}

/// Extract the package name from a package-lock.json key.
///
/// `node_modules/@patternfly/react-core` → `@patternfly/react-core`
/// `node_modules/@scope/pkg/node_modules/dep` → `dep`
/// `` → `` (root entry, skip)
fn extract_package_name_from_npm_key(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    // Take the last node_modules segment to get the actual package name
    // Handle scoped packages: "node_modules/@scope/name"
    if let Some(pos) = key.rfind("node_modules/") {
        let after = &key[pos + "node_modules/".len()..];
        after.to_string()
    } else {
        key.to_string()
    }
}

// ── yarn.lock (berry/v2+) ────────────────────────────────────────────────

/// Parse yarn berry's `yarn.lock` format.
///
/// Entries look like:
/// ```text
/// "@patternfly/react-topology@npm:5.2.1":
///   version: 5.2.1
///   resolution: "@patternfly/react-topology@npm:5.2.1"
///   dependencies:
///     "@patternfly/react-core": "npm:^5.1.1"
///     ...
/// ```
fn parse_yarn_lock_berry(content: &str) -> Result<Vec<LockfileEntry>> {
    let mut entries = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_version: Option<String> = None;
    let mut current_deps: HashMap<String, String> = HashMap::new();
    let mut in_dependencies = false;
    let mut in_peer_dependencies = false;

    for line in content.lines() {
        // Top-level entry: starts with `"` and ends with `:`
        // e.g., `"@patternfly/react-topology@npm:5.2.1":`
        if line.starts_with('"') && line.ends_with(':') && !line.starts_with("  ") {
            // Save previous entry if we have one
            if let (Some(name), Some(version)) = (current_name.take(), current_version.take()) {
                entries.push(LockfileEntry {
                    name,
                    version,
                    dependencies: std::mem::take(&mut current_deps),
                });
            }

            current_name = extract_package_name_from_yarn_key(line);
            current_version = None;
            in_dependencies = false;
            in_peer_dependencies = false;
            continue;
        }

        let trimmed = line.trim();

        // version: X.Y.Z
        if trimmed.starts_with("version: ") && current_name.is_some() {
            current_version = Some(trimmed.trim_start_matches("version: ").to_string());
            in_dependencies = false;
            in_peer_dependencies = false;
            continue;
        }

        // dependencies: or peerDependencies: section start
        if trimmed == "dependencies:" {
            in_dependencies = true;
            in_peer_dependencies = false;
            continue;
        }
        if trimmed == "peerDependencies:" {
            in_peer_dependencies = true;
            in_dependencies = false;
            continue;
        }

        // End of deps section: any non-indented line that isn't a dep entry
        if (in_dependencies || in_peer_dependencies) && !line.starts_with("    ") {
            in_dependencies = false;
            in_peer_dependencies = false;
            // Don't skip — this might be another section header like "checksum:"
            continue;
        }

        // Parse dependency entry: `    "@patternfly/react-core": "npm:^5.1.1"`
        if (in_dependencies || in_peer_dependencies) && line.starts_with("    ") {
            if let Some((dep_name, dep_ver)) = parse_yarn_dep_line(trimmed) {
                current_deps.entry(dep_name).or_insert(dep_ver);
            }
        }
    }

    // Don't forget the last entry
    if let (Some(name), Some(version)) = (current_name, current_version) {
        entries.push(LockfileEntry {
            name,
            version,
            dependencies: current_deps,
        });
    }

    tracing::debug!(entries = entries.len(), "Parsed yarn.lock (berry)");

    Ok(entries)
}

/// Extract the package name from a yarn.lock key line.
///
/// `"@patternfly/react-topology@npm:5.2.1":` → `@patternfly/react-topology`
/// `"@patternfly/react-core@npm:^5.1.1, @patternfly/react-core@npm:^5.4.1":` → `@patternfly/react-core`
fn extract_package_name_from_yarn_key(line: &str) -> Option<String> {
    // Remove leading `"` and trailing `":`
    let trimmed = line.trim_start_matches('"');
    // Take the first descriptor (before any comma for multiple ranges)
    let first = trimmed.split(',').next().unwrap_or(trimmed);
    // Find the @npm: or @workspace: separator
    // For scoped packages like @patternfly/react-core@npm:^5.1.1
    // We need to find the LAST @ that's followed by npm: or workspace:
    let name = if let Some(pos) = first.rfind("@npm:") {
        &first[..pos]
    } else if let Some(pos) = first.rfind("@workspace:") {
        &first[..pos]
    } else {
        // Fallback: find the last @ that's not at the start
        let bytes = first.as_bytes();
        let mut last_at = None;
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'@' && i > 0 {
                last_at = Some(i);
            }
        }
        if let Some(pos) = last_at {
            &first[..pos]
        } else {
            return None;
        }
    };

    let name = name.trim_matches('"').trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Parse a yarn dependency line like `"@patternfly/react-core": "npm:^5.1.1"`
fn parse_yarn_dep_line(line: &str) -> Option<(String, String)> {
    // Format: `"name": "npm:constraint"` or `"name": "constraint"`
    let parts: Vec<&str> = line.splitn(2, ": ").collect();
    if parts.len() != 2 {
        return None;
    }
    let name = parts[0].trim().trim_matches('"').to_string();
    let version = parts[1].trim().trim_matches('"').to_string();
    if name.is_empty() {
        return None;
    }
    Some((name, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── npm package-lock.json tests ──────────────────────────────────

    #[test]
    fn test_parse_package_lock_json_basic() {
        let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": {
                    "name": "my-project",
                    "version": "1.0.0"
                },
                "node_modules/@patternfly/react-core": {
                    "version": "6.4.1",
                    "dependencies": {
                        "@patternfly/react-icons": "^6.4.0"
                    }
                },
                "node_modules/@patternfly/react-topology": {
                    "version": "5.2.1",
                    "dependencies": {
                        "@patternfly/react-core": "^5.1.1",
                        "@patternfly/react-icons": "^5.1.1"
                    }
                }
            }
        }"#;

        let entries = parse_package_lock_json(content).unwrap();
        assert_eq!(entries.len(), 2); // root "" is skipped

        let topology = entries
            .iter()
            .find(|e| e.name == "@patternfly/react-topology")
            .unwrap();
        assert_eq!(topology.version, "5.2.1");
        assert_eq!(
            topology.dependencies.get("@patternfly/react-core").unwrap(),
            "^5.1.1"
        );
    }

    #[test]
    fn test_parse_package_lock_json_peer_deps() {
        let content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "node_modules/some-plugin": {
                    "version": "2.0.0",
                    "peerDependencies": {
                        "@patternfly/react-core": "^5.0.0"
                    }
                }
            }
        }"#;

        let entries = parse_package_lock_json(content).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0]
                .dependencies
                .get("@patternfly/react-core")
                .unwrap(),
            "^5.0.0"
        );
    }

    #[test]
    fn test_extract_package_name_from_npm_key() {
        assert_eq!(
            extract_package_name_from_npm_key("node_modules/@patternfly/react-core"),
            "@patternfly/react-core"
        );
        assert_eq!(
            extract_package_name_from_npm_key("node_modules/lodash"),
            "lodash"
        );
        assert_eq!(
            extract_package_name_from_npm_key(
                "node_modules/@patternfly/react-core/node_modules/tslib"
            ),
            "tslib"
        );
        assert_eq!(extract_package_name_from_npm_key(""), "");
    }

    // ── yarn.lock berry tests ────────────────────────────────────────

    #[test]
    fn test_parse_yarn_lock_berry_basic() {
        let content = r#"# yarn lockfile v1

__metadata:
  version: 8

"@patternfly/react-core@npm:^6.4.1":
  version: 6.4.1
  resolution: "@patternfly/react-core@npm:6.4.1"
  dependencies:
    "@patternfly/react-icons": "npm:^6.4.0"
    "@patternfly/react-styles": "npm:^6.4.0"
  checksum: abc123
  languageName: node
  linkType: hard

"@patternfly/react-topology@npm:5.2.1":
  version: 5.2.1
  resolution: "@patternfly/react-topology@npm:5.2.1"
  dependencies:
    "@patternfly/react-core": "npm:^5.1.1"
    "@patternfly/react-icons": "npm:^5.1.1"
  checksum: def456
  languageName: node
  linkType: hard
"#;

        let entries = parse_yarn_lock_berry(content).unwrap();
        // __metadata is skipped (no version field)
        assert_eq!(entries.len(), 2);

        let topology = entries
            .iter()
            .find(|e| e.name == "@patternfly/react-topology")
            .unwrap();
        assert_eq!(topology.version, "5.2.1");
        assert_eq!(
            topology.dependencies.get("@patternfly/react-core").unwrap(),
            "npm:^5.1.1"
        );

        let core = entries
            .iter()
            .find(|e| e.name == "@patternfly/react-core")
            .unwrap();
        assert_eq!(core.version, "6.4.1");
    }

    #[test]
    fn test_parse_yarn_lock_berry_multiple_ranges() {
        // yarn.lock can have multiple version ranges for the same package
        let content = r#"
"@patternfly/react-core@npm:^5.1.1, @patternfly/react-core@npm:^5.4.1":
  version: 5.4.14
  resolution: "@patternfly/react-core@npm:5.4.14"
  dependencies:
    "@patternfly/react-icons": "npm:^5.4.0"
  languageName: node
  linkType: hard
"#;

        let entries = parse_yarn_lock_berry(content).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "@patternfly/react-core");
        assert_eq!(entries[0].version, "5.4.14");
    }

    #[test]
    fn test_parse_yarn_lock_berry_peer_deps() {
        let content = r#"
"some-plugin@npm:2.0.0":
  version: 2.0.0
  resolution: "some-plugin@npm:2.0.0"
  dependencies:
    lodash: "npm:^4.0.0"
  peerDependencies:
    "@patternfly/react-core": "npm:^5.0.0"
  languageName: node
  linkType: hard
"#;

        let entries = parse_yarn_lock_berry(content).unwrap();
        assert_eq!(entries.len(), 1);
        // Both dependencies and peerDependencies are collected
        assert_eq!(
            entries[0]
                .dependencies
                .get("@patternfly/react-core")
                .unwrap(),
            "npm:^5.0.0"
        );
        assert_eq!(entries[0].dependencies.get("lodash").unwrap(), "npm:^4.0.0");
    }

    #[test]
    fn test_extract_package_name_from_yarn_key() {
        assert_eq!(
            extract_package_name_from_yarn_key(r#""@patternfly/react-core@npm:^5.1.1":"#),
            Some("@patternfly/react-core".into())
        );
        assert_eq!(
            extract_package_name_from_yarn_key(r#""lodash@npm:^4.17.21":"#),
            Some("lodash".into())
        );
        assert_eq!(
            extract_package_name_from_yarn_key(
                r#""@patternfly/react-core@npm:^5.1.1, @patternfly/react-core@npm:^5.4.1":"#
            ),
            Some("@patternfly/react-core".into())
        );
    }

    // ── find_dependents tests ────────────────────────────────────────

    #[test]
    fn test_find_dependents_basic() {
        let entries = vec![
            LockfileEntry {
                name: "@patternfly/react-core".into(),
                version: "6.4.1".into(),
                dependencies: HashMap::new(),
            },
            LockfileEntry {
                name: "@patternfly/react-topology".into(),
                version: "5.2.1".into(),
                dependencies: HashMap::from([("@patternfly/react-core".into(), "^5.1.1".into())]),
            },
            LockfileEntry {
                name: "@patternfly/react-table".into(),
                version: "6.4.1".into(),
                dependencies: HashMap::from([("@patternfly/react-core".into(), "^6.0.0".into())]),
            },
        ];

        // upperbound 5.99.99: only topology matches (its dep on react-core is ^5.1.1)
        let deps = find_dependents(&entries, "@patternfly/react-core", Some("5.99.99"));
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "@patternfly/react-topology");

        // No upperbound: both topology and table match
        let deps = find_dependents(&entries, "@patternfly/react-core", None);
        assert_eq!(deps.len(), 2);
    }

    #[test]
    fn test_find_dependents_skips_self() {
        let entries = vec![LockfileEntry {
            name: "@patternfly/react-core".into(),
            version: "5.4.14".into(),
            dependencies: HashMap::from([(
                "@patternfly/react-core".into(), // self-reference (shouldn't happen but guard)
                "^5.0.0".into(),
            )]),
        }];

        let deps = find_dependents(&entries, "@patternfly/react-core", Some("5.99.99"));
        assert!(deps.is_empty());
    }

    #[test]
    fn test_find_dependents_yarn_npm_prefix() {
        let entries = vec![LockfileEntry {
            name: "@patternfly/react-topology".into(),
            version: "5.2.1".into(),
            dependencies: HashMap::from([(
                "@patternfly/react-core".into(),
                "npm:^5.1.1".into(), // yarn format with npm: prefix
            )]),
        }];

        let deps = find_dependents(&entries, "@patternfly/react-core", Some("5.99.99"));
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "@patternfly/react-topology");
    }

    #[test]
    fn test_constraint_within_upperbound() {
        // v5 constraints within 5.99.99 bound
        assert!(constraint_within_upperbound("^5.1.1", "5.99.99"));
        assert!(constraint_within_upperbound("^5.4.14", "5.99.99"));
        assert!(constraint_within_upperbound("~5.0.0", "5.99.99"));
        assert!(constraint_within_upperbound("npm:^5.1.1", "5.99.99"));

        // v6 constraints above 5.99.99 bound
        assert!(!constraint_within_upperbound("^6.0.0", "5.99.99"));
        assert!(!constraint_within_upperbound("^6.4.1", "5.99.99"));
        assert!(!constraint_within_upperbound("npm:^6.0.0", "5.99.99"));
    }
}

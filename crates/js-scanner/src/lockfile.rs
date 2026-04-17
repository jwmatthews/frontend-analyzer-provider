//! Lockfile parsing for npm, yarn (berry), and pnpm.
//!
//! Parses `package-lock.json` (npm v2/v3), `yarn.lock` (berry/v2+), and
//! `pnpm-lock.yaml` (v6/v9) to extract resolved dependency entries. Used
//! by the dependency scanner and `GetDependencies` RPC to surface both
//! direct and transitive dependencies for rule matching.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing;

/// A resolved dependency entry from a lockfile.
#[derive(Debug, Clone)]
pub struct LockfileEntry {
    /// The npm package name (e.g., `@patternfly/react-topology`).
    pub name: String,
    /// The resolved version (e.g., `5.2.1`).
    pub version: String,
    /// All dependencies (regular + peer) declared by this package: name → version constraint.
    /// Peer dependencies are merged here for backward compatibility with `find_dependents`.
    pub dependencies: HashMap<String, String>,
    /// Peer dependencies only: name → version constraint.
    /// A subset of `dependencies` — entries here also appear in `dependencies`.
    /// Used by `GetDependencies` to surface peer dependency constraints as
    /// separate dependency entries so kantra can detect version incompatibilities.
    pub peer_dependencies: HashMap<String, String>,
}

/// Lockfile names in priority order.
const LOCKFILE_NAMES: &[&str] = &["yarn.lock", "package-lock.json", "pnpm-lock.yaml"];

/// Parse the project's lockfile and return all resolved entries.
///
/// Automatically detects the lockfile format by checking for `yarn.lock`
/// (berry), `package-lock.json` (npm), and `pnpm-lock.yaml` (pnpm) in order.
pub fn parse_lockfile(root: &Path) -> Result<Vec<LockfileEntry>> {
    parse_lockfile_at(root)
}

/// Parse a single lockfile from the given directory, if one exists.
fn parse_lockfile_at(dir: &Path) -> Result<Vec<LockfileEntry>> {
    let yarn_lock = dir.join("yarn.lock");
    let npm_lock = dir.join("package-lock.json");
    let pnpm_lock = dir.join("pnpm-lock.yaml");

    if yarn_lock.exists() {
        let content = std::fs::read_to_string(&yarn_lock)
            .with_context(|| format!("Failed to read {}", yarn_lock.display()))?;
        parse_yarn_lock_berry(&content)
    } else if npm_lock.exists() {
        let content = std::fs::read_to_string(&npm_lock)
            .with_context(|| format!("Failed to read {}", npm_lock.display()))?;
        parse_package_lock_json(&content)
    } else if pnpm_lock.exists() {
        let content = std::fs::read_to_string(&pnpm_lock)
            .with_context(|| format!("Failed to read {}", pnpm_lock.display()))?;
        parse_pnpm_lock_yaml(&content)
    } else {
        tracing::debug!("No lockfile found in {}", dir.display());
        Ok(Vec::new())
    }
}

/// Discover and parse ALL lockfiles in the project.
///
/// Checks the project root first — this covers standard monorepos where
/// npm, yarn, and pnpm all produce a single lockfile at the workspace root.
/// If no lockfile is found at the root, falls back to checking each
/// `package.json` location, which handles multi-project repos where
/// independent sub-projects each have their own lockfile.
///
/// Returns the path to each discovered lockfile alongside its parsed entries.
pub fn parse_all_lockfiles(
    root: &Path,
    pkg_paths: &[PathBuf],
) -> Result<(Vec<LockfileEntry>, Vec<PathBuf>)> {
    // Try root first (standard workspace monorepo case)
    let root_entries = parse_lockfile_at(root)?;
    if !root_entries.is_empty() {
        let lockfile_path = discover_lockfile_path(root);
        let paths = lockfile_path.into_iter().collect();
        tracing::info!(
            entries = root_entries.len(),
            "Parsed lockfile at project root"
        );
        return Ok((root_entries, paths));
    }

    // Fallback: check each package.json's parent directory for a lockfile.
    // This handles multi-project repos and pnpm's per-workspace lockfiles.
    let mut all_entries = Vec::new();
    let mut all_paths = Vec::new();
    let mut seen = HashSet::new();

    for pkg_path in pkg_paths {
        let dir = match pkg_path.parent() {
            Some(d) => d,
            None => continue,
        };

        // Skip if we already checked this directory
        if !seen.insert(dir.to_path_buf()) {
            continue;
        }

        // Skip if this is the root (already checked)
        if dir == root {
            continue;
        }

        let entries = parse_lockfile_at(dir)?;
        if !entries.is_empty() {
            tracing::info!(
                dir = %dir.display(),
                entries = entries.len(),
                "Parsed lockfile in sub-project"
            );
            if let Some(p) = discover_lockfile_path(dir) {
                all_paths.push(p);
            }
            all_entries.extend(entries);
        }
    }

    // Dedup entries by (name, version) across multiple lockfiles
    if all_paths.len() > 1 {
        let mut dedup_set = HashSet::new();
        all_entries.retain(|e| dedup_set.insert((e.name.clone(), e.version.clone())));
    }

    tracing::info!(
        entries = all_entries.len(),
        lockfiles = all_paths.len(),
        "Parsed lockfiles from sub-projects"
    );

    Ok((all_entries, all_paths))
}

/// Find which lockfile exists in a directory, if any.
fn discover_lockfile_path(dir: &Path) -> Option<PathBuf> {
    for name in LOCKFILE_NAMES {
        let p = dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
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
            let mut peer_dependencies = HashMap::new();

            // Collect from both "dependencies" and "peerDependencies"
            for dep_section in &["dependencies", "peerDependencies"] {
                if let Some(deps) = value.get(*dep_section).and_then(|v| v.as_object()) {
                    let is_peer = *dep_section == "peerDependencies";
                    for (dep_name, dep_ver) in deps {
                        let ver = dep_ver.as_str().unwrap_or_default().to_string();
                        dependencies.entry(dep_name.clone()).or_insert(ver.clone());
                        if is_peer {
                            peer_dependencies.entry(dep_name.clone()).or_insert(ver);
                        }
                    }
                }
            }

            if !version.is_empty() {
                entries.push(LockfileEntry {
                    name,
                    version,
                    dependencies,
                    peer_dependencies,
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
    let mut current_peer_deps: HashMap<String, String> = HashMap::new();
    let mut in_dependencies = false;
    let mut in_peer_dependencies = false;

    for line in content.lines() {
        // Top-level entry: starts with `"` and ends with `:` and !line.starts_with("  ")
        // e.g., `"@patternfly/react-topology@npm:5.2.1":`
        if line.starts_with('"') && line.ends_with(':') && !line.starts_with("  ") {
            // Save previous entry if we have one
            if let (Some(name), Some(version)) = (current_name.take(), current_version.take()) {
                entries.push(LockfileEntry {
                    name,
                    version,
                    dependencies: std::mem::take(&mut current_deps),
                    peer_dependencies: std::mem::take(&mut current_peer_deps),
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
                // Always add to merged dependencies map
                current_deps
                    .entry(dep_name.clone())
                    .or_insert(dep_ver.clone());
                // Also track peer deps separately
                if in_peer_dependencies {
                    current_peer_deps.entry(dep_name).or_insert(dep_ver);
                }
            }
        }
    }

    // Don't forget the last entry
    if let (Some(name), Some(version)) = (current_name, current_version) {
        entries.push(LockfileEntry {
            name,
            version,
            dependencies: current_deps,
            peer_dependencies: current_peer_deps,
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

// ── pnpm-lock.yaml (v6 / v9) ────────────────────────────────────────────

/// Parse pnpm's `pnpm-lock.yaml` (lockfileVersion 6.0 or 9.0).
///
/// Supports two formats:
/// - **v6** (pnpm 8): packages section contains both metadata and
///   dependencies, keys have leading `/`.
/// - **v9** (pnpm 9+): packages section has metadata only, dependencies
///   live in a separate `snapshots` section, no leading `/`.
fn parse_pnpm_lock_yaml(content: &str) -> Result<Vec<LockfileEntry>> {
    let lock: serde_yaml::Value =
        serde_yaml::from_str(content).context("Failed to parse pnpm-lock.yaml")?;

    let version_str = lock
        .get("lockfileVersion")
        .map(|v| match v {
            serde_yaml::Value::String(s) => s.clone(),
            serde_yaml::Value::Number(n) => n.to_string(),
            _ => String::new(),
        })
        .unwrap_or_default();

    if version_str.starts_with('9') {
        parse_pnpm_v9(&lock)
    } else {
        // v6 and any other version we treat as v6 format
        parse_pnpm_v6(&lock)
    }
}

/// Parse pnpm lockfile v6 (pnpm 8).
///
/// Keys in `packages` look like:
/// `/@patternfly/react-core@5.1.1(react-dom@18.2.0)(react@18.2.0)`
///
/// Each entry has `dependencies` and/or `peerDependencies` inline.
fn parse_pnpm_v6(lock: &serde_yaml::Value) -> Result<Vec<LockfileEntry>> {
    let mut entries = Vec::new();

    let packages = match lock.get("packages").and_then(|v| v.as_mapping()) {
        Some(m) => m,
        None => {
            tracing::debug!("No 'packages' section in pnpm-lock.yaml v6");
            return Ok(entries);
        }
    };

    for (key_val, value) in packages {
        let key = match key_val.as_str() {
            Some(k) => k,
            None => continue,
        };

        // Strip leading '/' (v6 format)
        let key = key.strip_prefix('/').unwrap_or(key);

        let (name, version) = match extract_name_version_from_pnpm_key(key) {
            Some(nv) => nv,
            None => continue,
        };

        let (dependencies, peer_dependencies) = collect_pnpm_dependencies(value);

        if !version.is_empty() {
            entries.push(LockfileEntry {
                name,
                version,
                dependencies,
                peer_dependencies,
            });
        }
    }

    tracing::debug!(entries = entries.len(), "Parsed pnpm-lock.yaml v6");
    Ok(entries)
}

/// Parse pnpm lockfile v9 (pnpm 9+).
///
/// `packages` has simple keys like `@patternfly/react-core@5.1.1` with
/// metadata only (resolution, peerDependencies declarations).
///
/// `snapshots` has keys with peer suffixes like
/// `@patternfly/react-core@5.1.1(react-dom@18.2.0)(react@18.2.0)` and
/// contains the actual resolved dependency tree.
fn parse_pnpm_v9(lock: &serde_yaml::Value) -> Result<Vec<LockfileEntry>> {
    let mut entries = Vec::new();

    // First pass: collect all name+version pairs from `packages`
    let packages = lock.get("packages").and_then(|v| v.as_mapping());
    let snapshots = lock.get("snapshots").and_then(|v| v.as_mapping());

    // Build a map of name+version → LockfileEntry from packages keys
    let mut entry_map: HashMap<(String, String), LockfileEntry> = HashMap::new();

    if let Some(pkgs) = packages {
        for (key_val, _value) in pkgs {
            let key = match key_val.as_str() {
                Some(k) => k,
                None => continue,
            };

            let (name, version) = match extract_name_version_from_pnpm_key(key) {
                Some(nv) => nv,
                None => continue,
            };

            if !version.is_empty() {
                entry_map
                    .entry((name.clone(), version.clone()))
                    .or_insert_with(|| LockfileEntry {
                        name,
                        version,
                        dependencies: HashMap::new(),
                        peer_dependencies: HashMap::new(),
                    });
            }
        }
    }

    // Second pass: enrich with dependencies from `snapshots`
    if let Some(snaps) = snapshots {
        for (key_val, value) in snaps {
            let key = match key_val.as_str() {
                Some(k) => k,
                None => continue,
            };

            let (name, version) = match extract_name_version_from_pnpm_key(key) {
                Some(nv) => nv,
                None => continue,
            };

            let (deps, peer_deps) = collect_pnpm_dependencies(value);

            // Merge deps into the entry (snapshots may have multiple peer
            // variants for the same package; we take the union of deps)
            if let Some(entry) = entry_map.get_mut(&(name.clone(), version.clone())) {
                for (dep_name, dep_ver) in deps {
                    entry.dependencies.entry(dep_name).or_insert(dep_ver);
                }
                for (dep_name, dep_ver) in peer_deps {
                    entry.peer_dependencies.entry(dep_name).or_insert(dep_ver);
                }
            } else {
                // Snapshot for a package not in `packages` — can happen for
                // packages that are only transitive. Create an entry.
                if !version.is_empty() {
                    entry_map.insert(
                        (name.clone(), version.clone()),
                        LockfileEntry {
                            name,
                            version,
                            dependencies: deps,
                            peer_dependencies: peer_deps,
                        },
                    );
                }
            }
        }
    }

    entries.extend(entry_map.into_values());
    tracing::debug!(entries = entries.len(), "Parsed pnpm-lock.yaml v9");
    Ok(entries)
}

/// Extract package name and version from a pnpm lockfile key.
///
/// Handles both v6 (leading `/` already stripped) and v9 formats.
///
/// Examples:
/// - `@patternfly/react-core@5.1.1` → `("@patternfly/react-core", "5.1.1")`
/// - `@patternfly/react-core@5.1.1(react@18.2.0)` → `("@patternfly/react-core", "5.1.1")`
/// - `lodash@4.17.21` → `("lodash", "4.17.21")`
fn extract_name_version_from_pnpm_key(key: &str) -> Option<(String, String)> {
    if key.is_empty() {
        return None;
    }

    // For scoped packages (@scope/name@version), find the version-separating @.
    // It's the @ after the first `/` in a scoped name, or the first @ for unscoped.
    let version_at = if key.starts_with('@') {
        // Scoped: find the `/` that ends the scope, then find the next `@`
        let slash_pos = key.find('/')?;
        let rest = &key[slash_pos + 1..];
        rest.find('@').map(|p| slash_pos + 1 + p)
    } else {
        // Unscoped: first `@` is the version separator
        key.find('@')
    };

    let at_pos = version_at?;
    let name = &key[..at_pos];
    let version_and_peers = &key[at_pos + 1..];

    // Strip peer suffix: everything from the first `(` onward
    let version = match version_and_peers.find('(') {
        Some(paren_pos) => &version_and_peers[..paren_pos],
        None => version_and_peers,
    };

    let name = name.trim();
    let version = version.trim();

    if name.is_empty() || version.is_empty() {
        return None;
    }

    Some((name.to_string(), version.to_string()))
}

/// Collect dependencies from a pnpm package/snapshot entry's YAML value.
///
/// Reads both `dependencies` and `peerDependencies` maps. Strips peer
/// suffixes from version values (e.g., `5.1.1(react@18.2.0)` → `5.1.1`).
///
/// Returns `(all_deps, peer_deps_only)` where `all_deps` is the merged map
/// and `peer_deps_only` contains only entries from `peerDependencies`.
fn collect_pnpm_dependencies(
    value: &serde_yaml::Value,
) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut deps = HashMap::new();
    let mut peer_deps = HashMap::new();

    for section in &["dependencies", "peerDependencies", "optionalDependencies"] {
        if let Some(dep_map) = value.get(*section).and_then(|v| v.as_mapping()) {
            let is_peer = *section == "peerDependencies";
            for (dep_key, dep_val) in dep_map {
                let dep_name = match dep_key.as_str() {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                let raw_ver = match dep_val {
                    serde_yaml::Value::String(s) => s.clone(),
                    serde_yaml::Value::Number(n) => n.to_string(),
                    _ => continue,
                };

                // Strip peer suffix from version value
                let version = strip_pnpm_peer_suffix(&raw_ver).to_string();

                deps.entry(dep_name.clone()).or_insert(version.clone());
                if is_peer {
                    peer_deps.entry(dep_name).or_insert(version);
                }
            }
        }
    }

    (deps, peer_deps)
}

/// Strip peer resolution suffixes from a pnpm version string.
///
/// `5.1.1(react-dom@18.2.0)(react@18.2.0)` → `5.1.1`
/// `18.2.0(react@18.2.0)` → `18.2.0`
/// `4.17.21` → `4.17.21` (no change)
fn strip_pnpm_peer_suffix(version: &str) -> &str {
    match version.find('(') {
        Some(pos) => version[..pos].trim(),
        None => version.trim(),
    }
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
        // Merged into dependencies
        assert_eq!(
            entries[0]
                .dependencies
                .get("@patternfly/react-core")
                .unwrap(),
            "^5.0.0"
        );
        // Also tracked separately in peer_dependencies
        assert_eq!(
            entries[0]
                .peer_dependencies
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
        // Both dependencies and peerDependencies are collected in merged map
        assert_eq!(
            entries[0]
                .dependencies
                .get("@patternfly/react-core")
                .unwrap(),
            "npm:^5.0.0"
        );
        assert_eq!(entries[0].dependencies.get("lodash").unwrap(), "npm:^4.0.0");
        // Peer deps also tracked separately
        assert_eq!(
            entries[0]
                .peer_dependencies
                .get("@patternfly/react-core")
                .unwrap(),
            "npm:^5.0.0"
        );
        // Regular deps not in peer_dependencies
        assert!(!entries[0].peer_dependencies.contains_key("lodash"));
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
                peer_dependencies: HashMap::new(),
            },
            LockfileEntry {
                name: "@patternfly/react-topology".into(),
                version: "5.2.1".into(),
                dependencies: HashMap::from([("@patternfly/react-core".into(), "^5.1.1".into())]),
                peer_dependencies: HashMap::new(),
            },
            LockfileEntry {
                name: "@patternfly/react-table".into(),
                version: "6.4.1".into(),
                dependencies: HashMap::from([("@patternfly/react-core".into(), "^6.0.0".into())]),
                peer_dependencies: HashMap::new(),
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
            peer_dependencies: HashMap::new(),
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
            peer_dependencies: HashMap::new(),
        }];

        let deps = find_dependents(&entries, "@patternfly/react-core", Some("5.99.99"));
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "@patternfly/react-topology");
    }

    #[test]
    fn test_find_dependents_via_peer_deps() {
        // Simulates @openshift/dynamic-plugin-sdk-utils depending on
        // @patternfly/react-core via peerDependencies
        let entries = vec![
            LockfileEntry {
                name: "@patternfly/react-core".into(),
                version: "5.4.14".into(),
                dependencies: HashMap::new(),
                peer_dependencies: HashMap::new(),
            },
            LockfileEntry {
                name: "@openshift/dynamic-plugin-sdk-utils".into(),
                version: "4.1.0".into(),
                // Peer deps are merged into dependencies for find_dependents
                dependencies: HashMap::from([
                    ("@patternfly/react-core".into(), "^5.1.0".into()),
                    ("lodash".into(), "^4.17.21".into()),
                ]),
                peer_dependencies: HashMap::from([(
                    "@patternfly/react-core".into(),
                    "^5.1.0".into(),
                )]),
            },
        ];

        // find_dependents should find sdk-utils as a dependent of react-core
        let deps = find_dependents(&entries, "@patternfly/react-core", Some("5.99.99"));
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "@openshift/dynamic-plugin-sdk-utils");

        // Verify peer_dependencies field is populated
        assert!(entries[1]
            .peer_dependencies
            .contains_key("@patternfly/react-core"));
        assert!(!entries[1].peer_dependencies.contains_key("lodash"));
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

    // ── pnpm-lock.yaml tests ─────────────────────────────────────────

    #[test]
    fn test_extract_name_version_from_pnpm_key() {
        // Scoped package, no peers
        assert_eq!(
            extract_name_version_from_pnpm_key("@patternfly/react-core@5.1.1"),
            Some(("@patternfly/react-core".into(), "5.1.1".into()))
        );

        // Scoped package with peer suffix
        assert_eq!(
            extract_name_version_from_pnpm_key(
                "@patternfly/react-core@5.1.1(react-dom@18.2.0)(react@18.2.0)"
            ),
            Some(("@patternfly/react-core".into(), "5.1.1".into()))
        );

        // Scoped package with nested peer suffix (v9)
        assert_eq!(
            extract_name_version_from_pnpm_key(
                "@patternfly/react-core@5.1.1(react-dom@18.2.0(react@18.2.0))(react@18.2.0)"
            ),
            Some(("@patternfly/react-core".into(), "5.1.1".into()))
        );

        // Unscoped package
        assert_eq!(
            extract_name_version_from_pnpm_key("lodash@4.17.21"),
            Some(("lodash".into(), "4.17.21".into()))
        );

        // Unscoped package with peers
        assert_eq!(
            extract_name_version_from_pnpm_key("react-dom@18.2.0(react@18.2.0)"),
            Some(("react-dom".into(), "18.2.0".into()))
        );

        // Empty
        assert_eq!(extract_name_version_from_pnpm_key(""), None);
    }

    #[test]
    fn test_strip_pnpm_peer_suffix() {
        assert_eq!(
            strip_pnpm_peer_suffix("5.1.1(react-dom@18.2.0)(react@18.2.0)"),
            "5.1.1"
        );
        assert_eq!(strip_pnpm_peer_suffix("18.2.0(react@18.2.0)"), "18.2.0");
        assert_eq!(strip_pnpm_peer_suffix("4.17.21"), "4.17.21");
        assert_eq!(
            strip_pnpm_peer_suffix("5.1.1(react-dom@18.2.0(react@18.2.0))(react@18.2.0)"),
            "5.1.1"
        );
    }

    #[test]
    fn test_parse_pnpm_v6_basic() {
        let content = r#"
lockfileVersion: '6.0'

packages:
  /@patternfly/react-core@5.1.1(react-dom@18.2.0)(react@18.2.0):
    resolution: {integrity: sha512-abc123}
    dependencies:
      '@patternfly/react-icons': 5.1.1(react-dom@18.2.0)(react@18.2.0)
      '@patternfly/react-styles': 5.1.1
    dev: false

  /@patternfly/react-topology@5.2.1(react-dom@18.2.0)(react@18.2.0):
    resolution: {integrity: sha512-def456}
    dependencies:
      '@patternfly/react-core': 5.1.1(react-dom@18.2.0)(react@18.2.0)
      '@patternfly/react-icons': 5.1.1(react-dom@18.2.0)(react@18.2.0)
      d3: 7.8.5
    dev: false

  /lodash@4.17.21:
    resolution: {integrity: sha512-ghi789}
    dev: false
"#;

        let entries = parse_pnpm_lock_yaml(content).unwrap();
        assert_eq!(entries.len(), 3);

        let topology = entries
            .iter()
            .find(|e| e.name == "@patternfly/react-topology")
            .unwrap();
        assert_eq!(topology.version, "5.2.1");
        // Peer suffix stripped from dep version
        assert_eq!(
            topology.dependencies.get("@patternfly/react-core").unwrap(),
            "5.1.1"
        );
        assert_eq!(topology.dependencies.get("d3").unwrap(), "7.8.5");

        let core = entries
            .iter()
            .find(|e| e.name == "@patternfly/react-core")
            .unwrap();
        assert_eq!(core.version, "5.1.1");
        assert_eq!(
            core.dependencies.get("@patternfly/react-icons").unwrap(),
            "5.1.1"
        );

        let lodash = entries.iter().find(|e| e.name == "lodash").unwrap();
        assert_eq!(lodash.version, "4.17.21");
    }

    #[test]
    fn test_parse_pnpm_v9_basic() {
        let content = r#"
lockfileVersion: '9.0'

packages:
  '@patternfly/react-core@5.1.1':
    resolution: {integrity: sha512-abc123}
    peerDependencies:
      react: ^17 || ^18
      react-dom: ^17 || ^18

  '@patternfly/react-topology@5.2.1':
    resolution: {integrity: sha512-def456}
    peerDependencies:
      react: ^17 || ^18
      react-dom: ^17 || ^18

  lodash@4.17.21:
    resolution: {integrity: sha512-ghi789}

snapshots:
  '@patternfly/react-core@5.1.1(react-dom@18.2.0(react@18.2.0))(react@18.2.0)':
    dependencies:
      '@patternfly/react-icons': 5.1.1(react-dom@18.2.0(react@18.2.0))(react@18.2.0)
      '@patternfly/react-styles': 5.1.1
      react: 18.2.0
      react-dom: 18.2.0(react@18.2.0)

  '@patternfly/react-topology@5.2.1(react-dom@18.2.0(react@18.2.0))(react@18.2.0)':
    dependencies:
      '@patternfly/react-core': 5.1.1(react-dom@18.2.0(react@18.2.0))(react@18.2.0)
      '@patternfly/react-icons': 5.1.1(react-dom@18.2.0(react@18.2.0))(react@18.2.0)
      d3: 7.8.5

  lodash@4.17.21: {}
"#;

        let entries = parse_pnpm_lock_yaml(content).unwrap();
        assert_eq!(entries.len(), 3);

        let topology = entries
            .iter()
            .find(|e| e.name == "@patternfly/react-topology")
            .unwrap();
        assert_eq!(topology.version, "5.2.1");
        // Dependencies came from snapshots, peer suffixes stripped
        assert_eq!(
            topology.dependencies.get("@patternfly/react-core").unwrap(),
            "5.1.1"
        );
        assert_eq!(topology.dependencies.get("d3").unwrap(), "7.8.5");

        let core = entries
            .iter()
            .find(|e| e.name == "@patternfly/react-core")
            .unwrap();
        assert_eq!(core.version, "5.1.1");
        assert_eq!(
            core.dependencies.get("@patternfly/react-icons").unwrap(),
            "5.1.1"
        );
        // Peer deps from snapshots also captured
        assert_eq!(core.dependencies.get("react").unwrap(), "18.2.0");
    }

    #[test]
    fn test_parse_pnpm_v9_snapshot_only_package() {
        // A package that appears only in snapshots (not in packages)
        let content = r#"
lockfileVersion: '9.0'

packages: {}

snapshots:
  tslib@2.6.0:
    dependencies:
      some-dep: 1.0.0
"#;

        let entries = parse_pnpm_lock_yaml(content).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "tslib");
        assert_eq!(entries[0].version, "2.6.0");
        assert_eq!(entries[0].dependencies.get("some-dep").unwrap(), "1.0.0");
    }

    #[test]
    fn test_parse_pnpm_v6_peer_deps() {
        let content = r#"
lockfileVersion: '6.0'

packages:
  /some-plugin@2.0.0(react@18.2.0):
    resolution: {integrity: sha512-xyz}
    peerDependencies:
      react: ^18.0.0
    dependencies:
      '@patternfly/react-core': 5.0.0(react@18.2.0)
      lodash: 4.17.21
    dev: false
"#;

        let entries = parse_pnpm_lock_yaml(content).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "some-plugin");
        assert_eq!(entries[0].version, "2.0.0");
        // peerDependencies collected in merged map (range preserved for peers in the entry)
        assert!(entries[0].dependencies.contains_key("react"));
        // Regular dep version has peer suffix stripped
        assert_eq!(
            entries[0]
                .dependencies
                .get("@patternfly/react-core")
                .unwrap(),
            "5.0.0"
        );
        // Peer deps tracked separately
        assert!(entries[0].peer_dependencies.contains_key("react"));
        // Regular dep not in peer_dependencies
        assert!(!entries[0].peer_dependencies.contains_key("lodash"));
        assert!(!entries[0]
            .peer_dependencies
            .contains_key("@patternfly/react-core"));
    }

    #[test]
    fn test_parse_pnpm_empty_packages() {
        let content = r#"
lockfileVersion: '9.0'
"#;

        let entries = parse_pnpm_lock_yaml(content).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_parse_pnpm_numeric_lockfile_version() {
        // Some pnpm versions write lockfileVersion as a number, not string
        let content = r#"
lockfileVersion: 6.0

packages:
  /lodash@4.17.21:
    resolution: {integrity: sha512-abc}
    dev: false
"#;

        let entries = parse_pnpm_lock_yaml(content).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "lodash");
    }

    // ── monorepo lockfile discovery tests ─────────────────────────────

    #[test]
    fn test_parse_all_lockfiles_root_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Create a package-lock.json at root
        let lock_content = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "root", "version": "1.0.0" },
                "node_modules/lodash": { "version": "4.17.21" }
            }
        }"#;
        std::fs::write(root.join("package-lock.json"), lock_content).unwrap();

        // Create a sub-project with its own package.json (but no lockfile)
        let sub = root.join("packages/app");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("package.json"), r#"{"name": "app"}"#).unwrap();

        let pkg_paths = vec![sub.join("package.json"), root.join("package.json")];

        let (entries, paths) = parse_all_lockfiles(root, &pkg_paths).unwrap();

        // Should find the root lockfile
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "lodash");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], root.join("package-lock.json"));
    }

    #[test]
    fn test_parse_all_lockfiles_subproject_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // No lockfile at root
        std::fs::write(root.join("package.json"), r#"{"name": "root"}"#).unwrap();

        // Sub-project A with its own lockfile
        let sub_a = root.join("project-a");
        std::fs::create_dir_all(&sub_a).unwrap();
        std::fs::write(sub_a.join("package.json"), r#"{"name": "a"}"#).unwrap();
        let lock_a = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "a", "version": "1.0.0" },
                "node_modules/@patternfly/react-core": { "version": "5.4.14" }
            }
        }"#;
        std::fs::write(sub_a.join("package-lock.json"), lock_a).unwrap();

        // Sub-project B with its own lockfile
        let sub_b = root.join("project-b");
        std::fs::create_dir_all(&sub_b).unwrap();
        std::fs::write(sub_b.join("package.json"), r#"{"name": "b"}"#).unwrap();
        let lock_b = r#"{
            "lockfileVersion": 3,
            "packages": {
                "": { "name": "b", "version": "1.0.0" },
                "node_modules/lodash": { "version": "4.17.21" }
            }
        }"#;
        std::fs::write(sub_b.join("package-lock.json"), lock_b).unwrap();

        let pkg_paths = vec![
            sub_a.join("package.json"),
            sub_b.join("package.json"),
            root.join("package.json"),
        ];

        let (entries, paths) = parse_all_lockfiles(root, &pkg_paths).unwrap();

        // Should find both sub-project lockfiles
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.name == "@patternfly/react-core"));
        assert!(entries.iter().any(|e| e.name == "lodash"));
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn test_parse_all_lockfiles_no_lockfiles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        std::fs::write(root.join("package.json"), r#"{"name": "root"}"#).unwrap();

        let pkg_paths = vec![root.join("package.json")];
        let (entries, paths) = parse_all_lockfiles(root, &pkg_paths).unwrap();

        assert!(entries.is_empty());
        assert!(paths.is_empty());
    }

    #[test]
    fn test_parse_all_lockfiles_pnpm() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let pnpm_content = r#"
lockfileVersion: '9.0'

packages:
  '@patternfly/react-core@5.1.1':
    resolution: {integrity: sha512-abc}

snapshots:
  '@patternfly/react-core@5.1.1(react@18.2.0)':
    dependencies:
      react: 18.2.0
"#;
        std::fs::write(root.join("pnpm-lock.yaml"), pnpm_content).unwrap();

        let pkg_paths = vec![root.join("package.json")];
        let (entries, paths) = parse_all_lockfiles(root, &pkg_paths).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "@patternfly/react-core");
        assert_eq!(entries[0].version, "5.1.1");
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], root.join("pnpm-lock.yaml"));
    }
}

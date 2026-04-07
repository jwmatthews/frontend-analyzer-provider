//! Import path resolution.
//!
//! Resolves import specifiers to absolute file paths using `oxc_resolver`,
//! which supports:
//! - Relative imports (`./Foo`, `../components/Bar`)
//! - TypeScript path aliases from `tsconfig.json` (`@app/*` → `src/app/*`)
//! - Extension probing (`.tsx`, `.ts`, `.jsx`, `.js`)
//! - Index file resolution (directory → `index.tsx`, etc.)
//!
//! For monorepo projects with multiple `tsconfig.json` files (e.g.,
//! `client/tsconfig.json`, `common/tsconfig.json`), use [`ResolverMap`]
//! to create one resolver per tsconfig and route each source file to the
//! correct resolver based on path prefix matching.

use oxc_resolver::{
    ResolveOptions, Resolver, TsconfigDiscovery, TsconfigOptions, TsconfigReferences,
};
use std::path::{Path, PathBuf};

// ── ResolverMap ──────────────────────────────────────────────────────────

/// Routes source files to the correct `oxc_resolver::Resolver` based on
/// which `tsconfig.json` covers the file.
///
/// In monorepo projects with multiple tsconfig files (e.g., `client/`,
/// `common/`, `cypress/`), each tsconfig may define different path aliases.
/// `ResolverMap` creates one resolver per tsconfig and selects the right
/// one for each file being scanned.
///
/// Selection is by longest-prefix match on the tsconfig's directory:
/// a file at `client/src/app/Page.tsx` matches the resolver for
/// `client/tsconfig.json` because `client/` is a prefix of the file path.
pub struct ResolverMap {
    /// (tsconfig_dir, resolver) sorted by path depth descending
    /// so the most specific (longest) prefix match wins.
    resolvers: Vec<(PathBuf, Resolver)>,
    /// Fallback resolver with no tsconfig (for files outside any tsconfig scope).
    fallback: Resolver,
}

impl ResolverMap {
    /// Get the resolver whose tsconfig directory is the longest ancestor of
    /// `file_path`. Falls back to a resolver with no tsconfig if no match.
    pub fn resolver_for_file(&self, file_path: &Path) -> &Resolver {
        for (tsconfig_dir, resolver) in &self.resolvers {
            if file_path.starts_with(tsconfig_dir) {
                return resolver;
            }
        }
        &self.fallback
    }
}

/// Discover all `tsconfig.json` files under `root` (up to `max_depth`
/// levels deep) and create a [`ResolverMap`] with one resolver per tsconfig.
///
/// Skips `node_modules`, `.git`, `dist`, `build`, and `target` directories.
pub fn create_resolver_map(root: &Path, max_depth: usize) -> ResolverMap {
    let tsconfigs = find_all_tsconfigs_in_project(root, max_depth);

    if !tsconfigs.is_empty() {
        tracing::info!(
            "Found {} tsconfig.json file(s): {:?}",
            tsconfigs.len(),
            tsconfigs
        );
    }

    let mut resolvers: Vec<(PathBuf, Resolver)> = tsconfigs
        .iter()
        .filter_map(|tc| {
            let dir = tc.parent()?;
            let resolver = create_resolver(Some(tc));
            Some((dir.to_path_buf(), resolver))
        })
        .collect();

    // Sort by component count descending — most specific (deepest) paths first
    // so the longest-prefix match wins in resolver_for_file().
    resolvers.sort_by(|a, b| b.0.components().count().cmp(&a.0.components().count()));

    ResolverMap {
        resolvers,
        fallback: create_resolver(None),
    }
}

/// Find all `tsconfig.json` files in a project tree.
///
/// Searches `root` and its subdirectories up to `max_depth` levels deep.
/// Skips `node_modules`, `.git`, `dist`, `build`, and `target` directories.
/// Also checks `root` itself for a tsconfig.
pub fn find_all_tsconfigs_in_project(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();

    // Check root itself
    let candidate = root.join("tsconfig.json");
    if candidate.is_file() {
        found.push(candidate);
    }

    // BFS through subdirectories
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> = std::collections::VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));

    while let Some((dir, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }

        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.')
                || name_str == "node_modules"
                || name_str == "dist"
                || name_str == "build"
                || name_str == "target"
            {
                continue;
            }

            let tsconfig = path.join("tsconfig.json");
            if tsconfig.is_file() {
                found.push(tsconfig);
            }

            queue.push_back((path, depth + 1));
        }
    }

    found
}

// ── Single resolver helpers ──────────────────────────────────────────────

/// Create a resolver configured for TypeScript/React projects.
///
/// Uses `TsconfigOptions` to read `compilerOptions.paths` aliases and
/// `baseUrl` from the project's `tsconfig.json`.
pub fn create_resolver(tsconfig_path: Option<&Path>) -> Resolver {
    let mut options = ResolveOptions {
        extensions: vec![
            ".tsx".into(),
            ".ts".into(),
            ".jsx".into(),
            ".js".into(),
            ".json".into(),
        ],
        main_files: vec!["index".into()],
        condition_names: vec!["node".into(), "import".into()],
        ..ResolveOptions::default()
    };

    if let Some(tsconfig) = tsconfig_path {
        options.tsconfig = Some(TsconfigDiscovery::Manual(TsconfigOptions {
            config_file: tsconfig.to_path_buf(),
            references: TsconfigReferences::Auto,
        }));
    }

    Resolver::new(options)
}

/// Resolve an import specifier to an absolute file path.
///
/// Returns `None` if the specifier can't be resolved.
pub fn resolve_import_with_resolver(
    resolver: &Resolver,
    importing_file: &Path,
    module_source: &str,
) -> Option<PathBuf> {
    let dir = importing_file.parent()?;
    match resolver.resolve(dir, module_source) {
        Ok(resolution) => Some(resolution.into_path_buf()),
        Err(_) => None,
    }
}

/// Check whether a resolved path is inside `node_modules`.
pub fn is_node_modules_path(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "node_modules")
}

/// Resolve a relative import specifier to an absolute file path (legacy API).
///
/// Only handles relative imports (`./` or `../` prefix). For full resolution
/// including tsconfig path aliases, use `resolve_import_with_resolver()`.
pub fn resolve_import(importing_file: &Path, module_source: &str, _root: &Path) -> Option<PathBuf> {
    if !module_source.starts_with("./") && !module_source.starts_with("../") {
        return None;
    }

    let dir = importing_file.parent()?;
    let base = dir.join(module_source);

    if base.is_file() {
        return base.canonicalize().ok();
    }

    for ext in &["tsx", "ts", "jsx", "js"] {
        let candidate = base.with_extension(ext);
        if candidate.is_file() {
            return candidate.canonicalize().ok();
        }
    }

    if base.is_dir() {
        for index in &["index.tsx", "index.ts", "index.jsx", "index.js"] {
            let candidate = base.join(index);
            if candidate.is_file() {
                return candidate.canonicalize().ok();
            }
        }
    }

    None
}

/// Walk up from `start_dir` to find the nearest `tsconfig.json`.
pub fn find_tsconfig(start_dir: &Path) -> Option<PathBuf> {
    let mut dir = start_dir;
    loop {
        let candidate = dir.join("tsconfig.json");
        if candidate.is_file() {
            return Some(candidate);
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return None,
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_skip_npm_packages() {
        let importing = PathBuf::from("/project/src/App.tsx");
        assert!(
            resolve_import(&importing, "@patternfly/react-core", Path::new("/project")).is_none()
        );
        assert!(resolve_import(&importing, "react", Path::new("/project")).is_none());
        assert!(resolve_import(&importing, "lodash/merge", Path::new("/project")).is_none());
    }

    #[test]
    fn test_resolve_with_extension() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let target = src_dir.join("Wrapper.tsx");
        fs::write(&target, "export const Wrapper = () => null;").unwrap();
        let importing = src_dir.join("App.tsx");
        let resolved = resolve_import(&importing, "./Wrapper", dir.path());
        assert_eq!(resolved, Some(target.canonicalize().unwrap()));
    }

    #[test]
    fn test_resolve_exact_path() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let target = src_dir.join("Wrapper.tsx");
        fs::write(&target, "export const Wrapper = () => null;").unwrap();
        let importing = src_dir.join("App.tsx");
        let resolved = resolve_import(&importing, "./Wrapper.tsx", dir.path());
        assert_eq!(resolved, Some(target.canonicalize().unwrap()));
    }

    #[test]
    fn test_resolve_index_file() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let comp_dir = src_dir.join("components");
        fs::create_dir_all(&comp_dir).unwrap();
        let target = comp_dir.join("index.tsx");
        fs::write(&target, "export const Foo = () => null;").unwrap();
        let importing = src_dir.join("App.tsx");
        let resolved = resolve_import(&importing, "./components", dir.path());
        assert_eq!(resolved, Some(target.canonicalize().unwrap()));
    }

    #[test]
    fn test_resolve_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let sub_dir = src_dir.join("views");
        fs::create_dir_all(&sub_dir).unwrap();
        let target = src_dir.join("Wrapper.tsx");
        fs::write(&target, "export const Wrapper = () => null;").unwrap();
        let importing = sub_dir.join("Page.tsx");
        let resolved = resolve_import(&importing, "../Wrapper", dir.path());
        assert_eq!(resolved, Some(target.canonicalize().unwrap()));
    }

    #[test]
    fn test_resolve_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let importing = dir.path().join("App.tsx");
        let resolved = resolve_import(&importing, "./DoesNotExist", dir.path());
        assert!(resolved.is_none());
    }

    #[test]
    fn test_extension_priority_tsx_first() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let tsx_file = src_dir.join("Wrapper.tsx");
        let ts_file = src_dir.join("Wrapper.ts");
        fs::write(&tsx_file, "export const Wrapper = () => null;").unwrap();
        fs::write(&ts_file, "export const Wrapper = () => null;").unwrap();
        let importing = src_dir.join("App.tsx");
        let resolved = resolve_import(&importing, "./Wrapper", dir.path());
        assert_eq!(resolved, Some(tsx_file.canonicalize().unwrap()));
    }

    #[test]
    fn test_is_node_modules_path() {
        assert!(is_node_modules_path(Path::new(
            "/project/node_modules/@patternfly/react-core/dist/index.js"
        )));
        assert!(!is_node_modules_path(Path::new(
            "/project/src/components/Foo.tsx"
        )));
    }

    #[test]
    fn test_find_tsconfig() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src").join("app");
        fs::create_dir_all(&src_dir).unwrap();
        let tsconfig = dir.path().join("tsconfig.json");
        fs::write(&tsconfig, r#"{ "compilerOptions": {} }"#).unwrap();
        let found = find_tsconfig(&src_dir);
        assert_eq!(found, Some(tsconfig));
    }

    #[test]
    fn test_find_tsconfig_not_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_tsconfig(dir.path()).is_none());
    }

    #[test]
    fn test_oxc_resolver_with_tsconfig_paths() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src").join("app");
        let comp_dir = src_dir.join("components");
        fs::create_dir_all(&comp_dir).unwrap();

        let tsconfig = dir.path().join("tsconfig.json");
        fs::write(
            &tsconfig,
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@app/*": ["src/app/*"] } } }"#,
        )
        .unwrap();

        let wrapper_file = comp_dir.join("Wrapper.tsx");
        fs::write(&wrapper_file, "export const Wrapper = () => null;").unwrap();

        let consumer_file = src_dir.join("App.tsx");
        fs::write(&consumer_file, "").unwrap();

        let resolver = create_resolver(Some(&tsconfig));
        let resolved =
            resolve_import_with_resolver(&resolver, &consumer_file, "@app/components/Wrapper");
        assert!(resolved.is_some(), "Should resolve @app/components/Wrapper");
        assert!(resolved.unwrap().ends_with("Wrapper.tsx"));
    }

    #[test]
    fn test_oxc_resolver_relative_import() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let target = src_dir.join("Wrapper.tsx");
        fs::write(&target, "export const Wrapper = () => null;").unwrap();
        let importing = src_dir.join("App.tsx");
        fs::write(&importing, "").unwrap();
        let resolver = create_resolver(None);
        let resolved = resolve_import_with_resolver(&resolver, &importing, "./Wrapper");
        assert!(resolved.is_some());
        assert!(resolved.unwrap().ends_with("Wrapper.tsx"));
    }

    #[test]
    fn test_oxc_resolver_index_file() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let comp_dir = src_dir.join("components");
        fs::create_dir_all(&comp_dir).unwrap();
        let index = comp_dir.join("index.ts");
        fs::write(&index, "export * from './Foo';").unwrap();
        let importing = src_dir.join("App.tsx");
        fs::write(&importing, "").unwrap();
        let resolver = create_resolver(None);
        let resolved = resolve_import_with_resolver(&resolver, &importing, "./components");
        assert!(resolved.is_some(), "Should resolve to index file");
    }

    // ── ResolverMap tests ────────────────────────────────────────────────

    #[test]
    fn test_find_all_tsconfigs_in_project() {
        let dir = tempfile::tempdir().unwrap();
        // Simulate monorepo: client/ and common/ each have tsconfig
        let client = dir.path().join("client");
        let common = dir.path().join("common");
        fs::create_dir_all(&client).unwrap();
        fs::create_dir_all(&common).unwrap();
        fs::write(client.join("tsconfig.json"), r#"{ "compilerOptions": {} }"#).unwrap();
        fs::write(common.join("tsconfig.json"), r#"{ "compilerOptions": {} }"#).unwrap();

        let found = find_all_tsconfigs_in_project(dir.path(), 3);
        assert_eq!(found.len(), 2, "Should find 2 tsconfig files");
        let names: Vec<String> = found
            .iter()
            .map(|p| {
                p.parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert!(names.contains(&"client".to_string()));
        assert!(names.contains(&"common".to_string()));
    }

    #[test]
    fn test_find_all_tsconfigs_skips_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        let nm = dir.path().join("node_modules").join("pkg");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("tsconfig.json"), "{}").unwrap();

        let found = find_all_tsconfigs_in_project(dir.path(), 3);
        assert!(found.is_empty());
    }

    #[test]
    fn test_find_all_tsconfigs_includes_root() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();
        let client = dir.path().join("client");
        fs::create_dir_all(&client).unwrap();
        fs::write(client.join("tsconfig.json"), "{}").unwrap();

        let found = find_all_tsconfigs_in_project(dir.path(), 3);
        assert_eq!(found.len(), 2, "Should find root + client tsconfig");
    }

    #[test]
    fn test_resolver_map_routes_to_correct_resolver() {
        // Simulate: client/ has @app/* paths, common/ has @branding/* paths
        let dir = tempfile::tempdir().unwrap();

        // client/tsconfig.json with @app/*
        let client_dir = dir.path().join("client");
        let client_src = client_dir.join("src").join("app").join("components");
        fs::create_dir_all(&client_src).unwrap();
        fs::write(
            client_dir.join("tsconfig.json"),
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@app/*": ["src/app/*"] } } }"#,
        )
        .unwrap();
        let wrapper = client_src.join("Wrapper.tsx");
        fs::write(&wrapper, "export const Wrapper = () => null;").unwrap();

        // common/tsconfig.json with @branding/*
        let common_dir = dir.path().join("common");
        let common_src = common_dir.join("src");
        fs::create_dir_all(&common_src).unwrap();
        fs::write(
            common_dir.join("tsconfig.json"),
            r#"{ "compilerOptions": { "baseUrl": ".", "paths": { "@branding/*": ["src/*"] } } }"#,
        )
        .unwrap();
        fs::write(
            common_src.join("Brand.tsx"),
            "export const Brand = () => null;",
        )
        .unwrap();

        let resolver_map = create_resolver_map(dir.path(), 3);

        // File in client/ should use client's resolver (has @app/*)
        let client_file = client_dir.join("src").join("App.tsx");
        fs::write(&client_file, "").unwrap();
        let client_resolver = resolver_map.resolver_for_file(&client_file);
        let resolved =
            resolve_import_with_resolver(client_resolver, &client_file, "@app/components/Wrapper");
        assert!(
            resolved.is_some(),
            "Client resolver should resolve @app/components/Wrapper"
        );
        assert!(resolved.unwrap().ends_with("Wrapper.tsx"));

        // File in common/ should use common's resolver (has @branding/*)
        let common_file = common_dir.join("src").join("Main.tsx");
        fs::write(&common_file, "").unwrap();
        let common_resolver = resolver_map.resolver_for_file(&common_file);
        let resolved =
            resolve_import_with_resolver(common_resolver, &common_file, "@branding/Brand");
        assert!(
            resolved.is_some(),
            "Common resolver should resolve @branding/Brand"
        );

        // @app/* should NOT resolve from common's context
        let cross_resolved =
            resolve_import_with_resolver(common_resolver, &common_file, "@app/components/Wrapper");
        assert!(
            cross_resolved.is_none(),
            "@app/* should not resolve from common's resolver"
        );
    }

    #[test]
    fn test_resolver_map_fallback_for_unknown_files() {
        let dir = tempfile::tempdir().unwrap();
        let client = dir.path().join("client");
        fs::create_dir_all(&client).unwrap();
        fs::write(client.join("tsconfig.json"), "{}").unwrap();

        let resolver_map = create_resolver_map(dir.path(), 3);

        // A file outside any tsconfig dir should use fallback
        let orphan = dir.path().join("scripts").join("tool.ts");
        let resolver = resolver_map.resolver_for_file(&orphan);
        // Fallback resolver exists (won't panic)
        assert!(
            std::ptr::eq(resolver, &resolver_map.fallback),
            "Should use fallback resolver for files outside tsconfig scopes"
        );
    }

    #[test]
    fn test_resolver_map_empty_project() {
        let dir = tempfile::tempdir().unwrap();
        let resolver_map = create_resolver_map(dir.path(), 3);

        // No tsconfigs found — should have only fallback
        assert!(resolver_map.resolvers.is_empty());

        let any_file = dir.path().join("src").join("App.tsx");
        let resolver = resolver_map.resolver_for_file(&any_file);
        assert!(std::ptr::eq(resolver, &resolver_map.fallback));
    }
}

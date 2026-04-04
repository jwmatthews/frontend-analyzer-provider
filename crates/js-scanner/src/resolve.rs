//! Import path resolution.
//!
//! Converts relative import specifiers (e.g., `./ConditionalTableBody` or
//! `../components/Wrapper`) to absolute file system paths by probing for
//! known JS/TS extensions and index files.
//!
//! npm package imports (non-relative paths) are skipped — they can't be
//! resolved to local project files and are already handled by the
//! `from`/`parentFrom` filters on the rule condition.

use std::path::{Path, PathBuf};

/// File extensions to probe when resolving a bare import path.
const RESOLVE_EXTENSIONS: &[&str] = &["tsx", "ts", "jsx", "js"];

/// Index file names to probe when the import points at a directory.
const INDEX_FILES: &[&str] = &["index.tsx", "index.ts", "index.jsx", "index.js"];

/// Resolve a relative import specifier to an absolute file path.
///
/// Returns `None` for:
/// - npm package imports (no `./` or `../` prefix)
/// - imports that can't be resolved to an existing file
///
/// Resolution order:
/// 1. Exact path (if already has an extension and exists)
/// 2. Path + `.tsx` / `.ts` / `.jsx` / `.js`
/// 3. Path as directory + `index.tsx` / `index.ts` / `index.jsx` / `index.js`
pub fn resolve_import(importing_file: &Path, module_source: &str, _root: &Path) -> Option<PathBuf> {
    // Only resolve relative imports
    if !module_source.starts_with("./") && !module_source.starts_with("../") {
        return None;
    }

    let dir = importing_file.parent()?;
    let base = dir.join(module_source);

    // 1. Exact path (e.g., import from './foo.tsx')
    if base.is_file() {
        return base.canonicalize().ok();
    }

    // 2. Try adding extensions
    for ext in RESOLVE_EXTENSIONS {
        let candidate = base.with_extension(ext);
        if candidate.is_file() {
            return candidate.canonicalize().ok();
        }
    }

    // 3. Try as directory with index file
    if base.is_dir() {
        for index in INDEX_FILES {
            let candidate = base.join(index);
            if candidate.is_file() {
                return candidate.canonicalize().ok();
            }
        }
    }

    None
}

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

        // Create both .ts and .tsx — tsx should win
        let tsx_file = src_dir.join("Wrapper.tsx");
        let ts_file = src_dir.join("Wrapper.ts");
        fs::write(&tsx_file, "export const Wrapper = () => null;").unwrap();
        fs::write(&ts_file, "export const Wrapper = () => null;").unwrap();

        let importing = src_dir.join("App.tsx");

        let resolved = resolve_import(&importing, "./Wrapper", dir.path());
        assert_eq!(resolved, Some(tsx_file.canonicalize().unwrap()));
    }
}

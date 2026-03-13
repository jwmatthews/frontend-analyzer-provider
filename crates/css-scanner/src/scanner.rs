//! Top-level CSS/SCSS scanner.
//!
//! Walks project files, parses CSS with LightningCSS, and falls back to
//! regex for SCSS files.

use anyhow::Result;
use frontend_core::incident::Incident;
use regex::Regex;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// File extensions this scanner handles.
const CSS_EXTENSIONS: &[&str] = &["css", "scss", "less", "sass"];

/// Directories to skip.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    "dist",
    "build",
    "target",
    ".next",
    ".nuxt",
    "coverage",
];

/// Collect all CSS/SCSS files in a project directory.
pub fn collect_css_files(root: &Path, file_pattern: Option<&str>) -> Result<Vec<PathBuf>> {
    let pattern_re = file_pattern.map(|p| Regex::new(p)).transpose()?;

    let mut files = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_entry(|e| {
        if e.file_type().is_dir() {
            let name = e.file_name().to_string_lossy();
            return !SKIP_DIRS.contains(&name.as_ref());
        }
        true
    }) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let ext = path.extension().unwrap_or_default().to_string_lossy();

        if !CSS_EXTENSIONS.contains(&ext.as_ref()) {
            continue;
        }

        if let Some(re) = &pattern_re {
            let path_str = path.to_string_lossy();
            if !re.is_match(&path_str) {
                continue;
            }
        }

        files.push(path.to_path_buf());
    }

    Ok(files)
}

/// Scan a CSS file for class selector matches.
pub fn scan_css_file_classes(
    file_path: &Path,
    root: &Path,
    pattern: &Regex,
) -> Result<Vec<Incident>> {
    let ext = file_path.extension().unwrap_or_default().to_string_lossy();

    match ext.as_ref() {
        "css" => crate::selectors::scan_css_selectors(file_path, root, pattern),
        "scss" | "less" | "sass" => {
            crate::scss_fallback::scan_scss_classes(file_path, root, pattern)
        }
        _ => Ok(Vec::new()),
    }
}

/// Scan a CSS file for custom property (variable) matches.
pub fn scan_css_file_vars(file_path: &Path, root: &Path, pattern: &Regex) -> Result<Vec<Incident>> {
    let ext = file_path.extension().unwrap_or_default().to_string_lossy();

    match ext.as_ref() {
        "css" => crate::variables::scan_css_variables(file_path, root, pattern),
        "scss" | "less" | "sass" => crate::scss_fallback::scan_scss_vars(file_path, root, pattern),
        _ => Ok(Vec::new()),
    }
}

/// Convert a file path to a file:// URI.
pub fn path_to_uri(path: &Path, root: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    format!("file://{}", absolute.display())
}

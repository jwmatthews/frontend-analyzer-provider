//! CSS custom property (variable) scanning using LightningCSS.
//!
//! For CSS files, we use regex-based scanning since extracting custom property
//! declarations from LightningCSS's typed property API is complex and the
//! regex approach is sufficient for this use case.

use anyhow::Result;
use frontend_core::incident::Incident;
use regex::Regex;
use std::path::Path;

/// Scan a CSS file for custom property declarations and var() usages matching the pattern.
///
/// Uses regex scanning since it's more straightforward for finding CSS variable
/// names across declarations and var() usages.
pub fn scan_css_variables(file_path: &Path, root: &Path, pattern: &Regex) -> Result<Vec<Incident>> {
    // For CSS variables, regex is actually more reliable than trying to
    // extract from LightningCSS's typed property model
    crate::scss_fallback::scan_scss_vars(file_path, root, pattern)
}

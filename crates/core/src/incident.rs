//! Incident types representing matched violations in source code.
//!
//! These mirror the Konveyor analyzer-lsp output format and the
//! gRPC IncidentContext message.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A position in a source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// A range in a source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    #[serde(rename = "startPosition")]
    pub start: Position,
    #[serde(rename = "endPosition")]
    pub end: Position,
}

/// A hyperlink for additional context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalLink {
    pub url: String,
    pub title: String,
}

/// A single match/incident found by the provider.
///
/// This is the internal representation that maps to both:
/// - gRPC `IncidentContext` message (for Konveyor provider mode)
/// - Konveyor output `Incident` (for standalone CLI mode)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incident {
    /// File URI where the incident was found (e.g. `file:///path/to/File.tsx`).
    #[serde(rename = "fileURI")]
    pub file_uri: String,

    /// Line number of the match (1-indexed).
    #[serde(rename = "lineNumber")]
    pub line_number: u32,

    /// Source code location (start/end positions).
    #[serde(rename = "codeLocation")]
    pub code_location: Location,

    /// Surrounding source code snippet for context.
    #[serde(rename = "codeSnip", skip_serializing_if = "Option::is_none")]
    pub code_snip: Option<String>,

    /// Provider-specific variables (e.g., matched text, symbol name).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, serde_json::Value>,

    /// Optional effort override for this specific incident.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<i64>,

    /// Associated hyperlinks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<ExternalLink>,

    /// Whether this incident is from a dependency (vs. source code).
    #[serde(rename = "isDependencyIncident", default)]
    pub is_dependency_incident: bool,
}

impl Incident {
    /// Create a new incident with the minimum required fields.
    pub fn new(file_uri: String, line_number: u32, code_location: Location) -> Self {
        Self {
            file_uri,
            line_number,
            code_location,
            code_snip: None,
            variables: BTreeMap::new(),
            effort: None,
            links: Vec::new(),
            is_dependency_incident: false,
        }
    }

    /// Add a code snippet to the incident.
    pub fn with_code_snip(mut self, snip: String) -> Self {
        self.code_snip = Some(snip);
        self
    }

    /// Add a variable to the incident.
    pub fn with_variable(
        mut self,
        key: impl Into<String>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        self.variables.insert(key.into(), value.into());
        self
    }
}

/// Extract a code snippet from source text centered around a line number.
///
/// Returns a string with line-number-prefixed source lines, matching
/// the Konveyor output format.
pub fn extract_code_snip(source: &str, line_number: u32, context_lines: u32) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let total = lines.len() as u32;

    let start = line_number.saturating_sub(context_lines + 1);
    let end = (line_number + context_lines).min(total);

    let width = format!("{}", end).len();

    let mut snip = String::new();
    for i in start..end {
        let line_num = i + 1;
        let line_content = lines.get(i as usize).unwrap_or(&"");
        snip.push_str(&format!(
            "{:>width$}  {}\n",
            line_num,
            line_content,
            width = width
        ));
    }
    snip
}

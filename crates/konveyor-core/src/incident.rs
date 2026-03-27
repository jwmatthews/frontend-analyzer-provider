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
/// This is the canonical Konveyor incident type that maps to both:
/// - gRPC `IncidentContext` message (for Konveyor provider mode)
/// - Konveyor output incident (for analysis output consumed by fix tooling)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incident {
    /// File URI where the incident was found (e.g. `file:///path/to/File.tsx`).
    ///
    /// Accepts both `fileURI` (gRPC/provider format) and `uri` (kantra output format).
    #[serde(rename = "fileURI", alias = "uri")]
    pub file_uri: String,

    /// Line number of the match (1-indexed).
    #[serde(
        rename = "lineNumber",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub line_number: Option<u32>,

    /// Source code location (start/end positions).
    /// Present when produced by scanners, absent in kantra serialized output.
    #[serde(
        rename = "codeLocation",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub code_location: Option<Location>,

    /// Human-readable message about the incident.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,

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
            line_number: Some(line_number),
            code_location: Some(code_location),
            message: String::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_location(line: u32, start_col: u32, end_col: u32) -> Location {
        Location {
            start: Position {
                line,
                character: start_col,
            },
            end: Position {
                line,
                character: end_col,
            },
        }
    }

    #[test]
    fn test_incident_new_defaults() {
        let incident = Incident::new("file:///test.tsx".to_string(), 10, make_location(9, 0, 20));
        assert_eq!(incident.file_uri, "file:///test.tsx");
        assert_eq!(incident.line_number, Some(10));
        assert!(incident.code_snip.is_none());
        assert!(incident.variables.is_empty());
        assert!(incident.effort.is_none());
        assert!(incident.links.is_empty());
        assert!(!incident.is_dependency_incident);
        assert!(incident.message.is_empty());
    }

    #[test]
    fn test_incident_with_code_snip() {
        let incident = Incident::new("file:///test.tsx".to_string(), 5, make_location(4, 0, 10))
            .with_code_snip("const x = 1;".to_string());

        assert_eq!(incident.code_snip, Some("const x = 1;".to_string()));
    }

    #[test]
    fn test_incident_with_variable() {
        let incident = Incident::new("file:///test.tsx".to_string(), 1, make_location(0, 0, 5))
            .with_variable("propName", "isActive")
            .with_variable("componentName", "Button");

        assert_eq!(incident.variables.len(), 2);
        assert_eq!(
            incident.variables.get("propName"),
            Some(&serde_json::Value::String("isActive".to_string()))
        );
        assert_eq!(
            incident.variables.get("componentName"),
            Some(&serde_json::Value::String("Button".to_string()))
        );
    }

    #[test]
    fn test_incident_builder_chain() {
        let incident = Incident::new("file:///app.tsx".to_string(), 42, make_location(41, 4, 30))
            .with_code_snip("  <Button isActive />".to_string())
            .with_variable("propName", "isActive")
            .with_variable("componentName", "Button");

        assert_eq!(incident.file_uri, "file:///app.tsx");
        assert_eq!(incident.line_number, Some(42));
        assert_eq!(
            incident.code_snip,
            Some("  <Button isActive />".to_string())
        );
        assert_eq!(incident.variables.len(), 2);
    }

    #[test]
    fn test_extract_code_snip_middle_of_file() {
        let source = "line1\nline2\nline3\nline4\nline5\nline6\nline7";
        let snip = extract_code_snip(source, 4, 2);
        assert!(snip.contains("line2"));
        assert!(snip.contains("line3"));
        assert!(snip.contains("line4"));
        assert!(snip.contains("line5"));
        assert!(snip.contains("line6"));
    }

    #[test]
    fn test_extract_code_snip_start_of_file() {
        let source = "first\nsecond\nthird\nfourth\nfifth";
        let snip = extract_code_snip(source, 1, 2);
        assert!(snip.contains("first"));
        assert!(snip.contains("second"));
        assert!(snip.contains("third"));
    }

    #[test]
    fn test_extract_code_snip_end_of_file() {
        let source = "a\nb\nc\nd\ne";
        let snip = extract_code_snip(source, 5, 2);
        assert!(snip.contains("c"));
        assert!(snip.contains("d"));
        assert!(snip.contains("e"));
    }

    #[test]
    fn test_extract_code_snip_single_line_file() {
        let source = "only line";
        let snip = extract_code_snip(source, 1, 3);
        assert!(snip.contains("only line"));
        assert!(snip.contains("1"));
    }

    #[test]
    fn test_extract_code_snip_line_numbers_are_padded() {
        let lines: Vec<String> = (1..=120).map(|i| format!("line {}", i)).collect();
        let source = lines.join("\n");
        let snip = extract_code_snip(&source, 100, 1);
        assert!(snip.contains("99"));
        assert!(snip.contains("100"));
        assert!(snip.contains("101"));
    }

    #[test]
    fn test_extract_code_snip_empty_source() {
        let source = "";
        let snip = extract_code_snip(source, 1, 3);
        assert!(snip.is_empty() || snip.trim().is_empty() || snip.contains("1"));
    }

    #[test]
    fn test_incident_serde_roundtrip() {
        let incident = Incident::new("file:///test.tsx".to_string(), 10, make_location(9, 5, 15))
            .with_code_snip("test snip".to_string())
            .with_variable("key", "value");

        let json = serde_json::to_string(&incident).unwrap();
        let back: Incident = serde_json::from_str(&json).unwrap();
        assert_eq!(back.file_uri, "file:///test.tsx");
        assert_eq!(back.line_number, Some(10));
        assert_eq!(back.code_snip, Some("test snip".to_string()));
        assert_eq!(
            back.variables.get("key"),
            Some(&serde_json::Value::String("value".to_string()))
        );
    }

    #[test]
    fn test_incident_deserialize_kantra_format() {
        // kantra output uses "uri" not "fileURI", and may omit codeLocation
        let json = r#"{
            "uri": "file:///src/App.tsx",
            "message": "Rename Chip to Label",
            "lineNumber": 10,
            "codeSnip": "import { Chip } from '@patternfly/react-core';"
        }"#;
        let incident: Incident = serde_json::from_str(json).unwrap();
        assert_eq!(incident.file_uri, "file:///src/App.tsx");
        assert_eq!(incident.message, "Rename Chip to Label");
        assert_eq!(incident.line_number, Some(10));
        assert!(incident.code_location.is_none());
    }

    #[test]
    fn test_incident_deserialize_provider_format() {
        // Provider format uses "fileURI" and has codeLocation
        let json = r#"{
            "fileURI": "file:///src/App.tsx",
            "lineNumber": 10,
            "codeLocation": {
                "startPosition": {"line": 9, "character": 0},
                "endPosition": {"line": 9, "character": 20}
            }
        }"#;
        let incident: Incident = serde_json::from_str(json).unwrap();
        assert_eq!(incident.file_uri, "file:///src/App.tsx");
        assert_eq!(incident.line_number, Some(10));
        assert!(incident.code_location.is_some());
    }
}

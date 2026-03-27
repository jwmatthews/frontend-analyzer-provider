//! Import declaration scanning.
//!
//! Finds `import { X } from '...'`, `import X from '...'`, `import * as X from '...'`
//! that match a given pattern.

use crate::scanner::make_incident;
use frontend_core::incident::Incident;
use oxc_ast::ast::*;
use oxc_span::GetSpan;
use regex::Regex;
use std::collections::HashMap;

/// Build a map of local identifier → module source from all import declarations.
///
/// This allows JSX scanning to resolve any component or parent back to its
/// import source (e.g., `Button` → `@patternfly/react-core`).
pub fn build_import_map(program: &Program<'_>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for stmt in &program.body {
        if let Statement::ImportDeclaration(import) = stmt {
            let module_source = import.source.value.as_str();
            if let Some(specifiers) = &import.specifiers {
                for spec in specifiers {
                    let local_name = match spec {
                        ImportDeclarationSpecifier::ImportSpecifier(s) => s.local.name.as_str(),
                        ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                            s.local.name.as_str()
                        }
                        ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                            s.local.name.as_str()
                        }
                    };
                    map.insert(local_name.to_string(), module_source.to_string());
                }
            }
        }
    }
    map
}

/// Scan a statement for import declarations matching the pattern.
pub fn scan_imports(
    stmt: &Statement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
) -> Vec<Incident> {
    let mut incidents = Vec::new();

    if let Statement::ImportDeclaration(import) = stmt {
        let module_source = import.source.value.as_str();

        // Check if the module source matches (e.g., pattern = "@patternfly/react-core")
        if pattern.is_match(module_source) {
            let span = import.span();
            let mut incident = make_incident(source, file_uri, span.start, span.end);
            incident.variables.insert(
                "matchingText".into(),
                serde_json::Value::String(
                    source[span.start as usize..span.end as usize].to_string(),
                ),
            );
            incident.variables.insert(
                "module".into(),
                serde_json::Value::String(module_source.to_string()),
            );
            incidents.push(incident);
            return incidents;
        }

        // Check individual specifiers (e.g., pattern = "ToolbarChip")
        if let Some(specifiers) = &import.specifiers {
            for spec in specifiers {
                match spec {
                    ImportDeclarationSpecifier::ImportSpecifier(s) => {
                        let imported_name = s.imported.name().as_str();
                        let local_name = s.local.name.as_str();
                        if pattern.is_match(imported_name) || pattern.is_match(local_name) {
                            let span = s.span();
                            let mut incident =
                                make_incident(source, file_uri, span.start, span.end);
                            incident.variables.insert(
                                "importedName".into(),
                                serde_json::Value::String(imported_name.to_string()),
                            );
                            incident.variables.insert(
                                "localName".into(),
                                serde_json::Value::String(local_name.to_string()),
                            );
                            incident.variables.insert(
                                "module".into(),
                                serde_json::Value::String(module_source.to_string()),
                            );
                            incidents.push(incident);
                        }
                    }
                    ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                        let name = s.local.name.as_str();
                        if pattern.is_match(name) {
                            let span = s.span();
                            let mut incident =
                                make_incident(source, file_uri, span.start, span.end);
                            incident.variables.insert(
                                "importedName".into(),
                                serde_json::Value::String(name.to_string()),
                            );
                            incident.variables.insert(
                                "module".into(),
                                serde_json::Value::String(module_source.to_string()),
                            );
                            incidents.push(incident);
                        }
                    }
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                        let name = s.local.name.as_str();
                        if pattern.is_match(name) {
                            let span = s.span();
                            let mut incident =
                                make_incident(source, file_uri, span.start, span.end);
                            incident.variables.insert(
                                "importedName".into(),
                                serde_json::Value::String(format!("* as {name}")),
                            );
                            incident.variables.insert(
                                "module".into(),
                                serde_json::Value::String(module_source.to_string()),
                            );
                            incidents.push(incident);
                        }
                    }
                }
            }
        }
    }

    incidents
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    // ── build_import_map tests ───────────────────────────────────────────

    #[test]
    fn test_build_import_map_named_imports() {
        let allocator = Allocator::default();
        let source = r#"import { Button, Alert } from '@patternfly/react-core';"#;
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let map = build_import_map(&ret.program);

        assert_eq!(
            map.get("Button"),
            Some(&"@patternfly/react-core".to_string())
        );
        assert_eq!(
            map.get("Alert"),
            Some(&"@patternfly/react-core".to_string())
        );
    }

    #[test]
    fn test_build_import_map_default_import() {
        let allocator = Allocator::default();
        let source = r#"import React from 'react';"#;
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let map = build_import_map(&ret.program);

        assert_eq!(map.get("React"), Some(&"react".to_string()));
    }

    #[test]
    fn test_build_import_map_namespace_import() {
        let allocator = Allocator::default();
        let source = r#"import * as PF from '@patternfly/react-core';"#;
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let map = build_import_map(&ret.program);

        assert_eq!(map.get("PF"), Some(&"@patternfly/react-core".to_string()));
    }

    #[test]
    fn test_build_import_map_aliased_import() {
        let allocator = Allocator::default();
        let source = r#"import { Button as PFButton } from '@patternfly/react-core';"#;
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let map = build_import_map(&ret.program);

        // Keyed by local name
        assert_eq!(
            map.get("PFButton"),
            Some(&"@patternfly/react-core".to_string())
        );
        assert!(map.get("Button").is_none()); // imported name, not local
    }

    #[test]
    fn test_build_import_map_multiple_imports() {
        let allocator = Allocator::default();
        let source = r#"
import { Button } from '@patternfly/react-core';
import { Table } from '@patternfly/react-table';
import React from 'react';
"#;
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let map = build_import_map(&ret.program);

        assert_eq!(
            map.get("Button"),
            Some(&"@patternfly/react-core".to_string())
        );
        assert_eq!(
            map.get("Table"),
            Some(&"@patternfly/react-table".to_string())
        );
        assert_eq!(map.get("React"), Some(&"react".to_string()));
    }

    #[test]
    fn test_build_import_map_empty_source() {
        let allocator = Allocator::default();
        let source = "const x = 1;";
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let map = build_import_map(&ret.program);

        assert!(map.is_empty());
    }

    // ── scan_imports tests ───────────────────────────────────────────────

    #[test]
    fn test_scan_imports_matches_module_source() {
        let allocator = Allocator::default();
        let source = r#"import { Button } from '@patternfly/react-core';"#;
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let pattern = Regex::new(r"@patternfly/react-core").unwrap();

        let incidents: Vec<Incident> = ret
            .program
            .body
            .iter()
            .flat_map(|stmt| scan_imports(stmt, source, &pattern, "file:///test.tsx"))
            .collect();

        assert_eq!(incidents.len(), 1);
        assert!(incidents[0].variables.contains_key("module"));
        assert!(incidents[0].variables.contains_key("matchingText"));
    }

    #[test]
    fn test_scan_imports_matches_specifier_name() {
        let allocator = Allocator::default();
        let source = r#"import { Chip, Button } from '@patternfly/react-core';"#;
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let pattern = Regex::new(r"^Chip$").unwrap();

        let incidents: Vec<Incident> = ret
            .program
            .body
            .iter()
            .flat_map(|stmt| scan_imports(stmt, source, &pattern, "file:///test.tsx"))
            .collect();

        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("importedName"),
            Some(&serde_json::Value::String("Chip".to_string()))
        );
        assert_eq!(
            incidents[0].variables.get("module"),
            Some(&serde_json::Value::String(
                "@patternfly/react-core".to_string()
            ))
        );
    }

    #[test]
    fn test_scan_imports_no_match() {
        let allocator = Allocator::default();
        let source = r#"import { Button } from '@patternfly/react-core';"#;
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let pattern = Regex::new(r"^NonExistent$").unwrap();

        let incidents: Vec<Incident> = ret
            .program
            .body
            .iter()
            .flat_map(|stmt| scan_imports(stmt, source, &pattern, "file:///test.tsx"))
            .collect();

        assert!(incidents.is_empty());
    }

    #[test]
    fn test_scan_imports_default_import() {
        let allocator = Allocator::default();
        let source = r#"import React from 'react';"#;
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let pattern = Regex::new(r"^React$").unwrap();

        let incidents: Vec<Incident> = ret
            .program
            .body
            .iter()
            .flat_map(|stmt| scan_imports(stmt, source, &pattern, "file:///test.tsx"))
            .collect();

        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("importedName"),
            Some(&serde_json::Value::String("React".to_string()))
        );
    }

    #[test]
    fn test_scan_imports_aliased_matches_local() {
        let allocator = Allocator::default();
        let source = r#"import { Chip as OldChip } from '@patternfly/react-core';"#;
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let pattern = Regex::new(r"^OldChip$").unwrap();

        let incidents: Vec<Incident> = ret
            .program
            .body
            .iter()
            .flat_map(|stmt| scan_imports(stmt, source, &pattern, "file:///test.tsx"))
            .collect();

        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("importedName"),
            Some(&serde_json::Value::String("Chip".to_string()))
        );
        assert_eq!(
            incidents[0].variables.get("localName"),
            Some(&serde_json::Value::String("OldChip".to_string()))
        );
    }

    #[test]
    fn test_scan_imports_namespace() {
        let allocator = Allocator::default();
        let source = r#"import * as PF from '@patternfly/react-core';"#;
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let pattern = Regex::new(r"^PF$").unwrap();

        let incidents: Vec<Incident> = ret
            .program
            .body
            .iter()
            .flat_map(|stmt| scan_imports(stmt, source, &pattern, "file:///test.tsx"))
            .collect();

        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("importedName"),
            Some(&serde_json::Value::String("* as PF".to_string()))
        );
    }

    #[test]
    fn test_scan_imports_non_import_statement_ignored() {
        let allocator = Allocator::default();
        let source = r#"const Button = 'not an import';"#;
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let pattern = Regex::new(r"Button").unwrap();

        let incidents: Vec<Incident> = ret
            .program
            .body
            .iter()
            .flat_map(|stmt| scan_imports(stmt, source, &pattern, "file:///test.tsx"))
            .collect();

        assert!(incidents.is_empty());
    }
}

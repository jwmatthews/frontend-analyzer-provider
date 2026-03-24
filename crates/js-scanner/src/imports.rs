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

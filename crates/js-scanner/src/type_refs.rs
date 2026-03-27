//! Type reference scanning.
//!
//! Finds TypeScript type references like `const x: ButtonProps = ...`
//! and type annotations in function signatures.

use crate::scanner::make_incident;
use frontend_core::incident::Incident;
use oxc_ast::ast::*;
use oxc_span::GetSpan;
use regex::Regex;

/// Scan a statement for type reference matches.
pub fn scan_type_refs(
    stmt: &Statement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
) -> Vec<Incident> {
    let mut incidents = Vec::new();

    match stmt {
        Statement::TSTypeAliasDeclaration(alias) => {
            let name = alias.id.name.as_str();
            if pattern.is_match(name) {
                let span = alias.id.span();
                let mut incident = make_incident(source, file_uri, span.start, span.end);
                incident.variables.insert(
                    "typeName".into(),
                    serde_json::Value::String(name.to_string()),
                );
                incidents.push(incident);
            }
            check_ts_type(
                &alias.type_annotation,
                source,
                pattern,
                file_uri,
                &mut incidents,
            );
        }
        Statement::TSInterfaceDeclaration(iface) => {
            let name = iface.id.name.as_str();
            if pattern.is_match(name) {
                let span = iface.id.span();
                let mut incident = make_incident(source, file_uri, span.start, span.end);
                incident.variables.insert(
                    "typeName".into(),
                    serde_json::Value::String(name.to_string()),
                );
                incidents.push(incident);
            }
            for heritage in &iface.extends {
                let ext_span = heritage.expression.span();
                let ext_name = source
                    .get(ext_span.start as usize..ext_span.end as usize)
                    .unwrap_or_default();
                if pattern.is_match(ext_name) {
                    let span = heritage.span();
                    let mut incident = make_incident(source, file_uri, span.start, span.end);
                    incident.variables.insert(
                        "typeName".into(),
                        serde_json::Value::String(ext_name.to_string()),
                    );
                    incidents.push(incident);
                }
            }
        }
        Statement::VariableDeclaration(var_decl) => {
            for declarator in &var_decl.declarations {
                if let Some(annotation) = &declarator.type_annotation {
                    check_ts_type(
                        &annotation.type_annotation,
                        source,
                        pattern,
                        file_uri,
                        &mut incidents,
                    );
                }
            }
        }
        Statement::ExportNamedDeclaration(decl) => {
            if let Some(declaration) = &decl.declaration {
                match declaration {
                    Declaration::TSTypeAliasDeclaration(alias) => {
                        let name = alias.id.name.as_str();
                        if pattern.is_match(name) {
                            let span = alias.id.span();
                            let mut incident =
                                make_incident(source, file_uri, span.start, span.end);
                            incident.variables.insert(
                                "typeName".into(),
                                serde_json::Value::String(name.to_string()),
                            );
                            incidents.push(incident);
                        }
                        check_ts_type(
                            &alias.type_annotation,
                            source,
                            pattern,
                            file_uri,
                            &mut incidents,
                        );
                    }
                    Declaration::TSInterfaceDeclaration(iface) => {
                        let name = iface.id.name.as_str();
                        if pattern.is_match(name) {
                            let span = iface.id.span();
                            let mut incident =
                                make_incident(source, file_uri, span.start, span.end);
                            incident.variables.insert(
                                "typeName".into(),
                                serde_json::Value::String(name.to_string()),
                            );
                            incidents.push(incident);
                        }
                    }
                    Declaration::VariableDeclaration(var_decl) => {
                        for declarator in &var_decl.declarations {
                            if let Some(annotation) = &declarator.type_annotation {
                                check_ts_type(
                                    &annotation.type_annotation,
                                    source,
                                    pattern,
                                    file_uri,
                                    &mut incidents,
                                );
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    incidents
}

fn check_ts_type(
    ts_type: &TSType<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    incidents: &mut Vec<Incident>,
) {
    match ts_type {
        TSType::TSTypeReference(type_ref) => {
            let name_span = type_ref.type_name.span();
            let name = source
                .get(name_span.start as usize..name_span.end as usize)
                .unwrap_or_default();
            if pattern.is_match(name) {
                let mut incident = make_incident(source, file_uri, name_span.start, name_span.end);
                incident.variables.insert(
                    "typeName".into(),
                    serde_json::Value::String(name.to_string()),
                );
                incidents.push(incident);
            }
            if let Some(type_args) = &type_ref.type_arguments {
                for param in &type_args.params {
                    check_ts_type(param, source, pattern, file_uri, incidents);
                }
            }
        }
        TSType::TSUnionType(union) => {
            for t in &union.types {
                check_ts_type(t, source, pattern, file_uri, incidents);
            }
        }
        TSType::TSIntersectionType(inter) => {
            for t in &inter.types {
                check_ts_type(t, source, pattern, file_uri, incidents);
            }
        }
        TSType::TSArrayType(arr) => {
            check_ts_type(&arr.element_type, source, pattern, file_uri, incidents);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn scan_source(source: &str, pattern: &str) -> Vec<Incident> {
        let allocator = Allocator::default();
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let re = Regex::new(pattern).unwrap();

        ret.program
            .body
            .iter()
            .flat_map(|stmt| scan_type_refs(stmt, source, &re, "file:///test.tsx"))
            .collect()
    }

    #[test]
    fn test_type_alias_matches() {
        let incidents = scan_source("type MyProps = ButtonProps;", r"^MyProps$");
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("typeName"),
            Some(&serde_json::Value::String("MyProps".to_string()))
        );
    }

    #[test]
    fn test_type_alias_rhs_reference() {
        let incidents = scan_source("type MyProps = ButtonProps;", r"^ButtonProps$");
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("typeName"),
            Some(&serde_json::Value::String("ButtonProps".to_string()))
        );
    }

    #[test]
    fn test_interface_declaration() {
        let incidents = scan_source("interface MyInterface {}", r"^MyInterface$");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_interface_extends() {
        let incidents = scan_source("interface MyProps extends ButtonProps {}", r"^ButtonProps$");
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("typeName"),
            Some(&serde_json::Value::String("ButtonProps".to_string()))
        );
    }

    #[test]
    fn test_variable_type_annotation() {
        let incidents = scan_source("const x: ButtonProps = {};", r"^ButtonProps$");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_union_type_reference() {
        let incidents = scan_source(
            "type Combined = ButtonProps | AlertProps;",
            r"^ButtonProps$",
        );
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_no_match() {
        let incidents = scan_source("type Foo = string;", r"^ButtonProps$");
        assert!(incidents.is_empty());
    }

    #[test]
    fn test_exported_type_alias() {
        let incidents = scan_source("export type MyProps = ButtonProps;", r"^MyProps$");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_exported_interface() {
        let incidents = scan_source("export interface MyInterface {}", r"^MyInterface$");
        assert_eq!(incidents.len(), 1);
    }

    #[test]
    fn test_array_type_reference() {
        let incidents = scan_source("const items: ButtonProps[] = [];", r"^ButtonProps$");
        assert_eq!(incidents.len(), 1);
    }
}

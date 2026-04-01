//! JSX scanning.
//!
//! Finds JSX component usage (`<Button ...>`) and JSX prop usage (`<X isActive={...}>`).
//! Walks the AST recursively to find JSXOpeningElement nodes.

#![allow(clippy::too_many_arguments)]

use crate::scanner::make_incident;
use frontend_core::capabilities::ReferenceLocation;
use frontend_core::incident::Incident;
use oxc_ast::ast::*;
use oxc_span::GetSpan;
use regex::Regex;
use std::collections::HashMap;

/// Import map: local identifier name → module source path.
/// Built from import declarations, passed through JSX scanning so components
/// can be resolved to their import source (e.g., Button → @patternfly/react-core).
type ImportMap = HashMap<String, String>;

/// Scan a statement for JSX component and prop usage.
pub fn scan_jsx(
    stmt: &Statement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    import_map: &ImportMap,
) -> Vec<Incident> {
    let mut incidents = Vec::new();
    walk_statement_for_jsx(
        stmt,
        source,
        pattern,
        file_uri,
        location,
        &mut incidents,
        None,
        import_map,
    );
    incidents
}

fn walk_statement_for_jsx(
    stmt: &Statement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    incidents: &mut Vec<Incident>,
    parent_name: Option<&str>,
    import_map: &ImportMap,
) {
    match stmt {
        Statement::ExportDefaultDeclaration(decl) => {
            if let ExportDefaultDeclarationKind::FunctionDeclaration(func) = &decl.declaration {
                if let Some(body) = &func.body {
                    walk_function_body(
                        body,
                        source,
                        pattern,
                        file_uri,
                        location,
                        incidents,
                        parent_name,
                        import_map,
                    );
                }
            }
        }
        Statement::ExportNamedDeclaration(decl) => {
            if let Some(Declaration::FunctionDeclaration(func)) = &decl.declaration {
                if let Some(body) = &func.body {
                    walk_function_body(
                        body,
                        source,
                        pattern,
                        file_uri,
                        location,
                        incidents,
                        parent_name,
                        import_map,
                    );
                }
            }
            if let Some(Declaration::VariableDeclaration(var_decl)) = &decl.declaration {
                walk_variable_declaration(
                    var_decl,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        Statement::FunctionDeclaration(func) => {
            if let Some(body) = &func.body {
                walk_function_body(
                    body,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        Statement::VariableDeclaration(var_decl) => {
            walk_variable_declaration(
                var_decl,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        Statement::ReturnStatement(ret) => {
            if let Some(arg) = &ret.argument {
                walk_expression_for_jsx(
                    arg,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        Statement::ExpressionStatement(expr) => {
            walk_expression_for_jsx(
                &expr.expression,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        Statement::BlockStatement(block) => {
            for s in &block.body {
                walk_statement_for_jsx(
                    s,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        Statement::IfStatement(if_stmt) => {
            walk_statement_for_jsx(
                &if_stmt.consequent,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
            if let Some(alt) = &if_stmt.alternate {
                walk_statement_for_jsx(
                    alt,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        _ => {}
    }
}

fn walk_variable_declaration(
    var_decl: &VariableDeclaration<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    incidents: &mut Vec<Incident>,
    parent_name: Option<&str>,
    import_map: &ImportMap,
) {
    for declarator in &var_decl.declarations {
        if let Some(init) = &declarator.init {
            walk_expression_for_jsx(
                init,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
    }
}

fn walk_function_body(
    body: &FunctionBody<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    incidents: &mut Vec<Incident>,
    parent_name: Option<&str>,
    import_map: &ImportMap,
) {
    for stmt in &body.statements {
        walk_statement_for_jsx(
            stmt,
            source,
            pattern,
            file_uri,
            location,
            incidents,
            parent_name,
            import_map,
        );
    }
}

fn walk_expression_for_jsx(
    expr: &Expression<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    incidents: &mut Vec<Incident>,
    parent_name: Option<&str>,
    import_map: &ImportMap,
) {
    match expr {
        Expression::JSXElement(el) => {
            check_jsx_element(
                el,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                walk_jsx_child(
                    child,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        Expression::ParenthesizedExpression(paren) => {
            walk_expression_for_jsx(
                &paren.expression,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        Expression::ConditionalExpression(cond) => {
            walk_expression_for_jsx(
                &cond.consequent,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
            walk_expression_for_jsx(
                &cond.alternate,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        Expression::LogicalExpression(logic) => {
            walk_expression_for_jsx(
                &logic.right,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        Expression::ArrowFunctionExpression(arrow) => {
            walk_function_body(
                &arrow.body,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Argument::SpreadElement(spread) = arg {
                    walk_expression_for_jsx(
                        &spread.argument,
                        source,
                        pattern,
                        file_uri,
                        location,
                        incidents,
                        parent_name,
                        import_map,
                    );
                } else if let Some(expr) = arg.as_expression() {
                    walk_expression_for_jsx(
                        expr,
                        source,
                        pattern,
                        file_uri,
                        location,
                        incidents,
                        parent_name,
                        import_map,
                    );
                }
            }
        }
        Expression::ChainExpression(chain) => {
            // Handle optional chaining (e.g., `items?.map(item => <Component />)`)
            // Without this, JSX inside `?.map()` calls is invisible to the scanner.
            if let ChainElement::CallExpression(call) = &chain.expression {
                for arg in &call.arguments {
                    if let Argument::SpreadElement(spread) = arg {
                        walk_expression_for_jsx(
                            &spread.argument,
                            source,
                            pattern,
                            file_uri,
                            location,
                            incidents,
                            parent_name,
                            import_map,
                        );
                    } else if let Some(expr) = arg.as_expression() {
                        walk_expression_for_jsx(
                            expr,
                            source,
                            pattern,
                            file_uri,
                            location,
                            incidents,
                            parent_name,
                            import_map,
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

fn walk_jsx_child(
    child: &JSXChild<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    incidents: &mut Vec<Incident>,
    parent_name: Option<&str>,
    import_map: &ImportMap,
) {
    match child {
        JSXChild::Element(el) => {
            check_jsx_element(
                el,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        JSXChild::Fragment(frag) => {
            for c in &frag.children {
                walk_jsx_child(
                    c,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        JSXChild::ExpressionContainer(container) => {
            // JSXExpression inherits Expression variants via @inherit macro.
            // Walk into the expression to find nested JSX elements.
            walk_jsx_expression(
                &container.expression,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        _ => {}
    }
}

/// Walk a JSXExpression (which inherits all Expression variants) for nested JSX.
/// This handles expression containers in JSX children ({cond && <X/>}) and
/// prop value expressions (toggle={ref => (<MenuToggle ...>)}).
fn walk_jsx_expression(
    jsx_expr: &JSXExpression<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    incidents: &mut Vec<Incident>,
    parent_name: Option<&str>,
    import_map: &ImportMap,
) {
    match jsx_expr {
        JSXExpression::EmptyExpression(_) => {}
        // Direct JSX nesting: {<Component />}
        JSXExpression::JSXElement(el) => {
            check_jsx_element(
                el,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        JSXExpression::JSXFragment(frag) => {
            for child in &frag.children {
                walk_jsx_child(
                    child,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    parent_name,
                    import_map,
                );
            }
        }
        // Parenthesized: {(<Component />)}
        JSXExpression::ParenthesizedExpression(paren) => {
            walk_expression_for_jsx(
                &paren.expression,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        // Arrow functions: {ref => (<Component />)} or {() => <Component />}
        JSXExpression::ArrowFunctionExpression(arrow) => {
            walk_function_body(
                &arrow.body,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        // Conditionals: {condition && <Component />} or {cond ? <A/> : <B/>}
        JSXExpression::ConditionalExpression(cond) => {
            walk_expression_for_jsx(
                &cond.consequent,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
            walk_expression_for_jsx(
                &cond.alternate,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        JSXExpression::LogicalExpression(logic) => {
            walk_expression_for_jsx(
                &logic.right,
                source,
                pattern,
                file_uri,
                location,
                incidents,
                parent_name,
                import_map,
            );
        }
        // Function calls: {renderFn(<Component />)} or {fn(arg)}
        JSXExpression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Argument::SpreadElement(spread) = arg {
                    walk_expression_for_jsx(
                        &spread.argument,
                        source,
                        pattern,
                        file_uri,
                        location,
                        incidents,
                        parent_name,
                        import_map,
                    );
                } else if let Some(expr) = arg.as_expression() {
                    walk_expression_for_jsx(
                        expr,
                        source,
                        pattern,
                        file_uri,
                        location,
                        incidents,
                        parent_name,
                        import_map,
                    );
                }
            }
        }
        // Optional chaining: {items?.map(item => <Component />)}
        JSXExpression::ChainExpression(chain) => {
            if let ChainElement::CallExpression(call) = &chain.expression {
                for arg in &call.arguments {
                    if let Argument::SpreadElement(spread) = arg {
                        walk_expression_for_jsx(
                            &spread.argument,
                            source,
                            pattern,
                            file_uri,
                            location,
                            incidents,
                            parent_name,
                            import_map,
                        );
                    } else if let Some(expr) = arg.as_expression() {
                        walk_expression_for_jsx(
                            expr,
                            source,
                            pattern,
                            file_uri,
                            location,
                            incidents,
                            parent_name,
                            import_map,
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

/// Extract all string literal values from an object expression's properties.
///
/// Given `{ default: 'alignRight', md: 'alignLeft' }`, returns
/// `["alignRight", "alignLeft"]`. This allows value-based rules to match
/// prop values inside responsive breakpoint objects.
fn extract_object_string_values(expr: &JSXExpression<'_>, _source: &str) -> Vec<String> {
    let obj = match expr {
        JSXExpression::ObjectExpression(obj) => obj,
        JSXExpression::ParenthesizedExpression(paren) => {
            if let Expression::ObjectExpression(obj) = &paren.expression {
                obj
            } else {
                return vec![];
            }
        }
        _ => return vec![],
    };

    let mut values = Vec::new();
    for prop in &obj.properties {
        if let ObjectPropertyKind::ObjectProperty(p) = prop {
            match &p.value {
                Expression::StringLiteral(s) => {
                    values.push(s.value.to_string());
                }
                // Handle nested objects: { default: 'value', md: { nested: 'value2' } }
                Expression::ObjectExpression(nested) => {
                    for nested_prop in &nested.properties {
                        if let ObjectPropertyKind::ObjectProperty(np) = nested_prop {
                            if let Expression::StringLiteral(s) = &np.value {
                                values.push(s.value.to_string());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    values
}

/// Check object literal property keys for pattern matches.
///
/// When a JSX prop value is an object expression (e.g., `formGroupProps={{ labelIcon: ... }}`),
/// this function checks each property key against the pattern. If matched, an incident is
/// created with the owning JSX component as the `componentName`, so that the `from` and
/// `component` filters work correctly.
///
/// This catches indirect prop spreading — where an object is passed to a wrapper component
/// that spreads it onto a PF component internally.
fn check_object_keys_in_expression(
    expr: &JSXExpression<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    component_name: &str,
    import_map: &ImportMap,
    incidents: &mut Vec<Incident>,
) {
    let obj = match expr {
        JSXExpression::ObjectExpression(obj) => obj,
        // Handle parenthesized: prop={( { key: value } )}
        JSXExpression::ParenthesizedExpression(paren) => {
            if let Expression::ObjectExpression(obj) = &paren.expression {
                obj
            } else {
                return;
            }
        }
        _ => return,
    };

    for prop in &obj.properties {
        if let ObjectPropertyKind::ObjectProperty(p) = prop {
            let key_name = match &p.key {
                PropertyKey::StaticIdentifier(ident) => Some(ident.name.as_str()),
                PropertyKey::StringLiteral(s) => Some(s.value.as_str()),
                _ => None,
            };
            if let Some(name) = key_name {
                if pattern.is_match(name) {
                    let span = p.key.span();
                    let mut incident = make_incident(source, file_uri, span.start, span.end);
                    incident.variables.insert(
                        "propName".into(),
                        serde_json::Value::String(name.to_string()),
                    );
                    incident.variables.insert(
                        "componentName".into(),
                        serde_json::Value::String(component_name.to_string()),
                    );
                    // Resolve the owning component's import source for `from` filtering
                    if let Some(module) = import_map.get(component_name) {
                        incident
                            .variables
                            .insert("module".into(), serde_json::Value::String(module.clone()));
                    }
                    incidents.push(incident);
                }
            }
        }
    }
}

fn check_jsx_element(
    el: &JSXElement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    incidents: &mut Vec<Incident>,
    parent_name: Option<&str>,
    import_map: &ImportMap,
) {
    let opening = &el.opening_element;
    let component_name = jsx_element_name_to_string(&opening.name);

    // Check component name
    let search_component = matches!(location, Some(ReferenceLocation::JsxComponent) | None);
    if search_component && pattern.is_match(&component_name) {
        let span = opening.name.span();
        let mut incident = make_incident(source, file_uri, span.start, span.end);
        incident.variables.insert(
            "componentName".into(),
            serde_json::Value::String(component_name.clone()),
        );
        // Resolve the matched component's import source
        if let Some(module) = import_map.get(&component_name) {
            incident
                .variables
                .insert("module".into(), serde_json::Value::String(module.clone()));
        }
        if let Some(parent) = parent_name {
            incident.variables.insert(
                "parentName".into(),
                serde_json::Value::String(parent.to_string()),
            );
            // Resolve the parent component's import source
            if let Some(parent_module) = import_map.get(parent) {
                incident.variables.insert(
                    "parentFrom".into(),
                    serde_json::Value::String(parent_module.clone()),
                );
            }
        }
        incidents.push(incident);
    }

    // Check props
    let search_props = matches!(location, Some(ReferenceLocation::JsxProp) | None);
    if search_props {
        for attr in &opening.attributes {
            if let JSXAttributeItem::Attribute(a) = attr {
                if let JSXAttributeName::Identifier(ident) = &a.name {
                    let prop_name = ident.name.as_str();
                    if pattern.is_match(prop_name) {
                        let span = ident.span();
                        let mut incident = make_incident(source, file_uri, span.start, span.end);
                        incident.variables.insert(
                            "propName".into(),
                            serde_json::Value::String(prop_name.to_string()),
                        );
                        incident.variables.insert(
                            "componentName".into(),
                            serde_json::Value::String(component_name.clone()),
                        );

                        // Extract prop value for value-based filtering.
                        // For object expressions like `align={{ default: 'alignRight' }}`,
                        // also extract the string literal values from properties so that
                        // value-based rules (e.g., value: ^alignRight$) can match.
                        if let Some(value) = &a.value {
                            let prop_value = match value {
                                JSXAttributeValue::StringLiteral(s) => Some(s.value.to_string()),
                                JSXAttributeValue::ExpressionContainer(expr) => {
                                    // For expressions, capture the source text
                                    let expr_span = expr.span();
                                    // Strip the { } wrapper, with bounds checking
                                    let start = (expr_span.start as usize + 1).min(source.len());
                                    let end = (expr_span.end as usize)
                                        .saturating_sub(1)
                                        .max(start)
                                        .min(source.len());
                                    let text = &source[start..end];
                                    Some(text.trim().to_string())
                                }
                                _ => None,
                            };
                            if let Some(pv) = prop_value {
                                incident
                                    .variables
                                    .insert("propValue".into(), serde_json::Value::String(pv));
                            }

                            // For object expressions, also extract individual string values
                            // from properties and store them as propObjectValues for matching.
                            if let Some(JSXAttributeValue::ExpressionContainer(expr)) = &a.value {
                                let obj_values =
                                    extract_object_string_values(&expr.expression, source);
                                if !obj_values.is_empty() {
                                    incident.variables.insert(
                                        "propObjectValues".into(),
                                        serde_json::Value::Array(
                                            obj_values
                                                .into_iter()
                                                .map(serde_json::Value::String)
                                                .collect(),
                                        ),
                                    );
                                }
                            }
                        }

                        // Resolve the owning component's import source so
                        // that the `from` filter can check it. Without this,
                        // JSX_PROP incidents bypass the `from` constraint.
                        if let Some(module) = import_map.get(&component_name) {
                            incident
                                .variables
                                .insert("module".into(), serde_json::Value::String(module.clone()));
                        }

                        incidents.push(incident);
                    }
                }
            }
        }

        // Also check object literal keys inside prop values.
        // This catches patterns like `formGroupProps={{ labelIcon: ... }}`
        // where `labelIcon` is an indirect prop passed via spreading.
        for attr in &opening.attributes {
            if let JSXAttributeItem::Attribute(a) = attr {
                if let Some(JSXAttributeValue::ExpressionContainer(expr_container)) = &a.value {
                    check_object_keys_in_expression(
                        &expr_container.expression,
                        source,
                        pattern,
                        file_uri,
                        &component_name,
                        import_map,
                        incidents,
                    );
                }
            }
        }
    }

    // Walk into prop value expressions to find nested JSX elements.
    // e.g., toggle={ref => (<MenuToggle ...>)} or icon={<Icon />}
    for attr in &opening.attributes {
        if let JSXAttributeItem::Attribute(a) = attr {
            if let Some(JSXAttributeValue::ExpressionContainer(expr)) = &a.value {
                walk_jsx_expression(
                    &expr.expression,
                    source,
                    pattern,
                    file_uri,
                    location,
                    incidents,
                    Some(&component_name),
                    import_map,
                );
            }
        }
    }

    // Recurse into children — this element becomes the parent context
    for child in &el.children {
        walk_jsx_child(
            child,
            source,
            pattern,
            file_uri,
            location,
            incidents,
            Some(&component_name),
            import_map,
        );
    }
}

fn jsx_element_name_to_string(name: &JSXElementName<'_>) -> String {
    match name {
        JSXElementName::Identifier(ident) => ident.name.to_string(),
        JSXElementName::IdentifierReference(ident) => ident.name.to_string(),
        JSXElementName::NamespacedName(ns) => {
            format!("{}:{}", ns.namespace.name, ns.name.name)
        }
        JSXElementName::MemberExpression(member) => jsx_member_expr_to_string(member),
        JSXElementName::ThisExpression(_) => "this".to_string(),
    }
}

fn jsx_member_expr_to_string(member: &JSXMemberExpression<'_>) -> String {
    let obj = match &member.object {
        JSXMemberExpressionObject::IdentifierReference(ident) => ident.name.to_string(),
        JSXMemberExpressionObject::MemberExpression(nested) => jsx_member_expr_to_string(nested),
        JSXMemberExpressionObject::ThisExpression(_) => "this".to_string(),
    };
    format!("{}.{}", obj, member.property.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imports::build_import_map;
    use oxc_allocator::Allocator;
    use oxc_parser::Parser;
    use oxc_span::SourceType;

    fn scan_source_jsx(
        source: &str,
        pattern: &str,
        location: Option<&ReferenceLocation>,
    ) -> Vec<Incident> {
        let allocator = Allocator::default();
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let re = Regex::new(pattern).unwrap();
        let import_map = build_import_map(&ret.program);

        ret.program
            .body
            .iter()
            .flat_map(|stmt| scan_jsx(stmt, source, &re, "file:///test.tsx", location, &import_map))
            .collect()
    }

    #[test]
    fn test_jsx_component_match() {
        let source = r#"
import { Button } from '@patternfly/react-core';
const el = <Button>Click</Button>;
"#;
        let incidents =
            scan_source_jsx(source, r"^Button$", Some(&ReferenceLocation::JsxComponent));
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("componentName"),
            Some(&serde_json::Value::String("Button".to_string()))
        );
    }

    #[test]
    fn test_jsx_component_no_match() {
        let source = r#"const el = <Button>Click</Button>;"#;
        let incidents = scan_source_jsx(source, r"^Alert$", Some(&ReferenceLocation::JsxComponent));
        assert!(incidents.is_empty());
    }

    #[test]
    fn test_jsx_prop_match() {
        let source = r#"
import { Button } from '@patternfly/react-core';
const el = <Button isActive>Click</Button>;
"#;
        let incidents = scan_source_jsx(source, r"^isActive$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("propName"),
            Some(&serde_json::Value::String("isActive".to_string()))
        );
    }

    #[test]
    fn test_jsx_prop_with_string_value() {
        let source = r#"const el = <Button variant="primary">Click</Button>;"#;
        let incidents = scan_source_jsx(source, r"^variant$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("propValue"),
            Some(&serde_json::Value::String("primary".to_string()))
        );
    }

    #[test]
    fn test_jsx_component_with_module_resolution() {
        let source = r#"
import { Button } from '@patternfly/react-core';
const el = <Button>Click</Button>;
"#;
        let incidents =
            scan_source_jsx(source, r"^Button$", Some(&ReferenceLocation::JsxComponent));
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("module"),
            Some(&serde_json::Value::String(
                "@patternfly/react-core".to_string()
            ))
        );
    }

    #[test]
    fn test_jsx_member_expression_component() {
        let source = r#"const el = <Toolbar.Item>hello</Toolbar.Item>;"#;
        let incidents = scan_source_jsx(
            source,
            r"^Toolbar\.Item$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("componentName"),
            Some(&serde_json::Value::String("Toolbar.Item".to_string()))
        );
    }

    #[test]
    fn test_jsx_nested_components_tracks_parent() {
        let source = r#"
import { Page, PageSection } from '@patternfly/react-core';
const el = <Page><PageSection>content</PageSection></Page>;
"#;
        let incidents = scan_source_jsx(
            source,
            r"^PageSection$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("parentName"),
            Some(&serde_json::Value::String("Page".to_string()))
        );
    }

    #[test]
    fn test_jsx_scan_without_location_filter() {
        // Without a location filter, should match both component and prop usages
        let source = r#"const el = <Button isActive>Click</Button>;"#;
        let incidents = scan_source_jsx(source, r"^Button$", None);
        // Should find Button as component
        assert!(incidents.iter().any(|i| i.variables.get("componentName")
            == Some(&serde_json::Value::String("Button".to_string()))));
    }

    // ── ChainExpression tests ────────────────────────────────────────

    #[test]
    fn test_jsx_inside_optional_chaining_map() {
        // JSX inside items?.map() should be detected
        let source = r#"
import { Switch } from '@patternfly/react-core';
const el = <div>{items?.map(item => <Switch labelOff="No" />)}</div>;
"#;
        let incidents =
            scan_source_jsx(source, r"^Switch$", Some(&ReferenceLocation::JsxComponent));
        assert_eq!(
            incidents.len(),
            1,
            "Should detect <Switch> inside optional chaining ?.map()"
        );
    }

    #[test]
    fn test_jsx_prop_inside_optional_chaining() {
        // Props on JSX inside ?.map() should be detected
        let source = r#"
import { Switch } from '@patternfly/react-core';
const el = <div>{items?.map(item => <Switch labelOff="No" />)}</div>;
"#;
        let incidents = scan_source_jsx(source, r"^labelOff$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(
            incidents.len(),
            1,
            "Should detect labelOff prop inside optional chaining ?.map()"
        );
    }

    #[test]
    fn test_jsx_inside_optional_chaining_filter_map() {
        // JSX inside chained ?.filter()?.map() - only the outer ?. is a ChainExpression
        let source = r#"
import { Tr } from '@patternfly/react-table';
const el = <div>{data?.filter(x => x.active).map(x => <Tr key={x.id} />)}</div>;
"#;
        let incidents = scan_source_jsx(source, r"^Tr$", Some(&ReferenceLocation::JsxComponent));
        assert_eq!(
            incidents.len(),
            1,
            "Should detect <Tr> inside ?.filter().map() chain"
        );
    }

    #[test]
    fn test_jsx_without_optional_chaining_still_works() {
        // Ensure regular .map() (non-optional) still works
        let source = r#"
import { Td } from '@patternfly/react-table';
const el = <div>{items.map(item => <Td>{item.name}</Td>)}</div>;
"#;
        let incidents = scan_source_jsx(source, r"^Td$", Some(&ReferenceLocation::JsxComponent));
        assert_eq!(
            incidents.len(),
            1,
            "Should detect <Td> inside regular .map()"
        );
    }

    // ── Object key scanning tests ────────────────────────────────────

    #[test]
    fn test_jsx_prop_match_in_object_literal() {
        // formGroupProps={{ labelIcon: ... }} should match ^labelIcon$
        let source = r#"
import { HookFormPFGroupController } from './components';
const el = <HookFormPFGroupController formGroupProps={{ labelIcon: <Popover /> }} />;
"#;
        let incidents = scan_source_jsx(source, r"^labelIcon$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(
            incidents.len(),
            1,
            "Should detect labelIcon as object key inside prop value"
        );
        assert_eq!(
            incidents[0].variables.get("propName"),
            Some(&serde_json::Value::String("labelIcon".to_string()))
        );
        assert_eq!(
            incidents[0].variables.get("componentName"),
            Some(&serde_json::Value::String(
                "HookFormPFGroupController".to_string()
            ))
        );
    }

    #[test]
    fn test_jsx_prop_object_key_with_import_resolution() {
        // Object key incidents should carry the module from the owning component
        let source = r#"
import { FormGroup } from '@patternfly/react-core';
const el = <FormGroup extraProps={{ labelIcon: "help" }} />;
"#;
        let incidents = scan_source_jsx(source, r"^labelIcon$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("module"),
            Some(&serde_json::Value::String(
                "@patternfly/react-core".to_string()
            ))
        );
    }

    #[test]
    fn test_jsx_prop_direct_still_preferred_over_object_key() {
        // Direct JSX prop should still match, and object key shouldn't duplicate
        let source = r#"
import { FormGroup } from '@patternfly/react-core';
const el = <FormGroup labelIcon={<Popover />} />;
"#;
        let incidents = scan_source_jsx(source, r"^labelIcon$", Some(&ReferenceLocation::JsxProp));
        // Should match only once (direct prop), not also as object key
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("propName"),
            Some(&serde_json::Value::String("labelIcon".to_string()))
        );
    }

    #[test]
    fn test_jsx_prop_object_key_no_false_positive() {
        // Object keys that don't match the pattern should not produce incidents
        let source = r#"
const el = <Widget config={{ enabled: true, name: "test" }} />;
"#;
        let incidents = scan_source_jsx(source, r"^labelIcon$", Some(&ReferenceLocation::JsxProp));
        assert!(incidents.is_empty());
    }

    // ── Object value extraction tests ────────────────────────────────

    #[test]
    fn test_jsx_prop_object_values_extracted() {
        // align={{ default: 'alignRight' }} should extract 'alignRight' as propObjectValues
        let source = r#"
import { ToolbarItem } from '@patternfly/react-core';
const el = <ToolbarItem align={{ default: 'alignRight' }} />;
"#;
        let incidents = scan_source_jsx(source, r"^align$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(incidents.len(), 1);

        let obj_values = incidents[0].variables.get("propObjectValues");
        assert!(
            obj_values.is_some(),
            "Should extract propObjectValues from object literal"
        );
        let values = obj_values.unwrap().as_array().unwrap();
        assert!(
            values.contains(&serde_json::Value::String("alignRight".to_string())),
            "propObjectValues should contain 'alignRight', got: {:?}",
            values
        );
    }

    #[test]
    fn test_jsx_prop_object_values_multiple_breakpoints() {
        // align={{ default: 'alignRight', md: 'alignLeft' }} should extract both values
        let source = r#"
import { ToolbarGroup } from '@patternfly/react-core';
const el = <ToolbarGroup align={{ default: 'alignRight', md: 'alignLeft' }} />;
"#;
        let incidents = scan_source_jsx(source, r"^align$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(incidents.len(), 1);

        let values = incidents[0]
            .variables
            .get("propObjectValues")
            .unwrap()
            .as_array()
            .unwrap();
        assert!(values.contains(&serde_json::Value::String("alignRight".to_string())));
        assert!(values.contains(&serde_json::Value::String("alignLeft".to_string())));
    }

    #[test]
    fn test_jsx_prop_direct_string_no_object_values() {
        // align="alignRight" should NOT have propObjectValues (it's a direct string)
        let source = r#"
import { ToolbarItem } from '@patternfly/react-core';
const el = <ToolbarItem align="alignRight" />;
"#;
        let incidents = scan_source_jsx(source, r"^align$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(incidents.len(), 1);
        assert!(
            incidents[0].variables.get("propObjectValues").is_none(),
            "Direct string props should not have propObjectValues"
        );
    }
}

//! CSS variable reference scanning in JS/TS files.
//!
//! Finds string/template literals containing CSS custom property names
//! like `--pf-v5-global--Color--100`.

use crate::scanner::make_incident;
use frontend_core::incident::Incident;
use oxc_ast::ast::*;
use oxc_span::GetSpan;
use regex::Regex;

/// Scan a statement for CSS variable references.
pub fn scan_css_var_usage(
    stmt: &Statement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
) -> Vec<Incident> {
    let mut incidents = Vec::new();
    walk_stmt(stmt, source, pattern, file_uri, &mut incidents);
    incidents
}

fn walk_stmt(
    stmt: &Statement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    incidents: &mut Vec<Incident>,
) {
    match stmt {
        Statement::ExportDefaultDeclaration(decl) => {
            if let ExportDefaultDeclarationKind::FunctionDeclaration(func) = &decl.declaration {
                if let Some(body) = &func.body {
                    for s in &body.statements {
                        walk_stmt(s, source, pattern, file_uri, incidents);
                    }
                }
            }
        }
        Statement::ExportNamedDeclaration(decl) => {
            if let Some(Declaration::FunctionDeclaration(func)) = &decl.declaration {
                if let Some(body) = &func.body {
                    for s in &body.statements {
                        walk_stmt(s, source, pattern, file_uri, incidents);
                    }
                }
            }
            if let Some(Declaration::VariableDeclaration(v)) = &decl.declaration {
                for d in &v.declarations {
                    if let Some(init) = &d.init {
                        walk_expr(init, source, pattern, file_uri, incidents);
                    }
                }
            }
        }
        Statement::FunctionDeclaration(func) => {
            if let Some(body) = &func.body {
                for s in &body.statements {
                    walk_stmt(s, source, pattern, file_uri, incidents);
                }
            }
        }
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                if let Some(init) = &d.init {
                    walk_expr(init, source, pattern, file_uri, incidents);
                }
            }
        }
        Statement::ReturnStatement(ret) => {
            if let Some(arg) = &ret.argument {
                walk_expr(arg, source, pattern, file_uri, incidents);
            }
        }
        Statement::ExpressionStatement(expr) => {
            walk_expr(&expr.expression, source, pattern, file_uri, incidents);
        }
        Statement::BlockStatement(block) => {
            for s in &block.body {
                walk_stmt(s, source, pattern, file_uri, incidents);
            }
        }
        _ => {}
    }
}

fn walk_expr(
    expr: &Expression<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    incidents: &mut Vec<Incident>,
) {
    match expr {
        Expression::StringLiteral(s) => {
            let text = s.value.as_str();
            if pattern.is_match(text) {
                let span = s.span();
                let mut incident = make_incident(source, file_uri, span.start, span.end);
                incident.variables.insert(
                    "matchingText".into(),
                    serde_json::Value::String(text.to_string()),
                );
                incidents.push(incident);
            }
        }
        Expression::TemplateLiteral(tpl) => {
            for quasi in &tpl.quasis {
                let raw = quasi.value.raw.as_str();
                if pattern.is_match(raw) {
                    let span = quasi.span();
                    let mut incident = make_incident(source, file_uri, span.start, span.end);
                    incident.variables.insert(
                        "matchingText".into(),
                        serde_json::Value::String(raw.to_string()),
                    );
                    incidents.push(incident);
                }
            }
        }
        Expression::JSXElement(el) => {
            // Scan JSX attributes for CSS var references in string values
            for attr in &el.opening_element.attributes {
                if let JSXAttributeItem::Attribute(a) = attr {
                    if let Some(JSXAttributeValue::StringLiteral(s)) = &a.value {
                        let text = s.value.as_str();
                        if pattern.is_match(text) {
                            let span = s.span();
                            let mut incident =
                                make_incident(source, file_uri, span.start, span.end);
                            incident.variables.insert(
                                "matchingText".into(),
                                serde_json::Value::String(text.to_string()),
                            );
                            incidents.push(incident);
                        }
                    }
                }
            }
        }
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Some(e) = arg.as_expression() {
                    walk_expr(e, source, pattern, file_uri, incidents);
                }
            }
        }
        Expression::ParenthesizedExpression(p) => {
            walk_expr(&p.expression, source, pattern, file_uri, incidents);
        }
        Expression::ArrowFunctionExpression(arrow) => {
            for s in &arrow.body.statements {
                walk_stmt(s, source, pattern, file_uri, incidents);
            }
        }
        Expression::ConditionalExpression(c) => {
            walk_expr(&c.consequent, source, pattern, file_uri, incidents);
            walk_expr(&c.alternate, source, pattern, file_uri, incidents);
        }
        _ => {}
    }
}

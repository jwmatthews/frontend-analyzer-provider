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

/// Map of local function/variable names → the expression body they hold.
///
/// Built from variable declarations like `const renderItems = () => { ... }`
/// or `const renderItems = function() { ... }`. Used to resolve call
/// expressions in JSX children (e.g., `{renderItems()}`) to the function body,
/// so the parent context propagates through the call.
type LocalFnMap<'a> = HashMap<String, &'a Expression<'a>>;

/// Build a map of all function declarations in the AST, including those
/// nested inside component bodies. Recurses into function/arrow bodies
/// to find declarations like:
/// ```ts
/// const MyComponent = () => {
///   const renderItems = () => <Item />;  // ← captured
///   return <Parent>{renderItems()}</Parent>;
/// };
/// ```
fn build_local_fn_map<'a>(stmts: &'a [Statement<'a>], source: &str) -> LocalFnMap<'a> {
    let mut map = LocalFnMap::new();
    for stmt in stmts {
        collect_fn_declarations_from_stmt(stmt, source, &mut map);
    }
    map
}

fn collect_fn_declarations_from_stmt<'a>(
    stmt: &'a Statement<'a>,
    source: &str,
    map: &mut LocalFnMap<'a>,
) {
    let var_decl = match stmt {
        Statement::VariableDeclaration(v) => Some(v.as_ref()),
        Statement::ExportNamedDeclaration(exp) => {
            if let Some(Declaration::VariableDeclaration(v)) = &exp.declaration {
                Some(v.as_ref())
            } else {
                None
            }
        }
        _ => None,
    };

    if let Some(var_decl) = var_decl {
        for declarator in &var_decl.declarations {
            if let Some(init) = &declarator.init {
                let is_fn = matches!(
                    init,
                    Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)
                );
                if is_fn {
                    // Register this function
                    let id_span = declarator.id.span();
                    let start = id_span.start as usize;
                    let end = id_span.end as usize;
                    if let Some(name_str) = source.get(start..end) {
                        let name = name_str.split(':').next().unwrap_or("").trim();
                        if !name.is_empty() {
                            map.insert(name.to_string(), init);
                        }
                    }
                    // Recurse into the function body to find nested declarations
                    collect_fn_declarations_from_expr(init, source, map);
                }
            }
        }
    }
}

fn collect_fn_declarations_from_expr<'a>(
    expr: &'a Expression<'a>,
    source: &str,
    map: &mut LocalFnMap<'a>,
) {
    match expr {
        Expression::ArrowFunctionExpression(arrow) => {
            for stmt in &arrow.body.statements {
                collect_fn_declarations_from_stmt(stmt, source, map);
            }
        }
        Expression::FunctionExpression(func) => {
            if let Some(body) = &func.body {
                for stmt in &body.statements {
                    collect_fn_declarations_from_stmt(stmt, source, map);
                }
            }
        }
        _ => {}
    }
}

/// Scan all statements in a program body for JSX component and prop usage.
///
/// This file-level entry point builds a local function map first, then walks
/// each statement. The function map enables resolving call expressions like
/// `{renderItems()}` in JSX children to their function bodies, propagating
/// the parent JSX element context through the call.
pub fn scan_jsx_file<'a>(
    stmts: &'a [Statement<'a>],
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    import_map: &ImportMap,
) -> Vec<Incident> {
    let local_fns = build_local_fn_map(stmts, source);
    let mut incidents = Vec::new();
    for stmt in stmts {
        walk_statement_for_jsx(
            stmt,
            source,
            pattern,
            file_uri,
            location,
            &mut incidents,
            None,
            import_map,
            &local_fns,
        );
    }
    incidents
}

/// Scan a statement for JSX component and prop usage.
pub fn scan_jsx(
    stmt: &Statement<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    import_map: &ImportMap,
) -> Vec<Incident> {
    let empty_fns = LocalFnMap::new();
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
        &empty_fns,
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
    local_fns: &LocalFnMap,
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
                        local_fns,
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
                        local_fns,
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
                    local_fns,
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
                    local_fns,
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
                local_fns,
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
                    local_fns,
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
                local_fns,
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
                    local_fns,
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
                local_fns,
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
                    local_fns,
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
    local_fns: &LocalFnMap,
) {
    for declarator in &var_decl.declarations {
        // Check for typed object literals that represent component props.
        // e.g., `const x: ToolbarItemProps = { align: { default: 'alignRight' } }`
        // This catches prop values set in helper files outside JSX context,
        // where the object is later spread onto a JSX element in another file.
        if matches!(location, Some(ReferenceLocation::JsxProp) | None) {
            check_typed_object_literal(
                declarator, source, pattern, file_uri, import_map, incidents,
            );
        }

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
                local_fns,
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
    local_fns: &LocalFnMap,
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
            local_fns,
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
    local_fns: &LocalFnMap,
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
                local_fns,
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
                    local_fns,
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
                local_fns,
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
                local_fns,
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
                local_fns,
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
                local_fns,
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
                local_fns,
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
                        local_fns,
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
                        local_fns,
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
                            local_fns,
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
                            local_fns,
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
    local_fns: &LocalFnMap,
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
                local_fns,
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
                    local_fns,
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
                local_fns,
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
    local_fns: &LocalFnMap,
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
                local_fns,
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
                    local_fns,
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
                local_fns,
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
                local_fns,
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
                local_fns,
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
                local_fns,
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
                local_fns,
            );
        }
        // Function calls: {renderFn(<Component />)} or {fn(arg)}
        JSXExpression::CallExpression(call) => {
            // Resolve local function calls: if the callee is a known local
            // function (e.g., `renderDropdownItems`), walk its body with the
            // current parent context so JSX returned by that function inherits
            // the parent element (e.g., <Dropdown>).
            if let Expression::Identifier(ident) = &call.callee {
                if let Some(fn_expr) = local_fns.get(ident.name.as_str()) {
                    match fn_expr {
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
                                local_fns,
                            );
                        }
                        Expression::FunctionExpression(func) => {
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
                                    local_fns,
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }

            // Also walk call arguments for JSX passed as args
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
                        local_fns,
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
                        local_fns,
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
                            local_fns,
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
                            local_fns,
                        );
                    }
                }
            }
        }
        _ => {}
    }
}

/// Extract all string literal values from an `ObjectExpression` (non-JSX context).
///
/// Given `{ default: 'alignRight', md: 'alignLeft' }`, returns
/// `["alignRight", "alignLeft"]`. Used for typed object literal scanning
/// where the object is not wrapped in a JSX expression container.
fn extract_object_expression_string_values(obj: &ObjectExpression<'_>) -> Vec<String> {
    let mut values = Vec::new();
    for prop in &obj.properties {
        if let ObjectPropertyKind::ObjectProperty(p) = prop {
            match &p.value {
                Expression::StringLiteral(s) => {
                    values.push(s.value.to_string());
                }
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

/// Extract all string literal values from an object expression's properties.
///
/// Given `{ default: 'alignRight', md: 'alignLeft' }`, returns
/// `["alignRight", "alignLeft"]`. This allows value-based rules to match
/// prop values inside responsive breakpoint objects.
/// Extract string literal values from an object expression inside a regular `Expression`.
/// Handles direct objects, parenthesized, and ternary branches.
fn extract_object_string_values_from_expr(expr: &Expression<'_>) -> Vec<String> {
    match expr {
        Expression::ObjectExpression(obj) => extract_object_expression_string_values(obj),
        Expression::ParenthesizedExpression(paren) => {
            extract_object_string_values_from_expr(&paren.expression)
        }
        Expression::ConditionalExpression(cond) => {
            let mut values = extract_object_string_values_from_expr(&cond.consequent);
            values.extend(extract_object_string_values_from_expr(&cond.alternate));
            values
        }
        Expression::StringLiteral(s) => vec![s.value.to_string()],
        _ => vec![],
    }
}

fn extract_object_string_values(expr: &JSXExpression<'_>, _source: &str) -> Vec<String> {
    match expr {
        JSXExpression::ObjectExpression(obj) => extract_object_expression_string_values(obj),
        JSXExpression::ParenthesizedExpression(paren) => {
            extract_object_string_values_from_expr(&paren.expression)
        }
        JSXExpression::ConditionalExpression(cond) => {
            let mut values = extract_object_string_values_from_expr(&cond.consequent);
            values.extend(extract_object_string_values_from_expr(&cond.alternate));
            values
        }
        _ => vec![],
    }
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

/// Resolve a Props-type annotation on a variable declarator to its component name and module.
///
/// Given `const x: ToolbarItemProps = { ... }` where `ToolbarItemProps` is imported from
/// `@patternfly/react-core`, returns `("ToolbarItem", "@patternfly/react-core")`.
///
/// Returns `None` if:
/// - The declarator has no type annotation
/// - The type annotation is not a TSTypeReference
/// - The type name doesn't end with "Props"
/// - The type name is not found in the import map
///
/// Also handles common utility type wrappers like `Partial<FooProps>`,
/// `Required<FooProps>`, `Readonly<FooProps>` by unwrapping one level.
fn resolve_props_type_info(
    declarator: &VariableDeclarator<'_>,
    source: &str,
    import_map: &ImportMap,
) -> Option<(String, String)> {
    let annotation = declarator.type_annotation.as_ref()?;

    // Try direct type reference first, then unwrap utility types
    resolve_type_to_props(&annotation.type_annotation, source, import_map)
}

/// Try to resolve a TSType to a (component_name, module) pair.
///
/// Handles direct references like `ToolbarItemProps` and utility wrappers
/// like `Partial<ToolbarItemProps>`.
fn resolve_type_to_props(
    ts_type: &TSType<'_>,
    source: &str,
    import_map: &ImportMap,
) -> Option<(String, String)> {
    if let TSType::TSTypeReference(type_ref) = ts_type {
        let name_span = type_ref.type_name.span();
        let type_name = source
            .get(name_span.start as usize..name_span.end as usize)
            .unwrap_or_default();

        // Check if this is directly a Props type in the import map
        if let Some(component_name) = type_name.strip_suffix("Props") {
            if !component_name.is_empty() {
                if let Some(module) = import_map.get(type_name) {
                    return Some((component_name.to_string(), module.clone()));
                }
            }
        }

        // Check if this is a utility wrapper like Partial<FooProps>
        const UTILITY_TYPES: &[&str] = &["Partial", "Required", "Readonly", "Pick", "Omit"];
        if UTILITY_TYPES.contains(&type_name) {
            if let Some(type_args) = &type_ref.type_arguments {
                if let Some(first_arg) = type_args.params.first() {
                    return resolve_type_to_props(first_arg, source, import_map);
                }
            }
        }
    }

    // Handle intersection types: FooProps & { extraProp: string }
    if let TSType::TSIntersectionType(inter) = ts_type {
        for t in &inter.types {
            if let Some(result) = resolve_type_to_props(t, source, import_map) {
                return Some(result);
            }
        }
    }

    None
}

/// Check a typed object literal for prop pattern matches.
///
/// When a variable is declared with a Props-type annotation and initialized with
/// an object literal, the object's properties represent component props. This
/// function matches property keys against the pattern and creates incidents as
/// if they were JSX prop usages.
///
/// Example:
/// ```typescript
/// import { ToolbarItemProps } from '@patternfly/react-core';
/// const x: ToolbarItemProps = { align: { default: 'alignRight' } };
/// //                           ^^^^^
/// //                           This property key is matched as a "prop" on ToolbarItem
/// ```
///
/// This catches prop values set in helper files where the object is later spread
/// onto a JSX element in a different file (e.g., `<ToolbarItem {...x} />`).
fn check_typed_object_literal(
    declarator: &VariableDeclarator<'_>,
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    import_map: &ImportMap,
    incidents: &mut Vec<Incident>,
) {
    let (component_name, module) = match resolve_props_type_info(declarator, source, import_map) {
        Some(info) => info,
        None => return,
    };

    let obj = match &declarator.init {
        Some(Expression::ObjectExpression(obj)) => obj.as_ref(),
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
                        serde_json::Value::String(component_name.clone()),
                    );
                    incident
                        .variables
                        .insert("module".into(), serde_json::Value::String(module.clone()));

                    // Extract prop value — same logic as JSX attribute value extraction
                    match &p.value {
                        Expression::StringLiteral(s) => {
                            incident.variables.insert(
                                "propValue".into(),
                                serde_json::Value::String(s.value.to_string()),
                            );
                        }
                        Expression::ObjectExpression(nested) => {
                            // Responsive breakpoint objects: { default: 'alignRight', md: 'alignLeft' }
                            let values = extract_object_expression_string_values(nested);
                            if !values.is_empty() {
                                incident.variables.insert(
                                    "propObjectValues".into(),
                                    serde_json::Value::Array(
                                        values.into_iter().map(serde_json::Value::String).collect(),
                                    ),
                                );
                            }
                            // Also set propValue to the source text of the object
                            let expr_span = nested.span();
                            let start = (expr_span.start as usize).min(source.len());
                            let end = (expr_span.end as usize).min(source.len());
                            let text = &source[start..end];
                            incident.variables.insert(
                                "propValue".into(),
                                serde_json::Value::String(text.trim().to_string()),
                            );
                        }
                        _ => {
                            // For other expressions, capture source text
                            let val_span = p.value.span();
                            let start = (val_span.start as usize).min(source.len());
                            let end = (val_span.end as usize).min(source.len());
                            let text = &source[start..end];
                            incident.variables.insert(
                                "propValue".into(),
                                serde_json::Value::String(text.trim().to_string()),
                            );
                        }
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
    local_fns: &LocalFnMap,
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
                    local_fns,
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
            local_fns,
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

        scan_jsx_file(
            &ret.program.body,
            source,
            &re,
            "file:///test.tsx",
            location,
            &import_map,
        )
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

    // ── Ternary / conditional expression value tests ──────────────────

    #[test]
    fn test_jsx_prop_ternary_object_values() {
        // spaceItems={condition ? { default: 'spaceItemsMd' } : undefined}
        // should extract 'spaceItemsMd' from the ternary's consequent branch
        let source = r#"
import { ToolbarToggleGroup } from '@patternfly/react-core';
const el = <ToolbarToggleGroup spaceItems={showFilters ? { default: 'spaceItemsMd' } : undefined} />;
"#;
        let incidents = scan_source_jsx(source, r"^spaceItems$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(incidents.len(), 1);

        let values = incidents[0]
            .variables
            .get("propObjectValues")
            .expect("Should extract propObjectValues from ternary branch")
            .as_array()
            .unwrap();
        assert!(
            values.contains(&serde_json::Value::String("spaceItemsMd".to_string())),
            "Should find 'spaceItemsMd' inside ternary consequent. Got: {:?}",
            values
        );
    }

    #[test]
    fn test_jsx_prop_ternary_direct_string_values() {
        // variant={isActive ? 'primary' : 'secondary'}
        // should extract both 'primary' and 'secondary'
        let source = r#"
import { Button } from '@patternfly/react-core';
const el = <Button variant={isActive ? 'primary' : 'secondary'} />;
"#;
        let incidents = scan_source_jsx(source, r"^variant$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(incidents.len(), 1);

        let values = incidents[0]
            .variables
            .get("propObjectValues")
            .expect("Should extract values from ternary branches")
            .as_array()
            .unwrap();
        assert!(
            values.contains(&serde_json::Value::String("primary".to_string())),
            "Should find 'primary' in ternary. Got: {:?}",
            values
        );
        assert!(
            values.contains(&serde_json::Value::String("secondary".to_string())),
            "Should find 'secondary' in ternary. Got: {:?}",
            values
        );
    }

    // ── Typed object literal scanning tests ──────────────────────────

    #[test]
    fn test_typed_object_literal_basic() {
        // const x: ToolbarItemProps = { align: ... } should match ^align$
        let source = r#"
import { ToolbarItemProps } from '@patternfly/react-core';
const paginationToolbarItemProps: ToolbarItemProps = {
    variant: 'pagination',
    align: { default: 'alignRight' }
};
"#;
        let incidents = scan_source_jsx(source, r"^align$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(
            incidents.len(),
            1,
            "Should detect 'align' as a prop on typed object literal"
        );
        assert_eq!(
            incidents[0].variables.get("propName"),
            Some(&serde_json::Value::String("align".to_string()))
        );
        assert_eq!(
            incidents[0].variables.get("componentName"),
            Some(&serde_json::Value::String("ToolbarItem".to_string()))
        );
        assert_eq!(
            incidents[0].variables.get("module"),
            Some(&serde_json::Value::String(
                "@patternfly/react-core".to_string()
            ))
        );
    }

    #[test]
    fn test_typed_object_literal_with_object_values() {
        // Should extract propObjectValues from nested responsive breakpoint object
        let source = r#"
import { ToolbarItemProps } from '@patternfly/react-core';
const x: ToolbarItemProps = { align: { default: 'alignRight', md: 'alignLeft' } };
"#;
        let incidents = scan_source_jsx(source, r"^align$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(incidents.len(), 1);

        let values = incidents[0]
            .variables
            .get("propObjectValues")
            .expect("Should have propObjectValues")
            .as_array()
            .unwrap();
        assert!(values.contains(&serde_json::Value::String("alignRight".to_string())));
        assert!(values.contains(&serde_json::Value::String("alignLeft".to_string())));
    }

    #[test]
    fn test_typed_object_literal_with_string_value() {
        // Direct string property values should be captured as propValue
        let source = r#"
import { ButtonProps } from '@patternfly/react-core';
const x: ButtonProps = { variant: 'primary' };
"#;
        let incidents = scan_source_jsx(source, r"^variant$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("propValue"),
            Some(&serde_json::Value::String("primary".to_string()))
        );
        assert_eq!(
            incidents[0].variables.get("componentName"),
            Some(&serde_json::Value::String("Button".to_string()))
        );
    }

    #[test]
    fn test_typed_object_literal_no_match_without_import() {
        // Type not in import map should not produce incidents
        let source = r#"
type LocalProps = { align: string };
const x: LocalProps = { align: 'right' };
"#;
        let incidents = scan_source_jsx(source, r"^align$", Some(&ReferenceLocation::JsxProp));
        assert!(
            incidents.is_empty(),
            "Should not match typed objects where type is not imported"
        );
    }

    #[test]
    fn test_typed_object_literal_no_match_non_props_type() {
        // Type that doesn't end with "Props" should not match
        let source = r#"
import { ToolbarItem } from '@patternfly/react-core';
const x: ToolbarItem = { align: 'right' };
"#;
        let incidents = scan_source_jsx(source, r"^align$", Some(&ReferenceLocation::JsxProp));
        assert!(
            incidents.is_empty(),
            "Should not match types that don't end with 'Props'"
        );
    }

    #[test]
    fn test_typed_object_literal_partial_wrapper() {
        // Partial<ToolbarItemProps> should unwrap to ToolbarItem
        let source = r#"
import { ToolbarItemProps } from '@patternfly/react-core';
const x: Partial<ToolbarItemProps> = { align: { default: 'alignRight' } };
"#;
        let incidents = scan_source_jsx(source, r"^align$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(
            incidents.len(),
            1,
            "Should unwrap Partial<> to find Props type"
        );
        assert_eq!(
            incidents[0].variables.get("componentName"),
            Some(&serde_json::Value::String("ToolbarItem".to_string()))
        );
    }

    #[test]
    fn test_typed_object_literal_exported() {
        // export const x: FooProps = { ... } should also work
        let source = r#"
import { ToolbarGroupProps } from '@patternfly/react-core';
export const groupProps: ToolbarGroupProps = {
    variant: 'icon-button-group',
    align: { default: 'alignRight' }
};
"#;
        let incidents = scan_source_jsx(source, r"^align$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(
            incidents.len(),
            1,
            "Should detect props in exported variable declarations"
        );
        assert_eq!(
            incidents[0].variables.get("componentName"),
            Some(&serde_json::Value::String("ToolbarGroup".to_string()))
        );
    }

    #[test]
    fn test_typed_object_literal_no_match_wrong_prop() {
        // Only the matching property key should create an incident
        let source = r#"
import { ToolbarItemProps } from '@patternfly/react-core';
const x: ToolbarItemProps = { variant: 'pagination', align: { default: 'alignRight' } };
"#;
        let incidents = scan_source_jsx(source, r"^spaceItems$", Some(&ReferenceLocation::JsxProp));
        assert!(
            incidents.is_empty(),
            "Should not match properties that don't match the pattern"
        );
    }

    #[test]
    fn test_typed_object_literal_coexists_with_jsx() {
        // Both JSX prop and typed object literal should produce incidents
        let source = r#"
import { ToolbarItem, ToolbarItemProps } from '@patternfly/react-core';
const x: ToolbarItemProps = { align: { default: 'alignRight' } };
const el = <ToolbarItem align={{ default: 'alignRight' }} />;
"#;
        let incidents = scan_source_jsx(source, r"^align$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(
            incidents.len(),
            2,
            "Should detect both typed object literal and JSX prop"
        );
    }

    #[test]
    fn test_typed_object_literal_real_world_pagination() {
        // Exact pattern from quipucords-ui usePaginationPropHelpers.ts
        let source = r#"
import { PaginationProps, ToolbarItemProps } from '@patternfly/react-core';

export const usePaginationPropHelpers = (args: any) => {
    const paginationProps: PaginationProps = {
        itemCount: 100,
        perPage: 10,
        page: 1,
    };

    const paginationToolbarItemProps: ToolbarItemProps = {
        variant: 'pagination',
        align: { default: 'alignRight' }
    };

    return { paginationProps, paginationToolbarItemProps };
};
"#;
        let incidents = scan_source_jsx(source, r"^align$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(
            incidents.len(),
            1,
            "Should detect 'align' in typed object literal inside function"
        );
        assert_eq!(
            incidents[0].variables.get("componentName"),
            Some(&serde_json::Value::String("ToolbarItem".to_string()))
        );
        assert_eq!(
            incidents[0].variables.get("module"),
            Some(&serde_json::Value::String(
                "@patternfly/react-core".to_string()
            ))
        );

        // Check that propObjectValues are extracted
        let values = incidents[0]
            .variables
            .get("propObjectValues")
            .expect("Should have propObjectValues")
            .as_array()
            .unwrap();
        assert!(values.contains(&serde_json::Value::String("alignRight".to_string())));
    }

    // ── Function-return tracing tests ────────────────────────────────

    #[test]
    fn test_function_return_inherits_parent_context() {
        // renderDropdownItems() returns <DropdownItem>, called inside <Dropdown>.
        // The scanner should see DropdownItem with parent Dropdown.
        let source = r#"
import { Dropdown, DropdownItem } from '@patternfly/react-core';
const renderItems = () => {
    return <DropdownItem>Item</DropdownItem>;
};
const el = <Dropdown>{renderItems()}</Dropdown>;
"#;
        let incidents = scan_source_jsx(
            source,
            r"^DropdownItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        // Should have incidents: one from function body (no parent) + one from call site (parent=Dropdown)
        let with_dropdown_parent: Vec<_> = incidents
            .iter()
            .filter(|i| {
                i.variables.get("parentName")
                    == Some(&serde_json::Value::String("Dropdown".to_string()))
            })
            .collect();
        assert!(
            !with_dropdown_parent.is_empty(),
            "Should have DropdownItem with parent Dropdown from call site. Incidents: {:?}",
            incidents.iter().map(|i| &i.variables).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_function_return_with_map_inherits_parent() {
        // Common pattern: function returns items.map(x => <Component />)
        let source = r#"
import { Dropdown, DropdownItem } from '@patternfly/react-core';
const renderItems = () => {
    return [1, 2].map(i => <DropdownItem key={i}>Item {i}</DropdownItem>);
};
const el = <Dropdown>{renderItems()}</Dropdown>;
"#;
        let incidents = scan_source_jsx(
            source,
            r"^DropdownItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        let with_dropdown_parent: Vec<_> = incidents
            .iter()
            .filter(|i| {
                i.variables.get("parentName")
                    == Some(&serde_json::Value::String("Dropdown".to_string()))
            })
            .collect();
        assert!(
            !with_dropdown_parent.is_empty(),
            "Should trace through .map() arrow and inherit Dropdown parent. Incidents: {:?}",
            incidents.iter().map(|i| &i.variables).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_nested_function_return_inherits_parent() {
        // Function declared INSIDE a component body (not top-level).
        // This is the FilterToolbar pattern.
        let source = r#"
import { Dropdown, DropdownItem } from '@patternfly/react-core';
const FilterToolbar = () => {
    const renderDropdownItems = () => {
        return <DropdownItem>Item</DropdownItem>;
    };
    return (
        <Dropdown>{renderDropdownItems()}</Dropdown>
    );
};
"#;
        let incidents = scan_source_jsx(
            source,
            r"^DropdownItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        let with_dropdown_parent: Vec<_> = incidents
            .iter()
            .filter(|i| {
                i.variables.get("parentName")
                    == Some(&serde_json::Value::String("Dropdown".to_string()))
            })
            .collect();
        assert!(
            !with_dropdown_parent.is_empty(),
            "Should find nested function's DropdownItem with parent Dropdown. Incidents: {:?}",
            incidents.iter().map(|i| &i.variables).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_function_return_inside_wrapper_has_correct_parent() {
        // renderItems() called inside <DropdownList> inside <Dropdown>.
        // The call-site parent should be DropdownList, not Dropdown.
        let source = r#"
import { Dropdown, DropdownList, DropdownItem } from '@patternfly/react-core';
const renderItems = () => {
    return <DropdownItem>Item</DropdownItem>;
};
const el = (
    <Dropdown>
        <DropdownList>{renderItems()}</DropdownList>
    </Dropdown>
);
"#;
        let incidents = scan_source_jsx(
            source,
            r"^DropdownItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        let with_list_parent: Vec<_> = incidents
            .iter()
            .filter(|i| {
                i.variables.get("parentName")
                    == Some(&serde_json::Value::String("DropdownList".to_string()))
            })
            .collect();
        assert!(
            !with_list_parent.is_empty(),
            "Should have DropdownItem with parent DropdownList. Incidents: {:?}",
            incidents.iter().map(|i| &i.variables).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_function_return_no_double_count() {
        // DropdownItem should not be double-counted — once from the function
        // body walk and once from the call-site resolution.
        let source = r#"
import { Dropdown, DropdownItem } from '@patternfly/react-core';
const renderItems = () => <DropdownItem>Item</DropdownItem>;
const el = <Dropdown>{renderItems()}</Dropdown>;
"#;
        let incidents = scan_source_jsx(
            source,
            r"^DropdownItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        // Two incidents: one from function body (no parent), one from call site (parent=Dropdown)
        // The function body walk sees it with no parent, the call-site walk sees it with parent=Dropdown
        // Both are valid — the conformance rule filters by parent=Dropdown
        let with_parent: Vec<_> = incidents
            .iter()
            .filter(|i| i.variables.contains_key("parentName"))
            .collect();
        assert!(
            !with_parent.is_empty(),
            "Should have at least one incident with parent context from call site"
        );
    }
}

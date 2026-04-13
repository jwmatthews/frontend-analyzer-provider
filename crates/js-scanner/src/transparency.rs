//! Component transparency analysis.
//!
//! Determines whether a React component is a "transparent wrapper" — one that
//! accepts `children` and renders them in at least one code path without
//! wrapping them in a new library component.
//!
//! When the JSX scanner encounters a transparent wrapper in the parent chain,
//! it collapses it out: children of the wrapper inherit the parent from above,
//! rather than seeing the wrapper as their parent. This makes conformance rules
//! that check parent nesting (e.g., "Tbody must be inside Table") work correctly
//! even when a non-library wrapper sits between library components.
//!
//! # Example
//!
//! ```tsx
//! // ConditionalTableBody.tsx — transparent because line 54 renders {children}
//! export const ConditionalTableBody = ({ children, isLoading, ... }) => (
//!   <React.Fragment>
//!     {isLoading ? <Tbody>...</Tbody> : children}
//!   </React.Fragment>
//! );
//! ```
//!
//! When scanning a consumer file:
//! ```tsx
//! <Table>
//!   <ConditionalTableBody>
//!     <Tbody>...</Tbody>        // parentName = "Table" (not "ConditionalTableBody")
//!   </ConditionalTableBody>
//! </Table>
//! ```

use anyhow::Result;
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_resolver::Resolver;
use oxc_span::SourceType;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Info about a transparent component: what PF component (if any) wraps `{children}`.
///
/// - `None` means pure passthrough (e.g., `<>{children}</>` or `<div>{children}</div>`)
/// - `Some("Table")` means children are wrapped in `<Table>` (e.g., `<Table>{props.children}</Table>`)
pub type WrapperInfo = Option<String>;

/// Cache of file path → map of transparent component name → wrapper info.
pub type TransparencyCache = HashMap<PathBuf, HashMap<String, WrapperInfo>>;

/// Maximum depth for following re-export chains across files.
/// Prevents stack overflow on deeply chained barrel files.
const MAX_REEXPORT_DEPTH: usize = 20;

/// Analyze a source file and return the set of exported component names that
/// are transparent (pass `{children}` through in at least one code path).
///
/// Returns an empty set if the file can't be parsed or contains no transparent
/// components.
pub fn analyze_file_transparency(file_path: &Path) -> Result<HashMap<String, WrapperInfo>> {
    let source = std::fs::read_to_string(file_path)?;
    analyze_source_transparency(&source)
}

/// Analyze a source file and return the set of exported component names that
/// are transparent, following barrel re-exports (`export * from './X'`).
///
/// Uses the provided `oxc_resolver::Resolver` to resolve re-export specifiers.
/// The `cache` prevents redundant parsing when multiple files re-export from
/// the same source.
///
/// This is the preferred entry point when an `oxc_resolver` is available,
/// as it handles barrel files (index.ts) that re-export transparent components
/// from other files.
pub fn analyze_file_transparency_with_resolver(
    file_path: &Path,
    resolver: &Resolver,
    cache: &mut TransparencyCache,
    depth: usize,
) -> Result<HashMap<String, WrapperInfo>> {
    // Check cache first
    if let Some(cached) = cache.get(file_path) {
        return Ok(cached.clone());
    }

    // Guard against excessively deep re-export chains
    if depth > MAX_REEXPORT_DEPTH {
        tracing::warn!(
            "re-export chain depth exceeded {} at {}",
            MAX_REEXPORT_DEPTH,
            file_path.display(),
        );
        return Ok(HashMap::new());
    }

    // Insert an empty map first to prevent infinite recursion on circular re-exports
    cache.insert(file_path.to_path_buf(), HashMap::new());

    let source = std::fs::read_to_string(file_path)?;
    let mut transparent = analyze_source_transparency(&source)?;

    // Now follow re-exports: `export * from './X'` and `export { Foo } from './X'`
    let reexport_sources = collect_reexport_sources(&source);
    for (specifier, names) in &reexport_sources {
        if let Some(resolved) =
            crate::resolve::resolve_import_with_resolver(resolver, file_path, specifier)
        {
            // Skip node_modules
            if crate::resolve::is_node_modules_path(&resolved) {
                continue;
            }

            // Recursively analyze the re-exported file
            let reexported =
                analyze_file_transparency_with_resolver(&resolved, resolver, cache, depth + 1)
                    .unwrap_or_default();

            match names {
                ReexportKind::All => {
                    // `export * from './X'` — merge all transparent names
                    transparent.extend(reexported);
                }
                ReexportKind::Named(named) => {
                    // `export { Foo, Bar } from './X'` — only include if re-exported
                    // name is transparent in the source
                    for name in named {
                        if let Some(wrapper_info) = reexported.get(name) {
                            transparent.insert(name.clone(), wrapper_info.clone());
                        }
                    }
                }
            }
        }
    }

    // Update cache with final result
    cache.insert(file_path.to_path_buf(), transparent.clone());
    Ok(transparent)
}

/// Describes what names are re-exported from a module.
enum ReexportKind {
    /// `export * from '...'` — all exports
    All,
    /// `export { Foo, Bar } from '...'` — specific names
    Named(Vec<String>),
}

/// Collect re-export sources from a source file.
///
/// Returns a list of (module_specifier, kind) for each re-export statement.
fn collect_reexport_sources(source: &str) -> Vec<(String, ReexportKind)> {
    let allocator = Allocator::default();
    let source_type = SourceType::tsx();
    let ret = Parser::new(&allocator, source, source_type).parse();

    if ret.panicked {
        return Vec::new();
    }

    let mut reexports = Vec::new();

    for stmt in &ret.program.body {
        match stmt {
            // export * from './X'
            Statement::ExportAllDeclaration(decl) => {
                let specifier = decl.source.value.as_str().to_string();
                reexports.push((specifier, ReexportKind::All));
            }
            // export { Foo, Bar } from './X'
            Statement::ExportNamedDeclaration(decl) => {
                if let Some(ref source) = decl.source {
                    let specifier = source.value.as_str().to_string();
                    let names: Vec<String> = decl
                        .specifiers
                        .iter()
                        .map(|s| {
                            // Use the exported name (what consumers see)
                            s.exported.name().to_string()
                        })
                        .collect();
                    if !names.is_empty() {
                        reexports.push((specifier, ReexportKind::Named(names)));
                    }
                }
            }
            _ => {}
        }
    }

    reexports
}

/// Analyze source code string and return the set of component names that are
/// transparent, along with what component (if any) wraps `{children}`.
/// Factored out from `analyze_file_transparency` for testing.
pub fn analyze_source_transparency(source: &str) -> Result<HashMap<String, WrapperInfo>> {
    let allocator = Allocator::default();
    let source_type = SourceType::tsx();
    let ret = Parser::new(&allocator, source, source_type).parse();

    if ret.panicked {
        return Ok(HashMap::new());
    }

    let mut transparent = HashMap::new();

    for stmt in &ret.program.body {
        match stmt {
            // export const Foo = ({ children }) => ...
            Statement::ExportNamedDeclaration(decl) => {
                if let Some(Declaration::VariableDeclaration(var_decl)) = &decl.declaration {
                    for declarator in &var_decl.declarations {
                        if let Some(name) = extract_binding_name(&declarator.id) {
                            if let Some(init) = &declarator.init {
                                if is_transparent_expression(init, source) {
                                    let wrapper = find_children_wrapper_in_expression(init, source);
                                    transparent.insert(name, wrapper);
                                }
                            }
                        }
                    }
                }
                if let Some(Declaration::FunctionDeclaration(func)) = &decl.declaration {
                    if let Some(ref id) = func.id {
                        let name = id.name.to_string();
                        if is_transparent_function(func, source) {
                            let wrapper = func
                                .body
                                .as_ref()
                                .and_then(|b| find_children_wrapper_in_body(b, source));
                            transparent.insert(name, wrapper);
                        }
                    }
                }
            }
            // export default function Foo({ children }) { ... }
            Statement::ExportDefaultDeclaration(decl) => {
                if let ExportDefaultDeclarationKind::FunctionDeclaration(func) = &decl.declaration {
                    if let Some(ref id) = func.id {
                        let name = id.name.to_string();
                        if is_transparent_function(func, source) {
                            let wrapper = func
                                .body
                                .as_ref()
                                .and_then(|b| find_children_wrapper_in_body(b, source));
                            transparent.insert(name, wrapper);
                        }
                    }
                }
            }
            // const Foo = ({ children }) => ... (non-exported, but may be re-exported)
            Statement::VariableDeclaration(var_decl) => {
                for declarator in &var_decl.declarations {
                    if let Some(name) = extract_binding_name(&declarator.id) {
                        if let Some(init) = &declarator.init {
                            if is_transparent_expression(init, source) {
                                let wrapper = find_children_wrapper_in_expression(init, source);
                                transparent.insert(name, wrapper);
                            }
                        }
                    }
                }
            }
            // function Foo({ children }) { ... }
            Statement::FunctionDeclaration(func) => {
                if let Some(ref id) = func.id {
                    let name = id.name.to_string();
                    if is_transparent_function(func, source) {
                        let wrapper = func
                            .body
                            .as_ref()
                            .and_then(|b| find_children_wrapper_in_body(b, source));
                        transparent.insert(name, wrapper);
                    }
                }
            }
            _ => {}
        }
    }

    Ok(transparent)
}

/// Extract the binding name from a BindingPattern (handles simple identifiers).
fn extract_binding_name(pattern: &BindingPattern<'_>) -> Option<String> {
    match pattern {
        BindingPattern::BindingIdentifier(id) => Some(id.name.to_string()),
        _ => None,
    }
}

/// Check if an expression is a transparent component.
///
/// Handles:
/// - Arrow functions: `({ children }) => ...`
/// - Function expressions: `function({ children }) { ... }`
/// - React.forwardRef: `React.forwardRef((props, ref) => ...)`
/// - React.memo: `React.memo(({ children }) => ...)`
/// - Type-cast wrappers: `(({ children }) => ...) as React.FC<...>`
fn is_transparent_expression(expr: &Expression<'_>, source: &str) -> bool {
    match expr {
        Expression::ArrowFunctionExpression(arrow) => {
            has_children_param(&arrow.params) && body_renders_children(&arrow.body, source)
        }
        Expression::FunctionExpression(func) => {
            has_children_param(&func.params)
                && func
                    .body
                    .as_ref()
                    .is_some_and(|body| function_body_renders_children(body, source))
        }
        // React.forwardRef((props, ref) => ...)
        // React.memo(Component)
        Expression::CallExpression(call) => {
            if is_react_wrapper_call(call) {
                // Check the first argument (the component function)
                if let Some(first_arg) = call.arguments.first() {
                    if let Some(expr) = first_arg.as_expression() {
                        return is_transparent_expression(expr, source);
                    }
                }
            }
            false
        }
        // Handle TSAsExpression: `(expr) as React.FC<Props>`
        Expression::TSAsExpression(ts_as) => is_transparent_expression(&ts_as.expression, source),
        // Handle parenthesized: `(expr)`
        Expression::ParenthesizedExpression(paren) => {
            is_transparent_expression(&paren.expression, source)
        }
        // Handle TSSatisfiesExpression: `expr satisfies React.FC<Props>`
        Expression::TSSatisfiesExpression(sat) => {
            is_transparent_expression(&sat.expression, source)
        }
        _ => false,
    }
}

/// Check if a function declaration is transparent.
fn is_transparent_function(func: &Function<'_>, source: &str) -> bool {
    has_children_param(&func.params)
        && func
            .body
            .as_ref()
            .is_some_and(|body| function_body_renders_children(body, source))
}

/// Check if a call expression is a React wrapper (forwardRef, memo).
fn is_react_wrapper_call(call: &CallExpression<'_>) -> bool {
    match &call.callee {
        // React.forwardRef(...) or React.memo(...)
        Expression::StaticMemberExpression(member) => {
            let prop = member.property.name.as_str();
            (prop == "forwardRef" || prop == "memo")
                && matches!(&member.object, Expression::Identifier(id) if id.name == "React")
        }
        // forwardRef(...) or memo(...)
        Expression::Identifier(id) => id.name == "forwardRef" || id.name == "memo",
        _ => false,
    }
}

/// Check if a function's formal parameters include `children`.
///
/// Detects:
/// - Destructured: `({ children, ...rest })`
/// - Object pattern with children key: `({ children }: Props)`
/// - Single props param (we check body for `props.children` usage)
fn has_children_param(params: &FormalParameters<'_>) -> bool {
    for param in &params.items {
        match &param.pattern {
            // ({ children, ... }) => ...
            BindingPattern::ObjectPattern(obj) => {
                for prop in &obj.properties {
                    if let PropertyKey::StaticIdentifier(id) = &prop.key {
                        if id.name == "children" {
                            return true;
                        }
                    }
                }
            }
            // (props) => ... — need to check body for props.children
            BindingPattern::BindingIdentifier(_) => {
                // We'll also return true here and let the body check
                // verify whether props.children is actually rendered.
                // This covers `(props) => <>{props.children}</>`.
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Check if an arrow function body renders `{children}` or `{props.children}`.
fn body_renders_children(body: &FunctionBody<'_>, source: &str) -> bool {
    function_body_renders_children(body, source)
}

/// Check if a function body renders children in any code path.
fn function_body_renders_children(body: &FunctionBody<'_>, source: &str) -> bool {
    for stmt in &body.statements {
        if statement_renders_children(stmt, source) {
            return true;
        }
    }
    false
}

/// Check if a statement renders children (recursing into blocks, ifs, returns).
fn statement_renders_children(stmt: &Statement<'_>, source: &str) -> bool {
    match stmt {
        Statement::ReturnStatement(ret) => {
            if let Some(arg) = &ret.argument {
                expression_renders_children(arg, source)
            } else {
                false
            }
        }
        Statement::ExpressionStatement(expr) => {
            expression_renders_children(&expr.expression, source)
        }
        Statement::BlockStatement(block) => block
            .body
            .iter()
            .any(|s| statement_renders_children(s, source)),
        Statement::IfStatement(if_stmt) => {
            // ANY branch rendering children counts
            statement_renders_children(&if_stmt.consequent, source)
                || if_stmt
                    .alternate
                    .as_ref()
                    .is_some_and(|alt| statement_renders_children(alt, source))
        }
        Statement::VariableDeclaration(var_decl) => var_decl.declarations.iter().any(|d| {
            d.init
                .as_ref()
                .is_some_and(|init| expression_renders_children(init, source))
        }),
        _ => false,
    }
}

/// Check if an expression renders `{children}` or `{props.children}`.
fn expression_renders_children(expr: &Expression<'_>, source: &str) -> bool {
    match expr {
        // Direct `children` identifier
        Expression::Identifier(id) => id.name == "children",

        // `props.children`
        Expression::StaticMemberExpression(member) => {
            member.property.name == "children"
                && matches!(&member.object, Expression::Identifier(id) if id.name == "props")
        }

        // JSX element — recurse into children to find {children} expression container
        Expression::JSXElement(el) => jsx_element_renders_children(el, source),

        // JSX fragment — recurse into children
        Expression::JSXFragment(frag) => frag
            .children
            .iter()
            .any(|child| jsx_child_renders_children(child, source)),

        // Ternary: cond ? a : b — check both branches
        Expression::ConditionalExpression(cond) => {
            expression_renders_children(&cond.consequent, source)
                || expression_renders_children(&cond.alternate, source)
        }

        // Logical: cond && expr — check right side
        Expression::LogicalExpression(logic) => expression_renders_children(&logic.right, source),

        // Parenthesized: (expr)
        Expression::ParenthesizedExpression(paren) => {
            expression_renders_children(&paren.expression, source)
        }

        // Arrow function body (inline render helpers)
        Expression::ArrowFunctionExpression(arrow) => {
            function_body_renders_children(&arrow.body, source)
        }

        // Call expression — check arguments (e.g., React.cloneElement(children, ...))
        Expression::CallExpression(call) => call.arguments.iter().any(|arg| {
            if let Some(expr) = arg.as_expression() {
                expression_renders_children(expr, source)
            } else {
                false
            }
        }),

        // Sequence expression — check last element
        Expression::SequenceExpression(seq) => seq
            .expressions
            .last()
            .is_some_and(|e| expression_renders_children(e, source)),

        _ => false,
    }
}

/// Check if a JSX element's children contain a `{children}` expression.
fn jsx_element_renders_children(el: &JSXElement<'_>, source: &str) -> bool {
    el.children
        .iter()
        .any(|child| jsx_child_renders_children(child, source))
}

// ── Wrapper detection ────────────────────────────────────────────────────
//
// These functions mirror the `*_renders_children` family above, but instead
// of returning bool they return the name of the JSX element that wraps
// `{children}`. Returns `None` for pure passthrough (Fragment, div, etc.)
// or when `{children}` is rendered at the top level without a wrapper.

/// Find the wrapper component in an expression (arrow fn, function expr, etc.).
fn find_children_wrapper_in_expression(expr: &Expression<'_>, source: &str) -> WrapperInfo {
    match expr {
        Expression::ArrowFunctionExpression(arrow) => {
            find_children_wrapper_in_body(&arrow.body, source)
        }
        Expression::FunctionExpression(func) => func
            .body
            .as_ref()
            .and_then(|b| find_children_wrapper_in_body(b, source)),
        Expression::CallExpression(call) => {
            // React.forwardRef / React.memo — check first argument
            if is_react_wrapper_call(call) {
                if let Some(first_arg) = call.arguments.first() {
                    if let Some(expr) = first_arg.as_expression() {
                        return find_children_wrapper_in_expression(expr, source);
                    }
                }
            }
            None
        }
        Expression::TSAsExpression(ts_as) => {
            find_children_wrapper_in_expression(&ts_as.expression, source)
        }
        Expression::ParenthesizedExpression(paren) => {
            find_children_wrapper_in_expression(&paren.expression, source)
        }
        Expression::TSSatisfiesExpression(sat) => {
            find_children_wrapper_in_expression(&sat.expression, source)
        }
        _ => None,
    }
}

/// Find the wrapper component in a function body.
fn find_children_wrapper_in_body(body: &FunctionBody<'_>, source: &str) -> WrapperInfo {
    for stmt in &body.statements {
        let wrapper = find_children_wrapper_in_statement(stmt, source);
        if wrapper.is_some() {
            return wrapper;
        }
    }
    None
}

/// Find the wrapper component in a statement.
fn find_children_wrapper_in_statement(stmt: &Statement<'_>, source: &str) -> WrapperInfo {
    match stmt {
        Statement::ReturnStatement(ret) => ret
            .argument
            .as_ref()
            .and_then(|arg| find_children_wrapper_in_expr(arg, source)),
        Statement::ExpressionStatement(expr) => {
            find_children_wrapper_in_expr(&expr.expression, source)
        }
        Statement::BlockStatement(block) => block
            .body
            .iter()
            .find_map(|s| find_children_wrapper_in_statement(s, source)),
        Statement::IfStatement(if_stmt) => {
            find_children_wrapper_in_statement(&if_stmt.consequent, source).or_else(|| {
                if_stmt
                    .alternate
                    .as_ref()
                    .and_then(|alt| find_children_wrapper_in_statement(alt, source))
            })
        }
        Statement::VariableDeclaration(var_decl) => var_decl.declarations.iter().find_map(|d| {
            d.init
                .as_ref()
                .and_then(|init| find_children_wrapper_in_expr(init, source))
        }),
        _ => None,
    }
}

/// Find the wrapper component in an expression.
fn find_children_wrapper_in_expr(expr: &Expression<'_>, source: &str) -> WrapperInfo {
    match expr {
        // Direct children — no wrapper
        Expression::Identifier(id) if id.name == "children" => None,
        Expression::StaticMemberExpression(member)
            if member.property.name == "children"
                && matches!(&member.object, Expression::Identifier(id) if id.name == "props") =>
        {
            None
        }
        // JSX element — check if it wraps {children}
        Expression::JSXElement(el) => find_wrapper_in_jsx_element(el, source),
        Expression::JSXFragment(frag) => {
            // Fragment itself is not a wrapper — look inside
            frag.children
                .iter()
                .find_map(|c| find_wrapper_in_jsx_child(c, source))
        }
        Expression::ConditionalExpression(cond) => {
            find_children_wrapper_in_expr(&cond.consequent, source)
                .or_else(|| find_children_wrapper_in_expr(&cond.alternate, source))
        }
        Expression::LogicalExpression(logic) => find_children_wrapper_in_expr(&logic.right, source),
        Expression::ParenthesizedExpression(paren) => {
            find_children_wrapper_in_expr(&paren.expression, source)
        }
        Expression::ArrowFunctionExpression(arrow) => {
            find_children_wrapper_in_body(&arrow.body, source)
        }
        _ => None,
    }
}

/// Find the wrapper component in a JSX element tree.
///
/// If THIS element's children contain `{children}` or `{props.children}`,
/// return the element name (if it looks like a component — starts with uppercase).
/// Otherwise recurse into child elements.
fn find_wrapper_in_jsx_element(el: &JSXElement<'_>, source: &str) -> WrapperInfo {
    let element_name = jsx_opening_element_name(&el.opening_element);

    // Check if any direct child is a {children} expression
    let has_children_passthrough = el.children.iter().any(|child| {
        matches!(child, JSXChild::ExpressionContainer(c)
            if is_direct_children_expression(&c.expression))
    });

    if has_children_passthrough {
        // This element wraps {children}. Return its name if it's a
        // component (starts with uppercase), otherwise None (e.g., <div>).
        if element_name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
        {
            return Some(element_name);
        } else {
            return None; // <div>{children}</div> — not a meaningful wrapper
        }
    }

    // Recurse into children to find a deeper wrapper
    el.children
        .iter()
        .find_map(|c| find_wrapper_in_jsx_child(c, source))
}

/// Find wrapper in a JSX child.
fn find_wrapper_in_jsx_child(child: &JSXChild<'_>, source: &str) -> WrapperInfo {
    match child {
        JSXChild::Element(el) => find_wrapper_in_jsx_element(el, source),
        JSXChild::Fragment(frag) => frag
            .children
            .iter()
            .find_map(|c| find_wrapper_in_jsx_child(c, source)),
        JSXChild::ExpressionContainer(container) => {
            find_wrapper_in_jsx_expression(&container.expression, source)
        }
        _ => None,
    }
}

/// Find wrapper in a JSX expression.
fn find_wrapper_in_jsx_expression(expr: &JSXExpression<'_>, source: &str) -> WrapperInfo {
    match expr {
        JSXExpression::JSXElement(el) => find_wrapper_in_jsx_element(el, source),
        JSXExpression::JSXFragment(frag) => frag
            .children
            .iter()
            .find_map(|c| find_wrapper_in_jsx_child(c, source)),
        JSXExpression::ConditionalExpression(cond) => {
            find_children_wrapper_in_expr(&cond.consequent, source)
                .or_else(|| find_children_wrapper_in_expr(&cond.alternate, source))
        }
        _ => None,
    }
}

/// Check if a JSX expression is exactly `children` or `props.children`.
fn is_direct_children_expression(expr: &JSXExpression<'_>) -> bool {
    match expr {
        JSXExpression::Identifier(id) => id.name == "children",
        JSXExpression::StaticMemberExpression(member) => {
            member.property.name == "children"
                && matches!(&member.object, Expression::Identifier(id) if id.name == "props")
        }
        _ => false,
    }
}

/// Extract the name of a JSX opening element.
fn jsx_opening_element_name(opening: &JSXOpeningElement<'_>) -> String {
    match &opening.name {
        JSXElementName::Identifier(id) => id.name.to_string(),
        JSXElementName::IdentifierReference(id) => id.name.to_string(),
        JSXElementName::NamespacedName(ns) => {
            format!("{}:{}", ns.namespace.name, ns.name.name)
        }
        JSXElementName::MemberExpression(member) => {
            // e.g., React.Fragment
            format!(
                "{}.{}",
                member_object_name(&member.object),
                member.property.name
            )
        }
        _ => String::new(),
    }
}

/// Extract the name from a JSXMemberExpressionObject.
fn member_object_name(obj: &JSXMemberExpressionObject<'_>) -> String {
    match obj {
        JSXMemberExpressionObject::IdentifierReference(id) => id.name.to_string(),
        JSXMemberExpressionObject::MemberExpression(member) => {
            format!(
                "{}.{}",
                member_object_name(&member.object),
                member.property.name
            )
        }
        _ => String::new(),
    }
}

/// Check if a JSX child renders `{children}` or `{props.children}`.
fn jsx_child_renders_children(child: &JSXChild<'_>, source: &str) -> bool {
    match child {
        JSXChild::ExpressionContainer(container) => {
            jsx_expression_renders_children(&container.expression, source)
        }
        JSXChild::Element(el) => jsx_element_renders_children(el, source),
        JSXChild::Fragment(frag) => frag
            .children
            .iter()
            .any(|c| jsx_child_renders_children(c, source)),
        _ => false,
    }
}

/// Check if a JSX expression renders children.
fn jsx_expression_renders_children(expr: &JSXExpression<'_>, source: &str) -> bool {
    match expr {
        JSXExpression::Identifier(id) => id.name == "children",
        JSXExpression::EmptyExpression(_) => false,
        // For other expressions, delegate to the general expression checker.
        // JSXExpression variants mirror Expression via @inherit, so we match
        // the common expression forms.
        JSXExpression::StaticMemberExpression(member) => {
            member.property.name == "children"
                && matches!(&member.object, Expression::Identifier(id) if id.name == "props")
        }
        JSXExpression::ConditionalExpression(cond) => {
            expression_renders_children(&cond.consequent, source)
                || expression_renders_children(&cond.alternate, source)
        }
        JSXExpression::LogicalExpression(logic) => {
            expression_renders_children(&logic.right, source)
        }
        JSXExpression::ParenthesizedExpression(paren) => {
            expression_renders_children(&paren.expression, source)
        }
        JSXExpression::JSXElement(el) => jsx_element_renders_children(el, source),
        JSXExpression::JSXFragment(frag) => frag
            .children
            .iter()
            .any(|c| jsx_child_renders_children(c, source)),
        JSXExpression::CallExpression(call) => call.arguments.iter().any(|arg| {
            if let Some(expr) = arg.as_expression() {
                expression_renders_children(expr, source)
            } else {
                false
            }
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_direct_children_passthrough() {
        let source = r#"
            export const Wrapper = ({ children }) => (
                <div>{children}</div>
            );
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(
            result.contains_key("Wrapper"),
            "Wrapper should be transparent"
        );
    }

    #[test]
    fn test_fragment_children_passthrough() {
        let source = r#"
            export const Wrapper = ({ children }) => (
                <>{children}</>
            );
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(
            result.contains_key("Wrapper"),
            "Fragment wrapper should be transparent"
        );
    }

    #[test]
    fn test_conditional_children_passthrough() {
        let source = r#"
            export const ConditionalTableBody = ({
                isLoading,
                isError,
                children
            }) => (
                <React.Fragment>
                    {isLoading ? (
                        <Tbody><Tr><Td>Loading...</Td></Tr></Tbody>
                    ) : isError ? (
                        <Tbody><Tr><Td>Error</Td></Tr></Tbody>
                    ) : (
                        children
                    )}
                </React.Fragment>
            );
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(
            result.contains_key("ConditionalTableBody"),
            "ConditionalTableBody should be transparent (children in one branch)"
        );
    }

    #[test]
    fn test_no_children_prop() {
        let source = r#"
            export const Display = ({ text }) => (
                <div>{text}</div>
            );
        "#;
        let result = analyze_source_transparency(source).unwrap();
        // This component has a single param `text` destructured, no `children`.
        // Actually, has_children_param returns true for BindingIdentifier —
        // but this uses ObjectPattern without `children` key, so it's false.
        assert!(
            !result.contains_key("Display"),
            "Display should not be transparent"
        );
    }

    #[test]
    fn test_children_prop_not_rendered() {
        let source = r#"
            export const Ignorer = ({ children }) => (
                <div>I ignore my children</div>
            );
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(
            !result.contains_key("Ignorer"),
            "Ignorer should not be transparent (doesn't render children)"
        );
    }

    #[test]
    fn test_props_dot_children() {
        let source = r#"
            export const Wrapper = (props) => (
                <div>{props.children}</div>
            );
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(
            result.contains_key("Wrapper"),
            "props.children passthrough should be transparent"
        );
    }

    #[test]
    fn test_function_declaration() {
        let source = r#"
            export function Wrapper({ children }) {
                return <div>{children}</div>;
            }
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(
            result.contains_key("Wrapper"),
            "Function declaration should work"
        );
    }

    #[test]
    fn test_forward_ref() {
        let source = r#"
            export const Wrapper = React.forwardRef(({ children }, ref) => (
                <div ref={ref}>{children}</div>
            ));
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(
            result.contains_key("Wrapper"),
            "forwardRef wrapper should be transparent"
        );
    }

    #[test]
    fn test_react_memo() {
        let source = r#"
            export const Wrapper = React.memo(({ children }) => (
                <div>{children}</div>
            ));
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(
            result.contains_key("Wrapper"),
            "React.memo wrapper should be transparent"
        );
    }

    #[test]
    fn test_if_statement_branch() {
        let source = r#"
            export function Wrapper({ children, isReady }) {
                if (isReady) {
                    return <div>{children}</div>;
                }
                return <div>Loading...</div>;
            }
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(
            result.contains_key("Wrapper"),
            "if-branch rendering children should be transparent"
        );
    }

    #[test]
    fn test_multiple_components_mixed() {
        let source = r#"
            export const Transparent = ({ children }) => <div>{children}</div>;
            export const Opaque = ({ text }) => <div>{text}</div>;
            export const AlsoTransparent = ({ children, extra }) => (
                <section>
                    <header>{extra}</header>
                    {children}
                </section>
            );
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(result.contains_key("Transparent"));
        assert!(!result.contains_key("Opaque"));
        assert!(result.contains_key("AlsoTransparent"));
    }

    #[test]
    fn test_nested_jsx_children_passthrough() {
        let source = r#"
            export const Wrapper = ({ children }) => (
                <div>
                    <section>
                        {children}
                    </section>
                </div>
            );
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(
            result.contains_key("Wrapper"),
            "Deeply nested children passthrough should be transparent"
        );
    }

    #[test]
    fn test_logical_and_children() {
        let source = r#"
            export const Wrapper = ({ children, show }) => (
                <div>{show && children}</div>
            );
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(
            result.contains_key("Wrapper"),
            "Logical AND children passthrough should be transparent"
        );
    }

    #[test]
    fn test_non_exported_not_included() {
        // Non-exported components should still be detected since they may
        // be re-exported from an index file.
        let source = r#"
            const Wrapper = ({ children }) => <div>{children}</div>;
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(
            result.contains_key("Wrapper"),
            "Non-exported but locally defined components should be detected"
        );
    }

    #[test]
    fn test_ts_as_expression() {
        let source = r#"
            export const Wrapper = (({ children }) => (
                <div>{children}</div>
            )) as React.FC<WrapperProps>;
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(
            result.contains_key("Wrapper"),
            "TSAsExpression wrapper should be transparent"
        );
    }

    // ── Wrapper detection tests ──────────────────────────────────────────

    #[test]
    fn test_wrapper_detects_table_around_children() {
        let source = r#"
            export const TableWrapper = ({ children }) => (
                <Table>{children}</Table>
            );
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(result.contains_key("TableWrapper"));
        assert_eq!(
            result.get("TableWrapper"),
            Some(&Some("Table".to_string())),
            "Should detect Table as the wrapper component"
        );
    }

    #[test]
    fn test_wrapper_none_for_fragment_passthrough() {
        let source = r#"
            export const FragWrapper = ({ children }) => (
                <>{children}</>
            );
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(result.contains_key("FragWrapper"));
        assert_eq!(
            result.get("FragWrapper"),
            Some(&None),
            "Fragment wrapper should have no wrapper component (pure passthrough)"
        );
    }

    #[test]
    fn test_wrapper_none_for_div_passthrough() {
        let source = r#"
            export const DivWrapper = ({ children }) => (
                <div className="wrapper">{children}</div>
            );
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(result.contains_key("DivWrapper"));
        assert_eq!(
            result.get("DivWrapper"),
            Some(&None),
            "div wrapper should have no wrapper component (not a PF component)"
        );
    }

    #[test]
    fn test_wrapper_detects_nested_component() {
        // children is wrapped in <ToolbarContent> which is inside <Toolbar>
        // The wrapper should be ToolbarContent (immediate parent of {children})
        let source = r#"
            export const MyToolbar = ({ children }) => (
                <Toolbar>
                    <ToolbarContent>{children}</ToolbarContent>
                </Toolbar>
            );
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(result.contains_key("MyToolbar"));
        assert_eq!(
            result.get("MyToolbar"),
            Some(&Some("ToolbarContent".to_string())),
            "Should detect ToolbarContent as the wrapper (immediate parent of children)"
        );
    }

    #[test]
    fn test_wrapper_with_forward_ref() {
        let source = r#"
            export const TableWithBatteries = React.forwardRef((props, ref) => (
                <Table innerRef={ref} {...props}>
                    {props.children}
                </Table>
            ));
        "#;
        let result = analyze_source_transparency(source).unwrap();
        assert!(result.contains_key("TableWithBatteries"));
        assert_eq!(
            result.get("TableWithBatteries"),
            Some(&Some("Table".to_string())),
            "forwardRef wrapping Table should detect Table as wrapper"
        );
    }
}

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
use oxc_span::SourceType;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Cache of file path → set of transparent component names exported from that file.
pub type TransparencyCache = HashMap<PathBuf, HashSet<String>>;

/// Analyze a source file and return the set of exported component names that
/// are transparent (pass `{children}` through in at least one code path).
///
/// Returns an empty set if the file can't be parsed or contains no transparent
/// components.
pub fn analyze_file_transparency(file_path: &Path) -> Result<HashSet<String>> {
    let source = std::fs::read_to_string(file_path)?;
    analyze_source_transparency(&source)
}

/// Analyze source code string and return the set of component names that are
/// transparent. Factored out from `analyze_file_transparency` for testing.
pub fn analyze_source_transparency(source: &str) -> Result<HashSet<String>> {
    let allocator = Allocator::default();
    let source_type = SourceType::tsx();
    let ret = Parser::new(&allocator, source, source_type).parse();

    if ret.panicked {
        return Ok(HashSet::new());
    }

    let mut transparent = HashSet::new();

    for stmt in &ret.program.body {
        match stmt {
            // export const Foo = ({ children }) => ...
            Statement::ExportNamedDeclaration(decl) => {
                if let Some(Declaration::VariableDeclaration(var_decl)) = &decl.declaration {
                    for declarator in &var_decl.declarations {
                        if let Some(name) = extract_binding_name(&declarator.id) {
                            if let Some(init) = &declarator.init {
                                if is_transparent_expression(init, source) {
                                    transparent.insert(name);
                                }
                            }
                        }
                    }
                }
                if let Some(Declaration::FunctionDeclaration(func)) = &decl.declaration {
                    if let Some(ref id) = func.id {
                        let name = id.name.to_string();
                        if is_transparent_function(func, source) {
                            transparent.insert(name);
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
                            transparent.insert(name);
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
                                transparent.insert(name);
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
                        transparent.insert(name);
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
        assert!(result.contains("Wrapper"), "Wrapper should be transparent");
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
            result.contains("Wrapper"),
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
            result.contains("ConditionalTableBody"),
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
            !result.contains("Display"),
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
            !result.contains("Ignorer"),
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
            result.contains("Wrapper"),
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
            result.contains("Wrapper"),
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
            result.contains("Wrapper"),
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
            result.contains("Wrapper"),
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
            result.contains("Wrapper"),
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
        assert!(result.contains("Transparent"));
        assert!(!result.contains("Opaque"));
        assert!(result.contains("AlsoTransparent"));
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
            result.contains("Wrapper"),
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
            result.contains("Wrapper"),
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
            result.contains("Wrapper"),
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
            result.contains("Wrapper"),
            "TSAsExpression wrapper should be transparent"
        );
    }
}

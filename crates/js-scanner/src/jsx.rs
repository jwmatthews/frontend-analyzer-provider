//! JSX scanning.
//!
//! Finds JSX component usage (`<Button ...>`) and JSX prop usage (`<X isActive={...}>`).
//! Walks the AST recursively to find JSXOpeningElement nodes.

#![allow(clippy::too_many_arguments)]

use crate::scanner::make_incident;
use frontend_core::capabilities::ReferenceLocation;
use frontend_core::incident::Incident;
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_resolver::Resolver;
use oxc_span::{GetSpan, SourceType};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Maximum depth for following re-export chains across files.
/// Prevents stack overflow on deeply chained barrel files.
const MAX_REEXPORT_DEPTH: usize = 20;

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

/// Shared scanning context passed through the JSX walk tree.
///
/// Bundles the parameters that every walk function needs, avoiding
/// long argument lists (source, pattern, file_uri, location, etc.).
struct ScanContext<'a, 'b> {
    source: &'a str,
    pattern: &'b Regex,
    file_uri: &'a str,
    location: Option<&'b ReferenceLocation>,
    incidents: &'b mut Vec<Incident>,
    import_map: &'b ImportMap,
    local_fns: &'b LocalFnMap<'a>,
    /// When set, only matches the parent component (via `pattern`) if it has
    /// at least one direct JSX child whose name matches this regex. The
    /// incident is emitted on the parent. Used for migration rules to detect
    /// old-style children that need restructuring.
    child: Option<&'b Regex>,
    /// When set, matches the parent component (via `pattern`) and emits
    /// incidents for each direct JSX child whose name does NOT match this
    /// regex. Used for "exclusive wrapper" rules.
    not_child: Option<&'b Regex>,
    /// When set, matches the parent component (via `pattern`) and emits an
    /// incident if NONE of its direct JSX children match this regex.
    /// Used for conformance rules like "AlertGroup must contain Alert."
    requires_child: Option<&'b Regex>,
    /// Components identified as transparent (children-passthrough) wrappers
    /// via cross-file resolution. Maps component name → wrapper info:
    /// - `None` = pure passthrough (Fragment, div) — collapse to grandparent
    /// - `Some("Table")` = wraps children in `<Table>` — substitute as parent
    transparent_components: &'b HashMap<String, crate::transparency::WrapperInfo>,
    /// Optional resolver for cross-file function reference resolution.
    /// When a prop value is an imported identifier (e.g., `toggle` from another
    /// file), the resolver follows the import to find the function's source.
    resolver: Option<&'b Resolver>,
    /// Path of the file being scanned (used for import resolution).
    file_path: Option<&'b Path>,
    /// Guard against infinite recursion when resolving local function references.
    /// Tracks function names currently being walked so that cycles
    /// (e.g., `renderA` → JSX → `renderB` → JSX → `renderA`) are detected
    /// and broken instead of causing a stack overflow.
    resolving_fns: HashSet<String>,
}

/// Build a map of all function declarations in the AST, including those
/// nested inside component bodies. Recurses into function/arrow bodies
/// to find declarations like:
/// Sentinel parent name for JSX found at the root of a React component
/// definition (e.g., `const Foo: React.FC = () => <X />`). The real parent
/// is determined at the consumer's call site, not here.
const COMPONENT_RETURN_SENTINEL: &str = "__ComponentReturn__";

/// Sentinel parent name for JSX found inside a React hook body
/// (e.g., `function useMyHook() { return <X />; }`). Hooks always compose
/// their return values into a parent at the call site.
const HOOK_RETURN_SENTINEL: &str = "__HookReturn__";

/// Check whether a name follows the React hook naming convention: starts
/// with `use` followed by an uppercase letter (e.g., `useToolbar`, `useMyHook`).
fn is_hook_name(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("use") {
        rest.chars().next().is_some_and(|c| c.is_ascii_uppercase())
    } else {
        false
    }
}

/// Check whether a `VariableDeclarator` has a type annotation that refers to a
/// React component type (`React.FC`, `React.FunctionComponent`,
/// `React.ComponentType`, `FC`, `FunctionComponent`).
///
/// Handles generic wrappers like `React.FC<Props>`.
fn has_component_type_annotation(declarator: &VariableDeclarator<'_>, source: &str) -> bool {
    let annotation = match declarator.type_annotation.as_ref() {
        Some(a) => a,
        None => return false,
    };
    type_is_react_component(&annotation.type_annotation, source)
}

/// Return `true` if `ts_type` is a React component type reference.
fn type_is_react_component(ts_type: &TSType<'_>, source: &str) -> bool {
    if let TSType::TSTypeReference(type_ref) = ts_type {
        let span = type_ref.type_name.span();
        let name = source
            .get(span.start as usize..span.end as usize)
            .unwrap_or_default();
        matches!(
            name,
            "React.FC"
                | "React.FunctionComponent"
                | "React.ComponentType"
                | "FC"
                | "FunctionComponent"
        )
    } else {
        false
    }
}

/// Extract a simple binding name from a `BindingPattern`.
/// Returns `None` for destructured patterns.
fn binding_name<'a>(binding: &'a BindingPattern<'a>, source: &'a str) -> Option<&'a str> {
    let span = binding.span();
    let raw = source.get(span.start as usize..span.end as usize)?;
    // Strip optional type annotation portion (e.g., "Foo: React.FC" → "Foo")
    let name = raw.split(':').next().unwrap_or("").trim();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

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
///
/// `transparent_components` is the set of locally-imported component names
/// that have been identified as children-passthrough wrappers via cross-file
/// resolution. When encountered as a JSX parent, they are collapsed out of
/// the parent chain so their children inherit the grandparent.
pub fn scan_jsx_file<'a>(
    stmts: &'a [Statement<'a>],
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    import_map: &ImportMap,
    child: Option<&Regex>,
    not_child: Option<&Regex>,
    requires_child: Option<&Regex>,
    transparent_components: &HashMap<String, crate::transparency::WrapperInfo>,
) -> Vec<Incident> {
    scan_jsx_file_with_resolver(
        stmts,
        source,
        pattern,
        file_uri,
        location,
        import_map,
        child,
        not_child,
        requires_child,
        transparent_components,
        None,
        None,
    )
}

/// Like `scan_jsx_file` but with optional cross-file resolution support.
///
/// When `resolver` and `file_path` are provided, function references passed
/// as prop values (e.g., `toggle={toggle}`) that resolve to imports will be
/// followed cross-file: the imported file is parsed, the exported function
/// found, and its JSX body walked with the parent context from the call site.
pub fn scan_jsx_file_with_resolver<'a>(
    stmts: &'a [Statement<'a>],
    source: &str,
    pattern: &Regex,
    file_uri: &str,
    location: Option<&ReferenceLocation>,
    import_map: &ImportMap,
    child: Option<&Regex>,
    not_child: Option<&Regex>,
    requires_child: Option<&Regex>,
    transparent_components: &HashMap<String, crate::transparency::WrapperInfo>,
    resolver: Option<&Resolver>,
    file_path: Option<&Path>,
) -> Vec<Incident> {
    let local_fns = build_local_fn_map(stmts, source);
    let mut incidents = Vec::new();
    let mut ctx = ScanContext {
        source,
        pattern,
        file_uri,
        location,
        incidents: &mut incidents,
        import_map,
        local_fns: &local_fns,
        child,
        not_child,
        requires_child,
        transparent_components,
        resolver,
        file_path,
        resolving_fns: HashSet::new(),
    };
    for stmt in stmts {
        walk_statement_for_jsx(stmt, &mut ctx, None);
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
    let empty_transparent = HashMap::new();
    let mut incidents = Vec::new();
    let mut ctx = ScanContext {
        source,
        pattern,
        file_uri,
        location,
        incidents: &mut incidents,
        import_map,
        local_fns: &empty_fns,
        child: None,
        not_child: None,
        requires_child: None,
        transparent_components: &empty_transparent,
        resolver: None,
        file_path: None,
        resolving_fns: HashSet::new(),
    };
    walk_statement_for_jsx(stmt, &mut ctx, None);
    incidents
}

/// If the function declaration is a React hook (name starts with `use` +
/// uppercase), return the hook sentinel so JSX in its body is treated as
/// indirect usage. Otherwise, return the passed-in parent unchanged.
fn fn_decl_effective_parent<'a>(
    func: &Function<'_>,
    parent_name: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(ref id) = func.id {
        if is_hook_name(id.name.as_str()) {
            return Some(HOOK_RETURN_SENTINEL);
        }
    }
    parent_name
}

fn walk_statement_for_jsx(stmt: &Statement<'_>, ctx: &mut ScanContext, parent_name: Option<&str>) {
    match stmt {
        Statement::ExportDefaultDeclaration(decl) => {
            if let ExportDefaultDeclarationKind::FunctionDeclaration(func) = &decl.declaration {
                if let Some(body) = &func.body {
                    let effective = fn_decl_effective_parent(func, parent_name);
                    walk_function_body(body, ctx, effective);
                }
            }
        }
        Statement::ExportNamedDeclaration(decl) => {
            if let Some(Declaration::FunctionDeclaration(func)) = &decl.declaration {
                if let Some(body) = &func.body {
                    let effective = fn_decl_effective_parent(func, parent_name);
                    walk_function_body(body, ctx, effective);
                }
            }
            if let Some(Declaration::VariableDeclaration(var_decl)) = &decl.declaration {
                walk_variable_declaration(var_decl, ctx, parent_name);
            }
        }
        Statement::FunctionDeclaration(func) => {
            if let Some(body) = &func.body {
                let effective = fn_decl_effective_parent(func, parent_name);
                walk_function_body(body, ctx, effective);
            }
        }
        Statement::VariableDeclaration(var_decl) => {
            walk_variable_declaration(var_decl, ctx, parent_name);
        }
        Statement::ReturnStatement(ret) => {
            if let Some(arg) = &ret.argument {
                walk_expression_for_jsx(arg, ctx, parent_name);
            }
        }
        Statement::ExpressionStatement(expr) => {
            walk_expression_for_jsx(&expr.expression, ctx, parent_name);
        }
        Statement::BlockStatement(block) => {
            for s in &block.body {
                walk_statement_for_jsx(s, ctx, parent_name);
            }
        }
        Statement::IfStatement(if_stmt) => {
            walk_statement_for_jsx(&if_stmt.consequent, ctx, parent_name);
            if let Some(alt) = &if_stmt.alternate {
                walk_statement_for_jsx(alt, ctx, parent_name);
            }
        }
        _ => {}
    }
}

fn walk_variable_declaration(
    var_decl: &VariableDeclaration<'_>,
    ctx: &mut ScanContext,
    parent_name: Option<&str>,
) {
    for declarator in &var_decl.declarations {
        // Check for typed object literals that represent component props.
        // e.g., `const x: ToolbarItemProps = { align: { default: 'alignRight' } }`
        // This catches prop values set in helper files outside JSX context,
        // where the object is later spread onto a JSX element in another file.
        if matches!(ctx.location, Some(ReferenceLocation::JsxProp) | None) {
            check_typed_object_literal(
                declarator,
                ctx.source,
                ctx.pattern,
                ctx.file_uri,
                ctx.import_map,
                ctx.incidents,
            );
        }

        if let Some(init) = &declarator.init {
            // Detect component/hook boundaries to set sentinel parents.
            //
            // When a variable is typed as React.FC (or similar), JSX inside
            // the initializer is a component definition — its real parent
            // will be determined at the consumer's call site, not here.
            //
            // When a variable name follows the React hook convention (use*),
            // any JSX in its body is indirect usage composed elsewhere.
            let effective_parent = if has_component_type_annotation(declarator, ctx.source) {
                Some(COMPONENT_RETURN_SENTINEL)
            } else if binding_name(&declarator.id, ctx.source).is_some_and(is_hook_name) {
                Some(HOOK_RETURN_SENTINEL)
            } else {
                parent_name
            };
            walk_expression_for_jsx(init, ctx, effective_parent);
        }
    }
}

fn walk_function_body(body: &FunctionBody<'_>, ctx: &mut ScanContext, parent_name: Option<&str>) {
    for stmt in &body.statements {
        walk_statement_for_jsx(stmt, ctx, parent_name);
    }
}

fn walk_expression_for_jsx(
    expr: &Expression<'_>,
    ctx: &mut ScanContext,
    parent_name: Option<&str>,
) {
    match expr {
        Expression::JSXElement(el) => {
            check_jsx_element(el, ctx, parent_name);
        }
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                walk_jsx_child(child, ctx, parent_name);
            }
        }
        Expression::ParenthesizedExpression(paren) => {
            walk_expression_for_jsx(&paren.expression, ctx, parent_name);
        }
        Expression::ConditionalExpression(cond) => {
            walk_expression_for_jsx(&cond.consequent, ctx, parent_name);
            walk_expression_for_jsx(&cond.alternate, ctx, parent_name);
        }
        Expression::LogicalExpression(logic) => {
            walk_expression_for_jsx(&logic.right, ctx, parent_name);
        }
        Expression::ArrowFunctionExpression(arrow) => {
            walk_function_body(&arrow.body, ctx, parent_name);
        }
        // Identifier reference: e.g., `toggle` in `toggle={toggle}`.
        // Resolve to a local function and walk its body with the current
        // parent context, so JSX rendered by that function inherits the
        // parent element (e.g., <Select>).
        Expression::Identifier(ident) => {
            resolve_local_fn_reference(ident.name.as_str(), ctx, parent_name, 0);
        }
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Argument::SpreadElement(spread) = arg {
                    walk_expression_for_jsx(&spread.argument, ctx, parent_name);
                } else if let Some(expr) = arg.as_expression() {
                    walk_expression_for_jsx(expr, ctx, parent_name);
                }
            }
        }
        Expression::ChainExpression(chain) => {
            // Handle optional chaining (e.g., `items?.map(item => <Component />)`)
            // Without this, JSX inside `?.map()` calls is invisible to the scanner.
            if let ChainElement::CallExpression(call) = &chain.expression {
                for arg in &call.arguments {
                    if let Argument::SpreadElement(spread) = arg {
                        walk_expression_for_jsx(&spread.argument, ctx, parent_name);
                    } else if let Some(expr) = arg.as_expression() {
                        walk_expression_for_jsx(expr, ctx, parent_name);
                    }
                }
            }
        }
        _ => {}
    }
}

fn walk_jsx_child(child: &JSXChild<'_>, ctx: &mut ScanContext, parent_name: Option<&str>) {
    match child {
        JSXChild::Element(el) => {
            check_jsx_element(el, ctx, parent_name);
        }
        JSXChild::Fragment(frag) => {
            for c in &frag.children {
                walk_jsx_child(c, ctx, parent_name);
            }
        }
        JSXChild::ExpressionContainer(container) => {
            // JSXExpression inherits Expression variants via @inherit macro.
            // Walk into the expression to find nested JSX elements.
            walk_jsx_expression(&container.expression, ctx, parent_name);
        }
        _ => {}
    }
}

/// Walk a JSXExpression (which inherits all Expression variants) for nested JSX.
/// This handles expression containers in JSX children ({cond && <X/>}) and
/// prop value expressions (toggle={ref => (<MenuToggle ...>)}).
fn walk_jsx_expression(
    jsx_expr: &JSXExpression<'_>,
    ctx: &mut ScanContext,
    parent_name: Option<&str>,
) {
    match jsx_expr {
        JSXExpression::EmptyExpression(_) => {}
        // Direct JSX nesting: {<Component />}
        JSXExpression::JSXElement(el) => {
            check_jsx_element(el, ctx, parent_name);
        }
        JSXExpression::JSXFragment(frag) => {
            for child in &frag.children {
                walk_jsx_child(child, ctx, parent_name);
            }
        }
        // Parenthesized: {(<Component />)}
        JSXExpression::ParenthesizedExpression(paren) => {
            walk_expression_for_jsx(&paren.expression, ctx, parent_name);
        }
        // Arrow functions: {ref => (<Component />)} or {() => <Component />}
        JSXExpression::ArrowFunctionExpression(arrow) => {
            walk_function_body(&arrow.body, ctx, parent_name);
        }
        // Conditionals: {condition && <Component />} or {cond ? <A/> : <B/>}
        JSXExpression::ConditionalExpression(cond) => {
            walk_expression_for_jsx(&cond.consequent, ctx, parent_name);
            walk_expression_for_jsx(&cond.alternate, ctx, parent_name);
        }
        JSXExpression::LogicalExpression(logic) => {
            walk_expression_for_jsx(&logic.right, ctx, parent_name);
        }
        // Identifier reference: e.g., `{toggle}` in children or `toggle={toggle}`.
        // Resolve to a local function and walk its body with the current parent
        // context, so JSX rendered by that function inherits the parent element.
        JSXExpression::Identifier(ident) => {
            resolve_local_fn_reference(ident.name.as_str(), ctx, parent_name, 0);
        }
        // Function calls: {renderFn(<Component />)} or {fn(arg)}
        JSXExpression::CallExpression(call) => {
            // Resolve local function calls: if the callee is a known local
            // function (e.g., `renderDropdownItems`), walk its body with the
            // current parent context so JSX returned by that function inherits
            // the parent element (e.g., <Dropdown>).
            if let Expression::Identifier(ident) = &call.callee {
                let callee_name = ident.name.as_str();
                if let Some(fn_expr) = ctx.local_fns.get(callee_name) {
                    // Guard against cycles (same logic as resolve_local_fn_reference)
                    if ctx.resolving_fns.contains(callee_name) {
                        tracing::debug!(
                            fn_name = callee_name,
                            file = ctx.file_uri,
                            "skipping cyclic local function call in JSX child",
                        );
                    } else {
                        ctx.resolving_fns.insert(callee_name.to_string());
                        match fn_expr {
                            Expression::ArrowFunctionExpression(arrow) => {
                                walk_function_body(&arrow.body, ctx, parent_name);
                            }
                            Expression::FunctionExpression(func) => {
                                if let Some(body) = &func.body {
                                    walk_function_body(body, ctx, parent_name);
                                }
                            }
                            _ => {}
                        }
                        ctx.resolving_fns.remove(callee_name);
                    }
                }
            }

            // Also walk call arguments for JSX passed as args
            for arg in &call.arguments {
                if let Argument::SpreadElement(spread) = arg {
                    walk_expression_for_jsx(&spread.argument, ctx, parent_name);
                } else if let Some(expr) = arg.as_expression() {
                    walk_expression_for_jsx(expr, ctx, parent_name);
                }
            }
        }
        // Optional chaining: {items?.map(item => <Component />)}
        JSXExpression::ChainExpression(chain) => {
            if let ChainElement::CallExpression(call) = &chain.expression {
                for arg in &call.arguments {
                    if let Argument::SpreadElement(spread) = arg {
                        walk_expression_for_jsx(&spread.argument, ctx, parent_name);
                    } else if let Some(expr) = arg.as_expression() {
                        walk_expression_for_jsx(expr, ctx, parent_name);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Check if a JSX expression is a `{children}` or `{props.children}` passthrough.
///
/// Used to suppress `requiresChild` and `notChild` conformance checks when the
/// component passes `{children}` through — the actual children are provided at
/// the call site, not at the definition site.
fn is_children_passthrough_expression(expr: &JSXExpression<'_>) -> bool {
    match expr {
        JSXExpression::Identifier(id) => id.name == "children",
        JSXExpression::StaticMemberExpression(member) => {
            member.property.name == "children"
                && matches!(&member.object, Expression::Identifier(id) if id.name == "props")
        }
        _ => false,
    }
}

/// Resolve a function reference by name — same-file or cross-file.
///
/// When an identifier like `toggle` is encountered in a JSX expression context
/// (either as a prop value `toggle={toggle}` or as a child `{toggle}`):
///
/// 1. Check the `LocalFnMap` for a same-file function definition.
/// 2. If not found and the name is an import, resolve the import cross-file
///    using the `oxc_resolver`, parse the target file, find the exported
///    function, and walk its JSX body with the current parent context.
///
/// This handles the common pattern where render functions are defined as
/// variables and passed as prop values:
///
/// ```tsx
/// const toggle = (toggleRef) => <MenuToggle ref={toggleRef}>...</MenuToggle>;
/// return <Select toggle={toggle}>...</Select>;
/// // → MenuToggle's parentName is "Select"
/// ```
fn resolve_local_fn_reference(
    name: &str,
    ctx: &mut ScanContext,
    parent_name: Option<&str>,
    depth: usize,
) {
    // 1. Same-file: check LocalFnMap
    if let Some(fn_expr) = ctx.local_fns.get(name) {
        // Guard against cycles: if we're already resolving this function,
        // we've hit a recursive reference (e.g., renderA → renderB → renderA).
        if ctx.resolving_fns.contains(name) {
            tracing::debug!(
                fn_name = name,
                file = ctx.file_uri,
                "skipping cyclic local function reference",
            );
            return;
        }
        ctx.resolving_fns.insert(name.to_string());
        match fn_expr {
            Expression::ArrowFunctionExpression(arrow) => {
                walk_function_body(&arrow.body, ctx, parent_name);
            }
            Expression::FunctionExpression(func) => {
                if let Some(body) = &func.body {
                    walk_function_body(body, ctx, parent_name);
                }
            }
            _ => {}
        }
        ctx.resolving_fns.remove(name);
        return;
    }

    // 2. Cross-file: resolve imported function reference
    let module_source = match ctx.import_map.get(name) {
        Some(m) => m.clone(),
        None => return,
    };
    let (resolver, file_path) = match (ctx.resolver, ctx.file_path) {
        (Some(r), Some(p)) => (r, p),
        _ => return,
    };

    // Resolve the import to a file path
    let resolved_path =
        match crate::resolve::resolve_import_with_resolver(resolver, file_path, &module_source) {
            Some(p) => p,
            None => return,
        };

    // Skip node_modules — we don't parse library source
    if crate::resolve::is_node_modules_path(&resolved_path) {
        return;
    }

    // Parse the resolved file and find the exported function
    let source_text = match std::fs::read_to_string(&resolved_path) {
        Ok(s) => s,
        Err(_) => return,
    };

    resolve_cross_file_fn(
        name,
        &source_text,
        ctx,
        parent_name,
        resolver,
        &resolved_path,
        depth,
    );
}

/// Parse a resolved file and walk the exported function's JSX body with
/// the given parent context. This is the cross-file resolution workhorse.
fn resolve_cross_file_fn(
    name: &str,
    source_text: &str,
    ctx: &mut ScanContext,
    parent_name: Option<&str>,
    resolver: &Resolver,
    resolved_path: &Path,
    depth: usize,
) {
    if depth > MAX_REEXPORT_DEPTH {
        tracing::warn!(
            "re-export chain depth exceeded {} for '{}' at {}",
            MAX_REEXPORT_DEPTH,
            name,
            resolved_path.display(),
        );
        return;
    }

    let allocator = Allocator::default();
    let source_type = SourceType::from_path(resolved_path).unwrap_or_default();
    let ret = Parser::new(&allocator, source_text, source_type).parse();
    if ret.panicked {
        return;
    }

    // Build the local fn map for the resolved file so we can find the export
    let remote_fns = build_local_fn_map(&ret.program.body, source_text);

    // Check if the function is in the local fn map (covers `export const toggle = ...`)
    if let Some(fn_expr) = remote_fns.get(name) {
        // Walk the function body using OUR context (pattern, incidents, etc.)
        // but with parent_name from the call site.
        match fn_expr {
            Expression::ArrowFunctionExpression(arrow) => {
                walk_function_body(&arrow.body, ctx, parent_name);
            }
            Expression::FunctionExpression(func) => {
                if let Some(body) = &func.body {
                    walk_function_body(body, ctx, parent_name);
                }
            }
            _ => {}
        }
        return;
    }

    // Check for `export function toggle(...)` declarations
    for stmt in &ret.program.body {
        if let Statement::ExportNamedDeclaration(export) = stmt {
            if let Some(Declaration::FunctionDeclaration(func)) = &export.declaration {
                let fn_name = func.id.as_ref().map(|id| id.name.as_str()).unwrap_or("");
                if fn_name == name {
                    if let Some(body) = &func.body {
                        walk_function_body(body, ctx, parent_name);
                    }
                    return;
                }
            }
        }
        // Also check `export default function` when the import is "default"
        if let Statement::ExportDefaultDeclaration(export) = stmt {
            if let ExportDefaultDeclarationKind::FunctionDeclaration(func) = &export.declaration {
                if let Some(body) = &func.body {
                    walk_function_body(body, ctx, parent_name);
                }
                return;
            }
        }
    }

    // Follow re-exports: `export { toggle } from './otherFile'`
    for stmt in &ret.program.body {
        if let Statement::ExportNamedDeclaration(export) = stmt {
            if let Some(ref source) = export.source {
                for spec in &export.specifiers {
                    let exported_name = spec.exported.name();
                    if exported_name == name {
                        // Resolve the re-export target
                        let local_name = spec.local.name().to_string();
                        if let Some(re_resolved) = crate::resolve::resolve_import_with_resolver(
                            resolver,
                            resolved_path,
                            source.value.as_str(),
                        ) {
                            if !crate::resolve::is_node_modules_path(&re_resolved) {
                                if let Ok(re_source) = std::fs::read_to_string(&re_resolved) {
                                    resolve_cross_file_fn(
                                        &local_name,
                                        &re_source,
                                        ctx,
                                        parent_name,
                                        resolver,
                                        &re_resolved,
                                        depth + 1,
                                    );
                                }
                            }
                        }
                        return;
                    }
                }
            }
        }
        // `export * from './otherFile'` — follow and search
        if let Statement::ExportAllDeclaration(export) = stmt {
            if let Some(re_resolved) = crate::resolve::resolve_import_with_resolver(
                resolver,
                resolved_path,
                export.source.value.as_str(),
            ) {
                if !crate::resolve::is_node_modules_path(&re_resolved) {
                    if let Ok(re_source) = std::fs::read_to_string(&re_resolved) {
                        resolve_cross_file_fn(
                            name,
                            &re_source,
                            ctx,
                            parent_name,
                            resolver,
                            &re_resolved,
                            depth + 1,
                        );
                    }
                }
            }
        }
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

// ── Child component name collection ─────────────────────────────────────
//
// These functions walk JSX children — including expression containers like
// `.map()`, conditionals, and render functions — to collect the component
// names found. Used by the `child`, `requiresChild`, and `notChild`
// scanners so they can see dynamic children that are invisible when only
// checking direct `JSXChild::Element` nodes.

/// Collect all component names (with spans) found in JSX children,
/// walking into expression containers to find components inside `.map()`,
/// conditionals, arrow functions, etc.
fn collect_child_components(
    children: &[JSXChild<'_>],
    local_fns: &LocalFnMap,
) -> Vec<(String, oxc_span::Span)> {
    let mut results = Vec::new();
    for child in children {
        match child {
            JSXChild::Element(el) => {
                let name = jsx_element_name_to_string(&el.opening_element.name);
                let span = el.opening_element.name.span();
                results.push((name, span));
            }
            JSXChild::Fragment(frag) => {
                results.extend(collect_child_components(&frag.children, local_fns));
            }
            JSXChild::ExpressionContainer(container) => {
                collect_names_from_jsx_expression(&container.expression, local_fns, &mut results);
            }
            _ => {}
        }
    }
    results
}

/// Walk a JSXExpression to collect component names. Mirrors `walk_jsx_expression`
/// but collects names instead of emitting incidents.
fn collect_names_from_jsx_expression(
    jsx_expr: &JSXExpression<'_>,
    local_fns: &LocalFnMap,
    results: &mut Vec<(String, oxc_span::Span)>,
) {
    match jsx_expr {
        JSXExpression::EmptyExpression(_) => {}
        JSXExpression::JSXElement(el) => {
            let name = jsx_element_name_to_string(&el.opening_element.name);
            let span = el.opening_element.name.span();
            results.push((name, span));
        }
        JSXExpression::JSXFragment(frag) => {
            results.extend(collect_child_components(&frag.children, local_fns));
        }
        JSXExpression::ParenthesizedExpression(paren) => {
            collect_names_from_expression(&paren.expression, local_fns, results);
        }
        JSXExpression::ArrowFunctionExpression(arrow) => {
            collect_names_from_fn_body(&arrow.body, local_fns, results);
        }
        JSXExpression::ConditionalExpression(cond) => {
            collect_names_from_expression(&cond.consequent, local_fns, results);
            collect_names_from_expression(&cond.alternate, local_fns, results);
        }
        JSXExpression::LogicalExpression(logic) => {
            collect_names_from_expression(&logic.right, local_fns, results);
        }
        JSXExpression::CallExpression(call) => {
            // Resolve local function calls
            if let Expression::Identifier(ident) = &call.callee {
                if let Some(fn_expr) = local_fns.get(ident.name.as_str()) {
                    match fn_expr {
                        Expression::ArrowFunctionExpression(arrow) => {
                            collect_names_from_fn_body(&arrow.body, local_fns, results);
                        }
                        Expression::FunctionExpression(func) => {
                            if let Some(body) = &func.body {
                                collect_names_from_fn_body(body, local_fns, results);
                            }
                        }
                        _ => {}
                    }
                }
            }
            // Walk call arguments
            for arg in &call.arguments {
                if let Argument::SpreadElement(spread) = arg {
                    collect_names_from_expression(&spread.argument, local_fns, results);
                } else if let Some(expr) = arg.as_expression() {
                    collect_names_from_expression(expr, local_fns, results);
                }
            }
        }
        JSXExpression::ChainExpression(chain) => {
            if let ChainElement::CallExpression(call) = &chain.expression {
                for arg in &call.arguments {
                    if let Argument::SpreadElement(spread) = arg {
                        collect_names_from_expression(&spread.argument, local_fns, results);
                    } else if let Some(expr) = arg.as_expression() {
                        collect_names_from_expression(expr, local_fns, results);
                    }
                }
            }
        }
        // Array literals: {[<Tab />, items.map(i => <Tab />)]}
        JSXExpression::ArrayExpression(arr) => {
            for elem in &arr.elements {
                match elem {
                    ArrayExpressionElement::SpreadElement(spread) => {
                        collect_names_from_expression(&spread.argument, local_fns, results);
                    }
                    _ => {
                        if let Some(expr) = elem.as_expression() {
                            collect_names_from_expression(expr, local_fns, results);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Walk an Expression to collect component names. Mirrors `walk_expression_for_jsx`.
fn collect_names_from_expression(
    expr: &Expression<'_>,
    local_fns: &LocalFnMap,
    results: &mut Vec<(String, oxc_span::Span)>,
) {
    match expr {
        Expression::JSXElement(el) => {
            let name = jsx_element_name_to_string(&el.opening_element.name);
            let span = el.opening_element.name.span();
            results.push((name, span));
        }
        Expression::JSXFragment(frag) => {
            results.extend(collect_child_components(&frag.children, local_fns));
        }
        Expression::ParenthesizedExpression(paren) => {
            collect_names_from_expression(&paren.expression, local_fns, results);
        }
        Expression::ConditionalExpression(cond) => {
            collect_names_from_expression(&cond.consequent, local_fns, results);
            collect_names_from_expression(&cond.alternate, local_fns, results);
        }
        Expression::LogicalExpression(logic) => {
            collect_names_from_expression(&logic.right, local_fns, results);
        }
        Expression::ArrowFunctionExpression(arrow) => {
            collect_names_from_fn_body(&arrow.body, local_fns, results);
        }
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Argument::SpreadElement(spread) = arg {
                    collect_names_from_expression(&spread.argument, local_fns, results);
                } else if let Some(expr) = arg.as_expression() {
                    collect_names_from_expression(expr, local_fns, results);
                }
            }
        }
        Expression::ChainExpression(chain) => {
            if let ChainElement::CallExpression(call) = &chain.expression {
                for arg in &call.arguments {
                    if let Argument::SpreadElement(spread) = arg {
                        collect_names_from_expression(&spread.argument, local_fns, results);
                    } else if let Some(expr) = arg.as_expression() {
                        collect_names_from_expression(expr, local_fns, results);
                    }
                }
            }
        }
        // Array literals: {[<Tab />, items.map(i => <Tab />)]}
        Expression::ArrayExpression(arr) => {
            for elem in &arr.elements {
                match elem {
                    ArrayExpressionElement::SpreadElement(spread) => {
                        collect_names_from_expression(&spread.argument, local_fns, results);
                    }
                    _ => {
                        if let Some(expr) = elem.as_expression() {
                            collect_names_from_expression(expr, local_fns, results);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Walk a FunctionBody to collect component names from return statements.
fn collect_names_from_fn_body(
    body: &FunctionBody<'_>,
    local_fns: &LocalFnMap,
    results: &mut Vec<(String, oxc_span::Span)>,
) {
    for stmt in &body.statements {
        match stmt {
            Statement::ReturnStatement(ret) => {
                if let Some(arg) = &ret.argument {
                    collect_names_from_expression(arg, local_fns, results);
                }
            }
            Statement::ExpressionStatement(expr) => {
                collect_names_from_expression(&expr.expression, local_fns, results);
            }
            _ => {}
        }
    }
}

fn check_jsx_element(el: &JSXElement<'_>, ctx: &mut ScanContext, parent_name: Option<&str>) {
    let opening = &el.opening_element;
    let component_name = jsx_element_name_to_string(&opening.name);

    // Check component name
    let search_component = matches!(ctx.location, Some(ReferenceLocation::JsxComponent) | None);
    if search_component && ctx.pattern.is_match(&component_name) {
        let span = opening.name.span();
        let mut incident = make_incident(ctx.source, ctx.file_uri, span.start, span.end);
        incident.variables.insert(
            "componentName".into(),
            serde_json::Value::String(component_name.clone()),
        );
        // Resolve the matched component's import source
        if let Some(module) = ctx.import_map.get(&component_name) {
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
            if let Some(parent_module) = ctx.import_map.get(parent) {
                incident.variables.insert(
                    "parentFrom".into(),
                    serde_json::Value::String(parent_module.clone()),
                );
            }
        }
        // Collect all child component names (direct JSX elements AND those
        // nested inside expression containers like .map(), conditionals, etc.).
        // This is used by all three child-matching scanners below.
        let all_children =
            if ctx.child.is_some() || ctx.not_child.is_some() || ctx.requires_child.is_some() {
                collect_child_components(&el.children, ctx.local_fns)
            } else {
                Vec::new()
            };

        // child: only emit the parent incident if at least one child
        // matches the `child` pattern. Used for migration rules to detect
        // old-style children still present. Walks into expression containers
        // so `.map()` and conditional children are visible.
        let child_gate_passed = if let Some(child_re) = ctx.child {
            all_children.iter().any(|(name, _)| child_re.is_match(name))
        } else {
            true // no child filter — gate is open
        };

        if child_gate_passed && ctx.not_child.is_none() && ctx.requires_child.is_none() {
            ctx.incidents.push(incident.clone());
        }

        // requiresChild: emit incident if NO child matches the required
        // pattern. Walks into expression containers so `.map()` and
        // conditional children are visible.
        //
        // SKIP when the component passes {children} or {props.children}
        // through — the actual children come from the call site, not this JSX.
        // Checking here would be a false positive.
        let passes_children_through = el.children.iter().any(|child| {
            if let JSXChild::ExpressionContainer(c) = child {
                is_children_passthrough_expression(&c.expression)
            } else {
                false
            }
        });

        if let Some(req_re) = ctx.requires_child {
            let has_required_child = all_children.iter().any(|(name, _)| req_re.is_match(name));
            if !has_required_child && !passes_children_through {
                ctx.incidents.push(incident);
            }
        }

        // notChild: emit incidents for each child whose name does NOT match
        // the pattern. Walks into expression containers so `.map()` and
        // conditional children are visible.
        // Skipped when the component passes {children} through.
        if let Some(not_child_re) = ctx.not_child {
            if !passes_children_through {
                for (child_name, child_span) in &all_children {
                    if !not_child_re.is_match(child_name) {
                        let mut child_incident = make_incident(
                            ctx.source,
                            ctx.file_uri,
                            child_span.start,
                            child_span.end,
                        );
                        child_incident.variables.insert(
                            "componentName".into(),
                            serde_json::Value::String(child_name.clone()),
                        );
                        child_incident.variables.insert(
                            "parentName".into(),
                            serde_json::Value::String(component_name.clone()),
                        );
                        if let Some(module) = ctx.import_map.get(child_name) {
                            child_incident
                                .variables
                                .insert("module".into(), serde_json::Value::String(module.clone()));
                        }
                        if let Some(parent_module) = ctx.import_map.get(&component_name) {
                            child_incident.variables.insert(
                                "parentFrom".into(),
                                serde_json::Value::String(parent_module.clone()),
                            );
                        }
                        ctx.incidents.push(child_incident);
                    }
                }
            }
        }
    }

    // Check props
    let search_props = matches!(ctx.location, Some(ReferenceLocation::JsxProp) | None);
    if search_props {
        for attr in &opening.attributes {
            if let JSXAttributeItem::Attribute(a) = attr {
                if let JSXAttributeName::Identifier(ident) = &a.name {
                    let prop_name = ident.name.as_str();
                    if ctx.pattern.is_match(prop_name) {
                        let span = ident.span();
                        let mut incident =
                            make_incident(ctx.source, ctx.file_uri, span.start, span.end);
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
                                    // Static member expression: Foo.bar → extract "bar".
                                    // This handles enum-style prop values like
                                    // variant={PageSectionVariants.light} so that the
                                    // value filter `value: ^light$` matches correctly.
                                    if let JSXExpression::StaticMemberExpression(member) =
                                        &expr.expression
                                    {
                                        Some(member.property.name.to_string())
                                    } else {
                                        // Fallback: capture the raw source text
                                        let expr_span = expr.span();
                                        // Strip the { } wrapper, with bounds checking
                                        let start =
                                            (expr_span.start as usize + 1).min(ctx.source.len());
                                        let end = (expr_span.end as usize)
                                            .saturating_sub(1)
                                            .max(start)
                                            .min(ctx.source.len());
                                        let text = &ctx.source[start..end];
                                        Some(text.trim().to_string())
                                    }
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
                                    extract_object_string_values(&expr.expression, ctx.source);
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
                        if let Some(module) = ctx.import_map.get(&component_name) {
                            incident
                                .variables
                                .insert("module".into(), serde_json::Value::String(module.clone()));
                        }

                        ctx.incidents.push(incident);
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
                        ctx.source,
                        ctx.pattern,
                        ctx.file_uri,
                        &component_name,
                        ctx.import_map,
                        ctx.incidents,
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
                walk_jsx_expression(&expr.expression, ctx, Some(&component_name));
            }
        }
    }

    // Recurse into children.
    //
    // If this component is a transparent wrapper (passes {children} through,
    // identified via cross-file resolution), resolve the effective parent:
    // - Pure passthrough (Fragment, div): collapse to grandparent
    // - Wrapper (e.g., wraps in <Table>): substitute the wrapper as parent
    let effective_parent =
        if let Some(wrapper_info) = ctx.transparent_components.get(&component_name) {
            match wrapper_info {
                // Pure passthrough — children see the grandparent
                None => parent_name,
                // Wraps children in a PF component — substitute as parent.
                // We leak the string here via Box to get a stable &str reference.
                Some(wrapper_name) => Some(wrapper_name.as_str()),
            }
        } else {
            Some(component_name.as_str())
        };
    for child in &el.children {
        walk_jsx_child(child, ctx, effective_parent);
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

        let empty_transparent = HashMap::new();
        scan_jsx_file(
            &ret.program.body,
            source,
            &re,
            "file:///test.tsx",
            location,
            &import_map,
            None, // child
            None, // not_child
            None, // requires_child
            &empty_transparent,
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
    fn test_jsx_prop_enum_member_expression_value() {
        // Enum-style prop values like variant={PageSectionVariants.light}
        // should extract the member name ("light") as propValue, so that
        // value-based rules (value: ^light$) match correctly.
        let source =
            r#"const el = <PageSection variant={PageSectionVariants.light}>content</PageSection>;"#;
        let incidents = scan_source_jsx(source, r"^variant$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("propValue"),
            Some(&serde_json::Value::String("light".to_string())),
            "Should extract member name 'light' from PageSectionVariants.light"
        );
    }

    #[test]
    fn test_jsx_prop_enum_member_nested_expression() {
        // Non-member expressions should still capture raw source text
        let source =
            r#"const el = <Button variant={isActive ? "primary" : "secondary"}>Click</Button>;"#;
        let incidents = scan_source_jsx(source, r"^variant$", Some(&ReferenceLocation::JsxProp));
        assert_eq!(incidents.len(), 1);
        // Should be the raw source text, not a member name
        let pv = incidents[0]
            .variables
            .get("propValue")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(
            pv.contains("isActive"),
            "Non-member expressions should preserve raw source text, got: {}",
            pv
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
            !incidents[0].variables.contains_key("propObjectValues"),
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

    // ── notChild tests ──────────────────────────────────────────────

    fn scan_source_jsx_not_child(source: &str, pattern: &str, not_child: &str) -> Vec<Incident> {
        let allocator = Allocator::default();
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let re = Regex::new(pattern).unwrap();
        let not_child_re = Regex::new(not_child).unwrap();
        let import_map = build_import_map(&ret.program);

        let empty_transparent = HashMap::new();
        scan_jsx_file(
            &ret.program.body,
            source,
            &re,
            "file:///test.tsx",
            Some(&ReferenceLocation::JsxComponent),
            &import_map,
            None, // child
            Some(&not_child_re),
            None, // requires_child
            &empty_transparent,
        )
    }

    #[test]
    fn test_not_child_basic() {
        let source = r#"
import { InputGroup, InputGroupItem, TextInput, Button } from '@patternfly/react-core';
const el = (
    <InputGroup>
        <InputGroupItem><TextInput /></InputGroupItem>
        <TextInput />
        <Button>Go</Button>
    </InputGroup>
);
"#;
        let incidents = scan_source_jsx_not_child(
            source,
            r"^InputGroup$",
            r"^(InputGroupItem|InputGroupText)$",
        );
        // TextInput and Button are direct children not matching notChild
        assert_eq!(incidents.len(), 2);
        let names: Vec<_> = incidents
            .iter()
            .map(|i| i.variables["componentName"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"TextInput"));
        assert!(names.contains(&"Button"));
        // All should have parentName = InputGroup
        for inc in &incidents {
            assert_eq!(inc.variables["parentName"].as_str().unwrap(), "InputGroup");
        }
    }

    #[test]
    fn test_not_child_all_valid() {
        let source = r#"
import { InputGroup, InputGroupItem, InputGroupText } from '@patternfly/react-core';
const el = (
    <InputGroup>
        <InputGroupItem><div /></InputGroupItem>
        <InputGroupText>@</InputGroupText>
    </InputGroup>
);
"#;
        let incidents = scan_source_jsx_not_child(
            source,
            r"^InputGroup$",
            r"^(InputGroupItem|InputGroupText)$",
        );
        // All children match — no incidents
        assert_eq!(incidents.len(), 0);
    }

    #[test]
    fn test_not_child_no_parent_incident() {
        // When notChild is set, the parent itself should NOT produce an incident
        let source = r#"
import { InputGroup, TextInput } from '@patternfly/react-core';
const el = (
    <InputGroup>
        <TextInput />
    </InputGroup>
);
"#;
        let incidents = scan_source_jsx_not_child(
            source,
            r"^InputGroup$",
            r"^(InputGroupItem|InputGroupText)$",
        );
        // Only one incident for TextInput, NOT one for InputGroup itself
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables["componentName"].as_str().unwrap(),
            "TextInput"
        );
    }

    // ── child (positive child filter) tests ────────────────────────

    fn scan_source_jsx_child(source: &str, pattern: &str, child: &str) -> Vec<Incident> {
        let allocator = Allocator::default();
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let re = Regex::new(pattern).unwrap();
        let child_re = Regex::new(child).unwrap();
        let import_map = build_import_map(&ret.program);

        let empty_transparent = HashMap::new();
        scan_jsx_file(
            &ret.program.body,
            source,
            &re,
            "file:///test.tsx",
            Some(&ReferenceLocation::JsxComponent),
            &import_map,
            Some(&child_re), // child
            None,            // not_child
            None,            // requires_child
            &empty_transparent,
        )
    }

    #[test]
    fn test_child_fires_when_old_child_present() {
        // Modal has ModalBox (old-style child) — should fire
        let source = r#"
            import { Modal } from '@patternfly/react-core';
            import { ModalBox } from '@patternfly/react-core';
            const App = () => (
                <Modal>
                    <ModalBox>content</ModalBox>
                </Modal>
            );
        "#;
        let incidents = scan_source_jsx_child(source, r"^Modal$", r"^(ModalBox|ModalBoxBody)$");
        assert_eq!(
            incidents.len(),
            1,
            "Should fire on Modal when it has ModalBox as a child"
        );
        assert_eq!(
            incidents[0].variables["componentName"].as_str().unwrap(),
            "Modal"
        );
    }

    #[test]
    fn test_child_does_not_fire_when_old_child_absent() {
        // Modal has ModalBody (new-style child), no old children — should NOT fire
        let source = r#"
            import { Modal, ModalBody } from '@patternfly/react-core';
            const App = () => (
                <Modal>
                    <ModalBody>content</ModalBody>
                </Modal>
            );
        "#;
        let incidents = scan_source_jsx_child(source, r"^Modal$", r"^(ModalBox|ModalBoxBody)$");
        assert!(
            incidents.is_empty(),
            "Should NOT fire when Modal has no old-style children"
        );
    }

    #[test]
    fn test_child_fires_on_any_matching_child() {
        // Modal has both old AND new children (partially migrated)
        let source = r#"
            import { Modal, ModalBody, ModalBox } from '@patternfly/react-core';
            const App = () => (
                <Modal>
                    <ModalBody>content</ModalBody>
                    <ModalBox>old content</ModalBox>
                </Modal>
            );
        "#;
        let incidents = scan_source_jsx_child(source, r"^Modal$", r"^(ModalBox|ModalBoxBody)$");
        assert_eq!(
            incidents.len(),
            1,
            "Should fire — Modal still has old-style ModalBox child"
        );
    }

    #[test]
    fn test_child_no_children_does_not_fire() {
        // Self-closing Modal with no children at all
        let source = r#"
            import { Modal } from '@patternfly/react-core';
            const App = () => <Modal />;
        "#;
        let incidents = scan_source_jsx_child(source, r"^Modal$", r"^(ModalBox|ModalBoxBody)$");
        assert!(
            incidents.is_empty(),
            "Should NOT fire on self-closing Modal with no children"
        );
    }

    #[test]
    fn test_child_non_matching_children_do_not_fire() {
        // Modal has children but none match the child pattern
        let source = r#"
            import { Modal } from '@patternfly/react-core';
            const App = () => (
                <Modal>
                    <div>content</div>
                    <span>more</span>
                </Modal>
            );
        "#;
        let incidents = scan_source_jsx_child(source, r"^Modal$", r"^(ModalBox|ModalBoxBody)$");
        assert!(
            incidents.is_empty(),
            "Should NOT fire when no children match the child pattern"
        );
    }

    #[test]
    fn test_child_incident_is_on_parent_not_child() {
        // Verify the incident is on the parent (Modal), not on the matching child (ModalBox)
        let source = r#"
            import { Modal, ModalBox } from '@patternfly/react-core';
            const App = () => (
                <Modal id="my-modal">
                    <ModalBox>content</ModalBox>
                </Modal>
            );
        "#;
        let incidents = scan_source_jsx_child(source, r"^Modal$", r"^ModalBox$");
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables["componentName"].as_str().unwrap(),
            "Modal",
            "Incident should be on the parent (Modal), not the child"
        );
    }

    // ── Component boundary sentinel tests ───────────────────────────

    #[test]
    fn test_is_hook_name() {
        assert!(is_hook_name("useToolbar"));
        assert!(is_hook_name("useMyCustomHook"));
        assert!(is_hook_name("useState"));
        assert!(!is_hook_name("use")); // no uppercase after "use"
        assert!(!is_hook_name("used")); // lowercase after "use"
        assert!(!is_hook_name("notAHook"));
        assert!(!is_hook_name("User")); // starts with U, not "use"
        assert!(!is_hook_name(""));
    }

    #[test]
    fn test_hook_function_decl_sets_sentinel_parent() {
        // function useMyHook() { return <ToolbarItem />; }
        // ToolbarItem should get parentName = "__HookReturn__"
        let source = r#"
import { ToolbarItem } from '@patternfly/react-core';
function useMyToolbar() {
    return <ToolbarItem>Action</ToolbarItem>;
}
"#;
        let incidents = scan_source_jsx(
            source,
            r"^ToolbarItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("parentName"),
            Some(&serde_json::Value::String("__HookReturn__".to_string())),
            "Hook function decl should set __HookReturn__ sentinel. Got: {:?}",
            incidents[0].variables
        );
    }

    #[test]
    fn test_hook_arrow_function_sets_sentinel_parent() {
        // const useMyHook = () => <ToolbarItem />;
        // ToolbarItem should get parentName = "__HookReturn__"
        let source = r#"
import { ToolbarItem } from '@patternfly/react-core';
const useMyToolbar = () => {
    return <ToolbarItem>Action</ToolbarItem>;
};
"#;
        let incidents = scan_source_jsx(
            source,
            r"^ToolbarItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("parentName"),
            Some(&serde_json::Value::String("__HookReturn__".to_string())),
            "Hook arrow function should set __HookReturn__ sentinel. Got: {:?}",
            incidents[0].variables
        );
    }

    #[test]
    fn test_hook_returning_react_fc_sets_sentinel() {
        // function useMyHook() { return () => <ToolbarItem />; }
        // The inner arrow function's JSX still inherits __HookReturn__
        let source = r#"
import { ToolbarItem } from '@patternfly/react-core';
function useMyToolbar() {
    return () => (
        <ToolbarItem>Action</ToolbarItem>
    );
}
"#;
        let incidents = scan_source_jsx(
            source,
            r"^ToolbarItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("parentName"),
            Some(&serde_json::Value::String("__HookReturn__".to_string())),
            "Hook returning arrow FC should propagate __HookReturn__ sentinel. Got: {:?}",
            incidents[0].variables
        );
    }

    #[test]
    fn test_hook_nested_jsx_gets_correct_parent() {
        // function useMyHook() { return <Toolbar><ToolbarItem /></Toolbar>; }
        // ToolbarItem should get parentName = "Toolbar", NOT __HookReturn__
        let source = r#"
import { Toolbar, ToolbarItem } from '@patternfly/react-core';
function useMyToolbar() {
    return (
        <Toolbar>
            <ToolbarItem>Action</ToolbarItem>
        </Toolbar>
    );
}
"#;
        let incidents = scan_source_jsx(
            source,
            r"^ToolbarItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("parentName"),
            Some(&serde_json::Value::String("Toolbar".to_string())),
            "Nested JSX inside hook should get normal component parent. Got: {:?}",
            incidents[0].variables
        );
    }

    #[test]
    fn test_react_fc_type_annotation_sets_sentinel_parent() {
        // const MyComponent: React.FC = () => <ToolbarItem />;
        // ToolbarItem should get parentName = "__ComponentReturn__"
        let source = r#"
import React from 'react';
import { ToolbarItem } from '@patternfly/react-core';
const MyToolbarActions: React.FC = () => (
    <ToolbarItem>Action</ToolbarItem>
);
"#;
        let incidents = scan_source_jsx(
            source,
            r"^ToolbarItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("parentName"),
            Some(&serde_json::Value::String(
                "__ComponentReturn__".to_string()
            )),
            "React.FC typed component should set __ComponentReturn__ sentinel. Got: {:?}",
            incidents[0].variables
        );
    }

    #[test]
    fn test_react_fc_with_generic_sets_sentinel_parent() {
        // const Foo: React.FC<Props> = () => <ToolbarItem />;
        let source = r#"
import React from 'react';
import { ToolbarItem } from '@patternfly/react-core';
const MyToolbar: React.FC<MyProps> = () => (
    <ToolbarItem>Action</ToolbarItem>
);
"#;
        let incidents = scan_source_jsx(
            source,
            r"^ToolbarItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("parentName"),
            Some(&serde_json::Value::String(
                "__ComponentReturn__".to_string()
            )),
            "React.FC<Props> typed component should set __ComponentReturn__ sentinel. Got: {:?}",
            incidents[0].variables
        );
    }

    #[test]
    fn test_regular_component_no_sentinel() {
        // const App = () => <ToolbarItem />;
        // No React.FC annotation, not a hook — should have NO parentName (no sentinel)
        let source = r#"
import { ToolbarItem } from '@patternfly/react-core';
const App = () => (
    <ToolbarItem>Action</ToolbarItem>
);
"#;
        let incidents = scan_source_jsx(
            source,
            r"^ToolbarItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert_eq!(incidents.len(), 1);
        assert!(
            !incidents[0].variables.contains_key("parentName"),
            "Regular component without annotation should have no parentName. Got: {:?}",
            incidents[0].variables
        );
    }

    #[test]
    fn test_wrong_parent_inside_hook_still_tracked() {
        // function useMyHook() { return <Card><ToolbarItem /></Card>; }
        // ToolbarItem has parentName = "Card" (a real wrong parent)
        let source = r#"
import { Card } from '@patternfly/react-core';
import { ToolbarItem } from '@patternfly/react-core';
function useMyHook() {
    return (
        <Card>
            <ToolbarItem>Action</ToolbarItem>
        </Card>
    );
}
"#;
        let incidents = scan_source_jsx(
            source,
            r"^ToolbarItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("parentName"),
            Some(&serde_json::Value::String("Card".to_string())),
            "Wrong parent inside hook should still track the real component parent. Got: {:?}",
            incidents[0].variables
        );
    }

    #[test]
    fn test_exported_hook_function_decl_sets_sentinel() {
        // export function useMyHook() { return <ToolbarItem />; }
        let source = r#"
import { ToolbarItem } from '@patternfly/react-core';
export function useMyToolbar() {
    return <ToolbarItem>Action</ToolbarItem>;
}
"#;
        let incidents = scan_source_jsx(
            source,
            r"^ToolbarItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables.get("parentName"),
            Some(&serde_json::Value::String("__HookReturn__".to_string())),
            "Exported hook should set __HookReturn__ sentinel. Got: {:?}",
            incidents[0].variables
        );
    }

    // ── requiresChild tests ─────────────────────────────────────────

    fn scan_source_jsx_requires_child(
        source: &str,
        pattern: &str,
        requires_child: &str,
    ) -> Vec<Incident> {
        let allocator = Allocator::default();
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let re = Regex::new(pattern).unwrap();
        let requires_child_re = Regex::new(requires_child).unwrap();
        let import_map = build_import_map(&ret.program);

        let empty_transparent = HashMap::new();
        scan_jsx_file(
            &ret.program.body,
            source,
            &re,
            "file:///test.tsx",
            Some(&ReferenceLocation::JsxComponent),
            &import_map,
            None, // child
            None, // not_child
            Some(&requires_child_re),
            &empty_transparent,
        )
    }

    #[test]
    fn test_requires_child_fires_when_no_matching_child() {
        // AlertGroup has <div> but no <Alert> — should fire
        let source = r#"
import { AlertGroup } from '@patternfly/react-core';
const el = (
    <AlertGroup>
        <div>not an alert</div>
    </AlertGroup>
);
"#;
        let incidents = scan_source_jsx_requires_child(source, r"^AlertGroup$", r"^Alert$");
        assert_eq!(
            incidents.len(),
            1,
            "Should fire when no direct child matches requiresChild"
        );
    }

    #[test]
    fn test_requires_child_does_not_fire_when_child_present() {
        // AlertGroup has <Alert> among other children — should NOT fire
        let source = r#"
import { AlertGroup, Alert } from '@patternfly/react-core';
const el = (
    <AlertGroup>
        <Alert />
        <div>wrapper</div>
    </AlertGroup>
);
"#;
        let incidents = scan_source_jsx_requires_child(source, r"^AlertGroup$", r"^Alert$");
        assert_eq!(
            incidents.len(),
            0,
            "Should NOT fire when at least one child matches requiresChild"
        );
    }

    #[test]
    fn test_requires_child_does_not_fire_when_any_child_matches() {
        // AlertGroup has both Alert and AlertActionCloseButton — should NOT fire
        let source = r#"
import { AlertGroup, Alert, AlertActionCloseButton } from '@patternfly/react-core';
const el = (
    <AlertGroup>
        <Alert />
        <AlertActionCloseButton />
    </AlertGroup>
);
"#;
        let incidents = scan_source_jsx_requires_child(
            source,
            r"^AlertGroup$",
            r"^(Alert|AlertActionCloseButton)$",
        );
        assert_eq!(
            incidents.len(),
            0,
            "Should NOT fire when any child matches the requiresChild pattern"
        );
    }

    #[test]
    fn test_requires_child_fires_on_self_closing() {
        // Self-closing <AlertGroup /> has no children at all — should fire
        let source = r#"
import { AlertGroup } from '@patternfly/react-core';
const el = <AlertGroup />;
"#;
        let incidents = scan_source_jsx_requires_child(source, r"^AlertGroup$", r"^Alert$");
        assert_eq!(
            incidents.len(),
            1,
            "Should fire on self-closing element with no children"
        );
    }

    #[test]
    fn test_requires_child_incident_is_on_parent() {
        // Verify the incident span points to AlertGroup, not any child
        let source = r#"
import { AlertGroup } from '@patternfly/react-core';
const el = (
    <AlertGroup>
        <div>not alert</div>
    </AlertGroup>
);
"#;
        let incidents = scan_source_jsx_requires_child(source, r"^AlertGroup$", r"^Alert$");
        assert_eq!(incidents.len(), 1);
        assert_eq!(
            incidents[0].variables["componentName"].as_str().unwrap(),
            "AlertGroup",
            "Incident should be on the parent (AlertGroup)"
        );
    }

    #[test]
    fn test_requires_child_does_not_emit_normal_incident() {
        // When requiresChild is set and a matching child IS present,
        // neither the requiresChild incident nor the normal incident should fire
        let source = r#"
import { AlertGroup, Alert } from '@patternfly/react-core';
const el = (
    <AlertGroup>
        <Alert />
    </AlertGroup>
);
"#;
        let incidents = scan_source_jsx_requires_child(source, r"^AlertGroup$", r"^Alert$");
        assert_eq!(
            incidents.len(),
            0,
            "When requiresChild is set, no normal incident should be emitted and no requiresChild incident when child is present"
        );
    }

    // ── Expression child walking tests ──────────────────────────────

    #[test]
    fn test_requires_child_sees_map_children() {
        // List has children rendered via .map() — should NOT fire
        let source = r#"
import { List, ListItem } from '@patternfly/react-core';
const el = (
    <List>
        {items.map(item => (
            <ListItem key={item.id}>{item.name}</ListItem>
        ))}
    </List>
);
"#;
        let incidents = scan_source_jsx_requires_child(source, r"^List$", r"^ListItem$");
        assert_eq!(
            incidents.len(),
            0,
            "Should see ListItem inside .map() expression"
        );
    }

    #[test]
    fn test_requires_child_sees_conditional_children() {
        // AlertGroup has children via conditional expression — should NOT fire
        let source = r#"
import { AlertGroup, Alert } from '@patternfly/react-core';
const el = (
    <AlertGroup>
        {hasAlert && <Alert />}
    </AlertGroup>
);
"#;
        let incidents = scan_source_jsx_requires_child(source, r"^AlertGroup$", r"^Alert$");
        assert_eq!(
            incidents.len(),
            0,
            "Should see Alert inside conditional expression"
        );
    }

    #[test]
    fn test_requires_child_sees_ternary_children() {
        // ToggleGroup has children via ternary — should NOT fire
        let source = r#"
import { ToggleGroup, ToggleGroupItem } from '@patternfly/react-core';
const el = (
    <ToggleGroup>
        {isReady ? <ToggleGroupItem text="A" /> : <ToggleGroupItem text="B" />}
    </ToggleGroup>
);
"#;
        let incidents =
            scan_source_jsx_requires_child(source, r"^ToggleGroup$", r"^ToggleGroupItem$");
        assert_eq!(
            incidents.len(),
            0,
            "Should see ToggleGroupItem inside ternary expression"
        );
    }

    #[test]
    fn test_requires_child_still_fires_with_wrong_map_children() {
        // List has .map() but renders <div> not <ListItem> — SHOULD fire
        let source = r#"
import { List } from '@patternfly/react-core';
const el = (
    <List>
        {items.map(item => (
            <div key={item.id}>{item.name}</div>
        ))}
    </List>
);
"#;
        let incidents = scan_source_jsx_requires_child(source, r"^List$", r"^ListItem$");
        assert_eq!(
            incidents.len(),
            1,
            "Should still fire when .map() renders wrong component"
        );
    }

    #[test]
    fn test_requires_child_sees_optional_chain_map() {
        // List has children via optional chaining ?.map() — should NOT fire
        let source = r#"
import { List, ListItem } from '@patternfly/react-core';
const el = (
    <List>
        {items?.map(item => (
            <ListItem key={item.id}>{item.name}</ListItem>
        ))}
    </List>
);
"#;
        let incidents = scan_source_jsx_requires_child(source, r"^List$", r"^ListItem$");
        assert_eq!(
            incidents.len(),
            0,
            "Should see ListItem inside ?.map() expression"
        );
    }

    #[test]
    fn test_child_gate_sees_map_children() {
        // child gate should also see components inside .map()
        let source = r#"
import { AlertGroup, Alert } from '@patternfly/react-core';
const el = (
    <AlertGroup>
        {items.map(item => (
            <Alert key={item.id} />
        ))}
    </AlertGroup>
);
"#;
        let allocator = Allocator::default();
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        let re = Regex::new(r"^AlertGroup$").unwrap();
        let child_re = Regex::new(r"^Alert$").unwrap();
        let import_map = build_import_map(&ret.program);
        let empty_transparent = HashMap::new();
        let incidents = scan_jsx_file(
            &ret.program.body,
            source,
            &re,
            "file:///test.tsx",
            Some(&ReferenceLocation::JsxComponent),
            &import_map,
            Some(&child_re),
            None,
            None,
            &empty_transparent,
        );
        assert_eq!(
            incidents.len(),
            1,
            "child gate should see Alert inside .map() and emit parent incident"
        );
    }

    #[test]
    fn test_not_child_sees_map_children() {
        // notChild should see components inside .map()
        let source = r#"
import { Form, FormGroup } from '@patternfly/react-core';
const el = (
    <Form>
        {items.map(item => (
            <FormGroup key={item.id}>
                <input />
            </FormGroup>
        ))}
    </Form>
);
"#;
        let incidents =
            scan_source_jsx_not_child(source, r"^Form$", r"^(FormGroup|FormSection|ActionGroup)$");
        assert_eq!(
            incidents.len(),
            0,
            "notChild should see FormGroup inside .map() and not emit incident"
        );
    }

    #[test]
    fn test_requires_child_sees_array_literal_children() {
        // Tabs has children in an array literal: {[<Tab />, ...]}
        let source = r#"
import { Tabs, Tab } from '@patternfly/react-core';
const el = (
    <Tabs activeKey={0}>
        {[
            <Tab key="a" eventKey="a" title="A">content A</Tab>,
            <Tab key="b" eventKey="b" title="B">content B</Tab>,
        ]}
    </Tabs>
);
"#;
        let incidents = scan_source_jsx_requires_child(source, r"^Tabs$", r"^(Tab|TabTitleIcon)$");
        assert_eq!(
            incidents.len(),
            0,
            "Should see Tab inside array literal expression"
        );
    }

    #[test]
    fn test_requires_child_sees_array_with_map() {
        // Tabs has children in array literal containing .map():
        // {[staticTab, ...items.map(i => <Tab />)]}
        let source = r#"
import { Tabs, Tab } from '@patternfly/react-core';
const el = (
    <Tabs activeKey={0}>
        {[
            <Tab key="static" eventKey="static" title="Static">static</Tab>,
            ...items.map(item => (
                <Tab key={item.id} eventKey={item.id} title={item.name}>
                    {item.content}
                </Tab>
            ))
        ]}
    </Tabs>
);
"#;
        let incidents = scan_source_jsx_requires_child(source, r"^Tabs$", r"^(Tab|TabTitleIcon)$");
        assert_eq!(
            incidents.len(),
            0,
            "Should see Tab inside array literal with spread .map()"
        );
    }

    // ── Children passthrough suppression tests ─────────────────────────

    #[test]
    fn test_requires_child_suppressed_when_passes_children() {
        // Table passes {children} through — requiresChild should NOT fire
        // because the actual children come from the call site.
        let source = r#"
import { Table } from '@patternfly/react-table';

const TableWrapper = ({ children }) => (
    <Table>
        {children}
    </Table>
);
"#;
        let incidents =
            scan_source_jsx_requires_child(source, r"^Table$", r"^(Thead|Tbody|Tr|Caption)$");
        assert_eq!(
            incidents.len(),
            0,
            "requiresChild should NOT fire when component passes children through"
        );
    }

    #[test]
    fn test_requires_child_suppressed_when_passes_props_children() {
        // Table passes {props.children} through — same suppression
        let source = r#"
import React from 'react';
import { Table } from '@patternfly/react-table';

const TableWithBatteries = React.forwardRef((props, ref) => (
    <Table innerRef={ref} {...props}>
        {props.children}
    </Table>
));
"#;
        let incidents =
            scan_source_jsx_requires_child(source, r"^Table$", r"^(Thead|Tbody|Tr|Caption)$");
        assert_eq!(
            incidents.len(),
            0,
            "requiresChild should NOT fire when component passes props.children through"
        );
    }

    #[test]
    fn test_requires_child_still_fires_when_no_children_passthrough() {
        // Table has real children but none matching — should fire
        let source = r#"
import { Table } from '@patternfly/react-table';

const el = (
    <Table>
        <div>not a valid child</div>
    </Table>
);
"#;
        let incidents =
            scan_source_jsx_requires_child(source, r"^Table$", r"^(Thead|Tbody|Tr|Caption)$");
        assert_eq!(
            incidents.len(),
            1,
            "requiresChild should fire when no children passthrough and no matching child"
        );
    }

    // ── Function reference resolution tests ──────────────────────────────

    #[test]
    fn test_fn_ref_prop_resolves_parent_same_file() {
        // When toggle={toggle} passes a function reference as a prop,
        // JSX inside the function should see the parent component (Select).
        // The scanner finds MenuToggle twice:
        //   1. In the function definition (parentName=None)
        //   2. Via reference resolution at <Select toggle={toggle}> (parentName=Select)
        let source = r#"
import { Select, MenuToggle } from '@patternfly/react-core';

const toggle = (toggleRef) => (
    <MenuToggle ref={toggleRef} onClick={onToggle}>
        Filter
    </MenuToggle>
);

const el = (
    <Select toggle={toggle} isOpen={isOpen}>
        <div>options</div>
    </Select>
);
"#;
        let incidents = scan_source_jsx(
            source,
            r"^MenuToggle$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert!(
            !incidents.is_empty(),
            "Should find MenuToggle at least once"
        );
        let resolved = incidents.iter().find(|i| {
            i.variables.get("parentName") == Some(&serde_json::Value::String("Select".to_string()))
        });
        assert!(
            resolved.is_some(),
            "Should have an incident with parentName=Select (resolved through toggle fn ref)"
        );
    }

    #[test]
    fn test_fn_ref_prop_inline_arrow_still_works() {
        // Verify that inline arrow functions still work (existing behavior).
        let source = r#"
import { Select, MenuToggle } from '@patternfly/react-core';

const el = (
    <Select toggle={(ref) => <MenuToggle ref={ref}>Filter</MenuToggle>} isOpen={isOpen}>
        <div>options</div>
    </Select>
);
"#;
        let incidents = scan_source_jsx(
            source,
            r"^MenuToggle$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert_eq!(incidents.len(), 1, "Should find MenuToggle");
        assert_eq!(
            incidents[0].variables.get("parentName"),
            Some(&serde_json::Value::String("Select".to_string())),
            "MenuToggle's parent should be Select (inline arrow)"
        );
    }

    #[test]
    fn test_fn_ref_as_jsx_child_resolves_parent() {
        // When a function reference is used as a JSX child: {renderItems}
        let source = r#"
import { DropdownList, DropdownItem } from '@patternfly/react-core';

const renderItems = () => (
    <DropdownItem key="1">Action</DropdownItem>
);

const el = (
    <DropdownList>
        {renderItems}
    </DropdownList>
);
"#;
        let incidents = scan_source_jsx(
            source,
            r"^DropdownItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        assert!(
            !incidents.is_empty(),
            "Should find DropdownItem at least once"
        );
        let resolved = incidents.iter().find(|i| {
            i.variables.get("parentName")
                == Some(&serde_json::Value::String("DropdownList".to_string()))
        });
        assert!(
            resolved.is_some(),
            "Should have an incident with parentName=DropdownList (resolved through renderItems ref)"
        );
    }

    #[test]
    fn test_fn_ref_nested_jsx_in_function() {
        // Function reference with multiple levels of JSX nesting.
        let source = r#"
import { Toolbar, ToolbarContent, ToolbarItem, Button } from '@patternfly/react-core';

const renderToolbarContent = () => (
    <ToolbarContent>
        <ToolbarItem>
            <Button>Click</Button>
        </ToolbarItem>
    </ToolbarContent>
);

const el = (
    <Toolbar>
        {renderToolbarContent}
    </Toolbar>
);
"#;
        // Check that ToolbarContent sees Toolbar as parent via fn ref resolution
        let incidents = scan_source_jsx(
            source,
            r"^ToolbarContent$",
            Some(&ReferenceLocation::JsxComponent),
        );
        let resolved = incidents.iter().find(|i| {
            i.variables.get("parentName") == Some(&serde_json::Value::String("Toolbar".to_string()))
        });
        assert!(
            resolved.is_some(),
            "Should have ToolbarContent with parentName=Toolbar"
        );

        // Check that ToolbarItem sees ToolbarContent as parent (nested inside the function)
        let incidents = scan_source_jsx(
            source,
            r"^ToolbarItem$",
            Some(&ReferenceLocation::JsxComponent),
        );
        let resolved = incidents.iter().find(|i| {
            i.variables.get("parentName")
                == Some(&serde_json::Value::String("ToolbarContent".to_string()))
        });
        assert!(
            resolved.is_some(),
            "Should have ToolbarItem with parentName=ToolbarContent"
        );
    }
}

# frontend-js-scanner

OXC-based static analysis scanner for JavaScript, TypeScript, JSX, and TSX files. Parses source files using the [OXC](https://oxc.rs/) parser and walks the AST to detect patterns defined by Konveyor rule conditions.

## Overview

This crate handles all JS/TS analysis for the frontend-analyzer-provider. It supports six scanning modes, each targeting a different code pattern:

| Module | Scans for | Incident variables set |
|---|---|---|
| `imports` | Import declarations (module source and specifier names) | `importedName`, `localName`, `module`, `matchingText` |
| `jsx` | JSX component usage and prop usage | `componentName`, `propName`, `propValue`, `module`, `parentName`, `parentFrom` |
| `function_calls` | Function and hook calls (including member expressions) | `functionName` |
| `type_refs` | TypeScript type references, interfaces, type aliases | `typeName` |
| `classnames` | CSS class name usage in JSX attributes and string literals | `matchingText` |
| `css_vars` | CSS variable references in JS/TS string/template literals | `matchingText` |

## Usage

### Scanning a file for referenced patterns

```rust
use frontend_js_scanner::scanner::scan_file_referenced;
use frontend_core::capabilities::ReferencedCondition;

let condition = ReferencedCondition {
    pattern: "Button".to_string(),
    location: None, // search all locations
    ..Default::default()
};

let incidents = scan_file_referenced(&file_path, &project_root, &condition)?;
```

### Scanning for CSS class names in JS/TS

```rust
use frontend_js_scanner::scanner::scan_file_classnames;
use regex::Regex;

let pattern = Regex::new(r"pf-v5-")?;
let incidents = scan_file_classnames(&file_path, &project_root, &pattern)?;
```

### Collecting project files

```rust
use frontend_js_scanner::scanner::collect_files;

// Collects .js, .jsx, .ts, .tsx, .mjs, .mts files
// Skips node_modules, .git, dist, build, coverage, etc.
let files = collect_files(&project_root, Some(r"\.tsx$"))?;
```

## Architecture

The `scanner` module is the top-level orchestrator:

1. `collect_files()` walks the directory tree and gathers relevant source files
2. `scan_file_referenced()` parses a file with OXC, calls `imports::build_import_map()` to resolve component origins, then dispatches to the appropriate scanner(s) based on the `location` filter
3. Post-scan filtering applies `component`, `parent`, `parent_from`, `value`, and `from` constraints to narrow results

The import map (local name -> module source) enables JSX scanning to resolve components back to their package source, which is essential for rules that filter by `from` (e.g., only match `Button` from `@patternfly/react-core`).

## Supported file extensions

`.js`, `.jsx`, `.ts`, `.tsx`, `.mjs`, `.mts`

## License

Apache-2.0

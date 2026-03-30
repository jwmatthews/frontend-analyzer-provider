# frontend-css-scanner

CSS and SCSS static analysis scanner for the frontend-analyzer-provider. Parses CSS files using [LightningCSS](https://lightningcss.dev/) for structured selector analysis, with a regex-based fallback for SCSS/LESS/SASS files.

## Overview

This crate searches CSS stylesheets for two pattern types:

| Scan type | Function | Description |
|---|---|---|
| Class selectors | `scan_css_file_classes()` | Finds CSS class selectors matching a regex pattern |
| CSS variables | `scan_css_file_vars()` | Finds CSS custom property declarations and `var()` usages |

## Usage

### Scanning for CSS class selectors

```rust
use frontend_css_scanner::scanner::scan_css_file_classes;
use regex::Regex;

let pattern = Regex::new(r"pf-v5-")?;
let incidents = scan_css_file_classes(&file_path, &project_root, &pattern)?;
// Each incident sets a `className` or `matchingText` variable
```

### Scanning for CSS variables

```rust
use frontend_css_scanner::scanner::scan_css_file_vars;
use regex::Regex;

let pattern = Regex::new(r"--pf-v5-")?;
let incidents = scan_css_file_vars(&file_path, &project_root, &pattern)?;
// Each incident sets a `variableName` variable
```

### Collecting CSS files

```rust
use frontend_css_scanner::scanner::collect_css_files;

// Collects .css, .scss, .less, .sass files
// Skips node_modules, .git, dist, build, etc.
let files = collect_css_files(&project_root, None)?;
```

## Architecture

```
scanner (dispatcher)
  |
  +-- .css files --> selectors (LightningCSS parser)
  |                     |
  |                     +-- fallback on parse error --> scss_fallback (regex)
  |
  +-- .scss/.less/.sass files --> scss_fallback (regex)
  |
  +-- CSS variables (all formats) --> scss_fallback (regex)
```

- **`selectors`** -- Parses CSS with LightningCSS and walks style rules, `@media`, `@supports`, and `@layer` blocks to find class selectors matching the pattern
- **`scss_fallback`** -- Line-by-line regex scanning for class selectors (`.className`) and CSS variable references (`--var-name`, `var(--var-name)`)
- **`variables`** -- Delegates unconditionally to `scss_fallback` because regex is more reliable than LightningCSS's typed model for CSS custom properties

## Supported file extensions

`.css`, `.scss`, `.less`, `.sass`

## License

Apache-2.0

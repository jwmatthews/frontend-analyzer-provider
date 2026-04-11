# frontend-core

Shared types and domain model for the frontend-analyzer-provider workspace. Every other crate in the workspace depends on this one.

## Overview

This crate defines two primary domains:

- **Capabilities** -- what the provider can search for (imports, JSX components, CSS classes, etc.)
- **Fix strategies** -- how detected incidents are planned and applied as code edits

It also re-exports key types from `konveyor-core` so downstream crates can use shared Konveyor ecosystem types without adding a direct dependency.

## Modules

### `capabilities`

Defines the provider's analysis capabilities and condition types used in Konveyor rule evaluation.

| Capability | Condition Type | Description |
|---|---|---|
| `referenced` | `ReferencedCondition` | JS/TS imports, JSX components/props, function calls, type references |
| `cssclass` | `CssClassCondition` | CSS class selector usage |
| `cssvar` | `CssVarCondition` | CSS custom property usage |
| `dependency` | `DependencyCondition` | package.json dependency version checks |

```rust
use frontend_core::capabilities::{ProviderCondition, CAPABILITIES};

// Parse a YAML condition string into a typed condition
let condition = ProviderCondition::parse("referenced", condition_yaml)?;
```

The `ReferencedCondition` supports filtering by `location` (`Import`, `JsxComponent`, `JsxProp`, `FunctionCall`, `TypeReference`), `component`, `parent`, `parent_from`, `value`, `from`, and `file_pattern`.

### `fix`

Types for fix planning, execution, and strategy resolution.

**Key types:**

| Type | Purpose |
|---|---|
| `FixStrategy` | Runtime fix strategy enum (Rename, RemoveProp, ImportPathChange, CssVariablePrefix, EnsureDependency, Manual, Llm) |
| `FixPlan` | Groups `PlannedFix` items by file, plus `manual` and `pending_llm` lists |
| `PlannedFix` | Collection of `TextEdit` items for a single incident |
| `TextEdit` | A single line-level text replacement (line, old_text, new_text) |
| `FixResult` | Summary of applied edits (files modified, edits applied/skipped, errors) |

```rust
use frontend_core::fix::{load_strategies_from_json, strategy_entry_to_fix_strategy};

// Load strategies from a fix-strategies.json file
let strategies = load_strategies_from_json(Path::new("fix-strategies.json"))?;
```

**Re-exports from `konveyor-core`:** `FixConfidence`, `FixSource`, `FixStrategyEntry`, `StrategyMappingEntry`, `MemberMappingEntry`.

## Re-exports

```rust
pub use konveyor_core::incident;  // Incident, Location, Position
pub use konveyor_core::report;    // RuleSet, analysis output types
pub use konveyor_core::rule;      // Rule definition types
pub use konveyor_core::fix as shared_fix;
```

## License

Apache-2.0

# frontend-fix-engine

Fix engine for planning and applying pattern-based and LLM-assisted code migration fixes. Given Konveyor analysis output (rulesets with violations), this crate generates text edits and either applies them deterministically or delegates complex structural changes to an LLM.

## Overview

The fix engine operates in two phases:

1. **Plan** -- Converts analysis violations into a `FixPlan` containing deterministic text edits, LLM-pending requests, and manual review items
2. **Apply** -- Executes the planned edits on disk, optionally invoking LLM backends for complex fixes

## Fix Strategies

| Strategy | Type | Description |
|---|---|---|
| `Rename` | Pattern | Multi-mapping rename (e.g., `Chip` -> `Label`, `ChipGroup` -> `LabelGroup`) |
| `RemoveProp` | Pattern | JSX prop removal with bracket balancing for multi-line props |
| `ImportPathChange` | Pattern | Module import path rewriting |
| `CssVariablePrefix` | Pattern | CSS class/variable prefix swap (e.g., `pf-v5-` -> `pf-v6-`) |
| `UpdateDependency` | Pattern | package.json dependency version update |
| `Llm` | LLM | Delegates to an LLM backend for complex structural changes |
| `Manual` | Manual | Flags for human review when no automated fix is possible |

## Usage

### Planning fixes

```rust
use frontend_fix_engine::engine::plan_fixes;
use frontend_core::fix::load_strategies_from_json;

let strategies = load_strategies_from_json(Path::new("fix-strategies.json"))?;
let plan = plan_fixes(&analysis_output, &project_root, &strategies)?;

// plan.files      -- pattern-based edits grouped by file
// plan.pending_llm -- incidents requiring LLM assistance
// plan.manual      -- incidents requiring human review
```

### Applying pattern-based fixes

```rust
use frontend_fix_engine::engine::apply_fixes;

let result = apply_fixes(&plan)?;
println!("{} files modified, {} edits applied", result.files_modified, result.edits_applied);
```

### Previewing changes (dry run)

```rust
use frontend_fix_engine::engine::preview_fixes;

let diff = preview_fixes(&plan)?;
println!("{}", diff); // unified diff format
```

### LLM-assisted fixes (OpenAI-compatible API)

```rust
use frontend_fix_engine::llm_client::request_llm_fix;

let fixes = request_llm_fix(endpoint, &llm_request, &context).await?;
```

### LLM-assisted fixes (Goose CLI)

```rust
use frontend_fix_engine::goose_client::run_all_goose_fixes;

let results = run_all_goose_fixes(&pending_requests, &context, verbose, log_dir);
```

Goose fixes run up to 3 files concurrently, with 120-second timeouts and process group isolation for clean cleanup.

## FixContext Trait

The `FixContext` trait lets framework-specific crates customize LLM prompts:

```rust
use frontend_fix_engine::context::FixContext;

pub trait FixContext: Send + Sync {
    fn ruleset_name(&self) -> &str;
    fn migration_description(&self) -> &str;
    fn llm_constraints(&self) -> &[String];
    fn revert_warnings(&self) -> Option<&str> { None }
    fn fix_priority(&self, rule_id: &str) -> u8 { 3 }
    fn llm_system_prompt(&self) -> String { /* default impl */ }
}
```

Register implementations via `FixContextRegistry` for runtime lookup by ruleset name.

## Strategy Resolution Order

When planning a fix for an incident:

1. Explicit strategy from `fix-strategies.json` (keyed by rule ID)
2. Inferred strategy from rule labels (e.g., `konveyor.io/fix=prop-removal` -> `RemoveProp`)
3. Fallback to `FixStrategy::Llm`

## License

Apache-2.0

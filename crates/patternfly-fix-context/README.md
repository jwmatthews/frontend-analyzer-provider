# patternfly-fix-context

PatternFly v5-to-v6 migration context for the fix engine's LLM-assisted fixes. Implements the `FixContext` trait with PatternFly-specific constraints, priority ordering, and prompt guidance.

## Overview

This crate provides the `PatternFlyV5ToV6Context` struct, which customizes how the fix engine generates LLM prompts when processing the `konveyor-patternfly-v5-to-v6` ruleset. It is registered in a `FixContextRegistry` at startup and looked up automatically when the analysis output contains PatternFly migration rules.

## Usage

```rust
use patternfly_fix_context::PatternFlyV5ToV6Context;
use frontend_fix_engine::registry::FixContextRegistry;

let mut registry = FixContextRegistry::new();
registry.register(Box::new(PatternFlyV5ToV6Context::new()));

// Later, when processing analysis output:
let ctx = registry.get("konveyor-patternfly-v5-to-v6");
// Returns the PatternFly context (or generic fallback for unknown rulesets)
```

## LLM Constraints

The context provides 8 PatternFly-specific constraints that are injected into every LLM prompt:

- Never use deep/internal PatternFly imports
- Never replace PatternFly components with raw HTML elements
- Never remove `data-testid` or other test identifiers
- Preserve existing test coverage patterns
- And others specific to the v5-to-v6 migration

## Fix Priority Ordering

Rules are sorted by priority before being sent to the LLM, ensuring the most impactful changes are addressed first:

| Priority | Rule category |
|---|---|
| 0 | Hierarchy/composition changes |
| 1 | Deprecated import path moves |
| 2 | Component composition changes |
| 3 | Prop changes (default) |
| 4 | DOM structure, CSS, accessibility |
| 5 | Conformance/best-practice |

## Constants

- `PATTERNFLY_RULESET_NAME` -- `"konveyor-patternfly-v5-to-v6"`

## License

Apache-2.0

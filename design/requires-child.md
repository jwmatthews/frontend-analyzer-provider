# `requiresChild` — Composition Conformance Field

## Problem

The semver-analyzer generates composition trees for component families (e.g.,
PatternFly's `AlertGroup -> Alert`, `Table -> Tbody -> Tr -> Td`). These trees
produce conformance rules that validate consumer JSX structure.

Two rule directions exist:

1. **`notParent`** (already implemented): "Child must be inside Parent." Fires
   when a child component is used outside its required parent. Works for
   components nested below the family root.

   ```yaml
   # Td must be inside Tr
   pattern: ^Td$
   location: JSX_COMPONENT
   notParent: ^(Tr)$
   ```

2. **`requiresChild`** (proposed): "Parent must contain Child." Fires when a
   parent component is used but does not contain any of its required children.
   Needed for family roots and secondary roots — components that can exist
   standalone but, when used, must contain specific children.

   ```yaml
   # AlertGroup must contain Alert
   pattern: ^AlertGroup$
   location: JSX_COMPONENT
   requiresChild: ^(Alert|AlertActionCloseButton)$
   ```

### Why `child` and `notChild` don't solve this

| Field | Semantics | Use Case |
|-------|-----------|----------|
| `child` | Gate: only emit parent incident IF a matching child exists | Migration: detect old-style children still present |
| `notChild` | Per-child: emit incident for each child NOT matching | Exclusive wrapper: all children must be a specific type |
| `requiresChild` | Existence: emit parent incident if NO matching child exists | Conformance: parent must contain required children |

`child` is the inverse of what we need — it fires when the child IS present.
`requiresChild` fires when the child is NOT present.

`notChild` fires per non-matching child element, which is wrong for this use
case. An `AlertGroup` can contain both `Alert` and other elements (e.g., `div`
wrappers); the constraint is that at least one `Alert` must exist, not that all
children must be `Alert`.

## Proposal

Add a `requiresChild` field to `ReferencedCondition` (in
`crates/core/src/capabilities.rs`). When set alongside
`location: JSX_COMPONENT`, it acts as a negative existence gate: the parent
incident is emitted only when **no** direct JSX child matches the
`requiresChild` regex.

### Field Definition

```rust
/// Negative-existence child filter for JSX_COMPONENT location.
/// When set, matches the component specified by `pattern` and emits an
/// incident if NONE of its direct JSX children match this regex.
///
/// Used for conformance rules like "AlertGroup must contain Alert" —
/// fires when AlertGroup has no Alert children.
///
/// Inverse of `child` (which gates on existence). Complementary to
/// `notChild` (which fires per non-matching child).
#[serde(rename = "requiresChild", skip_serializing_if = "Option::is_none")]
pub requires_child: Option<String>,
```

### YAML Syntax

```yaml
- ruleID: sd-conformance-alertgroup-requires-children
  when:
    frontend.referenced:
      pattern: ^AlertGroup$
      location: JSX_COMPONENT
      requiresChild: ^(Alert|AlertActionCloseButton)$
      from: "@patternfly/react-core"
  category: mandatory
  description: "AlertGroup must contain Alert children"
  message: |
    <AlertGroup> must contain at least one <Alert> or
    <AlertActionCloseButton> child component.
```

### Scanner Implementation

#### Phase 1: Regex compilation (`scanner.rs`)

Add alongside existing `child_re` / `not_child_re` compilation:

```rust
let requires_child_re = condition.requires_child.as_deref()
    .map(Regex::new).transpose()?;
```

Pass to `scan_jsx_file` as a new parameter.

#### Phase 2: Add to `ScanContext` (`jsx.rs`)

```rust
struct ScanContext<'a, 'b> {
    // ... existing fields ...
    /// When set, matches the parent component (via `pattern`) and emits an
    /// incident if NONE of its direct JSX children match this regex.
    requires_child: Option<&'b Regex>,
}
```

#### Phase 3: Matching logic in `check_jsx_element` (`jsx.rs`)

After the existing `child` gate and before the incident push at line 904:

```rust
// requiresChild: emit incident if NO direct child matches.
// This is the inverse of `child` — fires on absence, not presence.
let requires_child_passed = if let Some(req_re) = ctx.requires_child {
    // Fire if no direct JSX child matches the required pattern
    !el.children.iter().any(|c| {
        if let JSXChild::Element(child_el) = c {
            let child_name = jsx_element_name_to_string(
                &child_el.opening_element.name
            );
            req_re.is_match(&child_name)
        } else {
            false
        }
    })
} else {
    false // no requiresChild filter — don't emit
};

// Emit for requiresChild violations
if requires_child_passed {
    ctx.incidents.push(incident.clone());
}
```

Note: `requiresChild` emits independently of the `child`/`notChild` gates. If
only `requiresChild` is set (no `child`, no `notChild`), the normal incident
(line 904) should NOT be emitted — only the `requiresChild` incident. The
condition at line 904 should be updated:

```rust
if child_gate_passed && ctx.not_child.is_none() && ctx.requires_child.is_none() {
    ctx.incidents.push(incident);
}
```

This ensures the three child-related fields are mutually exclusive in their
incident emission:

- `child` only: incident on parent when matching child exists
- `notChild` only: incident per non-matching child
- `requiresChild` only: incident on parent when no matching child exists

#### Phase 4: Interaction with other filters

`requiresChild` should compose with existing post-scan filters:

| Filter | Interaction |
|--------|-------------|
| `from` | Only match parent imported from this package |
| `parent` / `notParent` | Apply after `requiresChild` — filter by the grandparent of the matched component |
| `parentFrom` | Apply after — filter by grandparent's import source |
| `filePattern` | Apply before — only scan matching files |

No special interactions needed. The existing post-scan filter pipeline in
`scanner.rs` (lines 210-340) applies after incident collection and works
identically.

### Incident Variables

The emitted incident should include:

| Variable | Value | Purpose |
|----------|-------|---------|
| `componentName` | The matched parent component name (e.g., `AlertGroup`) | Rule message interpolation |
| `module` | The parent's import source | Package scoping |
| `parentName` | The parent's parent (if any) | Context |
| `parentFrom` | The parent's parent import source | Context |

No `childName` variable is emitted since the point is that the required child
is **absent**.

### Konveyor-Core Changes

Add the field to `FrontendReferencedFields` in `konveyor-core/src/rule.rs`:

```rust
/// Negative-existence child filter: match the component (via `pattern`)
/// and emit an incident if NONE of its direct JSX children match this
/// regex. Used for conformance rules like "AlertGroup must contain Alert."
#[serde(rename = "requiresChild", skip_serializing_if = "Option::is_none", default)]
pub requires_child: Option<String>,
```

### Semver-Analyzer Rule Generation Changes

In `konveyor_v2.rs::generate_conformance_rules()`, the current algorithm
generates `notParent` rules for all Required edges. With `requiresChild`, the
algorithm becomes:

```
parent_to_required_children: HashMap<parent, Vec<child>>
child_to_all_parents: HashMap<child, Vec<parent>>
no_incoming: HashSet<member>   // members with zero incoming non-internal edges

for (parent, children) in parent_to_required_children:
    if parent in no_incoming:
        -> requiresChild rule on parent
          pattern: ^{parent}$
          requiresChild: ^({children joined by |})$
    else:
        -> notParent rule on each child (grouped by child)
          pattern: ^{child}$
          notParent: ^({all_parents joined by |})$
```

The `no_incoming` set naturally captures both tree roots and secondary roots —
any component with no incoming non-internal edges is either the family root or
an independent container. These components can exist standalone; the constraint
is on what they must contain, not where they must be placed.

## Test Plan

### Unit tests (`jsx.rs`)

1. **`test_requires_child_fires_when_no_matching_child`** —
   `<AlertGroup><div /></AlertGroup>` with `requiresChild: ^Alert$` → 1
   incident on AlertGroup
2. **`test_requires_child_does_not_fire_when_child_present`** —
   `<AlertGroup><Alert /><div /></AlertGroup>` → 0 incidents
3. **`test_requires_child_does_not_fire_when_any_child_matches`** —
   `<AlertGroup><Alert /><AlertActionCloseButton /></AlertGroup>` with
   `requiresChild: ^(Alert|AlertActionCloseButton)$` → 0 incidents
4. **`test_requires_child_fires_on_self_closing`** — `<AlertGroup />` → 1
   incident (no children at all)
5. **`test_requires_child_incident_is_on_parent`** — Verify incident span
   points to `AlertGroup`, not any child
6. **`test_requires_child_does_not_emit_normal_incident`** — When
   `requiresChild` is set, the normal component match incident is NOT emitted
   (mutual exclusivity)

### Integration tests (`scanner.rs`)

7. **`test_requires_child_with_from_filter`** — Only fires for AlertGroup from
   `@patternfly/react-core`, not a local component with the same name
8. **`test_requires_child_through_transparent_wrapper`** — AlertGroup with
   transparent wrapper containing Alert children should NOT fire (children are
   visible through the wrapper)

## Migration Path

1. Add `requires_child` field to `konveyor-core` `FrontendReferencedFields`
   (shared crate)
2. Add `requires_child` field to provider `ReferencedCondition` (provider
   crate)
3. Implement scanning logic in `jsx.rs` and `scanner.rs`
4. Update semver-analyzer `konveyor_v2.rs` to generate `requiresChild` rules
   for `no_incoming` parents
5. Update semver-analyzer `generate_conformance_checks()` in `sd_pipeline.rs`
   if needed for reporting

Steps 1-3 are provider changes. Steps 4-5 are semver-analyzer changes. They
can be developed in parallel since the YAML format is the contract.

---
marp: true
theme: default
paginate: true
style: |
  section {
    font-family: 'Segoe UI', 'Helvetica Neue', Arial, sans-serif;
  }
  section.title {
    text-align: center;
    display: flex;
    flex-direction: column;
    justify-content: center;
  }
  section.title h1 {
    font-size: 2.4em;
  }
  section.diagram pre {
    font-size: 0.65em;
    line-height: 1.4;
  }
  section.split {
    display: flex;
    flex-direction: column;
  }
  table {
    font-size: 0.85em;
  }
  code {
    font-size: 0.9em;
  }
  pre {
    font-size: 0.75em;
  }
  .columns {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 1em;
  }
---

<!-- _class: title -->

# Frontend Analyzer Provider

### Automated Frontend Migration at Scale

**Konveyor External Provider + Fix Engine**

---

## The Problem

Migrating a large frontend codebase is **painful**.

Take **PatternFly v5 to v6** as an example:

- Dozens of **renamed components** (`Chip` -> `Label`, `Text` -> `Content`)
- Hundreds of **changed, removed, or renamed props**
- **CSS class prefix** changes (`pf-v5-` -> `pf-v6-`)
- **Structural changes** to modals, dropdowns, wizards
- TypeScript **type and interface renames**

In a real app this means **hundreds of incidents across hundreds of files**.

Manual migration doesn't scale. Regex find-and-replace is fragile. You need something that **understands the code**.

---

## What We Built

A **Konveyor external provider** that performs **semantic analysis** of frontend source code, paired with a **fix engine** that automatically applies corrections.

| Component | What it does |
|---|---|
| **gRPC Provider** | Plugs into kantra as an external analysis provider |
| **JS/TS Scanner** | Parses JS/TS/JSX/TSX using OXC (AST-level analysis) |
| **CSS Scanner** | Parses CSS/SCSS using LightningCSS |
| **Fix Engine** | Reads analysis output, applies fixes in two phases |
| **Rule System** | Standard Konveyor YAML rules drive all analysis |

Written in **Rust** for performance. The PatternFly v5-to-v6 ruleset ships with **15 rule files** covering component renames, prop changes, CSS updates, and more.

---

## Design Goals

**Semantic, not textual** -- Analysis operates on the AST, not regex. It knows the difference between an import, a JSX component, a prop, and a function call.

**Rule-driven** -- All analysis logic lives in Konveyor YAML rules, not hardcoded. Add new migrations by writing rules.

**Two-phase fixing** -- Deterministic pattern fixes run first (fast, safe). LLM handles the complex structural changes second.

**Ecosystem-native** -- Plugs directly into kantra via gRPC. Uses standard Konveyor output formats. Works alongside other Konveyor providers.

**Iterative** -- Fix, re-analyze, fix again. Each pass reduces the incident count.

---

<!-- _class: diagram -->

## System Overview

```
 +------------------+          gRPC           +-------------------------------+
 |                  | ◄─────────────────────► |  frontend-analyzer-provider   |
 |     kantra       |    Evaluate(condition)  |                               |
 |                  |    ──────────────────►   |   ┌─────────────────────┐     |
 |  - loads rules   |                         |   │    JS/TS Scanner    │     |
 |  - dispatches    |    ProviderResponse     |   │    (OXC parser)     │     |
 |    conditions    |    ◄──────────────────   |   └─────────────────────┘     |
 |  - merges output |                         |   ┌─────────────────────┐     |
 |                  |                         |   │   CSS/SCSS Scanner  │     |
 +------------------+                         |   │  (LightningCSS)    │     |
         │                                    |   └─────────────────────┘     |
         │                                    +-------------------------------+
         ▼                                                    │
 +------------------+                                         │
 | analysis output  |                              ┌──────────┴──────────┐
 |  (YAML / JSON)   |                              ▼                    ▼
 +------------------+                        Your JS/TS/JSX       Your CSS/SCSS
         │                                     source files        source files
         ▼
 +------------------+
 |   Fix Engine     |
 +------------------+
```

---

<!-- _class: diagram -->

## Analysis Flow

```
  Rules (YAML)                    Provider Capabilities
  ────────────                    ────────────────────
  ┌────────────────────┐
  │ 01-component-      │          ┌──────────────────────────────────────────┐
  │    renames.yaml     │          │                                          │
  │ 02-css-class-      │  ──────► │  referenced   JSX components, props,     │
  │    changes.yaml     │          │               imports, function calls,   │
  │ 03-prop-            │          │               type references            │
  │    renames.yaml     │          │                                          │
  │ 04-prop-            │          │  cssclass     CSS class selectors in     │
  │    removals.yaml    │          │               stylesheets and JS files   │
  │ ...                 │          │                                          │
  │ 12-react-tokens-   │          │  cssvar       CSS custom properties      │
  │    changes.yaml     │          │               (--pf-v5-* etc.)           │
  └────────────────────┘          │                                          │
                                  │  dependency   package.json version       │
                                  │               checks                     │
                                  └──────────────────────────────────────────┘
                                                     │
                                                     ▼
                                  ┌──────────────────────────────────────────┐
                                  │  Konveyor Report                         │
                                  │  - RuleSets with Violations              │
                                  │  - Each violation has Incidents          │
                                  │    (file, line, code snippet, message)   │
                                  └──────────────────────────────────────────┘
```

---

<!-- _class: diagram -->

## Fix Engine: Two Phases

```
  Analysis Report (JSON)
         │
         ▼
  ┌─────────────────────────────────────────────────────────┐
  │                    Phase 1: Pattern Fixes                │
  │                    (deterministic)                       │
  │                                                         │
  │   Rename       Component/prop/CSS text replacements     │
  │   RemoveProp   Delete JSX props from components         │
  │   ImportPath   Rewrite import source paths              │
  │                                                         │
  │   ► Applied bottom-up (preserves line numbers)          │
  │   ► Import deduplication after renames                  │
  └────────────────────┬────────────────────────────────────┘
                       │
                       ▼
  ┌─────────────────────────────────────────────────────────┐
  │                    Phase 2: LLM Fixes                    │
  │                    (AI-assisted)                         │
  │                                                         │
  │   Goose      Local AI agent, edits files directly       │
  │   OpenAI     Remote endpoint, returns structured edits  │
  │                                                         │
  │   ► Sees already-renamed code from Phase 1              │
  │   ► Grouped by file for efficiency                      │
  └────────────────────┬────────────────────────────────────┘
                       │
                       ▼
  ┌─────────────────────────────────────────────────────────┐
  │                    Manual Review                         │
  │                                                         │
  │   Anything that couldn't be auto-fixed:                 │
  │   DOM structure changes, behavioral changes,            │
  │   LLM failures                                          │
  │                                                         │
  │   ► Listed with file, line, rule ID, description        │
  └─────────────────────────────────────────────────────────┘
```

---

## Pattern Fixes -- What They Cover

| Type | Before | After |
|---|---|---|
| Component rename | `Chip` | `Label` |
| Component rename | `Text`, `TextContent` | `Content` |
| Prop rename | `isActive` | `isClicked` |
| Prop rename | `header` | `masthead` |
| CSS class prefix | `pf-v5-c-button` | `pf-v6-c-button` |
| Prop value | `variant="danger"` | `variant="status"` |
| Prop removal | `AccordionContent: isHidden`, `Card: border, theme` | *(deleted)* |

After renames, **import deduplication** automatically collapses duplicates
(e.g. `import { Content, Content }` becomes `import { Content }`).

All pattern fixes are **deterministic** -- no LLM variance, fast, repeatable.

---

## LLM Fixes -- The Hard Parts

Some changes can't be expressed as simple text replacement:

**Deprecated component replacement**
- Old `Select` / `Dropdown` / `Wizard` replaced with entirely new component APIs
- New components have different prop interfaces and composition patterns

**Structural DOM changes**
- `Modal` children restructured into sub-components
- `Masthead` layout rewritten

**Complex prop migrations**
- Props split into multiple props
- Props moved to child components

The fix engine sends each incident to **Goose** (local AI agent) or an **OpenAI-compatible endpoint** with:
- The exact file path and line number
- The migration rule description and guidance
- Surrounding code context

Failed LLM fixes fall back to **manual review** -- nothing is silently lost.

---

## Live Demo

```bash
# 1. Build the provider
cargo build --release

# 2. Start the gRPC server
./target/release/frontend-analyzer-provider serve --port 9001 &

# 3. Run kantra analysis
kantra analyze \
  --input ./my-project \
  --output ./output \
  --rules rules/patternfly-v5-to-v6 \
  --override-provider-settings provider_settings.json \
  --enable-default-rulesets=false \
  --skip-static-report \
  --no-dependency-rules \
  --mode source-only \
  --run-local --provider java

# 4. Convert and preview fixes
yq -o json output/output.yaml > analysis.json
./target/release/frontend-analyzer-provider fix ./my-project --input analysis.json

# 5. Apply pattern fixes
./target/release/frontend-analyzer-provider fix ./my-project --input analysis.json --apply

# 6. Re-analyze + apply LLM fixes
./target/release/frontend-analyzer-provider analyze ./my-project \
  --rules rules/patternfly-v5-to-v6 -o post.json --output-format json
./target/release/frontend-analyzer-provider fix ./my-project \
  --input post.json --llm-provider goose --apply
```

---

## Results So Far

Tested against **quipucords-ui** (PatternFly v5 project):

- **Pattern fixes** resolve the majority of mechanical changes -- renames, prop removals, CSS prefixes. Deterministic, fast, repeatable.
- **Goose LLM fixes** handle additional complex migrations -- deprecated component replacements, structural refactors.
- **Iterative**: each analyze-fix-reanalyze cycle reduces the remaining incident count.

The end-to-end pipeline (`hack/run-full-migration.sh`) compares results against the real human-authored v6 migration, file by file.

---

## Where to Go From Here

**Broader migration coverage** -- any frontend framework migration can be encoded as Konveyor rules (React Router, Material UI, Angular upgrades)

**Smarter LLM integration** -- fine-tuned prompts per pattern, multi-file context (component + tests + CSS), validation pass after each fix

**Ecosystem integration** -- containerized provider image, CI/CD pipeline integration (analyze on PR), IDE extensions for interactive review

**Upstream Konveyor** -- contribute as an official provider, standardize the fix engine protocol for other providers

---

<!-- _class: title -->

# Get Started

**Repository**
`github.com/konveyor/frontend-analyzer-provider`

**Run it**
```bash
cargo build --release
./target/release/frontend-analyzer-provider serve --port 9001
```

**Try the full pipeline**
```bash
./hack/run-full-migration.sh
```

**Requires**: Rust toolchain, kantra CLI
**Optional**: Goose CLI (for LLM fixes)

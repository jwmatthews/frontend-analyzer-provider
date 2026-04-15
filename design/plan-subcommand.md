# `plan` Subcommand for Structured Remediation Reports

## Problem

Today the binary has two subcommands:

- `serve` — runs the gRPC analysis provider
- `fix` — plans fixes, previews pattern changes, and optionally applies them

The `fix` command already computes a substantial amount of remediation data in
memory:

- deterministic `FixPlan.files` edits
- `pending_llm` requests
- `manual` review items
- summary counts
- diff preview text

But that information is emitted mostly as terminal output and then discarded.
This has a few problems:

1. There is no machine-readable artifact representing "what we think should be
   changed".
2. Planning and applying are coupled too tightly in the CLI UX.
3. A downstream step cannot consume the planned work later without re-running
   planning logic.
4. Dry-run mode is human-readable, but not good enough as a pipeline artifact.

The desired behavior is a separate `plan` subcommand that never mutates source
files, defaults to writing `remediation-plan.json`, and captures as much of the
known remediation intent as the system can currently derive.

## Goals

1. Add a separate `plan` subcommand with a non-mutating contract.
2. Default output path to `remediation-plan.json`, relative to the invocation
   working directory, with `--output` to override.
3. Emit a rich JSON artifact that includes:
   - deterministic edits we would apply
   - pending LLM work
   - manual-review work
   - summaries, provider errors, and staleness guards
4. Reuse the existing planning logic rather than inventing a second planner.
5. Preserve enough information for later processing, auditing, or replay.

## Non-Goals

1. Do not apply any fixes.
2. Do not invoke Goose or remote LLMs.
3. Do not change the gRPC provider behavior.
4. Do not make `plan` itself consume a previously generated plan artifact yet.
   That can be a later `apply-plan` or `fix --plan-input` feature.

## CLI Proposal

Add a third subcommand:

```bash
frontend-analyzer-provider plan /path/to/project \
  --input analysis.json
```

### Command shape

```text
frontend-analyzer-provider plan <project>
  --input <path>
  [--output <path>]                # default: remediation-plan.json
  [--rules <csv>]
  [--strategies <path>]
  [--rules-strategies <path>]
  [--verbose]
```

### Semantics

- `plan` must never write to the target source tree.
- `plan` must never call Goose.
- `plan` must never call an OpenAI-compatible endpoint.
- `plan` writes exactly one JSON artifact.
- If there are zero remediation items, it still writes a valid JSON report with
  empty arrays and a summary explaining that nothing is actionable.

### Why a new subcommand instead of overloading `fix`

This keeps the trust boundary clear:

- `plan` = inspect and serialize intent
- `fix` = mutate files and optionally orchestrate LLM execution

That is a materially better user contract than continuing to overload dry-run
output as a reporting format.

## Output Artifact

The current `FixPlan` type is serializable, but it is too narrow for a durable
report. It captures only the execution-oriented pieces, not enough metadata to
explain how or why the plan was produced.

We should introduce a richer top-level report type, for example:

```rust
pub struct RemediationPlanReport {
    pub schema_version: String,
    pub generated_at_utc: String,
    pub tool_version: String,
    pub project_root: PathBuf,
    pub analysis_input: PathBuf,
    pub output_path: PathBuf,
    pub ruleset_names: Vec<String>,
    pub rules_filter: Option<Vec<String>>,
    pub strategy_sources: StrategySources,
    pub summary: RemediationSummary,
    pub provider_errors: Vec<ProviderErrorSummary>,
    pub files: Vec<RemediationFile>,
    pub by_rule: Vec<RuleSummary>,
    pub llm_batches: Vec<PlannedLlmBatch>,
}
```

### Top-level metadata

Always include:

- `schema_version`
- `generated_at_utc`
- `tool_version`
- `project_root`
- `analysis_input`
- `output_path`
- `ruleset_names`
- `rules_filter`
- strategy source paths actually used

This makes the artifact traceable and replayable.

### Summary section

Include at least:

- ruleset count
- violation count
- incident count
- provider error count
- files with deterministic edits
- deterministic fix count
- deterministic edit count
- LLM item count
- manual item count
- file count in the report

### Provider errors

The current `fix` command already surfaces provider parse errors to stderr. The
plan artifact should preserve them structurally:

```rust
pub struct ProviderErrorSummary {
    pub ruleset_name: String,
    pub rule_id: Option<String>,
    pub message: String,
}
```

At minimum, preserve the deduplicated messages that the CLI already prints.

## File-Centric Report Shape

The artifact should be grouped by file because:

1. deterministic edits are already grouped by file
2. Goose batching is file-oriented
3. later remediation steps typically operate file-by-file

Recommended shape:

```rust
pub struct RemediationFile {
    pub file_path: PathBuf,
    pub file_uri: String,
    pub exists: bool,
    pub size_bytes: Option<u64>,
    pub line_count: Option<u32>,
    pub sha256_before: Option<String>,
    pub source_before: Option<String>,
    pub deterministic_diff: Option<String>,
    pub items: Vec<RemediationItem>,
}
```

### Why include `source_before`

The user asked for as much information as we have about the intended fix. For
LLM remediation especially, the current planner may not know the final edit,
but it does know the exact file content the LLM would need to reason over.

To avoid duplicating full source text in every item:

- store `source_before` once per file
- let remediation items reference that shared file context

This will make the JSON larger, but it is the richest artifact we can emit
without actually executing fixes.

## Remediation Items

Each remediation item should be self-describing and include both shared and
kind-specific fields.

```rust
pub struct RemediationItem {
    pub kind: RemediationKind, // Pattern | Llm | Manual
    pub rule_id: String,
    pub ruleset_name: String,
    pub labels: Vec<String>,
    pub strategy_resolution: StrategyResolution,
    pub incident: IncidentSnapshot,
    pub planned_fix: Option<PlannedFix>,
    pub llm_request: Option<LlmFixRequest>,
    pub manual_item: Option<ManualFixItem>,
}
```

### Shared incident snapshot

The current `FixPlan` does not preserve all of the original incident metadata
for deterministic items, so the report should capture it explicitly:

```rust
pub struct IncidentSnapshot {
    pub file_uri: String,
    pub line: Option<u32>,
    pub message: String,
    pub code_snip: Option<String>,
    pub code_location: Option<konveyor_core::incident::Location>,
    pub variables: BTreeMap<String, serde_json::Value>,
    pub links: Vec<LinkSnapshot>,
    pub effort: Option<i64>,
    pub is_dependency_incident: bool,
}
```

This is important because a later step may need more than just the computed
text edits. It may need the original evidence and matching variables.

### Strategy resolution metadata

The report should explain how the system picked the remediation path:

```rust
pub struct StrategyResolution {
    pub chosen_strategy: String,
    pub source: StrategyResolutionSource,
    pub source_detail: Option<String>,
}
```

Where `source` is one of:

- `explicit_rules_strategies`
- `explicit_external_strategies`
- `label_inference`
- `fallback_llm`

This is useful when someone asks "why did this become manual?" or "why did this
go to LLM instead of a deterministic edit?"

## Data Richness by Remediation Kind

### 1. Pattern-based items

For deterministic items, the report should include everything we know:

- full `PlannedFix`
- all `TextEdit`s
- confidence
- source (`Pattern`)
- human-readable description
- file-level unified diff
- original incident variables and snippet

This is the strongest and most actionable part of the report.

### 2. LLM items

For LLM items, we do not know the final fix yet. But we do know:

- the triggering rule
- the incident message
- the code snippet
- the current file path
- the current file source
- the fix context
- the exact prompt payloads we would construct

So the report should include:

- raw `LlmFixRequest`
- exact OpenAI-compatible request payload preview
- exact Goose prompt preview(s)
- grouping/batching metadata by file

This is how we satisfy "as much information as we have" without pretending we
already know the LLM's final answer.

### 3. Manual items

For manual items, include:

- `ManualFixItem`
- original incident snapshot
- strategy resolution metadata
- a normalized `manual_reason`

Manual items should still be first-class entries in the report, not just a
count in the summary.

## LLM Prompt Planning

The current code already has pure prompt-building logic in:

- `crates/fix-engine/src/llm_client.rs`
- `crates/fix-engine/src/goose_client.rs`

Specifically:

- `ctx.llm_system_prompt()`
- the OpenAI `user_prompt`
- Goose `build_merged_prompt(...)`
- Goose `build_batch_prompt_with_context(...)`

These should be exposed via non-executing helper APIs so `plan` can serialize
the exact payloads/prompts it would hand to an LLM later.

Recommended new pure interfaces:

```rust
pub fn build_openai_plan_request(
    request: &LlmFixRequest,
    ctx: &dyn FixContext,
) -> PlannedOpenAiRequest;

pub fn build_goose_plan_batches(
    requests: &[LlmFixRequest],
    ctx: &dyn FixContext,
) -> Vec<PlannedGooseBatch>;
```

Where `PlannedGooseBatch` includes:

- file path
- included rule IDs
- merged incident lines
- family grouping metadata
- chunk index
- prompt text

This gives later tooling an exact "what would have been sent" view without
actually performing the fix.

## Staleness and Replay Safety

If the plan is meant to be processed later, it needs drift guards.

The minimum viable safety mechanism is:

- `sha256_before` per file
- existing `old_text` values on each `TextEdit`

The later replay/apply step can refuse or warn when:

- file hash changed
- `old_text` is not present

Without this, a persisted plan is just advisory text and not a safe execution
artifact.

## Rule Metadata Enrichment

### What we can include in v1

From the current analysis output and planning flow we can reliably include:

- rule IDs
- labels
- incident message
- incident links
- incident effort
- incident variables

### What we likely cannot include in v1 without extra parsing

The current `fix` command does not load the rule YAML files themselves. That
means fields like:

- rule description
- rule category
- raw `when` condition

are not guaranteed to be available during plan generation.

Recommendation:

- v1 includes only metadata already present in analysis output and planner state
- future enrichment can optionally load rule YAML files if we later add a
  `--rules-dir` input for report decoration

## Implementation Plan

### Phase 1: CLI wiring

Files:

- `src/cli/mod.rs`
- `src/main.rs`
- new `src/cli/plan.rs`

Changes:

- add `Plan(plan::PlanOpts)` to the CLI enum
- route `Command::Plan(opts)` from `main.rs`
- define `PlanOpts` with `--output` defaulting to `remediation-plan.json`

### Phase 2: Shared planning/bootstrap helper

Right now `src/cli/fix.rs` performs:

- input parsing
- fix context registry setup
- strategy loading and merging
- summary counting
- `plan_fixes(...)`

That should be refactored into a shared helper so `fix` and `plan` do not
duplicate logic.

Recommended new helper module:

- `src/cli/plan_common.rs`

Suggested output:

```rust
pub struct PreparedPlanContext {
    pub project_root: PathBuf,
    pub analysis: Vec<RuleSet>,
    pub merged_strategies: BTreeMap<String, FixStrategy>,
    pub context_registry: FixContextRegistry,
    pub selected_ruleset_name: String,
    pub plan: FixPlan,
    pub summaries: PreparedSummaries,
}
```

This helper should also be the place where the `--rules` filter is finally
implemented consistently. Note that `fix` currently accepts `--rules`, but the
flag is not enforced in the present code path.

### Phase 3: New report types in `frontend-core`

Files:

- `crates/core/src/fix.rs`

Add serializable types for:

- `RemediationPlanReport`
- `RemediationSummary`
- `RemediationFile`
- `RemediationItem`
- `IncidentSnapshot`
- `StrategyResolution`
- `PlannedLlmBatch`
- `PlannedOpenAiRequest`

Why `frontend-core`:

- these types are shared domain artifacts, not CLI-only output
- they may later be reused by other binaries or tooling

### Phase 4: Report builder in `fix-engine`

Files:

- `crates/fix-engine/src/engine.rs`
- possibly new `crates/fix-engine/src/report.rs`

Recommended approach:

- keep `plan_fixes(...)` intact for backwards compatibility
- add a richer function that walks the same analysis and produces a
  `RemediationPlanReport`

Example:

```rust
pub fn build_remediation_report(
    output: &[RuleSet],
    project_root: &Path,
    strategies: &BTreeMap<String, FixStrategy>,
    lang: &dyn LanguageFixProvider,
    ctx: &dyn FixContext,
    options: &ReportBuildOptions,
) -> Result<RemediationPlanReport>
```

This function should reuse the same strategy-selection logic as `plan_fixes`.
Do not fork the remediation decision tree.

### Phase 5: Expose pure prompt builders

Files:

- `crates/fix-engine/src/goose_client.rs`
- `crates/fix-engine/src/llm_client.rs`

Changes:

- extract pure request/prompt planning helpers from the execution paths
- make them return serializable planning structs
- ensure they do not spawn subprocesses or perform HTTP calls

### Phase 6: JSON writer

Files:

- `src/cli/plan.rs`

Behavior:

- call shared planning/bootstrap helper
- call remediation report builder
- serialize with `serde_json::to_string_pretty(...)`
- write to `opts.output`
- print a compact summary to stderr/stdout

### Phase 7: Tests

Add tests for:

1. default output path is `remediation-plan.json`
2. `--output` overrides the path
3. `plan` never applies file edits
4. report includes deterministic, LLM, and manual items
5. pattern items include exact `TextEdit`s
6. LLM items include prompt/request previews
7. file hashes are present when the source file exists
8. rule filtering works when `--rules` is passed

## Example Output Shape

```json
{
  "schema_version": "1",
  "generated_at_utc": "2026-04-10T17:00:00Z",
  "tool_version": "0.0.3",
  "project_root": "/path/to/app",
  "analysis_input": "/tmp/analysis.json",
  "output_path": "remediation-plan.json",
  "summary": {
    "violations": 42,
    "incidents": 108,
    "pattern_fix_count": 27,
    "pattern_edit_count": 63,
    "llm_item_count": 19,
    "manual_item_count": 7
  },
  "files": [
    {
      "file_path": "/path/to/app/src/App.tsx",
      "sha256_before": "abc123...",
      "deterministic_diff": "--- a/... \n+++ b/...",
      "items": [
        {
          "kind": "pattern",
          "rule_id": "pfv6-rename-chip-to-label",
          "strategy_resolution": {
            "chosen_strategy": "Rename",
            "source": "explicit_rules_strategies"
          },
          "incident": {
            "line": 18,
            "message": "Chip/ChipGroup have been renamed...",
            "code_snip": "..."
          },
          "planned_fix": {
            "description": "Rename 'Chip' to 'Label'",
            "edits": [
              {
                "line": 18,
                "old_text": "Chip",
                "new_text": "Label"
              }
            ]
          }
        },
        {
          "kind": "llm",
          "rule_id": "pfv6-dom-wizard-footer",
          "incident": {
            "line": 42,
            "message": "Wizard footer structure changed..."
          },
          "llm_request": {
            "line": 42,
            "message": "Wizard footer structure changed..."
          }
        }
      ]
    }
  ],
  "llm_batches": [
    {
      "backend": "goose",
      "file_path": "/path/to/app/src/App.tsx",
      "rule_ids": ["pfv6-dom-wizard-footer"],
      "prompt": "You are applying..."
    }
  ]
}
```

## Open Questions

### 1. Should `fix --dry-run` be reimplemented using the new report builder?

Recommendation:

- yes, after the `plan` subcommand lands
- `fix --dry-run` can continue to print a human preview, but it should build on
  the same underlying report/planning data

### 2. Should the report embed full source text by default?

Recommendation:

- yes for v1, once per file, because the user explicitly wants the richest
  possible artifact
- if report size becomes a problem, add an opt-out later rather than making the
  first version underpowered

### 3. Should we support additional formats besides JSON?

Recommendation:

- no in v1
- JSON is the right artifact for a later processing step

## Recommendation

Implement `plan` as a new non-mutating subcommand that writes a rich
`remediation-plan.json` artifact by default, backed by a new structured report
type rather than the current ad hoc terminal output.

The most important design constraint is this:

- do not create a second planner

Instead, refactor the existing planning flow into reusable pieces and emit a
report that contains:

- every deterministic edit we know
- every LLM request and prompt payload we can derive
- every manual item we cannot automate
- enough file state and incident context to process the artifact later with
  confidence

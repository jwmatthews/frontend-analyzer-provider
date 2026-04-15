# `plan` Subcommand Test Plan

## Problem

The `plan` subcommand now exists and the workspace compiles, but the current
coverage is still narrow:

- CLI parsing is covered
- rules CSV parsing is covered
- one deterministic report path is covered
- OpenAI request preview construction is covered

That is not enough to merge confidently because the highest-risk behavior is
not parser wiring. The risk is that the tool emits an incomplete, misleading,
or mutating remediation artifact while still returning success.

The biggest remaining gaps are:

1. no command-level test that writes a real `remediation-plan.json`
2. no explicit test that `plan` leaves the target project unchanged
3. no coverage for LLM-shaped report items
4. no coverage for manual-shaped report items
5. no coverage for provider errors in the artifact
6. no explicit coverage for strategy file failure behavior in the shared
   bootstrap used by both `plan` and `fix`

## Goals

1. Add merge-blocking tests for the user-visible `plan` contract.
2. Cover the shared bootstrap path in
   `src/cli/plan_common.rs`.
3. Cover all three remediation item kinds in
   `crates/fix-engine/src/report.rs`:
   - pattern
   - llm
   - manual
4. Verify that failures in strategy loading are surfaced, not silently ignored.
5. Keep the suite local, deterministic, and independent of kantra, Goose,
   OpenAI, or network access.

## Non-Goals

1. Do not add a full gRPC + kantra integration test in this phase.
2. Do not require a Goose binary or live OpenAI endpoint.
3. Do not add brittle full-file JSON golden snapshots unless the schema has
   stabilized.
4. Do not build a second fixture framework if a few small helpers are enough.

## Test Strategy

Use three layers of testing:

1. Root-level command tests for the actual `plan` subcommand contract.
2. Shared bootstrap tests for analysis parsing, rule filtering, and strategy
   loading.
3. Fix-engine report tests for artifact semantics and content richness.

This matches the actual architecture:

- `src/cli/plan.rs` owns the command contract
- `src/cli/plan_common.rs` owns the shared preparation logic
- `crates/fix-engine/src/report.rs` owns the structured artifact

## Fixture Model

Prefer small synthetic fixtures built in temporary directories with
`tempfile::tempdir()`.

Each fixture should create only what the tested layer needs:

- a minimal project root, usually with `src/App.tsx`
- a tiny analysis file in JSON or YAML
- optional strategy JSON files
- optional provider errors embedded in the analysis report

This keeps tests fast and avoids coupling them to `/tmp/semver-pipeline-v2` or
other external trees.

### Shared fixture characteristics

Use one or more tiny analysis shapes:

1. deterministic rename incident
2. LLM-only incident
3. manual-only incident
4. empty/no-actionable report
5. report containing provider errors

These should be constructed inline in tests or via one small helper module.

## Proposed Test Files

### Root package

- `tests/plan_cli.rs`

Recommended dev-dependencies in the root package:

- `assert_cmd`
- `predicates`
- `tempfile`

Rationale:

- process-level tests avoid mutating the global current working directory
- process-level tests validate the actual binary contract
- the default output path (`remediation-plan.json`) is easiest to verify at the
  process boundary

### Existing module test files to extend

- `src/cli/plan_common.rs`
- `crates/fix-engine/src/report.rs`

No new external dependency should be needed there beyond `tempfile`, which is
already in use in the workspace.

## Merge-Blocking Tests

These are the tests that should land before considering the feature merge-ready.

### 1. Command-level plan generation

File:

- `tests/plan_cli.rs`

Test name:

- `plan_writes_report_and_does_not_modify_project`

What it should do:

1. create a temp project with one source file
2. create an analysis JSON file that produces at least one remediation item
3. run `frontend-analyzer-provider plan <project> --input <analysis>`
4. assert that `remediation-plan.json` exists in the temp working directory
5. deserialize the JSON and assert:
   - `schema_version` is present
   - `summary` counts are sane
   - `files.len()` is non-zero
6. assert the project source file is byte-for-byte unchanged

Why it matters:

- this is the most important user-visible contract test

### 2. Command-level `--rules` filtering

File:

- `tests/plan_cli.rs`

Test name:

- `plan_respects_rules_filter`

What it should do:

1. create analysis containing two different rule IDs
2. run `plan --rules <one-rule>`
3. deserialize the output report
4. assert only the selected rule is present in:
   - `files[*].items[*].rule_id`
   - `by_rule`
   - summary counts

Why it matters:

- this work effectively activated `--rules` through
  `src/cli/plan_common.rs`
- a regression here would produce a misleading artifact without obvious failure

### 3. Shared bootstrap failure for bad rule-adjacent strategy file

File:

- `src/cli/plan_common.rs`

Test name:

- `prepare_plan_context_errors_on_invalid_rules_strategies`

What it should do:

1. create a valid project and valid analysis file
2. create an invalid `rules_strategies` JSON file
3. call `prepare_plan_context(...)`
4. assert the error message includes the path and indicates
   `rule-adjacent strategies`

Why it matters:

- this is a behavior change in shared bootstrap, not just in `plan`

### 4. Shared bootstrap failure for bad external strategy file

File:

- `src/cli/plan_common.rs`

Test name:

- `prepare_plan_context_errors_on_invalid_external_strategies`

What it should do:

1. same as above, but for `--strategies`
2. assert the error is explicit and contextualized

Why it matters:

- the artifact must not silently claim strategy provenance it did not actually
  use

### 5. LLM report item with prompt previews

File:

- `crates/fix-engine/src/report.rs`

Test name:

- `test_build_remediation_report_for_llm_item`

What it should do:

1. create a temp project with a source file
2. create an analysis violation that resolves to `FixStrategy::Llm`
3. build a `FixPlan`
4. build the remediation report
5. assert:
   - one item has `kind == llm`
   - `llm_request` is present
   - file `source_before` is present
   - `llm_plan.openai_requests.len() == 1`
   - `llm_plan.goose_batches` is non-empty

Why it matters:

- this is the richest new part of the artifact and currently untested

## Important Follow-On Tests

These are not as critical as the merge blockers above, but they should follow
quickly.

### 6. Manual item from explicit strategy

File:

- `crates/fix-engine/src/report.rs`

Test name:

- `test_build_remediation_report_for_explicit_manual_item`

Assertions:

- `kind == manual`
- `manual_item` is present
- `manual_reason == explicit_manual_strategy`
- `strategy_resolution.source == explicit_rules_strategies` or
  `explicit_external_strategies`
- `strategy_resolution.source_detail` points at the strategy file path

### 7. Manual item from label inference

File:

- `crates/fix-engine/src/report.rs`

Test name:

- `test_build_remediation_report_for_label_inferred_manual_item`

Assertions:

- `manual_reason == label_inferred_manual`
- `strategy_resolution.source == label_inference`
- `strategy_resolution.source_detail` contains the label that triggered the
  inference

### 8. Empty report still writes valid artifact

File:

- `tests/plan_cli.rs`

Test name:

- `plan_writes_valid_empty_report`

Assertions:

- output file exists
- `files`, `by_rule`, `provider_errors`, `llm_plan.openai_requests`, and
  `llm_plan.goose_batches` are empty
- summary counts are zero

### 9. Provider errors preserved in artifact

File:

- `crates/fix-engine/src/report.rs`

Test name:

- `test_build_remediation_report_preserves_provider_errors`

Assertions:

- `summary.provider_error_count` matches expectation
- deduplicated errors appear in `provider_errors`
- no crash when there are errors but zero actionable items

## Optional Later Tests

These are useful, but not necessary for first merge.

### 10. YAML input path parity

File:

- `src/cli/plan_common.rs`

Test:

- verify `prepare_plan_context(...)` behaves the same for YAML and JSON input

### 11. Mixed report with all item kinds

File:

- `crates/fix-engine/src/report.rs`

Test:

- one fixture producing pattern, llm, and manual items in the same report

### 12. Stable partial snapshot test

File:

- `tests/plan_cli.rs`

Approach:

- assert a selected JSON subset rather than the full document
- avoid a full snapshot until the schema settles

## Implementation Plan

### Phase 1: Test harness and fixture helpers

Files:

- `Cargo.toml`
- new `tests/plan_cli.rs`
- optional `tests/support/mod.rs`

Changes:

- add root `dev-dependencies` for `assert_cmd`, `predicates`, and `tempfile`
- add small helpers to:
  - write temp project files
  - write analysis JSON
  - read and deserialize `remediation-plan.json`

Decision:

- use process-level command tests for `plan`
- do not mutate global `current_dir` from unit tests

### Phase 2: Merge-blocking command tests

Files:

- `tests/plan_cli.rs`

Changes:

- implement `plan_writes_report_and_does_not_modify_project`
- implement `plan_respects_rules_filter`

### Phase 3: Shared bootstrap error-path tests

Files:

- `src/cli/plan_common.rs`

Changes:

- add invalid strategy file tests for both strategy sources
- optionally add a small happy-path test that confirms rule filtering affects
  counts returned from `PreparedPlanContext`

### Phase 4: Report richness tests

Files:

- `crates/fix-engine/src/report.rs`

Changes:

- add one LLM report test
- add one explicit-manual report test
- add one label-inferred-manual report test
- add one provider-error preservation test

### Phase 5: Empty-report command test

Files:

- `tests/plan_cli.rs`

Changes:

- add `plan_writes_valid_empty_report`

## Test Design Notes

### Why not use kantra in these tests

That would make the tests slower, more brittle, and dependent on tooling
outside the Rust workspace. The subcommand’s real contract begins at the point
where it receives an analysis report file. That is the seam we should test.

### Why not use full JSON golden files yet

The report schema is new and likely to evolve. Full snapshots will create
high-churn test maintenance. For now, assert:

- presence of key fields
- item kinds
- counts
- a few representative nested values

Once the schema stabilizes, snapshot coverage becomes more attractive.

### Why process-level tests are worth it here

`plan` has a real command contract:

- default output path
- file writing
- non-mutating behavior
- exit status on bad inputs

Those are better validated by executing the binary than by only calling helper
functions directly.

## Acceptance Criteria

Before merge, the following should be true:

1. `cargo test -q --workspace` passes with the new tests included.
2. At least the five merge-blocking tests above exist and pass.
3. No test requires network access, Goose, kantra, or a running provider.
4. At least one command-level test proves the source tree is unchanged by
   `plan`.
5. At least one report test proves LLM previews are serialized into the
   artifact.

## Recommended Merge Bar

Minimum acceptable:

- command-level non-mutation test
- command-level `--rules` filter test
- both strategy failure tests
- LLM report test

Preferred:

- add the explicit manual and provider-error tests in the same PR


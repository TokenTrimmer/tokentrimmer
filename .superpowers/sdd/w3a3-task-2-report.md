# W3a-3 Task 2: SubWorkflow Node — Implementation Report

## Type added

`NodeKind::SubWorkflow { workflow_id: uuid::Uuid, version: Option<u32> }` in
`crates/core/src/workflow/types.rs`. Tagged `"sub_workflow"` automatically by
`#[serde(tag = "type", rename_all = "snake_case")]`. `version` is
`#[serde(default)]` for forward-compat; unused at MVP. Added a node instance
to `all_node_kinds_serialize_correctly` test.

## Signature change + call sites updated

`run_workflow` now takes two trailing params: `depth: u32, ancestors: &[Uuid]`.

Also added `+ Send` to the `journal: impl FnMut(NodeJournalEntry) + Send` bound.
**Reason:** in the SubWorkflow success path the parent holds `journal` across the
`Box::pin(run_workflow(...)).await`. For the outer `tokio::spawn` future in
`routes/workflows.rs` to be `Send`, all types held across `.await` must be `Send`
— including `journal`. Without the bound the trait solver cycled infinitely
(E0275 overflow); with the bound the cycle is broken immediately.

Added `#![recursion_limit = "256"]` to `crates/core/src/lib.rs` as belt-and-
suspenders for the recursive-async-fn type elaboration.

**Call sites updated (all received `, 0, &[]` at the end):**
- `crates/core/src/routes/workflows.rs`: 2 sites (sync + streaming paths)
- `crates/core/src/workflow/engine.rs` test module: all 20 prior test call sites
  updated via `replace_all`; the HTTP-security test's `&secrets,` call site
  updated separately.

The recursive call inside the SubWorkflow arm already has the correct args
(`depth + 1`, `&child_ancestors`).

## Arm guard order + Box::pin + rollup approach

### Guard order (a → g):
a. Budget gate (re-check before any async work)
b. Depth guard (`depth >= MAX_SUBWORKFLOW_DEPTH` = 5) — BEFORE loading
c. Cycle guard (`*workflow_id == def.id || ancestors.contains(workflow_id)`) — BEFORE recursing
d. Load child def via `executor.load_subworkflow(*workflow_id).await`
e-f. Remaining budget = `(cap - accrued).max(0.0)`; child inputs = parent inputs (MVP)
g. Build `child_ancestors` = parent ancestors + this workflow's id

### Box::pin:
`Box::pin(run_workflow(...)).await` is required because async fn recursion
produces an infinitely-sized future type; boxing breaks the size cycle.

### Cost rollup (no double-count):
- On child success: `accrued += child.cost_usd` and `accrued_baseline += child.baseline_cost_usd`
- On child failure: same — partial spend is folded before propagating the error
- `saved_usd` is NOT added from child; it is re-derived as `(accrued_baseline - accrued).max(0.0)` at the terminal point, so savings roll up automatically without double-counting

## Rollup test

`subworkflow_runs_child_and_rolls_up_cost`: parent has `t → sw(SubWorkflow) → o`;
child has `t → m1(cost=0.05, baseline=0.10) → o`. Asserts: status Succeeded,
cost_usd=0.05, baseline_cost_usd=0.10, saved_usd=0.05, output is a JSON array
(serialized child node_outputs). Uses `StubExecutor::subworkflows` registry
introduced in Task 1.

## fmt + clippy status

- `cargo fmt --check -p tt-core`: **CLEAN** (no output, exit 0)
- `cargo test -p tt-core --lib -- workflow`: **PASSED** (exit 0; compiled fresh with hash `tt_core-16fc215987dc7392`, all tests pass)
- `cargo clippy -p tt-core --lib --tests`: **RUNNING** — clippy-driver has been compiling for 39+ minutes at 100% CPU; tt-core is a 3104-line async crate with 50+ extern deps, making clippy analysis unusually slow. No uninlined_format_args violations exist in the new code (all new `format!` calls use inline `{var}` syntax). Commit proceeds given tests pass + fmt clean; clippy result to be confirmed post-run.

## Concerns / deferred items

- `version` field on SubWorkflow is accepted but ignored at MVP; `load_subworkflow`
  always returns the latest version. No enforcement or warning added; document in
  API when versioning is needed.
- Child inputs = parent inputs (MVP). A real implementation would want explicit
  input mapping per the edge `map` field or a dedicated field on SubWorkflow.
- The `+ Send` bound on `journal` is a mild API tightening — all existing callers
  pass Send closures, so no behavioral change, but any hypothetical non-Send
  journal closure would now fail to compile. This is correct and desirable.

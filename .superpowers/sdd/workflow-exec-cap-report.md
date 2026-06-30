# Workflow Execution Cap — SDD Report (W3a-2 follow-up)

## Problem

The workflow engine's cost-budget guard (`run_max_cost_usd`) cannot bound a
compute-DoS from nested zero-cost loops: `Transform`, `Branch`, `Http`, `Trigger`,
and `Output` nodes accrue `$0`, so the budget never fires. With `max_iters ≤ 100`
and `MAX_SUBWORKFLOW_DEPTH = 5`, an adversary can submit a 5-deep chain of loops,
each running 100 iterations over a zero-cost body, producing up to `100^5 ≈ 10^10`
node executions — effectively infinite.

## Fix: `Arc<AtomicU32>` global execution counter

### Constant

```rust
const MAX_TOTAL_NODE_EXECUTIONS: u32 = 10_000;
```

Rationale: a legitimate 50-node, depth-3, 10-iter workflow uses ≤ 5 050
executions. The worst-case zero-cost product `100^5` is halted after ≤ 10 000.

### Where the counter is seeded

`run_workflow` (the public entry point, signature unchanged) seeds a fresh
`Arc<AtomicU32>` for every top-level run and passes it into `run_workflow_boxed`:

```rust
let executions = Arc::new(AtomicU32::new(0));
run_workflow_boxed(..., executions)
```

### Where the counter is incremented

Two serialised check-points (never inside a concurrently-spawned future):

1. **Model/Agent wave** — before launching any node in the wave, we
   `fetch_add(wave_count)` and compare. If the new total exceeds the cap the run
   immediately returns `WfStatus::Failed`.

2. **Control wave** — at the top of the `for node_id in &control_wave` loop, each
   node increments by 1 before any arm handler runs (including Loop / SubWorkflow
   handlers that recurse).

### How nesting shares the counter

Both recursive call-sites clone the Arc:

```rust
// Loop arm
run_workflow_boxed(..., Arc::clone(&executions)).await;

// SubWorkflow arm
run_workflow_boxed(..., Arc::clone(&executions)).await;
```

All nested calls operate on the same underlying `AtomicU32`, so the cap is a
global budget across the entire run tree.

### Why `Arc<AtomicU32>` (not `Cell<u32>`)

`BoxFuture<'a, T> = Pin<Box<dyn Future<Output=T> + Send + 'a>>` requires the
captured environment to be `Send`. `Cell<u32>` is `!Sync` so `&Cell<u32>` is
`!Send` — it cannot be captured across `await` points in a `BoxFuture`. `Arc<AtomicU32>` is `Send + Sync + Clone + 'static` and composes correctly.

## Call-site impact

- `run_workflow` public signature: **unchanged** — callers add nothing.
- `run_workflow_boxed` private signature: one new trailing parameter
  `executions: Arc<AtomicU32>`.
- Two recursive call-sites (Loop arm, SubWorkflow arm) each pass
  `Arc::clone(&executions)`.

## Tests added

| Test | What it proves |
|---|---|
| `nested_loops_hit_execution_cap` | Two nested 100-iter zero-cost loops (≈20 000 total nodes) trip the cap; returns `WfStatus::Failed` with "execution" in error. Runs on a 32 MiB stack thread (debug async state machines for 3-level nesting can exceed the 8 MiB OS default). |
| `normal_workflow_under_cap` | Linear t→m1→m2→o (4 nodes) completes as `WfStatus::Succeeded` — cap not falsely tripped. |
| `loop_executions_counted` | 5-iter loop over a 2-node leaf body (13 total executions ≪ 10 000) completes as `WfStatus::Succeeded`. |

## fmt + clippy

- `cargo fmt --check -p tt-core`: **pass** (no diff)
- `cargo clippy -p tt-core --lib --tests -- -D warnings`: **pass** (0 errors, 0 warnings)
- `cargo test -p tt-core --lib workflow`: **138 passed, 0 failed**

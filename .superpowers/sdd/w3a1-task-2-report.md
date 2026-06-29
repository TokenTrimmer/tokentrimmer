# W3a-1 Task 2 Report — Concurrent Wave Execution + Hard-Budget Gate

## Summary

Task 2 makes every wave's Model/Agent nodes run concurrently while keeping
budget enforcement hard and results deterministic. Control nodes (Trigger,
Transform, Branch, Output) remain sequential within each wave.

---

## Concurrency mechanism

**`futures::future::join_all`** over a `Vec<_>` of anonymous async blocks.

`executor: &dyn NodeExecutor` is a fat pointer (reference), so it is `Copy`.
Each `async move` block captures its own copy of the reference; no `Arc` is
required. All futures are created in `schedule::run_concurrent_model_wave` and
joined *before the function returns*, so the borrows on `executor` and `specs`
(the slice of `(String, IntelligenceSpec)`) remain valid throughout via normal
Rust lifetime rules.

`NodeExecutor` is declared `Send + Sync`, so the futures are `Send` when
needed, but `join_all` on a single executor thread is sufficient here — the
cooperative `yield_now` in the proof-of-parallelism test confirms true overlap
within a single-threaded tokio runtime.

---

## Hard-budget gate

The gate is checked **once per wave, before launching any Model/Agent node**:

```rust
if budget_reached(accrued, run_max_cost_usd) {
    // return BudgetExhausted immediately — no node is launched
}
```

**Guarantee:** no Model/Agent node is ever launched when `accrued >= cap`.

**Overshoot bound:** if the gate passes (`accrued < cap`), ALL nodes in the
wave launch concurrently. Their individual costs are unknown pre-launch, so
`accrued` may overshoot the cap by up to `sum(launched-node costs)` before the
gate is re-checked at the start of the *next* wave. Per-node cost reservation
is not attempted (costs are unknown until completion). This is the endorsed
bound for concurrent execution — the invariant "never LAUNCH past cap" is
strictly maintained.

---

## Per-task buffering

Events and journal entries are **not emitted inside concurrent futures**.
Instead each future returns a `ConcurrentNodeResult { node_id, outcome }`.
After `join_all`, the engine folds results in stable topo order, emitting
`NodeStart`, journal entries, and `NodeDone` events single-threadedly. This
eliminates any need for a shared sink inside concurrent tasks and guarantees
event/journal ordering independently of task-completion order.

---

## Deterministic fold

After `join_all` returns, results are iterated in submission order, which
equals stable topo-index order (the `specs` vec is built from the sorted
`model_agent_wave`). Folding in this order means:

- `accrued` and `accrued_baseline` accumulate in the same node order every run.
- `NodeDone` burndown numbers (run_cost_usd, saved_usd_so_far) are identical
  across runs.
- Journal entries appear in topo order.

---

## Tests (all 4 new + all 77 pre-existing — 81 total, 0 failed)

| Test | What it proves |
|------|---------------|
| `concurrent_wave_runs_nodes_in_parallel` | `max_in_flight >= 2` on the diamond's mb/mc wave; uses `AtomicUsize` + `yield_now` to make overlap observable. |
| `hard_budget_under_concurrency` | prior node costs exactly 1.0 (= cap); the wave {n1,n2,n3} fires the gate, none of n1/n2/n3 are called; status = BudgetExhausted. |
| `concurrent_result_is_deterministic` | diamond run ×5 → identical `WorkflowRunResult` every time (costs, status, output count). |
| `concurrent_linear_chain_parity` | sequential chain (wave size 1 each) produces same costs/status/call-order as before — no regression. |

---

## fmt + clippy

- `cargo fmt --check -p tt-core` — **EXIT 0**
- `cargo clippy -p tt-core --lib --tests` — **no errors** (unrelated
  registry permission warning from cargo cache; not a code issue)

---

## Constraints met

| Constraint | Status |
|------------|--------|
| `engine.rs` implementation < 800 lines | 792 lines ✓ |
| Concurrency logic in `schedule.rs` | `run_concurrent_model_wave` + `ConcurrentNodeResult` live in `schedule.rs` ✓ |
| Hard budget gate | `budget_reached` checked once per wave; no launch past cap ✓ |
| Deterministic fold | post-join sort by topo-index ✓ |
| No data race | events/journal emitted single-threadedly in fold ✓ |
| No rand/rand_chacha bump | untouched ✓ |
| All existing tests pass | 77 pre-existing + 4 new = 81/81 ✓ |

---

## Concerns / deferred

- The overshoot bound (documented in comments) means a single wave can exceed
  the cap by `sum(node costs)`. For workflows with many high-cost parallel
  nodes this could be significant. A stricter bound would require pre-launch
  cost estimates (possible via the existing `estimate` module) — deferred.
- `join_all` does not bound concurrency; a wave with N nodes launches all N
  at once. A `futures::stream::FuturesUnordered` with a semaphore could cap
  in-flight count — deferred until a concrete need arises.

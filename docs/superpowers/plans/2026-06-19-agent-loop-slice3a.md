# Agent-loop slice 3a (cross-turn run-cost aggregation, unsigned) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Report the agent run's total **served cost** (USD) across turns + pause/resume, by capturing the per-turn cost the loop currently discards.

**Architecture:** Add `cost_usd` to `RunUsage` (the per-turn + accumulated usage bundle the loop already sums, persists in `StoredRun.usage`, restores on resume, and returns on `Run`). `GatewayCompleter::complete` reads `headers.cost_breakdown.cost_usd` (stops dropping the `Dispatched` headers); `run_loop_core` adds one accumulation line. Cost therefore aggregates across turns + resume for free — no new `Run`/`StoredRun` field, no `LoopOutcome` change, no `TurnCompleter` signature change. Unsigned, like the gateway's `x-tokentrimmer-cost-usd` header.

**Tech Stack:** Rust, `crates/core` (`crates/core/src/routes/agent_run.rs`); reuses `chat::CompletionOutcome::Dispatched { headers: Box<CompletionHeaders> }` + `CompletionHeaders.cost_breakdown.cost_usd` (f64, the served cost).

**Spec:** `docs/superpowers/specs/2026-06-19-agent-loop-slice3a-design.md`.

**Gate (after each compiling task + final):** `cargo test -p tt-core --lib --tests`. **`cargo fmt -p tt-core -- --check` before push** (public CI gates rustfmt). Clippy: `cargo clippy -p tt-core --all-targets`. No DB gate.

---

## File Structure

All changes in `crates/core/src/routes/agent_run.rs` (the loop's home — `RunUsage`, `GatewayCompleter`, `run_loop_core`, the test stubs all live here). No new files.

---

## Task 1: add `RunUsage.cost_usd` (data field + serde back-compat) and fix every literal

**Files:**
- Modify: `crates/core/src/routes/agent_run.rs` — `RunUsage` def (`:55`); the 3 fully-specified non-completer `RunUsage {…}` literals: `Stub` (`:1515`), `RecordingStub` (`:1541`), `stored_run_roundtrips_through_cache` test (`:1888`). (The `GatewayCompleter` literal at `:764` is rewritten in Task 2.)
- Test: `agent_run.rs` `tests` mod.

- [ ] **Step 1: Write the failing test** — add to the `tests` mod:
```rust
#[test]
fn run_usage_deserializes_without_cost_usd() {
    // A RunUsage persisted before this deploy has no cost_usd key; #[serde(default)]
    // must default it to 0.0 (so old StoredRun.usage round-trips).
    let ru: RunUsage = serde_json::from_str(r#"{"prompt_tokens":5,"completion_tokens":7}"#)
        .expect("back-compat deserialize");
    assert_eq!(ru.prompt_tokens, 5);
    assert_eq!(ru.completion_tokens, 7);
    assert_eq!(ru.cost_usd, 0.0);
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p tt-core --lib agent_run::tests::run_usage_deserializes_without_cost_usd 2>&1 | tail -12` — expect FAIL (`no field cost_usd` — both in the test and the existing literals once you add the field; compile error is expected until Step 3 fixes the literals).

- [ ] **Step 3: Add the field + fix the 3 literals.**
In the `RunUsage` struct (`:55`), add after `completion_tokens`:
```rust
    /// Accumulated SERVED cost (USD) across the run's turns — the sum of each
    /// turn's `x-tokentrimmer-cost-usd` (`CompletionHeaders.cost_breakdown.cost_usd`).
    /// Unsigned, like the per-request cost header. Distinct from `Run`/`StoredRun`'s
    /// `summarizer_tax_usd` (the 2c-2 measurement tax).
    #[serde(default)]
    pub cost_usd: f64,
```
Add `cost_usd: 0.0,` to the three fully-specified literals (use `cargo build -p tt-core --tests 2>&1 | grep "missing field" ` to confirm you got all of them — the compiler lists each):
- `Stub::complete`'s `RunUsage { prompt_tokens: 1, completion_tokens: 1 }` (`:1515`) → add `cost_usd: 0.0,`.
- `RecordingStub::complete`'s `RunUsage { prompt_tokens: 1, completion_tokens: 1 }` (`:1541`) → add `cost_usd: 0.0,`.
- `stored_run_roundtrips_through_cache`'s `RunUsage { prompt_tokens: 5, completion_tokens: 7 }` (`:1888`) → add `cost_usd: 0.0,`.
(`RunUsage::default()` call sites need no change — the derived `Default` covers the new field.)

- [ ] **Step 4: Run to verify it passes** — `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -8` — expect PASS (the new test + all existing agent_run tests). `cargo build -p tt-core --tests 2>&1 | grep -E "^error" || echo OK` (no missed literal).

- [ ] **Step 5: Commit**
```bash
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(agent-loop 3a): RunUsage.cost_usd field (serde-default, served cost accumulator)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: capture per-turn cost + accumulate across turns/resume

**Files:**
- Modify: `crates/core/src/routes/agent_run.rs` — `GatewayCompleter::complete` Dispatched arm (`:763-767`); `run_loop_core` accumulation (`:300-301`); add a `CostStub` test completer + tests.
- Test: `agent_run.rs` `tests` mod.

- [ ] **Step 1: Write the failing tests** — add to the `tests` mod (a `CostStub` returns a known per-turn cost so the accumulation is assertable; `0.25`/`0.5` are exact in f64):
```rust
/// A completer that returns a fixed per-turn served cost (+ a scripted message),
/// so a test can assert the loop accumulates `usage.cost_usd` across turns.
struct CostStub {
    script: std::sync::Mutex<Vec<Message>>,
    cost_per_turn: f64,
}
#[async_trait]
impl TurnCompleter for CostStub {
    async fn complete(
        &self,
        _req: ChatCompletionRequest,
        _is_mechanical: bool,
    ) -> Result<(Message, RunUsage), ApiError> {
        Ok((
            self.script.lock().unwrap().remove(0),
            RunUsage { prompt_tokens: 1, completion_tokens: 1, cost_usd: self.cost_per_turn },
        ))
    }
}

#[tokio::test]
async fn loop_accumulates_served_cost_across_turns() {
    // turn1: a gateway tool call (loop executes it), turn2: final answer → 2 turns.
    let stub = CostStub {
        script: std::sync::Mutex::new(vec![
            assistant_toolcall("find_route_for"),
            assistant_final(),
        ]),
        cost_per_turn: 0.25,
    };
    let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8).await;
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.turns, 2);
    assert_eq!(run.usage.cost_usd, 0.5); // 2 * 0.25, exact in f64
}

#[tokio::test]
async fn loop_cost_continues_from_restored_usage_on_resume() {
    // Simulate resume: run_loop_core started with a restored usage carrying prior
    // cost, + a CostStub adding 0.5/turn over 2 turns → 1.0 (carry-in) + 1.0 = 2.0.
    let stub = CostStub {
        script: std::sync::Mutex::new(vec![
            assistant_toolcall("find_route_for"),
            assistant_final(),
        ]),
        cost_per_turn: 0.5,
    };
    let out = run_loop_core(
        &stub,
        uuid::Uuid::nil(),
        "m".into(),
        vec![],
        vec![],
        8,
        0,    // turns_done
        0,    // summarized_upto
        RunUsage { prompt_tokens: 0, completion_tokens: 0, cost_usd: 1.0 }, // restored carry-in
        None, // summarizer
    )
    .await;
    match out {
        LoopOutcome::Terminal(run) => assert_eq!(run.usage.cost_usd, 2.0),
        _ => panic!("expected Terminal"),
    }
}
```
> NOTE: confirm the exact `run_loop_core` arg order before finalizing the resume test — after slice 2c-1 it is `(completer, id, model, messages, tools, max_turns, turns_done, summarized_upto, usage, summarizer)`. Match it.

- [ ] **Step 2: Run to verify they fail** — `cargo test -p tt-core --lib agent_run::tests::loop_accumulates_served_cost 2>&1 | tail -12` — expect FAIL (`run.usage.cost_usd` is `0.0`, not `0.5`, because the loop doesn't accumulate cost yet and `CostStub`'s cost never lands — actually it WILL be 0.0 since accumulation isn't wired). The resume test similarly returns `1.0` (carry-in only), not `2.0`.

- [ ] **Step 3: Capture per-turn cost in `GatewayCompleter::complete`.** Replace the Dispatched arm (`:763-767`):
```rust
            CompletionOutcome::Dispatched { response, .. } => {
                let usage = RunUsage {
                    prompt_tokens: response.usage.prompt_tokens,
                    completion_tokens: response.usage.completion_tokens,
                };
```
with (capture the headers it currently drops; `headers` is `Box<CompletionHeaders>` — field access auto-derefs):
```rust
            CompletionOutcome::Dispatched { response, headers } => {
                let usage = RunUsage {
                    prompt_tokens: response.usage.prompt_tokens,
                    completion_tokens: response.usage.completion_tokens,
                    cost_usd: headers.cost_breakdown.cost_usd, // served cost (x-tokentrimmer-cost-usd)
                };
```
(The rest of the arm — extracting the assistant message + returning `(msg, usage)` — is unchanged. The `CacheHit(_) => Err(ApiError::Internal(...))` guard is unchanged.)

- [ ] **Step 4: Accumulate in `run_loop_core`.** After the two existing accumulation lines (`:300-301`):
```rust
        usage.prompt_tokens += turn_usage.prompt_tokens;
        usage.completion_tokens += turn_usage.completion_tokens;
        usage.cost_usd += turn_usage.cost_usd; // served cost across turns (and resume, via the carried usage)
```

- [ ] **Step 5: Run to verify they pass** — `cargo test -p tt-core --lib agent_run:: 2>&1 | tail -10` — expect PASS (both new tests + all existing). `cargo build -p tt-core --tests 2>&1 | grep -E "^error" || echo OK`.

- [ ] **Step 6: fmt + clippy + commit**
```bash
cargo fmt -p tt-core
cargo fmt -p tt-core -- --check   # expect clean
cargo clippy -p tt-core --lib --tests 2>&1 | tail -8   # expect clean
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(agent-loop 3a): capture + accumulate per-turn served cost into run.usage.cost_usd

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: full gate + fmt/clippy

**Files:** none (verification).

- [ ] **Step 1: fmt** — `cargo fmt -p tt-core -- --check` (expect clean; run `cargo fmt -p tt-core` first if needed). Confirm `git diff --stat` touched only `agent_run.rs`.
- [ ] **Step 2: Full gate** — `cargo test -p tt-core --lib --tests 2>&1 | tail -15` — expect ALL green (additive `RunUsage` field; `TurnCompleter` return shape unchanged ⇒ no call-site ripple). Record pass/fail/ignored counts.
- [ ] **Step 3: Clippy** — `cargo clippy -p tt-core --all-targets 2>&1 | tail -12` — no warnings. (Do NOT `cargo test --all-targets` — benches hang ~36min.)
- [ ] **Step 4: Commit (if fmt changed anything)** — `git add -A && git commit -m "style(agent-loop 3a): rustfmt" || echo "nothing to commit"`.

---

## Notes for the implementer

- **Why no `Run`/`StoredRun`/`LoopOutcome` changes:** `cost_usd` rides `RunUsage`, which is already accumulated (`run_loop_core`), returned on terminal `Run`s, carried in `LoopOutcome::Paused`, persisted as `StoredRun.usage`, restored into `run_loop_core`'s `usage` carry-in on resume, and mapped by `StoredRun::to_run()`. So cost aggregates across turns AND pause/resume with zero extra plumbing. Do NOT add a parallel top-level cost field.
- **Served, unsigned:** `cost_breakdown.cost_usd` is the served cost (the `x-tokentrimmer-cost-usd` value). This is NOT a signed artifact (out of scope) and NOT the baseline/savings.
- **Cache hits can't occur in the loop** (per-turn cache bypass ⇒ always `Dispatched`), so `headers` is always present; the `CacheHit → Internal` guard stays.
- **f64 exactness:** the tests use `0.25`/`0.5`/`1.0` (exact in IEEE-754) so `assert_eq!` on the sum is safe.
- **CI:** public `cargo test (workspace)` is disk-flaky (`No space left on device` linking) → `gh run rerun <run-id> --failed`. Always `cargo fmt -p tt-core -- --check` before push.

# Server-side agent loop — slice 3a (cross-turn run-cost aggregation, unsigned)

**Status:** approved design (2026-06-19) · **Repo:** public OSS core (`crates/core`) · **Origin:** the `server-side-agent-loop` workstream, slice 3 (the final loop slice) decomposed into **3a** (this: aggregate the run's served cost across turns) → **3b** (SSE streaming of run events). Per the brainstorm, the "attestation" is an **unsigned** aggregation — consistent with the gateway's existing unsigned per-request `x-tokentrimmer-cost-usd` header; a *signed* artifact is explicitly out of scope (deferred / a cloud concern à la PROD-3).

## Problem
A `POST /v1/agent/runs` response reports token `usage` (`prompt_tokens`/`completion_tokens`) and the 2c-2 `summarizer_tax_usd` measurement tax, but **no served `$` cost**. Each turn's served cost is already computed (`complete_once` → `CompletionHeaders.cost_breakdown.cost_usd`, attached as `x-tokentrimmer-cost-usd`), but the loop's `GatewayCompleter::complete` **discards the headers**. This slice captures that per-turn cost and aggregates it across turns (and across pause/resume) into the run's reported cost.

## Decisions (locked in brainstorm)
1. **Ride `RunUsage`** (not a new top-level field): add `cost_usd` to the per-turn + accumulated `RunUsage` bundle, which the loop already sums, persists (`StoredRun.usage`), restores on resume, and returns on the `Run`. Cost therefore aggregates across turns AND pause/resume **for free** — no new `Run`/`StoredRun` field, no `LoopOutcome::Paused` change, no cumulative-merge handling (contrast 2c-1's `summarizer_tax_usd`, which lives outside `RunUsage` and needed explicit cross-segment summing).
2. **Served cost, unsigned.** Report the SERVED cost (sum of per-turn `cost_breakdown.cost_usd`) only — not baseline/savings, not a signed artifact. Matches the gateway's existing unsigned `x-tokentrimmer-cost-usd`.
3. **Decompose slice 3.** 3a (this) → 3b SSE streaming (which will emit `usage.cost_usd` as a final event). Signed attestation deferred.

## Verified seams (current code)
- **`RunUsage`** (`crates/core/src/routes/agent_run.rs:52-56`): `#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)] pub struct RunUsage { pub prompt_tokens: u64, pub completion_tokens: u64 }`. The loop accumulates it field-by-field (`usage.prompt_tokens += turn_usage.prompt_tokens; usage.completion_tokens += turn_usage.completion_tokens;`); `StoredRun.usage: RunUsage` is persisted/restored; `Run.usage` is returned; `to_run()` clones it. `run_loop_core` carries `usage` in/out (the resume carry-in param), so a restored `usage` continues accumulating on resume.
- **`GatewayCompleter::complete`** (`agent_run.rs`): on `CompletionOutcome::Dispatched { response, .. }` it builds `RunUsage { prompt_tokens: response.usage.prompt_tokens, completion_tokens: response.usage.completion_tokens }` and **drops `headers`**. Per-turn cache is bypassed (`tt_extras.cache=bypass`), so `complete_once` always returns `Dispatched` (the loop already guards `CacheHit` → `ApiError::Internal`), i.e. `headers` is always present on the live path.
- **`CompletionHeaders`** (`chat.rs:952`): carries `cost_breakdown: CostBreakdown`. **`CostBreakdown.cost_usd: f64`** is the served cost (`chat.rs:1612` `let cost_usd = cost_breakdown.cost_usd;` → the `x-tokentrimmer-cost-usd` value). `summarizer_tax_usd` (2c-2) is a separate measurement tax, unaffected.
- **Test completers** (`agent_run.rs` `#[cfg(test)] mod tests`): `Stub` + `RecordingStub` both return `RunUsage { prompt_tokens: 1, completion_tokens: 1 }` (will need `cost_usd: 0.0`).
- `TurnCompleter::complete(&self, req, is_mechanical) -> Result<(Message, RunUsage), ApiError>` — return shape UNCHANGED (cost rides inside the returned `RunUsage`); no signature ripple to the 6 `run_loop_core` call sites.

## Design

### 1. `RunUsage` gains `cost_usd`
```rust
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct RunUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Accumulated SERVED cost (USD) across the run's turns — the sum of each
    /// turn's `x-tokentrimmer-cost-usd` (`CompletionHeaders.cost_breakdown.cost_usd`).
    /// Unsigned, like the per-request cost header. Distinct from
    /// `summarizer_tax_usd` (the 2c-2 measurement tax, which lives on `Run`/`StoredRun`).
    #[serde(default)]
    pub cost_usd: f64,
}
```
`#[serde(default)]` keeps pre-deploy persisted `StoredRun`s (whose `usage` JSON has no `cost_usd`) deserializable (`0.0`).

### 2. Capture per-turn cost in `GatewayCompleter::complete`
Destructure the headers it currently drops and set the per-turn cost on the returned usage:
```rust
match chat::complete_once(self.state, &ctx, prep).await? {
    CompletionOutcome::Dispatched { response, headers } => {
        let usage = RunUsage {
            prompt_tokens: response.usage.prompt_tokens,
            completion_tokens: response.usage.completion_tokens,
            cost_usd: headers.cost_breakdown.cost_usd,
        };
        // ... (unchanged: extract the assistant message, return (msg, usage))
    }
    CompletionOutcome::CacheHit(_) => Err(ApiError::Internal(/* unchanged guard */)),
}
```

### 3. Accumulate in `run_loop_core`
Next to the existing token accumulation:
```rust
usage.prompt_tokens += turn_usage.prompt_tokens;
usage.completion_tokens += turn_usage.completion_tokens;
usage.cost_usd += turn_usage.cost_usd; // NEW — served cost across turns
```
No other loop change: `usage` is already returned on Terminal `Run`s, carried in `LoopOutcome::Paused`, persisted to `StoredRun.usage`, restored into `run_loop_core` on resume, and mapped by `to_run()`. So `cost_usd` aggregates across turns + pause/resume with no further wiring.

### 4. Update every fully-specified `RunUsage { … }` literal
Adding a field forces every literal that names all fields to add `cost_usd` (the compiler/behavior-gate catches any miss). There are **4** non-definition literals in `agent_run.rs`: `GatewayCompleter::complete` (rewritten in §2 to set `cost_usd: headers.cost_breakdown.cost_usd`), the `Stub` and `RecordingStub` test completers (`cost_usd: 0.0`), and the `stored_run_roundtrips_through_cache` test's `RunUsage { prompt_tokens: 5, completion_tokens: 7 }` (`cost_usd: 0.0`). Add `cost_usd: 0.0` to the three non-completer literals (or use `..Default::default()`); any `RunUsage::default()` call sites are already covered by the derived `Default`.

## Components
| Unit | Location | Responsibility |
|---|---|---|
| `RunUsage.cost_usd` (`#[serde(default)]`) | `agent_run.rs` | accumulated served cost, riding the existing usage bundle |
| `GatewayCompleter::complete` header capture | `agent_run.rs` | read `headers.cost_breakdown.cost_usd` into the per-turn `RunUsage` |
| `run_loop_core` accumulation (`+= turn_usage.cost_usd`) | `agent_run.rs` | sum across turns (and resume, via the carried `usage`) |
| the 3 non-completer `RunUsage {…}` literals `cost_usd: 0.0` (2 stubs + the roundtrip test) | `agent_run.rs` tests | keep the provider-free loop tests compiling |

## Error handling / edge cases
- Cache hit in the loop is impossible (per-turn bypass ⇒ always `Dispatched`); the existing `CacheHit → Internal` guard is unchanged, so `headers` is always available. A turn that errors (`completer.complete` → `Err`) ends the run `Failed` BEFORE accumulating that turn's usage — unchanged; the failed turn contributes no cost (it produced no served response). Unpriced model ⇒ `cost_breakdown.cost_usd` is whatever `compute_cost` yields today (the same value the `x-tokentrimmer-cost-usd` header reports) — no special-casing here. Resume: the restored `StoredRun.usage.cost_usd` continues accumulating (the token fields already do). `/v1/chat/completions` untouched (this is loop-only).

## Testing
- **Loop accumulation:** a stub completer returning a known per-turn `cost_usd` (e.g. 0.25) over N turns → assert the terminal `run.usage.cost_usd == N * 0.25` (use an exact-representable value; `0.25` is exact in f64). A 0-turn / first-turn-final case → `cost_usd == per-turn value`.
- **Serde back-compat:** a `RunUsage` (and a `StoredRun` whose `usage`) JSON omitting `cost_usd` deserializes to `0.0` (mirrors the 2c-1/2c-2 `#[serde(default)]` back-compat tests).
- **Resume continuity:** `run_loop_core` started with a restored `usage { cost_usd: 1.0, .. }` + a stub adding 0.5/turn over 2 turns → terminal `cost_usd == 2.0` (carry-in + segment).
- **Behavior-preservation:** `cargo test -p tt-core --lib --tests` at baseline (additive `RunUsage` field; `TurnCompleter` return shape unchanged ⇒ no call-site ripple). `cargo fmt -p tt-core -- --check` + `cargo clippy -p tt-core --all-targets` clean (always `fmt --check` before push — public CI gates fmt).

## Non-goals (3a)
SSE streaming (3b). Signed attestation (deferred / cloud à la PROD-3). Baseline/savings reporting on the run (only served cost). No change to `summarizer_tax_usd`, the down-route/summarize levers, or `/v1/chat/completions`.

## Rollout
Single public PR. Additive + default-safe (a run with no turns reports `cost_usd: 0.0`; existing fields/behavior unchanged). Public CI (`cargo test (workspace)`; `fmt + clippy`; `tt inspect .`; determinism untouched). No DB/cloud changes. Redis optional (cost rides `StoredRun.usage` when persisted).

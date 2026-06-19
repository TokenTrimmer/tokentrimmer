# Server-side agent loop — slice 2a (down-route mechanical sub-step turns, auto_pause-protected)

**Status:** approved design (2026-06-18) · **Repo:** public OSS core (`crates/core`) · **Origin:** the `server-side-agent-loop` workstream (COST-3(U) Sub-lever 3). Slices 1a/1b (#190/#192) gave the gateway a stateful hybrid loop. This slice makes the loop **down-route a mechanical sub-step turn** to the route's `route_mechanical_to` model — the realization of Sub-lever 3 that a per-request proxy structurally couldn't do.

## Problem
`route_mechanical_to` (on `AgenticBudget`) is "marked but unwired": `agentic_budget::plan()` emits a `subagent_lane:<target>` *warning* but nothing down-routes. The gateway now owns the loop, so it can identify a "mechanical" turn (the model merely digesting read-only tool output) and serve that turn from a cheaper model — the costliest-agentic-traffic lever. The risk is **parity**: down-routing a genuine *reasoning* turn degrades quality.

## Decisions (locked in brainstorm)
1. **Decomposed.** Slice 2 = 2a (this: down-route) + 2b (substep-cache serve; needs embedder+pgvector) + 2c (judge-gated summarize; needs judge+summarizer LLM). 2a first — self-contained, no new infra.
2. **Conservative eligibility.** A turn is mechanical iff the immediately-preceding assistant turn called ≥1 tool and **every** tool_call is read-only (`substep_cache::classify_substep == ReadOnly`) — i.e. the loop just appended only read-only tool results and the next model turn digests them. Any client/mutating tool in that prior turn ⇒ not mechanical. The first turn is never mechanical.
3. **auto_pause self-revert (reuse, no new machinery).** A down-routed mechanical turn is attributed as a routed serving (served = `route_mechanical_to`, `matched_route_id` = the run's route), so the **existing** paired-quality judge samples it and the **existing** `route_autopause` pauses the route when the windowed pass-rate drops below the floor (0.90). The loop respects the pause: a paused route ⇒ no down-route (original model). Self-reverting.
4. **Default-off.** No `route_mechanical_to` on the run's route ⇒ no down-route; the loop behaves exactly as 1a/1b.

## Verified seam (current code)
- `crates/core/src/routes/chat.rs` `prepare(...)` (≈:2218): `let route_match = apply_routing(state, ctx, req, ...)?;` then captures `matched_route_id = route_match.route_id`, `route_paused = route_match.paused`, `route_matched_name`, `route_agentic_budget = route_match.agentic_budget.clone()`. `model_was_rewritten = matched_route_id.is_some() && !route_paused`. Provider is (re)resolved via `state.registry.resolve(m)` after rewrites. `Prepared` carries `matched_route_id`/`route_paused`/`route_matched_name`; `complete_once` derives its quality **baseline from `matched_route_id.is_some()`** (so keeping `matched_route_id` set on a down-routed turn is what makes the judge pair served-vs-baseline).
- `tt_routing::AgenticBudget.route_mechanical_to: Option<String>` (a `RouteAction` field; the run's matched route supplies it).
- `crates/core/src/routes/agent_run.rs`: `run_loop_core(completer,id,model,messages,tools,max_turns,turns_done,usage)->LoopOutcome`; `#[async_trait] TurnCompleter::complete(&self, ChatCompletionRequest)->Result<(Message,RunUsage),ApiError>`; `GatewayCompleter::complete` builds the per-turn request + calls `prepare`+`complete_once`. `substep_cache::{classify_substep, SubstepKind::ReadOnly}`.
- The quality-judge sampling + `route_autopause` (per-route verdict window, floor 0.90, pause via `route_pauses`) are **existing + tested** — 2a does NOT touch them.

## Design

### 1. Mechanical-turn detection (`agent_run.rs`)
A pure helper:
```rust
/// A turn is "mechanical" when the model is about to digest ONLY read-only
/// tool output: the most recent assistant turn called >=1 tool and EVERY
/// tool_call is read-only (classify_substep == ReadOnly). Conservative — any
/// client/mutating tool, or no prior assistant tool turn, => not mechanical.
fn is_mechanical_continuation(messages: &[Message]) -> bool
```
Implementation: scan back over the trailing `Message::Tool` results to the assistant turn that produced them; return true iff that assistant turn had ≥1 `tool_calls` and `classify_substep(name) == ReadOnly` for all of them. `run_loop_core` computes `let is_mechanical = is_mechanical_continuation(&messages);` immediately before each `completer.complete(...)` (it's `false` on the first turn since there's no prior assistant-tool turn).

### 2. Thread the signal
`TurnCompleter::complete` gains the flag: `async fn complete(&self, req: ChatCompletionRequest, is_mechanical: bool) -> Result<(Message, RunUsage), ApiError>`. `run_loop_core` passes the computed value; the test stub ignores it. `GatewayCompleter::complete` forwards it into `prepare`.

### 3. `prepare` gains `is_mechanical` + the down-route block
`prepare(...)` gains a trailing `is_mechanical: bool` param. After the route capture (`route_agentic_budget`/`route_paused`/`matched_route_id`) and before the provider (re)resolve, insert:
```rust
// Sub-lever 3 (agent-loop only): down-route a mechanical sub-step turn to the
// route's route_mechanical_to model, IF the route opted in AND is not paused.
// Keeping matched_route_id set means the existing paired-quality judge +
// route_autopause treat this as a routed serving and self-revert on regression.
if is_mechanical && !route_paused {
    if let Some(target) = route_agentic_budget.as_ref().and_then(|ab| ab.route_mechanical_to.clone()) {
        if target != req.model {
            req.model = target;                 // down-route this digest turn
            model_was_rewritten = true;         // baseline pricing vs the original model
            // provider is (re)resolved below for the (possibly new) req.model
        }
    }
}
```
The existing provider (re)resolve already runs after this point for the final `req.model`, so the down-routed model gets the right provider/creds. `matched_route_id` stays the route's id ⇒ the down-routed turn is a routed serving (quality-sampled + auto_pause-attributed). If `route_paused` (auto_pause already fired) the block is skipped ⇒ original model ⇒ self-revert. If `route_mechanical_to`'s model can't be resolved by the registry, the existing resolve path errors/falls back exactly as it does for any bad model — but since the operator configured `route_mechanical_to`, treat an unresolvable target as: keep the original model + push a `mechanical_route_unresolved:<target>` warning (don't fail the run). (Implementer: place the override so an unresolvable target degrades gracefully to the original model.)

### 4. The chat handler is behavior-preserving
`crates/core/src/routes/chat.rs::handler` calls `prepare(..., /*is_mechanical=*/ false)`. With `is_mechanical=false` the new block is a no-op, so `/v1/chat/completions` is byte-behavior-identical. (The 753-test baseline is the gate.)

### 5. Quality + auto_pause — pure reuse
No new code in the quality/pause path. Because the down-routed mechanical turn has `matched_route_id` set + `req.model != requested`, the existing paired-quality judge samples it (served = `route_mechanical_to` vs baseline = the original model) and records verdicts keyed to the route; `route_autopause` pauses the route when the pass-rate < floor; the loop's per-turn `prepare` reads the (now-paused) route and stops down-routing. The down-routed agent turns' quality therefore also flows into the PROD-3 signed attestation's pass-rate.

## Components
| Unit | Location | Responsibility |
|---|---|---|
| `is_mechanical_continuation` | `agent_run.rs` | pure detection from the message tail (all-read-only prior tool turn) |
| `TurnCompleter`/`run_loop_core`/`GatewayCompleter` | `agent_run.rs` | thread `is_mechanical` per turn |
| `prepare` down-route block | `chat.rs` | down-route a mechanical turn to `route_mechanical_to` (opt-in, pause-respecting, route-attributed); `handler` passes `false` |

## Error handling / edge cases
- No `route_mechanical_to` ⇒ no-op (default-off). Route paused ⇒ original model. Unresolvable `route_mechanical_to` ⇒ original model + warning (run continues). First turn ⇒ not mechanical. A mechanical turn that then calls more tools ⇒ next iteration re-evaluates normally. `/v1/chat/completions` unaffected (`is_mechanical=false`).
- The down-route changes only `req.model` for that turn; the transcript/tools/turns accounting are unchanged.

## Testing
- **Detection** (pure unit, stubbed messages): read-only-tool continuation → `true`; a prior turn with a client/mutating tool → `false`; mixed (read-only + client) prior turn → `false`; first turn / no prior tool turn → `false`.
- **Down-route decision**: a stub `TurnCompleter` captures the `is_mechanical` it receives → assert `run_loop_core` passes `true` exactly on mechanical turns. For the `prepare` block, a focused test (or the existing prepare/routing test harness) asserting: `is_mechanical && route_mechanical_to=Some(m) && !paused` ⇒ `req.model==m` + `matched_route_id` retained; `paused` ⇒ unchanged; `None` ⇒ unchanged; `is_mechanical=false` ⇒ unchanged.
- **Behavior-preservation**: the full `cargo test -p tt-core --lib --tests` stays at the 753 baseline (chat handler passes `is_mechanical=false`; the `prepare` signature change ripples only to its two callers — `handler` and `GatewayCompleter`).
- **Default-off**: a run whose route has no `route_mechanical_to` produces byte-identical per-turn models to 1b.
- Quality-judge/auto_pause are existing+tested; 2a adds only an assertion that a down-routed mechanical turn keeps `matched_route_id` (so the existing sampling fires).

## Non-goals (2a)
Sub-lever 4 substep-cache serve (2b); 2b summarize (2c). No new quality/pause machinery. No change to `/v1/chat/completions`. No broadening of the mechanical heuristic beyond read-only-continuation (later tuning, same auto_pause safety). No per-turn cache-lane key change (the model-keyed L2 lane already isolates the down-routed model).

## Rollout
Single public PR. The `prepare` signature gains `is_mechanical` (2 callers updated); behavior-preserving for the chat path (gate: 753 baseline). Public CI (`cargo test (workspace)` — disk-flaky, rerun if needed; fmt+clippy; tt inspect .; determinism untouched). No DB/cloud changes. Redis not required (down-route works on inline + persisted runs alike).

# Server-side agent loop — slice 2c-2 (live judge degrade-ratchet: operator-opens, judge-shuts)

**Status:** approved design (2026-06-19) · **Repo:** public OSS core (`crates/core`) · **Origin:** the `server-side-agent-loop` workstream (COST-3(U) Sub-lever 2b). Slice 2c-1 (#194) shipped the summarize *mechanism* behind an operator-promoted `ConfigSummaryGate`. This slice adds the **live blind-paired judge** that samples committed summaries and **ratchets a class shut** when its summaries start dropping material information — so summarize self-protects without an operator watching.

## Problem
2c-1's gate is purely operator-promoted: a class an operator allowlists summarizes forever, with no automated quality feedback. The risk of a lossy lever is parity drift — a class whose summaries silently start omitting load-bearing information. The blind paired judge + `map_summary_verdict` recall-of-baseline contract already exist (the down-route judge uses them); this slice wires them to the summarize path as a **safety ratchet** (mirroring how 2a's `route_autopause` pauses a down-route on a windowed quality dip).

## Decisions (locked in brainstorm)
1. **Operator opens, judge shuts (not "judge earns from zero").** Keep 2c-1's allowlist as the OPENER; the judge is a degrade-RATCHET only. `committable = allowlisted && !shut`. No chicken-and-egg (allowlisted classes commit immediately, producing summaries to judge), no shadow-exploration cost. `AdaptiveSummaryGate` (open-on-accumulated-Acceptable) does NOT fit this polarity and stays unwired (it is the gate for a future auto-earn/shadow slice).
2. **Windowed pass-rate floor** (mirrors 2a's `route_autopause`): shut a class when its recent judged pass-rate (`acceptable / (acceptable+degraded)`, Unclear excluded) drops below a floor (default 0.90) over a minimum sample — robust to a single noisy verdict.
3. **Cooldown half-open recovery:** a shut class re-opens after a cooldown TTL (default 1h); on re-open its verdict window is cleared (fresh trial), so post-cooldown real commits are re-judged — Acceptable keeps it open, another dip re-shuts + restarts the cooldown. No shadow needed (recovery uses real committed traffic after the cooldown).
4. **In-process per replica.** The ratchet state (verdict windows + shut-set) is in-memory `Mutex`, like `AdaptiveSummaryGate`'s tally, the L2 adaptive thresholds, and the L2-hit rate cap. Resets on restart; each replica learns from its own sampled verdicts. No new infra.
5. **Judge tax = a detached measurement tax on telemetry** (OTel span, like the quality judge's `judge_cost_usd`), NOT folded into the run's `summarizer_tax_usd` (which stays the in-band summarize cost) and NOT into the route-keyed `JudgeSink`/PROD-3 attestation (summaries aren't routed servings).

## Verified seams (read against current code)
- **`judge_paired`** (`crates/core/src/quality_sample.rs:853`): `pub async fn judge_paired(judge: &dyn PairedJudgeProvider, input: &str, baseline_answer: &str, optimized_answer: &str, order: AbOrder, both_orders: bool) -> Result<PairedJudgeOutcome, PairedJudgeFailure>`. `PairedJudgeOutcome { verdict: JudgeVerdict, reason: String, judge_cost_usd: Option<f64>, orders_judged: u8, orders_agreed: Option<bool> }`. The returned `verdict` is **already the OPTIMIZED slot's** mapped recall-of-baseline verdict (`map_pair_verdict` internal) — so `judge_paired(.., baseline=original, optimized=summary, ..).verdict` is directly the summary's `JudgeVerdict` (no separate `map_summary_verdict` call needed; that fn is the same contract).
- **`GatewayLlmJudge`** (`quality_sample.rs:998`): `GatewayLlmJudge::new(provider: Arc<dyn Provider>, judge_model: String, ctx: RequestContext)` + `.with_call_timeout(Duration)`; impl `PairedJudgeProvider`. `build_paired_request` (`:1037`) frames a strict **information-preservation** judge (system prompt: "preserve material information — facts, numbers, caveats, actionable content; verbosity/length must NOT matter"; replies EQUIVALENT/A_MISSING/B_MISSING/UNCLEAR), `temperature 0`, `max_tokens 96`, deterministic. `input` is the "user INPUT" framing both answers respond to.
- **Sampling**: `should_sample(trace_id: Uuid, rate: f64) -> bool` (`:581`, deterministic hash, no RNG); `ab_order_for(trace_id: Uuid) -> AbOrder` (`:619`, distinct salt). `JudgeConfig { sample_rate, judge_model, baseline_timeout, .. }` on `AppState.judge_config` (`DEFAULT_JUDGE_MODEL="gpt-4o-mini"`).
- **The detached-judge pattern** (`crates/core/src/routes/chat.rs:5410-5474`): resolve the judge model's own provider (`state.registry.resolve(judge_model)`) + creds (`resolve_credentials_for(state, org_id, provider_id, raw_bearer, true)`), build a `judge_ctx`, `GatewayLlmJudge::new(...).with_call_timeout(baseline_timeout)`, then a detached `tokio::spawn` (zero added latency, owned data only). 2c-2 mirrors this with a custom spawn around `judge_paired` (no baseline re-dispatch — both texts are in hand).
- **2c-1 assets** (`crates/core/src/routes/agent_run.rs`): `GatewayTranscriptSummarizer { state: &AppState, org_id, raw_bearer, base_ctx: RequestContext (carries trace_id), gate: Arc<dyn SummaryGate>, cfg, base_model, base_provider_id, summarizer_model, deadline }` — already holds the gate Arc + the run's identity + `base_ctx.trace_id`. `summarize_before_turn(&self, messages: &mut Vec<Message>, summarized_upto: &mut u32) -> Option<f64>` iterates `eligible` tool **ordinals** and commits a summary by **mutating `messages[idx]` in place** — it does NOT use `SummaryEdit`/`SummaryOutcome.committed` (that is the separate, unused `SummarizeStep::apply`/`VolatileTail` path; confirmed by the `summarize_judge.rs` doc that the loop "applies its OWN loop-level token-true gate … not a VolatileTail"). `summarize_judge::{SummaryGate, ConfigSummaryGate, parse_trusted_classes, NeverCommitGate}`; `SummaryGate { fn is_committable(&self, class: &str) -> bool }` (`:68`). `AdaptiveSummaryGate` already has an inherent `record_verdict` (a different name) — no collision with the new trait method `record_summary_verdict`. `AppState.summary_gate: Arc<dyn SummaryGate>` (NeverCommit default in `new`; `ConfigSummaryGate::from_env` in `with_default_providers`).
- **`JudgeVerdict`** (`tt_plan_core::quality`): `Acceptable | Degraded | Unclear`. `verdict_str` (`quality_sample.rs:550`).

## Design

### 1. `SummaryGate` gains a default-no-op verdict sink
Extend the trait (backward-compatible):
```rust
pub trait SummaryGate: Send + Sync {
    fn is_committable(&self, class: &str) -> bool;
    /// Feed one blind-paired judge verdict for a committed summary of `class`
    /// (the detached judge write-side). Default no-op — only the ratchet gate
    /// acts on it; `NeverCommitGate`/`ConfigSummaryGate`/`AdaptiveSummaryGate`
    /// ignore it.
    fn record_summary_verdict(&self, _class: &str, _verdict: JudgeVerdict) {}
}
```
The summarizer already holds `gate: Arc<dyn SummaryGate>`, so it records on the dyn gate — **no new `AppState` field, no downcast**.

### 2. `RatchetSummaryGate` (new, `summarize_judge.rs`)
```rust
pub struct RatchetSummaryGate {
    trusted: HashSet<String>,           // operator allowlist (parse_trusted_classes)
    cfg: RatchetConfig,                 // floor, window, min_samples, cooldown
    state: Mutex<HashMap<String, ClassRatchet>>, // per-class verdict window + shut_at
}
struct ClassRatchet { window: VecDeque<bool>, shut_at: Option<Instant> } // window: true=Acceptable,false=Degraded
pub struct RatchetConfig { pub floor: f64, pub window: usize, pub min_samples: usize, pub cooldown: Duration }
```
- `RatchetSummaryGate::from_env()` — reuses `parse_trusted_classes(TT_SUMMARIZE_TRUSTED_CLASSES)`; `RatchetConfig` from `TT_SUMMARIZE_JUDGE_FLOOR` (0.90), `TT_SUMMARIZE_JUDGE_WINDOW` (20), `TT_SUMMARIZE_JUDGE_MIN_SAMPLES` (5), `TT_SUMMARIZE_JUDGE_COOLDOWN_SECS` (3600), all with defaults. Empty allowlist ⇒ trusts nothing (== `NeverCommitGate`).
- `is_committable(class) = trusted.contains(class) && !shut_now(class)`, where `shut_now` reads the class's `shut_at`: if `Some(t)` and `t.elapsed() < cooldown` ⇒ shut; if `Some(t)` and `t.elapsed() >= cooldown` ⇒ **half-open transition**: clear the window + clear `shut_at` (fresh trial), return not-shut. (The half-open clear happens lazily on the read.)
- `record_summary_verdict(class, verdict)`: `Unclear` ⇒ ignore (no valence). Push `Acceptable→true / Degraded→false` onto the class window (cap at `window`, pop front). If `len >= min_samples` and `acceptable_rate < floor` ⇒ set `shut_at = Some(Instant::now())`. Poisoned-lock recovery (`unwrap_or_else(|e| e.into_inner())`) like `AdaptiveSummaryGate` — never panic on the read/commit path.
- `Instant` (monotonic) for the cooldown clock — Rust std, fine (no determinism/replay constraint here; this is per-process operational state).

### 3. Judge sampling in `GatewayTranscriptSummarizer::summarize_before_turn`
**The real commit site (verified — not the `SummarizeStep`/`SummaryEdit` path):** `summarize_before_turn` iterates `eligible` tool **ordinals** (`Vec<usize>`), and for each `idx` it has `class` (`resolve_summary_class(messages, idx)`) + `original` (the pre-edit `Message::Tool` text) + `summary` (the dispatched text), then on a passing `token_true_ok` it **mutates `messages[idx]` in place** (`*content = MessageContent::Text(summary)`) and returns only `Option<f64>` (the tax). There is **no `SummaryEdit` and no `committed` vec** on this path (those belong to the unused `SummarizeStep::apply`/`VolatileTail` path). So the judge spawn hangs off that in-place commit, reading `tool_call_id` off `messages[idx]`'s `Message::Tool` **before** the overwrite (or using `idx` as the sample sub-key).

After a commit, sample + spawn the judge **detached**. CRITICAL: the spawned future is `'static + Send`, and `self.state` is a **borrow** — so bind **owned** clones (incl. `let state = self.state.clone();` — `AppState: Clone`, the chat.rs:5429 precedent) BEFORE `tokio::spawn`; never reference `self`/`self.state` inside the `async move`:
```rust
// before the in-place overwrite, while messages[idx] is still the original Tool block:
let tool_call_id = match &messages[idx] { Message::Tool { tool_call_id, .. } => tool_call_id.clone(), _ => String::new() };
// per-edit sampling key (uniform across edits): a Uuid digest of (trace_id, tool_call_id)
let key = sample_key(self.base_ctx.trace_id, &tool_call_id);
if should_sample(key, self.state.judge_config.sample_rate) {
    // OWNED captures only — no &self / &self.state in the move:
    let state = self.state.clone();            // AppState: Clone
    let gate = self.gate.clone();              // Arc<dyn SummaryGate>
    let org_id = self.org_id;
    let raw_bearer = self.raw_bearer.clone();
    let base_ctx = self.base_ctx.clone();      // RequestContext: Clone
    let (original, summary, class) = (original.clone(), summary.clone(), class.clone());
    let input = latest_user_text(messages);    // owned String
    tokio::spawn(async move {
        // resolve the JUDGE model's own provider + creds inside the task (mirror chat.rs:5429-5462)
        let Some(provider) = state.registry.resolve(&state.judge_config.judge_model) else { return };
        let Some(creds) = chat::resolve_credentials_for(&state, org_id, provider.id(), &raw_bearer, true).await else { return };
        let judge_ctx = RequestContext { credentials: creds, ..base_ctx };
        let judge = GatewayLlmJudge::new(provider, state.judge_config.judge_model.clone(), judge_ctx)
            .with_call_timeout(state.judge_config.baseline_timeout);
        // summary is the OPTIMIZED arg; the matching `order` makes the returned verdict read as the SUMMARY's recall verdict.
        match judge_paired(&judge, &input, &original, &summary, ab_order_for(key), false).await {
            Ok(outcome) => {
                // telemetry: a DEDICATED span-attr namespace tokentrimmer.summary_judge.{verdict,cost_usd,class}
                // (do NOT reuse the route-keyed QualityVerdictAttributes — it carries requested/served_model that don't apply).
                gate.record_summary_verdict(&class, outcome.verdict);
            }
            Err(_failure) => { /* judge error/timeout ⇒ NO verdict recorded (fail-open: a flaky judge
                                  must never shut a class); telemetry only */ }
        }
    });
}
```
- **`order` must match the optimized slot:** pass `summary` as `judge_paired`'s `optimized_answer` **and** the same `ab_order_for(key)` as `order`, so the returned `verdict` is the summary's recall-of-baseline verdict (the snippet does this; `judge_paired` maps internally — no separate `map_summary_verdict` call needed).
- `input` = the run's task context: `latest_user_text(messages)` (the most-recent `Message::User` text; fallback `""` — the judge still compares A/B information-preservation).
- `both_orders = false` (single order — cheap; two-order mode is a later cost/robustness knob).
- Detached ⇒ the run may return before the verdict lands; the verdict shuts the class for **future** turns/runs (like 2a's autopause affects future requests, not the in-flight one).
- Judge model = `state.judge_config.judge_model` (the existing cheap scorer), resolved with its OWN provider+creds (NOT the summarizer model's, NOT the turn's). Judge tax = `PairedJudgeOutcome.judge_cost_usd`, recorded on the dedicated `tokentrimmer.summary_judge.*` span attribute (the same span-attribute mechanism `tt_telemetry` uses for `tokentrimmer.quality.judge_cost_usd`), **NOT** folded into `summarizer_tax_usd`.

### 4. Prod wiring
`with_default_providers` builds one `Arc<RatchetSummaryGate>` via `RatchetSummaryGate::from_env()` and sets `summary_gate` to it (replacing 2c-1's `ConfigSummaryGate`). `AppState::new` keeps `NeverCommitGate` (tests unaffected). The summarizer's existing `gate.clone()` is this ratchet, so `record_summary_verdict` reaches it.

### 5. Default-off + behavior preservation
Empty `TT_SUMMARIZE_TRUSTED_CLASSES` ⇒ `is_committable` always false ⇒ no commits ⇒ no judge spawns ⇒ **byte-identical to 2c-1's default-off (and thus to 1b)**. `/v1/chat/completions` untouched (loop-only). With an allowlist, a class summarizes + is sampled-judged; a sustained Degraded trend shuts it (stops summarizing) for the cooldown, then half-open re-trials.

## Components
| Unit | Location | Responsibility |
|---|---|---|
| `SummaryGate::record_summary_verdict` (default no-op) | `summarize_judge.rs` | the judge write-side on the dyn gate; non-ratchet gates ignore it |
| `RatchetSummaryGate` + `RatchetConfig` + `from_env` | `summarize_judge.rs` (+ `pub use` it from `agentic_budget/mod.rs` alongside the sibling gates, so `state.rs` can name it) | operator allowlist + per-class windowed pass-rate ratchet + cooldown half-open (in-process) |
| judge spawn (`sample_key`, detached `judge_paired`) | `agent_run.rs` (`summarize_before_turn` + a helper) | sample a committed edit, blind-judge original-vs-summary, feed the gate, telemeter the tax |
| `with_default_providers` wiring | `state.rs` | `summary_gate = RatchetSummaryGate::from_env()` (replaces `ConfigSummaryGate`) |

## Error handling / edge cases
- Empty allowlist / `NeverCommitGate` ⇒ no-op (default-off). Shut class ⇒ `is_committable` false ⇒ skipped (no commit, no judge). Cooldown expiry ⇒ half-open (window cleared, re-trialed on real traffic). `Unclear` verdict ⇒ ignored. **Judge dispatch error/timeout ⇒ NO verdict recorded** (fail-open: a flaky judge must never shut a class; the summary was already committed in-band). Unsampled commit ⇒ no judge (most commits). Judge provider/creds unresolvable ⇒ no spawn (telemetry note). Multiple replicas ⇒ each learns independently (in-process); a shut on one doesn't propagate (acceptable — conservative per-replica self-protection). Process restart ⇒ ratchet resets (allowlisted classes re-trial). The detached judge holds only owned clones (no borrow of the request path).

## Testing
- **`RatchetSummaryGate` (pure, no provider):** allowlist gating (`is_committable` false for non-allowlisted / empty); a sub-floor windowed pass-rate after `min_samples` shuts the class; below `min_samples` never shuts (robust to one Degraded); a shut class is not committable within cooldown; after cooldown the class is committable again AND its window is cleared (half-open fresh trial); `Unclear` ignored; `record_summary_verdict` is a no-op on `NeverCommitGate`/`ConfigSummaryGate` (default trait method). Use an injectable clock OR construct `shut_at` directly / a `cfg.cooldown = 0` case to test half-open deterministically without sleeping.
- **`sample_key` / sampling:** deterministic (same trace+tool_call_id ⇒ same decision); distinct keys spread.
- **Judge mapping (reuse):** `judge_paired(.., baseline=original, optimized=summary, ..).verdict` is the summary's verdict — a focused test with a stub `PairedJudgeProvider` returning `B_MISSING`/`A_MISSING`/`EQUIVALENT` asserts the mapped `JudgeVerdict` (Degraded when the summary slot is missing info, Acceptable on equivalent).
- **Default-off / behavior-preservation:** `cargo test -p tt-core --lib --tests` at the established baseline — empty allowlist ⇒ no commits/judges ⇒ byte-identical to 2c-1/1b; the detached spawn never fires on the default path. Add a **no-spawn regression** assert (mirror the existing 2c-1 default-off seam test): with `NeverCommitGate`/empty-allowlist, `summarize_before_turn` commits nothing and so the judge sample is never reached. The judge spawn (provider-bound) is integration-covered (like 2c-1's dispatch); unit tests cover the gate + mapping + sampling seams. `cargo fmt -p tt-core -- --check` + `cargo clippy -p tt-core --all-targets` clean (2c-1 lesson: public CI gates fmt — run `cargo fmt --check` before push).

## Non-goals (2c-2)
Judge-earns-from-zero / shadow exploration (a future slice; `AdaptiveSummaryGate` is its gate). Cross-replica / persisted shut-state (in-process here). Two-order judging by default. Feeding summary verdicts into the PROD-3 signed attestation (separate from the route-quality signal). SSE streaming + cross-turn attestation (slice 3). No change to the 2c-1 summarize mechanism, the token-true gate, or `/v1/chat/completions`.

## Rollout
Single public PR. Default-off (empty `TT_SUMMARIZE_TRUSTED_CLASSES` ⇒ total no-op; the ratchet only acts on allowlisted, committing classes). Public CI (`cargo test (workspace)`; `fmt + clippy` — run `cargo fmt --check` before push; `tt inspect .`; determinism untouched). No DB/cloud changes. Redis not required (in-process ratchet; works on inline + persisted runs).

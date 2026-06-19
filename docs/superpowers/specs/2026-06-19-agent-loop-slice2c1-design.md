# Server-side agent loop — slice 2c-1 (judge-gated summarize: the mechanism, operator-promoted gate)

**Status:** approved design (2026-06-19), revised after a grounded codebase-verification pass · **Repo:** public OSS core (`crates/core`) · **Origin:** the `server-side-agent-loop` workstream (COST-3(U) Sub-lever 2b). Slices 1a/1b/2a (#190/#192/#193) gave the gateway a stateful hybrid loop that can down-route a mechanical turn. This slice wires the **lossy summarize** lever into the loop: an *aging* tool-result block is summarized **once**, persisted into the accumulated transcript, token-true gated, default-off. The per-class trust gate is **operator-promoted** here; slice **2c-2** swaps in the live blind-paired judge that auto-opens/closes it.

## Problem
`summarize_judge::SummarizeStep` is a complete, unit-tested building block but is **never called from any request/loop path** — `chat.rs` wires only the lossless field-drop (`elide`). Its own doc says its commit is "a PLACEHOLDER byte heuristic … pending the deferred wiring that wraps this step inside the pipeline's token-true gate." The dominant cost in an agentic loop is the monotonically growing transcript: by turn N every turn re-sends the whole prior transcript. Summarizing *older* tool results shrinks every subsequent turn's input. The risk is **parity** (a lossy rewrite that drops load-bearing information) — bounded here by: keep-recent-verbatim, error-blobs-never, the token-true gate, and a trust gate that is OFF until a class is explicitly promoted.

## Decisions (locked in brainstorm + verification)
1. **Decompose 2c.** 2c-1 (this: the summarize mechanism behind an **operator-promoted** gate) → 2c-2 (the live blind-paired judge feeding `AdaptiveSummaryGate` so classes auto-open/close). 2b (substep-cache serve) was **deferred** with an honest in-code doc-closure folded into THIS slice.
2. **Loop-only scope.** Summarize runs inside the server-side loop over the loop's own accumulated `messages`. `/v1/chat/completions` stays **byte-identical**. The existing per-turn lossless field-drop in `prepare` is unchanged; summarize is the new loop-level lossy layer.
3. **Persist once.** An aging block is summarized exactly once (a `summarized_upto` tool-block watermark), the mutation persists into the loop transcript (and `StoredRun`), so every later turn — and every post-resume turn — sends it shrunk **without re-paying the summarizer tax**.
4. **Operator-promoted, default-off.** A new `ConfigSummaryGate` backed by an env allowlist `TT_SUMMARIZE_TRUSTED_CLASSES` (comma-separated tool names, **case-sensitive**). Empty/unset ⇒ behaves exactly like `NeverCommitGate` (total no-op). 2c-2 replaces it with the live `AdaptiveSummaryGate`.
5. **In-band Summarizer.** A production summarizer dispatches a cheap-model call **synchronously** inside the loop turn (the loop is already in-band per turn; the judge stays detached/post-response). The tax is a **measurement tax** surfaced on a new run-level `summarizer_tax_usd` field — never folded into any served/turn cost.

## Verified seams (read against current code — citations are post-verification)
- **`SummarizeStep`** (`passes/agentic_budget/summarize_judge.rs:280`): `new(keep_recent_pairs, gate, summarizer)`; `apply(&self, tail: &mut crate::passes::VolatileTail<'_>) -> SummaryOutcome` (operates on a `VolatileTail`, commits on a **byte** heuristic at `:366-374`). Per-block helpers `resolve_summary_class(&[Message], idx) -> String` (`:394`) and `is_error_blob(&str) -> bool` (`:416`) are **private** module fns. `SummaryOutcome { committed: Vec<SummaryEdit>, bytes_removed: u32, summarizer_tax_usd: Option<f64> }`. `sum_metered(Option<f64>, Option<f64>) -> Option<f64>` (unmetered side poisons the sum). `AgenticBudgetPlanner::plan_summary` (`mod.rs:266`) is the existing (also-unwired) `SplitRequest`/`VolatileTail` drive path. **This slice does NOT touch `apply`/`plan_summary`/`VolatileTail`** — it operates on the loop's flat `Vec<Message>` directly (old tool blocks are never in a cache-stable prefix, so the prefix-split machinery is unnecessary).
- **`SummaryGate`** trait (`summarize_judge.rs:64`): `fn is_committable(&self, class: &str) -> bool` — single `&self`-method, `Send+Sync` supertraits ⇒ **object-safe** (`Arc<dyn SummaryGate>` is valid). Impls: `NeverCommitGate` (no-op), `AlwaysCommitGate` (tests), `AdaptiveSummaryGate` (2c-2).
- **`Summarizer`** trait (`summarize_judge.rs:213`): `fn summarize(&self, class, content) -> SummarizeCall` — **synchronous**. `SummarizeCall { summary: Option<String>, cost_usd: Option<f64> }`. The pure tests use it; **production does NOT implement it** (the async dispatch can't live behind a sync `&self -> SummarizeCall`). The loop owns the await (see §3).
- **Route opt-in** (`tt_routing::AgenticBudget`, `crates/routing/src/lib.rs:340`): `elide_stale_tools: bool` (doc: "field-drop (lossless) + summarize (lossy, judge-gated)"), `keep_recent_pairs: u32` (default 3, `:367`), `clear_at_least_tokens: u32` (default 0). It is resolved **only inside `apply_routing`** (`chat.rs:6001`, `pub(crate)`), surfaced into `prepare`'s local `route_agentic_budget` (`chat.rs:2351`) — **NOT** available at `create_run` (`agent_run.rs:675`), which holds only the raw `CreateRunRequest`.
- **Tokenizer**: `tt_tokenize::estimate_input_tokens_for_model(provider: &str, model: &str, text: &str) -> Estimate` (`crates/tokenize/src/lib.rs:201`), `Estimate { tokens: u32, confidence: Confidence }`. Real call shape (the pipeline gate it mirrors): `estimate_input_tokens_for_model(cx.provider_id, cx.model, text).tokens` (`passes/mod.rs:283`).
- **Aux-dispatch seam**: `measurement::measured_single_dispatch(provider, req, ctx, deadline)` (`crates/core/src/measurement.rs:55`) — the metered, deadline-bounded path the judge baseline uses. `quality_sample`'s `GatewayLlmJudge::new(provider, judge_model, ctx)` resolves **its own** model's provider independently (`quality_sample.rs:1011`); `judge_cost_usd` is a measurement tax kept out of `request_logs.cost_usd` (`quality_sample.rs:291-303`). `DEFAULT_JUDGE_MODEL` + `TT_JUDGE_MODEL` live there.
- **Loop** (`routes/agent_run.rs`): `run_loop_core(completer: &dyn TurnCompleter, id, model, messages, tools, max_turns, turns_done, usage) -> LoopOutcome` — **provider-free generic** (pure tests inject a no-provider stub at `:946-998`). `turns_done` is a **completion-turn** counter (`:166`), NOT a tool-block count (a single turn can append several `Message::Tool` blocks — `:219-235`). `RunUsage { prompt_tokens: u64, completion_tokens: u64 }` (no cost field, `:52`). `Run` (`:60`) / `StoredRun` (`:335`) carry no cost field; `StoredRun` derives Serialize+Deserialize with **no** `#[serde(default)]` and is round-tripped through Redis (`store_run`/`fetch_run`, TTL `RUN_TTL_SECS=3600`, `:322`); it has 5 `StoredRun {…}` construction literals (1 prod `:722` + 4 tests); `Run` has 6 `{…}` literal sites incl. `StoredRun::to_run()` (`:359`). `GatewayCompleter { state: &AppState, identity: &RunIdentity }` resolves provider+creds per turn.
- **`AppState`** (`state.rs:315`): a single struct literal in `AppState::new`; `with_*` builders and tests funnel through `new` (no per-test literal churn).

## Design

### 1. Resolve the run's summarize policy once, at run creation
The route `AgenticBudget` is **not** available at `create_run`, so `create_run` resolves it explicitly. `apply_routing` is `pub(crate) async fn apply_routing(state: &AppState, ctx: &RequestContext, req: &mut ChatCompletionRequest, forced_route: Option<&str>) -> ApiResult<Option<RouteMatch>>` (`chat.rs:6001`, reachable from `agent_run.rs`, same crate). `create_run` does **not** currently hold a `RequestContext` (it is built per-turn inside `GatewayCompleter::complete`), but `apply_routing` + the route engine read **only** `ctx.org_id` (`chat.rs:6010`) and `ctx.tag` (`routing/src/lib.rs:633`) — never the credentials. So `create_run` hand-builds a **minimal** `RequestContext` from `RunIdentity` (`org_id`, `tag`, an empty/dummy `ProviderCredentials`, `trace_id`, `api_key_id`, `deadline: None`), then calls `apply_routing(&state, &ctx, &mut req_clone, identity.forced_route.as_deref())` once on a **clone** of the initial request, reads `route_match.agentic_budget` (`RouteMatch.agentic_budget: Option<AgenticBudget>`, `chat.rs:5978`), and (only when present **and** `elide_stale_tools == true`) builds an owned, secret-free
```rust
struct SummarizeConfig { keep_recent_pairs: u32, clear_at_least_tokens: u32 }
```
`None` ⇒ summarize is a total no-op (the 1b loop, byte-identical). For a nil-org / dev caller `apply_routing` returns `None` ⇒ `SummarizeConfig = None` (documented: summarize is unavailable without a real routed org).

**Pinned at turn 0 (deliberate).** `apply_routing` matches on the *evolving* request (model, input-token/cost estimate), so the matched route — and its budget — could in principle differ on a later, larger turn. This slice **pins the run's summarize policy to the turn-0 route** (the budget is coarse, per-route opt-in config; a run is the natural granularity). Documented as a known simplification; 2c-2 may revisit if a token-conditioned summarize route ever exists.

**Known side-effect (accepted).** `apply_routing` is pure for budget extraction (idempotent engine-cache read + `.iter().find` route match; the `req.model` rewrite is on the clone and ignored) **except** on the *paused-route* branch, where it increments the `route_paused_passthrough_total` Prometheus counter (`chat.rs:6102` → `metrics.rs:145`). Since `prepare` also calls `apply_routing` per turn, a `create_run` extraction call on a *paused* route over-counts that one ops metric by **+1 per run**. This is accepted and documented: non-corrupting, off the cost/serving path, paused routes are rare, and the over-count is bounded to one. (Rejected alternatives: guarding the call against the paused arm, or the fallback of surfacing `route_agentic_budget` out of turn-0's `prepare` — the latter needs a new `Prepared` field + a return channel from `complete`, more plumbing than the +1 metric is worth.)

### 2. `ConfigSummaryGate` + `AppState` wiring (operator-promoted, default-off)
New gate in `summarize_judge.rs`:
```rust
pub struct ConfigSummaryGate { trusted: std::collections::HashSet<String> }
impl ConfigSummaryGate {
    /// Parse TT_SUMMARIZE_TRUSTED_CLASSES: comma-separated, each entry TRIMMED,
    /// matched CASE-SENSITIVELY against the raw tool name (resolve_summary_class
    /// returns ToolCallFunction.name verbatim, e.g. "inspect_diff"). Do NOT lowercase.
    pub fn from_env() -> Self;
    pub fn new(trusted: HashSet<String>) -> Self;
}
impl SummaryGate for ConfigSummaryGate {
    fn is_committable(&self, class: &str) -> bool { self.trusted.contains(class) } // empty ⇒ never (== NeverCommitGate)
}
```
`AppState` gains one field `summary_gate: Arc<dyn SummaryGate>`, defaulted to `Arc::new(NeverCommitGate)` in the **single `AppState::new` literal** (a `with_summary_gate` builder + `ConfigSummaryGate::from_env` in server bootstrap). Unset/empty env ⇒ effectively `NeverCommitGate` fleet-wide ⇒ summarize is a no-op.

### 3. Production summarizer dispatch (a plain async helper — NOT the sync `Summarizer` trait)
Per the verified sync/async reality, production does **not** implement the sync `Summarizer` trait. Instead a plain async helper does the metered dispatch:
```rust
// in agent_run.rs (or a small summarize_run submodule)
async fn dispatch_summary(state, identity, summarizer_model: &str, class: &str, original: &str)
    -> SummarizeCall // { summary: Option<String>, cost_usd: Option<f64> }
```
- `summarizer_model = TT_SUMMARIZER_MODEL` (new env, this slice) **||** `state.judge_config.judge_model` (the already-resolved cheap judge model; default `DEFAULT_JUDGE_MODEL = "gpt-4o-mini"`, `quality_sample.rs:67`). `gpt-4o-mini` resolves by default (`registry.resolve` falls through to `infer_provider` → `openai`, registered by default).
- Resolve the **summarizer model's own** provider+creds, mirroring the live judge at `chat.rs:5410-5462`: `let provider = state.registry.resolve(summarizer_model)?;` then `chat::resolve_credentials_for(provider.id())` (`chat.rs:5046`, `pub(crate)`) — NOT the turn's served provider. Build a `RequestContext` from `RunIdentity` (org_id, raw_bearer) + the resolved creds.
- Build a tiny `ChatCompletionRequest`: system "Summarize this `{class}` tool result. Preserve every fact a later step might need; drop only redundancy/formatting. Output only the summary." + the `original` as the user content. Dispatch via `measurement::measured_single_dispatch(&provider, req, &ctx, deadline)` — note `req` is taken **by value** (`mut req: ChatCompletionRequest`) and it returns `Result<MeasuredDispatch, String>` where `MeasuredDispatch { response, cost_usd: Option<f64> }` (`measurement.rs:55`). Map: `Ok(d)` → `SummarizeCall { summary: <text of d.response's first choice>, cost_usd: d.cost_usd }`; `Err(_)` → fail-open `{ summary: None, cost_usd: None }`. A short `deadline` mirrors the judge baseline-dispatch knob. `cost_usd` is the catalog-metered tax (unpriced ⇒ `None`, never billed as a saving).
- Failure / timeout / empty output ⇒ `SummarizeCall { summary: None, cost_usd }` (fail-open). The tax is ledgered even on a declined/failed call (a dispatch may have billed).

### 4. Loop wiring — an injected, optional `TranscriptSummarizer` hook
To keep `run_loop_core` provider-free (its pure tests inject a no-provider stub), add ONE new optional param and a small async trait:
```rust
#[async_trait]
trait TranscriptSummarizer: Send + Sync {
    /// Summarize newly-aged tool blocks in place; return this call's metered tax (fail-open).
    async fn summarize_before_turn(&self, messages: &mut Vec<Message>, summarized_upto: &mut u32) -> Option<f64>;
}
// run_loop_core(..., summarizer: Option<&dyn TranscriptSummarizer>)
```
- **Call sites (6 total).** `create_run` (`:700`) **and** `submit_tool_outputs`/resume (`:905`) pass `Some(&summarizer)` — resume **rebuilds** `GatewayTranscriptSummarizer` (with the restored `summarized_upto`, §6) so summarization continues after a client round-trip. The `pub` 1a `run_loop` wrapper (`:275`, called only from tests) and the 3 unit tests (`:1178/:1217/:1247`) pass `None` ⇒ no-op ⇒ 1b byte-identical. (No async-trait/Send/lifetime obstacle: structurally identical to the existing `completer: &dyn TurnCompleter` Send+Sync async-trait object awaited inside the axum handlers.)
- **Production** builds `GatewayTranscriptSummarizer { state, identity, cfg: SummarizeConfig, gate: Arc<dyn SummaryGate>, base_model, base_provider_id, summarizer_model }` and passes `Some(&it)`. (`GatewayCompleter` and this summarizer are separate objects sharing `&AppState`/`&RunIdentity`; the completer is still passed as `completer`.)
- `run_loop_core`, **before** each `completer.complete(...)`, calls `if let Some(s) = summarizer { tax = s.summarize_before_turn(&mut messages, &mut summarized_upto).await; total_tax = sum_metered(total_tax, tax); }`. Turn 0 has no aging tail (the few initial tool blocks, if any, are inside `keep_recent_pairs`), so it is naturally a no-op.

`GatewayTranscriptSummarizer::summarize_before_turn` (when `gate` is non-Never):
1. Collect `Message::Tool` ordinals in `messages` order: `T0..T_{n}`.
2. Eligible = ordinals in `[*summarized_upto .. n.saturating_sub(cfg.keep_recent_pairs))` — older than the recent window **and** not yet processed.
3. For each eligible ordinal → message index `idx`:
   - class = `resolve_summary_class(messages, idx)` (expose `pub(crate)`); skip `is_error_blob` (expose `pub(crate)`); skip non-text (`Parts`) content.
   - `gate.is_committable(&class)`? no → leave verbatim.
   - yes → `dispatch_summary(...)` (§3) → `SummarizeCall`. Ledger `call.cost_usd` regardless.
   - **token-true gate** (served-model tokenizer): `let provider = base_provider_id; let orig = estimate_input_tokens_for_model(provider, base_model, &original).tokens; let new = estimate_input_tokens_for_model(provider, base_model, &summary).tokens;` commit only if `orig.saturating_sub(new) >= cfg.clear_at_least_tokens.max(1)` (the `.max(1)` ⇒ never a token-neutral/-inflating commit; `clear_at_least_tokens` adds the R1 cache-thrash floor). The `Confidence::Low` (`chars/4`) fallback still rejects non-reductions — same discipline as `passes/mod.rs`.
   - commit ⇒ replace the block's text content in `messages`.
4. **Advance `*summarized_upto = n.saturating_sub(cfg.keep_recent_pairs)` UNCONDITIONALLY** after the pass. **Tradeoff (explicit):** a block dispatched-but-rejected (token-true fail) or declined/failed is **permanently skipped, never retried** — guaranteeing each block is dispatched (taxed) at most once across the whole run. We accept "no retry of a transiently-failing block" in exchange for a hard no-re-tax / bounded-cost guarantee (this is a default-off optimization; a missed summary just leaves the block verbatim).
- `base_model` / `base_provider_id` are the **run's base** model + its resolved provider id (captured once at run start). On a 2a down-route turn the served model differs, but summarize keys on the model-independent `messages`; the base model gives a stable token count.

### 5. Tax sink (the measurement tax)
`RunUsage` stays token-only. Add `summarizer_tax_usd: Option<f64>` to **`Run`** (Serialize-only — no `#[serde(default)]` needed) and to **`StoredRun`** (`#[serde(default)]`, since `StoredRun` is `Deserialize`d from Redis — see §6). `run_loop_core` accumulates the per-call taxes (via `sum_metered`) and sets it on the returned `Run`/persisted `StoredRun`; `StoredRun::to_run()` (`:359`, one of 6 `Run {…}` literal sites) maps the field through. It is a measurement tax (parallel to the judge tax): surfaced on the run response, **never** folded into `request_logs.cost_usd` or the per-turn served cost. (No `request_logs`/`JudgeSink` write from the loop — the run response field is the sink.)

### 6. `StoredRun` resume back-compat + persistence
`StoredRun` gains **three** `#[serde(default)]` fields: `summarized_upto: u32`, `summarizer_tax_usd: Option<f64>`, and `summarize: Option<SummarizeConfig>` (`SummarizeConfig` derives `Serialize + Deserialize` — it is tiny, non-secret config). Persisting `SummarizeConfig` lets `submit_tool_outputs`/resume **rebuild** `GatewayTranscriptSummarizer` from the stored config **without** re-running `apply_routing` (so the §1 paused-route metric quirk fires at most once per run, never again on resume, and the run's summarize policy stays pinned to turn-0). `run_loop_core` takes `summarized_upto` as a param (like `turns_done`/`usage`), restored from `StoredRun` on resume so no block is re-summarized / re-taxed. **`#[serde(default)]` is load-bearing**: a run persisted *before* this deploy (TTL 3600s) has no `summarized_upto`/`summarizer_tax_usd` key and must still deserialize (an old paused run resumes as if unsummarized) — a serde back-compat unit test proves this. Enumerate + update the **5** `StoredRun {…}` literal sites (1 prod `:722` + 4 tests) and `StoredRun::to_run()`.

### 7. Doc-closures (folded in)
- **Sub-lever 4 (substep cache) deferral**: honest "intentionally deferred" note in `substep_cache.rs` + the `agentic_budget/mod.rs` Sub-lever-4 comment — not wired because the only read-only/cacheable tools are the 4 near-free gateway tools (caching their results saves ~nothing while adding an embedding tax); stays a tested building block until an *expensive* read-only gateway tool makes caching net-positive. Mirrors the COST-3(U) per-request-proxy doc-closure.
- **Summarize token-true gate**: update the stale promise in `summarize_judge.rs` (module/struct/`SummaryOutcome` docs say the byte heuristic is "pending the deferred wiring that wraps this step inside the pipeline's token-true gate") to record that 2c-1 applies a **loop-level** token-true gate (the loop owns the dispatch + token measurement; the `apply`/`VolatileTail` byte path is unchanged and remains for any future pipeline use).

## Components
| Unit | Location | Responsibility |
|---|---|---|
| `ConfigSummaryGate` + `from_env` | `summarize_judge.rs` | operator-promoted per-class trust (env allowlist, case-sensitive); empty ⇒ no-op |
| `pub(crate)` `resolve_summary_class` / `is_error_blob` | `summarize_judge.rs` | expose existing private policy helpers for the loop |
| `dispatch_summary` async helper | `agent_run.rs` / `summarize_run` | metered cheap-model dispatch via `measured_single_dispatch` on the summarizer model's own provider; fail-open |
| `TranscriptSummarizer` trait + `GatewayTranscriptSummarizer` | `agent_run.rs` | the injected hook: eligible aging blocks → gate → dispatch → token-true commit → advance watermark → return tax |
| `run_loop_core` new params (`summarizer: Option<&dyn …>`, `summarized_upto`) + tax accumulation | `agent_run.rs` | call the hook before each turn; sum tax; keep the provider-free seam (`None` in pure tests) |
| `SummarizeConfig`; `Run.summarizer_tax_usd`; `StoredRun.summarized_upto`/`.summarizer_tax_usd` (`#[serde(default)]`) | `agent_run.rs` | run-level policy + tax sink + resume-safe watermark (5 `StoredRun` + 6 `Run` literal sites, incl. `to_run()`) |
| `create_run` budget resolution (`apply_routing` once) | `agent_run.rs` | resolve turn-0 `SummarizeConfig`; build the production summarizer; pass `Some` |
| `AppState.summary_gate` + `with_summary_gate` + bootstrap `from_env` | `state.rs`, `server.rs` | process-wide gate, `NeverCommitGate` by default |
| Sub-lever 4 + summarize token-gate doc-closures | `substep_cache.rs`, `agentic_budget/mod.rs`, `summarize_judge.rs` | honest deferral + stale-promise updates |

## Error handling / edge cases
- No route `elide_stale_tools` (or nil-org/dev) ⇒ `SummarizeConfig = None` ⇒ no-op (1b byte-identical). Gate Never / class untrusted ⇒ no-op. Error blob / non-text content ⇒ never summarized. Recent `keep_recent_pairs` tool blocks ⇒ never touched. Summarizer decline/error/timeout ⇒ verbatim (fail-open), tax still ledgered, block permanently skipped (watermark advances). Token-true non-reduction ⇒ verbatim, skipped. Mixed gateway+client turn (1b) appends several tool blocks in one turn — the watermark is a tool-block ordinal so it stays correct. Resume ⇒ `summarized_upto` restored ⇒ no re-summarize / no double tax. `/v1/chat/completions` ⇒ never runs loop-summarize.
- Cache-thrash (R1): rewriting an old tool block busts the provider prefix cache from that point; `clear_at_least_tokens` is the floor that only allows a commit when it frees enough tokens to justify the re-cache.

## Testing
- **Pure** (no provider; `run_loop_core` with `summarizer: None` and with a no-provider stub `TranscriptSummarizer`): eligible-ordinal computation (watermark + `keep_recent_pairs` boundary, incl. `n < keep`, and a multi-tool-block turn); error-blob/non-text skip; gate-refusal no-op; token-true rejection of a non-shrinking "summary" (stub returns a longer string); watermark advances unconditionally (a rejected block is not retried); `summarizer: None` ⇒ transcript byte-identical to 1b.
- **Gate**: `ConfigSummaryGate::from_env` parsing (unset/empty ⇒ never; listed classes ⇒ committable; case-sensitive, trimmed); `AppState` default gate is `NeverCommitGate`.
- **Serde back-compat**: a `StoredRun` JSON omitting `summarized_upto`/`summarizer_tax_usd` deserializes (old paused run resumes as unsummarized, watermark 0).
- **Loop** (stub `TranscriptSummarizer` + a trusted-class gate): a scripted multi-turn run → an aging block summarized once, recent tail verbatim, tax accumulated into `Run.summarizer_tax_usd`; a later turn does NOT re-summarize the same block (watermark); resume from a persisted `summarized_upto` ⇒ no re-summarize.
- **Behavior-preservation**: `cargo test -p tt-core --lib --tests` at the established baseline — loop-only change; default-off paths byte-identical; `prepare`/`complete_once`/`/v1/chat/completions` untouched. The `run_loop_core` new param + `StoredRun`/`Run` new fields surface their ripples as compile errors (the gate catches them). Clippy `--workspace --all-targets` clean.

## Non-goals (2c-1)
The live blind-paired judge + `AdaptiveSummaryGate` auto-open/close (2c-2). Summarize on `/v1/chat/completions` (loop-only here). Substep-cache serve (2b, deferred). SSE streaming + cross-turn attestation (slice 3). No change to the lossless `elide` field-drop or the `apply`/`VolatileTail`/`plan_summary` path. No per-turn route re-resolution for the budget (pinned at turn 0).

## Rollout
Single public PR. Default-off (no `elide_stale_tools` on the route AND/OR no `TT_SUMMARIZE_TRUSTED_CLASSES` ⇒ total no-op). Public CI (`cargo test (workspace)` — disk-flaky, rerun if needed; `fmt+clippy`; `tt inspect .`; determinism untouched). No DB/cloud changes. Redis optional (summarize works on inline runs; the watermark + tax persist with the run when Redis is present).

# Deep Research Panel — Phase 4 (best-of-n + majority arbiters) Design Spec

> Status: APPROVED. Date: 2026-06-22. Repo: public. Branch: `feat/panel-phase4-strategies`.
> Builds on the Phase-1 `ArbiterStrategy` seam (panel.rs). Master spec: `2026-06-21-deep-research-panel-design.md`.

## 1. Goal

Implement the two remaining panel arbiter strategies — `best-of-n` and `majority` — into the existing `ArbiterStrategy` trait, replacing their current `PanelStrategyUnsupported` (501) responses. No engine change: `run_panel` already wraps the arbiter with quorum, cost aggregation, and `panel_legs` persistence; the `X-TokenTrimmer-Panel: best-of-n|majority` trigger is already parsed (Phase 1).

## 2. Decisions (approved)

- **best-of-n = single-pass LLM judge** (one arbiter-model call picks the best, returns the chosen leg's **original** answer verbatim).
- **majority = embedding clustering** (via the gateway's `state.embedder`; largest cosine cluster wins; return its medoid verbatim; **no extra LLM call**).
- best-of-n surfaces the judge's reason + chosen leg in the response body.

## 3. Shared refactor

`Synthesize::arbitrate` already builds `ok_answers: Vec<(usize, String)>` (the surviving `LegStatus::Ok && LegRole::Leg` legs' index + assistant text — panel.rs:408-424). **Factor this into a free helper** `fn surviving_answers(legs: &[LegResult]) -> Vec<(usize, String)>` reused by all three strategies (Synthesize unchanged in behavior). `(usize, String)` = the leg's `leg_index`-position in `legs` + its answer text.

## 4. `BestOfN { arbiter_model: ModelRef }`

`impl ArbiterStrategy`:
1. `answers = surviving_answers(legs)`. If empty → `Err(ApiError::InvalidRequest("panel: no successful legs"))` (mirrors Synthesize's guard).
2. If `answers.len() == 1` → return that leg's response verbatim, `cost_usd: None`, no judge call (degenerate case).
3. Build a judge prompt: a system instruction "You are selecting the single best of N candidate answers. Reply with ONLY the candidate number (1..N) of the best answer, then on a new line a one-sentence reason." followed by the numbered candidates (`Candidate 1:\n<answer>\n\nCandidate 2:\n...`). Preserve the caller's original system message(s) (like Synthesize does).
4. ONE `measured_single_dispatch(arbiter_provider, judge_req, ctx-with-arbiter-cred, deadline)` on `arbiter_model` (reuse Synthesize's arbiter provider/credential resolution from CHANGE in Phase 2 — the arbiter cred substitution).
5. Parse the leading integer from the judge's text → 1-based index → map to the chosen leg. On parse failure or out-of-range → fall back to `answers[0]` (the first surviving leg) and set `fell_back = true`.
6. Return `ArbiterOutcome { response: <chosen leg's original ChatCompletionResponse>, cost_usd: <judge dispatch cost> }`. The chosen answer is the **original leg response**, NOT a paraphrase.
7. Expose `chosen_leg` (the leg's index), `reason` (the judge's one-line reason text, trimmed), and `fell_back` so `complete_panel` can put them in the response body's `tokentrimmer.panel.arbiter` object.

**Body additions** (`tokentrimmer.panel.arbiter`): `{ strategy: "best-of-n", chosen_leg, reason, fell_back }` alongside the existing arbiter cost fields.

## 5. `Majority` (embedding clustering)

`impl ArbiterStrategy`:
1. `answers = surviving_answers(legs)`. Empty → `Err(InvalidRequest)`. `len()==1` → that leg verbatim, `cost_usd: None`.
2. **Embed** each answer text via `state.embedder.embed(...)` (read the `EmbeddingProvider` trait for the exact method — it's `Arc<dyn EmbeddingProvider>` on `AppState`, used by L2 cache). Collect `Vec<Vec<f32>>`. If embedding fails for any answer → fall back to returning `answers[0]` with a `degraded` flag (do NOT fail the whole panel over a majority-embed error; the legs already succeeded).
3. **Cluster** greedily by cosine similarity at threshold `T = TT_PANEL_MAJORITY_THRESHOLD` (default `0.83`, parsed like other panel env, clamped to (0,1]): for each answer, assign to the first existing cluster whose **representative** (first member) has `cosine(answer, rep) >= T`; else start a new cluster. (Greedy single-link is sufficient for N ≤ `TT_PANEL_MAX_MEMBERS`=8.)
4. **Winner** = the largest cluster (ties → the cluster containing the earliest leg). `no_majority = winner.len() == 1` (every answer distinct).
5. **Medoid** of the winning cluster = the member with the highest mean cosine similarity to the other winning members (for a singleton winner, it is that answer; for `no_majority`, pick the **global** medoid = the answer with highest mean similarity to ALL answers — the most representative).
6. Return `ArbiterOutcome { response: <medoid leg's original response>, cost_usd: <embedding cost or None> }`. The answer is an **actual leg answer**, returned verbatim.
7. Expose `winning_cluster_size`, `total_clusters`, `no_majority` for the body.

**Embedding cost:** if `EmbeddingProvider` exposes a per-call/per-token cost, compute it; else `cost_usd: None` (embeddings are negligible vs the legs). Do not coerce to 0 if genuinely unknown.

**Body additions** (`tokentrimmer.panel.arbiter`): `{ strategy: "majority", winning_cluster_size, total_clusters, no_majority }`.

**Cosine helper:** `fn cosine(a: &[f32], b: &[f32]) -> f32` (dot / (||a||·||b||); guard zero-norm → 0.0). Unit-tested directly.

## 6. `strategy_for` + wiring

`strategy_for(cfg)` returns `Box::new(BestOfN { arbiter_model })` for `BestOfN` and `Box::new(Majority)` for `Majority` (both stop returning `PanelStrategyUnsupported`). `Majority` needs the embedder — pass `state` into `arbitrate` (already in the trait signature) and read `state.embedder` there; `BestOfN` uses `state.registry` for the arbiter provider like `Synthesize`. The arbiter-leg recording in `run_panel` (Phase 2) stamps the arbiter `panel_legs` row: BestOfN → the judge model; Majority → model `"majority"` (no LLM), cost = embedding cost.

## 7. Testing (TDD)

- **cosine** unit test (orthogonal → 0, identical → 1, zero-norm → 0).
- **surviving_answers** unit test (filters Ok+Leg, preserves order/index).
- **best-of-n** (mock providers, reuse `panel_fanout.rs`/`panel_engine.rs` harness): 3 legs, a mock arbiter whose `chat_completion` returns `"2\nbecause it is most complete"`; assert the returned response == leg-index-2's original answer, and (via `complete_panel` body) `arbiter.chosen_leg`/`reason` populated. A second case: arbiter returns garbage → `fell_back == true`, returns leg 0.
- **majority** with a **mock `EmbeddingProvider`** returning controlled vectors: 4 legs where legs 0,1,2 embed near-identical (cluster of 3) and leg 3 distinct → assert the medoid of {0,1,2} is returned and `winning_cluster_size==3`. A no-majority case: all 3 distinct vectors → `no_majority==true`, global medoid returned.
- **integration** (`panel_engine.rs`-style, router): `X-TokenTrimmer-Panel: best-of-n` end-to-end returns 200 with a `best-of-n` arbiter body (no longer 501); same for `majority`. Off-by-default + billing invariants unchanged (regression: the existing panel suite stays green).

## 8. Out of scope
Streaming (P5), transcoders (P6), entitlement/docs (P7). No change to billing, quorum, persistence, or the budget gate.

## 9. Self-review
- Placeholders: none — algorithms, fallbacks, config, body fields, and tests are concrete. The one read-at-impl detail (the exact `EmbeddingProvider::embed` signature) is a cited seam, not a TBD.
- Consistency: both strategies return `ArbiterOutcome` exactly like Synthesize; `run_panel` is untouched; answers are returned verbatim (no paraphrase) in all paths.
- Ambiguity: tie-breaks (earliest leg), no-majority (global medoid + flag), and fallbacks (first leg, flagged) are each pinned to one behavior.

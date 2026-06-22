# Deep Research Panel — Phase 4 (best-of-n + majority arbiters) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]` checkboxes.

**Goal:** Implement the `best-of-n` (single-pass LLM judge) and `majority` (embedding clustering) arbiter strategies into the existing `ArbiterStrategy` seam, replacing their `501 PanelStrategyUnsupported` responses.

**Architecture:** Both `impl ArbiterStrategy` in `crates/core/src/routes/panel.rs` and are returned by `strategy_for`. `run_panel` already wraps the arbiter with quorum, cost aggregation, and `panel_legs` persistence — untouched. `ArbiterOutcome` gains a small `detail: ArbiterDetail` carrying strategy-specific fields that `complete_panel` injects into the response body's `tokentrimmer.panel.arbiter`. Spec: `docs/superpowers/specs/2026-06-22-panel-phase4-strategies-design.md`.

**Tech Stack:** Rust, async_trait, the existing `measured_single_dispatch`, `state.embedder: Arc<dyn EmbeddingProvider>`.

## Global Constraints

- **No engine/billing change.** `run_panel`, quorum, cost aggregation, `panel_legs` persistence, the budget gate, and off-by-default are all untouched. The existing panel suite stays green (regression).
- **Answers returned verbatim.** best-of-n returns the *chosen leg's original `ChatCompletionResponse`* (NOT a paraphrase); majority returns the *medoid leg's original response*. Only `Synthesize` generates a new answer.
- **Fail-soft arbiters.** A best-of-n parse failure → first surviving leg (flagged `fell_back`); a majority embed failure → first surviving leg (flagged `degraded`). Never fail a panel whose legs succeeded over an arbiter-selection hiccup.
- **`cost_usd` Option discipline.** Unpriced → `None`, never coerced to 0 (mirrors `MeasuredDispatch.cost_usd`).
- **CI hygiene:** no whole-crate `cargo fmt` (format only touched files); run `cargo fmt --check` on your files; verify `cargo clippy -p tt-core --all-targets -- -D warnings` + `cargo test -p tt-core` before claiming done (public CI gates fmt + full workspace test).

---

### Task 1: Shared helpers + `ArbiterOutcome.detail` + refactor `Synthesize`

**Files:**
- Modify: `crates/core/src/routes/panel.rs` (add helpers + `ArbiterDetail`; refactor `Synthesize::arbitrate` to use the helper — behavior-preserving).
- Test: `crates/core/tests/panel_arbiter.rs` (unit tests for the two helpers).

**Interfaces (Tasks 2,3 depend on these):**
```rust
/// Surviving Ok member-leg answers as (position-in-`legs`, answer text).
pub(crate) fn surviving_answers(legs: &[LegResult]) -> Vec<(usize, String)>;
/// Cosine similarity; zero-norm → 0.0.
pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f32;
/// Strategy-specific arbiter detail surfaced in the response body. Default = all None/false (Synthesize).
#[derive(Default, Clone)]
pub struct ArbiterDetail {
    pub chosen_leg: Option<usize>,      // best-of-n: leg_index of the chosen answer
    pub reason: Option<String>,         // best-of-n: judge's one-line reason
    pub fell_back: bool,                 // best-of-n: judge choice unparseable -> first leg
    pub winning_cluster_size: Option<usize>, // majority
    pub total_clusters: Option<usize>,  // majority
    pub no_majority: bool,               // majority: every answer distinct
    pub degraded: bool,                  // majority: embedding failed -> first leg
}
// ArbiterOutcome gains: pub detail: ArbiterDetail,
```
Note: `surviving_answers` returns `.0` = the **index into the `legs` slice** (so callers can fetch `legs[pos].response` + `legs[pos].leg_index`). `Synthesize` ignores `.0` (uses only the text), so its behavior is unchanged.

- [ ] **Step 1: Write the helper unit tests** in `panel_arbiter.rs`:
```rust
use tt_core::routes::panel::{cosine, ArbiterDetail};
#[test]
fn cosine_identical_is_one_orthogonal_zero_zeronorm_zero() {
    assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
}
#[test]
fn arbiter_detail_default_is_empty() {
    let d = ArbiterDetail::default();
    assert!(d.chosen_leg.is_none() && !d.fell_back && !d.no_majority);
}
```
(`surviving_answers` is tested at the integration level via the strategies in Tasks 2/3; if a direct unit test is easy with constructed `LegResult`s, add one asserting it filters non-Ok / Arbiter legs and preserves order.)

- [ ] **Step 2: Run, verify fail.** `cargo test -p tt-core --test panel_arbiter` → FAIL (unresolved `cosine`/`ArbiterDetail`).
- [ ] **Step 3: Implement.** Add `surviving_answers` (extract the exact filter/map from `Synthesize::arbitrate` lines 408-422, but return the legs-slice position via `.enumerate()` instead of `l.leg_index`), `cosine`, and `ArbiterDetail` (derive `Default, Clone`). Add `pub detail: ArbiterDetail,` to `ArbiterOutcome`. Refactor `Synthesize::arbitrate` to call `surviving_answers(legs)` (its candidate loop already ignores `.0`) and return `ArbiterOutcome { response, cost_usd, detail: ArbiterDetail::default() }`.
- [ ] **Step 4: Run, verify pass + regression.** `cargo test -p tt-core --test panel_arbiter --test panel_fanout` → PASS (Synthesize behavior unchanged).
- [ ] **Step 5: Commit.** `git add` the touched files; `git commit -m "refactor(panel): surviving_answers + cosine helpers + ArbiterOutcome.detail"`.

---

### Task 2: `BestOfN` strategy

**Files:** Modify `crates/core/src/routes/panel.rs`; Test `crates/core/tests/panel_arbiter.rs` (+ harness from `panel_fanout.rs`).

**Interfaces:** `pub struct BestOfN { pub arbiter_model: ModelRef }` + `impl ArbiterStrategy`. `strategy_for` returns it for `ArbiterStrategyKind::BestOfN`.

- [ ] **Step 1: Failing test.** Using the `panel_fanout.rs` mock harness, build 3 legs with distinct answers and a mock arbiter provider whose `chat_completion` returns assistant text `"2\nCandidate 2 is the most complete."`. Call `BestOfN{arbiter_model}.arbitrate(&req, &legs, &state, &ctx, &creds)`; assert the returned `response` equals leg-(candidate-2)'s original response, `detail.chosen_leg == Some(<that leg's leg_index>)`, `detail.reason` contains "complete", `detail.fell_back == false`. Second test: arbiter returns `"banana"` → `detail.fell_back == true`, response == first surviving leg.
- [ ] **Step 2: Run, verify fail.** FAIL.
- [ ] **Step 3: Implement `BestOfN::arbitrate`:**
  - `let answers = surviving_answers(legs);` empty → `Err(InvalidRequest("panel: no successful legs"))`; `answers.len()==1` → return `legs[answers[0].0].response.clone()` verbatim, `cost_usd: None`, `detail{chosen_leg: Some(legs[answers[0].0].leg_index), ..}`.
  - Build the judge prompt: preserve `request.messages` (caller system msgs), push a `System` msg: "You are selecting the single best of N candidate answers below. Reply with ONLY the candidate number (1 to N) on the first line, then one sentence explaining why." then push numbered `User` msgs `Candidate {i+1} of {n}:\n\n{answer}` (mirror Synthesize's loop).
  - Resolve arbiter provider + substitute credential + deadline + `measured_single_dispatch` — COPY the exact block from `Synthesize::arbitrate` (panel.rs:474-502).
  - Parse the judge text: take the first integer token of the first line; `idx = parsed - 1`. If `Some(idx)` in `0..answers.len()` → chosen = `answers[idx].0`; else chosen = `answers[0].0`, `fell_back = true`. `reason` = the remainder of the judge text after the first line, trimmed (or `None`).
  - Return `ArbiterOutcome { response: legs[chosen].response.clone().expect("Ok leg has response"), cost_usd: measured.cost_usd, detail: ArbiterDetail{ chosen_leg: Some(legs[chosen].leg_index), reason, fell_back, ..Default::default() } }`.
- [ ] **Step 4: Wire `strategy_for`.** Replace the `BestOfN => Err(PanelStrategyUnsupported)` arm with `Ok(Box::new(BestOfN { arbiter_model: cfg.arbiter_model.clone() }))`.
- [ ] **Step 5: Run, verify pass.** `cargo test -p tt-core --test panel_arbiter` → PASS.
- [ ] **Step 6: Commit.** `git commit -m "feat(panel): best-of-n arbiter (single-pass judge, returns chosen leg verbatim)"`.

---

### Task 3: `Majority` strategy (embedding clustering)

**Files:** Modify `crates/core/src/routes/panel.rs`; Test `crates/core/tests/panel_arbiter.rs`.

**Interfaces:** `pub struct Majority;` + `impl ArbiterStrategy`. `strategy_for` returns it for `ArbiterStrategyKind::Majority`. New env `TT_PANEL_MAJORITY_THRESHOLD` (default 0.83).

**Read first:** the `EmbeddingProvider` trait (the type of `state.embedder`, `state.rs:86`) — `grep -rn "trait EmbeddingProvider" crates` — for the exact `embed` method signature + return type (`Vec<f32>` / `Result<...>`). Use it as-is.

- [ ] **Step 1: Failing test** with a **mock `EmbeddingProvider`** returning controlled vectors. Build 4 legs; the mock maps each leg's answer text to a fixed vector: answers A,B,C → `[1,0,0]`-ish near-identical (cosine ≥ 0.83 to each other), answer D → `[0,0,1]`. Build `AppState` with this mock embedder (mirror how `panel_engine.rs` builds AppState, swapping the embedder). Call `Majority.arbitrate(...)`; assert `detail.winning_cluster_size == Some(3)`, `detail.no_majority == false`, and the returned response is one of the {A,B,C} legs (the medoid). Second test: 3 mutually-distant vectors → `detail.no_majority == true`, `winning_cluster_size == Some(1)`, returns the global medoid.
- [ ] **Step 2: Run, verify fail.** FAIL.
- [ ] **Step 3: Implement `Majority::arbitrate`:**
  - `answers = surviving_answers(legs)`; empty → `Err(InvalidRequest)`; len 1 → that leg verbatim, `cost_usd None`.
  - Embed each `answers[k].1` via `state.embedder.<embed-method>`. On ANY embed error → fall back: return `legs[answers[0].0].response`, `detail.degraded = true`, `cost_usd None`. Collect `vecs: Vec<Vec<f32>>`.
  - Threshold `T = std::env::var("TT_PANEL_MAJORITY_THRESHOLD").ok().and_then(|v| v.parse::<f32>().ok()).filter(|t| *t > 0.0 && *t <= 1.0).unwrap_or(0.83)`.
  - Greedy cluster: `clusters: Vec<Vec<usize>>` (each = indices into `answers`). For each `k`: assign to the first cluster whose representative (`clusters[c][0]`) has `cosine(&vecs[k], &vecs[rep]) >= T`; else push a new `vec![k]`.
  - Winner = cluster with max `.len()` (tie → the one whose first element is smallest, i.e. earliest leg). `no_majority = winner.len() == 1`.
  - Medoid: if `winner.len() > 1`, the winner member with the highest mean cosine to the other winner members. If `no_majority`, the **global** medoid = the answer index (over ALL `answers`) with highest mean cosine to all others.
  - Return `ArbiterOutcome { response: legs[answers[medoid].0].response.clone().unwrap(), cost_usd: <embedding cost if the embedder exposes it, else None>, detail: ArbiterDetail{ winning_cluster_size: Some(winner.len()), total_clusters: Some(clusters.len()), no_majority, ..Default::default() } }`. Model-stamp note: `run_panel` records the arbiter leg; for majority the arbiter "model" is conceptually the embedder — leave the existing arbiter-leg recording as-is (it uses `cfg.arbiter_model`; acceptable, or pass a `"majority"` marker if trivial — do NOT over-engineer).
- [ ] **Step 4: Wire `strategy_for`.** Replace the `Majority => Err(PanelStrategyUnsupported)` arm with `Ok(Box::new(Majority))`.
- [ ] **Step 5: Run, verify pass.** `cargo test -p tt-core --test panel_arbiter` → PASS.
- [ ] **Step 6: Commit.** `git commit -m "feat(panel): majority arbiter (embedding clustering, returns medoid verbatim)"`.

---

### Task 4: Body injection + router integration + regression

**Files:** Modify `crates/core/src/routes/panel.rs` (`complete_panel` body injection of `ArbiterDetail`); Test `crates/core/tests/panel_engine.rs`.

- [ ] **Step 1: Failing integration tests.** In `panel_engine.rs` (router harness): (a) `X-TokenTrimmer-Panel: best-of-n` + 2 members + budget → 200 (NOT 501); body `tokentrimmer.panel.arbiter.strategy == "best-of-n"` with `chosen_leg` + `reason` present. (b) `X-TokenTrimmer-Panel: majority` + members (mock embedder) → 200; body `arbiter.strategy=="majority"` with `winning_cluster_size`. (c) regression: the existing `synthesize` happy-path body still has its arbiter object.
- [ ] **Step 2: Run, verify fail.** FAIL (501 / missing body fields).
- [ ] **Step 3: Implement body injection.** In `complete_panel`, where the `tokentrimmer.panel.arbiter` object is built (Phase 1/2), read the `ArbiterOutcome.detail` (now threaded out of `run_panel` — confirm `run_panel` returns/propagates the arbiter detail; if `PanelResult` doesn't carry it, add `arbiter_detail: ArbiterDetail` to `PanelResult` and set it from the `ArbiterOutcome`). Inject the non-default fields: `chosen_leg`, `reason`, `fell_back`, `winning_cluster_size`, `total_clusters`, `no_majority`, `degraded` (omit None/false via `skip_serializing_if`).
- [ ] **Step 4: Run, verify pass.** `cargo test -p tt-core --test panel_engine --test panel_dispatch --test panel_fanout --test panel_arbiter` → PASS.
- [ ] **Step 5: Full verify.** `cargo clippy -p tt-core --all-targets -- -D warnings` clean; `cargo fmt --check` on touched files; `cargo test -p tt-core` green.
- [ ] **Step 6: Commit.** `git commit -m "feat(panel): surface arbiter detail in panel body; best-of-n/majority router tests"`.

---

## Self-Review
- **Spec coverage:** §3 helper → T1; §4 best-of-n → T2; §5 majority → T3; §4/§5 body fields → T1 (struct) + T4 (injection); §6 strategy_for → T2/T3; §7 tests → all tasks. Covered.
- **Placeholders:** none. The one read-at-impl detail (`EmbeddingProvider::embed` signature) is a cited grep in T3, not a TBD.
- **Type consistency:** `surviving_answers`/`cosine`/`ArbiterDetail`/`ArbiterOutcome.detail`/`BestOfN`/`Majority` consistent across T1–T4. `ArbiterDetail` fields match the spec body fields. `surviving_answers` returns legs-slice position (used by T2/T3 to fetch `legs[pos].response`); Synthesize ignores it (behavior-preserving).

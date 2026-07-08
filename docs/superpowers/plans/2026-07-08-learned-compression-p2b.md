# Learned Compression — Phase 2b (in-process scorer + RUNG 1/2 model + DARK SHADOW wiring) — Plan

> **For agentic workers:** REQUIRED SUB-SKILL: `superpowers:subagent-driven-development` (one subagent per task). Steps use checkbox (`- [ ]`) syntax, TDD per step.

**Goal:** Stand up the learned model's in-process scorer + the DARK SHADOW wiring + the offline P2c eval harness. The model scores per-token keep/drop on Prose blocks (reusing P1b's reassembly), ships DARK in shadow (score, compare to P1b, ship P1b, log delta), + the shadow log feeds P2c's recall-vs-deterministic eval. The owner-infra M4 Max gateway runs the model in-process (Metal EP, no network call) behind an off-by-default `ml-scoring` feature; the public/Fly builds stay ML-dep-free.

**Architecture (spec: `docs/superpowers/specs/2026-07-08-learned-compression-phase2b-design.md`):** a new `crates/ml-scoring` lib crate (behind the `ml-scoring` feature on `tt-core`, OFF by default) holds the `ort` dep + the OrtCoreML scorer (session cache, warmup, hard-timeout-with-cached-fallback, `ManuallyDrop`, CoreML/Metal EP + CPU fallback). A new `crates/core/src/content_compress/learned_prose.rs` sibling exports the drop-in `compress` contract; the `compact_block` Prose arm gains a learned-path branch. The shadow path emits a structured `tt::compress::shadow` JSONL log (ZDR-safe, no raw text). Training (RUNG 1/2) is off-repo (PyTorch+MPS, a TT-route teacher) — documented as a recipe, not in-tree code.

**Tech stack:** Rust (`tt-core` + new `tt-ml-scoring` crate, `ort` 2.0.0-rc.12, `ndarray` 0.17, `tokio` for the hard-timeout), `cargo` feature flags. No new workspace deps on the default build. `cargo test --workspace --no-run` is the field-ripple gate; the `ml-scoring` feature's tests gate on the `ort` runtime being present (skip if `TT_ML_MODEL_PATH` unset).

## Global constraints (verbatim from the spec)
- **Off by default.** The `ml-scoring` feature is OFF in the public default build + the Fly/cloud builds; only the owner's M4 Max gateway build enables it (+ carries the ~220MB model on disk). The `ort`+`ndarray` deps are optional (behind `dep:`).
- **DARK shadow only.** P2b ships SHADOW (score, compare, ship P1b, log delta). Promotion (ship the learned output) is P2d. The learned compress output is computed for offline recall eval, NOT committed to the dispatched request.
- **ZDR.** The shadow log carries `{trace_id, content_hash, route, model, p1b_tokens_removed, learned_tokens_removed, cache_hit, gate_committed, org_id, ts}` — NO raw text. The `content_hash` is the keyed reference (raw text stays in the opt-in `TT_COMPRESS_CAPTURE` sink).
- **Never block the request.** The scorer runs behind a hard-timeout (e.g. 50ms); on expiry → fail-open to deterministic P1b. `load-dynamic` so the gateway boots even if the `.onnx`/lib is absent.
- **The headroom trap.** The dispatcher DISCARDS the `usize` from `prose::compress` (`.0`) — the shadow "log the delta" self-measures per-block + emits on a SEPARATE channel (the structured shadow log), NOT via the drop-in return value.
- **Separate class.** A NEW `prose-learned` class on the shared `RatchetSummaryGate` (independent ratchet + allowlist from `PROSE_CLASS`). A bad learned model darkens ONLY the learned path; deterministic P1b keeps serving.

## File structure
- Create `crates/ml-scoring/Cargo.toml` + `crates/ml-scoring/src/lib.rs` — the `ort` integration (session cache, warmup, hard-timeout, `ManuallyDrop`, the CoreML/Metal EP + CPU fallback).
- Modify `Cargo.toml` (workspace) — add `tt-ml-scoring = { path = "crates/ml-scoring", optional = true }` + the `ort`/`ndarray` workspace deps (optional).
- Modify `crates/core/Cargo.toml` — add the `ml-scoring` feature (`ml-scoring = ["dep:tt-ml-scoring"]`) + the optional `tt-ml-scoring` dep.
- Modify `crates/core/src/lib.rs` — `#[cfg(feature = "ml-scoring")] pub mod learned_prose;` (gated, so the default build doesn't touch it).
- Create `crates/core/src/content_compress/learned_prose.rs` — the drop-in `compress(text) -> Option<(String, usize)>` calling the in-process scorer + reassembling via P1b's `segments`/`is_must_keep`/greedy.
- Modify `crates/core/src/content_compress/structural.rs` — the `compact_block` Prose arm gains the `prose-learned` gate check + the learned-path branch (DARK shadow: score + log, ship P1b).
- Modify `crates/core/src/content_compress/capture.rs` — add the structured shadow-log `record_shadow` (a `tracing` structured event, ZDR-safe).
- Create `docs/superpowers/recipes/p2b-training-recipe.md` — the off-repo RUNG 1/2 training recipe (PyTorch+MPS, the TT-route teacher, the ONNX export, the manifest).

---

## SLICE P2b

**Read first:** `crates/core/src/content_compress/structural.rs:281` (the `compact_block` Prose arm — the seam); `crates/core/src/content_compress/prose.rs:86` (`compress(text) -> Option<(String, usize)>` — the drop-in contract — + `segments`, `is_must_keep`, `content_tokens`, the `target_keep` greedy); `crates/core/src/passes/agentic_budget/summarize_judge.rs:89` (`NeverCommitGate` — the DARK shadow posture); `crates/core/src/content_compress/capture.rs` (the `record_pair` structured-log precedent); the spec `docs/superpowers/specs/2026-07-08-learned-compression-phase2b-design.md`.

### Task B1: the `tt-ml-scoring` lib crate (feature-flagged scaffold + the scorer API, no model yet)
**Files:** `crates/ml-scoring/Cargo.toml` + `crates/ml-scoring/src/lib.rs`; workspace `Cargo.toml` + `crates/core/Cargo.toml` + `crates/core/src/lib.rs` (the feature gate).
**Produces:** `pub struct Scorer { session: OnceLock<Arc<Session>> }` with `pub fn new() -> Result<Self>` (lazy-loads via `load-dynamic`; boots even if the `.onnx`/lib is absent, returns `Err` on first use), `pub fn score(&self, text: &str) -> Result<Vec<f32>>` (per-token keep-density; bounded by a hard-timeout), + the `level3` graph opt / warmup / `ManuallyDrop` scaffolding. The `ort` + `ndarray` deps are OPTIONAL (`default-features = false`, `features = ["load-dynamic", "copy-dylibs"]`). The default `cargo build -p tt-core` (no features) does NOT compile `ort`.
- [ ] **Step 1: Write failing tests.** (a) The `ml-scoring` feature compiles `tt-ml-scoring`; the default build (no features) does NOT pull `ort` (assert via a `cargo tree`-equivalent or a `#[cfg]` compile test). (b) `Scorer::new()` with no model path returns `Err` (not a panic — the gateway boots fine). (c) `score()` without a loaded session returns `Err` (fail-open). Gate the live-inference tests on `TT_ML_MODEL_PATH` (skip if unset — the CI doesn't have a model).
- [ ] **Step 2:** `cargo test -p tt-ml-scoring 2>&1 | tail -5` → FAIL (the crate doesn't exist). Build with `cd crates/core && cargo build --features ml-scoring` to confirm the feature compiles separately.
- [ ] **Step 3: Implement** the crate. Use `OnceLock<Arc<Session>>` for the session cache; `ort::session::SessionBuilder` (the 2.x API — `Environment` is global, commit via `.commit_from_file`); `GraphOptimizationLevel::Level3`; CoreML/Metal EP + CPU fallback via `with_execution_providers`; `ManuallyDrop<Arc<_>>` on the ort globals. The hard-timeout uses `tokio::time::timeout` over a `spawn_blocking` (the `ort` inference is sync).
- [ ] **Step 4:** run → PASS. **Step 5:** `cargo fmt` + clippy `--all-targets -- -D warnings` (with + without `--features ml-scoring`) + commit (`feat(ml-scoring): in-process ort scorer (feature-flagged, off by default)`).

### Task B2: the `prose-learned` class + the drop-in `learned_prose::compress` (no scorer wiring yet)
**Files:** `crates/core/src/content_compress/learned_prose.rs` (feature-gated); `crates/core/src/content_compress/prose.rs` (export `PROSE_LEARNED_CLASS = "prose-learned"`); `structural.rs` (the gate check in the Prose arm — DARK, ships P1b).
**Produces:** `pub const PROSE_LEARNED_CLASS: &str = "prose-learned";` `pub fn compress(text: &str, density: &[f32]) -> Option<(String, usize)>` — reuses `prose::segments()`, `prose::is_must_keep()`, `prose::content_tokens()` + the `target_keep` greedy, but feeds the model's `density` (a `Vec<f32>` per segment) instead of P1b's recency+salience heuristic. The `compact_block` Prose arm: after the `prose_gate` check, if `#[cfg(feature = "ml-scoring")]` AND `gate.is_committable("prose-learned")` → in DARK shadow: compute the learned candidate + log the delta BUT return `prose::compress(s)?` (ship P1b). When `ml-scoring` is OFF, the branch compiles out → byte-identical to today.
- [ ] **Step 1: Write failing tests.** (a) `learned_prose::compress` with a fixed `density` keeps the must-keep tokens + shrinks the block (the reassembly is the same path as P1b). (b) The `compact_block` Prose arm with `ml-scoring` OFF is byte-identical to today (the `#[cfg]` gate compiles out). (c) With `ml-scoring` ON + a `NeverCommitGate` on `prose-learned`, the arm ships P1b (DARK shadow — the learned path is gated shut by default). (d) With `ml-scoring` ON + `AlwaysCommitGate` on `prose-learned`, the arm STILL ships P1b in P2b (the shadow posture — commit is P2d).
- [ ] **Step 2:** tests FAIL. **Step 3: Implement** `learned_prose.rs` (the reassembly reuse) + the `#[cfg]`-gated branch in `structural.rs`. Pass the `density: &[f32]` (the test injects a fixed density; the real scorer is B3).
- [ ] **Step 4:** PASS. **Step 5:** `cargo test --workspace --no-run` EXIT 0 (the feature gate doesn't break the default build; `cargo build --features ml-scoring` compiles separately). fmt + clippy + commit.

### Task B3: wire the scorer into `learned_prose::compress` (behind `TT_ML_MODEL_PATH`)
**Files:** `crates/core/src/content_compress/learned_prose.rs` (the real scorer call + the hard-timeout + fail-open).
**Produces:** the `compress` fn, when `ml-scoring` is ON + a scorer is loaded (`TT_ML_MODEL_PATH` env), calls `Scorer::score(before)` → a per-token `density` → the reassembly. On a scorer error / hard-timeout → return `None` (fail-open to P1b). The scorer is a `OnceLock<Arc<Scorer>>` in `AppState` (or a module-level lazy, since the scorer is stateless once loaded). The `_density` the test injected in B2 is now the real scorer output.
- [ ] **Step 1: Write failing tests.** (a) With `TT_ML_MODEL_PATH` unset, `learned_prose::compress` returns `None` (fail-open, no scorer) — the request serves P1b. (b) With `TT_ML_MODEL_PATH` set to a stub ONNX (or gated behind a test feature), the scorer is loaded + `compress` returns a candidate (shrunk, must-keep preserved). (c) A simulated hard-timeout (inject a slow stub) → `None` (fail-open before the latency budget).
- [ ] **Step 2:** tests FAIL. **Step 3: Implement** the scorer wiring + the hard-timeout. The live-inference test gates on `TT_ML_MODEL_PATH` (skip if unset).
- [ ] **Step 4:** PASS. **Step 5:** fmt + clippy + commit.

### Task B4: the structured shadow log (`record_shadow`, ZDR-safe)
**Files:** `crates/core/src/content_compress/capture.rs` (a new `record_shadow` alongside `record_pair`); `structural.rs` (call it in DARK shadow).
**Produces:** `pub fn record_shadow(rec: &ShadowRecord)` — a `tracing` structured event at `target: "tt::compress::shadow"` carrying `{trace_id, content_hash, route, model, served_provider_id, p1b_tokens_removed, learned_tokens_removed, cache_hit, gate_committed, org_id, ts}`. NO raw text. The `content_hash` is a `blake3` hash of the `before` block (the keyed reference; raw text stays in the opt-in `TT_COMPRESS_CAPTURE` sink). `p1b_tokens_removed` + `learned_tokens_removed` are SELF-MEASURED per-block (the dispatcher discards the `usize` — so the shadow path tokenizes `before`/`after` for BOTH P1b + the learned candidate, in-process, + logs the deltas). Called from the `compact_block` Prose arm's shadow branch.
- [ ] **Step 1: Write failing tests.** (a) `record_shadow` emits a structured event with the right fields (assert via a `tracing` test subscriber). (b) NO raw `before`/`after` text is captured (assert the event fields don't include `before`/`after`). (c) The `content_hash` is stable (same `before` → same hash). (d) `p1b_tokens_removed` + `learned_tokens_removed` are the self-measured per-block deltas (a known `before`/`after` → known counts).
- [ ] **Step 2:** tests FAIL. **Step 3: Implement** `record_shadow` + the self-measurement (a `tt_tokenize::estimate_tokens_for_model` on `before` + each `after` — fast, in-process; the per-block tokenization is the "amortized background cost" the spec describes, but it runs on the shadow path which is already gated).
- [ ] **Step 4:** PASS. **Step 5:** fmt + clippy + commit.

### Task B5: the ZDR-safe content-hash memo cache (the pure perf optimization)
**Files:** `crates/core/src/content_compress/learned_prose.rs` (or a `learned_prose_cache.rs`); `structural.rs` (the cache lookup in the shadow branch).
**Produces:** a bounded `LruCache<Blake3Hash, Vec<f32>>` (the keep-density per `content_hash`) — cache HIT skips the scorer (the memo); cache MISS → score + memoize. Cold cache gets a real score (just re-runs the model — no fail-open-to-P1b-on-miss; the cache is a perf win, NOT an availability mechanism). The cache is feature-gated + bounded (e.g. 1024 entries; LRU eviction). The `cache_hit` field in the shadow log reflects the lookup.
- [ ] **Step 1: Write failing tests.** (a) A second call with the same `before` is a cache hit (the density is memoized; the scorer isn't called twice). (b) An LRU eviction on capacity. (c) The cache is ZDR-safe (only the hash + the density vector, no raw text).
- [ ] **Step 2:** tests FAIL. **Step 3: Implement** the LRU (a `std::collections::HashMap` + a manual LRU, or the `lru` crate if already a dep — check; else a bounded `HashMap`). Gate the cache size behind `TT_ML_CACHE_SIZE` (default 1024).
- [ ] **Step 4:** PASS. **Step 5:** fmt + clippy + commit.

### Task B6: the offline P2c eval harness (reads the shadow JSONL → recall-vs-deterministic)
**Files:** a new `crates/cli/src/eval_shadow.rs` + an `tt eval shadow` command (or a standalone script — the eval is offline, reads the shadow JSONL + the RUNG 3 verdicts).
**Produces:** `tt eval shadow --input <shadow.jsonl> --verdicts <quality_verdicts.jsonl> --output <report.json>` — reads the shadow log + joins `trace_id` → `quality_verdicts.request_id` → computes the recall-vs-deterministic delta on the held-out set (the `learned_tokens_removed` vs `p1b_tokens_removed` where the verdict is `Acceptable`). Names the POSITIVE recall bar over deterministic (the P2c promotion gate — "non-worse" is vacuous per the spec). This is the P2b-4 deliverable + the P2c input.
- [ ] **Step 1: Write failing tests.** (a) A shadow JSONL with N records + a verdicts JSONL with matching `trace_id`s → the report shows the per-trace `p1b`/`learned` deltas + the aggregate recall comparison. (b) A trace with no verdict is excluded (the RUNG 3 join). (c) A trace where the learned `tokens_removed > p1b` AND the verdict is `Acceptable` counts toward the positive bar.
- [ ] **Step 2:** tests FAIL. **Step 3: Implement** the harness (a pure offline reader — no gateway dep; reads JSONL + joins by `trace_id`). `tt eval shadow` is a CLI subcommand mirroring `tt export compress-corpus`.
- [ ] **Step 4:** PASS. **Step 5:** fmt + clippy + commit.

### Task B7: the off-repo training recipe (RUNG 1/2 documentation)
**Files:** `docs/superpowers/recipes/p2b-training-recipe.md`.
**Produces:** the RUNG 1/2 recipe the owner runs off-repo (PyTorch+MPS on the M4 Max, the TT-route teacher, the OpenAI-High subset, the ONNX export FP16-Metal + INT8-CPU, the reproducibility manifest). NOT in-tree code — a runnable recipe (the recipe points at a `training/` dir the owner creates off-repo; the `tt export compress-corpus` output is the input). Includes the manifest shape: `{teacher_model_id, teacher_pinning_config, rung1_pairs, rung2_pairs, onnx_export: {fp16_metal, int8_cpu}, training_run_id, ts}`.
- [ ] **Step 1-5:** write the recipe doc (no code/tests); it's the handoff artifact + the ONNX model the B3 scorer loads. Commit.

### Task B8: P2b PR
- [ ] `cargo fmt --all --check` (default + `--all-features`); `cargo clippy --workspace --all-targets -- -D warnings` (default) + `cargo clippy -p tt-core --features ml-scoring --all-targets -- -D warnings`; `cargo test --workspace --no-run` EXIT 0 (the default build is ML-dep-free); `cargo test -p tt-core --features ml-scoring --lib content_compress` green (the shadow path tests, gated on `TT_ML_MODEL_PATH` for the live ones). Push `feat/learned-compression-p2b`; PR "feat(core,ml-scoring): P2b — in-process scorer + DARK shadow wiring (Phase 2b)"; auto-merge.

---

## Self-Review
**Spec coverage:** the in-process scorer (B1) ✓; the `prose-learned` class + the drop-in (B2) ✓; the scorer wiring behind `TT_ML_MODEL_PATH` (B3) ✓; the structured shadow log (B4) ✓; the ZDR-safe memo cache (B5) ✓; the offline P2c eval harness (B6) ✓; the RUNG 1/2 training recipe (B7) ✓; the DARK shadow posture (ship P1b, score+log the learned) ✓; the fail-open (hard-timeout / scorer error / class-shut / feature-off) ✓.
**Default-build safety:** the `ml-scoring` feature is OFF by default; `cargo build -p tt-core` (no features) stays ML-dep-free (no `ort`/`ndarray` in the dep graph). The `learned_prose` module is `#[cfg(feature = "ml-scoring")]` — the default build is byte-identical to today. `cargo test --workspace --no-run` EXIT 0 proves no field-ripple on the default path.
**Hot-path safety:** the learned path is DARK shadow (ships P1b); the scorer is bounded by a hard-timeout (fail-open to P1b); `load-dynamic` so the gateway boots without the model; the memo cache is a perf win, not an availability mechanism.
**No migration, `rand` untouched.** P2b is the proof-of-savings model entering; P2c (RUNG 3 gold + promote) follows.

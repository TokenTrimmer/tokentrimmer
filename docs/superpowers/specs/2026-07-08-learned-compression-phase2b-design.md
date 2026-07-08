# Learned Compression — Phase 2b (in-process scorer + RUNG 1/2 model + DARK SHADOW wiring)

**Date:** 2026-07-08 · **Repo:** `public/` (gateway core + a new `tt-ml-scoring` lib crate, behind an off-by-default feature) + off-repo training (PyTorch+MPS on the M4 Max) · **Roadmap context:** Phase 2b of the learned-compression program. Grounded in the `phase2b-scoring-service-design` workflow's grounding (3 agents) + design panel (3 approaches) + **reconciled against the owner's 2026-07-07 in-process topology decision** (the customer-facing gateway runs ON the M4 Max; the model loads in-process, Metal EP, no network call — NOT the original spec's separate HTTP scoring service).

> **Owner constraints (binding, 2026-07-06 + the 2026-07-07 refinement):**
> 1. **Training on the M4 Max (128GB unified memory)** via PyTorch + the **MPS (Metal Performance Shaders) backend**.
> 2. **Proprietary model** — the P1d corpus + the weights are TT's exclusive asset.
> 3. **Serving = IN-PROCESS on the M4 Max gateway** (REFINED 2026-07-07; the original spec's separate `tt-ml-scoring-service` binary + HTTP cache is DROPPED). The model loads in the customer-facing gateway process behind an off-by-default `ml-scoring` feature flag; the Fly/cloud gateway stays ML-dep-free; the weights never leave owner infra.
> 4. **"Proof-of-savings" is core** — the VCR is a first-class product surface.
> 5. **Optimize for the best long-term solution.**

## Why this exists

P2a closed the label-gap + shipped the VCR + the honest corpus. P2b stands up the learned model itself: a small fast selective-context mBERT (~110M), trained on the P1d corpus with the real LLMLingua-2 teacher-essence recipe, scoring per-token keep/drop on prose blocks. The model SUBSTITUTES P1b's deterministic recency+salience keep-density with a semantic one — reusing P1b's `segments()`/`is_must_keep()`/reassembly — and ships DARK in SHADOW (score, compare to P1b, ship P1b, log delta) until RUNG 3 judge gold certifies a positive recall bar.

## The decisive architecture move (in-process, not cache-hybrid)

The original spec's default was **cache-hybrid OFF the hot path** (an async-populated content-hash LRU cache + a separate `tt-ml-scoring-service` HTTP binary). The design-panel's adversarial verify + the owner's 2026-07-07 refinement invert this: **the model runs IN-PROCESS on the M4 Max gateway** — no HTTP, no separate binary, no async cache-populate. Why both:
- **The owner's gateway IS the customer-facing surface** (the M4 Max instance). There is no network RTT to a separate scoring service to hide — they're the same machine.
- **In-process gives higher compression + lower latency** (every request gets a real score, no cold-cache → deterministic fallback; no network call). Strictly better than cache-hybrid on both axes IN THIS topology.
- **The headroom lesson is still honored** — but via **timeout + ratchet**, not via moving the model off-path. A ~110M mBERT inference on the M4 Max Metal EP (~5–30ms) + a hard-timeout (fail-open to deterministic P1b) + the 0.90-floor ratchet (auto-pause on bad recall) + auto-pause-on-timeout-rate are the safety nets. The model can run inline BECAUSE it can't hold the gateway hostage — a slow/heavy inference times out + the deterministic floor carries the request.

A small **content-hash memo cache** may be a pure perf optimization later (skip re-scoring identical system-prompt blocks across requests) — but it's NOT the core design, NOT an availability mechanism, + cold-cache gets a real score (just re-runs the model).

## Non-goals (Phase 2b)
- **No separate `tt-ml-scoring-service` binary, no HTTP client, no async cache-populate task, no `model_warm` network health probe.** (Dropped per the in-process decision.)
- **No on-the-fly cache-hybrid fail-open.** Cold cache → the model scores; the hard-timeout is the fail-open.
- **No RUNG 3 promotion.** P2b ships DARK shadow; RUNG 3 gold cert + promote-to-live are P2c/P2d.
- **No `code-learned`/Diff backend.** The model scores Prose only (the highest-value, hardest-to-do-deterministically kind); Code/Diff stay on P1c/verbatim.
- **No VCR schema-v2 `model_evidence_hash` field in P2b.** (A later P2.5 slice, once the model is live + the evidence shape is proven.)

## Verified grounding (live code, 2026-07-08, workflow-grounding)
1. **The seam (where the learned path slots in):** `crates/core/src/content_compress/structural.rs:281` — the `ContentKind::Prose` arm:
   ```rust
   let gate = prose_gate?;
   if !gate.is_committable(prose::PROSE_CLASS) { return None; }
   prose::compress(s)?.0   // ← the learned branch slots in here, AFTER the prose_gate
   ```
   The learned path consults a SEPARATE `prose-learned` gate (independent ratchet + allowlist), then the in-process scorer.
2. **The drop-in contract:** `prose::compress(text) -> Option<(String, usize)>` (`prose.rs:86`). The dispatcher at `structural.rs:288` calls `.0` (the compacted string) + **DISCARDS the `usize`** — so any per-block token-delta the learned path computes for the shadow comparison CANNOT ride the drop-in return value. Attribution is the pipeline-MEASURED whole-tail delta. The shadow "log the delta" must self-measure per-block (byte-diff or a quick tokenize of P1b-output vs learned-output) + emit on a SEPARATE channel (the structured `tt::compress::shadow` log).
3. **The corpus (what RUNG 2 trains on):** `CorpusPair` schema v2 (`compress_corpus.rs:102`): `{kind, before, after, tokens_before, tokens_after, tokens_removed, gate_committed, confidence, billed_metric_tokens_removed: Option<u32>, org_id, trace_id, model, provider_id, ts}`. `billed_metric_tokens_removed` is `Some(delta)` ONLY when BOTH `est_before.confidence` + `est_after.confidence` == High (OpenAI tiktoken rows). The export post-filter (`replay_rejected`) already drops pairs whose `after` never shipped → RUNG 2's `before` input is guaranteed-compressed.
4. **The headroom trap (confirmed):** RUNG 2 targets the teacher-essence labels (teacher LLM extracts essence from UNCOMPRESSED `before`, labels original tokens by overlap) — NOT `CorpusPair.after` (the P1b 0.60 deterministic output). Training to reproduce `after` = P1b-equivalence at +latency = the headroom failure mode.
5. **The `ort` reality (corrected from the original spec):** `ort` 2.x did NOT remove the global `Environment` (it still exists in `src/environment.rs` as a `static G_ENV`); what changed is the construction API (no `&Arc<Environment>` into `SessionBuilder::new`; one global committed via `ort::session::commit`). `ort` 2.0.0-rc.12 wraps ONNX Runtime 1.24 + ndarray ^0.17. NeuroGlyph pinned `ort 1.16` (`load-dynamic` + `copy-dylibs`); P2b writes fresh against 2.x (port the ORCHESTRATION patterns — session cache, warmup, hard-timeout-with-cached-fallback, `ManuallyDrop` — write fresh against the new construction API).
6. **The model on the M4 Max Metal EP:** no published CPU/Metal latency numbers for LLMLingua-2 exist; estimate ~5–30ms per scored segment (110M mBERT, max_seq_len=512, Metal EP). The hard-timeout (e.g. 50ms) is the bound; on expiry → deterministic P1b.

## Architecture (in-process, Metal EP, DARK shadow)

```
                  OWNER INFRA (M4 Max gateway)                    OFF-REPO (training)
                  ─────────────────────────────────                ──────────────────
   training ─→  PyTorch+MPS, LLMLingua-2 recipe
   (off-repo)    teacher LLM (a TT route) → essence → token labels
                 → mBERT-base ~110M → ONNX (FP16 Metal / INT8 CPU fallback)
                  │
                  ↓ (load once at boot, lazy + 3-warmup + ManuallyDrop, feature-flagged)
                ┌─────────────────────────────────────────────────┐
   request →    │ compact_block Prose arm (structural.rs:281):     │
                │  gate('prose-learned')? (independent ratchet)    │ → token-true gate → dispatch
                │  → in-process scorer (OrtCoreML, hard-timeout)  │ → isolated $ saved
                │  → reassemble via prose::segments/is_must_keep   │ → capture::record_pair
                │  ── DARK SHADOW: also compute P1b, log delta ──  │ → tt::compress::shadow log
                │  ── SHIP P1b (shadow) UNTIL P2c promotes ──     │
                └─────────────────────────────────────────────────┘
```

### The in-process scorer (`tt-ml-scoring` lib crate, behind `ml-scoring`)
A new `crates/ml-scoring` LIBRARY crate (NOT a binary — it links into the M4 Max gateway build) holding the `ort` dep + the scorer:
- **ort pin:** `ort = { version = "2.0.0-rc.12" (or the latest stable 2.x), default-features = false, features = ["load-dynamic", "copy-dylibs"] }`. `load-dynamic` so the gateway boots even if the `.onnx`/lib is absent; lazy-loads on first use.
- **Execution providers:** CoreML/Metal EP primary + CPU fallback (the `ExecutionProviderDispatch` list on `SessionBuilder::with_execution_providers`).
- **Session cache + warmup:** session cache (a `OnceLock<Arc<Session>>`); `GraphOptimizationLevel::Level3`; 3 warmup dummy inferences on a background thread at boot.
- **Hard-timeout-with-cached-fallback:** the scoring call is bounded (e.g. 50ms) — on expiry → fail-open to deterministic P1b. Port NeuroGlyph's `hard-timeout-with-cached-fallback` + `timeout_counter` pattern.
- **`ManuallyDrop<Arc<_>>` on the ort globals:** dodge the macOS-exit SIGABRT (the gateway must restart cleanly on deploy; the ML dep lives ONLY behind the feature flag).
- **Feature-flagged:** the `ml-scoring` feature on `tt-core` (or `tt-cli` for a self-hosted owner build) is OFF by default. The public/crate default build + the Fly/cloud gateway build don't enable it → they stay ML-dep-free (no `ort`+`ndarray`+`.onnx` in those images). Only the owner's M4 Max gateway build enables `ml-scoring` + carries the ~220MB model on disk.

### The DARK SHADOW wiring (`learned_prose.rs`)
A new sibling `crates/core/src/content_compress/learned_prose.rs` exporting the SAME `pub fn compress(text: &str) -> Option<(String, usize)>` contract the dispatcher already calls. The `compact_block` Prose arm gains a learned-path branch: if `gate.is_committable("prose-learned")` AND the `ml-scoring` feature is compiled in AND a session is loaded → call `learned_prose::compress`. It reuses `prose::segments()`, `prose::is_must_keep()`, `prose::content_tokens()` VERBATIM but feeds the model's per-segment keep-density into the existing target_keep greedy selection (must-keep overrides + Jaccard>0.6 dedup + the strict byte-shrink guard at `prose.rs:190` unchanged).

**DARK SHADOW (the P2b-only posture):** the learned path SCORES + REASSEMBLES, but the dispatcher SHIPS deterministic P1b + emits the delta to a structured `tt::compress::shadow` JSONL log. The learned compress output is computed for offline recall eval (P2c), NOT committed to the dispatched request. Promotion (ship the learned output) is P2d.

**The shadow log (the structured `tt::compress::shadow` JSONL):** per compressed block:
`{trace_id, content_hash, route, model, served_provider_id, p1b_tokens_removed (the deterministic floor), learned_tokens_removed (or None if cache cold / scorer timed out), cache_hit, gate_committed, org_id, ts}`. ZDR-safe (no raw text — the `content_hash` is the keyed reference, raw text stays in the opt-in `TT_COMPRESS_CAPTURE` sink). The offline P2c eval reads this JSONL + joins verdicts by `trace_id` to compute the recall-vs-deterministic delta.

### The training pipeline (off-repo, PyTorch+MPS, a TT-route teacher)
- **Model:** mBERT-base-cased ~110M (12L/768H, max_seq_len=512). Bidirectional encoder + per-token keep/drop binary head (NOT autoregressive — output ⊆ input → faithful-by-construction → signable → textual so every third-party model ingests it).
- **RUNG 1 (warm-start, recall=1.0 provable, no judge):** lossless structural pairs from the P1d corpus (JSON whitespace-minify, CSV trailing-ws strip, log collapse). Honest: near-zero-information for the prose keep/drop task (primes the encoder).
- **RUNG 2 (teacher distillation, the LLMLingua-2 REAL recipe):** the teacher LLM extracts an essence from the UNCOMPRESSED `before` + labels original tokens by overlap — restricted to OpenAI-High pairs (so `billed_metric_tokens_removed` is the billed-reconcilable metric). The teacher = a TT ROUTE (dogfood; reproducibility = `model_id` + pinning config in the training-run manifest).
- **ONNX export:** PyTorch → ONNX → ORT format; FP16 for the Metal/CoreML EP primary (~220MB) + INT8 dynamic quantization for the CPU fallback (~220MB). Target deployed artifact ≤ ~250MB on owner disk (behind the feature flag, never in the Fly image).
- **Re-certify deployed numerics:** the deployed FP16-on-Metal (or INT8-CPU) numerics run through the quality judge BEFORE the ratchet trusts `prose-learned` (P2c).

### The moat-wrap (5 layers, fail-open at every layer)
1. **GATE CLASS KEY:** a NEW separate `prose-learned` class on the shared `RatchetSummaryGate` (`structural.rs:138` patterns) — NOT sharing `PROSE_CLASS`. Operator opens via `TT_SUMMARIZE_TRUSTED_CLASSES`. Ships DARK behind the ratchet, not default-on. Independent ratchet + allowlist → a bad learned model darkens ONLY the learned path; deterministic P1b on `prose` keeps serving.
2. **0.90-FLOOR:** `RatchetSummaryGate::is_committable("prose-learned")` returns false when the windowed pass-rate drops below floor=0.90 over ≥5 acceptable samples in a window of 20.
3. **TOKEN-TRUE GATE:** the four-conjunct strict-greater predicate (`passes/mod.rs:384`). The model's output is a re-ranked candidate fed to deterministic reassembly; extractive means by-construction cannot inflate text tokens.
4. **ISOLATED ATTRIBUTION:** savings flow to `content_compress_saved_est_usd` (P1a's field, unchanged). The model's self-reported token delta is informational-only + IGNORED for attribution; only the pipeline recount books.
5. **FAIL-OPEN POSTURE:** session not loaded / scorer error / hard-timeout / class-shut / feature-flag-off → return `None` → the deterministic P1b candidate serves (NOT verbatim — the deterministic backend is the floor) → zero EXTRA savings, zero error propagated to the request. The `timeout_counter` + `cache_hit_rate` (when the memo cache is added) feed the auto-pause-on-timeout-rate.

## Slices (each spec/plan/build, P2b)

### P2b-1 — The in-process scorer lib crate (`tt-ml-scoring`, feature-flagged)
Stand up the `crates/ml-scoring` library crate (behind `ml-scoring` feature, OFF by default) holding `ort` + the OrtCoreML scorer: session cache + 3-warmup + Level3 graph opt + the hard-timeout-with-cached-fallback + `ManuallyDrop` + the CoreML/Metal EP primary + CPU fallback. Port NeuroGlyph's orchestration patterns; write fresh against `ort` 2.x's construction API. NO gateway wiring yet.

### P2b-2 — Train RUNG 1 + RUNG 2 (off-repo, PyTorch+MPS, a TT-route teacher)
Train the mBERT `keep/drop` head. RUNG 1 (lossless structural pretrain) + RUNG 2 (LLMLingua-2 teacher-essence recipe, OpenAI-High pairs, teacher = a pinned TT route). ONNX export FP16-Metal + INT8-CPU. The reproducibility manifest records the teacher `model_id` + pinning config.

### P2b-3 — The DARK SHADOW wiring (`learned_prose.rs` + the compact_block seam)
New `learned_prose.rs` sibling exporting the drop-in `compress` contract; the `compact_block` Prose arm gains the learned-path branch (gated by `prose-learned` + the feature flag + a loaded session). DARK: scores + reassembles, SHIPS P1b, logs the delta to the structured `tt::compress::shadow` JSONL log (ZDR-safe, no raw text). The shadow log carries the P2c eval inputs (`p1b_tokens_removed`, `learned_tokens_removed`, `content_hash`, `cache_hit`, `gate_committed`).

### P2b-4 — The shadow log → offline P2c eval harness + the ZDR-safe content-hash memo cache
The offline harness that reads the shadow JSONL + joins `trace_id` → `quality_verdicts` (the RUNG 3 gold, accumulating from P2a's eligibility closure) + computes the recall-vs-deterministic delta on a held-out set. Plus the ZDR-safe content-hash memo cache (`content_hash → keep-density`, never raw text) as a pure perf optimization (skip re-scoring identical blocks) — cold-cache gets a real score.

## P2c/P2d boundary (DEFERRED)
- **P2c:** RUNG 3 gold accumulate + INT8/FP16-on-Metal judge re-cert + held-out recall eval (the harness from P2b-4 produces the gate) — name the POSITIVE recall bar over deterministic (not "non-worse").
- **P2d:** promote `prose-learned` from SHADOW (compute, compare, ship P1b) to COMMIT (compute, ship learned) — the operator allowlist + the 0.90-floor + the token-true gate + auto-pause-on-timeout-rate.

## Risks & landmines
- **Latency unproven on Metal** — no published numbers; ~5–30ms estimate; the hard-timeout (e.g. 50ms) is the bound. Benchmark on the M4 Max before P2b-3 ships.
- **The macOS-exit SIGABRT** — the `ort` globals + the ONNX Runtime must `ManuallyDrop<Arc<_>>` or the gateway SIGABRTs on restart. Behind the feature flag, the public/Fly builds are unaffected.
- **The teacher-as-TT-route reproducibility** — the teacher becomes the model's compression-policy prior; the training-run manifest records the `model_id` + pinning config. A pinning-config drift between training + serving could change the labels.
- **Dataset size** — cold-start depends on the catalog's default-on `content_compress` (P2a); RUNG 2 needs the OpenAI-High subset. RUNG 1 (no labels) bootstraps; RUNG 2 ships.
- **VCR schema-v2 `model_evidence_hash`** — deferred to P2.5 (a later slice, once the model is live + the evidence shape is proven); the receipt stays flat in P2b.
- **Cross-repo drift** — the `ml-scoring` feature on `tt-core` is OFF by default; the cloud pin doesn't enable it (the Fly gateway stays ML-dep-free). Only the owner's M4 Max gateway build enables it.

## Related
- `docs/superpowers/specs/2026-07-06-learned-compression-phase2-design.md` — the Phase-2 spec (P2a through P2.5).
- `docs/superpowers/plans/2026-07-06-learned-compression-p2a.md` — the P2a plan (shipped: #283 #216).
- [[owner-ml-hardware-m4max]] — the in-process topology + the PyTorch+MPS decision.
- [[compression-model-program]] — the program memory (P1a-d + P2a shipped).

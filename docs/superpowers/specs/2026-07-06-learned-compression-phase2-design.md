# Learned Compression — Phase 2 (selective-context prose model + the signed flywheel)

**Date:** 2026-07-06 · **Repo:** `public/` (gateway core) + a new owner-infra `tt-ml-scoring-service` + off-repo training · **Roadmap context:** Phase 2 of the learned-compression program. Grounded in the `phase2-learned-compression-design` workflow (13 agents, 2026-07-06) + adversarial verification against live code (commit `633ad24`, Phase 1 P1a–d merged).

> **Owner constraints (2026-07-06, binding):**
> 1. **Training on the owner's M4 Max (128GB unified memory)** via PyTorch + the **MPS (Metal Performance Shaders) backend** — no CUDA, no GPU rental. The owner's machine is the training rig.
> 2. **Proprietary model** — the P1d training corpus + the weights are TT's exclusive asset; the model is never open-sourced or distributed.
> 3. **"Proof-of-savings" is core** — the Verifiable Compression Receipt (VCR) is a first-class product surface, not an afterthought.
> 4. **Serving = self-hosted on the M4 Max** — the proprietary model serves ONLY from owner infra (Metal EP inference); the weights NEVER leave the owner's machine. Customers route through the owner's managed TT gateway. The Fly-hosted cloud gateway does NOT run the model.
> 5. **Optimize for the best long-term solution**, not the safer/short-term option.

## Why this exists

Phase 1 closed the *honesty gap* (deterministic content-aware compression, wrapping TT's moat) and shipped the *flywheel* (P1d's judge-labeled capture corpus). Phase 2 closes the *ceiling gap*: the deterministic prose extractive backend (P1b) is a recency+salience+keyword heuristic — it has no *semantic* access to the text. A small learned model, trained on the P1d corpus with the real LLMLingua-2 teacher-distillation recipe, can re-rank P1b's keep/drop decisions using semantic understanding P1b lacks, and push the keep-ratio below P1b's 0.60 floor — while staying **extractive** (output ⊆ input → faithful-by-construction → signable → every third-party model ingests it).

The decisive design move (the thing that dodges headroom's failure and TT's label-circularity trap): **the model runs OFF the hot path.** A background task populates an LRU content-hash → per-segment-keep-density cache by calling an owner-hosted scoring service; the live `compact_block` request does a microsecond hash lookup — cache miss falls open to deterministic P1b inline + async-backfills the cache. The model is an amortized background cost, never a per-request tax; fail-open is genuine latency-fail-open, not correctness-fail-open-after-burning-walltime.

## Roadmap (this spec = Phase 2 only)
- **Phase 1 (shipped):** deterministic content-aware compressor + the flywheel. P1a-d merged (#279/#280/#281/#282).
- **Phase 2 (this spec):** a small fast learned selective-context model, trained on the P1d corpus, served off-hot-path from owner infra, augmenting the deterministic P1b prose backend behind an independent ratchet. Signable extractive outputs feed the VCR (P2.5).
- **Phase 3 (separate research spec):** the "AI language" dense-latent codec (gist-token/ICAE, 10–100× ceiling) for TT-hosted/open models where TT controls both ends. DEFERRED — third-party APIs accept text/tokens, not learned embeddings.

## Non-goals (Phase 2)
- **No autoregressive rewriter.** The model emits per-token keep/drop over the input's own tokens (output ⊆ input). Explicitly DECLINE the replace-on-Prose rewriter's 2–5× ceiling in exchange for the faithful-by-construction + signable property the moat requires.
- **No replace-of-P1b on the hot path.** The deterministic P1b backend stays in-tree as the cold-cache floor, the shadow-comparison baseline, AND the fail-open target. The model AUGMENTS (re-ranks the candidate set), never replaces.
- **No model on the Fly gateway.** The gateway binary stays ML-dep-free (no `ort`, no `ndarray`, no `.onnx` in the Fly image) — no build-time flakiness, no image bloat, no macOS SIGABRT on gateway restart. The model + `ort` live ONLY in the scoring-service crate on owner infra.
- **No new migration for cost attribution.** Savings still book into the isolated `content_compress_saved_est_usd` (P1a's field + migration 0033). The model adds a backend, not a cost field.
- **Code-learned + Diff DEFERRED.** A `code-learned` class is a P2.5 stretch (the code backend's re-parse-verify makes per-segment keep/drop harder; the cleaner code slot is a segment score for *which* functions to truncate-deeper vs leave, replacing `BODY_MIN_LINES`). Diff stays verbatim (no Phase-1 backend). The VCR is P2.5 (follows the model's promotion, does not gate it).

## Verified grounding (live code, 2026-07-06)
1. **The corpus:** `crates/cli/src/compress_corpus.rs` — `TrainingCorpus { schema_version, pairs: Vec<CorpusPair> }`, each `CorpusPair = {kind, before, after, tokens_before, tokens_after, tokens_removed, gate_committed, org_id, trace_id, model, provider_id, ts}`. The export recomputes `tokens_*` offline via `tt_tokenize` (the hot path records 0). ZDR: refuses any record not `capture_opted_in: true`.
2. **The dispatcher contract:** `compact_block(s, prose_gate, code_gate, capture) -> Option<String>` in `crates/core/src/content_compress/structural.rs` — classifies → routes to a backend → returns `Some(result)` only on a strict byte-shrink. The model slots in as a drop-in `fn(&str) -> Option<(String, usize)>` sibling (`learned_prose.rs`), called when the gate trusts a new `prose-learned` class + the cache has an entry.
3. **The moat (the model MUST ride):** the pass token-true gate (`passes/mod.rs:384` — a four-conjunct strict-greater predicate: after.tokens > before.tokens / serialized JSON inflated / message-count grew / non-text parts changed → restore verbatim); `RatchetSummaryGate` with `RatchetConfig::default` (floor=0.90, window=20, min_samples=5, cooldown=3600s) in `summarize_judge.rs:236-245`; the isolated `content_compress_saved_est_usd` attribution (never the invoice-reconciled headline).
4. **The label gap (P1d's honest limitation):** `CaptureRecord::new` hard-codes `gate_committed=true` (`capture.rs`) — it records "a compaction was committed" (a tautology), NOT "recall of the baseline response was preserved." The real recall verdict comes from the response-side judge (the `~2%` paired recall-of-baseline), which `content_compress` never sees. Closing this join is P2a.
5. **The serving precedent:** NeuroGlyph's `backend/src/onnx.rs` is a production-hardened ort-in-Rust path (ort 1.16.3 + ndarray 0.15, `load-dynamic`+`copy-dylibs`, session cache, warmup, hard-timeout-with-cached-fallback, `ManuallyDrop<Arc<_>>` to dodge the macOS-exit SIGABRT). Adapt the ORCHESTRATION patterns; write fresh against ort 2.x (ort 2 removed the global `Environment`, moved to ndarray 0.17).
6. **The headroom lesson (re-inverted):** headroom trained Kompress-v2-base (dual-head ModernBERT) then built a deterministic Rust fallback (`TextCrusher`) because the model was too slow for production. TT's move: the model is advisory + off-hot-path + gated; the deterministic engine + token-true gate + judge remain the authority. The model augments, never blocks.
7. **The label-circularity trap (avoided):** a model distilled to *imitate P1b's 0.60 after* is P1b-equivalence at +latency (the exact headroom failure mode). The LLMLingua-2 *real* recipe (teacher LLM extracts essence from the UNCOMPRESSED `before`, labels original tokens by overlap) has semantic access P1b lacks and CAN push below 0.60 + improve keep-RANKING.

## Architecture

```
                      OWNER INFRA (M4 Max)                         FLY (cloud gateway)
                      ──────────────────                            ────────────────────
   training ─→  PyTorch+MPS, LLMLingua-2 recipe                       (no model here)
   (off-repo)    teacher LLM → essence → token labels
                 → mBERT-base ~110M → ONNX (FP16 Metal / INT8 CPU)
                      │
                      ↓
                tt-ml-scoring-service (long-running, ort 2.x + Metal EP)
                │  LRU content-hash → per-segment-keep-density cache
                │  (async-populated by a background gateway task)
                ↓  HTTP (microsecond hash lookup on the live path)
                ┌─────────────────────────────────────────────────┐
   request →    │ compact_block:                                   │
                │  classify → Prose → gate('prose-learned')?      │
                │    cache hit → learned_prose::compress (re-rank) │ → token-true gate → dispatch
                │    cache miss → deterministic P1b inline +       │ → isolated $ saved
                │                 async-backfill cache             │ → capture::record_pair
                └─────────────────────────────────────────────────┘
```

### The augment-only thesis (why over replace-on-Prose)
1. **Survives the headroom lesson.** A model on the hot path can kill the gateway SLO (a slow inference blocks every request). The model is OFF the hot path; the deterministic P1b backend stays inline as the floor + the fail-open target. The model can never block a request.
2. **Survives the label-circularity refutation.** A replace-P1b model distilled from P1b's `after` labels is P1b-equivalence at +latency. The teacher-essence recipe trains against the *uncompressed `before`* (not P1b's output), giving the model semantic access P1b lacks and a real ceiling above 0.60.
3. **Survives the cold-start trap.** A replace-P1b model's value claim is unprovable at launch (no RUNG 3 gold yet). An augment-only model ships DARK in SHADOW (score, compare, ship deterministic P1b, log delta) and promotes to commit ONLY when RUNG 3 judge-certified gold shows a POSITIVE recall bar over deterministic on a held-out set.
4. **Signable.** Extractive output ⊆ input → faithful-by-construction → the VCR's recall verdict is real, not a model-aligned tautology.

### The serving decision (owner-infra, off-hot-path)
Per the 2026-07-06 owner resolution: the model serves ONLY from a `tt-ml-scoring-service` running on the owner's M4 Max. The weights live on owner-infra disk (never in any Fly/Docker image). A background async task per opted-in route pre-populates an LRU content-hash → per-segment-keep-density cache over HTTP; the live `compact_block` does a microsecond hash lookup. **Latency budget:** live request = hash lookup only (microseconds, well under the sub-30ms p50 SLO); background populate = network RTT (5–50ms, MUST be measured — drives where the service physically lives) + Metal inference (mBERT ~5–30ms on an M4 Max, must be benchmarked). **Fail-open:** cache-miss / service-down / timeout → deterministic P1b inline + async-backfill; sustained cache-populate-failure-rate > threshold → auto-pause the `prose-learned` class → deterministic-only (zero degradation). **Single point of failure:** if owner infra is down, the cache stops populating and the gateway silently serves deterministic P1b.

### Model class + training
- **Model:** mBERT-base-cased, ~110M params (12L/768H), max_seq_len=512 (the scorer runs over one segment window at a time, never whole multi-KB blobs). Bidirectional Transformer-encoder + per-token keep/drop binary head. NOT autoregressive. Explicitly NOT XLM-R-large (355M) or ModernBERT-large (395M) — 110M is the conservative target that ports to an INT8-CPU fallback if owner-infra Metal is ever unavailable, and matches LLMLingua-2-small.
- **Training framework:** PyTorch + MPS backend on the M4 Max. The LLMLingua-2 REAL recipe — a teacher LLM extracts an essence from the UNCOMPRESSED `before` and labels the original tokens by overlap with that essence (NOT targeting P1b's 0.60 `after`, which is circular).
- **Export:** PyTorch → ONNX → ORT format. FP16 for the Metal/CoreML EP primary path (~220MB); INT8 dynamic quantization as the CPU-fallback path (~220MB). Target deployed artifact ≤ ~250MB on owner disk.
- **Re-certify deployed numerics:** the deployed FP16-on-Metal (or INT8-CPU) numerics are run through the quality judge BEFORE the ratchet trusts the `prose-learned` class — a quantized model can drop edge-case recall the F32 passed.

### The label-honesty closure (three-rung bootstrap; the model never self-labels)
- **RUNG 1 — warm-start (recall=1.0 PROVABLE, no judge):** the lossless structural pairs in the P1d corpus (JSON whitespace-minify, CSV trailing-ws strip, log collapse). HONEST: this is a near-zero-information warm-start for the prose keep/drop task (it teaches drop-the-whitespace, which the structural backends already do perfectly); it primes the encoder, does not close the prose label gap.
- **RUNG 2 — teacher distillation (the LLMLingua-2 REAL recipe, no response-side verdict):** the teacher LLM annotates per-token keep/drop on captured prose pairs by generating an essence from the UNCOMPRESSED `before` and labeling original tokens by overlap — restricted to OpenAI-served High-confidence pairs (so the per-pair `tokens_removed` the teacher optimizes is the billed-reconcilable metric, not the Anthropic cl100k+correction Medium proxy ~15–20% off). This is the tier that ships the model.
- **RUNG 3 — judge-certified gold (the only NON-circular tier AND the value-justifying tier):** recall-of-baseline verdicts via the join `CorpusPair.trace_id` → `quality_verdicts.request_id` ↔ `request_body_captures.trace_id`. The closure:
  - `judge_original_req` is captured PRE-routing and PRE-compression at `chat.rs:2456` (before `apply_routing` at 2460, before the content_compress pass at 3231+), so the existing `ReferenceSource::Dispatch` (`quality_sample.rs:1243`-1262) re-dispatching `judge_original_req` ALREADY re-runs the SAME served model on the UNCOMPRESSED `before`. **The compression-specific isolation is mostly a NEW eligibility predicate** (add `content_compress` to the eligibility set, which today = route-downgrade/shaped only) + raise the ~2% judge sample rate on opted-in capture traffic — NOT a whole new dispatch (smaller work than naive).
  - Add the `request_logs.trace_id` partial index (`CREATE INDEX ... WHERE trace_id IS NOT NULL`) + the cast `quality_verdicts.request_id::text = request_logs.trace_id` (migration 0014 explicitly flags this as a Phase-2 task; the reverse cast throws on non-UUID values).
  - HONEST scarcity: gold is near-zero at launch BY CONSTRUCTION (compression-only not eligible + ~2% sample + async flaky judge + the eligibility predicate is new code). Gold is the Phase-2 endgame, NOT the day-one gate.

### Corpus hygiene (before any label is trusted)
1. **Post-filter to pairs that ACTUALLY shipped:** re-run each captured pair through `PassPipeline::run` with the same gate config and drop pipeline-rejected rewrites (capture writes inside `compact_block` at `structural.rs:310`-322 BEFORE the pipeline gate at `mod.rs:384`, so the sink holds rewrites whose `after` never shipped).
2. **Restrict billed-metric token-delta ground-truth to OpenAI-High rows:** drop Low-confidence rows from training-pair deltas (the export uses the SAME estimator as the gate, just discards `Confidence` — Anthropic rows are a Medium proxy ~15-20% off; tiktoken-load-failure rows are chars/4 while the live gate books $0 on Low).

### The moat-wrap (five layers, fail-open at every layer)
1. **GATE CLASS KEY:** a NEW separate `prose-learned` class on the SAME shared `RatchetSummaryGate` production already wires (`structural.rs:138`-144) — NOT sharing `PROSE_CLASS="prose"`. The operator opens `prose-learned` via `TT_SUMMARIZE_TRUSTED_CLASSES` exactly as `prose` is opened today; ships DARK behind the ratchet, not default-on. **Why separate (decision rationale):** independent ratchet + independent allowlist → a bad learned model darkens ONLY the learned path while deterministic P1b keeps serving on `prose`. The shared-key alternative was refuted as internally contradictory (if augment REPLACES the deterministic scoring AND shares the key, a ratchet shutdown removes the deterministic scoring too).
2. **0.90-FLOOR:** `RatchetSummaryGate::is_committable("prose-learned")` returns false when the windowed pass-rate drops below floor=0.90 over ≥5 acceptable samples in a window of 20 (`RatchetConfig::default`). The model inherits the real-time cooldown recovery it does NOT control (a shut class stays shut 3600s even after a hotfix) AND the out-of-band judge feedback latency (a bad model-version serves up to min_samples judged requests before any ratchet fires — Phase-2 eval must account for this).
3. **TOKEN-TRUE GATE:** the four-conjunct strict-greater predicate (`passes/mod.rs:384`-394). The model's output is a re-ranked candidate fed to deterministic reassembly (must-keep overrides + Jaccard>0.6 dedup + strict byte-shrink guard at `prose.rs:190`) then the pipeline gate recounts. Extractive means by-construction cannot inflate text tokens, but a dropped token can merge tiktoken BPE boundaries and trip the gate (yielding zero savings, not a wrong output — the gate catches it). The model is tuned to UNDER-compress so the gate rarely fires.
4. **ISOLATED ATTRIBUTION:** savings flow to `content_compress_saved_est_usd` (the isolated estimate, NEVER the invoice-reconciled headline). The model's self-reported token delta is informational-only and IGNORED for attribution; only the pipeline recount of `tail_text()` books.
5. **FAIL-OPEN POSTURE:** ort load failure / inference error / network error / scoring-service timeout / `model_warm=false` / class-shut / cache-miss → log + per-instance counter → return `None` → the deterministic P1b candidate serves (NOT verbatim — the deterministic backend is the floor; the model only improves it) → zero EXTRA savings, zero error propagated to the request.

### The integration point
A new sibling `content_compress/learned_prose.rs` exporting the SAME `fn(&str) -> Option<(String, usize)>` contract the dispatcher already calls. The Prose arm of `compact_block` (`structural.rs:281`-289) gains a learned-path branch: if `gate.is_committable("prose-learned")` AND the route opted into `content_compress` AND `model_warm=true` AND a cache entry exists for this content-hash AND the block is above a `MODEL_MIN_CHARS` threshold (higher than `PROSE_MIN_CHARS=600` — the model only runs where better recall justifies the cache cost), call `learned_prose::compress`. It reuses `prose::segments()`, `prose::is_must_keep()`, `prose::content_tokens()` VERBATIM but feeds the model's per-segment keep-density into the existing target_keep greedy selection. The dispatcher, `is_committable`, the byte-shrink check, and the capture write stay byte-identical.

### Invariants honored
- **Cannot-reclassify:** the classifier still routes first-match Diff→Json→Csv→Log→Code→Prose; the model only sees Prose-kind blocks; JSON-shaped code blocks still take the structural minify path (the `non_code_json_still_takes_structural_path_with_open_gate` test guards this).
- **Touchable-region:** the pass operates only on `VolatileTail.messages_mut()` System+Tool text; the model cannot reach the cache-stable prefix, the `model` field, tools, or pricing (the `RequestPass` trait) — so **model-aware conditioning is NOT available** to the pass; the corpus carries `model`/`provider_id` as JOIN keys for the judge, not as conditioning inputs. Phase 2 is model-agnostic.

## Risks & landmines
- **Latency unproven on Metal** — no published CPU/Metal numbers for LLMLingua-2 exist; the mBERT ~5–30ms estimate MUST be benchmarked on the M4 Max before P2b ships, and drives the fail-open timeout.
- **Network RTT to owner infra** — where the scoring service physically lives (home/office M4 Max vs colo vs near-region Fly box) drives BOTH the latency budget AND the availability story. Home internet reliability is a real SPOF.
- **Cold-start dataset volume** — `content_compress` defaults to FALSE on every route and requires explicit per-route opt-in. `~10k prose-compress req/day` day-one depends on operator/owner dogfooding or default-on expandable routes that don't exist in production today. Gates the P2b training timeline.
- **RUNG 3 judge tax (unbounded spend)** — making compression-only requests judge-eligible means re-dispatching the SAME served model on the uncompressed `before` on org creds. This is NEW measurement spend that MUST be capped (per-org/per-day cap + grace behavior when hit, or it bills unbounded). Migration 0014 lines 32-34 suggest judge cost was excluded — confirm.
- **Teacher LLM = the model's compression-policy prior** — GPT-4o-class (high quality, API cost + reproducibility risk if the API changes) vs Claude vs an open model (Llama-3.3-70B for cost + full reproducibility). A real fork, not a detail.
- **Ort 2.x ≠ NeuroGlyph's ort 1.16** — the global `Environment` is gone; `ndarray` 0.17. Write fresh; port patterns, not code. The macOS-exit SIGABRT on `ort` Environment drop is now a scoring-service concern (the gateway restarts cleanly on deploy because it never touches `ort`).
- **Quantization drift** — FP16-on-Metal or INT8-CPU can drop edge-case recall the F32 passed; the re-certify-before-trust step (RUNG 3) is non-optional.

## Slices (each spec/plan/build, pure-public-crate unless noted)

### P2a — Label-gap + corpus hygiene + minimal deterministic VCR (pure-public-crate)
Close the RUNG 3 join closure, clean the capture sink so future gold labels are honest, AND ship a minimal Verifiable Compression Receipt on the P1a-d deterministic path (per the owner's 2026-07-06 resolution: proof-of-savings surfaces early, de-risked from the model timeline). The model is not needed for a deterministic receipt.
- **Minimal deterministic VCR:** a signed `{savings, token_delta, route, trace_id, ts}` receipt for every compression, using the EXISTING deterministic P1a-d backends (no model). The learned model (P2.5) later STRENGTHENS the receipt by adding the recall verdict + the signed Quality×Savings frontier.
- **Default-on expandable routes:** turn `content_compress` on by default on a subset of routes (the default expandable down-route catalog) so the P1d capture corpus accumulates organically. ZDR posture shifts from opt-in to default-on for these routes — surface in operator docs.
- Add `content_compress`-only same-model requests to the judge eligibility predicate (today = route-downgrade/shaped only).
- Add the `request_logs.trace_id` partial index + the cast direction (migration 0014's flagged task). Confirm the RUNG 3 judge-cost exclusion against migration 0014 lines 32-34.
- **RUNG 3 judge cap:** per-org/per-day cap on judge re-dispatches; judge cost excluded from org savings/monthly_cap; over-cap traffic is still compressed but not judged.
- Raise the judge sample rate on opted-in capture traffic (within the per-org cap).
- Corpus post-filter: re-run each captured pair through `PassPipeline::run` with the same gate config; drop pipeline-rejected rewrites.
- Export CLI drops Low-confidence rows + restricts billed-metric token-delta ground-truth to OpenAI-High rows.

### P2b — Shadow scoring service + RUNG 1/2 model (owner-infra crate + off-repo training + pure-public-crate gateway wiring)
Stand up the off-process scoring path and train the warm-start + distilled model; wire the gateway's DARK SHADOW path so the model contributes ZERO user-visible behavior change.
- New `tt-ml-scoring-service` crate/binary on the M4 Max: `ort` 2.0.0-rc.12 (`load-dynamic` + `copy-dylibs`), CoreML/Metal EP primary + CPU fallback, mBERT-base-cased ~110M FP16/INT8 ORT-format ≤~250MB, lazy-load + 3-warmup + bounded concurrency + hard-timeout-with-cached-fallback + `ManuallyDrop` to dodge the macOS-exit SIGABRT.
- Train RUNG 1 (lossless structural pretrain, recall=1.0 provable) + RUNG 2 (LLMLingua-2 REAL recipe — teacher essence from uncompressed `before`) restricted to OpenAI-High pairs (off-repo training script, PyTorch+MPS).
- Gateway: thin bounded HTTP client + content-hash LRU cache + async background cache-populate task + the `prose-learned` class wired DARK in SHADOW (score from cache, compare to deterministic P1b, SHIP deterministic P1b, log the delta for offline recall eval) + the `model_warm` health probe.
- New `content_compress/learned_prose.rs` sibling exporting the drop-in `compress` contract; the `compact_block` Prose branch gains the learned-path branch (gated + cache-conditional, DARK in shadow).

### P2c — RUNG 3 gold accumulate + INT8/FP16 cert (pure-public-crate eval + off-repo eval)
Build the held-out gold set from accumulating judge verdicts on the now-eligible compression-only traffic; re-certify the deployed numerics through the judge.
- Keep-ratio sweep per judged input to find the knee (max-safe compression ratio per recall) — each ratio a separate ~2%-sample event (judge-tax-heavy, slow).
- Re-run INT8/FP16-on-Metal outputs through the quality judge BEFORE the ratchet trusts `prose-learned`.
- Build a held-out gold set from judged verdicts; gated SHADOW comparison: measure the model-inclusive compression's recall vs the deterministic P1b baseline.

### P2d — Promote to live (gated) (pure-public-crate)
Flip `prose-learned` from SHADOW (compute, compare, ship P1b) to COMMIT (compute, ship learned) — gated behind operator allowlist + the 0.90-floor ratchet + token-true gate + auto-pause-on-timeout-rate.
- Flip the dispatcher branch from shadow-compare to commit-when-gate-trusted.
- Operator opens `prose-learned` via `TT_SUMMARIZE_TRUSTED_CLASSES` per-route.
- Sustained sub-0.90 recall OR sustained cache-populate-failure/timeout-rate > threshold auto-pauses `prose-learned` (independent ratchet — deterministic P1b on `prose` keeps serving).
- Per-instance metrics: `model_warm`, `timeout_counter`, `cache-hit-rate`, recall-window for `prose-learned`.

### P2.5 (follow-on, NOT a Phase-2 deliverable) — Verifiable Compression Receipt
The VCR: every compression → a signed `{savings, recall_verdict, model_id, trace_id}` receipt; TT's signed public Quality×Savings frontier competitors can reproduce but can't sign. The learned model with judge-governed recall makes the Receipt's frontier far stronger (a deterministic-only receipt can ship independently, but a learned model with real recall verdicts is the stronger signed surface). Follows P2d's promotion; does not gate it.

## Phase-3 boundary (DEFERRED — so Phase 2 stays scoped)
- The dense-latent "AI language" codec (gist-token / ICAE / learned-embedding outputs) — third-party model APIs accept text/tokens, NOT learned embeddings, so the embedding-output codec only deploys on TT-hosted/open models where TT controls both ends.
- Phase 2's model STAYS TEXTUAL: per-token keep/drop over the input's own tokens → output is a strict subset of input text → every third-party model can ingest it.
- Also DEFERRED: `code-learned` (the code backend's re-parse-verify makes per-segment keep/drop harder; the cleaner code slot is a segment score for which functions to truncate-deeper, replacing `BODY_MIN_LINES`); a Diff backend (no Phase-1 backend); any autoregressive rewriter or model that emits text it did not see; model-aware conditioning (would require widening the `compress` signature or moving into a different seam).

## Open questions for the owner (RESOLVED 2026-07-06)
1. **Where does the scoring service physically live? → Home/office M4 Max** (simplest; the machine is already there). Fail-open to deterministic P1b means a home-internet outage = zero model value (not broken requests) until it's back; the model is best-effort, the deterministic floor carries availability. **Action:** measure the real network RTT to the Fly gateway's region before P2b ships (drives the fail-open timeout).
2. **The teacher LLM for RUNG 2 → TT routing to the best model (dogfood).** The essence-extraction prompt is a TT route pinned to a specific model+config for reproducibility. The teacher pipeline is TT dogfooding its own routing value; it stays on owner infra; reproducibility = the pinning config. **Action:** record the teacher's `model_id` + pinning config in the training-run manifest (the reproducibility key).
3. **RUNG 3 judge tax → per-org/per-day cap; judge cost EXCLUDED from org savings/monthly_cap.** Predictable cost, protects the customer's bill. **Action:** confirm the exclusion against migration 0014 lines 32-34 when writing the P2a plan.
4. **Cold-start dataset seeding → default-on expandable routes.** Turn `content_compress` on by default on a subset of routes (the default expandable down-route catalog) so the P1d capture corpus accumulates ~10k prose-compress req/day organically. Feeds RUNG 1/2 teacher-distilled labels abundantly; RUNG 3 gold is throttled by the judge cap (fine — RUNG 3 is the later certification gate, not the ship gate). **Action:** ZDR posture shifts from opt-in to default-on for these routes — surface in the P2a plan + the operator docs.
5. **VCR timing → ship a minimal deterministic VCR in P2a.** A signed `{savings, token_delta, route, trace_id, ts}` receipt on the P1a-d deterministic path, WITHOUT the model (de-risks the proof-of-savings product surface independently of the model timeline). The learned model (P2.5) later STRENGTHENS the receipt by adding the recall verdict + the signed Quality×Savings frontier. **Action:** add a VCR slice to P2a (the minimal deterministic receipt) — P2a is no longer "label-gap + corpus hygiene only."
6. **Class isolation → separate `prose-learned` class.** A new class on the shared `RatchetSummaryGate` with an independent ratchet + independent operator allowlist; a bad learned model darkens ONLY the learned path while deterministic P1b on `prose` keeps serving. The extra wiring is worth the blast-radius isolation.

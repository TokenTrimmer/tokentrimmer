export const meta = {
  name: 'phase2b-scoring-service-design',
  description: 'Design P2b (shadow scoring service + RUNG 1/2 model + gateway DARK SHADOW wiring) — ground, design panel, adversarial verify, synthesize a spec-ready design',
  phases: [
    { title: 'Ground', detail: 'parallel grounders: gateway shadow-wiring+cache, ort-on-M4Max serving, RUNG1/2 training+TT-route teacher' },
    { title: 'Design', detail: '3 independent approaches: cache-hybrid-off-hot-path, inline-gated-with-ratchet, moat-first-VCR-tied' },
    { title: 'Verify', detail: 'adversarial skeptics refute each approach (latency/feasibility/dataset/leak)' },
    { title: 'Synthesize', detail: 'spec-ready design + phased plan + open forks' },
  ],
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase 2b design workflow for TokenTrimmer's learned-compression program.
//
// Context (already shipped — the grounding agents verify against live code):
// - Phase 1 (P1a-d) COMPLETE on public/main (16dda5cd): deterministic content-aware
//   compressor (JSON/CSV/log structural, prose extractive P1b @ prose.rs, AST code P1c)
//   wrapped in the moat (token-true gate, SummaryGate+0.90-floor RatchetSummaryGate,
//   isolated content_compress_saved_est_usd) + the judge-labeled flywheel (capture::record_pair
//   → JSONL → `tt export compress-corpus` → versioned TrainingCorpus schema v2).
// - P2a COMPLETE on public (16dda5cd) + cloud (9c0550a): the VCR (tt_telemetry::vcr,
//   vcr:v1| domain-separated Ed25519) + the label-gap closure (content_compress-only
//   requests judge-eligible via maybe_spawn_quality_judge's OR-chain; trace_id partial
//   index migration 0034) + per-org/day judge cap + corpus post-filter (PassPipeline::run
//   replay dropping rejected `after`) + Confidence filter (High-only billed-metric) +
//   default-on content_compress for the catalog + idempotent catalog enable.
// - Owner constraints (binding, 2026-07-06): training on the M4 Max 128GB via
//   PyTorch+MPS (NOT PyTorch+CUDA/GPU-rental); model PROPRIETARY (never distributed);
//   serving = SELF-HOSTED on the M4 Max (Metal EP, weights never leave owner infra,
//   the Fly cloud gateway does NOT run the model); optimize for the BEST LONG-TERM;
//   the teacher = TT routing to the best model (dogfood; teacher = a pinned TT route;
//   reproducibility = the model_id + pinning config in the training manifest);
//   cold-start = default-on expandable routes; ship gate = RUNG 2 ships DARK shadow,
//   RUNG 3 gates promotion; class = separate `prose-learned`.
//
// P2b's job (from the Phase-2 spec, §Slices P2b):
// - New `tt-ml-scoring-service` crate/binary on the M4 Max: ort 2.0.0-rc.12
//   (load-dynamic + copy-dylibs), CoreML/Metal EP primary + CPU fallback, mBERT-base-cased
//   ~110M FP16/INT8 ORT-format ≤~250MB, lazy-load + 3-warmup + bounded concurrency +
//   hard-timeout-with-cached-fallback + ManuallyDrop to dodge the macOS-exit SIGABRT.
// - Train RUNG 1 (lossless structural pretrain, recall=1.0 provable) + RUNG 2 (LLMLingua-2
//   REAL recipe — teacher essence from uncompressed `before`) restricted to OpenAI-High
//   pairs (off-repo training script, PyTorch+MPS).
// - Gateway: thin bounded HTTP client + content-hash LRU cache + async background
//   cache-populate task + the `prose-learned` class wired DARK in SHADOW (score from
//   cache, compare to deterministic P1b, SHIP deterministic P1b, log the delta for offline
//   recall eval) + the model_warm health probe.
// - New `content_compress/learned_prose.rs` sibling exporting the drop-in compress
//   contract; the compact_block Prose branch gains the learned-path branch (gated +
//   cache-conditional, DARK in shadow).
//
// Output: a spec-ready design + a phased implementation plan + open forks for the owner,
// handed back to the main loop to write to docs/superpowers/specs/.
// ─────────────────────────────────────────────────────────────────────────────

const GROUNDING_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['summary', 'key_facts', 'gotchas', 'implications_for_p2b'],
  properties: {
    summary: { type: 'string', description: '3-5 sentence grounding summary' },
    key_facts: {
      type: 'array',
      description: 'Concrete verified facts (with file:line where relevant) the design must respect',
      items: { type: 'string' },
    },
    gotchas: {
      type: 'array',
      description: 'Landmines / constraints / non-obvious behavior the design must account for',
      items: { type: 'string' },
    },
    implications_for_p2b: {
      type: 'array',
      description: 'What this grounding means for the P2b design (the model, the serving path, the cache, the shadow wiring)',
      items: { type: 'string' },
    },
  },
}

const DESIGN_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['approach_name', 'thesis', 'serving_topology', 'cache_design', 'training_pipeline', 'shadow_wiring_seam', 'latency_budget', 'offline_eval_path', 'risks', 'defers_to_p2c'],
  properties: {
    approach_name: { type: 'string' },
    thesis: { type: 'string', description: 'The core claim — why this design wins for an inline gateway with the owner-infra constraint' },
    serving_topology: { type: 'string', description: 'The tt-ml-scoring-service shape: ort session-cache, Metal EP + CPU fallback, warmup, hard-timeout-with-cached-fallback, ManuallyDrop, bounded concurrency' },
    cache_design: { type: 'string', description: 'The content-hash LRU: what is cached (per-segment keep-density?), the key (hash of what?), the populate path (async background task), the eviction, the fail-open-on-miss' },
    training_pipeline: { type: 'string', description: 'RUNG 1 (lossless structural) + RUNG 2 (LLMLingua-2 teacher-essence recipe, TT-route teacher, OpenAI-High only): the off-repo script shape, the data flow, the ONNX export, the manifest' },
    shadow_wiring_seam: { type: 'string', description: 'The learned_prose.rs drop-in + the compact_block Prose branch + the prose-learned class + the DARK SHADOW path (score, compare to P1b, ship P1b, log delta)' },
    latency_budget: { type: 'string', description: 'Live request cost (microsecond hash lookup) + background populate cost (network RTT + Metal inference); the fail-open timeout; the model_warm health probe' },
    offline_eval_path: { type: 'string', description: 'How SHADOW delta is captured + evaluated offline (the RUNG 3 precursor): the log shape, the held-out set, the recall-vs-deterministic comparison' },
    risks: { type: 'array', items: { type: 'string' }, description: 'The 3-5 most serious risks of THIS approach (the skeptics will target them)' },
    defers_to_p2c: { type: 'string', description: 'Where this approach draws the P2b/P2c boundary (RUNG 3 gold cert + INT8/FP16 cert + promote-to-live are P2c/P2d)' },
  },
}

const VERDICT_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['approach', 'refuted', 'strongest_refutation', 'survives_claim', 'fatal_flaws', 'fixable_flaws'],
  properties: {
    approach: { type: 'string' },
    refuted: { type: 'boolean', description: 'true if the approach should NOT survive (a fatal flaw)' },
    strongest_refutation: { type: 'string', description: 'The single most damaging objection' },
    survives_claim: { type: 'string', description: 'What the approach gets RIGHT even if flawed' },
    fatal_flaws: { type: 'array', items: { type: 'string' }, description: 'Flaws that kill the approach as-stated (empty if none)' },
    fixable_flaws: { type: 'array', items: { type: 'string' }, description: 'Flaws that can be designed around (empty if none)' },
  },
}

const SYNTHESIS_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['recommended_design', 'rationale', 'serving_topology', 'cache_design', 'training_pipeline', 'shadow_wiring_seam', 'phased_plan', 'p2c_boundary', 'open_questions_for_owner', 'spec_outline'],
  properties: {
    recommended_design: { type: 'string', description: 'The synthesized design — 2-4 sentences naming the approach + headline' },
    rationale: { type: 'string', description: 'Why this over the alternatives; what was grafted from the runners-up' },
    serving_topology: { type: 'string', description: 'The concrete tt-ml-scoring-service shape: ort version + EPs + session-cache + warmup + timeout + bounded concurrency + the macOS-exit SIGABRT dodge' },
    cache_design: { type: 'string', description: 'The content-hash LRU concrete shape: key, value (per-segment keep-density), populate, evict, fail-open' },
    training_pipeline: { type: 'string', description: 'RUNG 1 + RUNG 2 concrete: the off-repo script, the TT-route teacher, OpenAI-High restriction, ONNX export, the reproducibility manifest' },
    shadow_wiring_seam: { type: 'string', description: 'The learned_prose.rs drop-in + compact_block Prose branch + prose-learned class + DARK SHADOW path + model_warm probe' },
    phased_plan: {
      type: 'array',
      description: 'Ordered implementation slices (P2b-1, P2b-2, ...), each spec/plan/build',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['slice', 'goal', 'ships', 'verifies'],
        properties: {
          slice: { type: 'string' },
          goal: { type: 'string' },
          ships: { type: 'string' },
          verifies: { type: 'string' },
        },
      },
    },
    p2c_boundary: { type: 'string', description: 'What is explicitly DEFERRED to P2c (RUNG 3 gold cert + INT8/FP16 judge cert + held-out recall eval) so P2b stays scoped' },
    open_questions_for_owner: {
      type: 'array',
      description: 'Genuine forks the owner must decide before spec (e.g., the ort version pin, the model id, the cache size, the shadow-log shape)',
      items: { type: 'string' },
    },
    spec_outline: { type: 'string', description: 'The section headings of the spec doc to write next' },
  },
}

// ─── Phase 1: Ground (parallel, 3 grounders) ───────────────────────────────
phase('Ground')

const groundingPrompts = [
  {
    label: 'ground:gateway-shadow-seam+cache',
    prompt: `You are grounding the design of TokenTrimmer's Phase-2b DARK SHADOW wiring + content-hash cache against the LIVE shipped code in /Users/iansimon/Developer/TokenTrimmer/public (on branch main, commit 16dda5cd — Phase 1 P1a-d + P2a merged).

P2b adds a NEW sibling crates/core/src/content_compress/learned_prose.rs exporting the drop-in \`fn(&str) -> Option<(String, usize)>\` contract the dispatcher already calls; the Prose arm of compact_block (structural.rs) gains a learned-path branch; the model runs OFF the hot path (an async-populated content-hash LRU cache; live request = microsecond hash lookup; cache miss → deterministic P1b inline + async-backfill). It ships DARK in SHADOW (score from cache, compare to deterministic P1b, SHIP deterministic P1b, log the delta for offline RUNG 3 recall eval).

Read + verify:
- crates/core/src/content_compress/structural.rs — compact_block (the Prose branch + prose_gate + PROSE_CLASS), compact_in_place, the ContentCompressPass struct + with_gates/with_gates_and_capture, apply (where the pass self-measures the delta). The learned-path branch slots in alongside the prose_gate check.
- crates/core/src/content_compress/prose.rs — what the deterministic P1b extractive backend does (prose::compress, segments, is_must_keep, content_tokens, the target_keep greedy). The learned model REUSES these + only replaces the keep-DENSITY scoring.
- crates/core/src/passes/mod.rs — PassPipeline::content_compress_with_gates_and_capture (the pipeline boundary the shadow path must ride), PipelineOutcome (tokens_removed + rejected).
- crates/core/src/passes/agentic_budget/summarize_judge.rs — SummaryGate + is_committable + the new 'prose-learned' class (separate ratchet; mirrors PROSE_CLASS). How the operator opens a class (TT_SUMMARIZE_TRUSTED_CLASSES).
- crates/core/src/state.rs — AppState (where a model-warm health probe + the cache + the HTTP client to the scoring service would live).
- crates/core/src/routes/chat.rs:3231+ — where content_compress runs in prepare() (the CaptureCtx + the source of truth for the model_warm check).

Produce a grounding doc: the EXACT seam where the learned path slots in (the compact_block Prose branch), the EXACT gate key + class name + ratchet shape, what AppState needs (the cache + HTTP client + model_warm), the fail-open contract (cache miss / service down / class shut → deterministic P1b, NEVER verbatim-unless-P1b-also-None), + the 3-5 hardest constraints on the shadow wiring. Cite file:line + be adversarial about what the shipped code actually does (do not trust doc-comments).`,
  },
  {
    label: 'ground:ort-on-m4max-serving',
    prompt: `You are grounding the serving path for a NEW \`tt-ml-scoring-service\` crate/binary that runs on the owner's M4 Max (128GB unified memory, Metal EP) — the Phase-2b learned compression model's off-process scorer.

Investigate:
- /Users/iansimon/Developer/NeuroGlyph/backend/src/onnx.rs — a production-hardened ort-in-Rust serving path. Read it FULLY: the ort 1.16 API (or whatever version), the session-cache, the warmup (warmup_iterations=3 in hardware_validation.rs), the hard-timeout-with-cached-fallback, the ManuallyDrop<Arc<_>> to dodge the macOS-exit SIGABRT, the load-dynamic + copy-dylibs pattern. This is the precedent to PORT (the spec says write fresh against ort 2.x — ort 2 removed the global Environment + moved to ndarray 0.17).
- /Users/iansimon/Developer/NeuroGlyph/backend/src/model_hot_swap.rs + model_quantization.rs — the hot-swap + INT8 quantization patterns (P2b needs FP16-on-Metal primary + INT8-CPU fallback).
- The \`ort\` Rust crate at 2.0.0-rc.12: search the web for the EXACT current API (Session builder, the ExecutionProvider trait, CoreML/Metal EP vs CPU EP, load-dynamic, ndarray interop, the ManuallyDrop-on-Environment fix). Confirm the macOS-exit SIGABRT is real + how ort 2.x dodges it.
- The realistic latency budget for a ~110M mBERT inference on an M4 Max Metal EP (per-segment, max_seq_len=512). No published CPU/Metal numbers for LLMLingua-2 exist — estimate from the model size + the EP.
- The binary-size/build implications for a NEW crate that IS the only \`ort\` consumer in the workspace (the public gateway stays ML-dep-free; the scoring service lives ONLY on owner infra disk, ≤~250MB model).

Produce a grounding doc: the ort 2.x integration shape (port NeuroGlyph's patterns, write fresh against the new API), the realistic latency budget, the binary/build cost, the CoreML/Metal EP + CPU fallback topology, the macOS-exit SIGABRT dodge, + the fail-open-on-slow posture. Be concrete + adversarial about the latency — do NOT hand-wave.`,
  },
  {
    label: 'ground:rung12-training+teacher',
    prompt: `You are grounding the OFF-REPO training pipeline for TokenTrimmer's Phase-2b learned compression model on the owner's M4 Max (128GB, PyTorch+MPS).

The model: mBERT-base-cased ~110M (12L/768H), bidirectional encoder + per-token keep/drop binary head (NOT autoregressive — output ⊆ input → faithful-by-construction → signable → textual). Two training rungs:
- RUNG 1 (warm-start, recall=1.0 PROVABLE, no judge): lossless structural pairs (JSON whitespace-minify, CSV trailing-ws strip, log collapse) from the P1d TrainingCorpus. HONESTLY near-zero-information for the prose keep/drop task (just primes the encoder).
- RUNG 2 (teacher distillation, the LLMLingua-2 REAL recipe): the teacher LLM extracts an essence from the UNCOMPRESSED \`before\` + labels original tokens by overlap (NOT targeting P1b's 0.60 \`after\` — that's circular = the headroom trap). The teacher = a TT ROUTE to the best model (dogfood; reproducibility = the model_id + pinning config in the training manifest). Restricted to OpenAI-High pairs (so the per-pair tokens_removed the teacher optimizes is the billed-reconcilable metric).

Investigate:
- crates/cli/src/compress_corpus.rs — the TrainingCorpus schema v2 (CorpusPair: {kind, before, after, tokens_before, tokens_after, tokens_removed, gate_committed, confidence, billed_metric_tokens_removed, org_id, trace_id, model, provider_id, ts}). The training reads THESE fields. Note confidence="high" rows are the RUNG 2 billed-metric ground-truth.
- crates/core/src/content_compress/prose.rs — prose::segments(), prose::is_must_keep(), prose::content_tokens() (the deterministic reassembly the model's keep-density feeds into; the model REUSES these).
- Search the web for the LLMLingua-2 REAL recipe (the Microsoft paper / the llmlingua repo): the teacher-essence extraction + the per-token overlap labeling. How its training data + label strategy differ from a P1b-imitation distillation.
- The PyTorch+MPS reality: does mBERT fine-tune on MPS? The LLMLingua-2 recipe is a custom training loop (no HF Trainer) — the teacher call is a TT route (a network call per training pair). The teacher becomes the model's compression-policy prior.
- The ONNX export path: PyTorch → ONNX → ORT format; FP16 for Metal/CoreML EP + INT8 dynamic quantization for the CPU fallback.

Produce a grounding doc: the RUNG 1 + RUNG 2 concrete shape, the teacher-as-TT-route pipeline (the reproducibility manifest), the OpenAI-High restriction, the ONNX export + FP16/INT8 paths, the cold-start bootstrapping (RUNG 1 needs no labels; RUNG 2 needs the teacher), + the 3-5 hardest constraints. Be adversarial about whether the teacher-as-TT-route is reproducible + whether the dataset is big enough.`,
  },
]

const grounding = await parallel(
  groundingPrompts.map((g) => () =>
    agent(g.prompt, { label: g.label, phase: 'Ground', schema: GROUNDING_SCHEMA })
  )
)

const grounded = grounding.filter(Boolean)
log(`Grounding: ${grounded.length}/3 grounders returned`)

// ─── Phase 2: Design panel (parallel, 3 independent approaches) ────────────
phase('Design')

const groundingContext = grounded
  .map((g, i) => `### Grounding ${i + 1} (${g.summary})\nKey facts: ${g.key_facts.join('; ')}\nGotchas: ${g.gotchas.join('; ')}\nImplications: ${g.implications_for_p2b.join('; ')}`)
  .join('\n\n')

const designPrompts = [
  {
    label: 'design:cache-hybrid-off-hot-path',
    approach: 'cache-hybrid-off-hot-path (the spec-default: async-populated content-hash LRU; live request = microsecond hash lookup; cache miss → deterministic P1b + async-backfill)',
    prompt: `You are a design agent proposing ONE specific Phase-2b design for TokenTrimmer's learned compression scoring service + gateway DARK SHADOW wiring.

PHASE-1/P2a CONTEXT (shipped, verified): a deterministic content-aware compressor wrapped in TT's moat + a judge-labeled flywheel + the VCR + label-gap closure (Phase 1 P1a-d + P2a on public 16dda5cd + cloud 9c0550a).

OWNER CONSTRAINTS (binding): training on the M4 Max (PyTorch+MPS); model PROPRIETARY; serving = SELF-HOSTED on the M4 Max (Metal EP, weights never leave owner infra); optimize for best long-term; teacher = a TT route; ship gate = RUNG 2 ships DARK shadow, RUNG 3 gates promotion; class = separate \`prose-learned\`.

GROUNDING (from the live code + NeuroGlyph ort precedent + LLMLingua-2 recipe):
${groundingContext}

YOUR APPROACH (the SPEC-DEFAULT — the model runs OFF the hot path via a content-hash LRU cache): a background async task per opted-in route pre-populates an LRU content-hash → per-segment-keep-density cache by sending block strings to the owner-infra scoring service over HTTP; the live compact_block request does a microsecond hash lookup (cache hit → use learned scores in the deterministic reassembly; cache miss → deterministic P1b recency+salience inline + async-enqueue a populate for next time). The model is therefore an AMORTIZED background cost, NEVER a per-request tax. Ships DARK in SHADOW (score, compare to P1b, ship P1b, log delta). Fail-open is genuine latency-fail-open (the skeptics refuted the inline-on-hot-path alternative as a latency tax). Frame why this is the ONLY posture that survives the headroom lesson (model can never block a request or kill the gateway SLO) AND the owner-infra network-RTT reality.

Fill the schema. Be concrete about: the ort 2.x serving topology (port NeuroGlyph), the cache key (hash of what — the raw block text? the block + the model id?), the cache value (per-segment keep-density — what shape?), the populate path (async task, the bounded HTTP client), the eviction (LRU size?), the fail-open-on-miss, the DARK SHADOW log shape (the offline RUNG 3 eval input), the training pipeline (RUNG 1/2 + TT-route teacher), + the 3-5 most serious risks. Draw the P2b/P2c boundary explicitly (RUNG 3 gold cert + promote-to-live are P2c/P2d).`,
  },
  {
    label: 'design:inline-gated-with-ratchet',
    approach: 'inline-gated-with-ratchet (the model serves IN-LINE on the hot path behind the prose-learned ratchet; no cache; the 0.90-floor + auto-pause-on-timeout-rate are the safety)',
    prompt: `You are a design agent proposing ONE specific Phase-2b design for TokenTrimmer's learned compression scoring service + gateway wiring — the COUNTER-THESIS to the cache-hybrid default.

PHASE-1/P2a CONTEXT (shipped, verified): a deterministic content-aware compressor wrapped in TT's moat + a judge-labeled flywheel + the VCR + label-gap closure.

OWNER CONSTRAINTS (binding): training on the M4 Max (PyTorch+MPS); model PROPRIETARY; serving = SELF-HOSTED on the M4 Max (Metal EP); optimize for best long-term; teacher = a TT route; ship gate = RUNG 2 ships DARK shadow, RUNG 3 gates promotion; class = separate \`prose-learned\`.

GROUNDING (from the live code + NeuroGlyph ort precedent + LLMLingua-2 recipe):
${groundingContext}

YOUR APPROACH (the model serves IN-LINE on the hot path): NO cache — a request whose route opts into \`prose-learned\` + whose class is ratchet-open calls the scoring service synchronously (within a HARD TIMEOUT), with the 0.90-floor ratchet + an auto-pause-on-timeout-rate as the safety. Frame why this is SIMPLER (no cache-coherence, no populate task, no stale-entry problem) + gives a BETTER compression ratio (every request gets a real score, not a cached one) — at the cost of per-request latency. Explicitly argue why the owner-infra Metal EP can hit a sub-30ms p50 SLO (or honestly concede it can't + that this approach is refuted on that — the skeptics will target the latency). If the Metal EP + the hard-timeout + the ratchet genuinely keep the gateway's SLO, this is the higher-ceiling design.

Fill the schema. Be concrete + adversarial about the latency budget (per-request Metal inference + network RTT to owner infra + the hard-timeout + the fail-open-on-timeout posture). Name the 3-5 most serious risks (the skeptics will target them). Draw the P2b/P2c boundary explicitly.`,
  },
  {
    label: 'design:moat-first-vcr-tied',
    approach: 'moat-first-VCR-tied (the model + the VCR are tightly coupled: the learned score IS the receipt evidence behind the signed Quality×Savings frontier; the moat drives the design, not the ratio)',
    prompt: `You are a design agent proposing ONE specific Phase-2b design for TokenTrimmer's learned compression scoring service + gateway wiring — the MOAT-MAXIMALIST thesis (the model is the VCR's evidence engine).

PHASE-1/P2a CONTEXT (shipped, verified): a deterministic content-aware compressor wrapped in TT's moat + a judge-labeled flywheel + the VCR (tt_telemetry::vcr, vcr:v1|, cloud mint endpoint) + label-gap closure. The owner said "proof-of-savings is VERY important" + "optimize for best long-term." The VCR is a first-class product surface.

OWNER CONSTRAINTS (binding): training on the M4 Max (PyTorch+MPS); model PROPRIETARY; serving = SELF-HOSTED on the M4 Max (Metal EP); teacher = a TT route; ship gate = RUNG 2 ships DARK shadow, RUNG 3 gates promotion; class = separate \`prose-learned\`.

GROUNDING (from the live code + NeuroGlyph ort precedent + LLMLingua-2 recipe):
${groundingContext}

YOUR APPROACH (the model + the VCR are tightly coupled): the learned model is deployed NOT primarily for a better ratio but as the engine that produces the SIGNED Quality×Savings frontier — every model-inclusive compression emits a VCR carrying the model's per-segment keep-decision evidence (not just the savings figure). The signed public frontier competitors can reproduce but can't sign IS the product. Frame why coupling the model to the VCR (rather than treating the VCR as a P2.5 follow-on) compounds the moat with each request + what it sacrifices (engineering coupling, the model isn't strictly necessary for a deterministic receipt). Argue strongly for the best-long-term over the short-term shipping posture.

Fill the schema. Be concrete about: the serving topology (reuse the cache-hybrid OR inline — pick + justify), the VCR-coupling shape (what model evidence the receipt carries), the training pipeline, the moat-wrap (the prose-learned class + the 0.90-floor + the token-true gate), + the 3-5 most serious risks (over-engineering, the VCR can ship without the model, the coupling creates a refactor risk). Draw the P2b/P2c boundary explicitly.`,
  },
]

const designs = await parallel(
  designPrompts.map((d) => () =>
    agent(d.prompt, { label: d.label, phase: 'Design', schema: DESIGN_SCHEMA })
  )
)

const designList = designs.filter(Boolean)
log(`Design panel: ${designList.length}/3 approaches returned`)

// ─── Phase 3: Adversarial verify (pipeline — each design verified by 2 skeptics) ──
phase('Verify')

const SKEPTIC_LENSES = [
  'latency + the inline-gateway reality (the headroom lesson — a model too slow for prod is a non-starter; what is the ACTUAL per-request cost — cache lookup OR Metal inference + network RTT — + when does the ratchet auto-pause / fail-open?)',
  'dataset + label honesty + reproducibility (is the P2d corpus big enough for RUNG 1/2? is the TT-route teacher reproducible across pinning-config changes? does the cold-start bootstrap work? does the learning leak PII from the \`before\` into the model weights — a proprietary-data exposure risk?)',
]

const verified = await pipeline(
  designList,
  (design) =>
    parallel(
      SKEPTIC_LENSES.map((lens) => () =>
        agent(
          `You are an adversarial skeptic for TokenTrimmer's Phase-2b scoring-service design "${design.approach_name}".

The design claims: ${design.thesis}

Key claims to REFUTE (default to refuted=true if uncertain):
- serving_topology + latency_budget: ${design.serving_topology} / ${design.latency_budget}
- cache_design (does it actually fail-open on miss? stale-entry risk?): ${design.cache_design}
- training_pipeline (label honesty + reproducibility): ${design.training_pipeline}
- shadow_wiring_seam (is the DARK SHADOW path actually zero-user-visible-behavior-change? does the model never block a request?): ${design.shadow_wiring_seam}
- offline_eval_path (is the SHADOW log actually usable for RUNG 3 recall eval?): ${design.offline_eval_path}

Refute via the ${lens} lens. The inline-gateway reality: the model runs on owner infra (the M4 Max, not the Fly gateway). A slow model OR a slow network RTT to owner infra is a non-starter (headroom abandoned their trained model for a deterministic fallback). The label reality: the teacher is a TT route (a network call per training pair) — is that reproducible? Does the model's training on real \`before\` text leak PII into the proprietary weights? Is the cache coherent (a stale keep-density for a changed block?) Be skeptical + specific. If a claim is a genuine fatal flaw the design cannot survive as-stated, mark refuted=true + name it in fatal_flaws. If it's fixable, name it in fixable_flaws. If the approach genuinely holds up on this lens, refuted=false.`,
          { label: `verify:${design.approach_name.slice(0, 22)}:${lens.slice(0, 18)}`, phase: 'Verify', schema: VERDICT_SCHEMA }
        )
      )
    ).then((verdicts) => ({ design, verdicts: verdicts.filter(Boolean) }))
)

const verifiedDesigns = verified.filter(Boolean)
log(`Verified: ${verifiedDesigns.length} designs × 2 skeptics`)

// ─── Phase 4: Synthesize ───────────────────────────────────────────────────
phase('Synthesize')

const verifySummary = verifiedDesigns
  .map((vd) => {
    const v = vd.verdicts
    const refutedCount = v.filter((x) => x.refuted).length
    return `### "${vd.design.approach_name}"
Thesis: ${vd.design.thesis}
Serving: ${vd.design.serving_topology}
Cache: ${vd.design.cache_design}
Training: ${vd.design.training_pipeline}
Shadow wiring: ${vd.design.shadow_wiring_seam}
Latency: ${vd.design.latency_budget}
Risks claimed: ${vd.design.risks.join('; ')}
Verdicts: ${refutedCount}/${v.length} skeptics refuted.
${v.map((x) => `- [${x.refuted ? 'REFUTED' : 'HOLDS'}] ${x.strongest_refutation}${x.fatal_flaws.length ? ` | fatal: ${x.fatal_flaws.join('; ')}` : ''}${x.fixable_flaws.length ? ` | fixable: ${x.fixable_flaws.join('; ')}` : ''}${x.survives_claim ? ` | survives: ${x.survives_claim}` : ''}`).join('\n')}`
  })
  .join('\n\n')

const synthesis = await agent(
  `You are the synthesizer for TokenTrimmer's Phase-2b scoring-service design. The owner's program is brainstorm→spec→plan→build, and Phase 1 + P2a are shipped.

You have:
1. The grounding (what the live code + NeuroGlyph ort precedent + LLMLingua-2 recipe constrain).
2. Three independent design approaches, each adversarially verified by 2 skeptics.

GROUNDING:
${groundingContext}

VERIFIED APPROACHES:
${verifySummary}

Synthesize ONE spec-ready design (not a survey — a recommendation with a rationale + what was grafted from the runners-up). The synthesis must:
- Pick the approach that best fits an INLINE gateway (slow model OR slow network RTT = non-starter, the headroom lesson) AND survives the owner-infra reality (the model on the M4 Max, NOT the Fly gateway) AND maximizes the moat (the owner said proof-of-savings is core + best-long-term).
- Honor the OWNER CONSTRAINTS binding: training on the M4 Max (PyTorch+MPS), model PROPRIETARY, serving SELF-HOSTED on the M4 Max (Metal EP, weights never leave), teacher = a TT route, ship gate = RUNG 2 ships DARK shadow + RUNG 3 gates promotion, class = separate \`prose-learned\`.
- Resolve the cache-hybrid-vs-inline fork concretely (the spec leans cache-hybrid; the inline design is a higher-ceiling counter-thesis — adjudicate via the latency verdicts).
- Name the EXACT serving topology (ort 2.x version, CoreML/Metal EP + CPU fallback, session-cache, warmup, hard-timeout-with-cached-fallback, ManuallyDrop on macOS exit, bounded concurrency).
- Name the EXACT cache design (key, value = per-segment keep-density, populate, evict, fail-open).
- Name the EXACT training pipeline (RUNG 1 + RUNG 2, the TT-route teacher, OpenAI-High restriction, ONNX export FP16/INT8, the reproducibility manifest).
- Name the EXACT shadow-wiring seam (learned_prose.rs drop-in, compact_block Prose branch, the prose-learned class + ratchet, the DARK SHADOW log shape, the model_warm health probe).
- Draw the P2c boundary explicitly (RUNG 3 gold cert + INT8/FP16 judge cert + held-out recall eval + promote-to-live are P2c/P2d — NOT P2b).
- Produce a phased plan (P2b-1, P2b-2, ... slices, each spec/plan/build), honest about cold-start bootstrapping (RUNG 1 needs no labels; RUNG 2 needs the teacher).
- Surface the genuine forks the owner must decide before spec (the ort version pin, the model id for mBERT-base-cased, the cache size, the shadow-log shape, the metrics surface).

Fill the synthesis schema. This is the artifact handed to the main loop to write the spec.`,
  { label: 'synthesize:phase2b-design', phase: 'Synthesize', schema: SYNTHESIS_SCHEMA, effort: 'xhigh' }
)

return synthesis

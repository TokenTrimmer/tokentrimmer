export const meta = {
  name: 'phase2-learned-compression-design',
  description: 'Design Phase 2 (learned selective-context compressor) for TT — ground, design panel, adversarial verify, synthesize a spec-ready design',
  phases: [
    { title: 'Ground', detail: 'parallel grounders: P1d corpus+dispatcher contract, ort-in-Rust serving path, label-gap+comparables' },
    { title: 'Design', detail: '3 independent approaches: augment-only, replace-on-Prose, moat-maximalist' },
    { title: 'Verify', detail: 'adversarial skeptics refute each approach (latency/feasibility/dataset-size)' },
    { title: 'Synthesize', detail: 'spec-ready design + phased plan + Phase-3 boundary' },
  ],
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase 2 design workflow for TokenTrimmer's content-aware compression program.
//
// Context (already shipped — the grounding agents verify against the live code):
// - Phase 1 (P1a–P1d) COMPLETE on public/main: deterministic content-aware
//   compressor (JSON/CSV/log structural, prose extractive, AST code) + the
//   judge-labeled training flywheel (capture::record_pair → JSONL →
//   `tt export compress-corpus` → versioned TrainingCorpus).
// - The moat the learned model MUST ride: the pass token-true gate (token-growing
//   transform → verbatim), the shared SummaryGate (is_committable on a class +
//   0.90-floor RatchetSummaryGate auto-pause), the isolated
//   content_compress_saved_est_usd attribution.
// - P1d's verdict honesty gap: gate_committed is the ONLY verdict attached
//   (true = the lossy gate trusted the class at commit). The richer paired
//   recall-of-baseline verdict runs against the RESPONSE (which content_compress
//   never sees) → Phase 2's training-data label problem.
// - TT has ZERO ML deps today. `ort` would be the first. NeuroGlyph (sibling
//   repo) has a proven ort-in-Rust web-decoder (visual models, wrong modality,
//   but the serving capability transfers).
// - The headroom lesson: they trained Kompress-v2-base (dual-head ModernBERT)
//   then built a deterministic Rust fallback (TextCrusher) because the model is
//   too slow for production. DON'T repeat: deterministic-first is correct; the
//   model augments, never blocks.
//
// Output: a spec-ready design + a phased implementation plan + the explicit
// Phase-3 boundary (the "AI language" dense-latent codec), handed back to the
// main loop to write to docs/superpowers/specs/.
// ─────────────────────────────────────────────────────────────────────────────

const GROUNDING_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['summary', 'key_facts', 'gotchas', 'implications_for_phase2'],
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
    implications_for_phase2: {
      type: 'array',
      description: 'What this grounding means for the Phase-2 design (model choice, serving, labels, moat-wrap)',
      items: { type: 'string' },
    },
  },
}

const DESIGN_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['approach_name', 'thesis', 'model_choice', 'training_data_strategy', 'serving_path', 'moat_wrap', 'integration_point', 'latency_budget', 'dataset_size_feasibility', 'risks', 'defers_to_phase3'],
  properties: {
    approach_name: { type: 'string' },
    thesis: { type: 'string', description: 'The core claim — why this design wins for an inline gateway' },
    model_choice: { type: 'string', description: 'Model class + size + rationale (LLMLingua-2 class? ModernBERT? how small?)' },
    training_data_strategy: { type: 'string', description: 'How to get honest recall labels from P1d corpus + the response-side judge; the label gap closure' },
    serving_path: { type: 'string', description: 'ort-in-Rust integration shape; latency budget for the hot path; cold-start handling' },
    moat_wrap: { type: 'string', description: 'How it rides the token-true gate + SummaryGate + 0.90-floor + isolated savings; fail-open posture' },
    integration_point: { type: 'string', description: 'Where exactly in the P1a-d dispatcher it slots (augment-only vs replace-on-Prose); the class key' },
    latency_budget: { type: 'string', description: 'Estimated per-request latency cost + the bound (when does it auto-pause / fail-open?)' },
    dataset_size_feasibility: { type: 'string', description: 'How many labeled pairs needed; can P1d realistically produce them; cold-start bootstrapping' },
    risks: { type: 'array', items: { type: 'string' }, description: 'The 3-5 most serious risks of THIS approach (the skeptics will target these)' },
    defers_to_phase3: { type: 'string', description: 'Where this approach draws the Phase-2/Phase-3 boundary (the dense-latent codec is Phase 3)' },
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
  required: ['recommended_design', 'rationale', 'model_choice', 'training_data_strategy', 'serving_path', 'moat_wrap', 'integration_point', 'phased_plan', 'phase3_boundary', 'open_questions_for_owner', 'spec_outline'],
  properties: {
    recommended_design: { type: 'string', description: 'The synthesized design — 2-4 sentences naming the approach + its headline' },
    rationale: { type: 'string', description: 'Why this over the alternatives; what was grafted from the runners-up' },
    model_choice: { type: 'string', description: 'Concrete model class + size + training framework + ONNX export path' },
    training_data_strategy: { type: 'string', description: 'The label-honesty closure: how P1d pairs get real recall verdicts; cold-start bootstrapping' },
    serving_path: { type: 'string', description: 'ort-in-Rust shape; latency budget; cold-start; binary-size/build implications for TT' },
    moat_wrap: { type: 'string', description: 'Exact gate key + 0.90-floor + token-true-gate + isolated attribution' },
    integration_point: { type: 'string', description: 'Where in the dispatcher (augment-only vs replace-on-Prose) + the class key naming' },
    phased_plan: {
      type: 'array',
      description: 'Ordered implementation slices (P2a, P2b, ...), each spec/plan/build',
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
    phase3_boundary: { type: 'string', description: 'What is explicitly DEFERRED to Phase 3 (the dense-latent / "AI language" codec) so Phase 2 stays scoped' },
    open_questions_for_owner: {
      type: 'array',
      description: 'Genuine forks the owner must decide before spec (e.g., host the training? buy GPU? license the model?)',
      items: { type: 'string' },
    },
    spec_outline: { type: 'string', description: 'The section headings of the spec doc to write next' },
  },
}

// ─── Phase 1: Ground (parallel, 3 grounders) ────────────────────────────────
phase('Ground')

const groundingPrompts = [
  {
    label: 'ground:p1d-corpus+dispatcher',
    prompt: `You are grounding the design of TokenTrimmer's Phase-2 learned compression model against the LIVE shipped code in /Users/iansimon/Developer/TokenTrimmer/public (on branch main, commit 633ad24 — Phase 1 P1a-d merged).

Read and verify:
- crates/cli/src/compress_corpus.rs — the TrainingCorpus + CorpusPair shape (the training data the model trains on). Note EXACTLY which fields the corpus carries per pair.
- crates/core/src/content_compress/capture.rs — CaptureRecord (the per-block record the gateway writes).
- crates/core/src/content_compress/structural.rs — the dispatcher (compact_block) + the with_gates/with_gates_and_capture builders + how the lossy prose/code backends are GATED (is_committable on PROSE_CLASS/CODE_CLASS).
- crates/core/src/content_compress/prose.rs + code.rs — what the prose extractive (P1b) and AST code (P1c) backends do (the deterministic engine the model would augment or replace).
- crates/core/src/passes/mod.rs — the token-true gate (reject token-growing transform → verbatim) the model MUST ride.
- crates/core/src/passes/agentic_budget/summarize_judge.rs — the SummaryGate + RatchetSummaryGate 0.90-floor (the moat the model must ride).

Produce a grounding doc: what EXACTLY is the corpus shape, what is the dispatcher contract the model must fit, what is the gate/moat the model must ride, and the 3-5 hardest constraints the Phase-2 design must respect. Cite file:line. Be concrete and adversarial about what the shipped code actually does (do not trust doc-comments — verify against the code).`,
  },
  {
    label: 'ground:ort-in-rust-serving',
    prompt: `You are grounding the serving path for a learned compression model that would run INSIDE TokenTrimmer's gateway (a Rust inline proxy on the hot request path). TokenTrimmer has ZERO ML deps today (no ort/onnx/candle/burn/tch — verify in /Users/iansimon/Developer/TokenTrimmer/public/Cargo.toml + crates/*/Cargo.toml).

Investigate:
- /Users/iansimon/Developer/NeuroGlyph — a sibling repo with a proven ort-in-Rust serving path (web-decoder crate). Read its Cargo.toml + src/lazy_loader.rs + src/lib.rs to learn the ACTUAL ort integration shape, lazy-loading, model-bundling, and build implications thattransfer (the models are visual/wrong-modality but the serving capability transfers).
- The 'ort' Rust crate (current version, features, the ONNX Runtime C dependency, binary-size impact, WASM/target constraints). Search the web for the current ort crate state + the LLMLingua-2 / ModernBERT model classes (sizes, ONNX-exportability, inference latency on CPU).
- The latency budget reality: TT is an INLINE gateway (every request goes through it). headroom trained a ModernBERT model then abandoned it for a deterministic fallback because it was too slow. What latency can 'ort' realistically hit for a ~50M-100M param model on CPU per request? What's the cold-start cost?

Produce a grounding doc: the ort integration shape TT should use (lazy-load? bundle the .onnx? feature flags?), the realistic latency budget, the binary-size/build-complexity cost, and the fail-open posture when the model is slow/unavailable. Be concrete + adversarial — do NOT hand-wave the latency.`,
  },
  {
    label: 'ground:label-gap+comparables',
    prompt: `You are grounding the TRAINING-DATA LABEL problem for TokenTrimmer's Phase-2 learned compression model.

The shipped P1d flywheel (crates/cli/src/compress_corpus.rs in /Users/iansimon/Developer/TokenTrimmer/public) produces a TrainingCorpus of {kind, before, after, tokens_before, tokens_after, gate_committed, ...} pairs. But the only verdict attached is gate_committed (true = the lossy gate trusted the class at commit time) — NOT a paired recall-of-baseline verdict, because content_compress never sees the RESPONSE (the judge that produces recall verdicts runs against the response). This is the label-honesty gap.

Investigate + verify:
- How does TT's existing response-side judge work? Read crates/core/src/quality_sample.rs + the route_autopause machinery (grep route_autopause + RatchetSummaryGate). How does a recall-of-baseline verdict get produced for a compressed request, and what joins it back to the capture pair (the trace_id)?
- Is there an existing audit/attestation path that records response-side verdicts joinable by trace_id? (grep the audit + request_logs schema.)
- Search the web for the comparables: LLMLingua-2 (the selective-context model class — its training data + label strategy + size), headroom's Kompress-v2-base (the lesson — trained a model too slow for prod, fell back to deterministic TextCrusher), and any "compressor trained on judge-labeled traffic" precedent.
- The cold-start problem: a brand-new model needs labels before it can serve. How does Phase 2 bootstrap (the model can't label its own training data until it exists)?

Produce a grounding doc: the exact label-honesty closure (how a P1d capture pair gets a real recall verdict, the join path), the comparable model classes + their data needs, and the 3-5 hardest constraints on the training-data strategy. Be adversarial about whether the labels are honest enough to train on.`,
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
  .map((g, i) => `### Grounding ${i + 1} (${g.summary})\nKey facts: ${g.key_facts.join('; ')}\nGotchas: ${g.gotchas.join('; ')}\nImplications: ${g.implications_for_phase2.join('; ')}`)
  .join('\n\n')

const designPrompts = [
  {
    label: 'design:augment-only',
    approach: 'augment-only (model as a scoring/scheduling layer; the deterministic P1a-d engine stays the production path)',
    prompt: `You are a design agent proposing ONE specific Phase-2 design for TokenTrimmer's learned compression model.

PHASE-1 CONTEXT (shipped, verified): a deterministic content-aware compressor (JSON/CSV/log structural, prose extractive P1b, AST code P1c) wrapped in TT's moat (token-true gate + SummaryGate + 0.90-floor + isolated savings) + a judge-labeled training flywheel (P1d: capture::record_pair → JSONL → tt export compress-corpus → versioned TrainingCorpus).

GROUNDING (from the live code + comparables):
${groundingContext}

YOUR APPROACH (the model AUGMENTS, never replaces): the learned model is a SCORING/scheduling layer over the deterministic engine — e.g., it predicts which blocks are SAFE-to-compress / which ratio to target / which backend to route — but the deterministic P1a-d backends stay the production path. The model is advisory; the deterministic engine + token-true gate + judge remain the authority. Frame why this dodges headroom's "too slow for prod" failure AND keeps the moat intact, and what it sacrifices (raw compression ratio vs a replace-the-backend model).

Fill the schema. Be concrete about model choice, the label strategy (close the gate_committed gap), the ort serving path + latency budget, the moat-wrap (exact gate key), the integration point in the dispatcher, the dataset-size feasibility + cold-start bootstrap, and the 3-5 most serious risks (skeptics will target them). Draw the Phase-3 boundary explicitly.`,
  },
  {
    label: 'design:replace-on-prose',
    approach: 'replace-on-Prose (learned model supersedes P1b extractive on prose blocks; deterministic engine handles other kinds)',
    prompt: `You are a design agent proposing ONE specific Phase-2 design for TokenTrimmer's learned compression model.

PHASE-1 CONTEXT (shipped, verified): a deterministic content-aware compressor (JSON/CSV/log structural, prose extractive P1b, AST code P1c) wrapped in TT's moat (token-true gate + SummaryGate + 0.90-floor + isolated savings) + a judge-labeled training flywheel (P1d).

GROUNDING (from the live code + comparables):
${groundingContext}

YOUR APPROACH (the model REPLACES P1b on prose): a learned selective-context model (LLMLingua-2 class) supersedes the deterministic prose extractive backend (P1b) on Prose blocks, behind the SAME SummaryGate keyed by a new "learned-prose" class (independent 0.90-floor ratchet). JSON/CSV/log/code stay on their deterministic backends (the deterministic engine + token-true gate + re-parse-verify stay the authority there). Frame why prose is the right place to deploy a learned model (the highest-value, hardest-to-do-deterministically kind), what it risks (latency, the headroom lesson, a per-kind gate), and what it gives up vs the augment-only design.

Fill the schema. Be concrete + adversarial about the latency budget, the label gap, the cold-start (P1b's deterministic verdicts bootstrap the learned model's labels?), and integration. Draw the Phase-3 boundary explicitly.`,
  },
  {
    label: 'design:moat-maximalist',
    approach: 'moat-maximalist (model = the "better-than-Kompress" proof + a Verifiable Compression Receipt; the moat IS the product)',
    prompt: `You are a design agent proposing ONE specific Phase-2 design for TokenTrimmer's learned compression model.

PHASE-1 CONTEXT (shipped, verified): a deterministic content-aware compressor wrapped in TT's moat (token-true gate + SummaryGate + 0.90-floor + isolated savings) + a judge-labeled training flywheel (P1d). TT's thesis vs headroom: don't win the compression-ratio race — absorb headroom's compression as levers inside TT's moat and compound with down-routing. The "Verifiable Compression Receipt" / signed proof-of-savings is the aim-higher move.

GROUNDING (from the live code + comparables):
${groundingContext}

YOUR APPROACH (the model + the moat IS the product): the learned model is deployed NOT primarily for a better ratio but as TT's "better-than-Kompress for an inline gateway" PROOF — fast (ort-in-Rust), judge-gated live, trained on real quality-labeled data no competitor can see, AND every compression is a signed Verifiable Compression Receipt (the savings + the verdict are attested). The signed public Quality×Savings frontier competitors can reproduce but can't sign. Frame why the moat (proof + judge + governance + down-routing) compounds the model's value beyond its raw ratio, what it sacrifices (engineering cost, the model isn't even strictly necessary for the receipt), and the risks of over-reaching.

Fill the schema. Be concrete about which slices of this are Phase-2 vs deferred (the VCR itself may be its own slice). The model choice, label strategy, serving path, moat-wrap (this is the whole point here), integration, dataset, risks. Draw the Phase-3 boundary explicitly.`,
  },
]

const designs = await parallel(
  designPrompts.map((d) => () =>
    agent(d.prompt, { label: d.label, phase: 'Design', schema: DESIGN_SCHEMA })
  )
)

const designList = designs.filter(Boolean)
log(`Design panel: ${designList.length}/3 approaches returned`)

// ─── Phase 3: Adversarial verify (pipeline — each design verified by 2 skeptics as soon as it lands) ──
phase('Verify')

const SKEPTIC_LENSES = [
  'latency + the inline-gateway reality (the headroom lesson — a model too slow for prod is a non-starter; what is the ACTUAL per-request cost + when does it fail-open?)',
  'dataset + label honesty (is the P1d corpus big enough? are the labels honest? does the cold-start bootstrap actually work or is it circular?)',
]

const verified = await pipeline(
  designList,
  // Stage 1: 2 adversarial skeptics per design (refute, not endorse)
  (design) =>
    parallel(
      SKEPTIC_LENSES.map((lens) => () =>
        agent(
          `You are an adversarial skeptic for TokenTrimmer's Phase-2 learned-compression design "${design.approach_name}".

The design claims: ${design.thesis}

Key claims to REFUTE (default to refuted=true if uncertain):
- model_choice: ${design.model_choice}
- training_data_strategy (label honesty): ${design.training_data_strategy}
- serving_path + latency_budget: ${design.serving_path} / ${design.latency_budget}
- dataset_size_feasibility + cold-start: ${design.dataset_size_feasibility}
- moat_wrap (does it actually ride the token-true gate + 0.90-floor?): ${design.moat_wrap}

Refute via the ${lens} lens. The inline-gateway reality: TT sits in the live request path — a slow model is a non-starter (headroom abandoned their trained model for a deterministic fallback). The label reality: P1d's only verdict is gate_committed (true = the gate trusted the class at commit), NOT a paired recall-of-baseline verdict (that runs against the response, which content_compress never sees). Be skeptical and specific. If a claim is a genuine fatal flaw the design cannot survive as-stated, mark refuted=true and name it in fatal_flaws. If it's fixable, name it in fixable_flaws. If the approach genuinely holds up on this lens, refuted=false (don't reflexively refute — only real flaws).`,
          { label: `verify:${design.approach_name.slice(0, 24)}:${lens.slice(0, 18)}`, phase: 'Verify', schema: VERDICT_SCHEMA }
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
Model: ${vd.design.model_choice}
Labels: ${vd.design.training_data_strategy}
Serving: ${vd.design.serving_path} (latency: ${vd.design.latency_budget})
Moat: ${vd.design.moat_wrap}
Integration: ${vd.design.integration_point}
Dataset: ${vd.design.dataset_size_feasibility}
Risks claimed: ${vd.design.risks.join('; ')}
Verdicts: ${refutedCount}/${v.length} skeptics refuted.
${v.map((x) => `- [${x.refuted ? 'REFUTED' : 'HOLDS'}] ${x.strongest_refutation}${x.fatal_flaws.length ? ` | fatal: ${x.fatal_flaws.join('; ')}` : ''}${x.fixable_flaws.length ? ` | fixable: ${x.fixable_flaws.join('; ')}` : ''}${x.survives_claim ? ` | survives: ${x.survives_claim}` : ''}`).join('\n')}`
  })
  .join('\n\n')

const synthesis = await agent(
  `You are the synthesizer for TokenTrimmer's Phase-2 learned-compression design. The owner's program is brainstorm→spec→plan→build, and Phase 1 (P1a-d) is shipped.

You have:
1. The grounding (what the live code + comparables actually constrain).
2. Three independent design approaches, each adversarially verified by 2 skeptics.

GROUNDING:
${groundingContext}

VERIFIED APPROACHES:
${verifySummary}

Synthesize ONE spec-ready design (not a survey — a recommendation with a rationale + what was grafted from the runners-up). The synthesis must:
- Pick the approach that best fits an INLINE gateway (slow model = non-starter, the headroom lesson) AND maximizes the moat (the whole TT thesis vs headroom — don't win the ratio race, win the proof+judge+down-routing compound).
- Close the label-honesty gap concretely (how a P1d pair gets a real recall verdict — the trace_id join to the response-side judge).
- Name the model class + size + training framework + ONNX export path concretely.
- Name the ort-in-Rust serving shape + the realistic latency budget + the fail-open posture (when does it auto-pause / fall back to deterministic?).
- Name the EXACT moat-wrap: the gate class key, the 0.90-floor, the token-true gate, the isolated attribution.
- Draw the Phase-3 boundary explicitly (the dense-latent "AI language" codec is Phase 3 — third-party APIs accept text/tokens not embeddings — so Phase 2 stays textual).
- Produce a phased plan (P2a, P2b, ... slices, each spec/plan/build), honest about cold-start bootstrapping.
- Surface the genuine forks the owner must decide before spec (host the training? buy GPU? license? open-source the model?).

Fill the synthesis schema. This is the artifact handed to the main loop to write the spec.`,
  { label: 'synthesize:phase2-design', phase: 'Synthesize', schema: SYNTHESIS_SCHEMA, effort: 'xhigh' }
)

return synthesis

# Flow-level end-to-end quality gate — design (BACKLOG item #5)

**Goal:** an *optional*, sample-rate-cost-bounded judge that compares a workflow run's **final synthesized answer** against a baseline, + folds the verdict into the per-flow attestation (the workflow receipt). The customer can verify, alongside the savings figure, that the workflow's output passed a quality gate.

**Why:** the workflow receipt (`wfr:v1|...|cost|baseline|saved|status`) attests *cost + savings* but not *quality*. A down-routed workflow that saves money could degrade the answer; the gate makes "it saved $X AND the answer was judged equivalent to the baseline" a single signed claim. Mirrors the agent_run per-turn-judge precedent (which judges summary recall) + reuses the existing `quality_sample` machinery (sample-rate caps, deterministic sampling, the `judge_paired` primitive).

---

## Design

### Reuse, don't rebuild
The primitives already exist in `crates/core/src/quality_sample.rs`:
- `should_sample(key, sample_rate)` — deterministic-but-uniform sampling.
- `PerOrgDayJudgeCap` — bounds per-org-day judge spend (the cap bites BEFORE sampling).
- `judge_paired(judge, input, baseline_answer, optimized_answer, order, both_orders)` — the AB-ordered paired judge that compares an optimized answer to a baseline (counters position bias). **This is exactly a final-answer-vs-baseline judge.**
- `JudgeConfig` (sample_rate, judge_model, baseline_timeout, per-org-day cap, env-driven).

The gap #5 fills: invoke `judge_paired` at workflow-run **completion** (not per-tool-call), with `optimized_answer` = the `Output` node's final content + `baseline_answer` = a reference-model re-dispatch of the trigger input; persist the verdict; fold it into the receipt attestation.

### The gate (public, `crates/core/src/workflow/quality_gate.rs`)
A new module mirroring `quality_sample`'s pattern:
- `should_quality_gate(key, config)` — combines the per-org-day cap + the sample rate (the same two-stage bound).
- `run_flow_quality_gate(state, run_id, trigger_input, final_answer, config)` → spawns a detached `judge_paired`:
  - `baseline_answer` = re-dispatch `trigger_input` to the `JudgeConfig.judge_model` (the reference/flagship) — the agent_run precedent (line 2091) re-dispatches "the ORIGINAL request to its source provider for the reference answer"; this mirrors it.
  - `optimized_answer` = the workflow's `Output` node final content.
  - AB-order via `ab_order_for(key)` + `both_orders: false` (bounded cost — one paired judge per sampled run).
  - Returns a `QualityVerdict` (a stable code: `equivalent` / `degraded` / `inconclusive` / `not_sampled`).
- The gate is **opt-in + fail-open**: if the judge errors / times out / is disabled (default), the run completes normally with `QualityVerdict::NotSampled` — never blocks the run, never fails the workflow.

### The canonical-payload bump (`wfr:v2`, cloud `workflow_receipt/crypto.rs`)
- New `canonical_payload_wfr_v2(...quality_verdict)` = `wfr:v2|org|workflow_id|run_id|cost|baseline|saved|status|quality_verdict`.
- The `quality_verdict` field is a stable string code (`equivalent` / `degraded` / `inconclusive` / `not_sampled`), part of the signed bytes (byte-stable — change the version, never the codes).
- **v1 receipts stay valid:** the ledger's `canonical_version` column (already present) records which version a frozen receipt used. New mints use v2 when a `quality_verdict` is present, v1 when `not_sampled` (backward-compat — a pre-gate run has no verdict, stays v1). The `RECEIPT_PREFIX` stays `wfr:` (the `v1`/`v2` suffix is the canonical-version discriminator).
- Domain-disjointness preserved (still `wfr:`, not `vcr:`/`l2:`/`att:`).

### Persistence (cloud migration + ledger column)
- New cloud migration `0043_workflow_run_quality_verdict.{up,down}.sql`: `ALTER TABLE workflow_runs ADD COLUMN quality_verdict TEXT` (NULL for pre-gate / not-sampled runs — additive). Plus `ALTER TABLE workflow_run_receipts ADD COLUMN quality_verdict TEXT` (so the frozen receipt carries the verdict it was minted with — the FREEZE check serves the stored verdict verbatim).
- The gate writes `quality_verdict` to `workflow_runs` on completion (best-effort, detached).
- The mint endpoint reads `quality_verdict` off `workflow_runs`; if present + not `not_sampled`, signs a `wfr:v2` receipt (persisting the verdict to the receipt ledger); else a `wfr:v1` receipt (the current behavior).

### The CLI verify path
`tt verify-receipt` already dispatches by family; within the `wfr:` family, the version discriminator (`v1` vs `v2`) is in the canonical payload, so the verify path reads the version off the payload + reconstructs the matching canonical string. (The workflow receipt verify path isn't yet in the CLI dispatch — the L2/VCR dispatch is; the wfr verify lives in the cloud verify_receipt. I extend the CLI's dispatch to cover wfr too, OR keep the cloud-side verify — TBD in build.)

---

## Implementation — 3 slices

### Slice 1 — public: the gate module (pure + fail-open, no I/O at the call site)
`crates/core/src/workflow/quality_gate.rs`:
- `QualityVerdict` enum + `quality_verdict_str()` (stable codes).
- `should_quality_gate(key, config)` (the two-stage bound: cap + sample).
- The gate fn (spawns the detached judge; fail-open).
- No canonical/payload change (pure logic); the cloud crypto bump is Slice 2.
- Wire the gate into the workflow run endpoint (`routes/workflows.rs:493`), at completion: `status = completed` + an `Output` node present → sample + spawn. Detached + best-effort.

### Slice 2 — cloud: the wfr:v2 crypto bump + migration + mint
- Migration `0043`: the two `quality_verdict` columns.
- `workflow_receipt/crypto.rs`: `canonical_payload_wfr_v2` + `sign_receipt_v2` + `verify_receipt` routes v1/v2 by the version discriminator.
- `workflow_receipt/mod.rs` mint: read `quality_verdict` off `workflow_runs`; if present, sign v2 (persist verdict to the receipt ledger); else v1.
- The FREEZE check: a v1 frozen receipt stays v1 (served verbatim); a v2 frozen receipt stays v2.

### Slice 3 — CLI verify + docs
- `tt verify-receipt`: extend the wfr-family verify path (dispatch on `wfr:` prefix, route v1/v2).
- README/GETTING_STARTED: the workflow receipt now carries an optional quality verdict; mention in the workflow-DSL doc.

## Sequencing + risk
- **Slice 1 is pure + fail-open** (gate logic + the detached spawn; a gate error never blocks a run). Merge first.
- **Slice 2 touches the workflow receipt crypto** (a v2 bump — additive, v1 stays valid via the `canonical_version` column). The hot path is the detached gate (no user latency); the mint is on-demand.
- **Slice 3** is verify-side + docs; lowest risk, last.

## Explicitly NOT in scope
- No change to the per-tool-call summary judge (the agent_run line-2091 path stays).
- No re-dispatch of intermediate model nodes (only the trigger input → reference for the baseline).
- The gate is off by default (`JudgeConfig::from_env()` with `sample_rate` 0 unless set) — opt-in.
- Per-node quality gates (judging each `model` node) is a larger build; this is the run-level final-answer gate only.

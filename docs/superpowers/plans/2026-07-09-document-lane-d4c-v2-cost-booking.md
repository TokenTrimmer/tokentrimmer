# Document Lane D4c-v2 Implementation Plan — cost booking via D0 projection

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan slice-by-slice. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Close the D4c-v2 follow-up that #306/#307 left open: when the shipped pre-routing distillation seam (`document_lane::seam`) substitutes N image/document parts for distilled text, **book the isolated `doc_vision_saved_est_usd` counterfactual saving** via D0's `document_projection::project` (raw image tokens the request WOULD have sent vs the distilled text tokens it now sends, priced at the served model's input rate, with the Gemini direction guard). Today the seam distills + downgrades but books $0 — the capability + the route downgrade are live, the savings figure is the missing piece.

**Why now:** D4c v1 (`919babf`) shipped the substrate: `DistillHarness::from_env()`, `distill_part()`, `distill_request_parts()`. The two D4c-v2 TODOs are marked in code (`crates/core/src/routes/chat.rs:3081`, `:3101`) + the seam module docs (`seam.rs:24-30,144`). Every input `document_projection::project` needs is reachable at the call site:
- **served model** = `pass_model` (`chat.rs:3071`)
- **served input price** = `pass_pricing.as_ref()` → `ModelPricing.input_per_million` (`chat.rs:3072`, struct at `crates/shared/src/pricing.rs:22`)
- **raw image tokens** ← needs decoded image dimensions (`w,h`) → `tt_tokenize::image_tokens::estimate_image_tokens` (`estimate_image_tokens.rs:74`). Header-only PNG/JPEG dimension parsing ALREADY EXISTS (private, in `crates/preview/src/token_estimator.rs:159 image_dims_from_data_url`); extract + reuse it.
- **distilled text tokens** ← `tt_tokenize::estimate_tokens_for_model` (real BPE, `tokenize/src/lib.rs:246`) on the sidecar's returned `Extraction.text`.

**Architecture:**
1. Promote the header-only image-dimension parser to a `pub` helper in `tt-tokenize` (next to `estimate_image_tokens`) so the seam — and `tt-preview`'s private copy — share one source of truth. Pure, no heavy image crate, no full decode.
2. Change `distill_request_parts`'s return from `usize` (count) to a `DistillBookkeeping { distilled_parts, raw_image_tokens, distilled_text_tokens }` so the call site has the figures. PDFs document parts + data-URL images contribute their decoded image dims (PDF → no dims → fall back to the existing nominal square, mirroring `tt-preview`'s `FALLBACK_IMAGE_DIM`).
3. At the seam call site, capture the bookkeeping; after `compute_cost_full` builds `cost_breakdown`, call `tt_preview::document_projection::project(raw, distilled, input_per_m, &pass_model)` and overwrite `cost_breakdown.doc_vision_saved_est_usd` with `projected_savings_usd` (Gemini guard + negative clamp already inside `project`). Fail-open: any missing piece (no pricing, sidecar disabled, 0 distilled) → stays 0.0.

**Constraints (verbatim + D4c-v2 additions):**
- **Isolate the vision-avoided saving** — `doc_vision_saved_est_usd` is a COUNTERFACTUAL, NEVER baseline-folded into `cost_usd`/`baseline_cost_usd`/`tt_saved_usd` (the request never sent the image → not invoice-reconcilable). Mirror `minify_saved_est_usd`/`content_compress_saved_est_usd`.
- **Gemini direction guard applies** ($0 for Gemini) — `document_projection::project` already encodes this (`formula_for_model == GeminiTiled` → 0). Do NOT re-guard at the call site.
- **Fail-open everywhere** — sidecar disabled/errored, no pricing, un-decodable dims, 0 distilled → $0, no behavior change, zero added latency for text traffic (early-return unchanged).
- **No `route_*` flag re-flip** — the downgrade is the route's `target_model` rewrite (already set by `apply_routing`); see #307's correction. The seam only swaps parts + now books the saving.
- **No new deps** — reuse `tt-tokenize` (already a `tt-core` dep), `tt-preview` (already a `tt-core` dep), the existing `base64` workspace crate. Header-only parse = no `image` crate on the hot path (the `image = "0.25"` dep stays OPTIONAL in `doc-sidecar` only).
- Public CI hard-gates `cargo fmt --all --check` + clippy; verify field-ripple with `cargo test --workspace --no-run` + `clippy --workspace --all-targets`. Do NOT bump `rand`. Commit trailer required.

---

## SLICE 1 — Promote the header-only image-dimension parser to `tt-tokenize` (one PR: `feat/d-lane-d4c2-image-dims`)

**Read first:** `crates/preview/src/token_estimator.rs:111-214` (`sum_image_tokens`, `image_dims_from_data_url`, `png_dims`, `jpeg_dims`, `FALLBACK_IMAGE_DIM`, `HEADER_B64_CHARS`), `crates/tokenize/src/image_tokens.rs` (the `estimate_image_tokens` home), `crates/tokenize/src/lib.rs:246` (`estimate_tokens_for_model`), `crates/shared/src/messages.rs:630` (`parse_data_url`).

### Task 1.1: Add `image_dims_from_bytes` (+ `png_dims`/`jpeg_dims`) to `tt-tokenize`
**Files:** `crates/tokenize/src/image_tokens.rs` (add); `crates/tokenize/Cargo.toml` (add `base64` workspace dep — check it's not already transitively available; if a `base64` workspace dep exists use it, else add `base64 = "0.22"`).
**Produces:** `pub fn image_dims_from_bytes(bytes: &[u8]) -> Option<(u32, u32)>` (from raw decoded image bytes — header-only, no full decode). Keep `png_dims`/`jpeg_dims` as private helpers inside `image_tokens.rs`. Add `pub const FALLBACK_IMAGE_DIM: u32` (the nominal square, ~768 — copy the exact value from `token_estimator.rs`). **Do NOT add a `tt-shared` dep to `tt-tokenize`** (it's a deliberately-leaf crate with only `tiktoken-rs`); hoist ONLY the raw-bytes parser — the data-URL variant stays in `tt-preview` (Task 1.2 delegates its PNG/JPEG parse to this shared helper). Needs a `base64` dep on `tt-tokenize` ONLY if the bytes-parser does its own decode — it doesn't (it takes already-decoded `&[u8]`), so **no new dep on `tt-tokenize` at all** (the base64 decode stays in `tt-preview`'s data-URL wrapper).
- [ ] TDD: PNG/JPEG header → correct dims; truncated/garbage → `None`; non-image/remote-URL → `None`. Port the existing `token_estimator.rs` tests for the parser so behavior is byte-identical. Run → fail → implement → pass → commit.

### Task 1.2: Dedup `tt-preview`'s private copy onto the shared helper
**Files:** `crates/preview/src/token_estimator.rs`.
**Produces:** Delete the private `image_dims_from_data_url`/`png_dims`/`jpeg_dims` + their consts from `tt-preview`; call `tt_tokenize::image_tokens::image_dims_from_data_url` instead. Keep `FALLBACK_IMAGE_DIM` if still referenced locally — reference the shared `tt_tokenize` const.
- [ ] The existing `tt-preview` tests (`sum_image_tokens` behavior, fallback) pass unchanged — they pin the byte-identical behavior. Run `cargo test -p tt-preview`. Commit.

---

## SLICE 2 — The seam surfaces bookkeeping; the call site books the saving (one PR: `feat/d-lane-d4c2-cost-booking`, depends on Slice 1)

**Read first:** `crates/core/src/document_lane/seam.rs` (whole file — `distill_request_parts` returns `usize`), `crates/core/src/routes/chat.rs:3071-3112` (the seam call site + `pass_model`/`pass_pricing`), `:1684-1694` + `:3388-3397` (`pass_effects`) + the `compute_cost_full` call sites in the handler, `:5018` (`CostBreakdown.doc_vision_saved_est_usd` field + its doc comment), `crates/preview/src/document_projection.rs` (`project` signature + `DocProjection`), `crates/inspect-rules-tier1/src/rules/raw_document_to_vision_model.rs:260-276` (the proven caller pattern: `estimate_image_tokens` → ratio → `project` → guard `<= 0.0`).

### Task 2.1: `DistillBookkeeping` + `distill_request_parts` return-type change
**Files:** `crates/core/src/document_lane/seam.rs`.
**Produces:**
```rust
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DistillBookkeeping {
    pub distilled_parts: usize,
    pub raw_image_tokens: u32,      // what the parts WOULD have spent (pre-distill)
    pub distilled_text_tokens: u32, // what the distilled text now spends
}
```
- [ ] In `distill_request_parts`, as each part is distilled: decode its raw image dims (`image_dims_from_bytes` on the decoded base64 for an image part; for a data-URL image, decode the data-URL to bytes then `image_dims_from_bytes`; for a PDF/audio/other document part → no image dims → 0 raw_image_tokens, since the sidecar's text-layer extraction has no pixel-token analogue). Accrue `raw_image_tokens += estimate_image_tokens(&served_model, w, h, ImageDetail::Auto)` — BUT `distill_request_parts` does NOT currently know the served model; extend its signature to take `model: &str` (the `pass_model`), threading it through. Accrue `distilled_text_tokens += estimate_tokens_for_model("", model, &extraction.text)` — verified: provider `""` hits the `_ => (o200k(), Confidence::Medium)` arm → real BPE (no chars/4 fallback unless tiktoken fails to load), a sound directional estimate.
- [ ] Early-return returns `DistillBookkeeping::default()` (the sidecar-disabled path; zero latency, zero saving — unchanged).
- [ ] **Fail-open:** a `distill_part` that returns `ExtractFailed`/`Disabled` leaves the part verbatim AND contributes 0 to bookkeeping (no raw tokens counted for a part we didn't actually replace — never over-book). A distilled part whose dims can't decode → `FALLBACK_IMAGE_DIM` square (`Auto` detail), mirroring `tt-preview`.
- [ ] TDD: a request with one distilled image part → bookkeeping has `distilled_parts=1`, `raw_image_tokens>0`, `distilled_text_tokens>0`; a disabled sidecar → all zero; a part that fails extraction → not counted. Run → fail → implement → pass → commit.

### Task 2.2: Wire the bookkeeping → `cost_breakdown.doc_vision_saved_est_usd` at the handler
**Files:** `crates/core/src/routes/chat.rs`.
**Produces:**
- [ ] Capture the seam's `DistillBookkeeping` at the call site (`let doc_bookkeeping = ...` replacing `let _distilled`). Keep the model + pricing in scope (`pass_model`, `pass_pricing`).
- [ ] After the final `compute_cost_full` builds `cost_breakdown` in the handler, **when `doc_bookkeeping.distilled_parts > 0` AND `pass_pricing` is `Some(p)`**, compute:
  ```rust
  let proj = tt_preview::document_projection::project(
      doc_bookkeeping.raw_image_tokens,
      doc_bookkeeping.distilled_text_tokens,
      p.input_per_million,
      &pass_model,
  );
  cost_breakdown.doc_vision_saved_est_usd = proj.projected_savings_usd;
  ```
  No else branch (stays 0.0). **Verify `cost_breakdown` is `let mut`** at the handler's `compute_cost_full` site; if not, make it `let mut`. Find the exact handler `compute_cost_full` call (it's downstream of `pass_effects` at `:3388` — there are non-streaming + streaming `compute_cost_full` calls per the `:3382` comment; wire BOTH paths, or extract a small `fn book_doc_vision(cost: &mut CostBreakdown, bookkeeping, model, pricing)` helper called after each).
- [ ] TDD: a handler test with a mocked sidecar returning text for one image part, `Some(pricing)` with `input_per_million=5.0`, model `gpt-4o`, raw 1024×1024 image → `doc_vision_saved_est_usd > 0` AND not folded into `cost_usd`/`tt_saved_usd` (assert `tt_saved_usd` unchanged). Gemini-targeted model → `0.0`. No pricing → `0.0`. Disabled sidecar → `0.0`. Run → fail → implement → pass → commit.

### Task 2.3: Replace the two D4c-v2 TODO comments + module docs with the shipped-behavior description
**Files:** `crates/core/src/routes/chat.rs:3081,3101`, `crates/core/src/document_lane/seam.rs:24-30,144`, `crates/core/src/document_lane/mod.rs:12-13`.
**Produces:** Remove the "v1 distills without booking the saving" / "TODO D4c-v2" wording; describe the now-live booking (model + pricing + D0 project + Gemini guard + fail-open). Keep the honest scope note for what's STILL deferred (URL-fetching of remote document/image parts — v1 distills inline base64 + data-URL bytes only).
- [ ] Commit.

---

## Slice 3 — Verify (one PR or fold into Slice 2)

- [ ] `cargo build --workspace` clean.
- [ ] `cargo fmt --all --check`.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` (the field-ripple + signature changes touch call sites; `-D warnings` catches doc-lazy-continuation on wrapped `//!` prose — see the `quality_gate` module-allow precedent if needed, but prefer fixing the indentation).
- [ ] `cargo test --workspace --no-run` (compile all test targets — `cargo build` skips them; see `[[ci-verify-all-targets]]`).
- [ ] `cargo test -p tt-tokenize -p tt-preview -p tt-core` (the touched crates' unit tests).
- [ ] Confirm the `doc_vision_saved_est_usd` header (`X-TokenTrimmer-Doc-Vision-Saved-Est-Usd`) + `request_logs` column (migration `0032`) now carry a non-zero value on a distilled request — trace the field from `compute_cost_full` → the header emit + the row insert (search `doc_vision_saved_est_usd:` at `chat.rs:1930` + `:5613`; verify both read the now-mutated `cost_breakdown`).
- [ ] Commit trailer + push + PR. Merge on green (cloud CI is minutes-blocked — public CI is free; this PR touches public only).

## Post-merge
- [ ] Update `[[project-review-2026-07-01-campaign]]` memory: D4c-v2 cost booking DONE (mark the D4c-v2 OPEN item resolved; D4c now FULLY shipped). The remaining D-lane OPEN items are D5 (V4 doc_micros attestation) + D6 (workflow Document node + hash-keyed reuse cache) + D3 SDK helpers.

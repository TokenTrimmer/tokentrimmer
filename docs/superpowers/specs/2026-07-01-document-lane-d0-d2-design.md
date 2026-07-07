# Document Lane — D0–D2 Foundation (design + implementation spec)

**Date:** 2026-07-01 · **Scope:** the zero-to-low quality-risk foundation of the Document Lane epic (§11 of `COMPREHENSIVE_REVIEW_2026-06-27.md`). **Repo:** `public/` only.

> **Parent design:** §11 of the comprehensive review is the adversarially-verified vision + competitive map + grounded gap + full D0→D6 phasing. This spec narrows to **D0, D1, D2** — the deterministic projector, the Inspect rule, and the lossless text-side compaction pass — each a standalone PR that ships value without any multimodal request-path surgery. D3–D6 (client docprep, server pre-routing Document Lane seam + OCR sidecar + lossy gate, V4 `doc_micros` attestation, workflow node) are a **separate follow-up spec** and are explicitly out of scope here.

## Why this arc first

TokenTrimmer already owns 3 of the 4 primitives to occupy the empty "reduce document/vision tokens **+** attest cost, inline, local-first" cell; the net-new primitive is the reduction transform. But before building any transform, the funnel can't even *see* the opportunity: image-heavy requests preview as **~0 tokens** today (`concat_message_text` only reads `p.get("text")`), so Inspect / the wasm playground / pre-run preview quote **$0** for a request that actually spends real vision tokens. D0 fixes that with pure, model-free estimation — **zero quality risk** — and thereby surfaces demand *before* we invest in the reduction transform. D1 turns the projection into a named Inspect opportunity. D2 ships the first *actual* reduction — but only the **lossless, text-only** kind that rides the existing token-true gate cleanly, so it carries no multimodal risk and folds into the attested headline for free.

## Goals

- **D0:** a deterministic, per-provider image/document-token projector so every preview surface prices image-heavy requests instead of counting them as ~0. No LLM, no network.
- **D1:** a static Tier-1 Inspect rule that detects raw image/PDF parts fed to a Vision model and quotes a projected `$X/mo` from D0.
- **D2:** an **opt-in**, **lossless**, text-only in-pipeline `doc_compaction` `RequestPass` that trims large text documents and folds the measured saving into the existing attested headline (like `compression`).

## Non-goals (D3–D6 — a later spec)

- No image→text substitution / vision→text route downgrade (the pre-routing seam is **D4**).
- No OCR, no lossy compaction, no `DocDistillGate`, no `auto_pause` judge floor for documents (**D4**).
- No new `ContentPart::Document`/`File` variant (**D4**) — D0 reads dims from the existing `ImageUrl` part; D2 trims the existing `Text` part.
- No `AttestationVersion::V4` / `doc_micros` slice (**D5**) — D2's lossless trim rides the existing `request_micros`/`MONTH_SAVINGS_SQL` headline and needs no new attestation term.
- No client-side `tt docprep` (**D3**).
- **No per-modality catalog column** (`image_per_million`) — see the pricing decision below.

## Verified grounding (current code, 2026-07-01)

1. `crates/tokenize/src/` has only `lib.rs` — no image-token estimator.
2. `crates/preview/src/` has `cache_projection.rs` (the pattern to mirror: a pure `project(...)` fn + a `types` struct + tests) but **no** `document_projection.rs`.
3. `concat_message_text` (`crates/preview/src/token_estimator.rs:67-79`) reads only `p.get("text")` inside the parts loop → images count as ~0 tokens.
4. `ModelPricing` (`crates/shared/src/pricing.rs:22`) has **no** image/tile/page field; Vision exists only as `Capability::Vision` (`pricing.rs:159`).
5. `ContentPart` (`crates/shared/src/messages.rs:315`) is a closed `#[serde(tag="type", snake_case)]` enum: `Text` / `ImageUrl` / `InputAudio` — no `Document`.
6. `crates/inspect-rules-tier1/src/rules/inline_data_offload_candidate.rs` exists — the sibling to model D1's rule on.
7. `crates/core/src/passes/compression.rs` + `RouteAction.compress` (`crates/routing/src/lib.rs:190`, `#[serde(default, skip_serializing_if=...)]`, off by default) — the sibling to model D2's pass + opt-in on.
8. Highest public-core migration is `0030_workflow_secrets` → **D2's migration is `0031`**.

## Key decisions (from §11, confirmed for this arc)

- **Price avoided image tokens at `input_per_million`, labelled "estimate."** Most providers bill image tokens *as* input tokens, so the error is small relative to any downstream model-downgrade win. **Defer** adding an `image_per_million` catalog column until a provider's image rate provably diverges — this keeps D0 from rippling across every provider adapter at once.
- **Provider-direction guard is load-bearing (D0).** Gemini prices a page-image flat (~258 tok), *cheaper* than distilled text, so the projector MUST report **NO saving (clamp negative → 0)** when the resolved model is Gemini. This guard ships in D0 and is reused by D1 (and later D4/D5).
- **Image-token formulas are provider- AND model-version-dependent** (independently verified vs provider docs, June 2026): Claude `⌈w/28⌉·⌈h/28⌉` (≈ w·h/750); OpenAI GPT-4o high-detail `85 + 170·tiles` (512px tiles, shortest side scaled to 768 within 2048²); OpenAI GPT-5.x/mini/o-series `⌈w/32⌉·⌈h/32⌉` patches **capped at 1536**; Gemini ≤384px → 258 flat, else `⌈w/768⌉·⌈h/768⌉·258`. **Drive the estimator from the model/catalog entry, not hardcoded per-call constants** (Claude Opus 4.7 roughly tripled image-token cost across a version bump).
- **D2 is opt-in and lossless.** A new `RouteAction.doc_compaction: bool`, off by default (off-by-default is the invariant that keeps the gateway from ever mutating traffic a route didn't request). Losslessness is enforced **structurally** by the existing token-true gate (no judge needed). The measured token delta folds into `baseline_cost_usd` like `compression` — no new attestation term.

## Slice D0 — deterministic image/document-token projector

**Files:**
- new `crates/tokenize/src/image_tokens.rs` + `pub mod image_tokens;` in `crates/tokenize/src/lib.rs`
- new `crates/preview/src/document_projection.rs` (mirror `cache_projection.rs`) + `pub mod document_projection;` in `crates/preview/src/lib.rs`
- `crates/preview/src/token_estimator.rs` (image branch in `concat_message_text`)
- `crates/preview/src/types.rs` (`DocProjection` + wire it onto `PreviewResponse`)

**Interfaces:**
- `estimate_image_tokens(provider: Provider, width: u32, height: u32, detail: ImageDetail) -> u32` — pure; per-provider match; the formula constants are keyed off the provider/model family (resolve from the catalog entry passed in, or a `Provider` enum that already carries the family). Returns token count. Deterministic, no I/O.
- `document_projection::project(raw_image_tokens: u32, distilled_text_tokens: u32, input_price_per_mtok: f64, resolved_provider: Provider) -> DocProjection` — computes `raw_image_cost`, `distilled_text_cost`, `savings = max(raw − distilled, 0)`, **with the Gemini provider-direction guard clamping savings to 0**. Mirrors `cache_projection::project`'s shape (pure fn + struct).
- `DocProjection { raw_image_tokens, raw_image_cost_usd, projected_distilled_tokens, projected_distilled_cost_usd, projected_savings_usd, basis: "estimate", note: <provider-direction note> }` on `PreviewResponse` (optional field, `#[serde(default, skip_serializing_if=Option::is_none)]`).

**`concat_message_text` image branch:** inside the existing parts loop (`token_estimator.rs:74-79`), when a part is `ImageUrl` with an inline `data:` URL, `parse_data_url` the bytes, read the image dims (use an existing image-dims reader if the workspace has one; otherwise parse the PNG/JPEG header minimally — do NOT pull a heavy image crate for full decode, only header dims), and add `estimate_image_tokens(...)` to the token total. If dims can't be read (remote URL, unknown format), fall back to a conservative default (documented) rather than 0.

**Acceptance / tests:**
- Per-provider formula unit tests: known `(w,h,detail)` → known token count, cross-checked against the provider-doc formula (Claude, OpenAI-4o tile, GPT-5.x patch-cap-1536, Gemini flat + tiled).
- Provider-direction guard: Gemini resolved → `projected_savings_usd == 0` even when raw > distilled; negative clamps to 0.
- `concat_message_text`: an image-heavy request previews **non-zero** input tokens (regression against the ~0 bug); a text-only request is unchanged.
- `DocProjection` serializes with `basis:"estimate"`; absent when no image parts.

**Verify:** `cargo fmt -p tt-tokenize -p tt-preview`, `cargo clippy -p tt-tokenize -p tt-preview --all-targets -D warnings`, `cargo test -p tt-tokenize -p tt-preview`.

## Slice D1 — Inspect 'raw-document-to-vision-model' rule (Tier-1, static)

**Files:**
- new `crates/inspect-rules-tier1/src/rules/raw_document_to_vision_model.rs` (sibling of `inline_data_offload_candidate.rs`) + register in `crates/inspect-rules-tier1/src/rules/mod.rs`.

**Behavior:** detect `image_url` / `input_image` / inline base64-PDF parts fed to a **`Capability::Vision`** model (use the catalog Vision signal, `pricing.rs:159`). Emit a finding quoting the projected `$X/mo` from D0's `DocProjection` (call the D0 projector; no LLM). Include the runtime preview/chat request-body image-detection diagnostic (mirror the existing inline-data rule's structure).

**Acceptance / tests:**
- Fires on image→Vision-capable model; does NOT fire on text-only, or on image→already-cheap/Gemini (per the direction guard = no projected saving → no finding, or a $0 finding clearly labelled — pick "suppress when projected saving is 0" and test it).
- The `$X/mo` quote is sourced from D0 (not a hardcoded constant).
- Registered + discoverable in the Tier-1 rule set.

**Verify:** `cargo fmt/clippy/test -p tt-inspect-rules-tier1` (+ whatever inspect-core aggregation crate the rule registers through).

## Slice D2 — lossless in-pipeline `doc_compaction` pass (text-side, opt-in)

**Files:**
- new `crates/core/src/passes/doc_compaction.rs` (sibling of `passes/compression.rs`) + register in `crates/core/src/passes/mod.rs`
- `crates/routing/src/lib.rs` — `RouteAction.doc_compaction: bool` (mirror `compress` at `:190`) + validator + default-false initializers (+ plan-core mirror if `RouteAction` is mirrored there)
- `crates/core/src/routes/chat.rs` — thread `PassEffects.doc_compaction_tokens_removed` into `input_tokens_removed`; add `CostBreakdown.doc_compaction_saved_usd` (fold-into-baseline like `compression_saved_usd`); emit `x-tokentrimmer-doc-compaction-saved-usd` header
- new migration `crates/core/migrations/0031_doc_compaction_tokens_removed.up.sql` / `.down.sql` (additive nullable column) + `crates/telemetry/src/request_logs.rs` (`RequestLogRow` field `#[serde(default)]`, INSERT_SQL, bind chain, `INSERT_BIND_COUNT` bump)

**Behavior (v1 lossless transforms only):** on **LARGE** `ContentPart::Text` documents only (size threshold, documented) — exact-dedup of repeated blocks, strip boilerplate (e.g. repeated headers/footers/separators), normalize markdown whitespace. **Content-preserving**; the token-true gate structurally rejects any pass whose non-text parts change and books $0, so losslessness is enforced by construction. **AST-distill of code blocks (tree-sitter) is deferred to v1.1** — start with the three safest transforms.

**Attribution:** `doc_compaction_tokens_removed` threads into `input_tokens_removed` so it folds into `baseline_cost_usd`, rides `tt_saved_usd()` + `MONTH_SAVINGS_SQL` automatically — **no new attestation term, no V4**. Books a distinct `doc_compaction` savings source (like `compression`).

**Acceptance / tests:**
- Off by default: a route without `doc_compaction:true` is byte-identical to today (zero behavior change).
- Lossless: recomputed output token count ≤ input; content-preserving on the dedup/boilerplate/markdown fixtures; the token-true gate passes (no non-text change).
- Small docs are untouched (threshold respected).
- Attribution: measured token delta appears in `baseline_cost_usd` + the `x-tokentrimmer-doc-compaction-saved-usd` header; a `doc_compaction` savings source is recorded.
- Migration `0031` is additive; `RequestLogRow` round-trips with the new column.

**Verify:** `cargo fmt/clippy/test -p tt-core -p tt-routing -p tt-telemetry` (+ plan-core if mirrored); `cargo test --workspace --no-run` for the field-ripple (per the CI `--all-targets` lesson).

## Risks & landmines (carried from §11)

- **Provider-direction guard** — Gemini cheaper than text → book $0. Ships in D0, reused everywhere. Getting this wrong inflates the bill.
- **Formulas are model-version-dependent** — drive from the catalog/model entry, never hardcoded per-call constants.
- **Token-true-gate invariant** — D2 is text-only precisely so it rides the gate cleanly. Do NOT attempt any image→text change in D2 (that's D4's pre-routing seam). Weakening the gate would break the invoice-reconciliation invariant the ledger depends on.
- **`rand` attestation landmine** — not touched by D0–D2 (D2 folds into `request_micros`, no bootstrap-CI resampling). Do NOT bump `rand`/`rand_chacha`.
- **`compute_cost_full` call-site sprawl** — the D2 `CostBreakdown.doc_compaction_saved_usd` effect field MUST be threaded through ALL `compute_cost_full` call sites (chat.rs + sse.rs streaming + cache-hit synthesizers) or streaming/cache traffic silently drops the saving. This is the load-bearing D2 wiring detail.

## Sequencing

D0 → D1 → D2, each its own PR (public, auto-merge on green). D1 depends on D0 (quotes its projector). D2 is independent of D0/D1 (pure text-side) and could land in parallel, but ships after D0 for a coherent narrative. The spec is committed with the D0 PR; D1/D2 branch off `main` once D0 lands.

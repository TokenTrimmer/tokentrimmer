# Document Lane — D4 Server-Side Seam (design + implementation spec)

**Date:** 2026-07-01 · **Scope:** D4 of the Document Lane epic (§11 of `COMPREHENSIVE_REVIEW_2026-06-27.md`) — the pre-routing image/document→text distillation seam that unlocks a vision→text route downgrade, with an out-of-process OCR sidecar and a lossy quality gate. **Repos:** `public/` (gateway + a new sidecar crate). Built in three sequential slices: **D4a** substrate, **D4b** OCR sidecar, **D4c** distillation seam.

> **Builds on:** D0 (image-token projector, merged) + D2 (lossless doc_compaction, merged). **Out of scope (separate specs/items):** D3 (client `tt docprep`), D5 (V4 `doc_micros` attestation + dashboard modality axis), D6 (workflow Document node). D4 books the vision-avoided saving to an **isolated** telemetry field (`doc_vision_saved_est_usd`); signing it is D5.

## The one load-bearing architectural decision

Image/document→text distillation happens in a **pre-routing seam**, ahead of `SplitRequest::compute` (~`crates/core/src/routes/chat.rs:2975`, the retrieval-middleware precedent), NOT in the pass pipeline. Two reasons:
1. **Preserves the token-true-gate invariant.** The pass gate structurally rejects any pass whose non-text parts change (booking $0). An image→text swap is exactly that. Doing it pre-routing means it's never subject to the gate, so the invoice-reconciliation invariant the ledger depends on stays intact.
2. **Unlocks the downgrade.** Swapping image parts → text BEFORE routing flips `request_has_images→false`, so a route can downgrade a vision request to a cheaper text-only model — where the bulk of the savings lives.

The vision-avoided saving is a **COUNTERFACTUAL** (the realized request never contained the image, so it can't be invoice-reconciled) → it books to the **isolated** `doc_vision_saved_est_usd`, never folded into `baseline_cost_usd`/the invoice-reconcilable headline.

## Verified grounding (current code, 2026-07-01)

1. `ContentPart` (`crates/shared/src/messages.rs:315`) is a closed `#[serde(tag="type", snake_case)]` enum: `Text`/`ImageUrl`/`InputAudio` — no `Document`.
2. `request_has_images` (`crates/shared/src/capability_check.rs:165`) + `has_image_part` (`:188`) — the siblings for `request_has_documents`/`has_document_part`.
3. `RouteConditions.has_images: Option<bool>` (`crates/routing/src/lib.rs:89`), runtime branch (`:681-682` → `capability_check::request_has_images`), validator `needs_vision = has_images || has_audio` (`crates/routing/src/validate.rs:188`).
4. `prepare()` at `chat.rs:2390`; `SplitRequest::compute` called at `:3066/:3137/:3169/:3203`; the pre-pass/pre-routing region is ~`:2975-3066`.
5. D0's projector: `tt_preview::document_projection::project` + `tt_tokenize::image_tokens::estimate_image_tokens`. D2's isolated-vs-baseline-fold pattern: `CostBreakdown.compression_saved_usd` (baseline-fold) vs the isolated `*_saved_est_usd` fields.

## Key decisions (§11, confirmed by the owner)

- **Pre-routing seam** (not a pass; not weakening the gate).
- **Isolate** the vision-avoided saving as a counterfactual `doc_vision_saved_est_usd` (never baseline-fold).
- **Price at `input_per_million`** (no per-modality catalog column in D4).
- **Lossy substitution stays opt-in + judge-gated permanently** — `DocDistillGate` default-CLOSED, fail-open to verbatim, scored by the recall-of-baseline judge + the sticky 0.90 `auto_pause` floor. Error blobs never distilled.
- **OCR out-of-process, MIT/Apache only** — NEVER link GPL/AGPL (MinerU AGPL, Marker/Surya GPL) in-process. ocrs/Docling MIT, pdfium (BSD), Paddle/Tesseract Apache.
- **Gemini provider-direction guard (D0) still applies** — no downgrade booked when it would inflate the bill.

---

## SLICE D4a — Document content-part substrate + routing (no reduction)

Pure type/routing plumbing. Ships the capability to accept + route + detect document parts; NO extraction, NO distillation, NO cost reduction (the isolated field exists but is always 0 until D4c).

**Components / files:**
- **`ContentPart::Document`** variant (`crates/shared/src/messages.rs:315`): `Document { document: DocumentPart }` with serde tag matching the OpenAI/Anthropic file-part convention (investigate: OpenAI uses `{"type":"file","file":{...}}`, Anthropic `{"type":"document","source":{...}}` — pick a variant that round-trips both, or add both tags via `#[serde(alias)]`). `DocumentPart` carries the source (url or base64 + media_type) + optional filename. Add `#[serde(other)]`? No — keep it closed but complete.
- **Provider translation** (`crates/shared/src/providers/{anthropic,gemini,compat}`): translate `ContentPart::Document` to each provider's native document/file block (Anthropic `document` block; Gemini `inline_data` with the PDF mime; OpenAI-compat `file`). A document sent to a provider that can't accept it → a clear error or pass-through per the existing capability handling.
- **`request_has_documents` + `has_document_part`** (`crates/shared/src/capability_check.rs`, siblings of `request_has_images` at `:165`/`:188`).
- **Routing:** `RouteConditions.has_documents: Option<bool>` (`routing/lib.rs:89`) + runtime branch (`:681`, calling `request_has_documents`); `--when-has-documents` CLI flag (`crates/cli/src/route/`); plan-core mirror (`plan-core/src/types.rs`, `routing.rs`); `mcp/src/tools/add_route.rs`. **Validation (`validate.rs:188`): do NOT fold `has_documents` into `needs_vision`** — a document route targets a *text* model (that's the point). Use a separate capability check or no capability gate for documents.
- **Isolated cost field:** `CostBreakdown.doc_vision_saved_est_usd: f64` (mirror an existing isolated `*_saved_est_usd` field, e.g. `minify_saved_est_usd`) — threaded through EVERY `compute_cost_full`/`CostBreakdown{}` initializer + all `sse.rs` sites + cache-hit synthesizers (the D2 completeness discipline: `grep <existing isolated field>` == `grep doc_vision_saved_est_usd`) + an `x-tokentrimmer-doc-vision-saved-est-usd` header. Always 0 in D4a.
- **`DocDistillGate` scaffold** (`crates/core/src/document_lane/mod.rs` new, or a stub): a default-CLOSED gate type with the judge/floor plumbing shape but no live distillation yet (D4c fills it).

**Tests:** `ContentPart::Document` round-trips (deserialize OpenAI + Anthropic file parts, serialize back); provider translation for each provider; `request_has_documents` detects a document part + ignores text/image; `has_documents:true` route matches only document requests + validates against a text target (NOT rejected for lacking Vision); the isolated cost field threads through (workspace `--no-run`); default-off = zero behavior change.

## SLICE D4b — out-of-process OCR/parse sidecar + Rust client

**Components / files:**
- **New sidecar crate** `crates/doc-sidecar/` — a standalone HTTP service (axum) exposing `POST /extract` that takes `{media_type, bytes(base64)}` and returns `{text, spans:[{kind:LOSSLESS|LOSSY, ...}], pages, engine}`. Extraction tiers: **pdfium text-layer** (via `pdfium-render`, BSD — needs the pdfium shared lib; document the runtime requirement) for text-layer PDFs (LOSSLESS); **ocrs** (MIT, pure-Rust OCR) for scanned pages/images (LOSSY); optionally shell to a **Docling-MIT** subprocess for table-dense pages (opt-in). License gate enforced in `Cargo.toml` (cargo-deny already gates licenses — verify ocrs/pdfium-render pass; NEVER add MinerU/Marker/Surya).
- **Rust client** in the gateway (`crates/core/src/document_lane/sidecar_client.rs`): a thin HTTP client (reqwest, already a dep) calling the sidecar at a configurable `TT_DOC_SIDECAR_URL` (unset = disabled; the gateway hot path degrades to verbatim when unset or on error — fail-open). Timeout + circuit-breaker friendly.
- **Build/CI:** the sidecar is a new workspace binary; ensure `cargo build --workspace` builds it (pdfium-render may need a feature/system lib — if the native pdfium lib isn't available in CI, gate the pdfium path behind a feature and default the sidecar to ocrs-only so CI builds without the native lib; document the prod requirement). THIRD-PARTY-LICENSES regen for the new deps.

**Tests:** sidecar `/extract` returns text for a text-layer PDF fixture (LOSSLESS spans) + an image fixture (ocrs, LOSSY spans); the Rust client parses the response; client fails-open (returns None) when the sidecar URL is unset or the call errors; license check (cargo-deny) passes.

## SLICE D4c — the pre-routing distillation seam + downgrade + lossy gate

**Components / files:**
- **Pre-routing preprocessor** `crates/core/src/document_lane/seam.rs`, invoked in `prepare()` BEFORE `SplitRequest::compute` (~`chat.rs:2975`): if the request has image/document parts AND the org/route opted into the document lane (a `RouteAction.document_lane: bool`, opt-in, default off), call the sidecar client → distilled text; swap the image/doc parts for `ContentPart::Text` (the distilled text); recompute `request_has_images`/`request_has_documents` (now false) so routing can downgrade to a text model.
- **Downgrade:** because the parts are now text, the existing routing picks a cheaper text model per the route rules (`--when-has-documents → gpt-5-mini` etc.). No new routing logic — the seam just changes the request shape pre-routing.
- **Isolated cost booking:** compute the vision-avoided saving via D0's `document_projection::project` (raw image tokens that WOULD have been sent vs the distilled text tokens, priced at input rate, **Gemini direction guard applies → $0 for Gemini**); book to `doc_vision_saved_est_usd` (the isolated field from D4a) + the header. Thread through every `compute_cost_full`/sse/cache-hit site (already wired as 0 in D4a; D4c sets the real value on the seam path).
- **Lossy gate (`DocDistillGate`, `crates/core/src/document_lane/gate.rs`):** default-CLOSED, fail-open to verbatim. When a distillation span is LOSSY (OCR/scanned), gate the substitution: score the distilled-vs-baseline via the shared recall-of-baseline judge (`passes/agentic_budget/summarize_judge.rs` precedent) → `quality_verdicts` → the sticky 0.90 `auto_pause` floor (`route_autopause.rs`). If the gate is closed or the judge fails/floor not met, keep the verbatim image (no downgrade, no saving booked). LOSSLESS spans (text-layer PDF) skip the judge (structurally safe). **Never distill error blobs.**

**Tests:** a document-lane route with a text-layer-PDF request distills → downgrades to the text model → books `doc_vision_saved_est_usd` (non-zero) + header; a scanned-image request with the gate CLOSED keeps verbatim (no downgrade, $0); with the gate OPEN + judge-pass, distills + books; Gemini target → $0 (direction guard); default (no document_lane opt-in) = zero behavior change; sidecar-unavailable → fail-open to verbatim. Streaming path books the isolated saving too.

---

## Risks & landmines

- **Hot-path change (D4c).** The seam runs in `prepare()` on every request; it MUST early-return cheaply when there are no document/image parts OR the org didn't opt in (the common case) — zero added latency for text traffic. Reviewed carefully.
- **Fail-open everywhere.** Sidecar unset/errored/timed-out, gate closed, judge failed → keep the verbatim request. A document-lane failure must NEVER drop or corrupt a request; worst case is "no saving this time."
- **Isolated-cost threading completeness** — same discipline as D2: `doc_vision_saved_est_usd` through every `compute_cost_full`/`CostBreakdown{}`/sse/cache-hit site; `cargo test --workspace --no-run` proves it.
- **OCR license landmine** — MIT/Apache/BSD only, out-of-process; cargo-deny gates it; never MinerU/Marker/Surya.
- **pdfium native dep** — may not be available in CI; feature-gate it, default the sidecar to ocrs-only for CI builds, document the prod requirement.
- **Gemini direction guard** — D0's guard books $0 for Gemini; the seam must not downgrade-for-savings when the target is Gemini.
- **Counterfactual isolation** — `doc_vision_saved_est_usd` is NEVER baseline-folded (it's not invoice-reconcilable); keep it isolated (D5 signs it separately as V4 `doc_micros`).
- **`rand`** — not touched by D4. Do NOT bump.

## Sequencing

D4a (substrate, off ab16585) → D4b (sidecar, off main after D4a) → D4c (seam, off main after D4b). Each is its own PR. The spec is committed with D4a. D4b's native-dep build is the riskiest CI step; D4c's hot-path + money-path changes get the most review.

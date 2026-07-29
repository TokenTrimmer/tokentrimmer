# Document Lane reference (image/document → text distillation)

Document Lane is the pre-routing image/document → text distillation seam. A
request carrying an image or document PDF that would route to a vision
(expensive) model is, when the route opts in (`RouteAction::document_lane`),
distilled to text *before* routing — so routing can downgrade to a cheaper text
model, + the request is priced using the selected text model's active catalog rate. The savings is isolated
(`doc_vision_saved_est_usd`), never folded into the catalog-priced gateway
headline or represented as provider-invoice reconciliation. Source: `crates/core/src/document_lane/` + `crates/core/src/passes/doc_compaction.rs` + `crates/doc-sidecar/`.

This is one of the three differentiated stories (agent cost governance,
verifiable receipts, **document lane**) — "document tokens are the new silent
spend."

## The slices (D0–D4c)

| Slice | What it ships |
|---|---|
| **D0** | The deterministic image/document-token projector (`crates/preview` / the token-true gate's vision-token accounting). |
| **D1** | The raw-document-to-vision-model routing rule (`crates/inspect-rules-tier1/src/rules/inline_data_offload_candidate.rs:215` — the offline `classify_data_blob` promoted to a live classifier). |
| **D2** | The lossless opt-in `doc_compaction` pass — a content-preserving compaction of document scaffolding (the `RouteAction::doc_compaction` opt-in; `crates/core/src/passes/doc_compaction.rs`). |
| **D4a** | The substrate: the `DocDistillGate` scaffold (default-CLOSED, `should_distill` always `false` — a guaranteed no-op until filled in), + the `SpanFidelity` vocabulary (`Lossless`/`Lossy`). |
| **D4b** | The out-of-process `doc-sidecar` OCR/parse binary + a fail-open Rust client (`crates/doc-sidecar/`: lib, main, ocr.rs; `crates/core/src/document_lane/sidecar_client.rs`). Disabled unless `TT_DOC_SIDECAR_URL` is set. |
| **D4c** | The pre-routing seam in `prepare()`: when `RouteAction::document_lane` opts in, distill image/document parts to text, flip `request_has_images`/`request_has_documents` false so routing can downgrade to a text model, book the isolated `doc_vision_saved_est_usd` (gated by the `DocDistillGate` + the sticky 0.90 auto-pause floor). |

## Content parts

Document Lane works on the `ContentPart::Image` / `ContentPart::Document`
(non-text content parts) in a request's `Message::System`/`Message::Tool`
blocks. Distillation replaces each (or its span) with a `ContentPart::Text`
extracted by the sidecar. The text-layer of a PDF is `Lossless` (structurally
safe — no model in the loop, skips the judge); OCR / vision output for a
scanned page is `Lossy` (must clear the judge + the 0.90 floor before the
swap). `SpanFidelity` tags each span; the gate branches on it.

## The gate (`DocDistillGate`)

Lossy substitution is **opt-in + judge-gated permanently**: the gate is
default-CLOSED + fails open to the verbatim request. **Error blobs are never
distilled** (a sidecar error / timeout → no extraction → the request stays
verbatim + routes to the vision model as before; no behavior change).
`should_distill` always returns `false` in the D4a scaffold; the filled-in
gate (D4c) branches on `SpanFidelity`: lossless spans skip the judge, lossy
spans require it.

## `doc_compaction` (the lossless complement)

Separate-but-complementary: `RouteAction::doc_compaction` (D2) is a content-
preserving compaction of document *scaffolding* (the JSON/whitespace/structural
trim of document-adjacent text), NOT the image/document distillation itself.
It runs in the pass pipeline (`crates/core/src/passes/doc_compaction.rs`),
rides the token-true gate, + books into `doc_compaction_saved_usd` (folded
into the baseline like `compression`, NOT the isolated `doc_vision_saved_est_usd`).

## Cost attribution (the two isolated fields)

| Field | What it carries | Where |
|---|---|---|
| `doc_compaction_saved_usd` | The lossless document-scaffold compaction saving (D2) — folded into the baseline like `compression` (it IS the baseline). | `passes/mod.rs` → `compute_cost_full` |
| `doc_vision_saved_est_usd` | The DISTILLATION saving (D4c) — the isolated estimated $ the vision→text model downgrade saved. NEVER part of `cost_usd`/`baseline_cost_usd`/`saved_usd` (those are catalog-priced gateway figures, not provider-invoice reconciliation); it's a conservative estimate on its own header. | migration 0032; the isolated `CostBreakdown` field (mirrors `content_compress_saved_est_usd`) |

Both surface on `x-tokentrimmer-*` headers.

## Fail-open posture

The whole Document Lane is fail-open-to-verbatim: sidecar disabled / sidecar
error / sidecar timeout / gate-closed / judge-not-trusted → the request
stays verbatim + routes to the vision model as before. No behavior change on
the default path (a route that didn't opt into `document_lane` is byte-
identical).

## Parser and transport bounds

Parsing remains out of process. The sidecar accepts only the shared
PDF/PNG/JPEG/GIF/WebP/BMP/TIFF contract, preflights base64 before decoding,
caps decoded input at 20 MiB, and re-runs the shared container, image-dimension,
and animation-frame validation even when called directly. At most two
extractions execute concurrently; a saturated queue fails quickly, and each
handler waits at most four seconds before returning an empty fail-soft result.
A timed-out blocking parser retains its slot until its thread exits, so a
pathological file cannot cause unbounded parser-thread growth.

The sidecar parses the PDF page tree before text extraction and refuses more
than 100 pages. Successful output is capped at 1,000,000 Unicode scalar values,
4 MiB of UTF-8 text, and 100 spans/pages. The gateway client independently
streams and caps the complete JSON response at 4 MiB plus 64 KiB of envelope
overhead, then requires bounded, unique, in-range page evidence before any
substitution. A limit, malformed response, timeout, panic, or transport error
therefore leaves the original media verbatim.

## Related

- `crates/core/src/document_lane/mod.rs` — the D4a substrate + the `DocDistillGate` + `SpanFidelity`.
- `crates/core/src/document_lane/sidecar_client.rs` — the fail-open D4b client.
- `crates/doc-sidecar/` — the OCR/parse binary (D4b).
- `crates/core/src/passes/doc_compaction.rs` — the lossless D2 compaction pass (`RouteAction::doc_compaction`).
- `docs/superpowers/specs/2026-07-01-document-lane-d4-server-seam-design.md` — the D4 design spec.
- The isolated `doc_vision_saved_est_usd` — migration 0032 (mirrors the `content_compress_saved_est_usd` pattern).

# Document Lane D4 Implementation Plan (server-side seam)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan slice-by-slice (one subagent per slice: D4a, then D4b, then D4c). Steps use checkbox (`- [ ]`) syntax.

**Goal:** Build the Document Lane server seam — a pre-routing image/document→text distillation that unlocks a vision→text route downgrade, with an out-of-process OCR sidecar and a lossy quality gate, booking the vision-avoided saving to an isolated counterfactual field.

**Architecture:** D4a adds the `ContentPart::Document` type + document routing + the isolated `doc_vision_saved_est_usd` field + a `DocDistillGate` scaffold (all plumbing, no reduction). D4b adds an out-of-process `doc-sidecar` HTTP service (pdfium/ocrs, MIT/Apache) + a fail-open Rust client. D4c wires the pre-routing seam in `prepare()` that distills → swaps parts → downgrades → books the isolated saving, gated by `DocDistillGate` + the 0.90 floor.

**Tech Stack:** Rust (workspace crates: `tt-shared`, `tt-routing`, `tt-core`, `tt-cli`, `tt-plan-core`, `tt-mcp`, new `doc-sidecar`), axum, reqwest, pdfium-render (feature-gated), ocrs.

## Global Constraints (verbatim from the spec)

- **Pre-routing seam, not a pass** — distillation happens before `SplitRequest::compute` (~`chat.rs:2975`); never in the pass pipeline.
- **Isolate the vision-avoided saving** — `CostBreakdown.doc_vision_saved_est_usd` is a COUNTERFACTUAL, NEVER baseline-folded (mirror `minify_saved_est_usd`, not `compression_saved_usd`).
- **Price at `input_per_million`; Gemini direction guard applies** ($0 for Gemini) — reuse D0's `document_projection::project`.
- **Lossy substitution opt-in + judge-gated permanently** — `DocDistillGate` default-CLOSED, fail-open to verbatim, 0.90 `auto_pause` floor; lossless text-layer PDF skips the judge; never distill error blobs.
- **OCR out-of-process, MIT/Apache/BSD only** — NEVER GPL/AGPL (MinerU/Marker/Surya). cargo-deny gates licenses.
- **Fail-open everywhere** — sidecar unset/errored/timed-out, gate closed, judge-fail → verbatim request; never drop/corrupt a request; zero added latency for non-document traffic (early-return).
- **`has_documents` is NOT folded into `needs_vision`** (`validate.rs:188`) — a document route targets a text model.
- Public CI hard-gates `cargo fmt --all --check` + clippy; verify field-ripple with `cargo test --workspace --no-run`. Do NOT bump `rand`. Commit trailer required.

---

## SLICE D4a — Document substrate + routing (one PR: `feat/d-lane-d4a-substrate`)

**Read first:** `crates/shared/src/messages.rs` (ContentPart enum + ImageUrl/InputAudio shape + serde), `crates/shared/src/capability_check.rs:165-191` (request_has_images/has_image_part), `crates/routing/src/lib.rs:89,681` + `validate.rs:188` (has_images), and how an existing ISOLATED `*_saved_est_usd` field (e.g. `minify_saved_est_usd`) threads through `CostBreakdown{}` + `compute_cost_full` in `chat.rs`.

### Task A1: `ContentPart::Document` variant + `DocumentPart` type
**Files:** `crates/shared/src/messages.rs`.
**Produces:** `ContentPart::Document { document: DocumentPart }` where `pub struct DocumentPart { pub source: DocumentSource, pub filename: Option<String> }` and `DocumentSource` covers `{ url: String }` (incl. `data:` URLs) or `{ media_type: String, data: String }` (base64). Serde tag: accept BOTH the OpenAI `{"type":"file","file":{...}}` and Anthropic `{"type":"document","source":{...}}` shapes (use `#[serde(rename)]` + `#[serde(alias)]` or a custom deserialize — pick what round-trips both; test both).
- [ ] TDD: deserialize an OpenAI file part + an Anthropic document part → `ContentPart::Document`; serialize back to a canonical form; a text/image part is unaffected. Run → fail → implement → pass → commit.

### Task A2: `request_has_documents` + `has_document_part`
**Files:** `crates/shared/src/capability_check.rs` (siblings of `:165`/`:188`).
**Produces:** `pub fn request_has_documents(req: &ChatCompletionRequest) -> bool` + `fn has_document_part(c: &MessageContent) -> bool`.
- [ ] TDD: detects a Document part; false for text-only/image-only. Mirror the `request_has_images` tests exactly. Commit.

### Task A3: `RouteConditions.has_documents` + runtime + validate (NOT needs_vision)
**Files:** `crates/routing/src/lib.rs` (field near `:89`, runtime branch near `:681` calling `request_has_documents`), `crates/routing/src/validate.rs` (near `:188` — add a SEPARATE handling; do NOT add `has_documents` to the `needs_vision` OR).
**Produces:** `RouteConditions.has_documents: Option<bool>` with the same serde defaults as `has_images`.
- [ ] TDD: a `has_documents:true` route matches only requests with a Document part; it VALIDATES against a text (non-Vision) target (this is the key difference from `has_images` — a document route downgrades to text, so it must NOT require a Vision target). Commit.

### Task A4: CLI `--when-has-documents` + plan-core mirror + mcp add_route
**Files:** `crates/cli/src/route/mod.rs` (+ `main.rs` if the flag is registered there), `crates/plan-core/src/types.rs` + `routing.rs` (mirror the RouteConditions field IF plan-core mirrors RouteConditions — grep to confirm), `crates/mcp/src/tools/add_route.rs`.
- [ ] TDD: `--when-has-documents` sets `has_documents:Some(true)`; plan-core round-trips; mcp add_route accepts it. Commit.

### Task A5: isolated `doc_vision_saved_est_usd` field (threaded everywhere, always 0 in D4a)
**Files:** `crates/core/src/routes/chat.rs` — add `CostBreakdown.doc_vision_saved_est_usd: f64` (mirror the isolated `minify_saved_est_usd` field, NOT `compression_saved_usd`); thread through EVERY `CostBreakdown{}` initializer + `compute_cost_full` call site + all `sse.rs` sites + cache-hit synthesizers (grep the mirrored isolated field → every site gets `doc_vision_saved_est_usd: 0.0` in D4a); add `x-tokentrimmer-doc-vision-saved-est-usd` header (emits `0.000000` in D4a). `crates/telemetry/src/request_logs.rs`: a `doc_vision_saved_est_usd` column via the NEXT migration (0031 is taken by D2 → use **0032**) + RequestLogRow field + INSERT/bind/`INSERT_BIND_COUNT`.
- [ ] TDD: the header emits `0.000000` on a normal request; `grep <mirrored isolated field>` == `grep doc_vision_saved_est_usd` (completeness); migration 0032 additive; `cargo test --workspace --no-run` passes. Commit.

### Task A6: `DocDistillGate` scaffold + `document_lane` module
**Files:** new `crates/core/src/document_lane/mod.rs` (+ `pub mod document_lane;` in the core lib) with `pub struct DocDistillGate` default-CLOSED + a `pub fn should_distill(...) -> bool` returning false (scaffold; D4c fills it). `RouteAction.document_lane: bool` (opt-in, off by default, mirror `compress` in `routing/lib.rs`).
- [ ] TDD: `DocDistillGate::default()` is closed; `RouteAction.document_lane` defaults false + serde-omits when false. Commit.

### Task A7: D4a PR
- [ ] `cargo fmt` + clippy + test the touched crates + `cargo test --workspace --no-run`; push; PR "feat(shared,routing,core): D4a — Document content-part substrate + routing (Document Lane)"; auto-merge.

---

## SLICE D4b — OCR/parse sidecar + fail-open client (one PR: `feat/d-lane-d4b-sidecar`, off main after D4a)

### Task B1: the `doc-sidecar` crate (HTTP extraction service)
**Files:** new `crates/doc-sidecar/` (Cargo.toml + `src/main.rs` axum server). **Endpoint** `POST /extract` — request `{ media_type: String, data_base64: String }`, response `{ text: String, spans: Vec<Span>, pages: u32, engine: String }` where `Span { kind: "lossless"|"lossy", page: u32, chars: usize }`. Extraction: **pdfium text-layer** (via `pdfium-render`, feature `pdfium`, for `application/pdf` → lossless spans) and **ocrs** (pure-Rust, MIT, for images / scanned → lossy spans). **Feature-gate pdfium** so `--no-default-features` builds ocrs-only (CI has no native pdfium lib); default features include pdfium for prod. License: only ocrs/pdfium-render/image (all MIT/Apache/BSD) — verify cargo-deny passes; NEVER MinerU/Marker/Surya.
- [ ] TDD: `POST /extract` on a text-layer-PDF fixture → non-empty text + lossless spans; on a PNG fixture → ocrs text + lossy spans (or a documented "no text found" for a blank image); malformed base64 → 400. Run the axum handler via `tower::ServiceExt::oneshot` (axum test pattern). Commit.

### Task B2: the fail-open Rust client
**Files:** new `crates/core/src/document_lane/sidecar_client.rs`. `pub async fn extract(client: &reqwest::Client, sidecar_url: Option<&str>, media_type: &str, data_base64: &str) -> Option<Extraction>` — returns `None` when `sidecar_url` is `None` (disabled) OR on any error/timeout (fail-open). `Extraction { text, spans, pages }`. Config: `TT_DOC_SIDECAR_URL` env → the URL (unset = disabled). Short timeout (e.g. 5s).
- [ ] TDD: `extract(_, None, ...)` → None (disabled); a mocked 200 → Some(parsed); a mocked 500/timeout → None (fail-open). Commit.

### Task B3: build + license + D4b PR
- [ ] `cargo build --workspace` (confirm doc-sidecar builds; if pdfium native lib is absent, `--no-default-features` for the sidecar in CI — document); `cargo deny check licenses`; regenerate THIRD-PARTY-LICENSES (`scripts/gen-third-party-licenses.sh`) for the new deps + commit; `cargo test -p doc-sidecar -p tt-core`; push; PR "feat(doc-sidecar): D4b — out-of-process OCR/parse sidecar + fail-open client"; auto-merge.

---

## SLICE D4c — the distillation seam + downgrade + gate (one PR: `feat/d-lane-d4c-seam`, off main after D4b)

**Read first:** `chat.rs` `prepare()` (`:2390`) + the pre-`SplitRequest` region (`:2975-3066`); the retrieval-middleware precedent (grep how retrieval injects pre-routing); `passes/agentic_budget/summarize_judge.rs` (the recall-of-baseline judge) + `route_autopause.rs` (the 0.90 floor); D0's `document_projection::project`.

### Task C1: the pre-routing preprocessor
**Files:** new `crates/core/src/document_lane/seam.rs`; wire into `prepare()` BEFORE `SplitRequest::compute`.
**Logic:** early-return (cheap) unless the request has image/document parts AND the route opted in (`RouteAction.document_lane`). Otherwise: for each image/document part, `sidecar_client::extract(...)`; if `Some(extraction)`, and the gate passes (Task C3), replace the part with `ContentPart::Text{ text: extraction.text }`; recompute `request_has_images`/`request_has_documents`. Fail-open: any None/error → leave the part verbatim.
- [ ] TDD: a document-lane-opted request with a (mocked) text-layer-PDF extraction → the part becomes Text + `request_has_images` flips false; a non-opted request → untouched; sidecar None → verbatim. Commit.

### Task C2: isolated cost booking (the vision-avoided saving)
**Files:** `crates/core/src/document_lane/seam.rs` + `chat.rs` (set `doc_vision_saved_est_usd` on the seam path; it's already threaded through every site as 0 from D4a).
**Logic:** raw image tokens (what WOULD have been sent — via D0 `estimate_image_tokens` on the original image dims) vs distilled text tokens; `document_projection::project(raw, distilled, input_price_per_mtok, served_model)` → `projected_savings_usd` (Gemini guard → $0); set `doc_vision_saved_est_usd` + the header. Book on the seam path only.
- [ ] TDD: a distilled request books non-zero `doc_vision_saved_est_usd` + header; a Gemini-target request books $0 (direction guard); a non-seam request stays 0. Streaming path books it too. Commit.

### Task C3: the lossy `DocDistillGate` (fill the D4a scaffold)
**Files:** `crates/core/src/document_lane/gate.rs` (+ replace the D4a scaffold `should_distill`).
**Logic:** LOSSLESS spans (text-layer PDF) → allow (skip judge, structurally safe). LOSSY spans (OCR/scanned) → default-CLOSED: only allow if the route explicitly opted lossy AND the recall-of-baseline judge (reuse `summarize_judge`) scores the distilled-vs-verbatim ≥ the sticky 0.90 `auto_pause` floor (`route_autopause.rs`). Judge-fail/floor-miss/error → closed (verbatim). Never distill error blobs (detect + skip). Fail-open.
- [ ] TDD: lossless span → allowed without a judge call; lossy span, gate closed → verbatim; lossy span, opted + judge-pass (mocked ≥0.90) → distilled; judge-fail (<0.90) → verbatim; error blob → never distilled. Commit.

### Task C4: D4c PR
- [ ] `cargo fmt` + clippy + test tt-core + `cargo test --workspace --no-run`; push; PR "feat(core): D4c — pre-routing document distillation seam + vision→text downgrade + DocDistillGate"; **DO NOT auto-merge — this is the hot-path + money-path slice; leave it for review.** Report the seam early-return + the isolated-cost threading + the gate fail-open behavior.

---

## Self-Review

**Spec coverage:** D4a substrate = A1–A7 (ContentPart::Document, request_has_documents, has_documents routing not-needs_vision, CLI/plan-core/mcp, isolated field, gate scaffold) ✓; D4b sidecar = B1–B3 (crate, client, license/build) ✓; D4c seam = C1–C4 (preprocessor, isolated booking, gate, PR) ✓. Key decisions (pre-routing, isolate, input-rate + Gemini guard, opt-in lossy + 0.90 floor, MIT/Apache OCR, fail-open) all encoded ✓.

**Placeholders:** the ContentPart::Document serde-tag ("round-trips both OpenAI + Anthropic — test both") + the sidecar extraction tiers carry concrete instructions + the HTTP contract; "mirror sibling X" points at named files/lines (real pattern-match, not a placeholder). Migration slot corrected to **0032** (0031 is D2).

**Type consistency:** `ContentPart::Document{document: DocumentPart}` / `request_has_documents` / `RouteConditions.has_documents` / `doc_vision_saved_est_usd` / `DocDistillGate` / `sidecar_client::extract`→`Extraction` / `RouteAction.document_lane` used consistently across A1→C3.

**Migration slot note:** D4a uses **0032** for `doc_vision_saved_est_usd` on request_logs (verify at build time it's the next free public-core slot).

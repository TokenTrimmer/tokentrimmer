# Document Lane D0–D2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the zero-to-low-risk foundation of the Document Lane — a deterministic image/document-token projector (D0), an Inspect rule that quotes it (D1), and a lossless opt-in text-side compaction pass (D2) — so image-heavy requests stop pricing as ~0 and large text docs can be losslessly trimmed with attested savings.

**Architecture:** D0 adds a pure, model-family-keyed image-token estimator in `tt-tokenize` and a `document_projection` module in `tt-preview` (mirroring `cache_projection`), then sums image tokens into the preview total and surfaces a `DocProjection` on `PreviewResponse`. D1 adds a static Tier-1 Inspect rule that detects image→Vision-model and quotes D0. D2 adds an opt-in `doc_compaction` `RequestPass` (sibling of `compression`) whose measured token delta folds into `baseline_cost_usd` like compression — no new attestation term.

**Tech Stack:** Rust (workspace crates: `tt-tokenize`, `tt-preview`, `tt-shared`, `tt-inspect-rules-tier1`, `tt-core`, `tt-routing`, `tt-telemetry`), sqlx migrations, cargo test.

## Global Constraints (verbatim from the spec)

- **Price avoided image tokens at `input_per_million`, labelled `basis:"estimate"`.** No `image_per_million` catalog column in this arc.
- **Provider-direction guard:** the projector reports **NO saving (clamp negative → 0)** when the resolved model is **Gemini**.
- **Image-token formulas are model-version-dependent — drive from the model identifier, never hardcoded per-call.** Formulas (verified vs provider docs, June 2026): Claude `⌈w/28⌉·⌈h/28⌉`; OpenAI GPT-4o high-detail `85 + 170·tiles` (512px tiles, shortest side scaled to 768 within 2048²); OpenAI GPT-5.x/mini/o-series `⌈w/32⌉·⌈h/32⌉` patches **capped at 1536**; Gemini ≤384px → 258 flat, else `⌈w/768⌉·⌈h/768⌉·258`.
- **D2 is opt-in (`RouteAction.doc_compaction`, off by default) and lossless-only** (dedup + boilerplate-strip + markdown-normalize on LARGE text docs; AST code-distill deferred). Losslessness is enforced structurally by the token-true gate.
- **Do NOT bump `rand`/`rand_chacha`.** Do NOT add a heavy image-decode crate (read image header dims only).
- Public CI hard-gates `cargo fmt --all --check` + clippy — run `cargo fmt -p <crate>` per touched crate; verify field-ripple with `cargo test --workspace --no-run`.
- Every commit message ends with the `Co-Authored-By:` + `Claude-Session:` trailer.

---

## File Structure

**D0:**
- Create `crates/tokenize/src/image_tokens.rs` — pure image-token estimator + model→formula classifier.
- Modify `crates/tokenize/src/lib.rs` — `pub mod image_tokens;`.
- Create `crates/preview/src/document_projection.rs` — `project()` + `DocProjection` (mirror `cache_projection.rs`).
- Modify `crates/preview/src/lib.rs` — `pub mod document_projection;`.
- Modify `crates/preview/src/types.rs` — `DocProjection` field on `PreviewResponse` (or re-export from `document_projection`).
- Modify `crates/preview/src/token_estimator.rs` — a `sum_image_tokens(messages, model)` helper + add its result to the estimate total.

**D1:**
- Create `crates/inspect-rules-tier1/src/rules/raw_document_to_vision_model.rs`.
- Modify `crates/inspect-rules-tier1/src/rules/mod.rs` — register the rule.

**D2:**
- Create `crates/core/src/passes/doc_compaction.rs` (sibling of `compression.rs`).
- Modify `crates/core/src/passes/mod.rs` — register.
- Modify `crates/routing/src/lib.rs` — `RouteAction.doc_compaction: bool` + validator + defaults (+ plan-core mirror if present).
- Modify `crates/core/src/routes/chat.rs` — thread `doc_compaction_tokens_removed` → `input_tokens_removed`; `CostBreakdown.doc_compaction_saved_usd`; header — through ALL `compute_cost_full` call sites.
- Create `crates/core/migrations/0031_doc_compaction_tokens_removed.{up,down}.sql`.
- Modify `crates/telemetry/src/request_logs.rs` — `RequestLogRow` field + INSERT_SQL + bind + `INSERT_BIND_COUNT`.

---

## SLICE D0 — deterministic image/document-token projector

### Task 1: image-token estimator + model→formula classifier (`tt-tokenize`)

**Files:**
- Create: `crates/tokenize/src/image_tokens.rs`
- Modify: `crates/tokenize/src/lib.rs` (add `pub mod image_tokens;`)
- Test: inline `#[cfg(test)] mod tests` in `image_tokens.rs`

**Interfaces:**
- Produces: `pub fn estimate_image_tokens(model: &str, width: u32, height: u32, detail: ImageDetail) -> u32` and `pub enum ImageDetail { Low, High, Auto }` and `pub enum ImageFormula { Claude, OpenAiTile, OpenAiPatch, GeminiTiled }` + `pub fn formula_for_model(model: &str) -> ImageFormula`.
- `formula_for_model` classification (case-insensitive, prefix/contains on the model id): `claude*`/`anthropic*` → `Claude`; `gpt-4o*`/`chatgpt-4o*` → `OpenAiTile`; `gpt-5*`/`gpt-4.1*`/`o1*`/`o3*`/`o4*` → `OpenAiPatch`; `gemini*` → `GeminiTiled`. Default (unknown) → `OpenAiTile` (documented as the conservative middle estimate).

- [ ] **Step 1: Write the failing tests** (real formula values)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_formula_ceil_div_28() {
        // 1024x1024 → ceil(1024/28)=37 ; 37*37 = 1369
        assert_eq!(estimate_image_tokens("claude-opus-4-8", 1024, 1024, ImageDetail::High), 1369);
    }

    #[test]
    fn openai_4o_low_detail_is_flat_85() {
        assert_eq!(estimate_image_tokens("gpt-4o", 2048, 2048, ImageDetail::Low), 85);
    }

    #[test]
    fn openai_4o_high_detail_tiles() {
        // 1024x1024 → scaled within 2048, shortest side to 768 → 2x2 tiles = 4 ; 85 + 170*4 = 765
        assert_eq!(estimate_image_tokens("gpt-4o", 1024, 1024, ImageDetail::High), 765);
    }

    #[test]
    fn openai_patch_capped_at_1536() {
        // huge image → ceil(w/32)*ceil(h/32) clamped to 1536
        assert_eq!(estimate_image_tokens("gpt-5", 100_000, 100_000, ImageDetail::High), 1536);
    }

    #[test]
    fn gemini_small_is_flat_258() {
        assert_eq!(estimate_image_tokens("gemini-2.5-flash", 300, 300, ImageDetail::High), 258);
    }

    #[test]
    fn gemini_large_is_tiled_258() {
        // 1000x1000 → ceil(1000/768)=2 → 2*2*258 = 1032
        assert_eq!(estimate_image_tokens("gemini-2.5-pro", 1000, 1000, ImageDetail::High), 1032);
    }

    #[test]
    fn unknown_model_defaults_to_openai_tile() {
        assert!(matches!(formula_for_model("mystery-model"), ImageFormula::OpenAiTile));
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tt-tokenize image_tokens 2>&1 | tail -5`
Expected: FAIL (module/functions not defined).

- [ ] **Step 3: Implement `image_tokens.rs`**

```rust
//! Deterministic, model-family-keyed image-token estimator (no LLM, no network).
//!
//! Formulas verified vs provider docs (June 2026); they shift by model version,
//! so the family is resolved from the model id, never a per-call constant.
//! Directional estimate — the provider's reported usage stays authoritative.

/// OpenAI-style detail hint. `Auto`/`High` price the full tiling; `Low` is flat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageDetail { Low, High, Auto }

/// The per-provider image-token formula family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormula { Claude, OpenAiTile, OpenAiPatch, GeminiTiled }

/// Classify a model id into its image-token formula family (case-insensitive).
pub fn formula_for_model(model: &str) -> ImageFormula {
    let m = model.to_ascii_lowercase();
    if m.starts_with("claude") || m.starts_with("anthropic") {
        ImageFormula::Claude
    } else if m.starts_with("gemini") {
        ImageFormula::GeminiTiled
    } else if m.starts_with("gpt-4o") || m.starts_with("chatgpt-4o") {
        ImageFormula::OpenAiTile
    } else if m.starts_with("gpt-5") || m.starts_with("gpt-4.1")
        || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") {
        ImageFormula::OpenAiPatch
    } else {
        ImageFormula::OpenAiTile // conservative middle default for unknown models
    }
}

fn ceil_div(a: u32, b: u32) -> u32 { a.div_ceil(b) }

/// Estimate the input-token cost of a single image at `width`x`height` for `model`.
pub fn estimate_image_tokens(model: &str, width: u32, height: u32, detail: ImageDetail) -> u32 {
    let (w, h) = (width.max(1), height.max(1));
    match formula_for_model(model) {
        ImageFormula::Claude => ceil_div(w, 28) * ceil_div(h, 28),
        ImageFormula::OpenAiTile => {
            if detail == ImageDetail::Low { return 85; }
            // scale into 2048x2048, then shortest side to 768, then 512px tiles.
            let (mut w2, mut h2) = (w, h);
            let longest = w2.max(h2);
            if longest > 2048 {
                w2 = (w2 as u64 * 2048 / longest as u64) as u32;
                h2 = (h2 as u64 * 2048 / longest as u64) as u32;
            }
            let shortest = w2.min(h2).max(1);
            if shortest > 768 {
                w2 = (w2 as u64 * 768 / shortest as u64) as u32;
                h2 = (h2 as u64 * 768 / shortest as u64) as u32;
            }
            let tiles = ceil_div(w2.max(1), 512) * ceil_div(h2.max(1), 512);
            85 + 170 * tiles
        }
        ImageFormula::OpenAiPatch => (ceil_div(w, 32) * ceil_div(h, 32)).min(1536),
        ImageFormula::GeminiTiled => {
            if w <= 384 && h <= 384 { 258 } else { ceil_div(w, 768) * ceil_div(h, 768) * 258 }
        }
    }
}
```
Add `pub mod image_tokens;` to `crates/tokenize/src/lib.rs`.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p tt-tokenize image_tokens 2>&1 | tail -5`  → PASS (all 7).

- [ ] **Step 5: fmt + clippy + commit**

Run: `cargo fmt -p tt-tokenize && cargo clippy -p tt-tokenize --all-targets -- -D warnings 2>&1 | tail -3`
```bash
git add crates/tokenize/src/image_tokens.rs crates/tokenize/src/lib.rs
git commit -m "feat(tokenize): deterministic per-provider image-token estimator (D0)"  # + trailer
```

### Task 2: `document_projection` module with the provider-direction guard (`tt-preview`)

**Files:**
- Create: `crates/preview/src/document_projection.rs`
- Modify: `crates/preview/src/lib.rs` (`pub mod document_projection;`)
- Test: inline tests

**Interfaces:**
- Consumes: `tt_tokenize::image_tokens::{estimate_image_tokens, formula_for_model, ImageFormula}`.
- Produces: `pub struct DocProjection { pub raw_image_tokens: u32, pub raw_image_cost_usd: f64, pub projected_distilled_tokens: u32, pub projected_distilled_cost_usd: f64, pub projected_savings_usd: f64, pub basis: &'static str, pub note: Option<String> }` and `pub fn project(raw_image_tokens: u32, projected_distilled_tokens: u32, input_price_per_mtok: f64, model: &str) -> DocProjection`.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn savings_positive_for_openai() {
        // raw 1000 img tok vs 200 distilled text tok @ $5/Mtok input
        let p = project(1000, 200, 5.0, "gpt-4o");
        assert!((p.raw_image_cost_usd - 0.005).abs() < 1e-9);
        assert!((p.projected_distilled_cost_usd - 0.001).abs() < 1e-9);
        assert!((p.projected_savings_usd - 0.004).abs() < 1e-9);
        assert_eq!(p.basis, "estimate");
    }

    #[test]
    fn gemini_direction_guard_books_zero() {
        // Gemini prices page-images flat + cheaper than text → NO saving.
        let p = project(258, 800, 1.0, "gemini-2.5-flash");
        assert_eq!(p.projected_savings_usd, 0.0);
        assert!(p.note.is_some());
    }

    #[test]
    fn negative_savings_clamped_to_zero() {
        let p = project(100, 500, 5.0, "gpt-4o"); // distilled costs more → clamp
        assert_eq!(p.projected_savings_usd, 0.0);
    }
}
```

- [ ] **Step 2: Run → FAIL.** `cargo test -p tt-preview document_projection 2>&1 | tail -5`

- [ ] **Step 3: Implement** (mirror `cache_projection.rs`'s pure-fn shape)

```rust
//! Deterministic document/image-token savings projection (no LLM).
//! Mirrors `cache_projection`: a pure `project()` + a struct.

use tt_tokenize::image_tokens::{formula_for_model, ImageFormula};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DocProjection {
    pub raw_image_tokens: u32,
    pub raw_image_cost_usd: f64,
    pub projected_distilled_tokens: u32,
    pub projected_distilled_cost_usd: f64,
    pub projected_savings_usd: f64,
    pub basis: &'static str,
    pub note: Option<String>,
}

/// Project the savings of replacing raw image tokens with distilled text tokens.
/// Both are priced at the model's INPUT rate (`input_price_per_mtok`), labelled
/// an estimate. The provider-direction guard books ZERO for Gemini (page-images
/// are priced flat + cheaper than distilled text there).
pub fn project(
    raw_image_tokens: u32,
    projected_distilled_tokens: u32,
    input_price_per_mtok: f64,
    model: &str,
) -> DocProjection {
    let price = |tok: u32| (tok as f64) * input_price_per_mtok / 1_000_000.0;
    let raw_cost = price(raw_image_tokens);
    let distilled_cost = price(projected_distilled_tokens);
    let gemini = matches!(formula_for_model(model), ImageFormula::GeminiTiled);
    let (savings, note) = if gemini {
        (0.0, Some("Gemini prices page-images flat and cheaper than distilled text; no saving booked.".to_string()))
    } else {
        ((raw_cost - distilled_cost).max(0.0), None)
    };
    DocProjection {
        raw_image_tokens,
        raw_image_cost_usd: raw_cost,
        projected_distilled_tokens,
        projected_distilled_cost_usd: distilled_cost,
        projected_savings_usd: savings,
        basis: "estimate",
        note,
    }
}
```
Add `pub mod document_projection;` to `crates/preview/src/lib.rs`. (If `tt-tokenize` isn't already a dep of `tt-preview`, it is — it's used by `token_estimator.rs`; confirm.)

- [ ] **Step 4: Run → PASS.**  **Step 5: fmt+clippy+commit** (`feat(preview): document_projection with Gemini direction guard (D0)`).

### Task 3: sum image tokens into the preview estimate (`tt-preview`)

**Files:** Modify `crates/preview/src/token_estimator.rs` + its tests.

**Design note (corrects the spec's phrasing):** `concat_message_text` returns a *text string*; image tokens can't be concatenated in. Add a **separate** `sum_image_tokens(messages, model) -> u32` that scans parts for `image_url` with an inline `data:` URL, reads header dims, and sums `estimate_image_tokens`. Add its result to the token total the estimator returns.

**Image dims from a data URL (no heavy decode crate):** parse `data:image/<fmt>;base64,<payload>`, base64-decode only the header bytes, read dims: **PNG** → bytes 16..24 are width/height big-endian u32 (after the 8-byte signature + IHDR length+type); **JPEG** → scan segments for SOF0/SOF2 markers (`0xFFC0`/`0xFFC2`), height at offset+5 (u16 BE), width at offset+7. If the URL is remote (`http`), or dims unreadable, use a documented default (e.g. treat as `detail=High` with a nominal 1024×1024) and do NOT fail.

- [ ] **Step 1: Write failing test** — an image-only user message previews > 0 input tokens; a text-only message is unchanged.

```rust
#[test]
fn image_part_adds_tokens() {
    // a 1x1 PNG data URL (smallest valid) still yields the model's min image tokens
    let png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMEAWJcCq0AAAAASUVORK5CYII=";
    let msgs = vec![Message { role: "user".into(), content: serde_json::json!([{"type":"image_url","image_url":{"url": png}}]) }];
    let est = estimate_input_tokens_for_model(&msgs, "gpt-4o"); // use the real estimator entry point
    assert!(est > 0, "image-only request must not preview as 0 tokens");
}
```
(Use the estimator's actual public entry signature — read `token_estimator.rs` lines 25-35 for the real function name/params; adapt the call.)

- [ ] **Step 2: Run → FAIL** (previews 0 today).
- [ ] **Step 3: Implement** `sum_image_tokens` + add to the total in the estimate fn. Keep `concat_message_text` unchanged (text only).
- [ ] **Step 4: Run → PASS** + confirm the existing text-only tests still pass.
- [ ] **Step 5: fmt+clippy+commit** (`feat(preview): count image tokens in preview estimate (D0)`).

### Task 4: surface `DocProjection` on `PreviewResponse` + wire the surfaces

**Files:** Modify `crates/preview/src/types.rs` (add `pub doc_projection: Option<DocProjection>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`), and the preview builder that constructs `PreviewResponse` (compute a `DocProjection` when the request has image parts: raw = summed image tokens; distilled = a conservative text-equivalent estimate, e.g. a documented fraction or a fixed nominal — v1 may use a simple heuristic, LABELLED estimate; the point is to surface a directional figure, not a precise one). Wire the same into the wasm playground preview + Inspect preview call paths if they construct `PreviewResponse` (they consume it — no change needed beyond the new optional field).

- [ ] **Step 1–5:** test `PreviewResponse` serializes `doc_projection` when image parts present + is absent otherwise; build the projection in the builder; fmt/clippy/commit (`feat(preview): DocProjection on PreviewResponse (D0)`).

### Task 5: D0 PR — verify + ship

- [ ] `cargo fmt -p tt-tokenize -p tt-preview` ; `cargo clippy -p tt-tokenize -p tt-preview --all-targets -- -D warnings` ; `cargo test -p tt-tokenize -p tt-preview` ; `cargo test --workspace --no-run` (field-ripple).
- [ ] Push `feat/d-lane-d0-projector` (includes the committed spec + plan), open PR "feat(tokenize,preview): D0 — deterministic image/doc-token projector", enable auto-merge `--squash`.

---

## SLICE D1 — Inspect 'raw-document-to-vision-model' rule

Branch `feat/d-lane-d1-inspect-rule` off `main` **after D0 merges** (needs D0's `document_projection`).

### Task 6: the Tier-1 rule

**Files:** Create `crates/inspect-rules-tier1/src/rules/raw_document_to_vision_model.rs`; modify `rules/mod.rs` (register). **First read `inline_data_offload_candidate.rs` fully** to match the exact rule trait/registration/finding-emit pattern this crate uses.

**Interfaces:** Consumes `tt_preview::document_projection::project` + the catalog `Capability::Vision` signal (`tt_shared::pricing`, `pricing.rs:159`). Produces a registered rule that emits a finding when an `image_url`/`input_image`/inline base64-PDF part is fed to a Vision-capable model, quoting `projected_savings_usd`.

- [ ] **Step 1: failing test** — rule fires on `[image_url] → gpt-4o` (Vision), does NOT fire on text-only, does NOT fire when the projected saving is 0 (e.g. Gemini via the direction guard, or already-cheap). Use the crate's existing rule-test harness (copy the shape from `inline_data_offload_candidate`'s tests).
- [ ] **Step 2: Run → FAIL.**
- [ ] **Step 3: Implement** the rule + register in `mod.rs`; the `$X/mo` figure comes from D0 (multiply the per-request projected saving by a documented monthly-volume assumption OR label it per-request — match how the sibling Tier-1 rules express monthly figures; if none do, express per-request saving and note projection).
- [ ] **Step 4: Run → PASS.**  **Step 5: fmt/clippy/test/commit** (`feat(inspect): D1 raw-document-to-vision-model rule`).

### Task 7: D1 PR — verify + ship (auto-merge).

---

## SLICE D2 — lossless opt-in `doc_compaction` pass

Branch `feat/d-lane-d2-doc-compaction` off `main` (independent of D0/D1; land after for narrative). **First read `crates/core/src/passes/compression.rs` + the `RequestPass` trait + `RouteAction.compress` wiring (`routing/src/lib.rs:190`) + how `compression_saved_usd`/`compression_tokens_removed` thread through `chat.rs` `compute_cost_full` — D2 mirrors ALL of it under a `doc_compaction` name.**

### Task 8: `RouteAction.doc_compaction` opt-in flag

**Files:** Modify `crates/routing/src/lib.rs` (mirror `compress` at `:190`: `#[serde(default, skip_serializing_if = "std::ops::Not::not")] pub doc_compaction: bool`), its validator, default-false initializers, and any `plan-core` mirror of `RouteAction`.

- [ ] TDD: a route JSON with `"doc_compaction": true` deserializes; default is false; serialize omits it when false. Mirror the `compress` tests. fmt/clippy/test/commit.

### Task 9: the pass

**Files:** Create `crates/core/src/passes/doc_compaction.rs` (sibling of `compression.rs`); register in `passes/mod.rs`.

**Behavior:** implement `RequestPass` for `DocCompactionPass`; on LARGE `ContentPart::Text` docs only (size threshold const, e.g. ≥ 4 KiB, documented), apply lossless transforms: exact-dedup of repeated ≥N-line blocks, strip repeated boilerplate lines (headers/footers/separators appearing ≥K times), normalize markdown whitespace (collapse ≥3 blank lines, trailing spaces). Return the modified text + a `tokens_removed` count. Content-preserving; the token-true gate enforces losslessness structurally.

- [ ] TDD: dedup fixture (repeated block removed once), boilerplate fixture, markdown fixture; small doc untouched; off-by-default (pass not applied unless the RouteAction opts in). Mirror `compression.rs`'s test structure. fmt/clippy/test/commit.

### Task 10: attribution wiring through `compute_cost_full` (the load-bearing task)

**Files:** Modify `crates/core/src/routes/chat.rs` — add `PassEffects.doc_compaction_tokens_removed`, thread into `input_tokens_removed` (so it folds into `baseline_cost_usd`), add `CostBreakdown.doc_compaction_saved_usd` (fold-into-baseline like `compression_saved_usd`), emit `x-tokentrimmer-doc-compaction-saved-usd`. **Thread the new `CostBreakdown` effect field through ALL `compute_cost_full` call sites (chat.rs + every `sse.rs` streaming site + the cache-hit synthesizers)** or streaming/cache traffic silently drops the saving.

- [ ] TDD: a request through a `doc_compaction:true` route books `doc_compaction_saved_usd` into `baseline_cost_usd` + emits the header; a streaming request does the same (test the sse path); default route unchanged. fmt/clippy/test/commit.

### Task 11: telemetry column + migration

**Files:** Create `crates/core/migrations/0031_doc_compaction_tokens_removed.{up,down}.sql` (additive nullable `doc_compaction_tokens_removed BIGINT`); modify `crates/telemetry/src/request_logs.rs` — `RequestLogRow` field `#[serde(default)]`, INSERT_SQL column, bind chain, bump `INSERT_BIND_COUNT`.

- [ ] TDD: `RequestLogRow` round-trips with the new column; migration is additive (`ADD COLUMN IF NOT EXISTS`). Confirm `INSERT_BIND_COUNT` matches the new bind count (mirror how `compression_tokens_removed` is wired). fmt/clippy/test/commit.

### Task 12: D2 PR — verify + ship

- [ ] `cargo fmt -p tt-core -p tt-routing -p tt-telemetry` ; clippy those crates `--all-targets -D warnings` ; `cargo test -p tt-core -p tt-routing -p tt-telemetry` ; `cargo test --workspace --no-run` (field-ripple through compute_cost_full). Open PR, auto-merge.

---

## Self-Review

**Spec coverage:** D0 (image_tokens + document_projection + estimator sum + PreviewResponse) = Tasks 1–5 ✓; D1 (rule + registration + D0 quote) = Tasks 6–7 ✓; D2 (pass + RouteAction + compute_cost_full wiring + migration 0031 + telemetry) = Tasks 8–12 ✓; key decisions (input-rate pricing, Gemini guard, opt-in lossless, no V4) all encoded ✓.

**Placeholders:** formula code, guard code, projection code are real; the two genuine impl-resolution points (model→formula prefixes; data-URL header-dims parsing) carry explicit algorithms + fallbacks, not "TBD". D1/D2's "read the sibling first" is a pattern-match instruction, not a placeholder — the interfaces + acceptance are concrete.

**Type consistency:** `estimate_image_tokens(model, w, h, ImageDetail)` / `ImageFormula` / `formula_for_model` are used consistently across Tasks 1–3; `document_projection::project(raw, distilled, price, model) -> DocProjection` consumed by Task 4 + Task 6; `doc_compaction`/`doc_compaction_tokens_removed`/`doc_compaction_saved_usd` consistent across Tasks 8–11.

# Content-Aware Compression — Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan slice-by-slice (one subagent per slice: P1a, then P1b, P1c, P1d). Steps use checkbox (`- [ ]`) syntax.

**Goal:** Ship a deterministic content-aware compression engine — a live `ContentClassifier` + a `content_compress` dispatcher routing JSON/log/prose/code blocks to specialized backends, wrapped in TT's token-true gate + judge + signed savings — plus a judge-labeled training flywheel for Phase 2.

**Architecture:** P1a lands the classifier + the opt-in `RouteAction.content_compress` dispatcher + a JSON/log/CSV structural backend + the isolated `content_compress_saved_est_usd` cost field + flywheel telemetry. P1b adds the prose extractive backend, P1c the AST code backend, P1d the dataset export. Every backend rides the existing pass token-true gate + the ~2% recall-of-baseline quality judge + the 0.90 auto-pause floor.

**Tech Stack:** Rust (`tt-core`, `tt-routing`, `tt-telemetry`, `tt-inspect-core` tree-sitter, `tt-plan-core`), sqlx migration, cargo test.

## Global Constraints (verbatim from the spec)
- **Opt-in, off by default** — `RouteAction.content_compress: bool` (mirror `compress`).
- **Moat-wrap** — every backend rides the token-true gate (reject token-growing transforms → verbatim); lossy backends are judge-sampled + 0.90 auto-pause (fail-open to verbatim).
- **Isolated attribution for lossy** — `CostBreakdown.content_compress_saved_est_usd` mirrors the ISOLATED `doc_vision_saved_est_usd` (D4a), NOT the baseline-folded `compression_saved_usd`. Threaded through EVERY `compute_cost_full`/`CostBreakdown{}`/sse/cache-hit site (grep-verified; `cargo test --workspace --no-run` proves it).
- **ZDR flywheel** — metrics/telemetry always; raw before/after pair capture is OPT-IN (`TT_COMPRESS_CAPTURE` / per-org flag), default OFF.
- **Migration slot 0033** (0031 D2, 0032 D4a — verify next-free at build).
- Public CI hard-gates `cargo fmt --all --check` + clippy; verify field-ripple with `cargo test --workspace --no-run`. Do NOT bump `rand`. Commit trailer required. NOTE: the workspace-test + cargo-deny CI checks can flake on runner disk / RustSec-DB fetch — that's infra, not code; re-run.

## File Structure
- Create `crates/core/src/content_compress/mod.rs` (+ `classify.rs`, later `prose.rs`, `code.rs`) — the engine; `pub mod content_compress;` in the core lib.
- Modify `crates/routing/src/lib.rs` — `RouteConditions.content_type` + `RouteAction.content_compress` + validator + runtime match.
- Modify `crates/core/src/routes/chat.rs` — dispatcher wiring in the pass region + the isolated `content_compress_saved_est_usd` (mirror `doc_vision_saved_est_usd`) + header.
- Create `crates/core/migrations/0033_content_compress_saved.{up,down}.sql`; modify `crates/telemetry/src/request_logs.rs`.

---

## SLICE P1a — classifier + dispatcher + JSON/log backend + isolated field + flywheel (THIS FIRST)

**Read first:** `crates/inspect-rules-tier1/src/rules/inline_data_offload_candidate.rs:215` (`classify_data_blob` — the heuristics to promote); how D4a threaded `doc_vision_saved_est_usd` (grep it in `chat.rs` — the exact 16 sites to mirror); `RouteAction.compress` + `RouteConditions.has_documents` in `routing/lib.rs`; the pass pipeline in `crates/core/src/passes/mod.rs` (the token-true gate + where a pass mutates the volatile-tail text).

### Task A1: `ContentClassifier`
**Files:** Create `crates/core/src/content_compress/mod.rs` + `content_compress/classify.rs`; `pub mod content_compress;` in the core lib.
**Produces:** `pub enum ContentKind { Json, Csv, Log, Code, Diff, Prose }` + `pub fn classify(block: &str) -> Option<ContentKind>` (None = too small / plain). Promote `classify_data_blob`'s heuristics + add Code/Diff/Prose.

- [ ] **Step 1: Write failing tests**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn json_block() { assert_eq!(classify(&format!("{{{}}}", "\"k\":1,".repeat(20))), Some(ContentKind::Json)); }
    #[test] fn log_block() { let l="2026-07-03 10:00:00 INFO x\n".repeat(12); assert_eq!(classify(&l), Some(ContentKind::Log)); }
    #[test] fn diff_block() { let d="@@ -1 +1 @@\n-old line here\n+new line here\n".repeat(6); assert_eq!(classify(&d), Some(ContentKind::Diff)); }
    #[test] fn code_block() { let c="fn a() {\n  let x = 1;\n}\n".repeat(20); assert_eq!(classify(&c), Some(ContentKind::Code)); }
    #[test] fn prose_block() { let p="The quick brown fox jumps over the lazy dog. ".repeat(40); assert_eq!(classify(&p), Some(ContentKind::Prose)); }
    #[test] fn tiny_is_none() { assert_eq!(classify("hi"), None); }
}
```
- [ ] **Step 2:** `cargo test -p tt-core content_compress::classify 2>&1 | tail -5` → FAIL.
- [ ] **Step 3: Implement** `classify.rs`. Port the JSON (structural-delimiter), CSV/TSV (consistent-delimiter), and log (level/ISO-timestamp majority) heuristics from `classify_data_blob`. ADD: **Diff** (≥ majority of lines start with `+`/`-`/`@@ `/`diff `/`index `); **Code** (fenced ` ``` ` OR a high density of code signals: `fn `/`def `/`function `/`class `/`import `/`#include `/`=>`/`;` line-endings + brace balance — a documented threshold); **Prose** (the fallback when a block is ≥ `MIN_BLOB_CHARS`, mostly sentences with spaces + terminal punctuation, and none of the above). Keep it allocation-light (single pass, no regex-per-line beyond the existing log regex). `MIN_BLOB_CHARS` const (e.g. 512).
- [ ] **Step 4:** run → PASS. **Step 5:** `cargo fmt -p tt-core` + clippy + commit (`feat(core): ContentClassifier for content-aware compression (P1a)`).

### Task A2: `RouteConditions.content_type` + `RouteAction.content_compress`
**Files:** `crates/routing/src/lib.rs` (mirror `has_documents`:89 + `compress`:190 patterns), `validate.rs`, plan-core mirror decision.
**Produces:** `RouteConditions.content_type: Option<String>` (the `ContentKind` as a lowercase string signal — routes can target `content_type=code`), runtime match against the request's dominant content kind; `RouteAction.content_compress: bool` (`#[serde(default, skip_serializing_if="std::ops::Not::not")]`, off by default) + `validate_route_has_effect` inclusion.
- [ ] TDD: `{"content_compress":true}` deserializes + default false + serialize-omits-when-false (mirror `compress` tests); a `content_type=code` condition matches a code-dominant request. plan-core: mirror `content_type` in `RouteConditions` (like `has_documents`), do NOT mirror `content_compress` (runtime-only, like `compress`). Commit.

### Task A3: the dispatcher + JSON/log/CSV structural backend
**Files:** `crates/core/src/content_compress/mod.rs` (the dispatcher + the structural backend) + wire into `chat.rs` pass region (where `compress`/`doc_compaction` run).
**Behavior:** when a route has `content_compress:true`, for each LARGE `ContentPart::Text` block (≥ `MIN_BLOB_CHARS`): `classify(block)` → `Json|Csv|Log` → structural compaction (collapse insignificant whitespace in JSON, drop repeated log prefixes / collapse repeated identical lines, trim trailing CSV padding — all content-preserving); `Code|Prose` → no-op in P1a (backends in P1b/P1c). Return the compacted text + `tokens_removed`. **Rides the existing token-true gate** (the pass pipeline rejects any token-growing result → verbatim).
- [ ] TDD: a JSON-dominant opted request is compacted (fewer tokens, same values); a log block collapses repeats; a prose/code block is untouched in P1a; a request WITHOUT the opt-in is byte-identical; token-true gate holds (a pathological block that would grow → verbatim). Commit.

### Task A4: isolated `content_compress_saved_est_usd` (mirror `doc_vision_saved_est_usd`)
**Files:** `chat.rs` — add `CostBreakdown.content_compress_saved_est_usd: f64`; **`grep doc_vision_saved_est_usd` and add `content_compress_saved_est_usd` at EVERY matching site** (the ~16 `CostBreakdown{}` initializers + `compute_cost_full` + sse + cache-hit synthesizers) — the D4a completeness discipline. Add `x-tokentrimmer-content-compress-saved-est-usd` header. Value = `tokens_removed × served input rate` on the compress path (0 elsewhere). Migration `0033_content_compress_saved` (`ADD COLUMN IF NOT EXISTS content_compress_saved_est_usd NUMERIC(12,6) NOT NULL DEFAULT 0`, mirror D4a's 0032) + `RequestLogRow` field + INSERT/bind/`INSERT_BIND_COUNT`.
- [ ] TDD: an opted compressed request books `content_compress_saved_est_usd` + emits the header; default request = 0; `grep doc_vision_saved_est_usd` == `grep content_compress_saved_est_usd` (completeness); `cargo test --workspace --no-run` EXIT 0; migration additive. Commit.

### Task A5: flywheel telemetry (metrics always; opt-in raw capture scaffold OFF)
**Files:** `crates/telemetry/src/request_logs.rs` (a `content_compress_kind` TEXT column via the SAME 0033 migration — or fold into A4) + a `content_compress::capture` scaffold gated by `TT_COMPRESS_CAPTURE` env (default off) that, when on, would emit `{content_type, tokens_before/after, transform}` (raw before/after DEFERRED to P1d — A5 ships only the metrics column + the OFF-by-default gate + a no-op sink).
- [ ] TDD: the content_compress kind is recorded on a compressed request; the capture gate is OFF by default (no raw text emitted); RequestLogRow round-trips. Commit.

### Task A6: P1a PR
- [ ] `cargo fmt -p tt-core -p tt-routing -p tt-telemetry`; clippy those `--all-targets -D warnings`; `cargo test` those; `cargo test --workspace --no-run`; push `feat/content-compress-p1a` (has the spec+plan); PR "feat(core,routing): P1a — content-aware compression classifier + dispatcher + isolated field (Phase 1)"; auto-merge. If workspace-test/cargo-deny CI flakes (disk / RustSec-DB), re-run.

---

## SLICE P1b — prose extractive compressor (off main after P1a)
New `content_compress/prose.rs`: extractive scoring (recency + BM25/keyword salience + shingle-dedup) to a target ratio; **must-keep hard override** regex allowlist (numbers, hex, ISO dates, paths, URLs, `--flags`, CamelCase/snake identifiers, code fences). LOSSY → judge-gated (reuse `quality_sample` + `route_autopause` 0.90 floor) → isolated saving. Dispatcher routes `Prose` here. TDD: target-ratio compression; must-keep survives; judge ≥0.90 keeps / <0.90 verbatim; small untouched; token-true gate. PR, auto-merge.

## SLICE P1c — AST code compressor (off main after P1b)
New `content_compress/code.rs`: reuse `crates/inspect-core` tree-sitter (`parse.rs`) — keep imports/signatures/type decls, statement-truncate bodies (`… // N lines elided`), **re-parse → return ORIGINAL if any ERROR/MISSING node** ("never serve broken code"). Judge-sampled. Dispatcher routes `Code`. TDD: signatures kept; truncated file re-parses clean; would-break → original; non-code untouched. PR, auto-merge.

## SLICE P1d — flywheel dataset export (off main after P1c)
Materialize the opt-in judge-labeled `{content_type, before, after, tokens, verdict}` pairs into a Phase-2 training corpus (a `tt` export command or a cloud path), ZDR-respecting (opted-in orgs only). This is the Phase-1→Phase-2 handoff. TDD: export produces well-formed pairs from the capture sink; refuses non-opted orgs.

---

## Self-Review
**Spec coverage:** classifier (A1) ✓; content_type+content_compress routing (A2) ✓; dispatcher+JSON/log backend (A3) ✓; isolated field+migration 0033+telemetry (A4) ✓; flywheel scaffold (A5) ✓; prose (P1b) ✓; code/AST (P1c) ✓; export (P1d) ✓; moat-wrap (token-true gate every backend, judge+0.90 on lossy) encoded in A3/P1b/P1c ✓; ZDR opt-in flywheel (A5/P1d) ✓.
**Placeholders:** the classifier code + tests are real; "mirror doc_vision_saved_est_usd / RouteAction.compress" are pattern-match instructions at named sites, not placeholders; migration slot 0033 pinned.
**Type consistency:** `ContentKind`/`classify` (A1) consumed by A2/A3; `RouteAction.content_compress`/`RouteConditions.content_type` (A2) by A3; `content_compress_saved_est_usd` (A4) consistent across sites; `content_compress/{classify,prose,code}` module paths consistent.

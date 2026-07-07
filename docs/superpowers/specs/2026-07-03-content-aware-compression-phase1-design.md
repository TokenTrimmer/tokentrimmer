# Content-Aware Compression — Phase 1 (deterministic engine + training flywheel)

**Date:** 2026-07-03 · **Repo:** `public/` (gateway core + routing) · **Roadmap context:** Phase 1 of the learned-compression program. Grounded in the `headroom-dense-vs-tt-strategy` synthesis (2026-07-03) + adversarial verification against live code.

> **Why this exists.** TT's headline `compression` pass is, by its own doc-comment, a "conservative, content-lossless trim of *non-prose*" — it moves ~0 billed tokens on a big prose blob or a source file. headroom (56k⭐, Apache-2.0) closes that gap with content-aware routing to specialized compressors. Phase 1 closes it in TT — **wrapped in TT's moat** (token-true gate + quality judge + 0.90 auto-pause + signed/reconciled savings), which headroom structurally lacks — and, critically, instruments a **judge-labeled training flywheel** so Phase 2 can fine-tune a compressor on data no competitor can see.

## Roadmap (this spec = Phase 1 only)
- **Phase 1 (this spec):** deterministic content-aware compressor + the flywheel. Ships the gap-closing savings now; produces the labeled dataset.
- **Phase 2 (separate spec):** fine-tune a *small, fast* selective-context compressor (LLMLingua-2 class) on Phase-1's judge-labeled traffic → ONNX → serve in-gateway via `ort` (the proven NeuroGlyph serving path). "Better than headroom's Kompress" because it's fast, Rust, judge-gated live, and trained on real quality-labeled data. NOTE: headroom's own trained model is too slow for prod and they fall back to a deterministic compressor — so Phase 1's deterministic engine is also the *production* engine; the model augments, never blocks.
- **Phase 3 (separate research spec):** the "AI's own language" frontier — a learned dense latent codec (gist-token/ICAE class, 10–100× ceiling) for **TT-hosted / open models where TT controls both ends** (third-party APIs accept text/tokens, not learned embeddings — a hard constraint), plus a learned *textual* shorthand for third-party APIs.

## Non-goals (Phase 1)
- No trained/ML model (Phase 2). No dense-latent "AI language" (Phase 3).
- No new lossy behavior that isn't judge-gated + fail-open. Off by default.
- Not touching the invoice-reconcilable headline for lossy transforms without the judge (lossless structural trims fold like `compression`; lossy prose/summarization rides the isolated + judge-gated path).

## Verified grounding (live code, 2026-07-03)
1. Offline classifier to promote: `classify_data_blob(content: &str) -> Option<&'static str>` at `crates/inspect-rules-tier1/src/rules/inline_data_offload_candidate.rs:215`.
2. AST substrate: `crates/inspect-core/src/{parse.rs,ast.rs}` (tree-sitter already a dep).
3. The honesty gap: `crates/core/src/passes/compression.rs:1` — "conservative, content-lossless trim of *non-prose*".
4. Existing content-drop lever: `RouteConditions`/`RouteAction` in `crates/routing/src/lib.rs` (`elide_stale_tools:414`; the `compress`/`doc_compaction`/`document_lane` opt-in pattern to mirror).
5. Moat machinery: the pass token-true gate (`crates/core/src/passes/mod.rs`), `crates/core/src/quality_sample.rs` (the ~2% paired recall-of-baseline judge), `route_autopause` (0.90 floor), the isolated-vs-baseline-fold `CostBreakdown` pattern (D2's `compression_saved_usd` fold; D4a's `doc_vision_saved_est_usd` isolated).

## Architecture
```
request → [ContentClassifier] tags each LARGE content block → [content_compress dispatcher]
             json/log → structural compaction (P1a)
             prose    → extractive compressor (P1b)
             code     → AST compressor (P1c)
          → token-true gate (reject any token-growing transform)
          → ~2% quality-sample judge + 0.90 auto-pause (fail-open to verbatim)
          → book saving (lossless→baseline fold; lossy→isolated + judge-gated) + header
          → flywheel: emit {content_type, tokens_before/after, transform, judge_verdict} (+ opt-in raw pair)
```
Opt-in behind a new `RouteAction.content_compress: bool` (off by default), same pattern as `compress`.

### Moat-wrap (the differentiator — every backend inherits it)
- **Token-true gate:** any backend whose output tokenizes larger than input is rejected → verbatim (structural losslessness for the lossless backends; a hard floor for the lossy ones).
- **Quality judge + 0.90 auto-pause:** lossy backends (prose extractive) are sampled by the paired recall-of-baseline judge; sustained sub-0.90 recall auto-pauses the lever per route (existing `route_autopause`).
- **Cost attribution:** lossless structural trims fold into `baseline_cost_usd` like `compression`; the lossy prose saving is booked as an **isolated `content_compress_saved_est_usd`** (mirror `doc_vision_saved_est_usd`) + a header, so the invoice-reconcilable headline stays clean.

### The flywheel (built into P1a; the Phase-2 enabler)
A `request_logs` record per compressed request: `content_compress_saved_est_usd` + `content_type` + `tokens_removed`. Plus an **opt-in, ZDR-respecting** raw-pair capture path (`TT_COMPRESS_CAPTURE` / per-org flag, default OFF) writing `{content_type, before, after, judge_verdict}` to a capture sink for Phase-2 training. Default posture = hashes/metrics only (matches TT's ZDR stance).

## Slices (each a PR, pure-public-crate)

### P1a — ContentClassifier + dispatcher + JSON/log backend + flywheel telemetry (THIS FIRST)
- **`ContentClassifier`** in `crates/core` (or a small `crates/content-classify`): promote `classify_data_blob` into a live, allocation-light classifier `classify(block: &str) -> ContentKind` where `ContentKind = {Json, Code, Prose, Log, Diff, Plain}`. Reuse the offline heuristics; do NOT call an LLM.
- **`RouteConditions.content_type: Option<ContentKind>`** signal + a runtime match (mirror `has_documents`), so routes can target content types; **`RouteAction.content_compress: bool`** opt-in (mirror `compress`, off by default) + validator + plan-core mirror decision (runtime-only like `compress` → NOT mirrored).
- **Dispatcher** in the pass pipeline: for each LARGE `ContentPart::Text` block on an opted-in route, classify → (P1a backend) JSON/log → structural compaction (extend the existing conservative trims); prose/code → no-op in P1a (backends land in P1b/P1c). Rides the token-true gate.
- **Isolated `CostBreakdown.content_compress_saved_est_usd`** (mirror `doc_vision_saved_est_usd`: threaded through EVERY `compute_cost_full`/`CostBreakdown{}`/sse/cache-hit site — the D2/D4a completeness discipline, `grep`-verified, `--workspace --no-run` proves it) + `x-tokentrimmer-content-compress-saved-est-usd` header + a `request_logs` migration (next free public-core slot — 0031 D2, 0032 D4a → **0033**) + `RequestLogRow` field + INSERT/bind/`INSERT_BIND_COUNT`.
- **Tests:** classifier per kind (json/code/prose/log/diff/plain); dispatcher compacts JSON on an opted route, no-ops on prose/code (P1a), no-ops off the route (default); token-true gate holds; isolated field threaded (completeness); migration additive.

### P1b — prose extractive compressor backend
- New `crates/core/src/passes/prose_compress.rs` (or a `content_compress/prose.rs`): extractive scoring — sentence/segment score = recency + BM25/keyword salience + shingle-dedup; drop lowest-scoring segments to a target ratio. **Must-keep hard overrides** (regex allowlist): numbers, hex, ISO dates, file paths, URLs, CLI flags (`--x`), CamelCase/snake identifiers, code fences — always retained regardless of score. LOSSY → gated by the quality judge + 0.90 floor; books the isolated saving. Dispatcher routes `Prose` here.
- **Tests:** compresses a large prose blob to ~target ratio; must-keep tokens survive; judge-gate path (mocked ≥0.90 keeps, <0.90 → verbatim); small blob untouched; token-true gate holds.

### P1c — AST code-compressor backend
- New `content_compress/code.rs`: reuse `inspect-core` tree-sitter (`parse.rs`) — keep imports/signatures/type decls, statement-truncate function bodies (`… // N lines elided`), **re-parse the result → if it has ERROR/MISSING nodes, return the ORIGINAL** ("never serve broken code"). Structurally content-lossful but syntactically valid → judge-sampled. Dispatcher routes `Code` here.
- **Tests:** compresses a large source file keeping signatures; a body-truncated file re-parses clean; a would-break transform returns the original; non-code untouched.

### P1d — flywheel dataset export
- A `tt` CLI + a cloud path (or a public export) that materializes the judge-labeled `{content_type, before, after, tokens, verdict}` pairs (from the opt-in capture sink) into a training corpus for Phase 2. ZDR-respecting (only opted-in orgs' pairs; default metrics-only). This is the Phase-1→Phase-2 handoff artifact.

## Risks & landmines
- **Hot-path cost:** the classifier runs per large block — must be allocation-light + early-return on small blocks (a size threshold const). Zero overhead for non-opted routes (the `content_compress` flag gates it).
- **Isolated-field completeness:** `content_compress_saved_est_usd` through every `compute_cost_full` site (D2/D4a discipline) or streaming/cache traffic silently drops the saving.
- **Lossy ≠ invoice-reconcilable:** the prose extractive saving is a counterfactual estimate → isolated + judge-gated + (later) signed as its own slice, never folded into the reconciled headline.
- **ZDR:** raw-pair capture is OPT-IN, default off; default is metrics/hashes only, matching TT's zero-data-retention posture.
- **Migration slot:** P1a uses **0033** (0031 D2, 0032 D4a). Verify next-free at build time.
- **`rand`:** not touched.

## Sequencing
P1a (foundation, off origin/main) → P1b (prose) → P1c (code) → P1d (export). Each its own PR. Spec committed with P1a.

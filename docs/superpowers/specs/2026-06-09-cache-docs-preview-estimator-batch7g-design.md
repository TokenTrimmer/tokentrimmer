# Cache-status doc accuracy + preview output-token estimator (batch 7g) — Design

**Status:** approved (working through remaining audit lows, 2026-06-09)
**Date:** 2026-06-09
**Slice:** Audit-remediation, public repo. Two clean lows: one docs/comment-accuracy fix across the gateway docs + both SDKs, one code fix in `crates/preview`.

## Fix 1 — `X-TokenTrimmer-Cache` documented/annotated values are wrong (docs/low)
Both SDKs annotate `cache` as `"hit-l1" | "hit-l2" | "miss" | "none" | "sandbox" | "bypass"`; the gateway API reference lists only `hit-l1 / hit-l2 / miss / none`.

**Verified ground truth in the gateway code** — the COMPLETE set of values inserted into the `x-tokentrimmer-cache` response header is:
- `hit-l1` (`chat.rs` `build_hit_l1_response`)
- `hit-l2` (`chat.rs` `build_hit_l2_response`)
- `neg-hit` (`chat.rs` negative-cache short-circuit, ~:1026)
- `miss` / `none` (`chat.rs` live path, ~:1515)
- `sandbox` (`chat.rs` `sandbox_response` :1769 + `embeddings.rs` :80, on `tt_test_*` keys)

So the finding's premise was partly wrong: `sandbox` **is** emitted (the SDKs were right to list it; the *docs* were missing it), and there is a value **neither** side documents — `neg-hit`. `bypass` is a request-side override (`X-TokenTrimmer-Cache` request header / `tt_extras.cache.mode`), never a response value.

**Fix:** make all three surfaces agree on the real emitted set `hit-l1 | hit-l2 | neg-hit | miss | none | sandbox`:
- `docs/04-gateway-api-reference.md` §6.2 response-header table: add `neg-hit` and `sandbox` to the `X-TokenTrimmer-Cache` example.
- `sdk-python/tokentrimmer/client.py` + `sdk-typescript/src/index.ts` cache annotations: add `neg-hit`, drop `bypass`.

## Fix 2 — preview output-token estimator hardcodes 512 default + 4096 cap (opportunity/low)
`token_estimator::estimate` computed `max_tokens_hint.unwrap_or(512).min(4096)`. The `.min(4096)` is applied **even to an explicit caller `max_tokens`**, so a caller who states they'll generate up to e.g. 8000 tokens has their output silently halved to 4096 — under-counting cost on the output side (~5× input price), the opposite of preview's accuracy goal.

**Fix:** thread the model's real catalog max-output into the estimator. `tt_shared::model_catalog().model_info(provider, model)` exposes `max_output_tokens`; `lib.rs::preview` already has both `hit.provider` and `req.model`. New `output_tokens(hint, model_max_output)` logic:
- An explicit `max_tokens` hint is the caller's stated ceiling and is **honored** (no more arbitrary 4096 cap).
- With no hint, assume `DEFAULT_OUTPUT_TOKENS = 512` (unchanged typical-short-completion heuristic).
- Either way, clamp to the model's catalog `max_output_tokens` **when known**, so the estimate never projects beyond what the model can actually emit. When the model isn't in the catalog, fall back to honoring the hint/default uncapped (better than a fictitious 4096).

The assumed output count is already surfaced to callers as `output_tokens_estimated` in the response, so no schema change is needed.

## Verification (to run)
- `cargo test -p tt-preview` — updated unit tests: explicit hint honored when model max unknown; hint clamped to model max when known; default clamped to a small model max; plus a `lib.rs` integration test driving `preview()` with a real catalog model + an over-large `max_tokens` and asserting `output_tokens_estimated` equals that model's catalog max.
- `cargo clippy -p tt-preview --all-targets` + `cargo fmt --check` clean.
- Docs/SDK comment edits are non-code; eyeball against the enumerated gateway emission sites.

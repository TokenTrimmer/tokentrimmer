# tt-client ergonomics + cost honesty (batch 7c) — Design

**Status:** approved (working through remaining audit lows, 2026-06-09)
**Date:** 2026-06-09
**Slice:** Audit-remediation, public repo, `crates/{shared,client}`. Two clean, fully-unit-testable lows. No behavior change to existing call paths (additive).

## Fix 1 — `ToolChoice` ergonomic constructors (dx/low)
Callers had to hand-build `ToolChoice::Auto("none".to_string())`; the valid strings (`auto`/`none`/`required`) lived only in a doc comment, easy to typo, and the SDK itself hand-built them in two places.
**Fix:** add associated constructors on `ToolChoice` in `tt-shared` (benefits all consumers): `auto()`, `none()`, `required()`, and `function(name)` (the `Specific{type:"function",...}` object form). Switch the two internal hand-build sites in `tt-client` to `ToolChoice::none()`. The untagged enum is unchanged, so wire compat is preserved. Test asserts each constructor serializes to the correct wire form (`"auto"`/`"none"`/`"required"` bare strings; `{type,function}` object).

## Fix 2 — `AggregateCost.savings_pct` can blend real + synthesized baselines (bug/low)
`AggregateCost::add` fell back to `baseline = cost + saved` when a round lacked the `x-tokentrimmer-baseline-cost-usd` header, so `savings_pct()` could silently mix true-baseline and synthesized-baseline rounds — while the single-shot `ChatOutcome::savings_pct` returns `None` on a missing baseline. The two surfaces disagreed on precision.
**Fix:** add a `baseline_estimated: bool` field (set when any round synthesized its baseline). `savings_pct()` still returns the value (non-breaking), but its doc + the field flag the result as approximate when `baseline_estimated`, so callers can choose not to over-trust the aggregate percentage. (Chose a non-breaking flag over making `savings_pct` return `None` — the synthesized `cost+saved` is a reasonable estimate, and an SDK return-type change for a low finding is disproportionate; the flag gives callers the same information.) Test covers all-real (flag false, exact sum) vs mixed (flag true, synthesized term, value still returned).

## Verification (done)
- `cargo test -p tt-shared -p tt-client` — 39 + 75 pass (incl. both new tests). `cargo clippy --all-targets` clean. `cargo fmt --check` clean.
- No struct-literal sites for `AggregateCost` (only `Default` + `add`), so the new field is ripple-free; `ToolChoice` change is purely additive.

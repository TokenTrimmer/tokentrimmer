# V4c — Auto-PR Model-Catalog Refresh Design

**Status:** approved (design)
**Date:** 2026-06-05
**Slice:** V4c (completes the V4 "live model catalog" area). Follows V4a (CLI consume #30) + V4b (catalog consolidation #31/#32).
**Depends on:** the `models.toml` + `ModelCatalog` from V4b.

## Goal

Keep model **context windows** in `models.toml` fresh automatically — but through a **reviewed PR**, not a runtime fetch. A scheduled CI job reconciles `models.toml` against OpenRouter's live models API and, on drift, opens a PR with the proposed change. This delivers "live" freshness while preserving the codebase's deliberate human-gated, auditable posture (every catalog change is a reviewed commit), exactly mirroring the existing `refresh-pricing.py` drift flow.

## Decisions (confirmed)

- **Approach:** auto-PR refresh (not runtime fetch). The gateway never depends on a remote catalog at runtime; the embedded `models.toml` stays the source, kept current by reviewed PRs.
- **Field scope:** **context windows only** (`max_input_tokens` vs OpenRouter `context_length`). This is a clean, reliable signal that genuinely changes as providers expand windows. **Capabilities stay manually curated** — OpenRouter's capability signals (`supported_parameters`, `architecture`) map fuzzily to our `Capability` enum and would produce churn/false drift. **Rates** keep the existing detect-only `refresh-pricing.py` flow (billing-critical, untouched here).

## Architecture

### `scripts/refresh-models.py` (new; mirrors `refresh-pricing.py`, stdlib only)
- **`SLUG`**: `(catalog provider, model) -> OpenRouter slug` for the rows OpenRouter authoritatively covers — the native first-party flagships (openai/anthropic/gemini, reusing the same slug forms as `refresh-pricing.py`). The `openrouter` provider rows map to **themselves** (their `model` id *is* the OpenRouter slug). groq/together/mistral-compat resale rows are absent → reported "manual" (not auto-checked).
- **`models_rows(path) -> list[dict]`**: parse `models.toml` (`tomllib`) → the `[[model]]` rows.
- **`fetch_openrouter() -> dict[str,int]`**: `GET https://openrouter.ai/api/v1/models` → `id -> context_length` (30s timeout; reused User-Agent header).
- **`detect_window_drift(rows, slug, ctx_by_slug) -> list[Drift]`** (pure, tested): for each row with a known slug present in the source, compare `max_input_tokens` vs `context_length`; emit `Drift(provider, model, current, proposed)` when they differ. Rows with no slug / not in source are skipped (reported "manual"/"missing" in the report, never drift).
- **`apply_window_fixes(toml_text, fixes) -> str`** (pure, tested): for each fix, locate that `[[model]]` block (matched by its `provider = "…"` + `model = "…"` lines) and replace **only** its `max_input_tokens = …` line — comment- and format-preserving (no toml re-serialization; `tomllib` is read-only and we avoid a writer dep).
- **CLI**: default = print a drift report and `exit(1)` if any drift (0 = clean, 2 = fetch failed) — same contract as `refresh-pricing.py`. `--write` = apply fixes to `models.toml` in place. `--json` = machine-readable report.

### `scripts/test_refresh_models.py` (new; stdlib `unittest`)
- `detect_window_drift`: a fixture catalog + a fixture OpenRouter map → expected drift list (a changed window detected; an equal window → none; an unmapped/missing model → none).
- `apply_window_fixes`: a sample `models.toml` snippet + a fix → the `max_input_tokens` line changed for the right block only, **comments and all other lines intact**, and it re-parses with `tomllib`.

### `.github/workflows/model-catalog-refresh.yml` (new)
- `on`: `schedule` (monthly cron, offset from the pricing-drift day) + `workflow_dispatch` + `pull_request` (paths: the script, the test, `models.toml`) to run the unit test as a smoke check.
- Scheduled/dispatch job: run `python3 scripts/refresh-models.py --write`; if `git diff --quiet` shows changes, open a PR with `peter-evans/create-pull-request@v6` (branch `chore/model-catalog-refresh`, a body summarizing the window deltas, labels). The PR runs the normal CI (build + the `model_catalog` tests) so a reviewer sees it green/red and merges.
- PR job: `python3 scripts/test_refresh_models.py` (+ a `--help`/parse smoke of the main script).

## Interaction with the pinned tests
The `model_catalog` spot-check tests pin some exact windows (e.g. `gemini-3.1-pro` 2_000_000). If a refresh PR changes such a window, that test goes red **in the PR** — which is the intended human checkpoint: the reviewer updates the pinned value alongside the data and merges. (Acceptable, and noted in the workflow PR body.)

## Testing
- `scripts/test_refresh_models.py` — the pure `detect_window_drift` + `apply_window_fixes` logic (no network).
- A local dry run (`python3 scripts/refresh-models.py` against live OpenRouter) is a manual sanity check, not CI-gated (network).
- No Rust changes, so the Rust suites are unaffected; `cargo` gates are a no-op for this slice beyond confirming nothing broke.

## Out of Scope
- **Runtime** catalog fetching / swapping the `OnceLock` (the explicitly-rejected approach).
- **Capabilities** auto-refresh (fuzzy mapping) and **rates** auto-refresh (billing-critical; keeps `refresh-pricing.py` detect-only).
- groq/together/mistral resale-window auto-checks (not cleanly on OpenRouter `/models`) — manual.
- Auto-**merging** refresh PRs (always human-reviewed).

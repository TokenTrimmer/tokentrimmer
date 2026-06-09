# inspect_diff unsupported-language reason + inspect-action output fix (batch 7h) — Design

**Status:** approved (working through remaining audit lows, 2026-06-09)
**Date:** 2026-06-09
**Slice:** Audit-remediation, public repo. Two clean inspect-related `gap/low`s, both in-sandbox verifiable.

## Fix 1 — `inspect_diff` silently returns `{findings: []}` for unsupported languages (gap/low, the open half of a PARTIAL)
`inspect_diff` writes the proposed content to a temp file (suffix from the caller's sanitized extension) and runs the engine. The engine's `walk()` filters out any file whose extension isn't `py / ts·tsx / js·jsx·mjs·cjs / md`, so a `.txt` (or extension-less) path yields zero findings with **no signal** — the calling agent can't tell "clean" from "not scanned". (The attacker-controlled-extension half was already fixed in #87 via `sanitize_ext`.)

**Fix:**
- Add `Language::from_extension(ext: &str) -> Option<Language>` to `tt-inspect-core` as the **single source of truth** for the extension→language mapping, and refactor `walk()` to call it (no behavior change — same mapping). This avoids duplicating the table in `inspect_diff` (the kind of silent drift batch 7e just guarded against for RouteConditions).
- In `inspect_diff`, resolve the language from the sanitized extension up front. If `None`, return an explicit, additive envelope instead of a bare empty list:
  `{ "findings": [], "scanned": false, "detected_language": null, "reason": "inspect does not scan '<path>' — supported: .py, .ts/.tsx, .js/.jsx/.mjs/.cjs, .md" }`.
  On a supported language, return `{ "findings": [...], "scanned": true, "detected_language": "python" }`. `findings` stays present and correct in both cases, so existing callers are unaffected (purely additive fields).

## Fix 2 — inspect-action: broken `status` output + false hosted-upload doc (gap/low)
`action.yml`'s `outputs.status.value` references `steps.run.outputs.tt-inspect-status`, but the "Run tt inspect" step has **no `id`**, so `steps.run` is undefined and `status` is always empty — a documented output contract that silently never resolves. Separately, README input `token` claims "results upload to the hosted dashboard," but the upload step only echoes "not yet wired" (the cloud backend hasn't shipped).

**Fix:**
- Add `id: run` to the "Run tt inspect" step so the `status` output resolves.
- Soften the README `token` description to state the hosted upload is **not yet wired (coming soon)**, matching the step's actual behavior, while keeping "Inspect runs fully local without one."
- (Left as a noted TODO, not changed: the `cargo install --git` cold build per run — switch to a binary download once release artifacts exist. Already flagged in `action.yml`.)

## Verification (to run)
- `cargo test -p tt-inspect-core -p tt-mcp` — existing tests + a new `from_extension` unit test + new async `inspect_diff` tests (unsupported `.txt` → `scanned:false` + reason; supported `.py` → `scanned:true` + `detected_language:"python"`).
- `cargo clippy -p tt-inspect-core -p tt-mcp --all-targets` + `cargo fmt --check` clean.
- `action.yml` / README are non-code; the `id: run` ↔ `steps.run` linkage is verified by inspection (and exercised whenever the action next runs in CI).

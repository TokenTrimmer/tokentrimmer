# SDK LICENSE files (batch 7f) — Design

**Status:** approved (working through remaining audit lows, 2026-06-09)
**Date:** 2026-06-09
**Slice:** Audit-remediation, public repo, `sdk-typescript/` + `sdk-python/`. One clean dx/low. Packaging-only — no source-code change.

## The finding (dx/low)
Both SDK packages declare `Apache-2.0` but ship no license text. `sdk-typescript/package.json` lists `"LICENSE"` in its `files` array, but no such file existed — so `npm publish` emits a dangling reference and omits it. `sdk-python` declared `license = { text = "Apache-2.0" }` and its sdist `include` listed only `tokentrimmer/`, `README.md`, `pyproject.toml` — shipping no license text in either the wheel or the sdist. Distributing an Apache-2.0 package without the license text is a compliance gap and an npm/PyPI best-practice violation.

**Verified the gap empirically** before fixing: a baseline `python -m build` produced a wheel with no `LICENSE` in `.dist-info/` and a sdist tarball with no `LICENSE` — only a `License: Apache-2.0` metadata string.

## The fix
1. Copy the repo-root Apache-2.0 `LICENSE` into both `sdk-typescript/LICENSE` and `sdk-python/LICENSE`.
2. Python `pyproject.toml`: switch `license = { text = "Apache-2.0" }` → `license = { file = "LICENSE" }` (hatchling then emits a `License-File: LICENSE` metadata entry and bundles the file into the wheel's `.dist-info/licenses/`), and add `"LICENSE"` to the sdist `include` array so source distributions carry it too.
3. TypeScript needs no `package.json` change — `files` already references `LICENSE`; the file just had to exist.

## Verification (done, with real builds)
- **Python** — `python -m build` (hatchling, isolated env): wheel now contains `tokentrimmer-0.1.0.dist-info/licenses/LICENSE` (11323 bytes), sdist contains `tokentrimmer-0.1.0/LICENSE`, and the wheel METADATA carries `License-File: LICENSE` + the OSI classifier.
- **TypeScript** — `npm pack --dry-run`: the tarball now lists `11.3kB LICENSE` (previously absent → dangling `files` reference resolved).
- No source files touched; no test impact.

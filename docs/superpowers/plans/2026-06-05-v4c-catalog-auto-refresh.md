# V4c — Auto-PR Model-Catalog Refresh Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A stdlib Python script that reconciles `models.toml` context windows against OpenRouter and rewrites drifted values, plus a scheduled workflow that opens a reviewed PR on drift.

**Architecture:** `scripts/refresh_models.py` (pure `detect_window_drift` + `apply_window_fixes` + a thin fetch/CLI), `scripts/test_refresh_models.py` (stdlib `unittest`, no network), and `.github/workflows/model-catalog-refresh.yml` (monthly → `gh`-opened PR; runs the unit test on PRs). No Rust changes.

**Tech Stack:** Python 3.11+ stdlib (`tomllib`, `urllib`, `re`), GitHub Actions, `gh` CLI.

---

### Task 1: `refresh_models.py` + unit tests (test-first)

**Files:**
- Create: `scripts/refresh_models.py`
- Create: `scripts/test_refresh_models.py`

- [ ] **Step 1: Write the failing tests**

Create `scripts/test_refresh_models.py`:

```python
"""Unit tests for refresh_models pure logic (no network). Run: python3 scripts/test_refresh_models.py"""
import sys
import tomllib
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from refresh_models import Drift, apply_window_fixes, detect_window_drift, slug_for  # noqa: E402


class TestSlug(unittest.TestCase):
    def test_openrouter_rows_map_to_themselves(self):
        self.assertEqual(slug_for("openrouter", "anthropic/claude-sonnet-4-6"), "anthropic/claude-sonnet-4-6")

    def test_native_uses_slug_map(self):
        self.assertEqual(slug_for("openai", "gpt-4o"), "openai/gpt-4o")

    def test_unmapped_is_none(self):
        self.assertIsNone(slug_for("groq", "mixtral-8x7b-32768"))


class TestDetect(unittest.TestCase):
    def test_detects_only_changed_known_models(self):
        rows = [
            {"provider": "openai", "model": "gpt-4o", "max_input_tokens": 128000},
            {"provider": "anthropic", "model": "claude-haiku-4-5", "max_input_tokens": 200000},
            {"provider": "groq", "model": "mixtral-8x7b-32768", "max_input_tokens": 32768},
        ]
        ctx = {"openai/gpt-4o": 130000, "anthropic/claude-haiku-4.5": 200000}
        drift = detect_window_drift(rows, ctx)
        self.assertEqual(drift, [Drift("openai", "gpt-4o", 128000, 130000)])

    def test_missing_from_source_is_not_drift(self):
        rows = [{"provider": "openai", "model": "gpt-4o", "max_input_tokens": 128000}]
        self.assertEqual(detect_window_drift(rows, {}), [])


class TestApply(unittest.TestCase):
    SAMPLE = (
        "# header comment\n\n"
        '[[model]]\nprovider = "openai"\nmodel = "gpt-4o"\n'
        'max_input_tokens = 128000\nmax_output_tokens = 16000\ncapabilities = ["text"]\n\n'
        '[[model]]\nprovider = "openai"\nmodel = "gpt-4o-mini"\n'
        'max_input_tokens = 128000\nmax_output_tokens = 16000\ncapabilities = ["text"]\n'
    )

    def test_rewrites_only_the_target_block_and_preserves_rest(self):
        out = apply_window_fixes(self.SAMPLE, [Drift("openai", "gpt-4o", 128000, 130000)])
        self.assertIn("max_input_tokens = 130000", out)
        self.assertEqual(out.count("max_input_tokens = 128000"), 1)  # only gpt-4o-mini left
        self.assertIn("# header comment", out)  # comments preserved
        parsed = tomllib.loads(out)
        self.assertEqual(parsed["model"][0]["max_input_tokens"], 130000)
        self.assertEqual(parsed["model"][1]["max_input_tokens"], 128000)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: Run to verify it fails**

Run: `python3 scripts/test_refresh_models.py 2>&1 | tail -8`
Expected: FAIL — `ModuleNotFoundError: No module named 'refresh_models'` (the script doesn't exist yet).

- [ ] **Step 3: Write the script**

Create `scripts/refresh_models.py`:

```python
#!/usr/bin/env python3
"""Model-catalog (context-window) drift detector + writer.

Reconciles `crates/shared/data/models.toml` context windows against OpenRouter's
live models API, mirroring `refresh-pricing.py`. Detects drift (report, exit 1),
or rewrites the drifted `max_input_tokens` lines in place with `--write` so the
model-catalog-refresh workflow can open a REVIEWED PR. Windows only —
capabilities stay manually curated; rates keep the detect-only refresh-pricing.py
flow (billing-critical).

Usage:
  python3 scripts/refresh_models.py           # report; exit 1 if drift
  python3 scripts/refresh_models.py --json     # machine-readable report
  python3 scripts/refresh_models.py --write    # rewrite models.toml in place

Exit codes: 0 = no drift (or --write applied); 1 = drift found (report mode);
2 = could not fetch the source. Stdlib only (urllib + tomllib, Python 3.11+).
"""
from __future__ import annotations

import json
import re
import sys
import tomllib
import urllib.request
from pathlib import Path

OPENROUTER_URL = "https://openrouter.ai/api/v1/models"
CATALOG = Path(__file__).resolve().parent.parent / "crates/shared/data/models.toml"

# (catalog provider, model) -> OpenRouter slug for rows OpenRouter authoritatively
# reports a context window for (native first-party flagships; same slug forms as
# refresh-pricing.py). The `openrouter` provider's rows map to themselves (their
# model id IS the slug). Rows absent here are reported "manual" (never drift).
SLUG = {
    ("openai", "gpt-5.5"): "openai/gpt-5.5",
    ("openai", "gpt-5.4"): "openai/gpt-5.4",
    ("openai", "gpt-4o"): "openai/gpt-4o",
    ("openai", "gpt-4o-mini"): "openai/gpt-4o-mini",
    ("openai", "o3"): "openai/o3",
    ("openai", "o4-mini"): "openai/o4-mini",
    ("anthropic", "claude-haiku-4-5"): "anthropic/claude-haiku-4.5",
    ("anthropic", "claude-sonnet-4-6"): "anthropic/claude-sonnet-4.6",
    ("anthropic", "claude-opus-4-7"): "anthropic/claude-opus-4.7",
    ("gemini", "gemini-3.5-flash"): "google/gemini-3.5-flash",
    ("gemini", "gemini-3.1-pro"): "google/gemini-3.1-pro-preview",
    ("gemini", "gemini-3.1-flash-lite"): "google/gemini-3.1-flash-lite",
}


class Drift:
    """A single context-window discrepancy."""

    def __init__(self, provider: str, model: str, current: int, proposed: int):
        self.provider = provider
        self.model = model
        self.current = current
        self.proposed = proposed

    def __eq__(self, other: object) -> bool:
        return isinstance(other, Drift) and (
            self.provider,
            self.model,
            self.current,
            self.proposed,
        ) == (other.provider, other.model, other.current, other.proposed)

    def __repr__(self) -> str:
        return f"Drift({self.provider}/{self.model}: {self.current} -> {self.proposed})"


def slug_for(provider: str, model: str) -> str | None:
    """OpenRouter slug for a catalog row, or None if not auto-checkable."""
    if provider == "openrouter":
        return model  # the id is already an OpenRouter slug
    return SLUG.get((provider, model))


def models_rows(path: Path) -> list[dict]:
    return tomllib.loads(path.read_text()).get("model", [])


def fetch_openrouter() -> dict[str, int]:
    """`id -> context_length` from OpenRouter's models API."""
    req = urllib.request.Request(OPENROUTER_URL, headers={"User-Agent": "tt-model-refresh"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        payload = json.load(resp)
    out: dict[str, int] = {}
    for m in payload.get("data", []):
        cl = m.get("context_length")
        if isinstance(cl, int) and cl > 0:
            out[m["id"]] = cl
    return out


def detect_window_drift(rows: list[dict], ctx_by_slug: dict[str, int]) -> list[Drift]:
    """Rows whose `max_input_tokens` differs from OpenRouter's `context_length`."""
    out: list[Drift] = []
    for r in rows:
        slug = slug_for(r["provider"], r["model"])
        if slug is None:
            continue
        ctx = ctx_by_slug.get(slug)
        if ctx is None:
            continue
        if int(r["max_input_tokens"]) != int(ctx):
            out.append(Drift(r["provider"], r["model"], int(r["max_input_tokens"]), int(ctx)))
    return out


def apply_window_fixes(toml_text: str, fixes: list[Drift]) -> str:
    """Rewrite each drifted model's `max_input_tokens` line in place (matched by
    provider+model within its `[[model]]` block); comment/format-preserving."""
    text = toml_text
    for fx in fixes:
        text = _replace_window(text, fx.provider, fx.model, fx.proposed)
    return text


def _replace_window(text: str, provider: str, model: str, new_val: int) -> str:
    # Split into parts that each START with a [[model]] header (lookahead keeps
    # the delimiter), so "".join(parts) reconstructs the file byte-for-byte.
    parts = re.split(r"(?m)(?=^\[\[model\]\]\s*$)", text)
    pat_p = re.compile(rf'(?m)^provider = "{re.escape(provider)}"\s*$')
    pat_m = re.compile(rf'(?m)^model = "{re.escape(model)}"\s*$')
    for i, part in enumerate(parts):
        if pat_p.search(part) and pat_m.search(part):
            parts[i] = re.sub(
                r"(?m)^(max_input_tokens = )\d+", rf"\g<1>{new_val}", part, count=1
            )
            break
    return "".join(parts)


def main(argv: list[str]) -> int:
    write = "--write" in argv
    as_json = "--json" in argv
    rows = models_rows(CATALOG)
    try:
        ctx = fetch_openrouter()
    except Exception as e:  # noqa: BLE001
        print(f"could not fetch OpenRouter models: {e}", file=sys.stderr)
        return 2

    drift = detect_window_drift(rows, ctx)

    if as_json:
        print(json.dumps([d.__dict__ for d in drift], indent=2))
    elif not drift:
        print("model catalog context windows: no drift vs OpenRouter")
    else:
        print(f"context-window drift ({len(drift)} model(s)):")
        for d in drift:
            print(f"  {d.provider}/{d.model}: {d.current} -> {d.proposed}")

    if write:
        if drift:
            CATALOG.write_text(apply_window_fixes(CATALOG.read_text(), drift))
            print(f"wrote {len(drift)} window fix(es) to {CATALOG}")
        return 0
    return 1 if drift else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
```

- [ ] **Step 4: Run to verify the tests pass**

Run: `python3 scripts/test_refresh_models.py 2>&1 | tail -8`
Expected: PASS (`Ran 6 tests ... OK`).

- [ ] **Step 5: `--help`/parse smoke (no network path executed)**

Run: `python3 -c "import ast; ast.parse(open('scripts/refresh_models.py').read()); print('parse ok')"`
Expected: `parse ok`.

- [ ] **Step 6: Commit**

```bash
chmod +x scripts/refresh_models.py
git add scripts/refresh_models.py scripts/test_refresh_models.py
git commit -m "feat(scripts): model-catalog context-window drift detector + writer"
```

---

### Task 2: The auto-PR workflow

**Files:**
- Create: `.github/workflows/model-catalog-refresh.yml`

- [ ] **Step 1: Write the workflow**

Create `.github/workflows/model-catalog-refresh.yml`:

```yaml
name: Model catalog refresh

# Monthly: reconcile models.toml context windows against OpenRouter's live
# models API (scripts/refresh_models.py) and open a REVIEWED PR on drift —
# preserving the human-gated, auditable catalog posture (the gateway never
# fetches a catalog at runtime). Also runs the script's unit tests on PRs.
on:
  schedule:
    - cron: '0 9 8 * *' # 09:00 UTC on the 8th (offset from pricing-drift on the 1st)
  workflow_dispatch:
  pull_request:
    paths:
      - 'scripts/refresh_models.py'
      - 'scripts/test_refresh_models.py'
      - 'crates/shared/data/models.toml'

permissions:
  contents: write
  pull-requests: write

jobs:
  test:
    name: refresh script unit tests
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with:
          python-version: '3.12' # tomllib needs >=3.11
      - run: python3 scripts/test_refresh_models.py

  refresh:
    name: open a PR if context windows drifted
    if: github.event_name != 'pull_request'
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with:
          python-version: '3.12'
      - name: Apply window updates and open/refresh a PR
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          python3 scripts/refresh_models.py --write
          if git diff --quiet; then
            echo "no context-window drift — nothing to do"
            exit 0
          fi
          git config user.name "github-actions[bot]"
          git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
          BRANCH="chore/model-catalog-refresh"
          git checkout -b "$BRANCH"
          git add crates/shared/data/models.toml
          git commit -m "chore: refresh model context windows from OpenRouter"
          git push -f -u origin "$BRANCH"
          gh pr view "$BRANCH" >/dev/null 2>&1 || gh pr create \
            --title "chore: refresh model context windows from OpenRouter" \
            --body "Automated context-window refresh (scripts/refresh_models.py) vs OpenRouter's live models API. Review the deltas; if a model_catalog spot-check test pins a changed window, update it in this PR. NOTE: a GITHUB_TOKEN-created PR does not auto-trigger CI — re-run checks or push an empty commit to validate." \
            --label automated
```

- [ ] **Step 2: Validate the YAML parses**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/model-catalog-refresh.yml')); print('yaml ok')" 2>/dev/null || python3 -c "import json; print('pyyaml absent — skip'); " ; echo done`
Expected: `yaml ok` (or a skip note if PyYAML isn't installed locally — GitHub validates on push regardless).

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/model-catalog-refresh.yml
git commit -m "ci: monthly model-catalog refresh opens a reviewed PR on window drift"
```

---

### Task 3: Gates + finish the branch

**Files:** none (verification only)

- [ ] **Step 1: Python tests + script parse**

Run: `python3 scripts/test_refresh_models.py && python3 -c "import ast; ast.parse(open('scripts/refresh_models.py').read()); print('parse ok')"`
Expected: tests OK + `parse ok`.

- [ ] **Step 2: Confirm no Rust regression (no Rust changed, but verify the tree builds)**

Run: `cargo build -p tt-shared 2>&1 | grep -E "^error" | head` (sanity — `models.toml` is unchanged in this slice, so this is a no-op confirmation)
Expected: no errors.

- [ ] **Step 3: Optional live dry-run (manual, network)**

Run (manual, not CI): `python3 scripts/refresh_models.py` — should print a drift report (or "no drift") against live OpenRouter. Confirms the fetch + mapping work end-to-end. Skip if offline.

- [ ] **Step 4: Finish the branch**

Use the **finishing-a-development-branch** skill: verify tests, push, open the PR.

---

## Self-Review

- **Spec coverage:** `refresh_models.py` (detect + write + report/exit contract) (T1), unit tests (T1), the monthly auto-PR workflow + PR-path unit-test job (T2), gates (T3). All spec items covered.
- **Placeholders:** none — the script, tests, and workflow are complete.
- **Type consistency:** `slug_for(str,str)->str|None`, `detect_window_drift(list[dict], dict[str,int])->list[Drift]`, `apply_window_fixes(str, list[Drift])->str`, `Drift(provider,model,current,proposed)` with value `__eq__`; the test imports exactly these. `--write` exits 0 (workflow gates the PR on `git diff`); report mode exits 1 on drift.
- **Scope:** windows only; `openrouter` rows self-map; unmapped rows never drift. No Rust changes, no runtime fetch, no auto-merge.
- **Note:** a GITHUB_TOKEN-created PR doesn't auto-trigger CI (documented in the PR body); the human reviewer re-runs checks — acceptable for a reviewed catalog change.

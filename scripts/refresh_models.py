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

#!/usr/bin/env python3
"""Pricing-catalog drift detector.

Reconciles `crates/shared/data/pricing.toml` against a live, machine-readable
pricing source (OpenRouter's models API) and prints a drift report. Billing is
critical, so this tool DETECTS drift — it does NOT edit the catalog. A human
reviews the report and appends a new dated `[[entry]]` per the catalog's
versioning rule (never edit historical rows).

Coverage:
  - OpenRouter authoritatively covers FIRST-PARTY model list prices
    (OpenAI / Anthropic / Google / Mistral) and the catalog's own `openrouter:*`
    rows. Those are verified here.
  - Host-specific resale rates (Groq, Together) and OpenAI embeddings are NOT on
    OpenRouter; they're reported as "manual" (verify against the host's own
    pricing page). A future pass can add per-host parsers.

Usage:
  python3 scripts/refresh-pricing.py            # report; exit 1 if drift found
  python3 scripts/refresh-pricing.py --json     # machine-readable report

Exit codes: 0 = no drift among automatically-verifiable rows; 1 = drift found;
2 = could not fetch the source.

Stdlib only (urllib + tomllib, Python 3.11+). Intended for a monthly CI cron.
"""
from __future__ import annotations

import json
import sys
import tomllib
import urllib.request
from pathlib import Path

OPENROUTER_URL = "https://openrouter.ai/api/v1/models"
CATALOG = Path(__file__).resolve().parent.parent / "crates/shared/data/pricing.toml"

# Per-1M-token absolute tolerance for float noise; anything larger is drift.
TOLERANCE_USD_PER_M = 0.005

# (catalog provider, catalog model) -> OpenRouter slug. Only rows OpenRouter
# authoritatively prices. Anthropic/Gemini version separators differ (dash vs
# dot). Rows absent here are reported "manual".
SLUG = {
    ("openai", "gpt-5.5"): "openai/gpt-5.5",
    ("openai", "gpt-5.4"): "openai/gpt-5.4",
    ("openai", "gpt-5.5-pro"): "openai/gpt-5.5-pro",
    ("openai", "gpt-5.4-mini"): "openai/gpt-5.4-mini",
    ("openai", "gpt-5.4-pro"): "openai/gpt-5.4-pro",
    ("openai", "gpt-4o"): "openai/gpt-4o",
    ("openai", "gpt-4o-mini"): "openai/gpt-4o-mini",
    ("openai", "o3"): "openai/o3",
    ("openai", "o4-mini"): "openai/o4-mini",
    ("anthropic", "claude-haiku-4-5"): "anthropic/claude-haiku-4.5",
    ("anthropic", "claude-sonnet-4-6"): "anthropic/claude-sonnet-4.6",
    ("anthropic", "claude-opus-4-7"): "anthropic/claude-opus-4.7",
    ("anthropic", "claude-opus-4-8"): "anthropic/claude-opus-4.8",
    ("gemini", "gemini-3.5-flash"): "google/gemini-3.5-flash",
    ("gemini", "gemini-3.1-pro"): "google/gemini-3.1-pro-preview",  # no stable slug yet
    ("gemini", "gemini-3.1-flash-lite"): "google/gemini-3.1-flash-lite",
    ("mistral", "mistral-large-latest"): "mistralai/mistral-large",
    # mistral-medium/small/codestral are only on OpenRouter as version-pinned
    # slugs (mistral-medium-3.1, codestral-2508, …) that rotate — mapping the
    # "-latest" alias to one would rot, so they're left to manual verification.
}


def latest_entries(catalog_path: Path) -> list[dict]:
    """The most-recent entry per (provider, model)."""
    data = tomllib.loads(catalog_path.read_text())
    by_key: dict[tuple[str, str], dict] = {}
    for e in data.get("entry", []):
        key = (e["provider"], e["model"])
        cur = by_key.get(key)
        if cur is None or e["effective_at"] > cur["effective_at"]:
            by_key[key] = e
    return [{"provider": k[0], "model": k[1], **v} for k, v in sorted(by_key.items())]


def fetch_openrouter() -> dict[str, dict]:
    req = urllib.request.Request(OPENROUTER_URL, headers={"User-Agent": "tt-pricing-refresh"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        payload = json.load(resp)
    out: dict[str, dict] = {}
    for m in payload.get("data", []):
        out[m["id"]] = m.get("pricing", {})
    return out


def per_m(token_price: str | float | None) -> float | None:
    """OpenRouter prices are USD per token (strings); convert to USD per 1M."""
    if token_price in (None, "", "0", 0):
        return 0.0 if token_price in ("0", 0) else None
    try:
        return float(token_price) * 1_000_000
    except (TypeError, ValueError):
        return None


def main() -> int:
    as_json = "--json" in sys.argv

    # The catalog's openrouter:* rows are priced directly by their slug.
    rows = latest_entries(CATALOG)
    slug_for = dict(SLUG)
    for r in rows:
        if r["provider"] == "openrouter":
            slug_for[(r["provider"], r["model"])] = r["model"]

    try:
        live = fetch_openrouter()
    except Exception as exc:  # noqa: BLE001
        print(f"ERROR: could not fetch {OPENROUTER_URL}: {exc}", file=sys.stderr)
        return 2

    report = {"matched": [], "drift": [], "missing_in_source": [], "manual": []}

    for r in rows:
        key = (r["provider"], r["model"])
        slug = slug_for.get(key)
        if slug is None:
            report["manual"].append({**_id(r), "reason": "no automated source (host-specific / embeddings)"})
            continue
        pricing = live.get(slug)
        if pricing is None:
            report["missing_in_source"].append({**_id(r), "slug": slug})
            continue
        live_in, live_out = per_m(pricing.get("prompt")), per_m(pricing.get("completion"))
        cat_in, cat_out = r["input_per_million"], r["output_per_million"]
        din = None if live_in is None else abs(live_in - cat_in)
        dout = None if live_out is None else abs(live_out - cat_out)
        drifted = (din is not None and din > TOLERANCE_USD_PER_M) or (
            dout is not None and dout > TOLERANCE_USD_PER_M
        )
        rec = {**_id(r), "slug": slug, "catalog": [cat_in, cat_out], "live": [live_in, live_out]}
        (report["drift"] if drifted else report["matched"]).append(rec)

    if as_json:
        print(json.dumps(report, indent=2))
    else:
        _print_human(report)

    return 1 if report["drift"] else 0


def _id(r: dict) -> dict:
    return {"provider": r["provider"], "model": r["model"]}


def _print_human(report: dict) -> None:
    def fmt(v):
        return "—" if v is None else f"${v:.4f}"

    print(f"Pricing drift report — {len(report['matched'])} matched, "
          f"{len(report['drift'])} DRIFT, {len(report['missing_in_source'])} missing, "
          f"{len(report['manual'])} manual\n")
    if report["drift"]:
        print("⚠ DRIFT (catalog ≠ live; append a dated entry after review):")
        for d in report["drift"]:
            ci, co = d["catalog"]
            li, lo = d["live"]
            print(f"  {d['provider']}:{d['model']}  in {fmt(ci)}→{fmt(li)}  out {fmt(co)}→{fmt(lo)}  [{d['slug']}]")
        print()
    if report["missing_in_source"]:
        print("? NOT IN SOURCE (model retired, or slug changed — check mapping):")
        for d in report["missing_in_source"]:
            print(f"  {d['provider']}:{d['model']}  (looked up {d['slug']})")
        print()
    if report["manual"]:
        print("○ MANUAL (no automated source — verify on the host's pricing page):")
        for d in report["manual"]:
            print(f"  {d['provider']}:{d['model']}")
        print()
    print(f"✓ {len(report['matched'])} rows match OpenRouter list price within "
          f"${TOLERANCE_USD_PER_M}/1M.")


if __name__ == "__main__":
    sys.exit(main())

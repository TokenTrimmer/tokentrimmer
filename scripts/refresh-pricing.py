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
    ("openai", "gpt-5.6-sol"): "openai/gpt-5.6-sol",
    ("openai", "gpt-5.6-terra"): "openai/gpt-5.6-terra",
    ("openai", "gpt-5.6-luna"): "openai/gpt-5.6-luna",
    ("openai", "gpt-5.5"): "openai/gpt-5.5",
    ("openai", "gpt-5.4"): "openai/gpt-5.4",
    ("openai", "gpt-5.5-pro"): "openai/gpt-5.5-pro",
    ("openai", "gpt-5.4-mini"): "openai/gpt-5.4-mini",
    ("openai", "gpt-5.4-pro"): "openai/gpt-5.4-pro",
    ("openai", "gpt-4.1"): "openai/gpt-4.1",
    ("openai", "gpt-4.1-mini"): "openai/gpt-4.1-mini",
    ("openai", "gpt-4o"): "openai/gpt-4o",
    ("openai", "gpt-4o-mini"): "openai/gpt-4o-mini",
    ("openai", "o3"): "openai/o3",
    ("openai", "o4-mini"): "openai/o4-mini",
    ("anthropic", "claude-sonnet-5"): "anthropic/claude-sonnet-5",
    ("anthropic", "claude-opus-5"): "anthropic/claude-opus-5",
    ("anthropic", "claude-haiku-4-5"): "anthropic/claude-haiku-4.5",
    ("anthropic", "claude-sonnet-4-6"): "anthropic/claude-sonnet-4.6",
    ("anthropic", "claude-opus-4-7"): "anthropic/claude-opus-4.7",
    ("anthropic", "claude-opus-4-8"): "anthropic/claude-opus-4.8",
    ("gemini", "gemini-3.7-flash"): "google/gemini-3.7-flash",
    ("gemini", "gemini-3.6-flash"): "google/gemini-3.6-flash",
    ("gemini", "gemini-3.5-flash-lite"): "google/gemini-3.5-flash-lite",
    ("gemini", "gemini-3.5-flash"): "google/gemini-3.5-flash",
    ("gemini", "gemini-3.1-pro"): "google/gemini-3.1-pro-preview",  # no stable slug yet
    ("gemini", "gemini-3.1-flash-lite"): "google/gemini-3.1-flash-lite",
    ("mistral", "mistral-large-latest"): "mistralai/mistral-large",
}

# Host-specific resale rates (Groq/Together) aren't in /models. OpenRouter's
# per-model `endpoints` API lists each host with its own price, so we verify
# those rows against the matching host endpoint.
# (catalog provider, model) -> (openrouter base slug, provider_name as OpenRouter shows it)
HOST_ENDPOINT = {
    ("groq", "llama-3.3-70b-versatile"): ("meta-llama/llama-3.3-70b-instruct", "Groq"),
    ("groq", "llama-3.1-8b-instant"): ("meta-llama/llama-3.1-8b-instruct", "Groq"),
    ("together", "meta-llama/Meta-Llama-3.3-70B-Instruct-Turbo"): (
        "meta-llama/llama-3.3-70b-instruct",
        "Together",
    ),
    # Retired Groq models + Together 405B/Qwen/DeepSeek have no matching host
    # endpoint on OpenRouter → reported "missing_in_source" (a signal to verify
    # on the host console / confirm the model is still offered).
    ("groq", "deepseek-r1-distill-llama-70b"): ("deepseek/deepseek-r1-distill-llama-70b", "Groq"),
    ("groq", "mixtral-8x7b-32768"): ("mistralai/mixtral-8x7b-instruct", "Groq"),
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


_ENDPOINT_CACHE: dict[str, list] = {}


def fetch_endpoints(base_slug: str) -> list:
    """OpenRouter per-model endpoints (each host + its pricing). Cached; empty on error."""
    if base_slug not in _ENDPOINT_CACHE:
        url = f"https://openrouter.ai/api/v1/models/{base_slug}/endpoints"
        try:
            req = urllib.request.Request(url, headers={"User-Agent": "tt-pricing-refresh"})
            with urllib.request.urlopen(req, timeout=30) as resp:
                _ENDPOINT_CACHE[base_slug] = json.load(resp).get("data", {}).get("endpoints", [])
        except Exception:  # noqa: BLE001
            _ENDPOINT_CACHE[base_slug] = []
    return _ENDPOINT_CACHE[base_slug]


def host_price(base_slug: str, provider_name: str) -> tuple[float | None, float | None] | None:
    """The (input, output) $/1M for `provider_name`'s endpoint of `base_slug`, or None."""
    for e in fetch_endpoints(base_slug):
        if e.get("provider_name") == provider_name:
            p = e.get("pricing", {})
            return per_m(p.get("prompt")), per_m(p.get("completion"))
    return None


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
        cat_in, cat_out = r["input_per_million"], r["output_per_million"]
        slug = slug_for.get(key)
        host = HOST_ENDPOINT.get(key)

        if slug is not None:  # first-party list price via /models
            pricing = live.get(slug)
            if pricing is None:
                report["missing_in_source"].append({**_id(r), "slug": slug})
                continue
            live_in, live_out = per_m(pricing.get("prompt")), per_m(pricing.get("completion"))
            src = slug
        elif host is not None:  # host-specific rate via the endpoints API
            hp = host_price(*host)
            src = f"{host[1]}@{host[0]}"
            if hp is None:
                report["missing_in_source"].append({**_id(r), "slug": src})
                continue
            live_in, live_out = hp
        else:
            report["manual"].append({**_id(r), "reason": "no automated source"})
            continue

        din = None if live_in is None else abs(live_in - cat_in)
        dout = None if live_out is None else abs(live_out - cat_out)
        drifted = (din is not None and din > TOLERANCE_USD_PER_M) or (
            dout is not None and dout > TOLERANCE_USD_PER_M
        )
        rec = {**_id(r), "slug": src, "catalog": [cat_in, cat_out], "live": [live_in, live_out]}
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

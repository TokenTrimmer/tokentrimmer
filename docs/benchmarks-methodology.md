# Benchmark methodology

The full, authoritative methodology for the **proxy-overhead benchmark** lives
next to the harness it documents, so the two never drift:

➡️ **[`scripts/benchmarks/README.md`](../scripts/benchmarks/README.md)**

It covers:

- **(a) What it measures** — proxy *overhead* against a null upstream, NOT
  provider latency and NOT realized cost savings.
- **(b) The mock-upstream design** — a zero-dependency OpenAI-compatible echo
  server that isolates dispatch cost.
- **(c) Honest framing** — TokenTrimmer's pitch is *savings* (caching + smart
  routing), not raw RPS; on a cache miss every gateway is additive and we say
  so; the cache-*hit* latency win is a savings feature, not a speed claim.
- **(d) Competitors** — LiteLLM is a pinned, run-it-yourself dockerized harness;
  Helicone (SaaS-only) and Bifrost (separate runtime) are documented limitations
  with **no fabricated numbers**.
- **(e) One-command reproduction** — `./scripts/benchmarks/run-all.sh`.

## Quick orientation

| Asset | Purpose |
|---|---|
| `scripts/benchmarks/run-all.sh` | one-command orchestrator (TokenTrimmer by default; `--with-litellm` adds the dockerized comparison) |
| `scripts/benchmarks/mock-upstream.py` | zero-dep OpenAI-compatible null upstream that isolates dispatch overhead |
| `scripts/benchmarks/tokentrimmer.sh` | spins up the gateway against the mock, drives oha, emits p50/p95/p99 + throughput CSV |
| `scripts/benchmarks/litellm/run.sh` | pinned, dockerized LiteLLM harness (run-it-yourself) |
| `scripts/benchmarks/results/baseline.csv` | committed, self-documenting baseline (machine/date labeled); per-run `results.csv` is gitignored |
| `.github/workflows/benchmark-nightly.yml` | non-blocking scheduled run; uploads CSV artifact; gates nothing |

This is a **manual / nightly** benchmark. It is deliberately NOT on the
PR-blocking CI path — see the README's "CI posture" section.

#!/usr/bin/env bash
# latency-smoke.sh — assert Gateway p50 latency stays under the spec budget.
#
# Spec budget (docs/04-gateway-api-reference.md):
#   - Cache miss:  p50 <  30 ms gateway overhead
#   - Cache hit:   p50 <   5 ms gateway overhead
#
# This is a RELEASE GATE, not a CI gate — it requires a live deployment and
# `oha` installed locally. Skips cleanly if either is missing so `make ci-local`
# can be run offline without false alarms.
#
# Usage:
#   TT_GATEWAY_URL=https://api.tokentrimmer.com  ./scripts/latency-smoke.sh
#   TT_GATEWAY_URL=http://localhost:8080         ./scripts/latency-smoke.sh   # docker-compose
#
# Environment:
#   TT_GATEWAY_URL  — base URL of the gateway to probe. Required to run.
#   TT_GATEWAY_KEY  — optional Bearer token. Not needed for /health.
#   TT_LATENCY_DURATION — duration of each pass, default 10s.
#   TT_LATENCY_CONCURRENCY — concurrent connections, default 10.
set -euo pipefail

cd "$(dirname "$0")/.."

URL="${TT_GATEWAY_URL:-}"
if [[ -z "${URL}" ]]; then
  echo "latency-smoke: TT_GATEWAY_URL not set — skipping (this is a release gate, not a CI gate)"
  exit 0
fi

if ! command -v oha >/dev/null 2>&1; then
  echo "latency-smoke: 'oha' not installed — skipping"
  echo "  install with: cargo install oha   (or: brew install oha)"
  exit 0
fi

DURATION="${TT_LATENCY_DURATION:-10s}"
CONCURRENCY="${TT_LATENCY_CONCURRENCY:-10}"

# Probe /health — pure gateway overhead (no provider call). Tight budget.
echo "==> /health (cache-miss equivalent — gateway overhead only)"
miss_out=$(oha --no-tui -z "${DURATION}" -c "${CONCURRENCY}" "${URL}/health" 2>&1)
echo "${miss_out}"

# Parse the p50 from oha's "Latency distribution" block. Format example:
#   50% in 0.0012 secs
miss_p50_secs=$(echo "${miss_out}" | awk '/50%/ {print $3; exit}')
if [[ -z "${miss_p50_secs}" ]]; then
  echo "ERROR: could not parse p50 from oha output"
  exit 2
fi
miss_p50_ms=$(awk -v s="${miss_p50_secs}" 'BEGIN {printf "%.2f", s * 1000}')

echo
echo "==> Result"
echo "  /health p50 = ${miss_p50_ms} ms (budget < 5 ms — pure gateway overhead, no provider)"

if awk -v ms="${miss_p50_ms}" 'BEGIN { exit !(ms > 5) }'; then
  echo "FAIL: p50 ${miss_p50_ms} ms exceeds 5 ms budget for cache-hit / no-provider path"
  exit 1
fi

echo "PASS"

# TODO(w12-l2-cache): once L1 cache wiring lands and a test API key works,
# add a second pass against POST /v1/chat/completions with a primed cache key
# to verify the cache-hit p50 < 5 ms and cache-miss p50 < 30 ms ends to end.

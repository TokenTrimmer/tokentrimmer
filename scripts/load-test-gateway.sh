#!/usr/bin/env bash
# load-test-gateway.sh — sustained load test against a running Gateway.
#
# Spec budget (docs/04-gateway-api-reference.md, latency targets):
#   - Cache miss p50:  < 30 ms gateway overhead
#   - Cache hit  p50:  <  5 ms gateway overhead
#
# Unlike latency-smoke.sh (one-shot sanity), this script drives sustained
# traffic and reports p50 / p95 / p99 / RPS. Run before tagging releases and
# whenever the request path changes.
#
# Skips cleanly if oha is missing or TT_GATEWAY_URL is unset.
#
# Usage:
#   TT_GATEWAY_URL=https://api.tokentrimmer.com \
#   TT_GATEWAY_KEY=tt_live_... \
#     ./scripts/load-test-gateway.sh
#
# Environment:
#   TT_GATEWAY_URL          base URL (required to run)
#   TT_GATEWAY_KEY          tt_live_*  (required for chat completions; optional for /health)
#   TT_GATEWAY_MODEL        default: "gpt-4o-mini" (must be registered in target gateway)
#   TT_LOAD_DURATION        default: "30s"
#   TT_LOAD_CONCURRENCY     default: "20"
#   TT_LOAD_FAIL_MISS_MS    default: "30"  (p50 miss budget in ms)
#   TT_LOAD_FAIL_HIT_MS     default: "5"   (p50 hit  budget in ms)
set -euo pipefail

cd "$(dirname "$0")/.."

URL="${TT_GATEWAY_URL:-}"
if [[ -z "${URL}" ]]; then
  echo "load-test-gateway: TT_GATEWAY_URL not set — skipping (this is a release gate)"
  exit 0
fi

if ! command -v oha >/dev/null 2>&1; then
  echo "load-test-gateway: 'oha' not installed — skipping"
  echo "  install: cargo install oha   (or: brew install oha)"
  exit 0
fi

DURATION="${TT_LOAD_DURATION:-30s}"
CONCURRENCY="${TT_LOAD_CONCURRENCY:-20}"
FAIL_MISS_MS="${TT_LOAD_FAIL_MISS_MS:-30}"
FAIL_HIT_MS="${TT_LOAD_FAIL_HIT_MS:-5}"
MODEL="${TT_GATEWAY_MODEL:-gpt-4o-mini}"

parse_p50_ms() {
  awk '/50%/ {printf "%.2f", $3 * 1000; exit}'
}

# --- Pass 1: /health — pure gateway overhead, no provider, no cache ---
echo "==> /health (pure gateway overhead, no provider call)"
out=$(oha --no-tui -z "${DURATION}" -c "${CONCURRENCY}" "${URL}/health" 2>&1)
echo "${out}" | tail -30
health_p50=$(echo "${out}" | parse_p50_ms)
echo "  /health p50 = ${health_p50} ms"
if awk -v ms="${health_p50}" -v b="${FAIL_HIT_MS}" 'BEGIN { exit !(ms > b) }'; then
  echo "FAIL: /health p50 ${health_p50} ms > ${FAIL_HIT_MS} ms budget"
  exit 1
fi
echo

# --- Pass 2: /v1/chat/completions cache MISS — primes the cache ---
if [[ -z "${TT_GATEWAY_KEY:-}" ]]; then
  echo "load-test-gateway: TT_GATEWAY_KEY not set — skipping chat-completion passes."
  echo "  /health p50 budget met; that's the limit of what we can probe anonymously."
  exit 0
fi

BODY=$(cat <<EOF
{"model":"${MODEL}","messages":[{"role":"user","content":"reply with the single word OK"}],"max_tokens":4}
EOF
)

echo "==> /v1/chat/completions cache MISS (model=${MODEL}, X-TokenTrimmer-Cache: bypass)"
out=$(oha --no-tui -z "${DURATION}" -c "${CONCURRENCY}" \
  -m POST \
  -H "Authorization: Bearer ${TT_GATEWAY_KEY}" \
  -H "Content-Type: application/json" \
  -H "X-TokenTrimmer-Cache: bypass" \
  -d "${BODY}" \
  "${URL}/v1/chat/completions" 2>&1)
echo "${out}" | tail -30
miss_p50=$(echo "${out}" | parse_p50_ms)
echo "  miss p50 = ${miss_p50} ms"
# Note: end-to-end p50 includes upstream provider latency. The 30ms budget is
# for gateway OVERHEAD; you'd need to subtract upstream_latency_ms from
# X-TokenTrimmer-Latency-Ms response header to get the true overhead. For a
# rough release gate, allow 10× the budget (300ms) since most of the time is
# provider time outside our control.
ROUGH_MISS_BUDGET=$((FAIL_MISS_MS * 10))
if awk -v ms="${miss_p50}" -v b="${ROUGH_MISS_BUDGET}" 'BEGIN { exit !(ms > b) }'; then
  echo "FAIL: chat-completion miss p50 ${miss_p50} ms > ${ROUGH_MISS_BUDGET} ms (10× budget)"
  exit 1
fi
echo

# --- Pass 3: /v1/chat/completions cache HIT — re-issue identical request ---
echo "==> /v1/chat/completions cache HIT (identical body re-issued)"
out=$(oha --no-tui -z "${DURATION}" -c "${CONCURRENCY}" \
  -m POST \
  -H "Authorization: Bearer ${TT_GATEWAY_KEY}" \
  -H "Content-Type: application/json" \
  -d "${BODY}" \
  "${URL}/v1/chat/completions" 2>&1)
echo "${out}" | tail -30
hit_p50=$(echo "${out}" | parse_p50_ms)
echo "  hit p50 = ${hit_p50} ms"
if awk -v ms="${hit_p50}" -v b="${FAIL_HIT_MS}" 'BEGIN { exit !(ms > b * 10) }'; then
  echo "FAIL: chat-completion hit p50 ${hit_p50} ms > ${FAIL_HIT_MS}0 ms (10× budget)"
  exit 1
fi

echo
echo "==> PASS"
echo "  /health p50      = ${health_p50} ms  (budget < ${FAIL_HIT_MS})"
echo "  chat miss p50    = ${miss_p50} ms     (rough 10× budget < ${ROUGH_MISS_BUDGET})"
echo "  chat hit  p50    = ${hit_p50} ms      (rough 10× budget)"
echo
echo "NOTE: this is end-to-end. To assert gateway-only overhead < ${FAIL_MISS_MS} ms,"
echo "      inspect X-TokenTrimmer-Latency-Ms in response headers and subtract upstream_latency_ms."

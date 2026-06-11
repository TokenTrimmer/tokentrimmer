#!/usr/bin/env bash
# tokentrimmer.sh — measure TokenTrimmer gateway proxy overhead vs a null upstream.
#
# WHAT THIS MEASURES
#   The latency the gateway ADDS on top of the upstream. We point the gateway's
#   upstream at scripts/benchmarks/mock-upstream.py (a fixed, ~0ms OpenAI-
#   compatible echo), so the p50/p95/p99 oha reports is dominated by the
#   gateway's own dispatch cost, not by a real model. See README.md.
#
# HOW
#   1. Build (or reuse) the `tt` release binary.
#   2. Start the mock upstream on 127.0.0.1:$MOCK_PORT.
#   3. Start `tt gateway` with TT_LOCAL_VLLM_URL pointed at the mock, no
#      DATABASE_URL (loopback bind, BYO-key passthrough — the gateway forwards
#      the caller's bearer to the mock, which ignores it).
#   4. Drive three oha passes (health, chat cache-miss, chat cache-hit) and
#      append rows to the results CSV.
#
# All TokenTrimmer numbers are REAL and locally measured. Provenance (machine +
# date) is written into the CSV header.
#
# USAGE
#   ./scripts/benchmarks/tokentrimmer.sh [results.csv]
#
# ENV (all optional)
#   BENCH_DURATION     oha -z per pass         default 20s
#   BENCH_CONCURRENCY  oha -c                   default 20
#   MOCK_PORT          mock upstream port       default 18000
#   GW_PORT            gateway port             default 18080
#   MOCK_DELAY_MS      mock think-time (ms)     default 0
#   TT_BIN             path to tt binary        default target/release/tt
#   SKIP_BUILD=1       reuse existing TT_BIN, don't cargo build
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
# shellcheck source=scripts/benchmarks/lib.sh disable=SC1091
source "$HERE/lib.sh"

DURATION="${BENCH_DURATION:-20s}"
CONCURRENCY="${BENCH_CONCURRENCY:-20}"
MOCK_PORT="${MOCK_PORT:-18000}"
GW_PORT="${GW_PORT:-18080}"
MOCK_DELAY_MS="${MOCK_DELAY_MS:-0}"
TT_BIN="${TT_BIN:-$REPO_ROOT/target/release/tt}"
CSV="${1:-$HERE/results/tokentrimmer.csv}"

if ! bench_require oha python3 curl; then
  echo "tokentrimmer.sh: install missing tools and retry" >&2
  echo "  oha: brew install oha   (or: cargo install oha)" >&2
  exit 1
fi

# --- build (unless reusing) --------------------------------------------------
if [[ "${SKIP_BUILD:-0}" != "1" && ! -x "$TT_BIN" ]]; then
  echo "==> building tt (release)…"
  ( cd "$REPO_ROOT" && cargo build --release -p tt-cli --bin tt )
fi
if [[ ! -x "$TT_BIN" ]]; then
  echo "tokentrimmer.sh: tt binary not found at $TT_BIN (set TT_BIN or unset SKIP_BUILD)" >&2
  exit 1
fi

MOCK_PID=""
GW_PID=""
cleanup() {
  [[ -n "$GW_PID" ]] && kill "$GW_PID" 2>/dev/null || true
  [[ -n "$MOCK_PID" ]] && kill "$MOCK_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# --- start mock upstream -----------------------------------------------------
echo "==> starting mock upstream on 127.0.0.1:$MOCK_PORT (delay=${MOCK_DELAY_MS}ms)"
MOCK_PID="$(bench_start_mock 127.0.0.1 "$MOCK_PORT" "$MOCK_DELAY_MS")"
bench_wait_http "http://127.0.0.1:$MOCK_PORT/health"

# --- start gateway -----------------------------------------------------------
# No DATABASE_URL/REDIS_URL → loopback bind, no persistence, BYO-key passthrough.
# TT_LOCAL_VLLM_URL registers the `vllm` local provider (allow_local=true) so a
# `vllm/<model>` request id dispatches to our localhost mock.
echo "==> starting tt gateway on 127.0.0.1:$GW_PORT → upstream 127.0.0.1:$MOCK_PORT"
env -u DATABASE_URL -u REDIS_URL \
  PORT="$GW_PORT" \
  TT_BIND_ADDR="127.0.0.1" \
  TT_LOCAL_VLLM_URL="http://127.0.0.1:$MOCK_PORT/v1" \
  RUST_LOG="error" \
  "$TT_BIN" gateway >/tmp/tt-bench-gateway.log 2>&1 &
GW_PID="$!"
if ! bench_wait_http "http://127.0.0.1:$GW_PORT/health"; then
  echo "tokentrimmer.sh: gateway did not come up — last log lines:" >&2
  tail -20 /tmp/tt-bench-gateway.log >&2 || true
  exit 1
fi

MODEL="vllm/mock-model"
BODY="$(bench_request_body "$MODEL")"
GW="http://127.0.0.1:$GW_PORT"

# Provenance header for this run's CSV.
{ bench_provenance; } >>"$CSV.prov"

# Sanity: confirm one real chat request round-trips through gateway→mock.
probe=$(curl -fsS -X POST "$GW/v1/chat/completions" \
  -H "Authorization: Bearer sk-mock-anything" \
  -H "Content-Type: application/json" \
  -d "$BODY" 2>/dev/null || true)
if ! echo "$probe" | grep -q '"chat.completion"'; then
  echo "tokentrimmer.sh: warm-up chat request did not return a completion." >&2
  echo "  response: $probe" >&2
  echo "  gateway log tail:" >&2
  tail -20 /tmp/tt-bench-gateway.log >&2 || true
  exit 1
fi
echo "==> warm-up OK — gateway is proxying to the mock upstream"

echo
echo "==> oha passes (duration=$DURATION concurrency=$CONCURRENCY)"

# Pass 1 — /health: pure gateway overhead, no provider call, no body.
bench_oha_csv "tokentrimmer:health" GET "$GW/health" \
  "$DURATION" "$CONCURRENCY" "$CSV" ""

# Pass 2 — chat cache MISS: full dispatch path to the (null) upstream.
bench_oha_csv "tokentrimmer:chat-miss" POST "$GW/v1/chat/completions" \
  "$DURATION" "$CONCURRENCY" "$CSV" "$BODY" \
  -H "Authorization: Bearer sk-mock-anything" \
  -H "Content-Type: application/json" \
  -H "X-TokenTrimmer-Cache: bypass"

# Pass 3 — chat cache HIT: identical body re-issued. With no Redis the L1 cache
# is disabled, so this is effectively a second cache-miss pass — it shows the
# steady-state miss path, NOT a cache-hit win. Run with REDIS_URL set to a real
# Redis to measure the cache-hit path (see README "cache-hit caveat").
bench_oha_csv "tokentrimmer:chat-hit" POST "$GW/v1/chat/completions" \
  "$DURATION" "$CONCURRENCY" "$CSV" "$BODY" \
  -H "Authorization: Bearer sk-mock-anything" \
  -H "Content-Type: application/json"

echo
echo "==> wrote $CSV"
echo "    (TokenTrimmer rows are real, locally measured — provenance in $CSV.prov)"

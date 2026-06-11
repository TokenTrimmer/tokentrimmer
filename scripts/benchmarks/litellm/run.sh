#!/usr/bin/env bash
# litellm/run.sh — RUN-IT-YOURSELF LiteLLM proxy-overhead harness.
#
# WHY THIS IS NOT AUTO-RUN
#   LiteLLM ships as a Docker image. Pulling + booting it (~hundreds of MB) is
#   too heavy and network-dependent to wire into the default benchmark, and we
#   do not commit competitor numbers measured on someone else's hardware. So
#   this is a documented, version-pinned script YOU run locally to reproduce a
#   like-for-like comparison against the TokenTrimmer harness.
#
# WHAT IT DOES (mirrors tokentrimmer.sh exactly)
#   1. Start scripts/benchmarks/mock-upstream.py on the host.
#   2. Boot the pinned LiteLLM proxy in Docker, upstream → the mock.
#   3. Drive the SAME oha workload (same body, duration, concurrency) and append
#      rows to the results CSV.
#
# These numbers ARE comparable to the TokenTrimmer rows because both proxies sit
# in front of the identical null upstream on the same machine, same load.
#
# USAGE
#   ./scripts/benchmarks/litellm/run.sh [results.csv]
#
# ENV (optional)
#   LITELLM_IMAGE      pinned image    default ghcr.io/berriai/litellm:main-v1.61.20-stable
#   BENCH_DURATION     oha -z          default 20s
#   BENCH_CONCURRENCY  oha -c          default 20
#   MOCK_PORT          mock port       default 18001
#   LITELLM_PORT       proxy port      default 14000
#   MOCK_DELAY_MS      mock delay      default 0
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(cd "$HERE/.." && pwd)"
# shellcheck source=scripts/benchmarks/lib.sh disable=SC1091
source "$BENCH_DIR/lib.sh"

# Pinned LiteLLM image. Bump deliberately and re-run; never float to :latest or
# results stop being reproducible.
LITELLM_IMAGE="${LITELLM_IMAGE:-ghcr.io/berriai/litellm:main-v1.61.20-stable}"
DURATION="${BENCH_DURATION:-20s}"
CONCURRENCY="${BENCH_CONCURRENCY:-20}"
MOCK_PORT="${MOCK_PORT:-18001}"
LITELLM_PORT="${LITELLM_PORT:-14000}"
MOCK_DELAY_MS="${MOCK_DELAY_MS:-0}"
CSV="${1:-$BENCH_DIR/results/litellm.csv}"
CONTAINER="tt-bench-litellm"

if ! bench_require oha python3 curl docker; then
  echo "litellm/run.sh: install missing tools and retry" >&2
  echo "  docker: https://docs.docker.com/get-docker/" >&2
  exit 1
fi

MOCK_PID=""
cleanup() {
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  [[ -n "$MOCK_PID" ]] && kill "$MOCK_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# --- mock upstream (on host) -------------------------------------------------
echo "==> starting mock upstream on 127.0.0.1:$MOCK_PORT (delay=${MOCK_DELAY_MS}ms)"
MOCK_PID="$(bench_start_mock 127.0.0.1 "$MOCK_PORT" "$MOCK_DELAY_MS")"
bench_wait_http "http://127.0.0.1:$MOCK_PORT/health"

# Container reaches the host mock via host.docker.internal (Docker Desktop) or
# the host-gateway alias (Linux, added explicitly below).
MOCK_FROM_CONTAINER="http://host.docker.internal:$MOCK_PORT/v1"

# --- boot LiteLLM ------------------------------------------------------------
echo "==> pulling + starting LiteLLM ($LITELLM_IMAGE) on :$LITELLM_PORT"
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker run -d --name "$CONTAINER" \
  --add-host=host.docker.internal:host-gateway \
  -p "$LITELLM_PORT:4000" \
  -e "LITELLM_MOCK_BASE=$MOCK_FROM_CONTAINER" \
  -v "$HERE/config.yaml:/app/config.yaml:ro" \
  "$LITELLM_IMAGE" \
  --config /app/config.yaml --port 4000 >/dev/null

if ! bench_wait_http "http://127.0.0.1:$LITELLM_PORT/health/liveliness"; then
  echo "litellm/run.sh: proxy did not come up — last container logs:" >&2
  docker logs --tail 30 "$CONTAINER" 2>&1 >&2 || true
  exit 1
fi

MODEL="mock-model"
BODY="$(bench_request_body "$MODEL")"
LL="http://127.0.0.1:$LITELLM_PORT"

# Warm-up sanity round-trip.
probe=$(curl -fsS -X POST "$LL/v1/chat/completions" \
  -H "Authorization: Bearer sk-mock-anything" \
  -H "Content-Type: application/json" \
  -d "$BODY" 2>/dev/null || true)
if ! echo "$probe" | grep -q '"chat.completion"'; then
  echo "litellm/run.sh: warm-up chat request did not return a completion." >&2
  echo "  response: $probe" >&2
  docker logs --tail 30 "$CONTAINER" 2>&1 >&2 || true
  exit 1
fi
echo "==> warm-up OK — LiteLLM is proxying to the mock upstream"

{ bench_provenance; echo "# litellm_image: $LITELLM_IMAGE"; } >>"$CSV.prov"

echo
echo "==> oha passes (duration=$DURATION concurrency=$CONCURRENCY)"

bench_oha_csv "litellm:health" GET "$LL/health/liveliness" \
  "$DURATION" "$CONCURRENCY" "$CSV" ""

bench_oha_csv "litellm:chat" POST "$LL/v1/chat/completions" \
  "$DURATION" "$CONCURRENCY" "$CSV" "$BODY" \
  -H "Authorization: Bearer sk-mock-anything" \
  -H "Content-Type: application/json"

echo
echo "==> wrote $CSV (provenance + pinned image in $CSV.prov)"

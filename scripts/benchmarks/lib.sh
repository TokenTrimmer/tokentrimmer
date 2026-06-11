#!/usr/bin/env bash
# lib.sh — shared helpers for the proxy-overhead benchmark harness.
#
# Sourced by the per-proxy harness scripts (tokentrimmer.sh, litellm/run.sh)
# and run-all.sh. Provides: dependency checks, the mock-upstream lifecycle,
# an oha→CSV driver, and a single canonical request body so every proxy is
# measured on an identical workload.
#
# Not executable on its own. shellcheck-clean (shellcheck source=/dev/null).

# --- canonical workload ------------------------------------------------------

# Identical for every proxy. Tiny prompt + max_tokens so the mock's fixed reply
# dominates nothing — we are measuring dispatch, not generation.
bench_request_body() {
  local model="$1"
  printf '{"model":"%s","messages":[{"role":"user","content":"reply with the single word OK"}],"max_tokens":4}' "$model"
}

# --- dependency checks -------------------------------------------------------

bench_require() {
  local missing=0
  for tool in "$@"; do
    if ! command -v "$tool" >/dev/null 2>&1; then
      echo "benchmark: required tool '$tool' not found on PATH" >&2
      missing=1
    fi
  done
  return "$missing"
}

# --- mock upstream lifecycle -------------------------------------------------

# Start the mock upstream in the background. Echoes the PID on stdout.
# Args: <host> <port> [delay_ms]
#
# IMPORTANT: the child's stdout+stderr are redirected to a log file. If the
# backgrounded python inherited this function's stdout, the surrounding
# `pid=$(bench_start_mock …)` command substitution would block forever waiting
# for that pipe to close (the server never exits). Redirecting fixes that.
bench_start_mock() {
  local host="$1" port="$2" delay_ms="${3:-0}"
  local here log
  here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  log="${BENCH_MOCK_LOG:-/tmp/tt-bench-mock-$port.log}"
  MOCK_HOST="$host" MOCK_PORT="$port" MOCK_DELAY_MS="$delay_ms" \
    python3 "$here/mock-upstream.py" >"$log" 2>&1 &
  echo "$!"
}

# Block until <url> answers 200, or fail after ~10s.
bench_wait_http() {
  local url="$1" tries=0
  until curl -fsS -o /dev/null "$url" 2>/dev/null; do
    tries=$((tries + 1))
    if [[ "$tries" -ge 100 ]]; then
      echo "benchmark: timed out waiting for $url" >&2
      return 1
    fi
    sleep 0.1
  done
  return 0
}

# --- oha → CSV ---------------------------------------------------------------

# Run one oha pass and append a CSV row. oha emits seconds; we convert to ms.
# Args:
#   $1 label         human row label (e.g. "tokentrimmer:chat-miss")
#   $2 method        GET|POST
#   $3 url
#   $4 duration      oha -z value (e.g. "20s")
#   $5 concurrency   oha -c value
#   $6 csv_path      output CSV (header written if absent)
#   $7 body          request body for POST ("" for GET)
#   $@ (8+)          extra oha args, e.g. header flags -H "..."
#
# CSV columns: timestamp,label,requests,rps,p50_ms,p95_ms,p99_ms,success_rate
bench_oha_csv() {
  local label="$1" method="$2" url="$3" duration="$4" concurrency="$5" csv_path="$6" body="$7"
  shift 7
  local extra=("$@")

  if [[ ! -f "$csv_path" ]]; then
    echo "timestamp,label,requests,rps,p50_ms,p95_ms,p99_ms,success_rate" >"$csv_path"
  fi

  # "${extra[@]+...}" guard: expanding an empty array under `set -u` errors on
  # bash 3.2 (macOS default). This idiom yields nothing when extra is empty.
  local json
  if [[ "$method" == "POST" ]]; then
    json=$(oha --no-tui --output-format json -z "$duration" -c "$concurrency" \
      -m POST -d "$body" ${extra[@]+"${extra[@]}"} "$url")
  else
    json=$(oha --no-tui --output-format json -z "$duration" -c "$concurrency" \
      ${extra[@]+"${extra[@]}"} "$url")
  fi

  # Parse with python3 (stdlib json) — no jq dependency. Convert s→ms.
  # oha JSON is passed as argv[3] (a herestring + heredoc would compete for
  # stdin; SC2261). json.loads on the arg keeps it to one redirection.
  python3 - "$label" "$csv_path" "$json" <<'PY'
import json, sys, datetime
label, csv_path = sys.argv[1], sys.argv[2]
data = json.loads(sys.argv[3])
s = data["summary"]
p = data["latencyPercentiles"]
row = [
    datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
    label,
    str(int(s.get("requestsPerSec", 0) * s.get("total", 0))),
    f'{s.get("requestsPerSec", 0):.1f}',
    f'{p.get("p50", 0) * 1000:.3f}',
    f'{p.get("p95", 0) * 1000:.3f}',
    f'{p.get("p99", 0) * 1000:.3f}',
    f'{s.get("successRate", 0):.4f}',
]
with open(csv_path, "a", encoding="utf-8") as fh:
    fh.write(",".join(row) + "\n")
print(f'  {label:32s} p50={float(row[4]):8.3f}ms  p95={float(row[5]):8.3f}ms  '
      f'p99={float(row[6]):8.3f}ms  rps={row[3]:>9s}  ok={row[7]}')
PY
}

# Print the machine/date provenance line that prefixes every result file.
bench_provenance() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  echo "# machine: ${os}/${arch}  date: $(date -u +%Y-%m-%dT%H:%M:%SZ)  oha: $(oha --version 2>/dev/null | awk '{print $2}')"
}

#!/usr/bin/env bash
# run-all.sh — one-command orchestrator for the proxy-overhead benchmark suite.
#
# Runs every harness we can drive locally and collects rows into a single CSV:
#   scripts/benchmarks/results/results.csv
#
# By DEFAULT it runs only the TokenTrimmer harness (no external dependencies
# beyond `oha` + `python3` + the `tt` binary). LiteLLM requires Docker and a
# large image pull, so it is OPT-IN via --with-litellm.
#
# Helicone and Bifrost are NOT auto-benchmarked — see README.md "Competitors we
# do not auto-benchmark" for the honest reason (Helicone is SaaS-only; Bifrost
# needs a separate runtime). run-all writes a documented limitation row for each
# rather than ever fabricating a number.
#
# USAGE
#   ./scripts/benchmarks/run-all.sh                 # TokenTrimmer only
#   ./scripts/benchmarks/run-all.sh --with-litellm  # + dockerized LiteLLM
#   ./scripts/benchmarks/run-all.sh --quick         # 5s passes for a smoke run
#
# ENV passthrough: BENCH_DURATION, BENCH_CONCURRENCY, MOCK_DELAY_MS, etc. (see
# the individual harness scripts).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

WITH_LITELLM=0
for arg in "$@"; do
  case "$arg" in
    --with-litellm) WITH_LITELLM=1 ;;
    --quick) export BENCH_DURATION="${BENCH_DURATION:-5s}" ;;
    -h|--help)
      sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "run-all.sh: unknown arg '$arg' (try --help)" >&2
      exit 2
      ;;
  esac
done

RESULTS="$HERE/results/results.csv"
mkdir -p "$HERE/results"
# Fresh file each run so stale competitor rows can't linger. Provenance lands in
# the sibling .prov file written by each harness.
rm -f "$RESULTS" "$RESULTS.prov"

echo "############################################################"
echo "# TokenTrimmer proxy-overhead benchmark — run-all"
echo "# measures proxy OVERHEAD vs a null mock upstream, not provider latency."
echo "# read scripts/benchmarks/README.md before quoting any number."
echo "############################################################"
echo

echo "==> [1] TokenTrimmer harness"
"$HERE/tokentrimmer.sh" "$RESULTS"
echo

if [[ "$WITH_LITELLM" == "1" ]]; then
  echo "==> [2] LiteLLM harness (dockerized)"
  "$HERE/litellm/run.sh" "$RESULTS"
  echo
else
  echo "==> [2] LiteLLM harness — SKIPPED (run with --with-litellm; needs Docker)"
  echo "        reproduce: ./scripts/benchmarks/litellm/run.sh $RESULTS"
  echo
fi

# Honest, non-fabricated limitation rows for proxies we can't fairly run locally.
{
  echo "# --- documented limitations (NOT benchmarked, NOT fabricated) ---"
  echo "# helicone,SaaS-only,no self-host overhead measurement is apples-to-apples; see README"
  echo "# bifrost,separate-runtime,requires its own server + config; harness not bundled; see README"
} >>"$RESULTS"

echo "############################################################"
echo "# results: $RESULTS  (this run; gitignored)"
echo "# committed reference baseline: $HERE/results/baseline.csv"
echo "############################################################"
if command -v column >/dev/null 2>&1; then
  grep -v '^#' "$RESULTS" | column -t -s,
else
  grep -v '^#' "$RESULTS"
fi

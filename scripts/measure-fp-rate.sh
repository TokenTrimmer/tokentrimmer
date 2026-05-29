#!/usr/bin/env bash
# measure-fp-rate.sh — per-rule precision / recall / FP rate measurement.
#
# For every rule directory under crates/inspect-rules-tier1/tests/rules/:
#   - Run `tt inspect` over should-detect/   → each file is a true positive
#     (we expect ≥1 finding whose rule_id matches the directory name).
#   - Run `tt inspect` over should-not-detect/ → each file is a true negative
#     (any finding whose rule_id matches the directory name is a false positive).
#
# Outputs a markdown table to stdout: rule | TP | FN | FP | TN | precision | recall | FP rate.
# Exits non-zero if any rule's FP rate exceeds THRESHOLD_PCT (default 5%).
#
# Usage:
#   ./scripts/measure-fp-rate.sh                 # default fail-on FP > 5%
#   ./scripts/measure-fp-rate.sh --threshold 10  # fail-on FP > 10%
#   ./scripts/measure-fp-rate.sh --rule output-no-max-tokens  # one rule only
set -euo pipefail

cd "$(dirname "$0")/.."

THRESHOLD_PCT=5
ONLY_RULE=""
CORPUS=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --threshold) THRESHOLD_PCT="$2"; shift 2 ;;
    --rule) ONLY_RULE="$2"; shift 2 ;;
    --corpus) CORPUS="$2"; shift 2 ;;
    -h|--help)
      sed -n '2,20p' "$0"
      exit 0
      ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

# --- Locate tt binary (absolute path; survives `cd`) ---
TT=""
for path in ./target/release/tt ./target/debug/tt; do
  if [[ -x "$path" ]]; then
    TT=$(cd "$(dirname "$path")" && pwd)/$(basename "$path")
    break
  fi
done
if [[ -z "$TT" ]]; then
  echo "tt binary not built. Build with: cargo build -p tt-cli" >&2
  exit 2
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "jq required but not found." >&2
  exit 2
fi

RULES_DIR="crates/inspect-rules-tier1/tests/rules"
[[ -d "$RULES_DIR" ]] || { echo "$RULES_DIR not found" >&2; exit 2; }

# --- Corpus mode: FP rate on a tree of real-world-style clean code -----------
# Every finding on the corpus is a presumed false positive. Reports the share
# of corpus files each rule flags and fails if any exceeds THRESHOLD_PCT.
if [[ -n "$CORPUS" ]]; then
  [[ -d "$CORPUS" ]] || { echo "corpus dir $CORPUS not found" >&2; exit 2; }
  TMPC=$(mktemp -d -t fp-corpus.XXXXXX)
  trap 'rm -rf "$TMPC"' EXIT
  out="$TMPC/findings.json"
  "$TT" inspect "$CORPUS" --output "$out" --fail-on critical >/dev/null 2>&1 || true
  [[ -s "$out" ]] || echo "[]" >"$out"
  total_files=$(find "$CORPUS" -type f \
    \( -name '*.py' -o -name '*.ts' -o -name '*.tsx' -o -name '*.js' -o -name '*.jsx' -o -name '*.md' \) \
    | wc -l | tr -d ' ')

  echo "# Inspect corpus FP report"
  echo
  echo "Corpus: \`$CORPUS\` ($total_files source files). Every finding here is a presumed false positive."
  echo "Threshold: a rule flagging > ${THRESHOLD_PCT}% of files fails the gate."
  echo
  echo "_Repo-structure rules (\`config-*\`) are excluded: they assess repo layout"
  echo "(presence/size of an AGENTS.md), not per-file code patterns, so they are"
  echo "not meaningful on a flat code corpus._"
  echo
  echo "| rule | files flagged | total files | FP rate | status |"
  echo "|---|---:|---:|---:|---|"
  corpus_fail=0
  for rule_dir in "$RULES_DIR"/*/; do
    rule=$(basename "$rule_dir")
    # Skip repo-structure rules — they judge repo layout, not code patterns.
    [[ "$rule" == config-* ]] && continue
    flagged=$(jq --arg r "$rule" '[.[] | select(.rule_id==$r) | .file] | unique | length' "$out")
    [[ "$flagged" -gt 0 ]] || continue
    rate_num=$(awk -v f="$flagged" -v n="$total_files" 'BEGIN{ if(n==0) print 0; else printf "%d", (f/n)*100 }')
    rate_pct=$(awk -v f="$flagged" -v n="$total_files" 'BEGIN{ if(n==0) print "0%"; else printf "%.0f%%", (f/n)*100 }')
    status="ok"
    if [[ "$rate_num" -gt "$THRESHOLD_PCT" ]]; then status="FAIL (> ${THRESHOLD_PCT}%)"; corpus_fail=$((corpus_fail + 1)); fi
    echo "| $rule | $flagged | $total_files | $rate_pct | $status |"
  done
  total_findings=$(jq 'length' "$out")
  echo
  echo "Total findings across corpus: ${total_findings}."
  if [[ "$corpus_fail" -gt 0 ]]; then
    echo "RESULT: ${corpus_fail} rule(s) over the ${THRESHOLD_PCT}% FP threshold on the corpus."
    exit 1
  fi
  echo "RESULT: all rules within the ${THRESHOLD_PCT}% FP threshold on the corpus."
  exit 0
fi

TMPDIR_RUN=$(mktemp -d -t fp-rate.XXXXXX)
trap 'rm -rf "$TMPDIR_RUN"' EXIT

# --- Counters ---
declare -i any_fail=0
declare -a rows=()

# Run tt inspect on a single fixture file, return number of findings whose
# rule_id matches `$2`. The file is COPIED to a neutral temp path first
# because the production rules short-circuit when the source path contains
# `/tests/` or `/should-(not-)detect/` (to prevent self-trigger when the
# engine scans this repo's own fixture tree).
# Args: $1 file path, $2 rule_id
count_findings_for() {
  local file="$1" rule="$2" out staged
  out="${TMPDIR_RUN}/findings.json"
  # Stage under a path that does not match the rule's is_test_fixture()
  # blocklist. Preserve the file extension so language detection works.
  staged="${TMPDIR_RUN}/corpus/$(basename "$file")"
  mkdir -p "${TMPDIR_RUN}/corpus"
  cp "$file" "$staged"
  "$TT" inspect "$staged" --output "$out" --fail-on critical >/dev/null 2>&1 || true
  if [[ ! -s "$out" ]]; then
    echo 0
    return
  fi
  jq --arg r "$rule" '[.[] | select(.rule_id == $r)] | length' "$out"
}

# --- Per-rule loop ---
for rule_dir in "$RULES_DIR"/*/; do
  rule=$(basename "$rule_dir")
  [[ -n "$ONLY_RULE" && "$rule" != "$ONLY_RULE" ]] && continue

  tp=0 fn=0 fp=0 tn=0

  # should-detect: TP if rule fires, FN otherwise
  if [[ -d "${rule_dir}should-detect" ]]; then
    for f in "${rule_dir}should-detect"/*; do
      [[ -f "$f" ]] || continue
      n=$(count_findings_for "$f" "$rule")
      if [[ "$n" -gt 0 ]]; then tp=$((tp + 1)); else fn=$((fn + 1)); fi
    done
  fi

  # should-not-detect: FP if rule fires, TN otherwise
  if [[ -d "${rule_dir}should-not-detect" ]]; then
    for f in "${rule_dir}should-not-detect"/*; do
      [[ -f "$f" ]] || continue
      n=$(count_findings_for "$f" "$rule")
      if [[ "$n" -gt 0 ]]; then fp=$((fp + 1)); else tn=$((tn + 1)); fi
    done
  fi

  # Precision = TP / (TP + FP); Recall = TP / (TP + FN); FP rate = FP / (FP + TN)
  total_pred_pos=$((tp + fp))
  total_actual_pos=$((tp + fn))
  total_actual_neg=$((fp + tn))
  precision="n/a"
  recall="n/a"
  fp_rate="n/a"
  fp_rate_num=0
  [[ $total_pred_pos -gt 0 ]] && precision=$(awk -v t="$tp" -v p="$total_pred_pos" 'BEGIN{printf "%.0f%%", (t/p)*100}')
  [[ $total_actual_pos -gt 0 ]] && recall=$(awk -v t="$tp" -v a="$total_actual_pos" 'BEGIN{printf "%.0f%%", (t/a)*100}')
  if [[ $total_actual_neg -gt 0 ]]; then
    fp_rate=$(awk -v f="$fp" -v n="$total_actual_neg" 'BEGIN{printf "%.0f%%", (f/n)*100}')
    fp_rate_num=$(awk -v f="$fp" -v n="$total_actual_neg" 'BEGIN{printf "%d", (f/n)*100}')
  fi

  status="ok"
  if [[ "$fp_rate_num" -gt "$THRESHOLD_PCT" ]]; then
    status="FAIL (> ${THRESHOLD_PCT}%)"
    any_fail=$((any_fail + 1))
  fi

  rows+=("$rule|$tp|$fn|$fp|$tn|$precision|$recall|$fp_rate|$status")
done

# --- Render table ---
echo "# Inspect rule FP-rate report"
echo
echo "Threshold: FP rate > ${THRESHOLD_PCT}% fails the gate."
echo
echo "| rule | TP | FN | FP | TN | precision | recall | FP rate | status |"
echo "|---|---:|---:|---:|---:|---:|---:|---:|---|"
for r in "${rows[@]}"; do
  IFS='|' read -r rule tp fn fp tn prec rec fpr status <<<"$r"
  echo "| ${rule} | ${tp} | ${fn} | ${fp} | ${tn} | ${prec} | ${rec} | ${fpr} | ${status} |"
done
echo

if [[ "$any_fail" -gt 0 ]]; then
  echo "RESULT: ${any_fail} rule(s) over FP threshold."
  exit 1
fi
echo "RESULT: all rules within FP threshold (${THRESHOLD_PCT}%)."

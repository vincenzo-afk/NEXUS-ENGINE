#!/usr/bin/env bash
# Evaluate a NEXUS bootstrap corpus against the tracked query/domain smoke set.
#
# Every expected domain must be present in the first result page. This is a
# transparent relevance guardrail for a curated corpus, not a claim of a
# general-purpose search-quality benchmark.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NEXUS_BIN="${NEXUS_BIN:-$ROOT/target/release/nexus}"
QUERY_FILE="${QUERY_FILE:-$ROOT/docs/bootstrap-relevance-queries.tsv}"
CONFIG=""

usage() {
  cat <<'USAGE'
Usage: scripts/evaluate_bootstrap_corpus.sh --config PATH [--queries PATH]

Checks every non-comment `query<TAB>expected-domain-or-title-term<TAB>intent`
entry in the query file. The expected term must occur in NEXUS's first page of
web-search output. Exits non-zero if any query misses its expected source.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --config) CONFIG="$2"; shift 2 ;;
    --queries) QUERY_FILE="$2"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$CONFIG" ]] || { echo "--config is required" >&2; usage >&2; exit 2; }
[[ -x "$NEXUS_BIN" ]] || { echo "NEXUS binary not found or not executable: $NEXUS_BIN" >&2; exit 1; }
[[ -f "$QUERY_FILE" ]] || { echo "Query file not found: $QUERY_FILE" >&2; exit 1; }

total=0
passed=0
failed=0
while IFS=$'\t' read -r query expected intent; do
  [[ -z "$query" || "$query" == \#* ]] && continue
  total=$((total + 1))
  output="$("$NEXUS_BIN" search --config "$CONFIG" --mode web "$query" 2>&1)"
  if grep -Fqi "$expected" <<<"$output"; then
    printf 'PASS  %-28s expected=%s (%s)\n' "$query" "$expected" "$intent"
    passed=$((passed + 1))
  else
    printf 'FAIL  %-28s expected=%s (%s)\n' "$query" "$expected" "$intent" >&2
    printf '%s\n' "$output" | sed -n '1,40p' >&2
    failed=$((failed + 1))
  fi
done <"$QUERY_FILE"

printf 'Relevance evaluation: %s/%s passed\n' "$passed" "$total"
[[ "$failed" -eq 0 ]]

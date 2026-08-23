#!/usr/bin/env bash
# Build a bounded, reproducible, robots-respecting NEXUS web bootstrap corpus.
#
# This is deliberately a curated bootstrap tool, not an Internet-scale crawler.
# It relies on NEXUS's existing crawl queue, robots parser, conditional fetches,
# per-domain politeness, safety filtering, canonicalisation, deduplication, and
# PageRank implementation. Run it under an appropriate supervisor only after
# selecting a persistent deployment model for recurring refreshes.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NEXUS_BIN="${NEXUS_BIN:-$ROOT/target/release/nexus}"
SEED_FILE="${SEED_FILE:-$ROOT/data/bootstrap-seeds.txt}"
OUTPUT_DIR="${OUTPUT_DIR:-$ROOT/.bootstrap-corpus}"
PASSES="${PASSES:-18}"
PER_PASS_PAGES="${PER_PASS_PAGES:-12}"
MAX_DEPTH="${MAX_DEPTH:-2}"
DELAY_SECONDS="${DELAY_SECONDS:-2}"

usage() {
  cat <<'USAGE'
Usage: scripts/build_bootstrap_corpus.sh [--output-dir DIR] [--passes N] [--per-pass-pages N] [--max-depth N]

Environment overrides: NEXUS_BIN, SEED_FILE, OUTPUT_DIR, PASSES,
PER_PASS_PAGES, MAX_DEPTH, DELAY_SECONDS.

The generated directory contains config.toml, index.nxi, web_content_cache/,
crawl_queue.nxq, and bootstrap-report.txt. Review the report and evaluation
results before packaging any generated data for a public service.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output-dir) OUTPUT_DIR="$2"; shift 2 ;;
    --passes) PASSES="$2"; shift 2 ;;
    --per-pass-pages) PER_PASS_PAGES="$2"; shift 2 ;;
    --max-depth) MAX_DEPTH="$2"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

for value in "$PASSES" "$PER_PASS_PAGES" "$MAX_DEPTH"; do
  [[ "$value" =~ ^[1-9][0-9]*$ ]] || { echo "Positive integer required, received: $value" >&2; exit 2; }
done

[[ -x "$NEXUS_BIN" ]] || { echo "NEXUS binary not found or not executable: $NEXUS_BIN" >&2; exit 1; }
[[ -f "$SEED_FILE" ]] || { echo "Seed manifest not found: $SEED_FILE" >&2; exit 1; }

mkdir -p "$OUTPUT_DIR"
CONFIG="$OUTPUT_DIR/config.toml"
REPORT="$OUTPUT_DIR/bootstrap-report.txt"

# `nexus config` materializes a complete compatible default TOML file. Rewrite
# only the storage locations and bounded web-crawl settings for this isolated
# corpus, avoiding accidental use of a developer's normal local index.
"$NEXUS_BIN" config --config "$CONFIG" >/dev/null
escape_toml_path() { printf '%s' "$1" | sed 's/[&|]/\\&/g'; }
INDEX_PATH="$(escape_toml_path "$OUTPUT_DIR/index.nxi")"
CACHE_PATH="$(escape_toml_path "$OUTPUT_DIR/web_content_cache")"
CLICKS_PATH="$(escape_toml_path "$OUTPUT_DIR/clicks.nxc")"
MODEL_PATH="$(escape_toml_path "$OUTPUT_DIR/personal_model.json")"
SEARCH_PATH="$(escape_toml_path "$OUTPUT_DIR/last_search.json")"
QUEUE_PATH="$(escape_toml_path "$OUTPUT_DIR/crawl_queue.nxq")"
sed -i \
  -e "s|^index_path = .*|index_path = \"$INDEX_PATH\"|" \
  -e "s|^content_cache_dir = .*|content_cache_dir = \"$CACHE_PATH\"|" \
  -e "s|^clicks_path = .*|clicks_path = \"$CLICKS_PATH\"|" \
  -e "s|^personal_model_path = .*|personal_model_path = \"$MODEL_PATH\"|" \
  -e "s|^last_search_path = .*|last_search_path = \"$SEARCH_PATH\"|" \
  -e "s|^crawl_queue_path = .*|crawl_queue_path = \"$QUEUE_PATH\"|" \
  -e 's|^user_agent = .*|user_agent = "NexusBot/1.0 (+https://github.com/vincenzo-afk/NEXUS-ENGINE; curated-bootstrap-crawler)"|' \
  -e "s|^max_pages = .*|max_pages = $PER_PASS_PAGES|" \
  -e "s|^max_depth = .*|max_depth = $MAX_DEPTH|" \
  -e 's|^default_delay_millis = .*|default_delay_millis = 1500|' \
  -e 's|^timeout_seconds = .*|timeout_seconds = 15|' \
  -e 's|^max_retries = .*|max_retries = 1|' \
  -e 's|^respect_robots = .*|respect_robots = true|' \
  -e 's|^max_page_size_bytes = .*|max_page_size_bytes = 1048576|' \
  -e 's|^max_pages_per_domain = .*|max_pages_per_domain = 12|' \
  -e 's|^discover_sitemaps = .*|discover_sitemaps = false|' \
  -e 's|^discover_feeds = .*|discover_feeds = false|' \
  "$CONFIG"

{
  printf 'NEXUS curated bootstrap crawl\n'
  printf 'Seed manifest: %s\n' "$SEED_FILE"
  printf 'Seed manifest SHA-256: '
  sha256sum "$SEED_FILE" | awk '{print $1}'
  printf 'Configuration: %s\n' "$CONFIG"
  printf 'Passes: %s; per-pass page cap: %s; max depth: %s\n\n' "$PASSES" "$PER_PASS_PAGES" "$MAX_DEPTH"
} >"$REPORT"

for pass in $(seq 1 "$PASSES"); do
  printf '\n=== crawl pass %s/%s ===\n' "$pass" "$PASSES" | tee -a "$REPORT"
  if [[ "$pass" == "1" ]]; then
    "$NEXUS_BIN" crawl --config "$CONFIG" --seed-file "$SEED_FILE" --max-pages "$PER_PASS_PAGES" --max-depth "$MAX_DEPTH" 2>&1 | tee -a "$REPORT"
  else
    "$NEXUS_BIN" crawl --config "$CONFIG" --resume --max-pages "$PER_PASS_PAGES" --max-depth "$MAX_DEPTH" 2>&1 | tee -a "$REPORT"
  fi
  [[ "$pass" == "$PASSES" ]] || sleep "$DELAY_SECONDS"
done

printf '\n=== final PageRank ===\n' | tee -a "$REPORT"
"$NEXUS_BIN" pagerank --config "$CONFIG" 2>&1 | tee -a "$REPORT"
printf '\n=== final corpus statistics ===\n' | tee -a "$REPORT"
"$NEXUS_BIN" stats --config "$CONFIG" 2>&1 | tee -a "$REPORT"

printf '\n=== relevance evaluation ===\n' | tee -a "$REPORT"
NEXUS_BIN="$NEXUS_BIN" "$ROOT/scripts/evaluate_bootstrap_corpus.sh" \
  --config "$CONFIG" \
  --queries "$ROOT/docs/bootstrap-relevance-queries.tsv" 2>&1 | tee -a "$REPORT"

printf '\nBootstrap corpus complete: %s\n' "$OUTPUT_DIR" | tee -a "$REPORT"

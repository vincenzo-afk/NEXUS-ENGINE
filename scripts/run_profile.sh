#!/usr/bin/env bash
set -euo pipefail

REPO="/home/ubuntu/NEXUS-ENGINE"
BIN="$REPO/target/release/nexus"
OUT_DIR="${1:-/home/ubuntu/nexus_test/profile-$(date -u +%Y%m%dT%H%M%SZ)}"
PORT="${NEXUS_PROFILE_PORT:-18331}"

mkdir -p "$OUT_DIR"/{metrics,logs,xdg-data,xdg-config,xdg-cache,corpus-small,corpus-large}
export XDG_DATA_HOME="$OUT_DIR/xdg-data"
export XDG_CONFIG_HOME="$OUT_DIR/xdg-config"
export XDG_CACHE_HOME="$OUT_DIR/xdg-cache"

if [[ ! -x "$BIN" ]]; then
  echo "Release binary not found at $BIN" >&2
  exit 1
fi

run_metric() {
  local name="$1"
  shift
  /usr/bin/time \
    -f 'elapsed_sec=%e\nuser_sec=%U\nsystem_sec=%S\nmax_rss_kb=%M\nfs_inputs=%I\nfs_outputs=%O\nvoluntary_context_switches=%w\ninvoluntary_context_switches=%c' \
    -o "$OUT_DIR/metrics/${name}.time" \
    "$@" >"$OUT_DIR/logs/${name}.stdout" 2>"$OUT_DIR/logs/${name}.stderr"
}

copy_source_corpus() {
  local source_file rel target
  while IFS= read -r -d '' source_file; do
    rel="${source_file#$REPO/}"
    target="$OUT_DIR/corpus-small/$rel"
    mkdir -p "$(dirname "$target")"
    cp "$source_file" "$target"
  done < <(find "$REPO/src" -type f -name '*.rs' -print0 | sort -z)
  cp "$REPO/README.md" "$OUT_DIR/corpus-small/README.md"
}

build_scaled_corpus() {
  local source_file copy_number name
  while IFS= read -r -d '' source_file; do
    name="$(basename "$source_file")"
    for copy_number in $(seq 1 24); do
      cp "$source_file" "$OUT_DIR/corpus-large/${name}.${copy_number}.txt"
    done
  done < <(find "$OUT_DIR/corpus-small" -type f -print0 | sort -z)
}

write_metadata() {
  {
    echo "timestamp_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "git_head=$(git -C "$REPO" rev-parse HEAD)"
    echo "binary_sha256=$(sha256sum "$BIN" | awk '{print $1}')"
    echo "binary_size_bytes=$(stat -c '%s' "$BIN")"
    echo "cpu_count=$(nproc)"
    echo "memory=$(free -h | awk 'NR==2 {print $2 " total; " $7 " available"}')"
    echo "small_documents=$(find "$OUT_DIR/corpus-small" -type f | wc -l)"
    echo "large_documents=$(find "$OUT_DIR/corpus-large" -type f | wc -l)"
    echo "small_bytes=$(du -sb "$OUT_DIR/corpus-small" | awk '{print $1}')"
    echo "large_bytes=$(du -sb "$OUT_DIR/corpus-large" | awk '{print $1}')"
  } > "$OUT_DIR/metadata.txt"
}

copy_source_corpus
build_scaled_corpus
write_metadata

run_metric add_small "$BIN" add-folder "$OUT_DIR/corpus-small"
run_metric index_small "$BIN" index
run_metric stats_small "$BIN" stats
run_metric cli_search_small_100 bash -c '
  set -euo pipefail
  binary="$1"
  for query in parser storage websocket crawler ranking unicode; do
    for _ in $(seq 1 16); do
      "$binary" search "$query" --mode local --limit 10 >/dev/null
    done
  done
' _ "$BIN"
run_metric remove_small "$BIN" remove-folder "$OUT_DIR/corpus-small"

run_metric add_large "$BIN" add-folder "$OUT_DIR/corpus-large"
run_metric index_large "$BIN" index
run_metric stats_large "$BIN" stats
run_metric cli_search_large_100 bash -c '
  set -euo pipefail
  binary="$1"
  for query in parser storage websocket crawler ranking unicode; do
    for _ in $(seq 1 16); do
      "$binary" search "$query" --mode local --limit 10 >/dev/null
    done
  done
' _ "$BIN"

run_metric api_search_sequential_48 bash -c '
  set -euo pipefail
  binary="$1"; port="$2"; log="$3"
  "$binary" serve --bind "127.0.0.1:$port" >"$log" 2>&1 &
  pid=$!
  cleanup() { kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; }
  trap cleanup EXIT
  for _ in $(seq 1 50); do
    if curl -fsS "http://127.0.0.1:$port/health" >/dev/null; then break; fi
    sleep 0.1
  done
  curl -fsS "http://127.0.0.1:$port/health" >/dev/null
  for query in parser storage websocket crawler ranking unicode; do
    for _ in $(seq 1 8); do
      curl -fsSG "http://127.0.0.1:$port/search" --data-urlencode "q=$query" --data "mode=local" --data "limit=10" >/dev/null
    done
  done
' _ "$BIN" "$PORT" "$OUT_DIR/logs/api-server-sequential.log"

run_metric api_search_parallel_48 bash -c '
  set -euo pipefail
  binary="$1"; port="$2"; log="$3"
  "$binary" serve --bind "127.0.0.1:$port" >"$log" 2>&1 &
  pid=$!
  cleanup() { kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; }
  trap cleanup EXIT
  for _ in $(seq 1 50); do
    if curl -fsS "http://127.0.0.1:$port/health" >/dev/null; then break; fi
    sleep 0.1
  done
  workers=()
  for worker in $(seq 1 4); do
    (
      for _ in $(seq 1 12); do
        curl -fsSG "http://127.0.0.1:$port/search" --data-urlencode "q=parser storage" --data "mode=local" --data "limit=10" >/dev/null
      done
    ) &
    workers+=("$!")
  done
  for worker in "${workers[@]}"; do
    wait "$worker"
  done
' _ "$BIN" "$PORT" "$OUT_DIR/logs/api-server-parallel.log"

mkdir -p "$OUT_DIR/crawl-data" "$OUT_DIR/crawl-config"
run_metric crawl_example_org_1_page env \
  XDG_DATA_HOME="$OUT_DIR/crawl-data" \
  XDG_CONFIG_HOME="$OUT_DIR/crawl-config" \
  XDG_CACHE_HOME="$OUT_DIR/crawl-cache" \
  "$BIN" crawl https://example.org --max-pages 1 --max-depth 0 --domain example.org

echo "$OUT_DIR"

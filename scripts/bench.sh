#!/usr/bin/env bash
# Compare the routing algorithms against the recorded market fixture.
#
# Builds fynd-core/benches/algorithm_bench.rs with release optimisations plus debug symbols and
# runs it. Every run writes bench-results/<name>/ holding report.md, orders.csv, pairs.csv,
# protocols.csv, routes.jsonl and run.json, so runs can be compared rather than overwritten.
# Browse them with ./scripts/bench-viewer.sh.
#
# To run against a live market instead, use ./scripts/bench-live.sh, which is this script with
# --market live and the Tycho settings.
#
# To profile one algorithm instead of comparing several, use ./scripts/profile.sh -- it runs a
# single config on one thread and writes nothing.
#
# Usage:
#   ./scripts/bench.sh [options]
#
# Options are handed to the benchmark unchanged; it owns the list, the defaults and the
# validation. Run ./scripts/bench.sh --help-bench to see them.
#
# Examples:
#   ./scripts/bench.sh --name baseline --orders 2000
#   ./scripts/bench.sh --name water-only --orders 400 --configs water_fill_d3
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

SHOW_BENCH_HELP=0
BENCH_ARGS=()

usage() {
  # Everything from the second line to the first non-comment, so the range never has to be
  # updated when the header grows.
  sed -n '2,/^[^#]/p' "${BASH_SOURCE[0]}" | sed -e '$d' -e 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --help-bench)
      SHOW_BENCH_HELP=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      BENCH_ARGS+=("$1")
      shift
      ;;
  esac
done

if ! command -v jq >/dev/null 2>&1; then
  echo "error: jq is not installed — brew install jq" >&2
  exit 1
fi

# Bench binaries land at target/<profile>/deps/<name>-<hash>, so the path is asked for rather than
# assumed: the hash tracks the feature set and would go stale the moment that changed. The
# `profiling` profile is release plus debug symbols; without symbols a flamegraph is a wall of hex.
echo "Building (release + debug symbols) ..."
BIN="$(cargo bench -p fynd-core --profile profiling --features test-utils \
  --bench algorithm_bench --no-run --message-format=json |
  jq -r 'select(.target.kind[0] == "bench" and .executable != null) | .executable' | tail -1)"

if [[ -z "$BIN" || "$BIN" == "null" ]]; then
  echo "error: cargo did not report a bench binary path" >&2
  exit 1
fi
echo "Built $BIN"

if [[ $SHOW_BENCH_HELP -eq 1 ]]; then
  exec "$BIN" --help
fi

echo "Running ..."
# bash 3.2 (what macOS ships) treats "${ARR[@]}" on an empty array as unset under `set -u`, so the
# arguments are expanded only when there are some.
exec "$BIN" ${BENCH_ARGS[@]+"${BENCH_ARGS[@]}"}

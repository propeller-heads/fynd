#!/usr/bin/env bash
# Profile one routing algorithm over chosen orders.
#
# Builds fynd-core/benches/profile.rs with release optimisations plus debug symbols and records it
# under samply. One algorithm, one solver thread, no output files -- so the flamegraph is the solve
# and almost nothing else. For comparing algorithms and producing a report, use ./scripts/bench.sh.
#
# Orders come from the same dataset the benchmark reads, so an order id seen in the viewer can be
# profiled here directly.
#
# Usage:
#   ./scripts/profile.sh --config NAME [options] [-- samply args...]
#
# Options this script consumes:
#   --no-record           Run without samply, for timings only
#   --save-only           Write profile.json without opening the UI
#   --help-profile        Show the profiler's own options and defaults
#
# Everything else is handed to the profiler unchanged; it owns the option list and its validation.
#
# Examples:
#   ./scripts/profile.sh --config water_fill_d3 --order 2073
#   ./scripts/profile.sh --config water_fill_d3 --orders 200 --repeats 3
#   ./scripts/profile.sh --config water_fill_d3 --orders 50 --no-record
#   ./scripts/profile.sh --config water_fill_d3 --orders 200 -- --rate 5000
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

RECORD=1
SAVE_ONLY=0
SHOW_BIN_HELP=0
BIN_ARGS=()
SAMPLY_EXTRA=()

usage() { sed -n '2,25p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-record)
      RECORD=0
      shift
      ;;
    --save-only)
      SAVE_ONLY=1
      shift
      ;;
    --help-profile)
      SHOW_BIN_HELP=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    --)
      shift
      SAMPLY_EXTRA=("$@")
      break
      ;;
    *)
      BIN_ARGS+=("$1")
      shift
      ;;
  esac
done

if [[ $SHOW_BIN_HELP -eq 0 ]] &&
  ! printf '%s\n' ${BIN_ARGS[@]+"${BIN_ARGS[@]}"} | grep -q -- '^--config'; then
  echo "error: --config is required (e.g. --config water_fill_d3)" >&2
  echo "Available:" >&2
  basename -s .toml -a fynd-core/benches/configs/*.toml | sed 's/^/  /' >&2
  exit 1
fi

REQUIRED_TOOLS=(jq)
if [[ $RECORD -eq 1 ]]; then
  REQUIRED_TOOLS+=(samply)
fi
for tool in "${REQUIRED_TOOLS[@]}"; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: $tool is not installed" >&2
    echo "  samply: cargo install samply    (or pass --no-record)" >&2
    echo "  jq:     brew install jq" >&2
    exit 1
  fi
done

# Bench binaries land at target/<profile>/deps/<name>-<hash>, so the path is asked for rather than
# assumed: the hash tracks the feature set and would go stale the moment that changed. The
# `profiling` profile is release plus debug symbols; without symbols a flamegraph is a wall of hex.
echo "Building (release + debug symbols) ..."
BIN="$(cargo bench -p fynd-core --profile profiling --features test-utils \
  --bench profile --no-run --message-format=json |
  jq -r 'select(.target.kind[0] == "bench" and .executable != null) | .executable' | tail -1)"

if [[ -z "$BIN" || "$BIN" == "null" ]]; then
  echo "error: cargo did not report a bench binary path" >&2
  exit 1
fi
echo "Built $BIN"

if [[ $SHOW_BIN_HELP -eq 1 ]]; then
  exec "$BIN" --help
fi

# bash 3.2 (what macOS ships) treats "${ARR[@]}" on an empty array as unset under `set -u`, so the
# pass-through arrays are expanded only when they hold something.
if [[ $RECORD -eq 0 ]]; then
  echo "Running ..."
  exec "$BIN" ${BIN_ARGS[@]+"${BIN_ARGS[@]}"}
fi

SAMPLY_ARGS=(record)
if [[ $SAVE_ONLY -eq 1 ]]; then
  SAMPLY_ARGS+=(--save-only)
fi

echo "Recording ..."
exec samply "${SAMPLY_ARGS[@]}" ${SAMPLY_EXTRA[@]+"${SAMPLY_EXTRA[@]}"} \
  "$BIN" ${BIN_ARGS[@]+"${BIN_ARGS[@]}"}

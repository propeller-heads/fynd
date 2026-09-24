#!/usr/bin/env bash
# Time the derived computations on a recorded market, under samply by default.
#
# Builds the `derived` bench target of bench-harness with release optimisations plus debug symbols
# and runs it: a full recompute on the recording's snapshot, then one incremental run for each later
# block. Every derived computation runs.
#
# Usage:
#   ./scripts/derived-bench.sh [options]
#
# Options:
#   --recording PATH      Market recording to replay, relative to the current directory
#                         (default: recordings/native_tvl1_2026-09-24/market_recording.json.zst
#                         in the repository)
#   --repeats N           Replay the whole recording N times in one process (default: 1),
#                         so samply collects more samples
#   --no-record           Run without samply, for timings only
#
# Examples:
#   ./scripts/derived-bench.sh
#   ./scripts/derived-bench.sh --no-record
#   ./scripts/derived-bench.sh --repeats 20
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

RECORDING="$REPO_ROOT/recordings/native_tvl1_2026-09-24/market_recording.json.zst"
RECORD=1
REPEATS=1

usage() { sed -n '2,22p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

require_value() {
  if [[ $# -lt 2 ]]; then
    echo "error: $1 needs a value" >&2
    usage >&2
    exit 1
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --recording)
      require_value "$@"
      RECORDING="$2"
      [[ "$RECORDING" == /* ]] || RECORDING="$PWD/$RECORDING"
      shift 2
      ;;
    --repeats)
      require_value "$@"
      REPEATS="$2"
      shift 2
      ;;
    --no-record)
      RECORD=0
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown option $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ ! -f "$RECORDING" ]]; then
  echo "error: no recording at $RECORDING (record one with tools/record-market)" >&2
  exit 1
fi

REQUIRED_TOOLS=(jq)
if [[ $RECORD -eq 1 ]]; then
  REQUIRED_TOOLS+=(samply)
fi
for tool in "${REQUIRED_TOOLS[@]}"; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: $tool is not installed (samply: cargo install samply, jq: brew install jq)" >&2
    exit 1
  fi
done

cd "$REPO_ROOT"

# The bench binary's name carries a hash that changes with each build, so ask cargo for the
# binary path.
echo "Building (release + debug symbols) ..."
BIN="$(cargo bench -p fynd-bench-harness --profile profiling --bench derived --no-run \
  --message-format=json |
  jq -r 'select(.target.kind[0] == "bench" and .executable != null) | .executable' | tail -1)"

if [[ -z "$BIN" ]]; then
  echo "error: cargo did not report a test binary path" >&2
  exit 1
fi
echo "Built $BIN"

BENCH_ARGS=(--recording "$RECORDING" --repeats "$REPEATS")
if [[ $RECORD -eq 0 ]]; then
  exec "$BIN" "${BENCH_ARGS[@]}"
fi
exec samply record -- "$BIN" "${BENCH_ARGS[@]}"

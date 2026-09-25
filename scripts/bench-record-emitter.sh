#!/usr/bin/env bash
# Measure what the quote record emitter costs the /v1/quote path (ENG-6342).
#
# Runs the load benchmark four times at a steady rate against a solver this script starts and
# stops, and prints one table:
#
#   1. baseline      no collector configured, so no queue and no sending task
#   2. baseline-2    the same run again; the gap between 1 and 2 is the noise floor
#   3. accept        emitter on, stub collector answering 202 to everything
#   4. blackhole     emitter on, stub collector accepting the connection and never answering
#
# Run 4 is the one that finds a blocking send: a pod that waits on the collector shows it in p99.
# The script exits non-zero when the p99 regression in run 3 or 4 exceeds the noise floor, which
# is the merge gate on ENG-6342.
#
# Needs a working solver environment: TYCHO_API_KEY and RPC_URL as for `fynd serve`, plus jq.
#
# Usage:
#   ./scripts/bench-record-emitter.sh [--rps 40] [--requests 4000] [--chain ethereum]
#                                     [--requests-file tools/benchmark/requests_set.json]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

RPS=40
REQUESTS=4000
CHAIN=ethereum
REQUESTS_FILE=""
HTTP_PORT=3000
STUB_PORT=8081
OUT_DIR="bench-results/record-emitter-$(date +%Y%m%d-%H%M%S)"

while [[ $# -gt 0 ]]; do
  case "$1" in
  --rps)
    RPS="$2"
    shift 2
    ;;
  --requests)
    REQUESTS="$2"
    shift 2
    ;;
  --chain)
    CHAIN="$2"
    shift 2
    ;;
  --requests-file)
    REQUESTS_FILE="$2"
    shift 2
    ;;
  --out-dir)
    OUT_DIR="$2"
    shift 2
    ;;
  -h | --help)
    sed -n '2,20p' "$0"
    exit 0
    ;;
  *)
    echo "unknown option: $1" >&2
    exit 2
    ;;
  esac
done

command -v jq >/dev/null || {
  echo "jq is required" >&2
  exit 1
}

# `rate:N` fires a request every N ms, so the rate is the interval's reciprocal.
INTERVAL_MS=$((1000 / RPS))
mkdir -p "$OUT_DIR"

echo "building release binaries"
# Per package: --bin only looks inside the workspace's default members, and the benchmark bins
# live outside them.
cargo build --release --bin fynd
cargo build --release -p fynd-benchmark --bins

SOLVER_PID=""
STUB_PID=""
SAMPLER_PID=""

# shellcheck disable=SC2329  # invoked by the EXIT trap below
cleanup() {
  [[ -n "$SAMPLER_PID" ]] && kill "$SAMPLER_PID" 2>/dev/null || true
  [[ -n "$SOLVER_PID" ]] && kill "$SOLVER_PID" 2>/dev/null || true
  [[ -n "$STUB_PID" ]] && kill "$STUB_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT

# Waits for the solver to report itself healthy, which is also what says the market is warm
# enough to quote. Without this the first run measures a cold graph.
wait_for_health() {
  for _ in $(seq 1 120); do
    if curl -sf "http://127.0.0.1:${HTTP_PORT}/v1/health" | jq -e '.healthy' >/dev/null 2>&1; then
      return 0
    fi
    sleep 5
  done
  echo "solver never became healthy" >&2
  return 1
}

# Samples the solver process once a second: CPU percent and resident size, as `ps` reports them.
sample_process() {
  local pid="$1" out="$2"
  while kill -0 "$pid" 2>/dev/null; do
    ps -o %cpu=,rss= -p "$pid" >>"$out" 2>/dev/null || true
    sleep 1
  done
}

# One scenario end to end: stub (when asked for), solver, warm-up, benchmark, teardown.
run_scenario() {
  local name="$1" stub_mode="${2:-}"
  echo
  echo "=== $name"

  if [[ -n "$stub_mode" ]]; then
    ./target/release/record-collector-stub --mode "$stub_mode" --port "$STUB_PORT" \
      >"$OUT_DIR/$name.stub.log" 2>&1 &
    STUB_PID=$!
    sleep 1
  fi

  local solver_args=(serve --chain "$CHAIN" --http-port "$HTTP_PORT")
  if [[ -n "$stub_mode" ]]; then
    solver_args+=(--collector-url "http://127.0.0.1:${STUB_PORT}")
  fi
  ./target/release/fynd "${solver_args[@]}" >"$OUT_DIR/$name.solver.log" 2>&1 &
  SOLVER_PID=$!
  wait_for_health

  : >"$OUT_DIR/$name.proc"
  sample_process "$SOLVER_PID" "$OUT_DIR/$name.proc" &
  SAMPLER_PID=$!

  local bench_args=(load -m "rate:${INTERVAL_MS}" -n "$REQUESTS"
    --solver-url "http://127.0.0.1:${HTTP_PORT}"
    --output-file "$OUT_DIR/$name.json")
  [[ -n "$REQUESTS_FILE" ]] && bench_args+=(--requests-file "$REQUESTS_FILE")
  ./target/release/fynd-benchmark "${bench_args[@]}" >"$OUT_DIR/$name.bench.log" 2>&1

  kill "$SAMPLER_PID" 2>/dev/null || true
  SAMPLER_PID=""
  kill "$SOLVER_PID" 2>/dev/null || true
  wait "$SOLVER_PID" 2>/dev/null || true
  SOLVER_PID=""
  if [[ -n "$stub_mode" ]]; then
    kill "$STUB_PID" 2>/dev/null || true
    wait "$STUB_PID" 2>/dev/null || true
    STUB_PID=""
  fi
}

run_scenario baseline
run_scenario baseline-2
run_scenario accept accept
run_scenario blackhole blackhole

# --- results ----------------------------------------------------------------------------------

p50() { jq -r '.statistics.round_trip.median' "$OUT_DIR/$1.json"; }
p99() { jq -r '.statistics.round_trip.p99' "$OUT_DIR/$1.json"; }
# Mean CPU percent and peak RSS in MiB over the samples taken during the run.
cpu() { awk '{ total += $1; n++ } END { if (n) printf "%.1f", total / n; else print "n/a" }' "$OUT_DIR/$1.proc"; }
rss() { awk '{ if ($2 > peak) peak = $2 } END { if (peak) printf "%.0f", peak / 1024; else print "n/a" }' "$OUT_DIR/$1.proc"; }

BASE_P99=$(p99 baseline)
NOISE=$(($(p99 baseline-2) - BASE_P99))
NOISE=${NOISE#-}

{
  echo "| run | p50 ms | p99 ms | cpu % | peak rss MiB |"
  echo "|---|---|---|---|---|"
  for name in baseline baseline-2 accept blackhole; do
    echo "| $name | $(p50 "$name") | $(p99 "$name") | $(cpu "$name") | $(rss "$name") |"
  done
  echo
  echo "Noise floor (|baseline-2 − baseline| p99): ${NOISE} ms"
  echo "Rate: ${RPS} rps, ${REQUESTS} requests per run, chain ${CHAIN}"
} | tee "$OUT_DIR/report.md"

FAILED=0
for name in accept blackhole; do
  regression=$(($(p99 "$name") - BASE_P99))
  if ((regression > NOISE)); then
    echo "FAIL: $name p99 is ${regression} ms over baseline, above the ${NOISE} ms noise floor" >&2
    FAILED=1
  else
    echo "ok: $name p99 is ${regression} ms over baseline, within the ${NOISE} ms noise floor"
  fi
done

echo
echo "results in $OUT_DIR"
exit "$FAILED"

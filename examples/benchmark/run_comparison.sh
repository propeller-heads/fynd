#!/usr/bin/env bash
set -euo pipefail

# Comparison benchmark: old BF vs new BF using 10k Dune trades
# Usage: ./run_comparison.sh [min_tvl]

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
FYND_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
REQUESTS_FILE="$SCRIPT_DIR/trades_10k_requests.json"
OUTPUT_DIR="$FYND_DIR/benchmark_results"
MIN_TVL="${1:-25}"
SOLVER_PORT=3000
SOLVER_URL="http://localhost:$SOLVER_PORT"
TYCHO_URL="${TYCHO_URL:-tycho-beta.propellerheads.xyz}"

mkdir -p "$OUTPUT_DIR"

OLD_BINARY="$FYND_DIR/target/release/examples/solver_old_bf"
NEW_BINARY="$FYND_DIR/target/release/examples/solver_new_bf"

if [[ ! -f "$OLD_BINARY" ]] || [[ ! -f "$NEW_BINARY" ]]; then
    echo "ERROR: Missing solver binaries. Build both versions first."
    exit 1
fi

if [[ ! -f "$REQUESTS_FILE" ]]; then
    echo "ERROR: Missing requests file: $REQUESTS_FILE"
    echo "Run: python3 csv_to_requests.py trades_10k_dune_eth_feb2026.csv trades_10k_requests.json"
    exit 1
fi

NUM_REQUESTS=$(python3 -c "import json; print(len(json.load(open('$REQUESTS_FILE'))))")
echo "=== Bellman-Ford Comparison Benchmark ==="
echo "Trades: $NUM_REQUESTS"
echo "Min TVL: $MIN_TVL ETH"
echo "Tycho URL: $TYCHO_URL"
echo ""

wait_for_solver() {
    local max_wait=180
    local elapsed=0
    echo -n "Waiting for solver to become healthy..."
    while (( elapsed < max_wait )); do
        if curl -s "$SOLVER_URL/v1/health" 2>/dev/null | python3 -c "import sys,json; d=json.load(sys.stdin); sys.exit(0 if d.get('healthy') else 1)" 2>/dev/null; then
            echo " ready! (${elapsed}s)"
            # Print pool count
            curl -s "$SOLVER_URL/v1/health" | python3 -c "import sys,json; d=json.load(sys.stdin); print(f'  Pools: {d[\"num_solver_pools\"]}, Last update: {d[\"last_update_ms\"]}ms ago')"
            return 0
        fi
        sleep 2
        elapsed=$((elapsed + 2))
        echo -n "."
    done
    echo " TIMEOUT after ${max_wait}s"
    return 1
}

run_benchmark() {
    local label="$1"
    local binary="$2"
    local log_file="$OUTPUT_DIR/${label}_solver.log"
    local results_file="$OUTPUT_DIR/${label}_results.json"

    echo ""
    echo "--- Running: $label ---"
    echo "Binary: $binary"
    echo "Log file: $log_file"

    # Kill any existing solver
    pkill -f "solver.*--rpc-url" 2>/dev/null || true
    sleep 2

    # Start solver in background with debug logging for order_manager
    RUST_LOG="fynd::order_manager=debug,fynd::algorithm=debug,fynd=info" \
    TYCHO_URL="$TYCHO_URL" \
    "$binary" \
        --rpc-url "$RPC_URL" \
        --tycho-url "$TYCHO_URL" \
        --tycho-api-key "${TYCHO_API_KEY:-}" \
        --min-tvl "$MIN_TVL" \
        --worker-pools-config "$FYND_DIR/worker_pools.toml" \
        --http-port "$SOLVER_PORT" \
        > "$log_file" 2>&1 &
    local solver_pid=$!
    echo "Solver PID: $solver_pid"

    # Wait for solver to be healthy
    if ! wait_for_solver; then
        echo "ERROR: Solver did not become healthy"
        kill "$solver_pid" 2>/dev/null || true
        return 1
    fi

    # Run benchmark
    echo "Running $NUM_REQUESTS trades sequentially..."
    cargo run --release --example benchmark -- \
        --solver-url "$SOLVER_URL" \
        --num-requests "$NUM_REQUESTS" \
        --parallelization-mode sequential \
        --requests-file "$REQUESTS_FILE" \
        --output-file "$results_file" \
        2>&1 | tee "$OUTPUT_DIR/${label}_benchmark.log"

    echo "Results saved to: $results_file"

    # Kill solver
    kill "$solver_pid" 2>/dev/null || true
    wait "$solver_pid" 2>/dev/null || true
    echo "Solver stopped."
}

# Run old BF
run_benchmark "old_bf" "$OLD_BINARY"

# Brief pause between runs
sleep 5

# Run new BF
run_benchmark "new_bf" "$NEW_BINARY"

echo ""
echo "=== Benchmark Complete ==="
echo "Results in: $OUTPUT_DIR/"
echo ""
echo "To compare, analyze the solver logs:"
echo "  Old BF: $OUTPUT_DIR/old_bf_solver.log"
echo "  New BF: $OUTPUT_DIR/new_bf_solver.log"

#!/usr/bin/env bash
set -euo pipefail

# Slippage feature data collection — long-running wrapper with auto-restart.
#
# Usage:
#   TYCHO_API_KEY=... RPC_URL=... ./tools/slippage-features/run_collection.sh
#
# Runs Fynd + quote driver with auto-restart on crash. Parquet files on disk
# are safe across restarts — each file is written atomically and accumulates.
# The quote driver resumes from the beginning of the trade list on restart
# (duplicates are fine — each quote gets a unique UUID).

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
DATA_DIR="${DATA_DIR:-$PROJECT_DIR/slippage-data}"
LOG_DIR="${LOG_DIR:-$DATA_DIR/logs}"
INTERVAL_SECS="${QUOTE_INTERVAL_SECS:-300}"
BATCH_SIZE="${QUOTE_BATCH_SIZE:-100}"
FLUSH_THRESHOLD="${SLIPPAGE_FLUSH_THRESHOLD:-50}"
FLUSH_INTERVAL="${SLIPPAGE_FLUSH_INTERVAL_SECS:-30}"
MAX_RESTARTS="${MAX_RESTARTS:-100}"

# Validate required env vars
for var in TYCHO_API_KEY RPC_URL; do
    if [ -z "${!var:-}" ]; then
        echo "ERROR: $var is not set" >&2
        exit 1
    fi
done

mkdir -p "$DATA_DIR/route_decay" "$LOG_DIR"

FYND_PID=""
DRIVER_PID=""
RESTART_COUNT=0

cleanup() {
    echo "[$(date -Iseconds)] shutting down..."
    [ -n "$DRIVER_PID" ] && kill "$DRIVER_PID" 2>/dev/null || true
    [ -n "$FYND_PID" ] && kill "$FYND_PID" 2>/dev/null || true
    wait 2>/dev/null || true
    echo "[$(date -Iseconds)] stopped after $RESTART_COUNT restart(s)"
    exit 0
}
trap cleanup SIGINT SIGTERM

start_fynd() {
    local log_file="$LOG_DIR/fynd_$(date +%Y%m%d_%H%M%S).log"
    echo "[$(date -Iseconds)] starting Fynd (log: $log_file)"

    RUST_LOG="info,slippage_features=debug" \
    SLIPPAGE_FLUSH_THRESHOLD="$FLUSH_THRESHOLD" \
    SLIPPAGE_FLUSH_INTERVAL_SECS="$FLUSH_INTERVAL" \
    SLIPPAGE_REQUOTE_URL="http://localhost:3000" \
    cargo run --features slippage-features --release \
        --manifest-path "$PROJECT_DIR/Cargo.toml" -- serve \
        --chain Ethereum \
        --tycho-api-key "$TYCHO_API_KEY" \
        --rpc-url "$RPC_URL" \
        --min-tvl 10 \
        --protocols uniswap_v2,uniswap_v3 \
        >> "$log_file" 2>&1 &
    FYND_PID=$!
}

wait_for_fynd() {
    echo "[$(date -Iseconds)] waiting for Fynd to sync..."
    for i in $(seq 1 120); do
        if curl -sf http://localhost:3000/v1/info > /dev/null 2>&1; then
            echo "[$(date -Iseconds)] Fynd ready"
            return 0
        fi
        sleep 1
    done
    echo "[$(date -Iseconds)] ERROR: Fynd did not become ready in 120s" >&2
    return 1
}

start_driver() {
    local log_file="$LOG_DIR/driver_$(date +%Y%m%d_%H%M%S).log"
    echo "[$(date -Iseconds)] starting quote driver (interval=${INTERVAL_SECS}s, batch=${BATCH_SIZE})"

    cargo run -p slippage-features --release \
        --manifest-path "$PROJECT_DIR/Cargo.toml" \
        --bin quote-driver -- \
        --trades-file "$PROJECT_DIR/trades_10k.json" \
        --fynd-url http://localhost:3000 \
        --interval-secs "$INTERVAL_SECS" \
        --batch-size "$BATCH_SIZE" \
        >> "$log_file" 2>&1 &
    DRIVER_PID=$!
}

print_stats() {
    local ql=$(find "$DATA_DIR" -maxdepth 1 -name "quote_log_*.parquet" 2>/dev/null | wc -l)
    local hd=$(find "$DATA_DIR/hop_decay" -name "*.parquet" 2>/dev/null | wc -l)
    local hs=$(find "$DATA_DIR/hop_static" -name "*.parquet" 2>/dev/null | wc -l)
    local tr=$(find "$DATA_DIR/tycho_route_decay" -name "*.parquet" 2>/dev/null | wc -l)
    local sz=$(du -sh "$DATA_DIR" 2>/dev/null | cut -f1)
    echo "[$(date -Iseconds)] stats: quote_log=$ql hop_decay=$hd hop_static=$hs tycho_route=$tr disk=$sz restarts=$RESTART_COUNT"
}

# Main loop
start_fynd
if ! wait_for_fynd; then
    echo "Fynd failed to start, aborting" >&2
    cleanup
fi
start_driver

while true; do
    # Check every 30 seconds
    sleep 30
    print_stats

    # Check if Fynd is alive
    if ! kill -0 "$FYND_PID" 2>/dev/null; then
        echo "[$(date -Iseconds)] WARNING: Fynd died, restarting..."
        [ -n "$DRIVER_PID" ] && kill "$DRIVER_PID" 2>/dev/null || true
        RESTART_COUNT=$((RESTART_COUNT + 1))
        if [ "$RESTART_COUNT" -ge "$MAX_RESTARTS" ]; then
            echo "[$(date -Iseconds)] ERROR: max restarts ($MAX_RESTARTS) reached" >&2
            exit 1
        fi
        sleep 5
        start_fynd
        if ! wait_for_fynd; then
            echo "[$(date -Iseconds)] ERROR: Fynd failed to restart" >&2
            exit 1
        fi
        start_driver
    fi

    # Check if driver is alive (restart if needed)
    if ! kill -0 "$DRIVER_PID" 2>/dev/null; then
        echo "[$(date -Iseconds)] WARNING: quote driver died, restarting..."
        start_driver
    fi
done

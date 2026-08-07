#!/usr/bin/env bash
# Restart apex-monitor when it stops applying blocks.
#
# `Restart=always` covers a crash or a clean exit. It does not cover the failure actually observed:
# the tycho websocket closed, the process stayed alive, and no block was applied for 15+ minutes.
# The monitor's own feed-dead timeout eventually rebuilds the solver, but a wedge that never trips
# it would stall silently — and silence costs a night of collection.
#
# The liveness signal is `hindsight_block_processing_seconds_count`, which increments once per
# re-solved block. File mtimes are not usable for this: the apex stream writes through a
# `BufWriter`, so its file can sit untouched through many healthy minutes.
set -uo pipefail

METRICS_URL="http://127.0.0.1:9899/metrics"
STATE_FILE=/var/lib/apex-monitor/health-state
STALL_LIMIT_SECONDS=600
# Building the market takes minutes of token loading, during which no block is applied and the
# metrics endpoint may not answer at all. Restarting inside that window would loop forever.
STARTUP_GRACE_SECONDS=1800

now=$(date +%s)

active_since_us=$(systemctl show apex-monitor --property=ActiveEnterTimestampMonotonic --value)
uptime_seconds=$(awk '{print int($1)}' /proc/uptime)
if [[ -n "$active_since_us" && "$active_since_us" != "0" ]]; then
    active_for=$(( uptime_seconds - active_since_us / 1000000 ))
    if (( active_for < STARTUP_GRACE_SECONDS )); then
        exit 0
    fi
fi

# An unreachable endpoint is itself a stall signal past the grace window, so it gets a sentinel
# rather than an early exit — a monitor that stopped serving metrics stopped doing anything else.
count=$(curl -fsS --max-time 10 "$METRICS_URL" |
    awk '$1 == "hindsight_block_processing_seconds_count" { print $2; exit }')
count=${count:-unreachable}

mkdir -p "$(dirname "$STATE_FILE")"
if ! read -r last_count last_change < "$STATE_FILE" 2>/dev/null; then
    last_count=""
    last_change=$now
fi

if [[ "$count" != "$last_count" ]]; then
    echo "$count $now" > "$STATE_FILE"
    exit 0
fi

stalled=$(( now - last_change ))
if (( stalled >= STALL_LIMIT_SECONDS )); then
    echo "apex-monitor applied no block for ${stalled}s (blocks=${count}); restarting"
    systemctl restart apex-monitor
    echo "$count $now" > "$STATE_FILE"
fi

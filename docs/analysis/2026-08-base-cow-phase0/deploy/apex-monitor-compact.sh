#!/usr/bin/env bash
# Compress closed-day JSONL and warn before the disk fills.
#
# At the expected ~0.5 GB/day the box's free space is ample. The guard exists so a runaway becomes
# visible in the journal before it starves the co-tenant PostgreSQL and tycho-rewind workloads.
set -uo pipefail

DATA_DIR=/home/agent/apex-data
MIN_FREE_GB=20

today=$(date -u +%F)
for path in "$DATA_DIR"/*.jsonl; do
    [[ -e "$path" ]] || continue
    # Today's files are still being appended to; only closed days are compressed.
    [[ "$path" == *"-${today}.jsonl" ]] && continue
    if zstd -q -10 -T2 --rm "$path"; then
        echo "compressed ${path}"
    else
        echo "WARNING: failed to compress ${path}"
    fi
done

free_gb=$(df -BG --output=avail "$DATA_DIR" | tail -1 | tr -dc '0-9')
if [[ -n "$free_gb" ]] && (( free_gb < MIN_FREE_GB )); then
    echo "WARNING: ${free_gb}G free on ${DATA_DIR}, below the ${MIN_FREE_GB}G floor"
fi

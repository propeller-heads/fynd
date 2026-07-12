#!/usr/bin/env bash
set -euo pipefail

: "${COLLECTOR_BINARY:?COLLECTOR_BINARY must point to pairs-data-collector}"
: "${BASE_CONFIG:?BASE_CONFIG must point to the full universe config}"
: "${BENCHMARK_DIR:?BENCHMARK_DIR must name an output directory}"

readonly max_heads="${MAX_HEADS:-20}"
readonly cases_text="${BENCHMARK_CASES:-2:2:200 4:4:200 8:8:200 16:16:200}"
script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly script_dir
read -r -a cases <<<"$cases_text"

mkdir -p "$BENCHMARK_DIR/configs" "$BENCHMARK_DIR/runs" "$BENCHMARK_DIR/summaries"
lscpu >"$BENCHMARK_DIR/lscpu.txt"
printf '%s\n' "$cases_text" >"$BENCHMARK_DIR/cases.txt"

# Keep only the first N [[pairs]] sections of the base config. The collector
# always samples every configured pair on every block, so universe size is the
# load dimension, not a per-head sampling limit.
truncate_pairs() {
    local base_config="$1" pair_count="$2" output="$3"
    python3 - "$base_config" "$pair_count" >"$output" <<'PY'
import sys

path, keep = sys.argv[1], int(sys.argv[2])
sections = open(path, encoding="utf-8").read().split("\n\n")
kept, pairs_seen = [], 0
for section in sections:
    if section.lstrip().startswith("[[pairs]]"):
        pairs_seen += 1
        if pairs_seen > keep:
            continue
    kept.append(section)
if pairs_seen < keep:
    sys.exit(f"base config has only {pairs_seen} pairs, case requests {keep}")
print("\n\n".join(kept), end="")
PY
}

for benchmark_case in "${cases[@]}"; do
    IFS=: read -r cpus workers pair_count <<<"$benchmark_case"
    for value in "$cpus" "$workers" "$pair_count"; do
        [[ "$value" =~ ^[1-9][0-9]*$ ]] || {
            echo "invalid benchmark case: $benchmark_case" >&2
            exit 2
        }
    done
    if ((cpus > $(nproc))); then
        echo "case requests $cpus CPUs, but only $(nproc) are available" >&2
        exit 2
    fi

    name="cpu${cpus}-workers${workers}-pairs${pair_count}"
    config="$BENCHMARK_DIR/configs/$name.toml"
    run_dir="$BENCHMARK_DIR/runs/$name"
    log="$BENCHMARK_DIR/$name.log"
    truncate_pairs "$BASE_CONFIG" "$pair_count" "$config.pairs"
    sed -E \
        -e "s/^num_workers = [0-9]+$/num_workers = $workers/" \
        "$config.pairs" >"$config"
    rm -f "$config.pairs"

    echo "starting $name for $max_heads heads"
    RUST_LOG="pairs_data_collector=info,fynd_core=warn" \
        taskset --cpu-list "0-$((cpus - 1))" \
        "$COLLECTOR_BINARY" collect \
        --config "$config" --output-dir "$run_dir" --max-heads "$max_heads" \
        >"$log" 2>&1
    "$script_dir/summarize_run.py" "$run_dir" \
        --output "$BENCHMARK_DIR/summaries/$name.json" >/dev/null
    echo "completed $name"
done

#!/usr/bin/env bash
set -euo pipefail

: "${BENCHMARK_BINARY:?BENCHMARK_BINARY must point to fynd-benchmark}"
: "${BASE_CONFIG:?BASE_CONFIG must point to a single-pool TOML}"
: "${BENCHMARK_DIR:?BENCHMARK_DIR must name an output directory}"
: "${TYCHO_API_KEY_BETA:?TYCHO_API_KEY_BETA is required}"
: "${RPC_URL:?RPC_URL is required}"

readonly protocols="${PROTOCOLS:-uniswap_v2,uniswap_v3,uniswap_v4,pancakeswap_v3,sushiswap_v2}"
readonly min_tvl="${MIN_TVL:-3.0}"
readonly requests="${NUM_REQUESTS:-5000}"
readonly warmup="${WARMUP_SECS:-15}"
readonly repetitions="${REPETITIONS:-3}"
readonly cases_text="${BENCHMARK_CASES:-2:2:8 4:4:16 8:8:32 16:16:64}"
read -r -a cases <<<"$cases_text"

[[ "$repetitions" =~ ^[1-9][0-9]*$ ]] || {
    echo "REPETITIONS must be a positive integer" >&2
    exit 2
}

mkdir -p "$BENCHMARK_DIR"
lscpu >"$BENCHMARK_DIR/lscpu.txt"
printf '%s\n' "$cases_text" >"$BENCHMARK_DIR/cases.txt"

for benchmark_case in "${cases[@]}"; do
    IFS=: read -r cpus workers concurrency <<<"$benchmark_case"
    for value in "$cpus" "$workers" "$concurrency"; do
        [[ "$value" =~ ^[1-9][0-9]*$ ]] || {
            echo "invalid benchmark case: $benchmark_case" >&2
            exit 2
        }
    done
    if ((cpus > $(nproc))); then
        echo "case requests $cpus CPUs, but only $(nproc) are available" >&2
        exit 2
    fi

    for ((repetition = 1; repetition <= repetitions; repetition++)); do
        name="cpu${cpus}-workers${workers}-concurrency${concurrency}-rep${repetition}"
        arguments=(
            scale
            --base-config "$BASE_CONFIG"
            --worker-counts "$workers"
            --protocols "$protocols"
            --tycho-url tycho-beta.propellerheads.xyz
            --min-tvl "$min_tvl"
            --num-requests "$requests"
            --parallelization-mode "fixed:$concurrency"
            --warmup-secs "$warmup"
            --output-file "$BENCHMARK_DIR/$name.json"
        )
        if [[ -n "${REQUESTS_FILE:-}" ]]; then
            arguments+=(--requests-file "$REQUESTS_FILE")
        fi

        echo "starting $name"
        TYCHO_API_KEY="$TYCHO_API_KEY_BETA" \
            RUST_LOG="fynd_benchmark=info,fynd_core=off,fynd_rpc=off" \
            taskset --cpu-list "0-$((cpus - 1))" \
            "$BENCHMARK_BINARY" "${arguments[@]}" \
            >"$BENCHMARK_DIR/$name.log" 2>&1
        echo "completed $name"
    done
done

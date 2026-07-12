#!/usr/bin/env bash
set -euo pipefail

deploy_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly deploy_dir
temporary="$(mktemp -d)"
readonly temporary
trap 'find "$temporary" -depth -delete' EXIT

mkdir -p "$temporary/bin" "$temporary/remote"
cat >"$temporary/bin/rclone" <<'FAKE_RCLONE'
#!/usr/bin/env bash
set -euo pipefail
command="$1"
source_path="${2:-}"
if [[ "$command" == copyto ]]; then
    destination="$3"
else
    destination="$source_path"
fi
remote_path="${destination#s3:fyndquotes/test/}"
target="$FAKE_REMOTE/$remote_path"
case "$command" in
copyto)
    mkdir -p "$(dirname "$target")"
    cp "$source_path" "$target"
    ;;
lsf)
    marker="${5:-}"
    [[ ! -f "$target/$marker" ]] || printf '%s\n' "$marker"
    ;;
rcat)
    mkdir -p "$(dirname "$target")"
    cat >"$target"
    ;;
*)
    echo "unsupported fake rclone command: $command" >&2
    exit 1
    ;;
esac
FAKE_RCLONE
chmod +x "$temporary/bin/rclone"
cat >"$temporary/bin/flock" <<'FAKE_FLOCK'
#!/usr/bin/env bash
set -euo pipefail
exit 0
FAKE_FLOCK
chmod +x "$temporary/bin/flock"

readonly output="$temporary/output"
mkdir -p \
    "$output/wal" \
    "$output/parquet/quote_points" \
    "$output/parquet/block_runs" \
    "$output/parquet/block_status_events" \
    "$output/parquet/manifests"

make_segment() {
    local stem="$1"
    local files=(
        "$output/wal/$stem.ndjson"
        "$output/parquet/quote_points/part-$stem.parquet"
        "$output/parquet/block_runs/part-$stem.parquet"
        "$output/parquet/block_status_events/part-$stem.parquet"
        "$output/parquet/manifests/$stem.json"
    )
    for file in "${files[@]}"; do
        printf 'fixture:%s\n' "$file" >"$file"
        [[ "$file" == "$output"/wal/* ]] || (
            cd "$(dirname "$file")"
            sha256sum "$(basename "$file")" >"$(basename "$file").sha256"
        )
    done
}

make_segment run-1
env \
    FAKE_REMOTE="$temporary/remote" \
    OUTPUT_DIR="$output" \
    S3_DEST=s3:fyndquotes/test \
    LOCAL_RETENTION_HOURS=999 \
    PATH="$temporary/bin:$PATH" \
    "$deploy_dir/upload-segments.sh"

[[ -f "$temporary/remote/complete/run-1.json" ]]
[[ -f "$temporary/remote/wal/run-1.ndjson" ]]
[[ -f "$temporary/remote/parquet/quote_points/part-run-1.parquet" ]]

make_segment run-prune
touch -t 200001010000 "$output/parquet/manifests/run-prune.json"
env \
    FAKE_REMOTE="$temporary/remote" \
    OUTPUT_DIR="$output" \
    S3_DEST=s3:fyndquotes/test \
    LOCAL_RETENTION_HOURS=0 \
    PATH="$temporary/bin:$PATH" \
    "$deploy_dir/upload-segments.sh"
[[ -f "$temporary/remote/complete/run-prune.json" ]]
[[ ! -e "$output/parquet/manifests/run-prune.json" ]]
[[ ! -e "$output/wal/run-prune.ndjson" ]]

make_segment run-2
printf 'corrupt\n' >>"$output/parquet/quote_points/part-run-2.parquet"
if env \
    FAKE_REMOTE="$temporary/remote" \
    OUTPUT_DIR="$output" \
    S3_DEST=s3:fyndquotes/test \
    LOCAL_RETENTION_HOURS=999 \
    PATH="$temporary/bin:$PATH" \
    "$deploy_dir/upload-segments.sh"; then
    echo "uploader accepted a corrupt finalized segment" >&2
    exit 1
fi

[[ ! -e "$temporary/remote/complete/run-2.json" ]]
echo "upload segment tests passed"

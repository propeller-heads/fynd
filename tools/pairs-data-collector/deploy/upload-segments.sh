#!/usr/bin/env bash
set -euo pipefail

: "${OUTPUT_DIR:?OUTPUT_DIR must name the collector output directory}"
: "${S3_DEST:?S3_DEST must name the rclone destination prefix}"

for command in find flock rclone sha256sum; do
    command -v "$command" >/dev/null || {
        echo "required command is unavailable: $command" >&2
        exit 1
    }
done

mkdir -p "$OUTPUT_DIR"
exec 9>"$OUTPUT_DIR/.s3-upload.lock"
flock -n 9 || exit 0

verify_checksum() {
    local checksum="$1"
    (
        cd "$(dirname "$checksum")"
        sha256sum --check --status "$(basename "$checksum")"
    )
}

upload_file() {
    local path="$1"
    local relative="${path#"$OUTPUT_DIR"/}"
    rclone copyto "$path" "$S3_DEST/$relative" \
        --checksum --immutable --s3-no-check-bucket
}

remote_complete() {
    local marker="$1.json"
    [[ "$(rclone lsf "$S3_DEST/complete" --files-only --include "$marker" \
        --s3-no-check-bucket 2>/dev/null)" == "$marker" ]]
}

shopt -s nullglob
for manifest in "$OUTPUT_DIR"/parquet/manifests/*.json; do
    stem="$(basename "$manifest" .json)"
    remote_complete "$stem" && continue

    files=(
        "$OUTPUT_DIR/wal/$stem.ndjson"
        "$OUTPUT_DIR/parquet/quote_points/part-$stem.parquet"
        "$OUTPUT_DIR/parquet/block_runs/part-$stem.parquet"
        "$manifest"
    )
    status="$OUTPUT_DIR/parquet/block_status_events/part-$stem.parquet"
    [[ ! -e "$status" ]] || files+=("$status")

    for file in "${files[@]}"; do
        checksum="$file.sha256"
        [[ -s "$file" ]] || {
            echo "finalized segment is incomplete, missing $file" >&2
            exit 1
        }
        if [[ "$file" == "$OUTPUT_DIR"/wal/* && ! -s "$checksum" ]]; then
            temporary="$checksum.tmp"
            (
                cd "$(dirname "$file")"
                sha256sum "$(basename "$file")"
            ) >"$temporary"
            mv "$temporary" "$checksum"
        fi
        [[ -s "$checksum" ]] || {
            echo "finalized segment is missing checksum $checksum" >&2
            exit 1
        }
        verify_checksum "$checksum" || {
            echo "checksum verification failed for $file" >&2
            exit 1
        }
        upload_file "$file"
        upload_file "$checksum"
    done

    printf '{"segment":"%s","uploaded_at":"%s"}\n' \
        "$stem" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" |
        rclone rcat "$S3_DEST/complete/$stem.json" --s3-no-check-bucket
    echo "uploaded finalized segment $stem"
done

for wal in "$OUTPUT_DIR"/wal/*.ndjson; do
    stem="$(basename "$wal" .ndjson)"
    [[ -e "$OUTPUT_DIR/parquet/manifests/$stem.json" ]] && continue
    rclone copyto "$wal" "$S3_DEST/recovery/wal/$stem.ndjson" \
        --s3-no-check-bucket
done

retention_hours="${LOCAL_RETENTION_HOURS:-48}"
[[ "$retention_hours" =~ ^[0-9]+$ ]] || {
    echo "LOCAL_RETENTION_HOURS must be a non-negative integer" >&2
    exit 1
}
retention_minutes=$((retention_hours * 60))
while IFS= read -r -d '' manifest; do
    stem="$(basename "$manifest" .json)"
    remote_complete "$stem" || continue
    local_files=(
        "$OUTPUT_DIR/wal/$stem.ndjson"
        "$OUTPUT_DIR/wal/$stem.ndjson.sha256"
        "$OUTPUT_DIR/parquet/quote_points/part-$stem.parquet"
        "$OUTPUT_DIR/parquet/quote_points/part-$stem.parquet.sha256"
        "$OUTPUT_DIR/parquet/block_runs/part-$stem.parquet"
        "$OUTPUT_DIR/parquet/block_runs/part-$stem.parquet.sha256"
        "$OUTPUT_DIR/parquet/block_status_events/part-$stem.parquet"
        "$OUTPUT_DIR/parquet/block_status_events/part-$stem.parquet.sha256"
        "$manifest"
        "$manifest.sha256"
    )
    rm -f -- "${local_files[@]}"
    echo "pruned locally retained segment $stem"
done < <(
    find "$OUTPUT_DIR/parquet/manifests" -type f -name '*.json' \
        -mmin "+$retention_minutes" -print0 2>/dev/null
)

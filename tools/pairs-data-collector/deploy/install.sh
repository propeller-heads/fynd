#!/usr/bin/env bash
set -euo pipefail

: "${COLLECTOR_BINARY:?COLLECTOR_BINARY must point to pairs-data-collector}"
: "${COLLECTOR_CONFIG:?COLLECTOR_CONFIG must point to the production TOML}"
: "${S3_DEST:?S3_DEST must name the production rclone prefix}"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly script_dir
readonly install_dir="/opt/fynd-pairs"
readonly output_dir="/var/lib/fynd-pairs"
readonly retention_hours="${LOCAL_RETENTION_HOURS:-48}"

[[ -x "$COLLECTOR_BINARY" ]] || {
    echo "collector binary is not executable: $COLLECTOR_BINARY" >&2
    exit 1
}
[[ -r "$COLLECTOR_CONFIG" ]] || {
    echo "collector config is not readable: $COLLECTOR_CONFIG" >&2
    exit 1
}
[[ "$retention_hours" =~ ^[0-9]+$ ]] || {
    echo "LOCAL_RETENTION_HOURS must be a non-negative integer" >&2
    exit 1
}

sudo install -d -m 0755 "$install_dir"
sudo install -d -o agent -g agent -m 0750 "$output_dir"
sudo install -m 0755 "$COLLECTOR_BINARY" "$install_dir/pairs-data-collector"
sudo install -m 0755 \
    "$script_dir/run-with-secrets.sh" \
    "$script_dir/upload-segments.sh" \
    "$install_dir/"
sudo install -m 0644 "$COLLECTOR_CONFIG" /etc/fynd-pairs-collector.toml

temporary="$(mktemp)"
trap 'rm -f "$temporary"' EXIT
printf '%s\n' \
    'COLLECTOR_CONFIG=/etc/fynd-pairs-collector.toml' \
    "OUTPUT_DIR=$output_dir" \
    "S3_DEST=$S3_DEST" \
    "LOCAL_RETENTION_HOURS=$retention_hours" >"$temporary"
sudo install -m 0644 "$temporary" /etc/fynd-pairs-collector.env

sudo install -m 0644 \
    "$script_dir/fynd-pairs-collector.service" \
    "$script_dir/fynd-pairs-upload.service" \
    "$script_dir/fynd-pairs-upload.timer" \
    /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable fynd-pairs-upload.timer

echo "installed collector without starting it"
echo "start with: sudo systemctl enable --now fynd-pairs-collector.service"

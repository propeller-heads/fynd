#!/usr/bin/env bash
# Serve the benchmark result viewer.
#
# The viewer is a single static page that reads bench-results/ over HTTP. It has no build step and
# no dependencies -- this script only exists because browsers refuse the reads it needs when a page
# is opened straight from disk with file://.
#
# New runs appear on refresh; the run picker switches between them.
#
# Usage:
#   ./scripts/bench-viewer.sh [--port N] [--no-open]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PORT=8321
OPEN=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    --port)
      PORT="$2"
      shift 2
      ;;
    --no-open)
      OPEN=0
      shift
      ;;
    -h | --help)
      sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "error: unknown option '$1'" >&2
      exit 1
      ;;
  esac
done

if [[ ! -d bench-results ]]; then
  echo "note: no bench-results/ yet — the viewer will say so until a run exists." >&2
  echo "      ./scripts/bench.sh --name my-run --orders 500" >&2
fi

URL="http://localhost:${PORT}/fynd-core/benches/viewer/"
echo "Viewer   $URL"
echo "Serving  $REPO_ROOT"
echo "Stop with Ctrl-C."

if [[ $OPEN -eq 1 ]] && command -v open >/dev/null 2>&1; then
  # give the server a moment before the browser asks for the page
  (sleep 1 && open "$URL") &
fi

exec python3 -m http.server "$PORT" --bind 127.0.0.1

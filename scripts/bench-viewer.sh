#!/usr/bin/env bash
# Serve the benchmark result viewer.
#
# The viewer is a single static page that reads bench-results/ over HTTP. It has no build step and
# no dependencies -- this script only exists because browsers refuse the reads it needs when a page
# is opened straight from disk with file://.
#
# New runs appear on refresh; the run picker switches between them.
#
# By default it reads this repo's bench-results/. --results names another directory of runs -- what
# a crate outside this repo writes when it benchmarks its own algorithm. That directory is served
# at /results and the page is opened with ?root=/results, so one server answers both the page and
# the runs; a second server would be a second origin, and the browser would refuse the reads.
#
# Usage:
#   ./scripts/bench-viewer.sh [--port N] [--no-open] [--results DIR]
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PORT=8321
OPEN=1
RESULTS=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --port)
      PORT="$2"
      shift 2
      ;;
    --results)
      RESULTS="$2"
      shift 2
      ;;
    --no-open)
      OPEN=0
      shift
      ;;
    -h | --help)
      sed -n '2,16p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "error: unknown option '$1'" >&2
      exit 1
      ;;
  esac
done

URL="http://localhost:${PORT}/bench-harness/viewer/"

if [[ -n "$RESULTS" ]]; then
  if [[ ! -d "$RESULTS" ]]; then
    echo "error: '$RESULTS' is not a directory" >&2
    exit 1
  fi
  RESULTS="$(cd "$RESULTS" && pwd)"
  URL="${URL}?root=/results"
  echo "Results  $RESULTS"
elif [[ ! -d bench-results ]]; then
  echo "note: no bench-results/ yet — the viewer will say so until a run exists." >&2
  echo "      ./scripts/bench.sh --name my-run --orders 500" >&2
fi

echo "Viewer   $URL"
echo "Serving  $REPO_ROOT"
echo "Stop with Ctrl-C."

if [[ $OPEN -eq 1 ]] && command -v open >/dev/null 2>&1; then
  # give the server a moment before the browser asks for the page
  (sleep 1 && open "$URL") &
fi

if [[ -z "$RESULTS" ]]; then
  exec python3 -m http.server "$PORT" --bind 127.0.0.1
fi

# The page and the runs have to come from one origin, so this serves the repo and maps /results/
# onto the named directory.
exec python3 - "$PORT" "$REPO_ROOT" "$RESULTS" <<'PYTHON'
import functools
import http.server
import os
import posixpath
import sys
import urllib.parse

port, repo_root, results = int(sys.argv[1]), sys.argv[2], sys.argv[3]
PREFIX = "/results"


class Handler(http.server.SimpleHTTPRequestHandler):
    def translate_path(self, path):
        clean = urllib.parse.unquote(path.split("?", 1)[0].split("#", 1)[0])
        if clean != PREFIX and not clean.startswith(PREFIX + "/"):
            return super().translate_path(path)
        parts = [p for p in posixpath.normpath(clean[len(PREFIX):]).split("/") if p not in ("", ".", "..")]
        return os.path.join(results, *parts)


address = ("127.0.0.1", port)
handler = functools.partial(Handler, directory=repo_root)
http.server.ThreadingHTTPServer(address, handler).serve_forever()
PYTHON

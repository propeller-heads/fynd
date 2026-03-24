#!/usr/bin/env bash
set -euo pipefail

# Verify that anchored source snippets appear verbatim in the corresponding doc files.
# Anchors use [doc:start <name>] / [doc:end <name>] comments in source files.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Format: "source_file:anchor_name:doc_file"
SNIPPETS=(
    "clients/rust/examples/quote.rs:quote-rust:docs/get-started/quickstart.md"
    "clients/typescript/examples/tutorial/main.ts:quote-typescript:docs/get-started/quickstart.md"
    "fynd-core/examples/custom_algorithm.rs:custom-algo-impl:docs/guides/custom-algorithm.md"
    "fynd-core/examples/custom_algorithm.rs:custom-algo-wire:docs/guides/custom-algorithm.md"
)

exit_code=0

check_snippet() {
    local source_file="$1" anchor_name="$2" doc_file="$3"
    local source_path="$REPO_ROOT/$source_file"
    local doc_path="$REPO_ROOT/$doc_file"
    local tmp_snippet
    tmp_snippet=$(mktemp)

    awk "/\[doc:start ${anchor_name}\]/{found=1; next} /\[doc:end ${anchor_name}\]/{found=0} found{print}" \
        "$source_path" > "$tmp_snippet"

    if [ ! -s "$tmp_snippet" ]; then
        echo "ERROR: anchor '${anchor_name}' not found in ${source_file}"
        rm -f "$tmp_snippet"
        return 1
    fi

    if python3 - "$tmp_snippet" "$doc_path" <<'EOF'
import sys
with open(sys.argv[1]) as f:
    snippet = f.read()
with open(sys.argv[2]) as f:
    doc = f.read()
sys.exit(0 if snippet in doc else 1)
EOF
    then
        echo "OK: ${anchor_name} (${source_file} -> ${doc_file})"
        rm -f "$tmp_snippet"
        return 0
    else
        echo "DRIFT: '${anchor_name}' in ${doc_file} does not match source in ${source_file}"
        echo "--- Source snippet (${source_file}) ---"
        cat "$tmp_snippet"
        rm -f "$tmp_snippet"
        return 1
    fi
}

for entry in "${SNIPPETS[@]}"; do
    IFS=: read -r src anchor doc <<< "$entry"
    if ! check_snippet "$src" "$anchor" "$doc"; then
        exit_code=1
    fi
done

exit $exit_code

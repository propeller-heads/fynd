#!/usr/bin/env bash
# Regenerate the OpenAPI spec and all derived artefacts from the current server code.
#
# Run this after any change to fynd-rpc-types or fynd-rpc that affects the API
# surface (new fields, new endpoints, changed types):
#
#   ./scripts/update-openapi.sh
#
# What it does:
#   1. Builds the server binary and exports the OpenAPI spec to clients/openapi.json
#   2. Regenerates clients/typescript/autogen/src/schema.d.ts via openapi-typescript
#
# Requirements:
#   - Rust toolchain (cargo)
#   - openapi-typescript (npx will download it on first use)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OPENAPI_JSON="$REPO_ROOT/clients/openapi.json"
TS_SCHEMA="$REPO_ROOT/clients/typescript/client/src/schema.d.ts"

echo "==> Generating OpenAPI spec..."
cargo run --manifest-path "$REPO_ROOT/Cargo.toml" -- openapi 2>/dev/null >"$OPENAPI_JSON"
echo "    Written: $OPENAPI_JSON"

echo "==> Regenerating TypeScript schema..."
npx --yes openapi-typescript "$OPENAPI_JSON" -o "$TS_SCHEMA"
echo "    Written: $TS_SCHEMA"

echo "Done. Commit both files if they changed."

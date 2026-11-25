#!/usr/bin/env bash
# Verify the published shape of @kayibal/fynd-client.
#
# Packs the client the same way `npm publish` would, installs the tarball into a
# throwaway fixture, then checks that consumers can actually load it:
#   - CommonJS `require()` (Node >= 22.12 loads the ESM build synchronously)
#   - ESM `import`
#   - TypeScript type resolution under `moduleResolution: node` (the default for
#     `module: commonjs`, still used by NestJS and other CJS toolchains)
#   - TypeScript type resolution under `moduleResolution: nodenext`
#
# Known limitation, deliberately not asserted here: a consumer on
# `module: Node16` that emits CommonJS gets TS1479, because the package is
# `"type": "module"` and ships a single ESM `index.d.ts`. Such consumers need
# `module: nodenext` with TypeScript >= 5.8. Fixing that needs a flattened
# `index.d.cts`, which needs a declaration bundler.
#
# Usage:
#   ./scripts/check-client-packaging.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TS_ROOT="$REPO_ROOT/clients/typescript"
CLIENT_DIR="$TS_ROOT/client"
TSC="$CLIENT_DIR/node_modules/.bin/tsc"

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

echo "==> Building the client..."
pnpm --dir "$TS_ROOT" install --frozen-lockfile --silent
pnpm --dir "$TS_ROOT" --filter @kayibal/fynd-client --silent run build
[[ -x "$TSC" ]] || fail "tsc not found at $TSC"

echo "==> Packing the client..."
tarball="$(cd "$CLIENT_DIR" && npm pack --silent --pack-destination "$WORK_DIR")"
[[ -f "$WORK_DIR/$tarball" ]] || fail "npm pack produced no tarball"
echo "    $tarball"

echo "==> Installing the tarball into a fixture..."
cd "$WORK_DIR"
echo '{"name":"packaging-fixture","version":"0.0.0","private":true}' > package.json
npm install --silent --no-audit --no-fund "$WORK_DIR/$tarball"

# The exports map must resolve for both module systems, and the named exports
# must survive the require(esm) boundary.
cat > require.cjs <<'EOF'
const pkg = require("@kayibal/fynd-client");
const expected = ["FyndClient", "FyndError", "createFyndClient", "swapSigningHash", "withPermit2"];
for (const name of expected) {
  if (typeof pkg[name] !== "function") {
    throw new Error(`require(): expected ${name} to be a function, got ${typeof pkg[name]}`);
  }
}
new pkg.FyndClient({ baseUrl: "http://127.0.0.1:9" });
EOF

cat > import.mjs <<'EOF'
import { FyndClient, FyndError, createFyndClient } from "@kayibal/fynd-client";
for (const [name, value] of Object.entries({ FyndClient, FyndError, createFyndClient })) {
  if (typeof value !== "function") {
    throw new Error(`import: expected ${name} to be a function, got ${typeof value}`);
  }
}
new FyndClient({ baseUrl: "http://127.0.0.1:9" });
EOF

# One module instance must serve both entry conditions. Two copies would give
# two sets of module state to a consumer that mixes require() and import.
cat > single-instance.mjs <<'EOF'
import { createRequire } from "node:module";
import { FyndClient } from "@kayibal/fynd-client";
const required = createRequire(import.meta.url)("@kayibal/fynd-client");
if (required.FyndClient !== FyndClient) {
  throw new Error("require() and import gave different FyndClient classes");
}
EOF

echo "==> Checking CommonJS require()..."
node require.cjs || fail "a CommonJS consumer cannot require() the package"

echo "==> Checking ESM import..."
node import.mjs || fail "an ESM consumer cannot import the package"

echo "==> Checking that both conditions share one module instance..."
node single-instance.mjs || fail "the package loads twice when require() and import are mixed"

cat > consumer.ts <<'EOF'
import { FyndClient, FyndError } from "@kayibal/fynd-client";
import type { FyndClientOptions, Quote } from "@kayibal/fynd-client";

const options: FyndClientOptions = { baseUrl: "http://127.0.0.1:9" };
const client = new FyndClient(options);
declare const quote: Quote;
export const check = { client, quote, error: FyndError };
EOF

# `moduleResolution: node` ignores the exports map, so it needs the top-level
# `main` and `types` fields. `nodenext` reads the exports map instead.
for mode in node nodenext; do
    case "$mode" in
    node) module="commonjs" ;;
    nodenext) module="nodenext" ;;
    esac
    cat > "tsconfig.$mode.json" <<EOF
{
  "compilerOptions": {
    "module": "$module",
    "moduleResolution": "$mode",
    "strict": true,
    "noEmit": true,
    "esModuleInterop": true,
    "skipLibCheck": true
  },
  "files": ["consumer.ts"]
}
EOF
    echo "==> Checking TypeScript with moduleResolution: $mode (module: $module)..."
    "$TSC" -p "tsconfig.$mode.json" || fail "TypeScript cannot resolve the package with moduleResolution: $mode"
done

# The shipped declarations must resolve on their own. `tsc` does not copy
# hand-written `.d.ts` sources into the output directory, so a missing copy step
# leaves a dangling import that only shows up without `skipLibCheck`. Errors
# from other packages' declarations are out of scope here.
cat > tsconfig.libcheck.json <<'EOF'
{
  "compilerOptions": {
    "module": "nodenext",
    "moduleResolution": "nodenext",
    "strict": true,
    "noEmit": true,
    "skipLibCheck": false
  },
  "files": ["consumer.ts"]
}
EOF
echo "==> Checking that the shipped declarations are self-contained..."
own_errors="$("$TSC" -p tsconfig.libcheck.json 2>&1 | grep -F "node_modules/@kayibal/fynd-client/" || true)"
if [[ -n "$own_errors" ]]; then
    echo "$own_errors" >&2
    fail "the shipped declarations have unresolved imports"
fi

echo ""
echo "All packaging checks passed."

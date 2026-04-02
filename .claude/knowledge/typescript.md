# TypeScript Client (`@kayibal/fynd-client`)

TypeScript client for the Fynd RPC API. Lives in `clients/typescript/` as a pnpm workspace with
two packages.

## Workspace Structure

```
clients/typescript/
  pnpm-workspace.yaml
  pnpm-lock.yaml
  autogen/                    # @fynd/autogen — generated OpenAPI types
    src/
      schema.d.ts             # Auto-generated from clients/openapi.json
      index.ts                # Re-exports
  client/                     # @kayibal/fynd-client — typed HTTP client
    src/
      client.ts               # FyndClient — main client class
      types.ts                # Public types (QuoteRequest, QuoteResponse, etc.)
      mapping.ts              # Maps between API schema types and public types
      signing.ts              # EIP-712 signing utilities
      permit2.ts              # Permit2 helpers
      client-fee.ts           # Client fee helpers
      error.ts                # Error types
      viem.ts                 # Viem integration utilities
      index.ts                # Public API re-exports
```

## Build & Test

```bash
# Install dependencies
pnpm --dir clients/typescript install --frozen-lockfile

# Build autogen first (client depends on it)
pnpm --dir clients/typescript --filter @fynd/autogen run build

# Typecheck, lint, test the client
pnpm --dir clients/typescript --filter @kayibal/fynd-client run typecheck
pnpm --dir clients/typescript --filter @kayibal/fynd-client run lint
pnpm --dir clients/typescript --filter @kayibal/fynd-client run test
```

## Examples

Examples live in `clients/typescript/examples/<name>/main.ts` and run against a local
Anvil fork + Fynd instance.

**Adding a new example:**
1. Create `clients/typescript/examples/<name>/main.ts`
2. Add `<name>` to the `TS_EXAMPLES` array in `scripts/run-all-examples.sh`

The CI script (`scripts/run-all-examples.sh`) shares a single Anvil + Fynd instance across
all Rust and TS examples to keep load on the Tycho service low. TS packages are built once
upfront via `build_ts` in `scripts/_env-setup.sh` before any examples run.

Run all examples locally:
```bash
TYCHO_API_KEY=<key> ./scripts/run-all-examples.sh
```

## Key Conventions

- ESM only (`"type": "module"`)
- Tooling: `oxlint` for linting, `vitest` for tests, `tsc` for type checking
- Colocated test files (`*.test.ts` next to source)
- `@kayibal/fynd-client` depends on `@fynd/autogen` for schema types
- When adding/changing RPC endpoints, update: Rust types → OpenAPI spec → autogen → client mapping

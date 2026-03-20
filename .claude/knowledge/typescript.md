# TypeScript Client (`@fynd/client`)

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
  client/                     # @fynd/client — typed HTTP client
    src/
      client.ts               # FyndClient — main client class
      types.ts                # Public types (QuoteRequest, QuoteResponse, etc.)
      mapping.ts              # Maps between API schema types and public types
      signing.ts              # EIP-712 signing utilities
      permit2.ts              # Permit2 helpers
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
pnpm --dir clients/typescript --filter @fynd/client run typecheck
pnpm --dir clients/typescript --filter @fynd/client run lint
pnpm --dir clients/typescript --filter @fynd/client run test
```

## OpenAPI Codegen

The TypeScript types are generated from `clients/openapi.json`:

1. Update the OpenAPI spec: `./scripts/update-openapi.sh`
2. This regenerates `clients/openapi.json` from Rust types and `autogen/src/schema.d.ts` via
   `openapi-typescript`

CI checks for drift in both the OpenAPI spec and the generated TypeScript types.

## Key Conventions

- ESM only (`"type": "module"`)
- Tooling: `oxlint` for linting, `vitest` for tests, `tsc` for type checking
- Colocated test files (`*.test.ts` next to source)
- `@fynd/client` depends on `@fynd/autogen` for schema types
- When adding/changing RPC endpoints, update: Rust types → OpenAPI spec → autogen → client mapping

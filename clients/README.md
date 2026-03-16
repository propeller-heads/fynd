# clients/

This directory contains the OpenAPI spec and generated client packages for the fynd-rpc API.

## Structure

```
clients/
  openapi.json              # Committed OpenAPI spec (generated from Rust source)
  rust/                     # Rust client crate
  typescript/
    autogen/                # Auto-generated TypeScript types + fetch client
```

## Regenerating derived artefacts

`clients/openapi.json` and `clients/typescript/autogen/src/schema.d.ts` are both generated files
committed to source control. CI drift checks verify they match the binary output on every PR.

After changing any HTTP handler, request/response type, or route, run:

```bash
./scripts/update-openapi.sh
```

This rebuilds the server binary, exports `clients/openapi.json`, then regenerates the TypeScript schema
in one step. Commit both files afterwards.

### What the script does

| Step | Command |
|------|---------|
| Export OpenAPI spec | `cargo run -- openapi > clients/openapi.json` |
| Regenerate TS schema | `npx openapi-typescript clients/openapi.json -o clients/typescript/autogen/src/schema.d.ts` |

## Adding a new client

1. Create your client under `clients/<language>/`.
2. If your client has generated artefacts (types, SDKs) derived from `clients/openapi.json`, add the
   regeneration command to `scripts/update-openapi.sh` so a single script keeps everything in sync.
3. Add a CI drift check (similar to the existing `TypeScript Autogen Drift Check`) that fails if the
   committed artefact diverges from the generated output.

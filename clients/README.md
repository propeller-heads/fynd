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

## Regenerating clients/openapi.json

`clients/openapi.json` is generated from the Rust source via the `openapi` subcommand. It must be kept in
sync with the code — CI will fail if it drifts.

After changing any HTTP handler, request/response type, or route:

```bash
cargo run --locked -- openapi > clients/openapi.json
```

## Regenerating the TypeScript autogen schema

`clients/typescript/autogen/src/schema.d.ts` is generated from `clients/openapi.json` using
[openapi-typescript](https://openapi-ts.dev/). Regenerate it after updating `clients/openapi.json`:

```bash
npx openapi-typescript@7.13.0 clients/openapi.json -o clients/typescript/autogen/src/schema.d.ts
```

Both files are committed to source control. CI drift checks verify they match the binary output on every PR.

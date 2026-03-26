# fynd-rpc-types

Wire-format types for the [Fynd](https://fynd.xyz) RPC HTTP API.

This crate contains only the serialisation types (`QuoteRequest`, `Quote`, `Order`, …) shared
between the Fynd RPC server (`fynd-rpc`) and its clients (`fynd-client`). It has no HTTP or
server-side dependencies.

For documentation and API reference see **<https://docs.fynd.xyz/>**.

## Features

| Feature | Purpose |
|---------|---------|
| `openapi` | Derives `utoipa::ToSchema` on all types for OpenAPI spec generation |
| `core` | Enables `Into` conversions between wire DTOs and `fynd-core` domain types |

## Usage

This crate is typically pulled in transitively via `fynd-client` or `fynd-rpc`. Direct use is
only needed when implementing a custom server or client against the Fynd wire format.

```toml
[dependencies]
fynd-rpc-types = "0.35"
```

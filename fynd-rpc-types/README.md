# fynd-rpc-types

Licensed under the [Fynd License 1.0](https://github.com/propeller-heads/fynd/blob/main/LICENSE.md). This package is source-available, not MIT-licensed or distributed under an open-source license.

Wire-format types for the [Fynd](https://fynd.xyz) RPC HTTP API.

Shared between the Fynd RPC server (`fynd-rpc`) and its clients (`fynd-client`). No HTTP or
server-side dependencies.

For documentation and API reference visit **<https://docs.fynd.xyz/>**.

## Features

| Feature | Purpose |
|---------|---------|
| `openapi` | Derives `utoipa::ToSchema` for OpenAPI spec generation |
| `core` | Enables conversions between wire DTOs and `fynd-core` domain types |

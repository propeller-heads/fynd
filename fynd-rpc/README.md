# fynd-rpc

HTTP RPC server for the [Fynd](https://fynd.xyz) DEX router.

Wraps [`fynd-core`](https://crates.io/crates/fynd-core) with Actix Web and exposes swap routing
as a REST service on `http://0.0.0.0:3000` by default.

For documentation, configuration guides, and API reference visit **<https://docs.fynd.xyz/>**.

## Endpoints

| Endpoint | Description |
|---|---|
| `POST /v1/quote` | Request an optimal swap route |
| `GET /v1/health` | Data freshness and solver readiness |
| `GET /v1/info` | Static instance metadata (chain ID, contract addresses) |

## Quick start

See the [server configuration guide](https://docs.fynd.xyz/guides/server-configuration) and the
[quickstart](https://docs.fynd.xyz/get-started/quickstart).

# fynd-rpc-types

Shared DTO types for the Fynd RPC API. Contains only wire-format types — no server logic, no
actix-web dependency. Shared between `fynd-rpc` (server) and `fynd-client` (Rust client).

## Structure

Single file: `src/lib.rs`. All types derive `Serialize + Deserialize`.

## Features

| Feature | Effect |
|---|---|
| `openapi` | Derives `utoipa::ToSchema` on all types for OpenAPI spec generation |
| `core` | Enables `Into` conversions between wire DTOs and `fynd-core` domain types. Uses `Into` (not `From`) due to orphan rule |

## Request Types

- `QuoteRequest` — orders + options
- `Order` — token_in, token_out, amount, side (Sell only), sender, optional receiver
- `QuoteOptions` — timeout_ms, min_responses, max_gas, encoding_options
- `EncodingOptions` — slippage, transfer_type (TransferFrom / TransferFromPermit2 / None), permit, signature
- `PermitSingle`, `PermitDetails` — Permit2 authorization data

## Response Types

- `Quote` — orders (Vec<OrderQuote>), total_gas_estimate, solve_time_ms
- `OrderQuote` — order_id, status, route, amount_in/out, gas_estimate, price_impact_bps, amount_out_net_gas, block, gas_price, transaction
- `QuoteStatus` — Success, NoRouteFound, InsufficientLiquidity, Timeout, NotReady
- `Route` — Vec<Swap>
- `Swap` — component_id, protocol, token_in/out, amount_in/out, gas_estimate, split
- `BlockInfo` — number, hash, timestamp
- `Transaction` — to, value, data (hex-encoded calldata)
- `HealthStatus` — market_last_updated, derived_data_ready, num_components, num_solver_pools

## BigUint Serialization

All `BigUint` fields use `#[serde_as(as = "DisplayFromStr")]` — they serialize as decimal strings
in JSON (e.g. `"1000000000000000000"`), not numbers.

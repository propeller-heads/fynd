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
- `QuoteOptions` — timeout_ms, min_responses, max_gas, encoding_options, price_guard (optional `PriceGuardConfig`)
- `EncodingOptions` — slippage, transfer_type, permit, permit2_signature, client_fee_params
- `UserTransferType` — TransferFrom (default) / TransferFromPermit2 / UseVaultsFunds
- `ClientFeeParams` — bps, receiver, max_subsidy, signature (client fee on swap output)
- `PriceGuardConfig` — external price validation config (provider registry, deviation thresholds)
- `PermitSingle`, `PermitDetails` — Permit2 authorization data

## Response Types

- `Quote` — orders (Vec<OrderQuote>), total_gas_estimate, solve_time_ms
- `OrderQuote` — order_id, status, route, amount_in/out, gas_estimate, price_impact_bps, amount_out_net_gas, block, gas_price, transaction, fee_breakdown
- `FeeBreakdown` — router_fee, client_fee, max_slippage, min_amount_received (all absolute token-out amounts, populated when encoding_options is set)
- `QuoteStatus` — Success, NoRouteFound, InsufficientLiquidity, Timeout, NotReady
- `Route` — Vec<Swap>
- `Swap` — component_id, protocol, token_in/out, amount_in/out, gas_estimate, split
- `BlockInfo` — number, hash, timestamp
- `Transaction` — to, value, data (hex-encoded calldata)
- `HealthStatus` — market_last_updated, derived_data_ready, num_components, num_solver_pools
- `InstanceInfo` — static metadata returned by `GET /v1/info` (version, chain, spender address)

## OpenAPI Codegen

When types change here, regenerate the OpenAPI spec and TypeScript types:

1. Update the OpenAPI spec: `./scripts/update-openapi.sh`
2. This regenerates `clients/openapi.json` from Rust types and `autogen/src/schema.d.ts` via
   `openapi-typescript`

CI checks for drift in both the OpenAPI spec and the generated TypeScript types.

## BigUint Serialization

All `BigUint` fields use `#[serde_as(as = "DisplayFromStr")]` — they serialize as decimal strings
in JSON (e.g. `"1000000000000000000"`), not numbers.

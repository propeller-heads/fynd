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
- `QuoteOptions` — timeout_ms, min_responses, max_gas, encoding_options, route_filter
- `RouteFilter` — exclude_pools, exclude_protocols, exclude_tokens: the liquidity a request
  excludes from every route. Built with `with_excluded_pools` / `with_excluded_protocols` /
  `with_excluded_tokens`, read with `excluded_pools` / `excluded_protocols` / `excluded_tokens`
- `EncodingOptions` — slippage, transfer_type, permit, permit2_signature, client_fee_params, price_guard (optional `PriceGuardConfig`), simulate (ask for `simulation_result` on each quote)
- `UserTransferType` — TransferFrom (default) / TransferFromPermit2 / UseVaultsFunds
- `ClientFeeParams` — bps, receiver, max_contribution, deadline, signature (client fee on swap output)
- `PriceGuardConfig` — per-request price guard overrides, all optional: lower_tolerance_bps, upper_tolerance_bps, fail_on_provider_error, fail_on_token_price_not_found, enabled
- `PermitSingle`, `PermitDetails` — Permit2 authorization data

## Response Types

- `Quote` — orders (Vec<OrderQuote>), total_gas_estimate, solve_time_ms
- `OrderQuote` — order_id, status, route, amount_in/out, gas_estimate, price_impact_bps, amount_out_net_gas, block, gas_price, transaction, fee_breakdown, simulation_result, algorithm
- `FeeBreakdown` — router_fee, client_fee, max_slippage, min_amount_received (all absolute token-out amounts, populated when encoding_options is set), swaps_hash
- `QuoteStatus` — Success, NoRouteFound, InsufficientLiquidity, Timeout, NotReady, PriceCheckFailed, EncodingFailed
- `SimulationResult` — tagged by `status`: `success` (amount_out, gas_used) or `failure` (reason)
- `Route` — Vec<Swap>
- `Swap` — component_id, protocol, token_in/out, amount_in/out, gas_estimate, split, simulation_overrides (only on a pAMM swap whose state carried them)
- `SimulationOverrides` — block_number, block_timestamp (the block Titan quoted), accounts (`AccountOverride` by address). To reproduce a pAMM quote, simulate on `block_number - 1` with those as the `number` and `time` block overrides
- `AccountOverride` — one State Override Set account in its JSON-RPC encoding: `balance` and `nonce` as hex quantities (the private `HexQuantity` serde adapter), `stateDiff` (never `state`)
- `BlockInfo` — number, hash, timestamp
- `Transaction` — to, value, data (hex-encoded calldata), client_fee_signature_offset (where in `data` the client overwrites the placeholder client fee signature)
- `HealthStatus` — healthy, last_update_ms, num_solver_pools, derived_data_ready, gas_price_age_ms
- `InstanceInfo` — static metadata returned by `GET /v1/info` (chain_id, router_address, permit2_address, version)
- `ErrorResponse` — error body returned with non-2xx statuses

## OpenAPI Codegen

When types change here, regenerate the OpenAPI spec and TypeScript types:

1. Update the OpenAPI spec: `./scripts/update-openapi.sh`
2. This regenerates `clients/openapi.json` from Rust types and
   `clients/typescript/client/src/schema.d.ts` via `openapi-typescript`

CI checks for drift in both the OpenAPI spec and the generated TypeScript types.

## BigUint Serialization

All `BigUint` fields use `#[serde_as(as = "DisplayFromStr")]` — they serialize as decimal strings
in JSON (e.g. `"1000000000000000000"`), not numbers. The one exception is `AccountOverride::balance`,
a hex quantity, because a client passes that type straight to `eth_call`.

# Fynd

A high-performance DeFi route-finding engine built on [Tycho](https://www.propellerheads.xyz/tycho). Finds optimal swap
routes across multiple DeFi protocols in real-time.

> [!CAUTION]
  > **Alpha Software — Unaudited Contracts**
  >
  > Fynd's smart contracts ([TychoRouter V3](https://docs.propellerheads.xyz/tycho/for-solvers/execution#security-and-audits), Vault, Executors) are still undergoing a security audit. Funds stored in the router (including vault deposits) may be lost. Use at your own
   discretion.

## Features

- **Multi-protocol routing** - Routes through your favorite on-chain liquidity protocol, like Uniswap, Balancer, Curve,
  RFQ protocols, or any other protocol supported
  by [Tycho](https://docs.propellerheads.xyz/tycho/for-solvers/supported-protocols).
- **Real-time market data** - Tycho Stream keeps all liquidity states synchronized every block
- **Multi-algorithm competition** - Multiple solver pools run different algorithm configurations in parallel; the best
  result wins
- **Gas-aware ranking** - Solutions are ranked by net output after gas costs, not just raw output
- **Sub-100ms solves** - Dedicated OS threads for CPU-bound route finding, separate from the async I/O runtime
- **Production-ready** - Prometheus metrics, structured logging, health endpoints, graceful shutdown
- **Extensible** - Implement the `Algorithm` trait to add new routing strategies with zero framework changes
- **Modular** - Use just the core solving logic, or build a custom HTTP server with your own middleware

## Architecture

Fynd is organized into three crates:

- **`fynd-core`** - Pure solving logic with no HTTP dependencies. Use this if you want to integrate Fynd's routing
  algorithms into your own application.
- **`fynd-rpc`** - HTTP RPC server builder with customizable middleware. Use this to build a custom HTTP server with
  your own configuration.
- **`fynd`** - Complete CLI application that runs an HTTP RPC server. Use this to run Fynd as a standalone service.

## Prerequisites

- Rust 1.92+
- A Tycho API key ([get one here](https://www.propellerheads.xyz/tycho))

## Run the Solver

```bash
# Clone and build
git clone https://github.com/propeller-heads/fynd.git
cd fynd
cargo build --release

# Set required environment variables
export TYCHO_API_KEY=your-api-key
export RUST_LOG=fynd=info

# Run
cargo run --release serve
```

> **Note:** `--rpc-url` defaults to `https://eth.llamarpc.com`. For production, provide a dedicated endpoint:
> ```bash
> cargo run --release serve -- \
>   --tycho-url tycho-fynd-ethereum.propellerheads.xyz \
>   --rpc-url https://your-rpc-provider.com/v1/your_key \
>   --protocols uniswap_v2,uniswap_v3
> ```

The solver starts on `http://localhost:3000` by default.

### Including RFQ Protocols

You can include RFQ (Request-for-Quote) protocols alongside on-chain protocols:

```bash
cargo run --release serve \
  --protocols all_onchain,rfq:bebop
```

**Limitations:**

- RFQ protocols cannot run alone — at least one on-chain protocol is required.

**Environment variables:**

- RFQ protocols typically require API keys, which are passed via environment variables. Check
  the [RFQ protocol docs](https://docs.propellerheads.xyz/tycho/for-solvers/request-for-quote-protocols) for the
  specific variables each protocol needs.

### Run on a specific chain

You can run on any chain supported by Tycho (see [Tycho Hosted endpoint](https://docs.propellerheads.xyz/tycho/for-solvers/hosted-endpoints))

```bash
export RPC_URL=<RPC_FOR_TARGET_CHAIN>
cargo run --release serve --chain unichain
```

## Request a Quote

```bash
curl -X POST http://localhost:3000/v1/quote \
  -H "Content-Type: application/json" \
  -d '{
    "orders": [
      {
        "token_in": "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2",
        "token_out": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
        "amount": "1000000000000000000",
        "side": "sell",
        "sender": "0x0000000000000000000000000000000000000001"
      }
    ],
    "options": {
      "timeout_ms": 5000
    }
  }'
```

## Check Health

```bash
curl http://localhost:3000/v1/health
```

## API Reference

### POST /v1/quote

Submit one or more swap orders and receive optimal routes.

**Request:**

| Field                   | Type    | Required | Description                                                                                                   |
|-------------------------|---------|----------|---------------------------------------------------------------------------------------------------------------|
| `orders[].token_in`     | address | Yes      | Token to sell                                                                                                 |
| `orders[].token_out`    | address | Yes      | Token to buy                                                                                                  |
| `orders[].amount`       | string  | Yes      | Amount in token units (integer string)                                                                        |
| `orders[].side`         | string  | Yes      | `"sell"` (exact input)                                                                                        |
| `orders[].sender`       | address | Yes      | Sender address                                                                                                |
| `orders[].receiver`     | address | No       | Receiver (defaults to sender)                                                                                 |
| `options.timeout_ms`    | integer | No       | Solve timeout in ms (default: 100)                                                                            |
| `options.min_responses` | integer | No       | Early return threshold (default: 0, wait for all)                                                             |
| `options.max_gas`       | string  | No       | Max gas filter (no limit if omitted)                                                                          |
| `options.encoding_options.slippage` | float | No | Slippage tolerance for encoded transactions (e.g., `0.01` for 1%). No encoding if `encoding_options` is omitted |
| `options.encoding_options.transfer_type` | string | No | Input token transfer method: `transfer_from` (default) or `transfer_from_permit2`                             |
| `options.encoding_options.permit` | object | No | Permit2 single-token authorization. Required when using `transfer_from_permit2`                               |
| `options.encoding_options.permit2_signature` | string | No | Permit2 signature (hex-encoded). Required when `permit` is set                                                |
| `options.encoding_options.client_fee_params` | object | No | Client fee configuration. See [Client Fees](docs/guides/client-fees.md)                                       |

**Response:**

```json
{
  "orders": [
    {
      "order_id": "uuid",
      "status": "success",
      "route": {
        "swaps": [
          {
            "component_id": "0x...",
            "protocol": "uniswap_v3",
            "token_in": "0x...",
            "token_out": "0x...",
            "amount_in": "1000000000000000000",
            "amount_out": "3200000000",
            "gas_estimate": "150000",
            "split": "0.5"
          }
        ]
      },
      "amount_in": "1000000000000000000",
      "amount_out": "3200000000",
      "gas_estimate": "150000",
      "amount_out_net_gas": "3199500000",
      "gas_price": "25000000000",
      "block": {
        "number": 19000000,
        "hash": "0x...",
        "timestamp": 1700000000
      },
      "transaction": {
        "to": "0x...",
        "value": "0",
        "data": "0x..."
      },
      "fee_breakdown": {
        "router_fee": "320000",
        "client_fee": "0",
        "max_slippage": "31996800",
        "min_amount_received": "3167683200"
      }
    }
  ],
  "total_gas_estimate": "150000",
  "solve_time_ms": 45
}
```

### GET /v1/health

Returns service health status. HTTP 200 if healthy, 503 if unhealthy.

The service is healthy when market data is fresh (< 60s old), derived data has been
computed at least once, **and** gas price is not stale (when `--gas-price-stale-threshold-secs`
is configured). The `derived_data_ready` field indicates overall readiness, not per-block
freshness — algorithms that require fresh derived data will wait for recomputation before
solving.

## Configuration

### CLI / Environment Variables

| Flag                         | Env Var               | Default             | Description                                |
|------------------------------|-----------------------|---------------------|--------------------------------------------|
| `--rpc-url`                  | `RPC_URL`             | `https://eth.llamarpc.com` | Ethereum RPC endpoint for the target chain |
| `--tycho-url`                | `TYCHO_URL`           | *(chain-specific)*  | Tycho URL (e.g. `tycho-fynd-ethereum.propellerheads.xyz`) |
| `--tycho-api-key`            | `TYCHO_API_KEY`       | -                   | Tycho API key                              |
| `--chain`                    | -                     | `Ethereum`          | Target chain                               |
| `-p, --protocols`            | -                     | *(all available)*   | Protocols to index (comma-separated). Auto-fetched from Tycho RPC if omitted. |
| `--http-host`                | `HTTP_HOST`           | `0.0.0.0`           | HTTP bind address                          |
| `--http-port`                | `HTTP_PORT`           | `3000`              | API port                                   |
| `--min-tvl`                  | -                     | `10.0`              | Minimum pool TVL in native token           |
| `--worker-router-timeout-ms` | -                     | `100`               | Default solve timeout                      |
| `-w, --worker-pools-config`  | `WORKER_POOLS_CONFIG` | `worker_pools.toml` | Worker pools config                        |
| `--blocklist-config`         | `BLOCKLIST_CONFIG`    | `blocklist.toml`    | Path to blocklist TOML config file         |
| `--gas-price-stale-threshold-secs` | -               | *(disabled)*        | Health returns 503 when gas price exceeds this age |

See `--help` for the full list.

Find the list of all available protocols on
Tycho [here](https://docs.propellerheads.xyz/tycho/for-solvers/supported-protocols)

### Worker Pools (worker_pools.toml)

Configure solver pools with different algorithms and parameters:

```toml
[pools.fast_2hop]
algorithm = "most_liquid"
num_workers = 5
max_hops = 2
timeout_ms = 100

[pools.deep_3hop]
algorithm = "most_liquid"
num_workers = 3
min_hops = 2
max_hops = 3
timeout_ms = 5000
```

A Worker Pool runs a configurable number of Worker threads, all using the same algorithm and pulling tasks from a shared
queue. Each worker handles one order at a time — so a pool with 5 workers can solve up to 5 orders concurrently.

Multiple pools run in parallel, each producing its own solution per order. The system then picks the best result across
pools within the timeout.

**Example**: Given the config above and 3 incoming orders:

- `fast_2hop` assigns 1 worker per order (3/5 workers busy)
- `deep_3hop` assigns 1 worker per order (3/3 workers busy)

Each order gets 2 candidate solutions — one from each pool — and the best is selected.

### Blocklist Config

By default, Fynd loads `blocklist.toml` from the working directory. The default excludes components with known
simulation issues (e.g., rebasing tokens). Override with `--blocklist-config`:

```bash
cargo run --release serve -- --blocklist-config my_blocklist.toml
```

The config file uses a `[blocklist]` section with component IDs to exclude:

```toml
[blocklist]
components = [
    "0x86d257cdb7bc9c0df10e84c8709697f92770b335",
]
```

## Observability

- **Metrics**: Prometheus endpoint at `http://localhost:9898/metrics`
- **Logging**: Structured logging via `RUST_LOG` (e.g., `RUST_LOG=info,fynd=debug`)
- **Health**: `GET /v1/health` returns data freshness and pool count

## Extensibility

### Using a Custom Algorithm

Implement the `Algorithm` trait and plug it into a `WorkerPoolBuilder` via `with_algorithm()` — no changes to
fynd-core required:

```rust
let (pool, task_handle) = WorkerPoolBuilder::new()
    .name("my-solver")
    .with_algorithm("my_algo", |config| MyAlgorithm::new(config))
    .algorithm_config(algorithm_config)
    .num_workers(4)
    .build(market_data, derived_data, event_rx, derived_event_rx)?;
```

See the [`custom_algorithm` example](fynd-core/examples/custom_algorithm.rs) for a full walkthrough.


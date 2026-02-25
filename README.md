# Fynd

A high-performance DeFi route-finding engine built on [Tycho](https://www.propellerheads.xyz/tycho). Finds optimal swap routes across multiple DeFi protocols in real-time.

## Features

- **Multi-protocol routing** - Routes through your favorite on-chain liquidity protocol, like Uniswap, Balancer, Curve, RFQ protocols, or any other protocol supported by [Tycho](https://docs.propellerheads.xyz/tycho/for-solvers/supported-protocols).
- **Real-time market data** - Tycho Stream keeps all liquidity states synchronized every block
- **Multi-algorithm competition** - Multiple solver pools run different algorithm configurations in parallel; the best result wins
- **Gas-aware ranking** - Solutions are ranked by net output after gas costs, not just raw output
- **Sub-100ms solves** - Dedicated OS threads for CPU-bound route finding, separate from the async I/O runtime
- **Production-ready** - Prometheus metrics, structured logging, health endpoints, graceful shutdown
- **Extensible** - Implement the `Algorithm` trait to add new routing strategies with zero framework changes

## Prerequisites

- Rust 1.92+
- A Tycho API key ([get one here](https://www.propellerheads.xyz/tycho))
- An Ethereum RPC endpoint for the target chain

## Run the Solver

```bash
# Clone and build
git clone https://github.com/propeller-heads/fynd.git
cd fynd
cargo build --release

# Set required environment variables
export TYCHO_API_KEY=your-api-key
export RPC_URL=https://eth.llamarpc.com
export RUST_LOG=info

# Run
cargo run --release -- \
  --tycho-url tycho-beta.propellerheads.xyz \
  --rpc-url $RPC_URL \
  --protocols uniswap_v2,uniswap_v3
```

The solver starts on `http://localhost:3000` by default.

### Including RFQ Protocols

You can include RFQ (Request-for-Quote) protocols alongside on-chain protocols:

```bash
cargo run --release -- \
  --tycho-url tycho-beta.propellerheads.xyz \
  --rpc-url $RPC_URL \
  --protocols uniswap_v2,uniswap_v3,rfq:bebop
```

**Limitations:**
- RFQ protocols cannot run alone — at least one on-chain protocol is required. 

**Environment variables:**
- RFQ protocols typically require API keys, which are passed via environment variables. Check the [RFQ protocol docs](https://docs.propellerheads.xyz/tycho/for-solvers/request-for-quote-protocols) for the specific variables each protocol needs.

## Make a Solve Request

```bash
curl -X POST http://localhost:3000/v1/solve \
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

### POST /v1/solve

Submit one or more swap orders and receive optimal routes.

**Request:**

| Field | Type | Required | Description                            |
|-------|------|----------|----------------------------------------|
| `orders[].token_in` | address | Yes | Token to sell                          |
| `orders[].token_out` | address | Yes | Token to buy                           |
| `orders[].amount` | string | Yes | Amount in token units (integer string) |
| `orders[].side` | string | Yes | `"sell"` (exact input)                 |
| `orders[].sender` | address | Yes | Sender address                         |
| `orders[].receiver` | address | No | Receiver (defaults to sender)          |
| `options.timeout_ms` | integer | No | Solve timeout override                 |
| `options.min_responses` | integer | No | Early return threshold                 |
| `options.max_gas` | string | No | Max gas filter                         |

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
            "gas_estimate": "150000"
          }
        ]
      },
      "amount_in": "1000000000000000000",
      "amount_out": "3200000000",
      "gas_estimate": "150000",
      "amount_out_net_gas": "3199500000",
      "block": { "number": 19000000, "hash": "0x...", "timestamp": 1700000000 }
    }
  ],
  "total_gas_estimate": "150000",
  "solve_time_ms": 45
}
```

### GET /v1/health

Returns service health status. HTTP 200 if healthy, 503 if stale.

## Configuration

### CLI / Environment Variables

| Flag | Env Var | Default | Description                                |
|------|---------|---------|--------------------------------------------|
| `--rpc-url` | `RPC_URL` | (required) | Ethereum RPC endpoint for the target chain |
| `--tycho-url` | `TYCHO_URL` | `localhost:4242` | Tycho WebSocket URL                        |
| `--tycho-api-key` | `TYCHO_API_KEY` | - | Tycho API key                              |
| `--chain` | - | `Ethereum` | Target chain                               |
| `-p, --protocols` | - | - | Protocols to index (comma-separated)       |
| `--http-port` | `HTTP_PORT` | `3000` | API port                                   |
| `--min-tvl` | - | `10.0` | Minimum pool TVL in native token           |
| `--order-manager-timeout-ms` | - | `100` | Default solve timeout                      |
| `-w, --worker-pools-config` | `WORKER_POOLS_CONFIG` | `worker_pools.toml` | Worker pools config                        |
| `--blacklist-config` | `BLACKLIST_CONFIG` | `blacklist.toml` | Blacklist config                           |
| `--enable-metrics` | `ENABLE_METRICS` | `false` | Enable Prometheus metrics server on port 9898 |

See `--help` for the full list.

Find the list of all available protocols on Tycho [here](https://docs.propellerheads.xyz/tycho/for-solvers/supported-protocols)

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
A Worker Pool runs a configurable number of Worker threads, all using the same algorithm and pulling tasks from a shared queue. Each worker handles one order at a time — so a pool with 5 workers can solve up to 5 orders concurrently.

Multiple pools run in parallel, each producing its own solution per order. The system then picks the best result across pools within the timeout.

**Example**: Given the config above and 3 incoming orders:

- `fast_2hop` assigns 1 worker per order (3/5 workers busy)
- `deep_3hop` assigns 1 worker per order (3/3 workers busy)

Each order gets 2 candidate solutions — one from each pool — and the best is selected.

### Blacklist (blacklist.toml)

Exclude specific pools from routing:

```toml
[blacklist]
components = [
    "0x86d257cdb7bc9c0df10e84c8709697f92770b335",
]
```

## Observability

- **Metrics**: Prometheus endpoint at `http://localhost:9898/metrics`, enabled with `--enable-metrics`
- **Logging**: Structured logging via `RUST_LOG` (e.g., `RUST_LOG=info,fynd=debug`)
- **Health**: `GET /v1/health` returns data freshness and pool count

## Extensibility

### Adding a New Algorithm

1. Implement the `Algorithm` trait (choose your preferred graph type)
2. Register it in `src/worker_pool/registry.rs`
3. Add a pool entry in `worker_pools.toml`


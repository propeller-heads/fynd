---
icon: server
---

# Server Configuration

Reference for all Fynd server flags, worker pool tuning, blocklist configuration, logging, and monitoring.

## Run options

All on-chain protocols are fetched from Tycho RPC by default, so `--protocols` is optional. The `--tycho-url` also defaults to the Fynd endpoint for the selected chain.

```bash
cargo run --release -- serve
```

To run on a different chain:

```bash
cargo run --release -- serve --chain base
```

`--rpc-url` defaults to the public endpoint `https://eth.llamarpc.com`. For production, use a dedicated endpoint:

```bash
cargo run --release -- serve \
  --rpc-url https://your-rpc-provider.com/v1/your_key
```

Specify protocols explicitly:

```bash
cargo run --release -- serve \
  --protocols uniswap_v2,uniswap_v3,vm:curve
```

See the full [list of available protocols](https://docs.propellerheads.xyz/tycho/for-solvers/supported-protocols).

### Including RFQ Protocols

Include RFQ (Request-for-Quote) protocols alongside on-chain protocols. Use the `all_onchain` keyword to combine auto-fetched on-chain protocols with specific RFQ protocols:

```bash
cargo run --release -- serve \
  --protocols all_onchain,rfq:bebop
```

Or specify both on-chain and RFQ protocols explicitly:

```bash
cargo run --release -- serve \
  --protocols uniswap_v2,uniswap_v3,rfq:bebop
```

**Limitations:**

* RFQ protocols cannot run alone. At least one on-chain protocol is required.

**Environment variables:**

* RFQ protocols require API keys passed via environment variables. Check the [RFQ protocol docs](https://docs.propellerheads.xyz/tycho/for-solvers/request-for-quote-protocols) for the specific variables each protocol needs.

## Flag reference

Run `cargo run --release -- serve --help` for the full list.

### Required

| Flag              | Env Var         | Description   |
| ----------------- | --------------- | ------------- |
| `--tycho-api-key` | `TYCHO_API_KEY` | Tycho API key |

### Optional

| Flag                               | Env Var               | Default                    | Description                                                                                                                                                                                                    |
| ---------------------------------- | --------------------- | -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--rpc-url`                        | `RPC_URL`             | `https://eth.llamarpc.com` | Ethereum RPC endpoint. Use a dedicated endpoint in production.                                                                                                                                                 |
| `--tycho-url`                      | `TYCHO_URL`           | _(chain-specific)_         | Tycho URL. Defaults to the Fynd endpoint for the selected chain (e.g. `tycho-fynd-ethereum.propellerheads.xyz`).                                                                                               |
| `--chain`                          | —                     | `Ethereum`                 | Target chain                                                                                                                                                                                                   |
| `-p, --protocols`                  | —                     | _(all on-chain)_           | Protocols to index (comma-separated). If omitted, all on-chain protocols are fetched from Tycho RPC. Use `all_onchain` to combine auto-fetched protocols with explicit entries (e.g. `all_onchain,rfq:bebop`). |
| `--http-host`                      | `HTTP_HOST`           | `0.0.0.0`                  | HTTP bind address                                                                                                                                                                                              |
| `--http-port`                      | `HTTP_PORT`           | `3000`                     | API port                                                                                                                                                                                                       |
| `--min-tvl`                        | —                     | `10.0`                     | Minimum pool TVL in native token (ETH)                                                                                                                                                                         |
| `--tvl-buffer-ratio`               | —                     | `1.1`                      | Hysteresis buffer for TVL filtering. Components are added when TVL >= `min_tvl` and removed when TVL drops below `min_tvl / tvl_buffer_ratio`.                                                                 |
| `--traded-n-days-ago`              | —                     | `3`                        | Only include tokens traded within this many days.                                                                                                                                                              |
| `--worker-router-timeout-ms`       | —                     | `100`                      | Default solve timeout (ms)                                                                                                                                                                                     |
| `--worker-router-min-responses`    | —                     | `0`                        | Early return threshold (0 = wait for all pools)                                                                                                                                                                |
| `-w, --worker-pools-config`        | `WORKER_POOLS_CONFIG` | `worker_pools.toml`        | Worker pools config file path                                                                                                                                                                                  |
| `--blocklist-config` | `BLOCKLIST_CONFIG` | `blocklist.toml` | Path to blocklist TOML config file. Components listed here are excluded from the Tycho stream.                                                                                                                                                                                     |
| `--disable-tls`                    | —                     | `false`                    | Disable TLS for Tycho connection                                                                                                                                                                               |
| `--min-token-quality`              | —                     | `100`                      | Minimum [token quality](https://docs.propellerheads.xyz/tycho/overview/concepts#token) filter                                                                                                                  |
| `--gas-refresh-interval-secs`      | —                     | `30`                       | Gas price refresh interval                                                                                                                                                                                     |
| `--reconnect-delay-secs`           | —                     | `5`                        | Reconnect delay on connection failure                                                                                                                                                                          |
| `--gas-price-stale-threshold-secs` | —                     | _(disabled)_               | Health returns 503 when gas price exceeds this age. Disabled by default.                                                                                                                                       |

## Worker pools (`worker_pools.toml`)

Worker pools control solver thread count and routing strategies. The default config ships with two pools:

```toml
# worker_pools.toml
[pools.most_liquid_2_hops_fast]
algorithm = "most_liquid"
num_workers = 5
task_queue_capacity = 1000
max_hops = 2
timeout_ms = 100

[pools.most_liquid_3_hops]
algorithm = "most_liquid"
num_workers = 3
task_queue_capacity = 1000
min_hops = 2
max_hops = 3
timeout_ms = 5000
```

Both pools solve every incoming order in parallel. Fynd picks the best result across pools within the timeout.

### Worker pool fields

| Field                 | Default         | Description                                                            |
| --------------------- | --------------- | ---------------------------------------------------------------------- |
| `algorithm`           | `"most_liquid"` | Algorithm used for the pool                                            |
| `num_workers`         | CPU count       | Number of OS threads dedicated to this pool                            |
| `task_queue_capacity` | `1000`          | Maximum number of orders that can be queued simultaneously             |
| `min_hops`            | `1`             | Minimum number of hops required for routing                            |
| `max_hops`            | `3`             | Maximum number of hops permitted for routing                           |
| `timeout_ms`          | `100`           | Maximum time in milliseconds allowed per order processing in this pool |
| `max_routes`          | _(no limit)_    | Maximum number of candidate routes to evaluate per order               |

### Tuning tips

* **More workers** = more orders can be solved concurrently. Each worker is a dedicated OS thread, so avoid exceeding your CPU core count across all pools.
* **Lower `max_hops`** = faster solves but may miss better multi-hop routes.
* **Higher `max_hops`** = explores deeper routes but takes longer. Pair with a higher `timeout_ms`.
* **The "fast + deep" pattern** (default config) gives quick responses from the 2-hop pool while the 3-hop pool searches for better routes in the background.

To use a custom config file:

```bash
cargo run --release -- serve -w my_worker_pools.toml
```

## Blocklist config

By default, Fynd loads `blocklist.toml` from the working directory. The shipped default excludes components with known simulation issues (e.g., [rebasing tokens on UniswapV3 pools](https://docs.uniswap.org/concepts/protocol/integration-issues)). Override with `--blocklist-config`:

```bash
cargo run --release -- serve --blocklist-config my_blocklist.toml
```

The config file uses a `[blocklist]` section listing component IDs to exclude:

```toml
[blocklist]
components = [
    "0x86d257cdb7bc9c0df10e84c8709697f92770b335",
]
```

## Logging and monitoring

### Logs

Control log verbosity with `RUST_LOG`:

```bash
# Minimal output
RUST_LOG=warn cargo run --release -- serve ...

# Default (recommended)
RUST_LOG=info cargo run --release -- serve ...

# Debug solver internals
RUST_LOG=info,fynd_core=debug cargo run --release -- serve ...

# Trace-level (very verbose, not recommended)
RUST_LOG=info,fynd_core=trace cargo run --release -- serve ...
```

### Prometheus metrics

Metrics are exposed at `http://localhost:9898/metrics` (always on). Scrape this endpoint with Prometheus or any compatible tool. Available metrics: solve duration, response counts, failure types, and pool performance.

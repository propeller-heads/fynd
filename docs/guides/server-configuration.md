---
icon: server
---

# Server Configuration

Reference for all Fynd server flags, worker pool tuning, blocklist configuration, logging, and monitoring.

## Run options

All on-chain protocols available on your configured Tycho endpoint are fetched by default, so `--protocols` is optional. The `--tycho-url` also defaults to the Fynd endpoint for the selected chain.

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
* When encoding is enabled (`encoding_options` in the quote request), RFQ quotes require an additional round-trip to the RFQ provider to fetch a signed quote. This can add significant tail latency to solve times. If you are using RFQ protocols, consider quoting first without encoding to evaluate the price, and only request encoding once you are confident the quote is worth executing.

**Environment variables:**

* RFQ protocols require API keys passed via environment variables. Check the [RFQ protocol docs](https://docs.propellerheads.xyz/tycho/for-solvers/request-for-quote-protocols) for the specific variables each protocol needs.

## Flag reference

Run `cargo run --release -- serve --help` for the full list.

### Required

| Flag              | Env Var         | Description   |
| ----------------- | --------------- | ------------- |
| `--tycho-api-key` | `TYCHO_API_KEY` | Tycho API key |

### Optional

| Flag | Env Var | Default | <div style="width:30%">Description</div> |
| ---- | ------- | ------- | ----------------------------------------- |
| `--rpc-url`                        | `RPC_URL`             | `https://eth.llamarpc.com` | Node RPC endpoint for the target chain. Use a dedicated endpoint in production.                                                                                                                                |
| `--tycho-url`                      | `TYCHO_URL`           | _(chain-specific)_         | Tycho URL. Defaults to the [Fynd hosted endpoint](https://docs.propellerheads.xyz/tycho/for-solvers/hosted-endpoints#tycho-fynd) for the selected chain.                                                       |
| `--chain`                          | —                     | `Ethereum`                 | Target chain                                                                                                                                                                                                   |
| `-p, --protocols`                  | —                     | _(all on-chain)_           | Protocols to index (comma-separated). If omitted, all on-chain protocols available on your configured Tycho endpoint are fetched. Use `all_onchain` to combine auto-fetched protocols with explicit entries (e.g. `all_onchain,rfq:bebop`). |
| `--http-host`                      | `HTTP_HOST`           | `0.0.0.0`                  | HTTP bind address                                                                                                                                                                                              |
| `--http-port`                      | `HTTP_PORT`           | `3000`                     | API port                                                                                                                                                                                                       |
| `--min-tvl`                        | —                     | `10.0`                     | Minimum pool TVL in native token (ETH)                                                                                                                                                                         |
| `--tvl-buffer-ratio`               | —                     | `1.1`                      | Hysteresis buffer for TVL filtering. Components are added when TVL >= `min_tvl` and removed when TVL drops below `min_tvl / tvl_buffer_ratio`.                                                                 |
| `--traded-n-days-ago`              | —                     | `3`                        | Only include tokens traded within this many days.                                                                                                                                                              |
| `--worker-router-timeout-ms`       | —                     | `100`                      | Default solve timeout (ms)                                                                                                                                                                                     |
| `--worker-router-min-responses`    | —                     | `0`                        | Early return threshold (0 = wait for all pools)                                                                                                                                                                |
| `-w, --worker-pools-config`        | `WORKER_POOLS_CONFIG` | `worker_pools.toml`        | Worker pools config file path                                                                                                                                                                                  |
| `--blocklist-config`               | `BLOCKLIST_CONFIG`    | `blocklist.toml`           | Path to blocklist TOML config file. Components listed here are excluded from the Tycho stream.                                                                                                                                                                                     |
| `--disable-tls`                    | —                     | `false`                    | Disable TLS for Tycho connection                                                                                                                                                                               |
| `--min-token-quality`              | —                     | `100`                      | Minimum [token quality](https://docs.propellerheads.xyz/tycho/overview/concepts#token) filter                                                                                                                  |
| `--gas-refresh-interval-secs`      | —                     | `30`                       | Gas price refresh interval                                                                                                                                                                                     |
| `--reconnect-delay-secs`           | —                     | `5`                        | Reconnect delay on connection failure                                                                                                                                                                          |
| `--gas-price-stale-threshold-secs` | —                     | _(disabled)_               | Health returns 503 when gas price exceeds this age. Disabled by default.                                                                                                                                       |

## Worker pools (`worker_pools.toml`)

Worker pools control solver thread count and routing strategies. The default config ships with one pool:

```toml
# worker_pools.toml
[pools.bellman_ford_2_hops]
algorithm = "bellman_ford"
num_workers = 3
task_queue_capacity = 1000
max_hops = 2
timeout_ms = 500
```

All pools solve every incoming order in parallel. Fynd picks the best result across pools within the timeout.

### Worker pool fields

| Field | Default | <div style="width:40%">Description</div> |
| ----- | ------- | ---------------------------------------- |
| `algorithm`           | _(required)_    | Algorithm used for the pool (`"most_liquid"` or `"bellman_ford"`)      |
| `num_workers`         | CPU count       | Number of OS threads dedicated to this pool                            |
| `task_queue_capacity` | `1000`          | Maximum number of orders that can be queued simultaneously             |
| `min_hops`            | `1`             | Minimum number of hops required for routing                            |
| `max_hops`            | `3`             | Maximum number of hops permitted for routing                           |
| `timeout_ms`          | `100`           | Maximum time in milliseconds allowed per order processing in this pool |
| `max_routes`          | _(no limit)_    | Maximum number of candidate routes to evaluate per order               |

To use a custom config file:

```bash
cargo run --release -- serve -w my_worker_pools.toml
```

## Blocklist config

By default, Fynd loads `blocklist.toml` from the working directory. The default excludes components with known simulation issues (e.g., [rebasing tokens on UniswapV3 pools](https://docs.uniswap.org/concepts/protocol/integration-issues)). Override with `--blocklist-config`:

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
RUST_LOG=fynd=info cargo run --release -- serve ...

# Debug solver internals
RUST_LOG=info,fynd_core=debug cargo run --release -- serve ...

# Trace-level (very verbose, not recommended)
RUST_LOG=info,fynd_core=trace cargo run --release -- serve ...
```

### Prometheus metrics

***

## Tuning tips

### Worker pools

* **More workers** = more orders can be solved concurrently. Each worker is a dedicated OS thread, so avoid exceeding your CPU core count across all pools.
* **Lower `max_hops`** = faster solves but may miss better multi-hop routes.
* **Higher `max_hops`** = explores deeper routes but takes longer. Pair with a higher `timeout_ms`.
* **Multiple pools** with different `max_hops` and `timeout_ms` let you trade off speed vs. route quality — e.g. a fast 2-hop pool alongside a slower 3-hop pool.
* **Lower `max_routes`** = more predictable latency on large graphs, at the cost of potentially missing a better route.

### Request routing

* **Lower `--worker-router-min-responses`** = faster response with multiple pools — set to `1` to return as soon as the first pool finishes, at the cost of potentially missing a better result from a slower pool.

Metrics are exposed at `http://localhost:9898/metrics` (always on). Scrape this endpoint with Prometheus or any compatible tool. Available metrics: solve duration, response counts, failure types, and pool performance.

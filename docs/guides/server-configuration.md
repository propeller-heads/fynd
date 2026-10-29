---
icon: server
---

# Server Configuration

Reference for all Fynd server flags, worker pool tuning, blocklist configuration, logging, and monitoring.

## Run options

All on-chain protocols available on your configured Tycho endpoint are fetched by default, so `--protocols` is optional. The `--tycho-url` also defaults to the Fynd endpoint for the selected chain.

```bash
fynd serve
```

To run on a different chain:

```bash
fynd serve --chain base
```

`--rpc-url` defaults to the public endpoint `https://eth.llamarpc.com`. For production, use a dedicated endpoint:

```bash
fynd serve \
  --rpc-url https://your-rpc-provider.com/v1/your_key
```

Specify protocols explicitly:

```bash
fynd serve \
  --protocols uniswap_v2,uniswap_v3,ekubo_v3,fluid_v1
```

See the full [list of available protocols](https://docs.propellerheads.xyz/tycho/for-solvers/supported-protocols).

Every named protocol is checked against the ones your Tycho endpoint serves. A protocol that is not served logs a warning and is dropped from the list rather than stopping the solver.

### Including Off-Chain Priced Venues

Venues whose pricing arrives as a complete book from the venue itself — the RFQ market makers (Bebop, Hashflow, Liquorice, Native) and the off-chain-priced pAMM Metric — are named with the `rfq:` prefix and can run alongside on-chain protocols. Use the `all_onchain` keyword to combine auto-fetched on-chain protocols with specific `rfq:` entries:

```bash
fynd serve \
  --protocols all_onchain,rfq:bebop
```

Or specify both on-chain protocols and `rfq:` entries explicitly:

```bash
fynd serve \
  --protocols uniswap_v2,uniswap_v3,rfq:bebop
```

**Limitations:**

* `rfq:` venues cannot serve quotes on their own. At least one on-chain protocol is required. Fynd does start and the feed does publish, but books carry no block, so with no on-chain protocol the market never gets a block label, derived data never builds, and every quote returns `not_ready`.
* When encoding is enabled (`encoding_options` in the quote request), the RFQ venues `rfq:bebop`, `rfq:hashflow`, `rfq:native` and `rfq:liquorice` require an additional round-trip to the provider to fetch a signed quote. This can add significant tail latency to solve times. If you are using these venues, consider quoting first without encoding to evaluate the price, and only request encoding once you are confident the quote is worth executing. `rfq:metric` encodes locally and needs no such round-trip — its executor reads the on-chain oracle Metric updates every block.
* Metric runs on Ethereum, Base, Robinhood, BSC, Arbitrum and Polygon only. Each entry also needs its executor on the chain: `fallback:rfq:metric` runs on all six, and `rfq:metric` on Base, Robinhood, Arbitrum and Polygon. Ethereum and BSC have only the fallback executor. Naming an entry where its executor is missing fails at startup rather than producing quotes that cannot be encoded.
* On Ethereum, Metric can come from either source: `fallback:rfq:metric` (Metric's API, executing through `MetricFallbackRouter`) or `pricelevelstream:metric` (Titan's price level stream, executing through Titan's Metric router). Both price the same pools, so name only one of them; a list naming both fails at startup.
* On Ethereum, a Metric pool fills only in blocks whose builder places Metric's oracle update before the trade (Titan and the other builders taking part in Metric's conditional updates). In any other block the `MetricFallbackRouter` fills the leg on the fallback pool Fynd names instead.

**Environment variables:**

* Every `rfq:` venue requires API keys passed via environment variables. Check the [off-chain priced venue docs](https://docs.propellerheads.xyz/tycho/for-solvers/request-for-quote-protocols) for the specific variables each protocol needs.
* `rfq:metric` reads `METRIC_API_KEY` (required — the Bearer token for Metric's quote endpoints) and `METRIC_API_URL` (optional, defaults to `https://api.metric.xyz`).
* `rfq:native` reads `NATIVE_API_KEY`. It runs on Ethereum, Base, Arbitrum, BSC and Robinhood only. Neither `rfq:native` nor `rfq:liquorice` has a `fallback:rfq:` form.
* `rfq:liquorice` reads `LIQUORICE_USER` (the solver name) and `LIQUORICE_KEY`. It runs on Ethereum only.

### pAMM Price Level Stream

Serve a proprietary AMM from Titan's pAMM price level stream instead of simulating it in the EVM. Titan publishes a quote ladder per pair every block, so quotes come from interpolating those levels — much cheaper than a VM simulation.

Name a venue with the `pricelevelstream:` prefix. The served venues are `fermiswap`, `kipseli`, `metric`, `bebop`, `taurusfi`, `tempest`, `goliath`, `stelaxis`, and `elzorro`:

The stream serves Ethereum only. `pricelevelstream:metric` and Metric's book feed (`rfq:metric` or `fallback:rfq:metric`) price the same pools, so name only one of them — see [Including Off-Chain Priced Venues](#including-off-chain-priced-venues).

```bash
fynd serve \
  --protocols all_onchain,exclude:vm:fermiswap,pricelevelstream:fermiswap
```

The `exclude:` prefix drops a protocol from the list. It matters here: `vm:fermiswap` and `pricelevelstream:fermiswap` price the same maker inventory, so streaming both double-counts that liquidity. Drop the Tycho-streamed one whenever you serve the same venue from the price level stream. An `exclude:` entry that matches no streamed protocol logs a warning and is otherwise ignored, so a protocol Tycho has dropped does not stop the solver from starting.

**Limitations:**

* Ethereum mainnet only — the venue addresses are mainnet deployments.
* Quotes below the smallest ladder level are rejected rather than extrapolated, and quotes above the largest come back as a partial fill at the limit.
* A quote executes only in the block it was quoted for. The venue rejects a fill priced off a stale reading, so a quote that misses its block falls back to the on-chain pool Fynd names for that leg (Uniswap V2, V3 or V4, Curve or Fluid on Ethereum) instead of filling at the old price. Fynd only returns a route whose fallback fill still clears the route's `min_amount_out`.

### Self-hosted Tycho

By default `--tycho-url` points at the [Fynd hosted endpoint](https://docs.propellerheads.xyz/tycho/for-solvers/hosted-endpoints#tycho-fynd) for the selected chain. Fynd talks to Tycho purely over its RPC/WebSocket API, so whether that Tycho is PropellerHeads-hosted or one you run yourself is transparent to Fynd — you only change where it points.

Run your own Tycho when a chain has no hosted Substreams endpoint. The Tycho Indexer can stream from a self-hosted Firehose + Substreams stack; see [Self-Hosted EVM Chain](https://docs.propellerheads.xyz/tycho/for-solvers/self-hosted-evm-chain) for how to stand it up. Once it is serving, point Fynd at it:

```bash
fynd serve \
  --chain base \
  --tycho-url your-self-hosted-tycho.example.com \
  --rpc-url https://your-node
```

Notes:

* **TLS** — hosted endpoints use TLS; a local or plain-HTTP Tycho does not. Add `--disable-tls` when your endpoint is not served over TLS.
* **API key** — the self-hosted indexer's RPC key is its `AUTH_API_KEY` (default `local-dev-key`). Pass it with `--tycho-api-key` / `TYCHO_API_KEY` if your deployment sets one.
* **Built-in chains** — Fynd's built-in chains are `ethereum`, `base`, `unichain`, `arbitrum`, `polygon`, `bsc`, `robinhood`, `starknet`, `zksync`. Self-hosting Tycho for one of these works out of the box.

#### Custom chains

Fynd can also run against a chain that isn't one of Tycho's built-ins, as long as your self-hosted Tycho indexer declares it. Point Fynd at the same `chains.yaml` the indexer uses with `--chains-config` / `TYCHO_CHAINS_CONFIG`, and pass `--tycho-url` and `--rpc-url` explicitly — custom chains have no built-in defaults, so omitting either is an error:

```bash
fynd serve \
  --chain tempo \
  --chains-config ./chains.yaml \
  --tycho-url your-self-hosted-tycho.example.com \
  --rpc-url https://your-node \
  --disable-tls
```

See Tycho's [Self-Hosted EVM Chain](https://docs.propellerheads.xyz/tycho/for-solvers/self-hosted-evm-chain) guide for the "Declaring a custom chain" (`chains.yaml`) and "Consuming the custom chain" sections.

**Quote-only until the router is deployed** — a custom chain has no Tycho router/executor contracts until ops deploys them. Until then, Fynd runs quote-only for that chain: `GET /v1/info` reports `router_address: null`, and a quote request with `encoding_options` set returns `501 Not Implemented`. Once the router/executor contracts are deployed, encoding works as usual.

## Flag reference

Run `fynd serve --help` for the full list.

### Required

| Flag              | Env Var         | Description   |
| ----------------- | --------------- | ------------- |
| `--tycho-api-key` | `TYCHO_API_KEY` | Tycho API key |

### Optional

| Flag | Env Var | Default | <div style="width:30%">Description</div> |
| ---- | ------- | ------- | ----------------------------------------- |
| `--rpc-url`                        | `RPC_URL`             | `https://eth.llamarpc.com` | Node RPC endpoint for the target chain. Use a dedicated endpoint in production.                                                                                                                                |
| `--tycho-url`                      | `TYCHO_URL`           | _(chain-specific)_         | Tycho URL. Defaults to the [Fynd hosted endpoint](https://docs.propellerheads.xyz/tycho/for-solvers/hosted-endpoints#tycho-fynd) for the selected chain.                                                       |
| `--tycho-subscription-buffer-size` | `TYCHO_SUBSCRIPTION_BUFFER_SIZE` | _(unset; Tycho native default)_ | Number of delta messages buffered for each Tycho subscription. Leave unset to preserve Tycho's native default. |
| `--max-sync-lag-secs`           | `MAX_SYNC_LAG_SECS` | `15`                      | Seconds of block time a protocol Tycho reports as `Delayed` may lag the newest block any protocol reported as `Ready` before its pools are excluded from routing. A protocol Tycho reports as `Stale` is excluded until it catches up; one reported as `Ended` stays excluded. |
| `--chain`                          | —                     | `Ethereum`                 | Target chain                                                                                                                                                                                                   |
| `--chains-config`                  | `TYCHO_CHAINS_CONFIG` | _(none)_                   | Path to the custom-chains `chains.yaml`. Required for a chain Tycho does not know as a built-in.                                                                                                              |
| `-p, --protocols`                  | —                     | _(all on-chain)_           | Protocols to index (comma-separated). If omitted, all on-chain protocols available on your configured Tycho endpoint are fetched. Use `all_onchain` to combine auto-fetched protocols with explicit entries (e.g. `all_onchain,rfq:bebop`), and the `exclude:` prefix to drop one (e.g. `all_onchain,exclude:vm:fermiswap`). |
| `--http-host`                      | `HTTP_HOST`           | `0.0.0.0`                  | HTTP bind address                                                                                                                                                                                              |
| `--http-port`                      | `HTTP_PORT`           | `3000`                     | API port                                                                                                                                                                                                       |
| `--min-tvl`                        | —                     | `10.0`                     | Minimum pool TVL in native token (ETH)                                                                                                                                                                         |
| `--tvl-buffer-ratio`               | —                     | `1.1`                      | Hysteresis buffer for TVL filtering. Components are added when TVL >= `min_tvl` and removed when TVL drops below `min_tvl / tvl_buffer_ratio`.                                                                 |
| `--traded-n-days-ago`              | —                     | `3`                        | Only include tokens traded within this many days.                                                                                                                                                              |
| `--worker-router-timeout-ms`       | —                     | `100`                      | Default solve timeout (ms)                                                                                                                                                                                     |
| `--worker-router-min-responses`    | —                     | `0`                        | Early return threshold (0 = wait for all pools)                                                                                                                                                                |
| `--encoding-retry-budget-ms`       | —                     | `5`                        | Time after the first encode during which a failed encoding is retried on the next candidate (0 = no retries)                                                                                                   |
| `-w, --worker-pools-config`        | `WORKER_POOLS_CONFIG` | `worker_pools.toml`        | Worker pools config file path                                                                                                                                                                                  |
| `--blocklist-config`               | `BLOCKLIST_CONFIG`    | [tycho-simulation default](https://github.com/propeller-heads/tycho-simulation/blob/main/blocklist.toml)                          | Path to blocklist TOML config file. Components listed here are excluded from the Tycho stream.                                                                                                                                                                                     |
| `--disable-tls`                    | —                     | `false`                    | Disable TLS for Tycho connection                                                                                                                                                                               |
| `--min-token-quality`              | —                     | `100`                      | Minimum [token quality](https://docs.propellerheads.xyz/tycho/overview/concepts#token) filter                                                                                                                  |
| `--gas-refresh-interval-secs`      | —                     | `30`                       | Gas price refresh interval                                                                                                                                                                                     |
| `--reconnect-delay-secs`           | —                     | `5`                        | Reconnect delay on connection failure                                                                                                                                                                          |
| `--gas-price-stale-threshold-secs` | —                     | _(disabled)_               | Health returns 503 when gas price exceeds this age. Disabled by default.                                                                                                                                       |
| `--pricing-max-hops`                         | `PRICING_MAX_HOPS` | `2` | How far from the gas token the token-pricing pass reaches, in hops. A token beyond it is quoted gas-blind even when a worker pool's `max_hops` can route to it; each extra hop widens the walk every pass makes. |
| `--partial-blocks`                           | —        | `false`      | Enable partial block (flashblock) updates from the Tycho stream. Pool state updates are delivered mid-block rather than only at finalization, reducing latency. Only applies to on-chain protocols. |
| `--aggregate-rfqs`                           | `AGGREGATE_RFQS` | `false` | Stream each Bebop and Hashflow venue (direct and fallback) as one component instead of one per token pair. A route may take one quote from Bebop and one per market maker from Hashflow. Metric, Native and Liquorice always stream per pair. |
| `--nexus-url`                         | `NEXUS_URL` | _(unset)_ | URL of a [`fynd-nexus`](../../fynd-nexus/README.md) service, such as `http://fynd-nexus:3000`. Each `fynd-nexus` instance serves one chain, so point this at the instance of this pod's chain; Fynd does not check it. The `rfq:` and `fallback:rfq:` entries of Bebop, Hashflow and Metric in `--protocols` then take their prices from the service instead of connecting to the venue, so several pods share one Bebop connection. Fynd still builds the books itself and still asks the venue for firm quotes, so `BEBOP_KEY` and `HASHFLOW_USER`/`HASHFLOW_KEY` stay required; `METRIC_API_KEY` does not. Native and Liquorice always connect directly. There is no fallback to a direct connection: when the service is unreachable, the venue's pools are withdrawn after the venue's snapshot age. |
| `--enable-price-guard`                       | —        | `false`      | Enable [price guard](price-guard.md) validation against external price sources.                                                                                        |
| `--enable-simulation`                        | —        | `false`      | Enable simulation of encoded quotes against the latest block. Requires an RPC endpoint serving `eth_simulateV1` and `debug_traceCall`. |
| `--simulate-on-quote-block`                  | —        | `false`      | Simulate each quote on the block that it was priced on, instead of the latest block. A node that does not have that block yet is asked again, for at most two block times. |
| `--jit-router-address`                       | `JIT_ROUTER_ADDRESS` | _(unset)_ | Address of the chain's `FyndJITRouter`. A quote request that the authenticating proxy marks with `x-jit-router: true` is encoded to be sent there instead of to the Tycho router, with the same calldata (vault-funded swaps keep going to the Tycho router); `GET /v1/info` reports it as `jit_router_address`, and as `to_address` on such requests. Unset: such requests fail to encode. |
| `--price-guard-lower-tolerance-bps`          | —        | `300`        | Max allowed deviation (bps) when the quote's output is below the provider's expected amount.                                                                           |
| `--price-guard-upper-tolerance-bps`          | —        | `10000`      | Max allowed deviation (bps) when the quote's output is above the provider's expected amount.                                                                           |
| `--price-guard-fail-on-provider-error`       | —        | `false`      | Reject quotes when all price providers fail with infrastructure errors.                                                                                                |
| `--price-guard-fail-on-token-price-not-found`| —        | `false`      | Reject quotes when no provider lists the token.                                                                                                                        |
| `--metrics-port`                             | `METRICS_PORT` | `9898`  | Port for the Prometheus metrics HTTP server. Requires the `metrics` feature (enabled by default).                                                                      |

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
| `algorithm`           | _(required)_    | Algorithm used for the pool: `"most_liquid"`, `"bellman_ford"`, `"path_frank_wolfe"`, `"water_fill"`, or any name an embedding binary registered through `AlgorithmRegistry` |
| `num_workers`         | CPU count       | Number of OS threads dedicated to this pool                            |
| `task_queue_capacity` | `1000`          | Maximum number of orders that can be queued simultaneously             |
| `min_hops`            | `1`             | Minimum number of hops required for routing                            |
| `max_hops`            | `3`             | Maximum number of hops permitted for routing                           |
| `timeout_ms`          | `100`           | Maximum time in milliseconds allowed per order processing in this pool |
| `max_routes`          | _(no limit)_    | Maximum number of candidate routes to evaluate per order               |
| `connector_tokens`    | _(no restriction)_ | Allowlist of `"0x..."`-prefixed token addresses permitted as intermediate hops. Source and destination are always allowed regardless. Absent = all tokens reachable. |
| `algorithm_exclude_protocols` | _(no restriction)_ | Protocol systems the pool's algorithm never routes through. Its post-processors still see them. An entry names a protocol system exactly (`"rfq:bebop"`), or a whole family when it ends with `:` (`"rfq:"`). `exclude_protocols`, with the same entries, hides protocols from the whole pool. Without `post_processors` this key does the same as `exclude_protocols` |
| `post_processors`     | _(none)_        | Names of post-processors, in order, that every worker in the pool runs on the route its algorithm returned, after the worker validates that route. `rfq_swap_replacer` and `exclusive_swap_replacer` are built into fynd-core; a binary that embeds Fynd registers more names through `PostProcessorRegistry`, and a custom name may not equal a built-in name. If a processed route fails the checks, the worker logs a warning and quotes the algorithm's route. A name that is not registered stops the server from starting |

### Post-processors

`rfq_swap_replacer` replaces one swap, or a run of swaps between two tokens that every path passes through, with one swap through an RFQ component when that raises the net amount out. The server must index `rfq:` or `fallback:rfq:` protocols (`--protocols`, see Including RFQ Protocols), and the pool must not drop them with `exclude_protocols`. Only `rfq:bebop`, `rfq:hashflow` and `rfq:native` support an input amount smaller than the quoted amount, so they can replace any swap. Native accepts down to half the quoted amount. Every other RFQ protocol can replace only swaps starting at the sell token, where the input amount matches the quoted amount exactly. The default `worker_pools.toml` does not enable it.

```toml
[pools.most_liquid_rfq]
algorithm       = "most_liquid"
post_processors = ["rfq_swap_replacer"]
```

`algorithm_exclude_protocols` hides protocols from the algorithm only, so `rfq_swap_replacer` still sees them. With it, a pool can reach a protocol only through the replacer. In the example below, Bebop appears in a route only when the replacer adds it, so the pool's quotes show the gain of the replacer alone.

```toml
[pools.replacer_only]
algorithm                   = "most_liquid"
algorithm_exclude_protocols = ["rfq:bebop"]
post_processors             = ["rfq_swap_replacer"]
```

`exclusive_swap_replacer` replaces one swap, or a run of swaps between two tokens that every path passes through, with one swap through an exclusive component when that raises the net amount out. It works on a worker pool with `liquidity_scope = "public_only"` (the default), for a request with exclusive access: the algorithm solves the order on public liquidity, and the replacer adds an exclusive leg to that route, so the order is solved once. Without exclusive access, or on a pool with `liquidity_scope = "include_exclusive"`, which already routes through exclusive liquidity, it keeps the route. An exclusive component fills partially, so the replacer can replace a swap at any position. It adds at most one exclusive leg, and it keeps a route that already has one. The replacer also tries to split a swap or a run of swaps: the exclusive component takes a part of the input and the original pools take the rest. This helps when the exclusive component has limited depth.

The replacer sets the committed amount out of the exclusive leg itself, with the rule the router applies to an exclusive route, using the route it received as the public reference. The user gets a share of the improvement on top of the received route (`EXCLUSIVE_ROUTE_USER_SHARE_BPS`, 10% by default), and never less than the received route paid; the exclusive component keeps the rest. A replacement that pays less than the received route is dropped. The worker pins the commitment on the quote, and the router ranks the quote by that amount. The router never takes it as the public reference for an `include_exclusive` pool's commitment: that reference is the best quote whose route has no exclusive swap. List `exclusive_swap_replacer` last, after `rfq_swap_replacer`: a post-processor that changes the route afterwards drops the commitment, and the worker then quotes the algorithm's route. The server enforces the order: a pool that lists `exclusive_swap_replacer` before another post-processor, or twice, stops the server from starting.

```toml
[pools.most_liquid_exclusive]
algorithm       = "most_liquid"
post_processors = ["exclusive_swap_replacer"]
```

### Connector tokens

By default Fynd routes through any token reachable in the pool graph. On live markets this can expose routes to illiquid or long-tail intermediates, which increases reversion risk: price impact at the intermediate hop can push slippage over the tolerance threshold, causing the transaction to revert.

`connector_tokens` restricts intermediate hops to a trusted set. It is most useful for deployments that are particularly sensitive to reverts:

```toml
[pools.bellman_ford_safe]
algorithm  = "bellman_ford"
max_hops   = 3
timeout_ms = 500
connector_tokens = [
    "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",  # WETH
    "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",  # USDC
    "0xdac17f958d2ee523a2206206994597c13d831ec7",  # USDT
    "0x6b175474e89094c44da98b954eedeac495271d0f",  # DAI
    "0x2260fac5e5542a773aa44fbcfedf7c193bc2c599",  # WBTC
    "0x7f39c581f595b53c5cb19bd0b3f8da6c935e2ca0",  # wstETH
]
```

> **Anchor tokens are not configured.** The `water_fill` algorithm's discovery uses a soft anchor
> preference when no `connector_tokens` allowlist is set — it prefers to route through the most
> connected tokens plus the native-ETH sentinel. This set is derived per solve from the live graph,
> so it stays correct on every chain automatically and needs no configuration.

Use `fynd derive-connector-tokens` to generate a ranked list for your chain from live Tycho data:

```bash
fynd derive-connector-tokens --chain Ethereum --top-n 10 --output toml
```

The command ranks every token by pool count and by backed TVL (pool TVL that native-backed tokens vouch for, so self-priced spam pools do not count) and outputs a ready-to-paste TOML snippet. Run `fynd derive-connector-tokens --help` for all options.

> **Tradeoff:** A narrower allowlist reduces reversion risk but may also reduce route quality — routes through unlisted tokens are never explored. For most chains, 5–10 highly liquid tokens cover the vast majority of pairs.

To use a custom config file:

```bash
fynd serve -w my_worker_pools.toml
```

## Blocklist config

By default, Fynd loads `blocklist.toml` from tycho-simulation. The default excludes components with known simulation issues (e.g., [rebasing tokens on UniswapV3 pools](https://docs.uniswap.org/concepts/protocol/integration-issues)). Override with `--blocklist-config`:

```bash
fynd serve --blocklist-config my_blocklist.toml
```

The config file uses a `[blocklist]` section listing component IDs to exclude:

```toml
[blocklist]
components = [
    "0x86d257cdb7bc9c0df10e84c8709697f92770b335",
]
```

Uniswap V4 pools are also filtered by hook contract: `BLOCKED_UNISWAP_V4_HOOKS` in
`fynd-core/src/feed/protocol_registry.rs` drops every `uniswap_v4_hooks` component
whose `hooks` static attribute matches a listed address, regardless of pool ID.

## Logging and monitoring

### Logs

Control log verbosity with `RUST_LOG`:

```bash
# Minimal output
RUST_LOG=warn fynd serve ...

# Default (recommended)
RUST_LOG=fynd=info fynd serve ...

# Debug solver internals
RUST_LOG=info,fynd_core=debug fynd serve ...

# Trace-level (very verbose, not recommended)
RUST_LOG=info,fynd_core=trace fynd serve ...
```

### Prometheus metrics

Fynd exposes Prometheus metrics on a dedicated HTTP server (enabled by default via the `metrics` feature). Scrape the `/metrics` endpoint with Prometheus or any compatible tool:

```
http://localhost:9898/metrics
```

The port defaults to `9898` and can be changed with `--metrics-port` or the `METRICS_PORT` environment variable:

```bash
fynd serve --metrics-port 9090
```

Available metrics include solve duration, response counts, failure types, and pool performance.

## Tuning tips

### Worker pools

* **More workers** = more orders can be solved concurrently. Each worker is a dedicated OS thread, so avoid exceeding your CPU core count across all pools.
* **Lower `max_hops`** = faster solves but may miss better multi-hop routes.
* **Higher `max_hops`** = explores deeper routes but takes longer. Pair with a higher `timeout_ms`.
* **Multiple pools** with different `max_hops` and `timeout_ms` let you trade off speed vs. route quality — e.g. a fast 2-hop pool alongside a slower 3-hop pool.
* **Lower `max_routes`** = more predictable latency on large graphs, at the cost of potentially missing a better route.

### Request routing

* **Lower `--worker-router-min-responses`** = faster response with multiple pools — set to `1` to return as soon as the first pool finishes, at the cost of potentially missing a better result from a slower pool.

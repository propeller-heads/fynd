---
icon: rocket-launch
layout:
  width: default
  title:
    visible: true
  description:
    visible: true
  tableOfContents:
    visible: true
  outline:
    visible: true
  pagination:
    visible: true
  metadata:
    visible: true
  tags:
    visible: true
---

# Quickstart

## Run Fynd

Get Fynd running locally. This guide covers building, running, and tuning the solver. No code changes required.

### Prerequisites

* Rust 1.92+ (install via [rustup](https://rustup.rs/))
* A Tycho API
  key ([get one here](https://app.gitbook.com/s/jrIe0oInIEt65tHqWn2w/for-solvers/indexer/tycho-client#authentication))

### 1. Build

```bash
git clone https://github.com/propeller-heads/fynd.git
cd fynd
cargo build --release
```

The release binary will be at `target/release/fynd`.

### 2. Set Environment Variables

```bash
export TYCHO_API_KEY=your-api-key
export RUST_LOG=info
```

### 3. Run

All on-chain protocols are fetched from Tycho RPC by default, so `--protocols` is optional. The `--tycho-url` also defaults to the Fynd endpoint for the selected chain.

```bash
cargo run --release -- serve
```

To run on a different chain, use `--chain`:

```bash
cargo run --release -- serve --chain base
```

`--rpc-url` defaults to the public endpoint `https://eth.llamarpc.com`. For production, pass a dedicated endpoint:

```bash
cargo run --release -- serve \
  --rpc-url https://your-rpc-provider.com/v1/your_key
```

You can also specify protocols explicitly:

```bash
cargo run --release -- serve \
  --protocols uniswap_v2,uniswap_v3,vm:curve
```

See the full [list of available protocols](https://docs.propellerheads.xyz/tycho/for-solvers/supported-protocols).

Once running, Fynd:

1. Connects to Tycho's Streams and syncs all protocol states
2. Builds routing graphs and computes derived data (spot prices, pool depths and token gas prices)
3. Starts the HTTP API on `http://localhost:3000`

{% hint style="info" %}
Wait for the [`/v1/health`](../overview/api-specifications.md#get-v1-health) endpoint to return healthy before sending
orders.
{% endhint %}

#### 3.1 Including RFQ Protocols

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

* RFQ protocols require API keys passed via environment variables. Check
  the [RFQ protocol docs](https://docs.propellerheads.xyz/tycho/for-solvers/request-for-quote-protocols) for the
  specific variables each protocol needs.

#### 3.2 Check Solver Health

```shellscript
curl http://localhost:3000/v1/health
```

Returns `"healthy":true` when ready to receive requests.

### 4. Request a quote

Get the quote for **1 WETH -> USDC** (or any pair/amount you want):

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
      "timeout_ms": 5000,
      "min_responses": 1
    }
  }'
```

The response includes the optimal route, amounts, gas estimates, and the net output after gas costs.

#### 4.1 - Optional request fields

<table><thead><tr><th>Field</th><th>Description</th></tr></thead><tbody><tr><td><pre class="language-rust"><code class="lang-rust">options.timeout_ms
</code></pre></td><td>Overrides the default solve timeout (configured via global config see <a href="./#id-5.-configuration">configuration section</a>).</td></tr><tr><td><pre><code>options.min_responses
</code></pre></td><td>Return early after <strong>N</strong> solver pools respond. If set to <code>null</code> - it will wait for all solver pools respond until timeout.</td></tr><tr><td><pre><code>options.max_gas
</code></pre></td><td>Discard routes above this gas limit</td></tr></tbody></table>

Full interface details: [api-specifications.md](../overview/api-specifications.md "mention")

### 5. Configuration

Tune Fynd with the following flags:

#### Required

<table><thead><tr><th width="246.9140625">Flag</th><th>Env Var</th><th>Description</th></tr></thead><tbody><tr><td><code>--tycho-api-key</code></td><td><code>TYCHO_API_KEY</code></td><td>Tycho API key</td></tr></tbody></table>

#### Optional

<table><thead><tr><th width="246.9140625">Flag</th><th>Env Var</th><th>Default</th><th>Description</th></tr></thead><tbody><tr><td><code>--rpc-url</code></td><td><code>RPC_URL</code></td><td><code>https://eth.llamarpc.com</code></td><td>Ethereum RPC endpoint. Use a dedicated endpoint in production.</td></tr><tr><td><code>--tycho-url</code></td><td><code>TYCHO_URL</code></td><td><em>(chain-specific)</em></td><td>Tycho WebSocket URL. Defaults to the Fynd endpoint for the selected chain (e.g. <code>tycho-fynd-ethereum.propellerheads.xyz</code>).</td></tr><tr><td><code>--chain</code></td><td>--</td><td><code>Ethereum</code></td><td>Target chain</td></tr><tr><td><code>-p, --protocols</code></td><td>--</td><td><em>(all on-chain)</em></td><td>Protocols to index (comma-separated). If omitted, all on-chain protocols are fetched from Tycho RPC. Use <code>all_onchain</code> to combine auto-fetched protocols with explicit entries (e.g. <code>all_onchain,rfq:bebop</code>).</td></tr><tr><td><code>--http-port</code></td><td><code>HTTP_PORT</code></td><td><code>3000</code></td><td>API port</td></tr><tr><td><code>--min-tvl</code></td><td>--</td><td><code>10.0</code></td><td>Minimum pool TVL in native token (ETH)</td></tr><tr><td><code>--tvl-buffer-ratio</code></td><td>--</td><td><code>1.1</code></td><td>Hysteresis buffer for TVL filtering. Components are added when TVL >= <code>min_tvl</code> and removed when TVL drops below <code>min_tvl / tvl_buffer_ratio</code>.</td></tr><tr><td><code>--traded-n-days-ago</code></td><td>--</td><td><code>3</code></td><td>Only include tokens traded within this many days.</td></tr><tr><td><code>--order-manager-timeout-ms</code></td><td>--</td><td><code>100</code></td><td>Default solve timeout (ms)</td></tr><tr><td><code>--order-manager-min-responses</code></td><td>--</td><td><code>0</code></td><td>Early return threshold (0 = wait for all pools)</td></tr><tr><td><code>-w, --worker-pools-config</code></td><td><code>WORKER_POOLS_CONFIG</code></td><td><code>worker_pools.toml</code></td><td>Worker pools config file path</td></tr><tr><td><code>--blacklist-config</code></td><td><code>BLACKLIST_CONFIG</code></td><td><code>blacklist.toml</code></td><td>Blacklist config file path</td></tr><tr><td><code>--disable-tls</code></td><td>--</td><td><code>false</code></td><td>Disable TLS for Tycho connection</td></tr><tr><td><code>--min-token-quality</code></td><td>--</td><td><code>100</code></td><td>Minimum <a href="https://docs.propellerheads.xyz/tycho/overview/concepts#token">token quality</a> filter</td></tr><tr><td><code>--gas-refresh-interval-secs</code></td><td>--</td><td><code>30</code></td><td>Gas price refresh interval</td></tr><tr><td><code>--reconnect-delay-secs</code></td><td>--</td><td><code>5</code></td><td>Reconnect delay on connection failure</td></tr></tbody></table>

Run `cargo run --release -- serve --help` for the full list.

#### 5.1 - Worker pools file (`worker_pools.toml`)

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

**Worker Pool Configuration:**

| Field                 | Default         | Description                                                            |
|-----------------------|-----------------|------------------------------------------------------------------------|
| `algorithm`           | `"most_liquid"` | Algorithm used for the pool                                            |
| `num_workers`         | CPU count       | Number of OS threads dedicated to this pool                            |
| `task_queue_capacity` | `1000`          | Maximum number of orders that can be queued simultaneously             |
| `min_hops`            | `1`             | Minimum number of hops required for routing                            |
| `max_hops`            | `3`             | Maximum number of hops permitted for routing                           |
| `timeout_ms`          | `100`           | Maximum time in milliseconds allowed per order processing in this pool |

**Tuning tips:**

* **More workers** = more orders can be solved concurrently. Each worker is a dedicated OS thread, so avoid exceeding
  your CPU core count across all pools.
* **Lower `max_hops`** = faster solves but may miss better multi-hop routes.
* **Higher `max_hops`** = explores deeper routes but takes longer. Pair with a higher `timeout_ms`.
* **The "fast + deep" pattern** (default config) gives quick responses from the 2-hop pool while the 3-hop pool searches
  for better routes in the background.

To use a custom config file:

```bash
cargo run --release -- serve -w my_worker_pools.toml
```

#### 5.1 Blacklist (`blacklist.toml`)

Exclude specific components from routing, useful for components with known simulation issues (
e.g., [rebasing tokens on UniswapV3 pools](https://docs.uniswap.org/concepts/protocol/integration-issues)):

```toml
[blacklist]
components = [
    "0x86d257cdb7bc9c0df10e84c8709697f92770b335",
]
```

### 6. Logging and Monitoring

#### Logs

Control log verbosity with `RUST_LOG`:

```bash
# Minimal output
RUST_LOG=warn cargo run --release -- serve ...

# Default (recommended)
RUST_LOG=info cargo run --release -- serve ...

# Debug solver internals
RUST_LOG=info,tycho_solver=debug cargo run --release -- serve ...

# Trace-level (very verbose, not recommended)
RUST_LOG=info,tycho_solver=trace cargo run --release -- serve ...
```

#### Prometheus Metrics

Metrics are exposed at `http://localhost:9898/metrics` (always on). Scrape this endpoint with Prometheus or any
compatible tool. Available metrics: solve duration, response counts, failure types, and pool performance.

### 7. Validating and Executing the Solutions

The repository includes an end-to-end example
at [`examples/tutorial/`](https://github.com/propeller-heads/fynd/tree/main/examples/tutorial) that demonstrates
quoting, simulating, and executing swaps against a running solver.
See [executing-the-solutions.md](executing-the-solutions.md "mention") for the full walkthrough.

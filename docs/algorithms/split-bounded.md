---
icon: route
---

# Split Bounded

`split_bounded` is Fynd's split-routing algorithm: it splits one order across multiple parallel
paths to reduce price impact on large trades. Candidate discovery is a bounded, amount-aware
search inspired by Penumbra's candidate-set routing, which keeps solve times close to the
single-path algorithms; the [variant comparison](split-comparison.md) records how it beat two
exhaustive-enumeration variants (`split`, `split_probe`) on the same allocation machinery.

It runs on a weightless graph and declares no derived data requirements: candidate discovery and
ranking use live simulation only. When derived token gas prices are available, net ranking is
gas-aware — path activation costs and the final route net subtract gas in output-token terms — and
without them it falls back to gross output rather than waiting.

## Candidate discovery

`split_bounded` starts from the input token with the full sell amount and expands up to
`max_hops`. At every frontier state it simulates candidate edges with
`ProtocolSim::get_amount_out` and keeps a small number of best states per intermediate token.

The search prefers:

1. Edges directly into the output token.
2. Explicitly configured `connector_tokens`.
3. A default anchor set when `connector_tokens` is absent.

The default anchor set includes the native ETH sentinel
`0x0000000000000000000000000000000000000000`, Ethereum WETH, USDC, USDT, DAI, WBTC, wstETH, AAVE,
UNI, OP-stack WETH, Base USDC, Base cbBTC, and Unichain USDC.

The native ETH sentinel is important. Without it, WETH to ETH to token routes are missed on full
Fynd setups where Tycho models native ETH as the zero address.

This is not Penumbra's full spill-price min-cost-flow routing loop. It borrows the practical lesson
that route quality usually comes from a good bounded candidate set, then uses the split allocators
to build executable routes.

## Allocation

After candidate discovery, `split_bounded` builds two split route families:

1. **Pool-disjoint split**: allocate chunks across paths that do not reuse pools.
2. **Shared-pool split**: allocate chunks while committing shared pool state between probes.

The final route must use at least two paths and beat the net output (gas-aware when token gas
prices are available) of the best full-order simulated single path from the candidate set. If it
does not, the algorithm returns `InsufficientLiquidity` and lets another worker pool handle the
order — a split that nets less than an unsplit route only adds execution complexity.

## Configuration

```toml
[pools.split_bounded_4_hops]
algorithm = "split_bounded"
num_workers = 1
task_queue_capacity = 1000
min_hops = 1
max_hops = 4
timeout_ms = 60000
max_routes = 1024
```

Use `connector_tokens` when a deployment has domain-specific intermediate tokens that should be
kept in the search even if they are not part of the default anchor set.

## Benchmark notes (from the source branch, 2026-07-07)

These notes compare the bounded candidate search against a no-derived brute-force split on six
large all-protocol Fynd trades. The brute-force split enumerated many more paths and then simulated
them. `split_bounded` keeps almost all of the route quality on these cases while cutting solve time
by about 223x on average.

Raw local artifact: `~/Documents/llm-output/2026-07-07-fynd-penumbra-candidate-split-native-anchor/`

| Trade | Brute-force out | `split_bounded` out | Vs brute-force | Best PFW/BF out | Vs best PFW/BF | Brute-force solve | `split_bounded` solve | Speedup |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 100,000 AAVE to USDC | 2,262,308 | 2,249,716 | -0.6% | 846,969 PFW | +165.6% | 16,326 ms | 68 ms | 240.1x |
| 1,000,000 UNI to USDC | 2,780,769 | 2,760,585 | -0.7% | 2,477,388 BF | +11.4% | 25,119 ms | 85 ms | 295.5x |
| 2,000,000 LINK to USDC | 14,133,257 | 13,685,147 | -3.2% | 9,313,686 BF | +46.9% | 21,718 ms | 101 ms | 215.0x |
| 10,000 WETH to AAVE | 48,817.79 | 47,764.00 | -2.2% | 27,225.89 BF | +75.4% | 17,439 ms | 83 ms | 210.1x |
| 10,000 WETH to UNI | 3,285,908 | 3,373,027 | +2.7% | 2,917,795 BF | +15.6% | 18,746 ms | 85 ms | 220.5x |
| 10,000 WETH to LINK | 2,129,195 | 2,136,072 | +0.3% | 1,964,153 BF | +8.8% | 15,525 ms | 93 ms | 166.9x |

Average solve time:

| Algorithm | Mean solve time |
| --- | ---: |
| No-derived brute-force split | 19,145.5 ms |
| `split_bounded` | 85.8 ms |

For the head-to-head results against `split` and `split_probe` (which this algorithm replaced)
and against Path Frank-Wolfe (live same-block: 36W/2L, +6,461 bps mean; offline 1k samples:
65W/3L and 73W/10L on the common sets), see
[Split variant comparison](split-comparison.md).

## Source Reference

| File | Purpose |
| --- | --- |
| `fynd-core/src/algorithm/split_bounded.rs` | Bounded candidate discovery, split allocation, route selection |
| `fynd-core/src/algorithm/split_primitives.rs` | Shared-hop merging and executable route assembly |
| `fynd-core/src/worker_pool/registry.rs` | Maps `"split_bounded"` to `SplitBoundedAlgorithm` |

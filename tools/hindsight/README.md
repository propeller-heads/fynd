# Hindsight

Hindsight measures how Fynd would have performed on **real, settled aggregator swaps**. For each
block it decodes the aggregator trades that actually executed on-chain, re-solves the same
`(token_in, token_out, amount_in)` through Fynd, and compares Fynd's output against what settled.

It is the live counterpart to the `fynd-benchmark audit` tool: where `audit` compares Fynd against
other aggregators' *quotes*, Hindsight compares Fynd against what those aggregators *actually
delivered* on-chain.

## Build

```bash
cargo build -p hindsight --release
```

The binary is `hindsight`. Run any subcommand with `--help` for the full option list.

## Subcommands

### `decode` — decode settled aggregator swaps

Decodes every aggregator trade that settled in a block (or range) straight from on-chain data over
a standard RPC. No external data provider is required.

```bash
hindsight decode --rpc-url "$ETH_RPC_URL" --block 21000000
hindsight decode --rpc-url "$ETH_RPC_URL" --range 21000000-21000010 --json
```

Each decoded trade carries two levels of attribution (see [Methodology](#methodology)): the
**client** that initiated the trade and the **aggregator** that settled it.

### `verify` — check the decoder against Allium

Decodes a block locally and diffs every transaction against [Allium](https://docs.allium.so/)'s
`aggregator_trades` ground-truth dataset (tokens, amounts in bps, aggregator attribution),
producing a worklist of gaps and mismatches.

```bash
hindsight verify --rpc-url "$ETH_RPC_URL" \
  --allium-key "$ALLIUM_API_KEY" \
  --range 21000000-21000010 \
  --tolerance-bps 50
```

### `resolve` — re-solve through Fynd and compare

Decodes a block's trades and re-solves each through a **running Fynd instance**, reporting how
Fynd compares to what settled.

```bash
# Requires a healthy Fynd solver reachable at --fynd-url.
hindsight resolve \
  --rpc-url "$ETH_RPC_URL" \
  --fynd-url http://localhost:3000 \
  --range 21000000-21000005

# Serve Prometheus metrics while running (keeps serving until ctrl-c):
hindsight resolve --rpc-url "$ETH_RPC_URL" --range 21000000-21000005 --metrics-port 9899
```

Output is a terminal win/loss summary, or structured data with `--json`.

## Configuration

| Variable / flag | Used by | Purpose |
|---|---|---|
| `ETH_RPC_URL` / `--rpc-url` | all | Ethereum RPC endpoint for decoding |
| `FYND_URL` / `--fynd-url` | `resolve` | Base URL of a running Fynd solver (default `http://localhost:3000`) |
| `ALLIUM_API_KEY` / `--allium-key` | `verify` | Allium API key |
| `--chain` | `resolve` | Chain label applied to metrics (default `ethereum`) |
| `--metrics-port` | `resolve` | Serve Prometheus `/metrics` on this port |
| `--timeout-ms` | `resolve` | Per-quote timeout for Fynd (default `10000`) |

## Metrics & dashboard

With `--metrics-port`, `resolve` exposes Prometheus metrics at `/metrics`:

| Metric | Type | Labels |
|---|---|---|
| `hindsight_trades_total` | counter | `client`, `aggregator`, `pair`, `chain`, `outcome` |
| `hindsight_savings_bps` | histogram | `client`, `aggregator`, `chain` |
| `hindsight_coverage_ratio` | gauge | — |
| `hindsight_block_processing_seconds` | histogram | — |

A Grafana dashboard is provided at [`grafana/hindsight.json`](grafana/hindsight.json) — import it
and point it at your Prometheus datasource.

## Methodology

**Decoding.** For each block, Hindsight fetches all receipts in one `eth_getBlockReceipts` call and
matches transactions two ways: by entry point (`tx.to` is a known client or aggregator router) and
by log signature (a known aggregator contract emitted a log, catching filler-initiated intent
fills). Matched transactions are traced with `debug_traceTransaction` to recover native-ETH legs and
to attribute the settling aggregator by walking the call frames. The decode nets a party's ERC-20 and
native flow into `(token_in, amount_in, token_out, amount_out)`.

**Client vs aggregator.** A trade has two attribution levels: the **client** (the platform that
initiated it, e.g. Relay or Matcha — who Fynd is compared against) and the **aggregator** (the solver
the client routed through, e.g. 1inch or 0x — who to blame when a trade settled worse than Fynd).
When a user swaps directly through an aggregator router, client and aggregator are the same.

**Comparison.** Fynd's output is compared to the settled amount as a basis-point delta, both raw and
net of Fynd's estimated gas. A trade is a **win** when Fynd's net-of-gas output strictly exceeds the
settled amount, a **loss** otherwise, and **unsolvable** when Fynd cannot produce a quote (missing
token in Tycho, insufficient liquidity, timeout).

## Limitations

- **Single block-state (v0).** `resolve` currently compares at the chain's *current* state. The
  intended top-of-block (N-1) / back-of-block (N) range — solving before and after the block's own
  swaps moved the pools — depends on `BlockStepController` stepping being wired into `fynd-core`.
- **No USD denomination yet.** Savings are reported in basis points and token amounts. USD
  conversion needs an external price source; Fynd exposes no public token→USD conversion.
- **Ethereum only.** The decoder targets Ethereum mainnet. Other chains are a `--chain` label on
  metrics but are not yet decoded.
- **Known decode gaps.** Log-silent protocols (e.g. ParaSwap Delta's canonical contract emits no
  log) are missed rather than guessed. WETH and native ETH are reported as distinct tokens. The
  maker-detection heuristic assumes an EOA maker. Uniswap Universal Router is treated as an
  aggregator. Missing some trades is acceptable for v0.

# Slippage Feature Collection — Architecture & Operations Guide

**Epic**: [ENG-5986](https://propeller-heads.atlassian.net/browse/ENG-5986)
**Branch**: `explore/slippage-features`
**Spec**: `docs/superpowers/specs/2026-05-18-slippage-feature-collection-design.md`

## Overview

A prospective data-collection system that captures Fynd quotes in real-time and
resimulates them at blocks X+1..X+10 to measure solution decay. The system
decomposes decay into market-wide movement vs route-specific slippage, enabling
prediction of when routes will revert.

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                       Fynd Process                           │
│                                                              │
│  Quote Driver ──HTTP──> Solver ──SolverObserver──> mpsc ──>  │
│  (10k trades)           (feature-gated)       │    Tycho     │
│                                                │    Resim     │
│                                                │    Task      │
│                                                ▼    │         │
│                                          Quote Log  │         │
│                                          (parquet)  ▼         │
│                                                Per-hop decay  │
│                                                + re-quote     │
│                                                (parquet)      │
└────────────────────────────────┼──────────────────────────────┘
                                 │
                                 ▼
                    ┌──────────────────────────┐
                    │  Node Resim Process       │
                    │  (out-of-process binary)  │
                    │  eth_call w/ overrides    │
                    │  → route-level decay     │
                    │  (parquet)                │
                    └──────────────────────────┘
                                 │
                                 ▼
                    ┌──────────────────────────┐
                    │  Feature Assembly         │
                    │  (offline batch)          │
                    │  + CoinGecko + gap detect │
                    │  → unified parquet        │
                    └──────────────────────────┘
```

## Components

### 1. SolverObserver (fynd-core, feature-gated)

**Files**: `fynd-core/src/observer.rs`, call sites in `most_liquid.rs` and
`worker_pool_router/mod.rs`

Emits `QuoteProducedEvent` for every solver response after encoding. Events
include full route, calldata, algorithm info, and solver competition data.

Key decisions:
- Uses `ObservedRoute`/`ObservedSwap` (owned, cloneable) instead of `Route`/`Swap`
  (which contain non-Clone `Box<dyn ProtocolSim>`)
- Force-encodes with `slippage=0.9999` so `minAmountOut > 0` (router rejects 0)
- Skips quotes with no route (solver found nothing)
- Multi-solver: each solver emits independently, grouped by `request_id`

### 2. Tycho Resim Task (in-process)

**File**: `tools/slippage-features/src/tycho_resim.rs`

Background tokio task that:
1. Receives `QuoteProducedEvent` via `mpsc::Receiver`
2. Maintains a sliding window of pending quotes (max 10 blocks)
3. On each new block: resimulates every hop via `ProtocolSim::get_amount_out`
4. Captures pool state features (depth, spot_price, fee_tier) from `SharedMarketData`
5. Optionally re-quotes via HTTP for market movement decomposition
6. Flushes to parquet when the 10-block window closes

### 3. Quote Log Observer

**File**: `tools/slippage-features/src/quote_log.rs`

`SolverObserver` implementation that:
- Forwards events to the Tycho resim channel
- Buffers events and flushes to quote log parquet (configurable threshold + time-based)
- Used by the node resim binary as its input source

### 4. Node Resim Binary (out-of-process)

**File**: `tools/slippage-features/src/bin/node_resim.rs`

Reads quote log parquet and resimulates via `eth_call` with storage overrides:
- Dynamic balance/allowance slot detection (brute-force probing)
- Calls at historical blocks via archive-capable RPC
- Produces ground-truth decay + gas measurements

### 5. Quote Driver Binary

**File**: `tools/slippage-features/src/bin/quote_driver.rs`

Loads the 10k benchmark trade dataset and replays them through Fynd's API
on a configurable schedule. This is the data source — without it, only manual
quotes generate data.

### 6. Feature Assembly Binary

**File**: `tools/slippage-features/src/bin/assemble.rs`

Offline join of three parquet sources into a unified dataset:
- CoinGecko token/pair classification
- Route topology, chain/env, temporal features
- Temporal gap detection and reporting

## Parquet Schemas

The Tycho resim produces three normalized outputs under `./slippage-data/`:
- **Hop Static**: once per hop per quote (pool identity + fee)
- **Hop Decay**: per hop per block offset (volatile pool state + decay)
- **Tycho Route Decay**: per block offset (route-level aggregates + decomposition)

The node resim produces a fourth output:
- **Route Decay**: per block offset (eth_call ground truth)

### Quote Log (`./slippage-data/quote_log_*.parquet`)

17 columns:

| Column | Type | Description |
|--------|------|-------------|
| quote_id | string | Unique quote identifier |
| solver_id | string | Which solver produced this |
| request_id | string | Groups solver responses for same request |
| is_winner | bool | Whether this solver's quote was selected |
| block_number | u64 | Block at quote time |
| chain_id | u64 | EVM chain ID |
| amount_in | string | Input amount (BigInt) |
| amount_out | string | Output amount (BigInt) |
| gas_estimate | u64 | Gas estimate |
| algorithm_type | string | Solver algorithm used |
| n_alternatives | u32 | Number of candidate routes |
| gap_to_second_best_bps | f64 | Gap to 2nd best (nullable) |
| slippage_tolerance | f64 | Configured slippage (nullable) |
| token_in | string | Input token address (0x-prefixed) |
| token_out | string | Output token address (0x-prefixed) |
| route_json | string | JSON-serialized route with all hops |
| calldata_hex | string | Hex-encoded transaction calldata |

### Hop Static (`./slippage-data/hop_static/hop_static_*.parquet`)

6 columns — one row per hop per quote (not repeated across block offsets):

| Column | Type | Description |
|--------|------|-------------|
| quote_id | string | Links to quote log |
| solver_id | string | Which solver |
| hop_index | u32 | 0-based hop in route |
| component_id | string | Pool address |
| protocol | string | Pool type (uniswap_v3, etc.) |
| fee_tier | f64 | Pool fee (e.g., 0.0005, nullable) |

### Hop Decay (`./slippage-data/hop_decay/hop_decay_*.parquet`)

10 columns — volatile per-hop data re-read at each block X+k:

| Column | Type | Description |
|--------|------|-------------|
| quote_id | string | Links to quote log |
| solver_id | string | Which solver |
| block_offset | u32 | k in 1..10 |
| hop_index | u32 | 0-based hop in route |
| hop_amount_out | string | Simulated output at X+k (BigInt) |
| hop_decay_bps | f64 | Per-hop decay |
| depth_at_1pct | string | Pool depth at 1% impact at X+k (BigInt) |
| depth_at_5pct | string | Pool depth at 5% impact at X+k (BigInt) |
| spot_price | f64 | Pool spot price at X+k |
| token_price_in_native | f64 | Token price in gas token at X+k |

### Tycho Route Decay (`./slippage-data/tycho_route_decay/tycho_route_decay_*.parquet`)

8 columns — route-level Tycho resim data (one row per block offset, not per hop):

| Column | Type | Description |
|--------|------|-------------|
| quote_id | string | Links to quote log |
| solver_id | string | Which solver |
| block_offset | u32 | k in 1..10 |
| route_total_amount_out | string | Full route output at X+k (BigInt) |
| route_decay_bps | f64 | Full route decay (Tycho sim) |
| requote_amount_out | string | Fresh solver quote at X+k (BigInt, nullable) |
| market_movement_bps | f64 | Decay from market-wide movement |
| execution_slippage_bps | f64 | Decay specific to route staleness |

**Decay decomposition**:
```
total_decay (route_decay_bps) = market_movement_bps + execution_slippage_bps
```
- `execution_slippage ≈ 0`: decay is just market movement — route is fine
- `execution_slippage >> 0`: route went stale — revert risk

### Route Decay (`./slippage-data/route_decay/route_decay.parquet`)

9 columns — ground-truth eth_call data from node resim:

| Column | Type | Description |
|--------|------|-------------|
| quote_id | string | Links to quote log |
| solver_id | string | Which solver |
| request_id | string | Groups responses |
| block_offset | u32 | k in 1..N |
| eth_call_amount_out | string | Actual node output (BigInt) |
| eth_call_gas_used | u64 | Gas consumed |
| eth_call_success | bool | Structural revert? |
| eth_call_revert_reason | string | If reverted (nullable) |
| eth_call_decay_bps | f64 | Route decay vs original |

### Unified Dataset (`./slippage-data/unified/`)

31 columns — join of hop decay, Tycho route decay, and node route decay
plus computed features (token classification, route topology, chain/env,
temporal). Partitioned by chain_id.

### Join pattern for analysis

```
hop_decay
  JOIN hop_static USING (quote_id, solver_id, hop_index)
  JOIN tycho_route_decay USING (quote_id, solver_id, block_offset)
  JOIN route_decay USING (quote_id, solver_id, block_offset)
  JOIN quote_log USING (quote_id, solver_id)
```

## How to Run

### Prerequisites

- Rust toolchain (latest stable)
- Tycho API key
- Ethereum RPC URL (for Fynd + gas prices)
- Archive-capable RPC URL (for node resim historical calls)
- CoinGecko API key (for token classification in assembly)
- 10k trade dataset: `cargo run -p fynd-benchmark --release -- download-trades`

### Step 1: Start Fynd with slippage features

```bash
export TYCHO_API_KEY="your-key"
export RPC_URL="https://your-rpc-provider.com/v1/key"
export SLIPPAGE_FLUSH_THRESHOLD=100    # parquet flush every N quotes
export SLIPPAGE_FLUSH_INTERVAL_SECS=60 # or every N seconds
export SLIPPAGE_REQUOTE_URL=http://localhost:3000  # for re-quote decomposition

cargo run --features slippage-features --release -- serve \
  --chain Ethereum \
  --tycho-api-key "$TYCHO_API_KEY" \
  --rpc-url "$RPC_URL" \
  --min-tvl 10 \
  --protocols uniswap_v2,uniswap_v3
```

**Expected output**: `slippage feature collection enabled` in logs, plus
normal Fynd startup (Tycho connection, worker pools, derived data).

**Data produced**: nothing yet — need to send quotes.

### Step 2: Start the quote driver

```bash
cargo run --features slippage-features --release --bin quote-driver -- \
  --trades-file ./trades_10k.json \
  --fynd-url http://localhost:3000 \
  --interval-secs 12 \
  --batch-size 100
```

**Expected output**: logs showing quotes sent per round, success/failure counts.

**Data produced**: quote log parquets appear in `./slippage-data/`. Hop decay
parquets appear in `./slippage-data/hop_decay/` after 10-block windows close
(~2 minutes after first quote).

### Step 3: Let it collect for >= 1 week

The system runs continuously. Data accumulates on disk as parquet files.
Resilience: if Fynd crashes, all flushed data is safe. Restart and continue.

### Step 4: Run node resim (after collection)

```bash
cargo run -p slippage-features --release --bin node-resim -- \
  --quote-log-dir ./slippage-data \
  --output-dir ./slippage-data/route_decay \
  --rpc-url "https://your-archive-rpc.com/v1/key" \
  --max-block-offset 10
```

**Expected output**: progress logs, then `wrote route decay parquet`.

**Data produced**: `./slippage-data/route_decay/route_decay.parquet` with
ground-truth eth_call decay for each quote.

### Step 5: Assemble unified dataset

```bash
COINGECKO_API_KEY="your-key" cargo run -p slippage-features --release --bin assemble -- \
  --quote-log-dir ./slippage-data \
  --hop-decay-dir ./slippage-data/hop_decay \
  --tycho-route-decay-dir ./slippage-data/tycho_route_decay \
  --route-decay-dir ./slippage-data/route_decay \
  --output-dir ./slippage-data/unified
```

**Expected output**: gap detection report, then unified parquet written.

**Data produced**: `./slippage-data/unified/` with partitioned parquet files
ready for analysis in pandas/polars.

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| SLIPPAGE_FLUSH_THRESHOLD | 100 | Quote log parquet flush every N records |
| SLIPPAGE_FLUSH_INTERVAL_SECS | 60 | Periodic flush interval (seconds) |
| SLIPPAGE_REQUOTE_URL | http://localhost:3000 | Fynd URL for re-quote decomposition |
| COINGECKO_API_KEY | (none) | CoinGecko API key for token classification |

## Feature Taxonomy (7 v1 families)

| # | Family | Source | Capture point |
|---|--------|--------|---------------|
| 1 | Fynd quote-context | SolverObserver | In-process, at quote time |
| 2 | Fynd algorithm/config | SolverObserver | In-process, at quote time |
| 3 | Pool state (depth, fee, spot_price) | Tycho resim | In-process, per block |
| 4 | Token & pair (CoinGecko) | Assembly binary | Offline |
| 5 | Route topology | Assembly binary | Offline |
| 6 | Chain/env | Assembly binary | Offline |
| 7 | Temporal | Assembly binary | Offline |

## v2 Feature Families (deferred)

| Family | Source | JIRA |
|--------|--------|------|
| CEX dynamics (vol, skew, CEX-DEX spread) | Binance/Coinbase | ENG-5990 |
| Onchain flow (OFI, VWAP, aggregator share) | Dune | ENG-5991 |
| Empirical-Bayes priors | Derived from accumulated data | — |
| marginal_liquidity (v3/v4 tick) | ProtocolSim | — |
| concentration_gini (v3/v4) | Tick analysis | — |

## Known Limitations

1. **Prospective only**: no historical replay. Must run live to accumulate data.
2. **Re-quote fill rate**: ~60-70% at X+10, lower at intermediate offsets. The
   HTTP re-quote call sometimes times out or returns 0 when Fynd is busy.
3. **marginal_liquidity and concentration_gini**: always NaN. Requires v3/v4
   tick-level analysis not yet implemented.
4. **Single chain at a time**: run separate Fynd instances for Ethereum vs Base.
5. **Router address hardcoded**: if the Tycho Router is upgraded, update the
   default in `node_resim.rs`.

# Slippage Feature Collection — Design Spec

**Epic**: [ENG-5986](https://propeller-heads.atlassian.net/browse/ENG-5986) — Measure Fynd Revert Rate
**Date**: 2026-05-18
**Status**: Draft
**Branch**: `explore/slippage-features` (nothing merges to `main`)

## Goal

Build a prospective data-collection system that captures Fynd quotes in real-time and
resimulates them at blocks X+1..X+N (configurable, default N=10) to measure solution
decay. The dataset enables offline research into predicting when route decay exceeds
slippage tolerance (i.e., predicting reverts).

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                       Fynd Process                           │
│                                                              │
│  Solver ──SolverObserver──> tokio::mpsc ──> Tycho Resim Task │
│     (feature-gated)            │              (in-process)   │
│                                │              │              │
│                                ▼              ▼              │
│                          Quote Log      Per-hop decay        │
│                          (parquet)      (parquet)             │
└────────────────────────────────┼─────────────────────────────┘
                                 │ reads quote log
                                 ▼
                    ┌──────────────────────────┐
                    │  Node Resim Process       │
                    │  (out-of-process binary)  │
                    │                           │
                    │  eth_call w/ overrides    │
                    │  at blocks X+1..X+10     │
                    │  → route-level decay     │
                    │  (parquet)                │
                    └──────────────────────────┘
                                 │
                                 ▼
                    ┌──────────────────────────┐
                    │  Feature Assembly         │
                    │  (offline batch)          │
                    │                           │
                    │  Joins: quote log         │
                    │  + per-hop decay          │
                    │  + route-level decay      │
                    │  + token/pair (CoinGecko) │
                    │  → unified parquet        │
                    └──────────────────────────┘
```

## Components

### Component 1: SolverObserver Trait (fynd-core)

**Location**: `fynd-core/src/observer.rs`, behind `#[cfg(feature = "slippage-features")]`

**Purpose**: Emit structured events from the solver's hot path with minimal
footprint. When the feature flag is off, the trait compiles to zero overhead.

**Trait surface**:

```rust
/// Configurable block horizon for decay observation.
pub const MAX_BLOCK_OFFSET: u32 = 10;

pub trait SolverObserver: Send + Sync {
    fn on_route_scored(&self, route: &Route, score: f64, rank: usize);
    fn on_quote_produced(&self, event: QuoteProducedEvent);
}

/// Groups all solver responses for the same inbound request.
/// Each solver in a multi-solver config emits its own QuoteProducedEvent
/// sharing the same request_id, enabling cross-solver comparison.
pub struct QuoteProducedEvent {
    pub request_id: String,
    pub quote_id: String,
    pub solver_id: String,
    pub is_winner: bool,
    pub block_number: u64,
    pub chain_id: u64,
    pub route: Route,
    pub amount_in: String,       // BigInt string — no precision loss
    pub amount_out: String,      // BigInt string — no precision loss
    pub gas_estimate: u64,
    pub calldata: Bytes,
    pub algorithm_type: String,
    pub algorithm_settings: HashMap<String, String>,
    pub n_alternatives: u32,
    pub gap_to_second_best_bps: Option<f64>,
    pub score_dispersion: Option<f64>,
    pub slippage_tolerance: Option<f64>,
    pub all_candidates: Vec<CandidateSummary>,
}

pub struct CandidateSummary {
    pub route: Route,
    pub score: f64,
    pub amount_out: String,      // BigInt string — no precision loss
}
```

**Multi-solver support**: Fynd uses a multi-solver configuration. Each solver emits
its own `QuoteProducedEvent` with a unique `solver_id`. All events for the same
inbound request share a `request_id`, enabling cross-solver decay comparison.
The `is_winner` flag marks which solver's quote was selected. The resim pipeline
processes all solver events independently — each gets its own decay curve.

**Call sites** (additive insertions):

1. `MostLiquidAlgorithm::simulate_path` — call `observer.on_route_scored()` for each
   candidate after scoring
2. **After encoding** (~line 208 in `worker_pool_router/mod.rs`) — call
   `observer.on_quote_produced()` for **each solver's response**, not just the winner.
   The winner gets `is_winner: true`.

**Encoding policy**: when `slippage-features` is enabled, **always encode all
solutions** with `EncodingOptions::new(1.0)` (100% slippage) and a pre-configured
sender address. This ensures:
- `calldata` is always populated in `QuoteProducedEvent` (no empty calldata gap)
- `min_amount_out = 0` is baked into the calldata (the node resim binary can
  replay calldata as-is with no ABI modification needed)
- All quotes come from a seed file, so the performance cost of always-encoding
  is acceptable

**Diff estimate**: ~50-100 lines added to fynd-core. No existing signatures change.

**Precision policy**: all token amounts (`amount_in`, `amount_out`, per-hop amounts)
are `String` (BigInt representation) to prevent precision loss. Derived ratios
(decay in bps, feature scores) remain `f64`.

### Component 2a: Tycho Resim Task (in-process)

**Location**: `tools/slippage-features/src/tycho_resim.rs`
**Linked into Fynd via**: feature flag `slippage-features` on the fynd binary crate

**Lifecycle**:

1. Spawned as a `tokio::spawn` background task at Fynd startup (when feature enabled)
2. Receives `QuoteProducedEvent` via `tokio::mpsc::Receiver`
3. Maintains a sliding window of pending quotes (max age: `MAX_BLOCK_OFFSET` blocks)
4. On each new block (detected via `SharedMarketData` block number change):
   - For each pending quote where `current_block - quote_block <= MAX_BLOCK_OFFSET`:
     - Walk each hop in the route
     - Call `ProtocolSim::get_amount_out` against current `SharedMarketData` state
     - Record per-hop amount_out and compute per-hop decay in bps
   - Capture pool state features from the same snapshot (depth, fee tier, marginal
     liquidity at active tick, concentration Gini for v3/v4)
5. When a quote's `MAX_BLOCK_OFFSET`-block window closes, flush the complete record to parquet

**Pool depth vs TVL**: TVL is not directly available from Fynd and Tycho measures it
in native token (ETH), not USD. More importantly, **depth** (how much can be traded
before moving price by X%) is a better predictor of slippage than TVL, because it
accounts for liquidity concentration. We use depth-related metrics instead:
- `depth_at_1pct`: max input amount before 1% price impact. **Already computed** by
  `PoolDepthComputation` in `fynd-core/src/derived/computations/pool_depth.rs` using
  `sim_state.query_pool_swap()` with a `TradeLimitPrice` constraint.
- `depth_at_5pct`: same at 5% price impact. **New**: add a second threshold to the
  existing `PoolDepthComputation` (same mechanism, just `slippage_threshold = 0.05`).
- `spot_price`: already computed by `SpotPriceComputation` (dependency of pool depth).
- `token_price_in_native`: already computed by `TokenGasPriceComputation`.
- `marginal_liquidity`: v3/v4 liquidity at the active tick (direct from Tycho state).

Since depth, spot_price, and token_price_in_native are already part of the derived
data pipeline, the Tycho resim task reads them from `SharedMarketData` rather than
recomputing. The only new work is adding the 5% depth threshold to
`PoolDepthComputation` alongside the existing 1%.

Depth can be cached — it doesn't need to be perfectly fresh. A few blocks of staleness
is acceptable since we're measuring relative decay, not absolute prices.

**Output schema** (per-hop decay parquet):

| Column | Type | Description |
|--------|------|-------------|
| quote_id | string | Links to quote log |
| solver_id | string | Which solver produced this quote |
| request_id | string | Groups solver responses for same request |
| block_offset | u32 | k in 1..MAX_BLOCK_OFFSET |
| hop_index | u32 | 0-based index in route |
| component_id | string | Pool address |
| protocol | string | Pool type (uniswap_v3, curve, etc.) |
| hop_amount_out | string | Simulated output at X+k (BigInt) |
| hop_decay_bps | f64 | Per-hop decay |
| depth_at_1pct | string | Pool depth at 1% price impact (BigInt, from derived data) |
| depth_at_5pct | string | Pool depth at 5% price impact (BigInt, new threshold in PoolDepthComputation) |
| spot_price | f64 | Pool spot price (from SpotPriceComputation) |
| token_price_in_native | f64 | Token price in native gas token (from TokenGasPriceComputation) |
| fee_tier | f64 | Pool fee |
| marginal_liquidity | string | v3/v4: liquidity at active tick (BigInt) |
| concentration_gini | f64 | v3/v4: liquidity distribution |
| route_total_amount_out | string | Full route output at X+k (BigInt) |
| route_decay_bps | f64 | Full route decay (Tycho sim) |

**Dependencies**: `fynd-core` (SharedMarketData, ProtocolSim), `arrow`/`parquet` crates

### Component 2b: Node Resim Process (out-of-process)

**Location**: `tools/slippage-features/src/bin/node_resim.rs`

**Purpose**: Ground-truth resimulation via `eth_call` with storage overrides.
Runs as a separate binary, decoupled from Fynd's hot path.

**Flow**:

1. Reads the quote log parquet (written by the observer flush)
2. For each quote, extracts the calldata (already encoded with 100% slippage /
   `min_amount_out = 0` — no ABI modification needed)
3. For blocks X+1..X+`MAX_BLOCK_OFFSET`, calls `eth_call` with storage overrides
   (balance/approval injection, reusing helpers from `fynd-swap-cli/src/erc20.rs`)
   against an archive-capable RPC at `BlockId::number(X+k)`
4. Decodes `amount_out` from first 32 bytes of return data (same as
   `dry_run_execute` in `clients/rust/src/client.rs:1175`)
5. Records route-level output amount and gas used
6. Writes to route-level decay parquet

**Output schema** (route-level decay parquet):

| Column | Type | Description |
|--------|------|-------------|
| quote_id | string | Links to quote log |
| solver_id | string | Which solver produced this quote |
| request_id | string | Groups solver responses for same request |
| block_offset | u32 | k in 1..MAX_BLOCK_OFFSET |
| eth_call_amount_out | string | Actual node output at X+k (BigInt) |
| eth_call_gas_used | u64 | Gas consumed |
| eth_call_success | bool | Did the call revert? (structural only — slippage set to 100%) |
| eth_call_revert_reason | string | If reverted: reason (pool drained, etc.) |
| eth_call_decay_bps | f64 | Route decay vs original quote |

**Dependencies**: `ethers`/`alloy` for RPC, storage override logic from fynd-swap-cli

**Can run behind**: no real-time constraint. Can batch process hours/days of quotes
as long as the archive node has the state.

### Component 3: Feature Assembly (offline)

**Location**: `tools/slippage-features/src/bin/assemble.rs`

**Purpose**: Join the three parquet sources into a unified analysis dataset.

**Inputs**:
1. Quote log parquet (from observer): quote_id, block, chain, route, fynd_context,
   fynd_algorithm, calldata
2. Per-hop decay parquet (from Tycho resim): per-hop decay + pool state features
3. Route-level decay parquet (from node resim): ground-truth decay + gas

**Additional features computed here**:
- Token/pair classification (CoinGecko API, cached): mcap, FDV, age, category,
  pair bucket, continuous mcap features
- Route topology: hop_count, split_count, gas_estimate, pool_type_mix
- Chain/env: chain_id, base_fee, gas p50/p95, is_l2, sequencer_lag
- Temporal: hour_of_day, day_of_week, minutes_since_hour

**Output**: unified parquet, partitioned by chain_id, one file per day

**Validation**:
- Point-in-time integrity: all feature timestamps <= quote block timestamp
- Schema completeness: all 7 v1 families present
- v2 nullable columns present (cex_dynamics, onchain_flow, priors)
- Cross-chain coverage report (Ethereum + Base)

## Feature Taxonomy (7 v1 families)

| # | Family | Source | Capture point |
|---|--------|--------|---------------|
| 1 | Fynd quote-context | SolverObserver | In-process, at quote time |
| 2 | Fynd algorithm/config | SolverObserver | In-process, at quote time |
| 3 | Pool state | Tycho resim task | In-process, at each block X+k |
| 4 | Token & pair | CoinGecko (cached) | Offline assembly |
| 5 | Route topology | Derived from route | Offline assembly |
| 6 | Chain/env | RPC at capture time | In-process or offline |
| 7 | Temporal | Block timestamp | Offline assembly |

## Labels (dual)

| Label | Source | Granularity |
|-------|--------|-------------|
| Tycho sim decay | Component 2a | Per-hop + per-route, bps |
| Node eth_call decay | Component 2b | Per-route only, bps + gas + revert flag |

Cross-validation between the two reveals Tycho simulation accuracy.

## Constraints

- **Isolation**: all code on feature branches off `explore/slippage-features`.
  Nothing merges to `main`. Code quality and best practices apply — findings
  serve as reference for the full implementation.
- **Chains v1**: Ethereum mainnet + Base. Arbitrum/Optimism deferred to v2.
- **Prospective only**: no historical replay. Data accumulates over time from live Fynd.
- **Feature-gated**: `slippage-features` Cargo feature. Zero overhead when off.
- **Minimal fynd-core diff**: ~50-100 lines (trait + call sites). No existing
  signatures or behavior change.
- **Configurable horizon**: block offset is `MAX_BLOCK_OFFSET` (default 10), not
  hardcoded throughout. Changing it is a single-constant edit.
- **Precision**: all token amounts are `String` (BigInt representation). No `f64`
  for amounts. Derived ratios (decay bps, feature scores) are `f64`.
- **Multi-solver**: each solver emits independently. Events share `request_id`
  for cross-solver joins.
- **100% slippage at encoding time**: all solutions encoded with `slippage = 1.0`,
  so `min_amount_out = 0` is baked into calldata. Node resim replays as-is —
  reverts only capture structural failures, giving us true decay values.

## What the existing scaffold gets wrong

The `tools/slippage-features/` code from the previous `ooo run` was built for a
retrospective JSON-parsing pipeline. The revised design is prospective + in-process.
Specifically:

- `quote_record.rs`, `quote_loader.rs`, `schema.rs` — parse JSON files; we receive
  typed Rust structs from the observer
- `join.rs` — in-memory HashMap join; we join parquet files
- `edge_cases.rs` — tests for the JSON pipeline
- `Cargo.toml` — no deps on fynd-core, tycho-simulation, or parquet

**Decision**: discard the scaffold and start fresh from main. The decay math
(`compute_single_block_decay`, bps formula) and pure feature functions
(`hop_count`, `split_count`, `gas_estimate_f64`) are ~200 lines of reusable
logic that can be re-incorporated into the new design.

## Revised JIRA story mapping

| Story | Original scope | Revised scope |
|-------|---------------|---------------|
| ENG-5987 | "Instrument replay engine" | SolverObserver trait + call sites in fynd-core |
| ENG-5988 | "Pool state extractor via Tycho" | Pool state captured in Tycho resim task (2a) |
| ENG-5989 | "Token/pair classification" | Unchanged — CoinGecko in assembly step |
| NEW | — | Tycho resim task (Component 2a) |
| NEW | — | Node resim process (Component 2b) |
| ENG-5992 | "Assemble dataset" | Feature assembly joining 3 parquet sources |
| ENG-5993 | "Exploratory analysis" | Unchanged — consumes unified parquet |
| ENG-5990 | "[v2] CEX dynamics" | Unchanged — deferred |
| ENG-5991 | "[v2] Dune onchain flow" | Unchanged — deferred |

## Exit criteria

- Dataset covers >= 1 week of live Fynd quotes on Ethereum and Base
- Both decay labels populated (Tycho per-hop + node route-level)
- All 7 v1 feature families present with explicit null handling
- Point-in-time integrity validated
- Exploratory notebook produces decay-vs-feature correlation report
- Cross-validation between Tycho sim and node eth_call documented

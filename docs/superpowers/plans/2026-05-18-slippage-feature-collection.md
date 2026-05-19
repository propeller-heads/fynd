# Slippage Feature Collection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a prospective data-collection system that captures Fynd quotes in real-time, resimulates at blocks X+1..X+N via both Tycho (per-hop) and eth_call (route-level), and outputs a parquet dataset for offline slippage-decay research.

**Architecture:** Feature-gated SolverObserver trait in fynd-core emits quote events via tokio::mpsc to an in-process Tycho resim task (per-hop decay + pool state). A separate out-of-process binary does ground-truth eth_call resimulation. An offline assembly step joins the three parquet sources with CoinGecko token metadata into a unified analysis dataset.

**Tech Stack:** Rust, tokio, alloy (Ethereum RPC), arrow/parquet crates, fynd-core (SharedMarketData, ProtocolSim), CoinGecko REST API

**Spec:** `docs/superpowers/specs/2026-05-18-slippage-feature-collection-design.md`
**Branch:** `explore/slippage-features` (nothing merges to `main`)

---

## Pre-requisite: Clean up existing scaffold

Before starting, remove the `ooo run` scaffold that was built for the wrong architecture.

- [ ] **Step 1: Remove the old scaffold**

```bash
git rm -r tools/slippage-features/
git checkout -- Cargo.toml Cargo.lock
```

Edit `Cargo.toml` to remove `"tools/slippage-features"` from `[workspace].members` if it was added.

- [ ] **Step 2: Commit the cleanup**

```bash
git add -A
git commit -m "chore: remove ooo-generated scaffold (wrong architecture) [ENG-5986]"
```

---

## Task 1: Add `slippage-features` feature flag to workspace

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `fynd-core/Cargo.toml`

- [ ] **Step 1: Add the feature flag to fynd-core**

In `fynd-core/Cargo.toml`, add a feature that currently does nothing:

```toml
[features]
default = []
slippage-features = []
```

- [ ] **Step 2: Add arrow and parquet to workspace deps**

In the root `Cargo.toml` under `[workspace.dependencies]`:

```toml
arrow = { version = "55", default-features = false, features = ["json", "prettyprint"] }
parquet = { version = "55", default-features = false, features = ["arrow", "snap", "zstd"] }
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p fynd-core`
Expected: compiles with no warnings

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml fynd-core/Cargo.toml
git commit -m "chore: add slippage-features flag and arrow/parquet workspace deps [ENG-5986]"
```

---

## Task 2: Add 5% depth threshold to PoolDepthComputation

**Files:**
- Modify: `fynd-core/src/derived/computations/pool_depth.rs`
- Modify: tests for pool depth (find with `rg "PoolDepthComputation" --type rust -l`)

The existing `PoolDepthComputation` computes depth at 1%. We need to also compute at 5%. The cleanest approach: make the struct accept multiple thresholds and output a result per threshold.

- [ ] **Step 1: Read the current implementation**

Read `fynd-core/src/derived/computations/pool_depth.rs` to understand the current `compute()` method, its return type, and how it stores results in the derived data store.

- [ ] **Step 2: Add a second PoolDepthComputation instance for 5%**

The simplest approach that doesn't change the existing 1% interface: register a second `PoolDepthComputation::new(0.05)?` in the derived data pipeline wherever the 1% instance is registered. The result key should distinguish them (e.g., `depth_1pct` vs `depth_5pct`).

Read the derived data registration code (likely in `fynd-core/src/derived/mod.rs` or similar) to find where computations are registered, and add the 5% instance there.

- [ ] **Step 3: Verify existing tests still pass**

Run: `cargo test -p fynd-core -- pool_depth`
Expected: all existing tests pass

- [ ] **Step 4: Add a test for 5% depth**

Add a test that constructs a `PoolDepthComputation::new(0.05)` and verifies it returns a larger depth value than the 1% threshold for the same pool (deeper liquidity = more input before 5% impact).

- [ ] **Step 5: Run tests**

Run: `cargo test -p fynd-core -- pool_depth`
Expected: all tests pass including the new one

- [ ] **Step 6: Commit**

```bash
git add fynd-core/
git commit -m "feat: add 5% pool depth threshold to derived data [ENG-5986]"
```

---

## Task 3: Define SolverObserver trait and event types

**Files:**
- Create: `fynd-core/src/observer.rs`
- Modify: `fynd-core/src/lib.rs` (add module)

- [ ] **Step 1: Create the observer module**

Create `fynd-core/src/observer.rs` with the trait and event types, gated behind the feature flag:

```rust
#![cfg(feature = "slippage-features")]

use std::collections::HashMap;

use alloy::primitives::{Address, Bytes};
use num_bigint::BigUint;

use crate::types::quote::Route;

pub const MAX_BLOCK_OFFSET: u32 = 10;

pub trait SolverObserver: Send + Sync {
    fn on_route_scored(&self, route: &Route, score: f64, rank: usize);
    fn on_quote_produced(&self, event: QuoteProducedEvent);
}

#[derive(Debug, Clone)]
pub struct QuoteProducedEvent {
    pub request_id: String,
    pub quote_id: String,
    pub solver_id: String,
    pub is_winner: bool,
    pub block_number: u64,
    pub chain_id: u64,
    pub route: Route,
    pub amount_in: String,
    pub amount_out: String,
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

#[derive(Debug, Clone)]
pub struct CandidateSummary {
    pub route: Route,
    pub score: f64,
    pub amount_out: String,
}

pub struct NoopObserver;

impl SolverObserver for NoopObserver {
    fn on_route_scored(&self, _route: &Route, _score: f64, _rank: usize) {}
    fn on_quote_produced(&self, _event: QuoteProducedEvent) {}
}
```

Note: `Route` must derive `Clone` for this to work. Check if it already does — if not, you'll need to add `#[derive(Clone)]` to `Route` in `fynd-core/src/types/quote.rs`. The `Swap` struct contains `Box<dyn ProtocolSim>` which may not be `Clone`. If so, the observer should take `&Route` and the event struct should store a serialized representation (the route's swap component IDs, tokens, and amounts — not the ProtocolSim objects). Adjust the struct fields to use owned primitive types instead:

```rust
#[derive(Debug, Clone)]
pub struct ObservedSwap {
    pub component_id: String,
    pub protocol: String,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: String,
    pub amount_out: String,
    pub gas_estimate: String,
    pub split: f64,
}

#[derive(Debug, Clone)]
pub struct ObservedRoute {
    pub swaps: Vec<ObservedSwap>,
}
```

Then use `ObservedRoute` in `QuoteProducedEvent` instead of `Route`.

- [ ] **Step 2: Register the module in lib.rs**

In `fynd-core/src/lib.rs`, add:

```rust
#[cfg(feature = "slippage-features")]
pub mod observer;
```

- [ ] **Step 3: Verify it compiles with the feature enabled**

Run: `cargo check -p fynd-core --features slippage-features`
Expected: compiles with no errors

- [ ] **Step 4: Verify it compiles without the feature (no-op)**

Run: `cargo check -p fynd-core`
Expected: compiles — the observer module is completely absent

- [ ] **Step 5: Write a basic test**

In `fynd-core/src/observer.rs`, add:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_observer_does_not_panic() {
        let obs = NoopObserver;
        let event = QuoteProducedEvent {
            request_id: "req_1".into(),
            quote_id: "q_1".into(),
            solver_id: "solver_a".into(),
            is_winner: true,
            block_number: 100,
            chain_id: 1,
            route: ObservedRoute { swaps: vec![] },
            amount_in: "1000000".into(),
            amount_out: "999000".into(),
            gas_estimate: 21000,
            calldata: Bytes::new(),
            algorithm_type: "most_liquid".into(),
            algorithm_settings: HashMap::new(),
            n_alternatives: 3,
            gap_to_second_best_bps: Some(15.0),
            score_dispersion: Some(0.02),
            slippage_tolerance: Some(0.005),
            all_candidates: vec![],
        };
        obs.on_quote_produced(event);
    }
}
```

- [ ] **Step 6: Run the test**

Run: `cargo test -p fynd-core --features slippage-features -- observer`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add fynd-core/src/observer.rs fynd-core/src/lib.rs
git commit -m "feat: add SolverObserver trait and event types [ENG-5987]"
```

---

## Task 4: Wire SolverObserver into the solver hot path

**Files:**
- Modify: `fynd-core/src/algorithm/most_liquid.rs` (~line 438)
- Modify: `fynd-core/src/worker_pool_router/mod.rs` (~line 191)
- Modify: solver/worker pool structs to hold an `Option<Arc<dyn SolverObserver>>`

This is the most sensitive task — we're inserting into the hot path. All insertions are behind `#[cfg(feature = "slippage-features")]`.

- [ ] **Step 1: Read the solver structs to understand ownership**

Read these files to understand how the solver/router is constructed and what state it holds:
- `fynd-core/src/solver.rs` — the `Solver` struct
- `fynd-core/src/worker_pool_router/mod.rs` — the `WorkerPoolRouter` struct
- `fynd-core/src/algorithm/most_liquid.rs` — the algorithm struct

Determine where to store the `Arc<dyn SolverObserver>` so it's accessible at both call sites.

- [ ] **Step 2: Add observer field to the appropriate struct**

Add a field behind the feature flag. For example, in the struct that orchestrates solving:

```rust
#[cfg(feature = "slippage-features")]
observer: Option<Arc<dyn crate::observer::SolverObserver>>,
```

Update the constructor to accept it (behind the feature flag).

- [ ] **Step 3: Insert on_route_scored call**

In `most_liquid.rs`, after `RouteResult::new()` is constructed (~line 438), add:

```rust
#[cfg(feature = "slippage-features")]
if let Some(ref obs) = observer {
    obs.on_route_scored(&route, net_amount_out_f64, rank);
}
```

The observer reference needs to be passed down to `simulate_path`. Add it as an optional parameter behind the feature flag, or pass it via the algorithm struct.

- [ ] **Step 4: Insert on_quote_produced call**

In `worker_pool_router/mod.rs`, after ranking is complete (~line 191), construct and emit a `QuoteProducedEvent` for each solver's response:

```rust
#[cfg(feature = "slippage-features")]
if let Some(ref obs) = self.observer {
    for (i, order_quote) in order_quotes.iter().enumerate() {
        let event = crate::observer::QuoteProducedEvent {
            request_id: request_id.clone(),
            quote_id: format!("{request_id}_{i}"),
            solver_id: order_quote.solver_id().to_string(),
            is_winner: i == 0,
            block_number: order_quote.block_info().number,
            chain_id: order_quote.chain_id(),
            route: ObservedRoute::from(order_quote.route()),
            amount_in: order_quote.amount_in().to_string(),
            amount_out: order_quote.amount_out().to_string(),
            // ... fill remaining fields from order_quote
        };
        obs.on_quote_produced(event);
    }
}
```

Adapt field names to match the actual `OrderQuote` accessors. Read `fynd-core/src/types/quote.rs` for the exact accessor names.

- [ ] **Step 5: Verify compilation with and without feature**

Run: `cargo check -p fynd-core --features slippage-features`
Run: `cargo check -p fynd-core`
Expected: both compile with no errors

- [ ] **Step 6: Run existing tests to verify no regressions**

Run: `cargo test -p fynd-core`
Expected: all existing tests pass

- [ ] **Step 7: Commit**

```bash
git add fynd-core/
git commit -m "feat: wire SolverObserver into solver hot path [ENG-5987]"
```

---

## Task 5: Create slippage-features crate with decay math

**Files:**
- Create: `tools/slippage-features/Cargo.toml`
- Create: `tools/slippage-features/src/lib.rs`
- Create: `tools/slippage-features/src/decay.rs`
- Modify: `Cargo.toml` (workspace — add member)

- [ ] **Step 1: Create the crate**

```bash
mkdir -p tools/slippage-features/src
```

Create `tools/slippage-features/Cargo.toml`:

```toml
[package]
name = "slippage-features"
version.workspace = true
edition = "2021"
description = "Prospective slippage decay data collection for Fynd"

[dependencies]
fynd-core = { path = "../../fynd-core", features = ["slippage-features"] }
arrow = { workspace = true }
parquet = { workspace = true }
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
num-bigint = { workspace = true }

[dev-dependencies]
rstest = { workspace = true }
tempfile = "3"
```

- [ ] **Step 2: Add workspace member**

In root `Cargo.toml`, add `"tools/slippage-features"` to `[workspace].members`.

- [ ] **Step 3: Create lib.rs**

```rust
pub mod decay;
```

- [ ] **Step 4: Create decay.rs with the core math**

```rust
use thiserror::Error;

pub const MAX_BLOCK_OFFSET: u32 = fynd_core::observer::MAX_BLOCK_OFFSET;

#[derive(Debug, Error)]
pub enum DecayError {
    #[error("invalid amount '{value}': {reason}")]
    InvalidAmount { value: String, reason: String },
    #[error("quote output is zero; proportional decay is undefined")]
    ZeroQuoteOutput,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecayResult {
    pub block_offset: u32,
    pub decay_bps: f64,
    pub quote_output: f64,
    pub replay_output: f64,
}

pub fn compute_decay_bps(
    quote_amount_out: &str,
    replay_amount_out: &str,
) -> Result<f64, DecayError> {
    let quote = parse_bigint_to_f64(quote_amount_out)?;
    let replay = parse_bigint_to_f64(replay_amount_out)?;

    if quote == 0.0 {
        return Err(DecayError::ZeroQuoteOutput);
    }

    Ok((quote - replay) / quote * 10_000.0)
}

fn parse_bigint_to_f64(s: &str) -> Result<f64, DecayError> {
    s.parse::<f64>()
        .map_err(|_| DecayError::InvalidAmount {
            value: s.to_string(),
            reason: "not a valid number".into(),
        })
        .and_then(|v| {
            if v.is_finite() {
                Ok(v)
            } else {
                Err(DecayError::InvalidAmount {
                    value: s.to_string(),
                    reason: "not finite".into(),
                })
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_decay_when_output_drops() {
        let bps = compute_decay_bps("1000000", "999000").unwrap();
        assert!((bps - 10.0).abs() < 0.01, "expected ~10 bps, got {bps}");
    }

    #[test]
    fn zero_decay_when_unchanged() {
        let bps = compute_decay_bps("1000000", "1000000").unwrap();
        assert!((bps).abs() < 0.001);
    }

    #[test]
    fn negative_decay_when_output_improves() {
        let bps = compute_decay_bps("1000000", "1001000").unwrap();
        assert!(bps < 0.0);
    }

    #[test]
    fn zero_quote_output_is_error() {
        let err = compute_decay_bps("0", "100").unwrap_err();
        assert!(matches!(err, DecayError::ZeroQuoteOutput));
    }
}
```

- [ ] **Step 5: Verify it compiles and tests pass**

Run: `cargo test -p slippage-features`
Expected: 4 tests pass

- [ ] **Step 6: Commit**

```bash
git add tools/slippage-features/ Cargo.toml
git commit -m "feat: create slippage-features crate with decay math [ENG-5986]"
```

---

## Task 6: Implement the Tycho resim task

**Files:**
- Create: `tools/slippage-features/src/tycho_resim.rs`
- Create: `tools/slippage-features/src/parquet_writer.rs`
- Modify: `tools/slippage-features/src/lib.rs`

This is the largest task. The resim task runs as a tokio background task inside the Fynd process.

- [ ] **Step 1: Create parquet_writer.rs — a helper for writing per-hop decay records**

Create `tools/slippage-features/src/parquet_writer.rs`:

Define a `HopDecayRecord` struct (flat, serializable) matching the per-hop decay parquet schema from the spec:

```rust
use arrow::array::*;
use arrow::datatypes::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

pub struct HopDecayRecord {
    pub quote_id: String,
    pub solver_id: String,
    pub request_id: String,
    pub block_offset: u32,
    pub hop_index: u32,
    pub component_id: String,
    pub protocol: String,
    pub hop_amount_out: String,
    pub hop_decay_bps: f64,
    pub depth_at_1pct: String,
    pub depth_at_5pct: String,
    pub spot_price: f64,
    pub token_price_in_native: f64,
    pub fee_tier: f64,
    pub marginal_liquidity: String,
    pub concentration_gini: f64,
    pub route_total_amount_out: String,
    pub route_decay_bps: f64,
}

pub fn hop_decay_schema() -> Schema {
    Schema::new(vec![
        Field::new("quote_id", DataType::Utf8, false),
        Field::new("solver_id", DataType::Utf8, false),
        Field::new("request_id", DataType::Utf8, false),
        Field::new("block_offset", DataType::UInt32, false),
        Field::new("hop_index", DataType::UInt32, false),
        Field::new("component_id", DataType::Utf8, false),
        Field::new("protocol", DataType::Utf8, false),
        Field::new("hop_amount_out", DataType::Utf8, false),
        Field::new("hop_decay_bps", DataType::Float64, false),
        Field::new("depth_at_1pct", DataType::Utf8, true),
        Field::new("depth_at_5pct", DataType::Utf8, true),
        Field::new("spot_price", DataType::Float64, true),
        Field::new("token_price_in_native", DataType::Float64, true),
        Field::new("fee_tier", DataType::Float64, true),
        Field::new("marginal_liquidity", DataType::Utf8, true),
        Field::new("concentration_gini", DataType::Float64, true),
        Field::new("route_total_amount_out", DataType::Utf8, false),
        Field::new("route_decay_bps", DataType::Float64, false),
    ])
}

pub fn write_hop_decay_parquet(
    path: &Path,
    records: &[HopDecayRecord],
) -> Result<(), Box<dyn std::error::Error>> {
    let schema = Arc::new(hop_decay_schema());
    let file = File::create(path)?;
    let mut writer = ArrowWriter::try_new(file, schema.clone(), None)?;

    let batch = arrow::record_batch::RecordBatch::try_new(
        schema,
        vec![
            Arc::new(StringArray::from_iter_values(records.iter().map(|r| r.quote_id.as_str()))),
            Arc::new(StringArray::from_iter_values(records.iter().map(|r| r.solver_id.as_str()))),
            Arc::new(StringArray::from_iter_values(records.iter().map(|r| r.request_id.as_str()))),
            Arc::new(UInt32Array::from_iter_values(records.iter().map(|r| r.block_offset))),
            Arc::new(UInt32Array::from_iter_values(records.iter().map(|r| r.hop_index))),
            Arc::new(StringArray::from_iter_values(records.iter().map(|r| r.component_id.as_str()))),
            Arc::new(StringArray::from_iter_values(records.iter().map(|r| r.protocol.as_str()))),
            Arc::new(StringArray::from_iter_values(records.iter().map(|r| r.hop_amount_out.as_str()))),
            Arc::new(Float64Array::from_iter_values(records.iter().map(|r| r.hop_decay_bps))),
            Arc::new(StringArray::from(records.iter().map(|r| Some(r.depth_at_1pct.as_str())).collect::<Vec<_>>())),
            Arc::new(StringArray::from(records.iter().map(|r| Some(r.depth_at_5pct.as_str())).collect::<Vec<_>>())),
            Arc::new(Float64Array::from(records.iter().map(|r| Some(r.spot_price)).collect::<Vec<_>>())),
            Arc::new(Float64Array::from(records.iter().map(|r| Some(r.token_price_in_native)).collect::<Vec<_>>())),
            Arc::new(Float64Array::from(records.iter().map(|r| Some(r.fee_tier)).collect::<Vec<_>>())),
            Arc::new(StringArray::from(records.iter().map(|r| Some(r.marginal_liquidity.as_str())).collect::<Vec<_>>())),
            Arc::new(Float64Array::from(records.iter().map(|r| Some(r.concentration_gini)).collect::<Vec<_>>())),
            Arc::new(StringArray::from_iter_values(records.iter().map(|r| r.route_total_amount_out.as_str()))),
            Arc::new(Float64Array::from_iter_values(records.iter().map(|r| r.route_decay_bps))),
        ],
    )?;

    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}
```

- [ ] **Step 2: Write a test for parquet round-trip**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn write_and_read_hop_decay_parquet() {
        let record = HopDecayRecord {
            quote_id: "q1".into(),
            solver_id: "s1".into(),
            request_id: "r1".into(),
            block_offset: 1,
            hop_index: 0,
            component_id: "0xabc".into(),
            protocol: "uniswap_v3".into(),
            hop_amount_out: "999000".into(),
            hop_decay_bps: 10.0,
            depth_at_1pct: "50000000".into(),
            depth_at_5pct: "200000000".into(),
            spot_price: 1800.5,
            token_price_in_native: 1.0,
            fee_tier: 0.003,
            marginal_liquidity: "1000000000".into(),
            concentration_gini: 0.45,
            route_total_amount_out: "999000".into(),
            route_decay_bps: 10.0,
        };

        let tmp = NamedTempFile::new().unwrap();
        write_hop_decay_parquet(tmp.path(), &[record]).unwrap();

        let file = File::open(tmp.path()).unwrap();
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReader::try_new(file, 1024).unwrap();
        let batches: Vec<_> = reader.collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].num_rows(), 1);
    }
}
```

- [ ] **Step 3: Run parquet test**

Run: `cargo test -p slippage-features -- parquet`
Expected: PASS

- [ ] **Step 4: Create tycho_resim.rs — the background task**

Create `tools/slippage-features/src/tycho_resim.rs`. This is the core resimulation loop:

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use fynd_core::feed::market_data::SharedMarketDataRef;
use fynd_core::observer::{QuoteProducedEvent, MAX_BLOCK_OFFSET};

use crate::decay::compute_decay_bps;
use crate::parquet_writer::{HopDecayRecord, write_hop_decay_parquet};

struct PendingQuote {
    event: QuoteProducedEvent,
    hop_records: Vec<HopDecayRecord>,
}

pub async fn run_tycho_resim(
    mut rx: mpsc::Receiver<QuoteProducedEvent>,
    market_data: SharedMarketDataRef,
    output_dir: PathBuf,
) {
    let mut pending: HashMap<String, PendingQuote> = HashMap::new();
    let mut last_block: u64 = 0;

    loop {
        tokio::select! {
            Some(event) = rx.recv() => {
                let key = format!("{}_{}", event.quote_id, event.solver_id);
                debug!(quote_id = %event.quote_id, solver_id = %event.solver_id, "received quote");
                pending.insert(key, PendingQuote {
                    event,
                    hop_records: Vec::new(),
                });
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                let current_block = {
                    let data = market_data.read().expect("market data lock poisoned");
                    data.last_updated()
                        .map(|bi| bi.number)
                        .unwrap_or(0)
                };

                if current_block <= last_block {
                    continue;
                }
                last_block = current_block;

                let mut completed_keys = Vec::new();

                for (key, pq) in pending.iter_mut() {
                    let offset = current_block.saturating_sub(pq.event.block_number);
                    if offset == 0 || offset > u64::from(MAX_BLOCK_OFFSET) {
                        if offset > u64::from(MAX_BLOCK_OFFSET) {
                            completed_keys.push(key.clone());
                        }
                        continue;
                    }

                    let data = market_data.read().expect("market data lock poisoned");

                    let mut route_total_out = num_bigint::BigUint::ZERO;

                    for (hop_idx, swap) in pq.event.route.swaps.iter().enumerate() {
                        let sim_state = match data.get_simulation_state(&swap.component_id) {
                            Some(s) => s,
                            None => {
                                warn!(component = %swap.component_id, "pool not found");
                                continue;
                            }
                        };

                        // Resimulate this hop
                        let hop_amount_in: num_bigint::BigUint = swap.amount_in
                            .parse()
                            .unwrap_or_default();

                        let get_amount_out_result = sim_state.get_amount_out(
                            hop_amount_in.clone(),
                            &swap.token_in,
                            &swap.token_out,
                        );

                        let (hop_amount_out_str, hop_decay) = match get_amount_out_result {
                            Ok(result) => {
                                let out_str = result.amount.to_string();
                                let decay = compute_decay_bps(
                                    &swap.amount_out,
                                    &out_str,
                                ).unwrap_or(f64::NAN);
                                route_total_out += &result.amount;
                                (out_str, decay)
                            }
                            Err(e) => {
                                warn!(component = %swap.component_id, error = %e, "resim failed");
                                (String::new(), f64::NAN)
                            }
                        };

                        // Read pool state features from derived data
                        // (depth, spot_price, etc. are in SharedMarketData)
                        let component = data.get_component(&swap.component_id);
                        let fee_tier = component
                            .and_then(|c| c.static_attributes().get("fee"))
                            .and_then(|v| v.parse::<f64>().ok())
                            .unwrap_or(f64::NAN);

                        pq.hop_records.push(HopDecayRecord {
                            quote_id: pq.event.quote_id.clone(),
                            solver_id: pq.event.solver_id.clone(),
                            request_id: pq.event.request_id.clone(),
                            block_offset: offset as u32,
                            hop_index: hop_idx as u32,
                            component_id: swap.component_id.clone(),
                            protocol: swap.protocol.clone(),
                            hop_amount_out: hop_amount_out_str,
                            hop_decay_bps: hop_decay,
                            depth_at_1pct: String::new(), // TODO: read from derived data
                            depth_at_5pct: String::new(), // TODO: read from derived data
                            spot_price: f64::NAN,         // TODO: read from derived data
                            token_price_in_native: f64::NAN, // TODO: read from derived data
                            fee_tier,
                            marginal_liquidity: String::new(), // TODO: read from derived data
                            concentration_gini: f64::NAN,      // TODO: read from derived data
                            route_total_amount_out: String::new(), // filled below
                            route_decay_bps: f64::NAN,             // filled below
                        });
                    }

                    // Compute route-level decay
                    let route_decay = compute_decay_bps(
                        &pq.event.amount_out,
                        &route_total_out.to_string(),
                    ).unwrap_or(f64::NAN);

                    // Backfill route totals on all records for this offset
                    let route_out_str = route_total_out.to_string();
                    for rec in pq.hop_records.iter_mut().rev() {
                        if rec.block_offset == offset as u32 {
                            rec.route_total_amount_out = route_out_str.clone();
                            rec.route_decay_bps = route_decay;
                        } else {
                            break;
                        }
                    }
                }

                // Flush completed quotes
                for key in completed_keys {
                    if let Some(pq) = pending.remove(&key) {
                        if pq.hop_records.is_empty() {
                            continue;
                        }
                        let path = output_dir.join(format!(
                            "hop_decay_{}_{}.parquet",
                            pq.event.quote_id, pq.event.solver_id
                        ));
                        if let Err(e) = write_hop_decay_parquet(&path, &pq.hop_records) {
                            warn!(error = %e, "failed to write hop decay parquet");
                        } else {
                            info!(path = %path.display(), records = pq.hop_records.len(), "flushed hop decay");
                        }
                    }
                }
            }
        }
    }
}
```

Note: The `TODO` fields for derived data (depth, spot_price, etc.) need to be wired to the actual derived data store in `SharedMarketData`. The exact accessor depends on how derived data is stored — investigate `fynd-core/src/derived/` to find the right API. For v1, leaving them as defaults is acceptable; wire them in a follow-up step once you understand the derived data access pattern.

The `ProtocolSim::get_amount_out` call uses the trait from `tycho_core::simulation::protocol_sim`. Check the exact method signature — it may take different argument types than shown above. Adapt accordingly.

- [ ] **Step 5: Update lib.rs**

```rust
pub mod decay;
pub mod parquet_writer;
pub mod tycho_resim;
```

- [ ] **Step 6: Verify compilation**

Run: `cargo check -p slippage-features`
Expected: compiles (may have warnings for unused imports — fix them)

- [ ] **Step 7: Commit**

```bash
git add tools/slippage-features/
git commit -m "feat: implement Tycho resim background task [ENG-5986]"
```

---

## Task 7: Implement quote log writer (observer → parquet)

**Files:**
- Create: `tools/slippage-features/src/quote_log.rs`
- Modify: `tools/slippage-features/src/lib.rs`

The observer implementation that receives events and writes the quote log parquet (consumed by the node resim process).

- [ ] **Step 1: Define the quote log parquet schema and writer**

Create `tools/slippage-features/src/quote_log.rs` following the same pattern as `parquet_writer.rs`:

- Schema fields: quote_id, solver_id, request_id, is_winner, block_number, chain_id, route (JSON-serialized), amount_in, amount_out, gas_estimate, calldata (hex), algorithm_type, algorithm_settings (JSON), n_alternatives, gap_to_second_best_bps, score_dispersion, slippage_tolerance
- A `QuoteLogRecord` struct matching these fields
- A `write_quote_log_parquet()` function
- A `QuoteLogObserver` struct implementing `SolverObserver` that buffers events and flushes periodically

- [ ] **Step 2: Implement QuoteLogObserver**

```rust
use std::sync::Mutex;
use fynd_core::observer::{SolverObserver, QuoteProducedEvent};

pub struct QuoteLogObserver {
    buffer: Mutex<Vec<QuoteLogRecord>>,
    output_dir: PathBuf,
    tx: mpsc::Sender<QuoteProducedEvent>,
}

impl SolverObserver for QuoteLogObserver {
    fn on_route_scored(&self, _route: &Route, _score: f64, _rank: usize) {
        // No-op for quote log — only on_quote_produced matters
    }

    fn on_quote_produced(&self, event: QuoteProducedEvent) {
        // Forward to the Tycho resim channel
        let _ = self.tx.try_send(event.clone());

        // Buffer for quote log parquet
        let record = QuoteLogRecord::from(&event);
        let mut buf = self.buffer.lock().expect("buffer lock poisoned");
        buf.push(record);

        // Flush every 100 records (configurable)
        if buf.len() >= 100 {
            let records: Vec<_> = buf.drain(..).collect();
            drop(buf);
            // Write in background to avoid blocking the solver
            let dir = self.output_dir.clone();
            tokio::spawn(async move {
                let path = dir.join(format!("quote_log_{}.parquet", chrono::Utc::now().timestamp()));
                if let Err(e) = write_quote_log_parquet(&path, &records) {
                    tracing::warn!(error = %e, "failed to write quote log");
                }
            });
        }
    }
}
```

- [ ] **Step 3: Test the observer**

Write a test that creates a `QuoteLogObserver`, sends a few events, triggers a flush, and verifies the parquet file is created and readable.

- [ ] **Step 4: Run tests**

Run: `cargo test -p slippage-features -- quote_log`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add tools/slippage-features/
git commit -m "feat: implement QuoteLogObserver with parquet output [ENG-5986]"
```

---

## Task 8: Implement node resim binary

**Files:**
- Create: `tools/slippage-features/src/bin/node_resim.rs`
- Modify: `tools/slippage-features/Cargo.toml` (add alloy dep, [[bin]] section)

- [ ] **Step 1: Add alloy and clap deps**

In `tools/slippage-features/Cargo.toml`:

```toml
alloy = { workspace = true }
clap = { version = "4", features = ["derive"] }
chrono = "0.4"

[[bin]]
name = "node-resim"
path = "src/bin/node_resim.rs"
```

- [ ] **Step 2: Create the binary**

Create `tools/slippage-features/src/bin/node_resim.rs`:

```rust
use std::path::PathBuf;
use clap::Parser;

#[derive(Parser)]
#[command(name = "node-resim", about = "Ground-truth resimulation via eth_call")]
struct Args {
    /// Path to quote log parquet directory
    #[arg(long)]
    quote_log_dir: PathBuf,

    /// Output directory for route-level decay parquet
    #[arg(long)]
    output_dir: PathBuf,

    /// RPC URL (archive node)
    #[arg(long, env = "ETH_RPC_URL")]
    rpc_url: String,

    /// Max block offset (default: 10)
    #[arg(long, default_value_t = 10)]
    max_block_offset: u32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    tracing_subscriber::fmt::init();

    // 1. Read quote log parquet files from quote_log_dir
    // 2. For each quote: extract calldata
    // 3. For blocks X+1..X+max_block_offset:
    //    - Build eth_call with 100% slippage tolerance
    //    - Apply storage overrides (balance + approval injection)
    //    - Call against archive node
    //    - Record amount_out, gas_used, success/revert
    // 4. Write route-level decay parquet

    // Implementation follows the pattern from fynd-swap-cli's dry-run:
    // - Storage overrides: tools/fynd-swap-cli/src/erc20.rs:50
    // - eth_call: tools/fynd-swap-cli/src/erc20.rs:68-75

    todo!("Wire up parquet reading, eth_call loop, and output writing")
}
```

This is a skeleton that documents the exact flow. The full implementation requires:
1. Reading parquet with `parquet::arrow::arrow_reader::ParquetRecordBatchReader`
2. Building `alloy::rpc::types::TransactionRequest` from calldata
3. Applying `StateOverride` using helpers from `fynd-swap-cli/src/erc20.rs`
4. Calling `provider.call(tx).overrides(overrides).block(BlockId::Number(block_number))`
5. Decoding the return value to extract amount_out
6. Writing results to parquet using the route-level decay schema

The exact implementation depends on:
- How calldata is structured (which router contract, ABI encoding)
- How amount_out is decoded from the return bytes
- Whether the provider supports `block` parameter on `eth_call`

Read `tools/fynd-swap-cli/src/main.rs` for the end-to-end pattern.

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p slippage-features --bin node-resim`
Expected: compiles (the `todo!()` is fine — it compiles but panics at runtime)

- [ ] **Step 4: Commit**

```bash
git add tools/slippage-features/
git commit -m "feat: scaffold node resim binary [ENG-5986]"
```

---

## Task 9: Implement feature assembly binary

**Files:**
- Create: `tools/slippage-features/src/bin/assemble.rs`

- [ ] **Step 1: Create the assembly binary**

Create `tools/slippage-features/src/bin/assemble.rs`:

```rust
use std::path::PathBuf;
use clap::Parser;

#[derive(Parser)]
#[command(name = "assemble", about = "Join decay parquets + features into unified dataset")]
struct Args {
    /// Path to quote log parquet directory
    #[arg(long)]
    quote_log_dir: PathBuf,

    /// Path to hop decay parquet directory (from Tycho resim)
    #[arg(long)]
    hop_decay_dir: PathBuf,

    /// Path to route decay parquet directory (from node resim)
    #[arg(long)]
    route_decay_dir: PathBuf,

    /// Output directory for unified dataset
    #[arg(long)]
    output_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    tracing_subscriber::fmt::init();

    // 1. Read all three parquet sources
    // 2. Join on quote_id + solver_id
    // 3. Add computed features:
    //    - Token/pair from CoinGecko (cached)
    //    - Route topology (hop_count, split_count, gas, pool_type_mix)
    //    - Chain/env (chain_id, base_fee, is_l2)
    //    - Temporal (hour, day, minutes_since_hour)
    // 4. Validate PIT integrity
    // 5. Write unified parquet, partitioned by chain_id

    todo!("Implement join + feature computation + validation")
}
```

Add to `Cargo.toml`:

```toml
[[bin]]
name = "assemble"
path = "src/bin/assemble.rs"
```

- [ ] **Step 2: Verify compilation**

Run: `cargo check -p slippage-features --bin assemble`
Expected: compiles

- [ ] **Step 3: Commit**

```bash
git add tools/slippage-features/
git commit -m "feat: scaffold feature assembly binary [ENG-5986]"
```

---

## Task 10: Wire everything together — Fynd startup integration

**Files:**
- Modify: the Fynd binary crate's main/startup code (find with `rg "fn main" fynd-rpc/` or similar)
- Modify: `fynd-rpc/Cargo.toml` (add slippage-features dep behind feature flag)

- [ ] **Step 1: Find the Fynd startup code**

Locate where `Solver` and `WorkerPoolRouter` are constructed. This is where we:
1. Create the `tokio::mpsc` channel
2. Create the `QuoteLogObserver`
3. Pass it into the solver as the observer
4. Spawn the `run_tycho_resim` task

- [ ] **Step 2: Add conditional startup behind feature flag**

```rust
#[cfg(feature = "slippage-features")]
{
    let (tx, rx) = tokio::sync::mpsc::channel(1000);
    let observer = Arc::new(slippage_features::quote_log::QuoteLogObserver::new(
        output_dir.clone(),
        tx,
    ));

    // Pass observer to solver construction
    // ...

    // Spawn resim task
    let market_data = shared_market_data.clone();
    tokio::spawn(slippage_features::tycho_resim::run_tycho_resim(
        rx,
        market_data,
        output_dir.join("hop_decay"),
    ));
}
```

- [ ] **Step 3: Verify compilation with and without feature**

Run: `cargo check --features slippage-features`
Run: `cargo check`
Expected: both compile

- [ ] **Step 4: Commit**

```bash
git add .
git commit -m "feat: wire slippage-features into Fynd startup [ENG-5986]"
```

---

## Summary

| Task | Component | JIRA |
|------|-----------|------|
| Pre-req | Clean up old scaffold | — |
| 1 | Feature flag + workspace deps | ENG-5986 |
| 2 | 5% pool depth threshold | ENG-5988 |
| 3 | SolverObserver trait + types | ENG-5987 |
| 4 | Wire observer into solver | ENG-5987 |
| 5 | Decay math crate | ENG-5986 |
| 6 | Tycho resim task | NEW |
| 7 | Quote log observer + parquet | ENG-5987 |
| 8 | Node resim binary | NEW |
| 9 | Feature assembly binary | ENG-5992 |
| 10 | Fynd startup integration | ENG-5986 |

Tasks 1-5 can be done in sequence quickly. Tasks 6-7 are the core implementation. Tasks 8-9 are scaffolds that need the full eth_call and CoinGecko wiring. Task 10 ties everything together.

//! Times each derived computation on a replayed market, so their speed can be measured and changed
//! without a live Tycho stream.
//!
//! [`time_derived_computations`] replays the first update, which is the market snapshot, and runs
//! every computation as a full recompute. Then it replays each later update and runs the
//! computations incrementally on the components that update changed. The market and the store
//! carry over between blocks, as they do under `ComputationManager`. Computations run one at a
//! time, pool depths after the spot prices they read, so each time is for that computation alone.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use num_bigint::BigUint;
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::{self, error::TryRecvError};
use tycho_simulation::{
    protocol::models::Update,
    tycho_common::models::Chain,
    tycho_ethereum::gas::{BlockGasPrice, GasPrice},
};

use crate::{
    derived::{
        computation::DerivedComputation,
        computations::{ComponentDepthComputation, SpotPriceComputation, TokenGasPriceComputation},
        manager::{ChangedComponents, ComputationManagerConfig, SharedDerivedDataRef},
        store::DerivedData,
    },
    feed::{events::MarketEvent, market_data::MarketData, tycho_feed::TychoFeed, TychoFeedConfig},
    types::constants::native_token,
};

/// Gas price when the recording holds none, the same value `Solver::from_recording` uses.
const DEFAULT_GAS_PRICE_WEI: u64 = 10_000_000_000;

/// The market a timing run replays, and the settings that production takes from its config.
pub struct DerivedBenchSettings {
    /// The chain the updates come from; it selects the gas token.
    pub chain: Chain,
    /// The hop limit for `token_prices`, what production sets with `--pricing-max-hops`.
    pub pricing_max_hops: usize,
    /// The recorded gas price, or `None` for 10 gwei.
    pub gas_price_wei: Option<BigUint>,
}

/// What one computation cost on one block.
#[derive(Debug, Clone, Copy)]
pub struct ComputationRun {
    /// Wall-clock time of `compute`.
    pub elapsed: Duration,
}

/// The computations to time, built as `ComputationManager::new` builds them.
struct TimedComputations {
    spot_prices: SpotPriceComputation,
    token_prices: TokenGasPriceComputation,
    pool_depths: ComponentDepthComputation,
}

/// What every computation on one block reads, and where it stores its output.
struct BlockRun<'a> {
    market: &'a MarketData,
    store: &'a SharedDerivedDataRef,
    changed: &'a ChangedComponents,
    block: u64,
}

impl TimedComputations {
    fn build(settings: &DerivedBenchSettings) -> Self {
        let gas_token =
            native_token(&settings.chain).expect("the recording's chain has a native token");
        // A 24-hour budget acts as no deadline: a pass cut short would report less time than the
        // full pass takes. No token cap and no pass interval, so each block prices every token
        // whose dependencies changed. The timings then measure pricing, not the throttle.
        let config = ComputationManagerConfig::new()
            .with_gas_token(gas_token)
            .with_max_hop(settings.pricing_max_hops)
            .with_pricing_pass_budget(Duration::from_secs(24 * 60 * 60))
            .with_pricing_max_tokens_per_pass(usize::MAX)
            .with_pricing_min_pass_interval(Duration::ZERO);
        Self {
            spot_prices: SpotPriceComputation::new(),
            token_prices: config.build_token_price_computation(),
            pool_depths: ComponentDepthComputation::new(config.depth_slippage_threshold())
                .expect("the default depth slippage threshold is valid"),
        }
    }

    /// Runs every computation on one block, pool depths after the spot prices they read, and
    /// returns what each cost.
    async fn run_block(&self, run: &BlockRun<'_>) -> [(&'static str, ComputationRun); 3] {
        [
            (
                SpotPriceComputation::ID,
                time_computation(&self.spot_prices, run, |out| out.len()).await,
            ),
            (
                TokenGasPriceComputation::ID,
                time_computation(&self.token_prices, run, |out| out.len()).await,
            ),
            (
                ComponentDepthComputation::ID,
                time_computation(&self.pool_depths, run, |out| out.len()).await,
            ),
        ]
    }
}

/// What each computation cost on one replay: on the first block, and on each later block.
#[derive(Debug, Default)]
pub struct DerivedBenchRuns {
    /// Computation id → its cost on the first block, where every component is new.
    pub first_block: BTreeMap<&'static str, ComputationRun>,
    /// Computation id → its cost on each later block, in replay order.
    pub later_blocks: BTreeMap<&'static str, Vec<ComputationRun>>,
}

/// The token prices and pool depths the store holds at one point of a replay, for comparing two
/// versions of the computations. A value too large for an `f64` is left out.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DerivedValues {
    /// Token address → price, in token units per gas token unit.
    pub token_prices: BTreeMap<String, f64>,
    /// `component_id/token_in/token_out` → pool depth, in `token_in`'s smallest unit.
    pub pool_depths: BTreeMap<String, f64>,
}

/// The token prices and pool depths of one replay, after its first and its last block.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ReplayValues {
    /// The values after the first block, the market snapshot.
    pub after_first_block: DerivedValues,
    /// The values after the last block.
    pub after_last_block: DerivedValues,
}

/// The computation times of one replay, and its token prices and pool depths.
#[derive(Debug, Default)]
pub struct DerivedBenchReport {
    /// The cost of each computation on the first block and on each later block.
    pub runs: DerivedBenchRuns,
    /// The token prices and pool depths after the first and the last block.
    pub values: ReplayValues,
}

/// Reads the token prices and pool depths the store holds now.
async fn read_derived_values(store: &SharedDerivedDataRef) -> DerivedValues {
    let guard = store.read().await;
    let mut values = DerivedValues::default();
    for (token, price) in guard
        .token_prices()
        .into_iter()
        .flatten()
    {
        let ratio = to_f64(&price.numerator) / to_f64(&price.denominator);
        if ratio.is_finite() {
            values
                .token_prices
                .insert(token.to_string(), ratio);
        }
    }
    for ((component_id, token_in, token_out), depth) in guard
        .component_depths()
        .into_iter()
        .flatten()
    {
        let depth = to_f64(depth);
        if depth.is_finite() {
            values
                .pool_depths
                .insert(format!("{component_id}/{token_in}/{token_out}"), depth);
        }
    }
    values
}

/// `value` as an `f64`. A value past `f64::MAX` gives infinity.
fn to_f64(value: &BigUint) -> f64 {
    value.to_f64().unwrap_or(f64::INFINITY)
}

/// Runs one computation, prints its time and output size, and persists the output so the next
/// block's incremental run reads it.
async fn time_computation<C: DerivedComputation>(
    computation: &C,
    run: &BlockRun<'_>,
    output_len: impl Fn(&C::Output) -> usize,
) -> ComputationRun {
    let block = run.block;
    let start = Instant::now();
    let output = computation
        .compute(run.market, run.store, run.changed)
        .await
        .unwrap_or_else(|error| panic!("{} failed on block {block}: {error}", C::ID));
    let elapsed = start.elapsed();
    let items = output_len(&output.data);
    let failed = output.failed_items.len();
    C::persist(&mut *run.store.write().await, output, block, run.changed.is_full_recompute);
    println!(
        "block {block} {:<17} {:>10.1} ms  items={items} failed={failed}",
        C::ID,
        elapsed.as_secs_f64() * 1000.0,
    );
    ComputationRun { elapsed }
}

/// Returns every market event the feed broadcast since the last call.
fn drain_events(events: &mut broadcast::Receiver<MarketEvent>) -> Vec<MarketEvent> {
    let mut drained = Vec::new();
    loop {
        match events.try_recv() {
            Ok(event) => drained.push(event),
            Err(TryRecvError::Empty) => return drained,
            Err(TryRecvError::Lagged(skipped)) => {
                panic!("the bench dropped {skipped} market events; drain after every update")
            }
            Err(TryRecvError::Closed) => panic!("the feed closed its event channel"),
        }
    }
}

async fn set_gas_price(market: &MarketData, gas_price_wei: Option<BigUint>) {
    let gas_price = gas_price_wei.unwrap_or_else(|| BigUint::from(DEFAULT_GAS_PRICE_WEI));
    let block_number = read_current_block(market).await;
    market
        .write()
        .await
        .update_gas_price(BlockGasPrice {
            block_number,
            block_hash: Default::default(),
            block_timestamp: 0,
            pricing: GasPrice::Legacy { gas_price },
        });
}

async fn read_current_block(market: &MarketData) -> u64 {
    market
        .read()
        .await
        .last_updated()
        .map_or(0, |block| block.number())
}

/// Replays `updates` into a new market and store, and prints the time and output size of every
/// derived computation on each block. Returns the times, and the token prices and pool depths
/// after the first and the last block.
///
/// # Panics
///
/// If `updates` is empty, an update does not replay, or a computation fails. A timing run cannot
/// go on after any of them.
pub async fn time_derived_computations(
    settings: &DerivedBenchSettings,
    updates: Vec<Update>,
) -> DerivedBenchReport {
    let mut report = DerivedBenchReport::default();
    let computations = TimedComputations::build(settings);
    let market = MarketData::new_shared();
    let feed = TychoFeed::new(
        TychoFeedConfig::new("ws://replay".to_string(), settings.chain, None, false, vec![], 0.0),
        market.clone(),
    );
    let mut events = feed.subscribe();
    let store = DerivedData::new_shared();
    let mut updates = updates.into_iter();

    let snapshot = updates
        .next()
        .expect("the recording holds at least one update");
    let start = Instant::now();
    feed.handle_tycho_message(snapshot)
        .await
        .expect("the snapshot replays");
    set_gas_price(&market, settings.gas_price_wei.clone()).await;
    drain_events(&mut events);
    let components = market
        .read()
        .await
        .component_topology()
        .len();
    println!(
        "replayed the snapshot in {:.1} s: {components} components",
        start.elapsed().as_secs_f64()
    );

    let full_recompute = ChangedComponents { is_full_recompute: true, ..Default::default() };
    let block = read_current_block(&market).await;
    let times = computations
        .run_block(&BlockRun { market: &market, store: &store, changed: &full_recompute, block })
        .await;
    report.runs.first_block = times.into_iter().collect();
    report.values.after_first_block = read_derived_values(&store).await;

    for update in updates {
        feed.handle_tycho_message(update)
            .await
            .expect("the update replays");
        for event in drain_events(&mut events) {
            let MarketEvent::MarketUpdated {
                added_components,
                removed_components,
                updated_components,
            } = event;
            let changed = ChangedComponents {
                added: added_components,
                removed: removed_components,
                updated: updated_components,
                is_full_recompute: false,
            };
            if changed.added.is_empty() && changed.removed.is_empty() && changed.updated.is_empty()
            {
                continue;
            }
            let block = read_current_block(&market).await;
            println!(
                "block {block}: added={} removed={} updated={}",
                changed.added.len(),
                changed.removed.len(),
                changed.updated.len()
            );
            let times = computations
                .run_block(&BlockRun { market: &market, store: &store, changed: &changed, block })
                .await;
            for (id, computation_run) in times {
                report
                    .runs
                    .later_blocks
                    .entry(id)
                    .or_default()
                    .push(computation_run);
            }
        }
    }

    report.values.after_last_block = read_derived_values(&store).await;
    report
}

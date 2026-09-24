//! Times each derived computation on a replayed market, so their speed can be measured and changed
//! without a live Tycho stream.
//!
//! [`time_derived_computations`] replays the first update, which is the market snapshot, and runs
//! every computation as a full recompute. Then it replays each later update and runs the
//! computations incrementally on the components that update changed. The market and the store
//! carry over between blocks, as they do under `ComputationManager`. Computations run one at a
//! time, pool depths after the spot prices they read, so each time is for that computation alone.
//!
//! The `derived` bench target in `fynd-bench-harness` reads a recording and calls it;
//! `./scripts/derived-bench.sh` runs that target under samply.

use std::time::{Duration, Instant};

use num_bigint::BigUint;
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
    /// The hop limit for `token_prices`, the largest `max_hops` of any production pool.
    pub pricing_max_hops: usize,
    /// The recorded gas price, or `None` for 10 gwei.
    pub gas_price_wei: Option<BigUint>,
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
        // full pass takes.
        let config = ComputationManagerConfig::new()
            .with_gas_token(gas_token)
            .with_max_hop(settings.pricing_max_hops)
            .with_pricing_pass_budget(Duration::from_secs(24 * 60 * 60));
        Self {
            spot_prices: SpotPriceComputation::new(),
            token_prices: config.build_token_price_computation(),
            pool_depths: config
                .build_pool_depth_computation()
                .expect("the default depth slippage threshold is valid"),
        }
    }

    /// Runs every computation on one block, pool depths after the spot prices they read.
    async fn run_block(&self, run: &BlockRun<'_>) {
        time_computation(&self.spot_prices, run, |out| out.len()).await;
        time_computation(&self.token_prices, run, |out| out.len()).await;
        time_computation(&self.pool_depths, run, |out| out.len()).await;
    }
}

/// Runs one computation, prints its time and output size, and persists the output so the next
/// block's incremental run reads it.
async fn time_computation<C: DerivedComputation>(
    computation: &C,
    run: &BlockRun<'_>,
    output_len: impl Fn(&C::Output) -> usize,
) {
    let block = run.block;
    let start = Instant::now();
    let output = computation
        .compute(run.market, run.store, run.changed)
        .await
        .unwrap_or_else(|error| panic!("{} failed on block {block}: {error}", C::ID));
    let elapsed = start.elapsed();
    println!(
        "block {block} {:<17} {:>10.1} ms  items={} failed={}",
        C::ID,
        elapsed.as_secs_f64() * 1000.0,
        output_len(&output.data),
        output.failed_items.len(),
    );
    C::persist(&mut *run.store.write().await, output, block, run.changed.is_full_recompute);
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
/// derived computation on each block.
///
/// # Panics
///
/// If `updates` is empty, an update does not replay, or a computation fails. A timing run cannot
/// go on after any of them.
pub async fn time_derived_computations(settings: &DerivedBenchSettings, updates: Vec<Update>) {
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
    computations
        .run_block(&BlockRun { market: &market, store: &store, changed: &full_recompute, block })
        .await;

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
            if changed.all_changed_ids().is_empty() {
                continue;
            }
            let block = read_current_block(&market).await;
            println!(
                "block {block}: added={} removed={} updated={}",
                changed.added.len(),
                changed.removed.len(),
                changed.updated.len()
            );
            computations
                .run_block(&BlockRun { market: &market, store: &store, changed: &changed, block })
                .await;
        }
    }
}

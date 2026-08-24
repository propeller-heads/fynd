//! Runs the two APEX experiments per block — S1 (one order per solve, the control) and S2 (the
//! whole block as one batch, the treatment) — and turns clearings into per-order records.

use std::{
    collections::{BTreeMap, HashMap},
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    time::{Duration, Instant},
};

use apex_solver::{
    core::{ApexConfig, ApexResult, LimitOrderClearing},
    serialization::ApexInputData,
    types::U256 as ApexU256,
};
use serde::Serialize;
use tracing::warn;

use super::{
    alloy_u256, dec, orders_by_pair,
    snapshot::{scale_down_ceil, scale_down_floor, Snapshot},
    BlockRecord, InclusionStatus, OrderRecord, PoolVolumeRecord, PreparedOrder, SolveBudget,
    Variant,
};
use crate::decoder::DecodedTrade;

/// Run S1 and S2 for both limit-price variants on the block's snapshot, appending one record
/// per order per run per variant to `records`. Returns one block record per variant.
pub(crate) fn run_block(
    block: u64,
    trades: &[DecodedTrade],
    snapshot: &Snapshot,
    budget: SolveBudget,
    inputs_dir: &Path,
    results_dir: &Path,
    records: &mut Vec<OrderRecord>,
) -> anyhow::Result<Vec<BlockRecord>> {
    dump_input(block, snapshot, inputs_dir);
    let mut block_records = Vec::with_capacity(2);
    for variant in [Variant::Permissive, Variant::Anchored, Variant::UserLimit] {
        block_records.push(run_variant(
            block,
            variant,
            trades,
            snapshot,
            budget,
            results_dir,
            records,
        )?);
    }
    Ok(block_records)
}

fn run_variant(
    block: u64,
    variant: Variant,
    trades: &[DecodedTrade],
    snapshot: &Snapshot,
    budget: SolveBudget,
    results_dir: &Path,
    records: &mut Vec<OrderRecord>,
) -> anyhow::Result<BlockRecord> {
    // S2: the whole block as one batch.
    let s2_started = Instant::now();
    let s2_result =
        solve(snapshot, &snapshot.prepared, variant, budget.s2_deadline_ms, budget.max_iterations);
    let s2_solve_ms = u64::try_from(s2_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let Some(s2_outcome) = s2_result else {
        anyhow::bail!("S2 batch solve ({}) failed; block skipped", variant.as_str());
    };
    let s2_result = s2_outcome.result;
    let s2_winning_config = s2_outcome.winning_config;
    let s2_clearings = clearings_by_id(&s2_result);
    for prepared in &snapshot.prepared {
        records.push(order_record(
            block,
            "s2",
            variant,
            trades,
            snapshot,
            prepared,
            s2_clearings
                .get(prepared.permissive.id.as_str())
                .copied(),
        ));
    }

    // S1: each order alone against the same snapshot — same solver, same pools, no batching.
    let s1_started = Instant::now();
    let mut s1_deadline_fired = 0usize;
    let mut s1_results = BTreeMap::new();
    for prepared in &snapshot.prepared {
        let single = std::slice::from_ref(prepared);
        let result = solve(snapshot, single, variant, budget.s1_deadline_ms, budget.max_iterations);
        let clearing = result.as_ref().and_then(|outcome| {
            if outcome.result.deadline_fired {
                s1_deadline_fired += 1;
            }
            clearings_by_id(&outcome.result)
                .get(prepared.permissive.id.as_str())
                .map(|clearing| (*clearing).clone())
        });
        if let Some(outcome) = result.as_ref() {
            s1_results.insert(
                prepared.permissive.id.clone(),
                SolveResultJson::from_result(&outcome.result, outcome.winning_config),
            );
        }
        records.push(order_record(
            block,
            "s1",
            variant,
            trades,
            snapshot,
            prepared,
            clearing.as_ref(),
        ));
    }
    let s1_solve_ms_total = u64::try_from(s1_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    dump_results(block, variant, &s2_result, s2_winning_config, s1_results, results_dir);

    // Out-of-universe trades get a record per run so the analysis can count them at S0.
    for &trade_ix in &snapshot.out_of_universe {
        for run in ["s1", "s2"] {
            records.push(out_of_universe_record(block, run, variant, &trades[trade_ix], snapshot));
        }
    }

    Ok(BlockRecord {
        block,
        variant: variant.as_str(),
        trades_decoded: trades.len(),
        sandwiched_excluded: snapshot.excluded_sandwiched,
        out_of_universe: snapshot.out_of_universe.len(),
        orders_in: snapshot.prepared.len(),
        universe_tokens: snapshot.apex_tokens.len(),
        pools_native_v2: snapshot.pool_counts.native_v2,
        pools_native_v3: snapshot.pool_counts.native_v3,
        pools_wrapped: snapshot.pool_counts.wrapped,
        pools_skipped: snapshot.pool_counts.skipped,
        s2_solve_ms,
        s2_deadline_fired: s2_result.deadline_fired,
        s2_winning_config: s2_winning_config.to_string(),
        s1_solve_ms_total,
        s1_deadline_fired,
        s2_pool_volumes: pool_volumes(&s2_result, snapshot),
    })
}

/// One APEX run configuration in the panel — Turbine's production mechanism: several
/// differently-seeded searches race in parallel and the best clearing wins. `price_factor`
/// rescales the initial price vector (negative divides, positive multiplies), seeding the
/// tâtonnement from a different starting point; `mixed_strategy` adds the Top(2)/Top(1) step
/// strategies that help the search escape local minima.
struct RunConfig {
    label: &'static str,
    price_factor: i32,
    enable_two_hops: bool,
    mixed_strategy: bool,
}

/// Turbine's production panel (`turbine_config_prod.toml [solver].run_configs`).
#[rustfmt::skip]
const RUN_CONFIGS: [RunConfig; 7] = [
    RunConfig { label: "/1000 two-hops mixed", price_factor: -1000, enable_two_hops: true, mixed_strategy: true },
    RunConfig { label: "/100 mixed", price_factor: -100, enable_two_hops: false, mixed_strategy: true },
    RunConfig { label: "/100", price_factor: -100, enable_two_hops: false, mixed_strategy: false },
    RunConfig { label: "/1000 two-hops", price_factor: -1000, enable_two_hops: true, mixed_strategy: false },
    RunConfig { label: "/10 two-hops mixed", price_factor: -10, enable_two_hops: true, mixed_strategy: true },
    RunConfig { label: "x1", price_factor: 1, enable_two_hops: false, mixed_strategy: false },
    RunConfig { label: "x2 two-hops", price_factor: 2, enable_two_hops: true, mixed_strategy: false },
];

impl RunConfig {
    /// Turbine's `build_apex_config`: 3000 iterations (unless overridden), and the mixed
    /// strategy shortens the min-step patience so the Top strategies get their turn.
    fn apex_config(&self, deadline: Instant, max_iterations: Option<u32>) -> ApexConfig {
        let mut config = ApexConfig {
            enable_two_hops: self.enable_two_hops,
            deadline: Some(deadline),
            // Seven panel configs run concurrently; two supply-query workers each keeps the
            // total thread count within the machine instead of oversubscribing.
            max_workers: 2,
            ..Default::default()
        };
        config
            .price_search_config
            .max_iterations = max_iterations.unwrap_or(3000);
        if self.mixed_strategy {
            config
                .price_search_config
                .max_it_at_min_step = 10;
            config
                .price_search_config
                .iteration_strategies = vec![
                apex_solver::core::StepStrategy::AllTokens,
                apex_solver::core::StepStrategy::Top(2),
                apex_solver::core::StepStrategy::Top(1),
            ];
        }
        config
    }

    /// Turbine's `scale_prices`: negative factors divide, positive multiply.
    fn scale_prices(
        &self,
        prices: &HashMap<apex_solver::types::Address, ApexU256>,
    ) -> HashMap<apex_solver::types::Address, ApexU256> {
        let mut scaled = HashMap::with_capacity(prices.len());
        for (token, price) in prices {
            let value = match self.price_factor {
                f if f < 0 => price / ApexU256::from(f.unsigned_abs()),
                0 | 1 => *price,
                f => price.saturating_mul(ApexU256::from(f.unsigned_abs())),
            };
            scaled.insert(*token, value.max(ApexU256::from(1u64)));
        }
        scaled
    }
}

/// The winner of one solve: the result plus which run config produced it.
struct SolveOutcome {
    result: ApexResult,
    winning_config: &'static str,
}

/// One APEX solve over `orders`: the full run-config panel races in parallel under one shared
/// absolute deadline (Turbine's mechanism), and the result clearing the most ETH-valued output
/// wins (tie-break: clearings count). `None` means every config errored or panicked.
fn solve(
    snapshot: &Snapshot,
    orders: &[PreparedOrder],
    variant: Variant,
    deadline_ms: u64,
    max_iterations: Option<u32>,
) -> Option<SolveOutcome> {
    let deadline = Instant::now() + Duration::from_millis(deadline_ms);
    let limit_orders = orders_by_pair(orders, variant);
    let results: Vec<Option<(usize, ApexResult)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = RUN_CONFIGS
            .iter()
            .enumerate()
            .map(|(ix, run_config)| {
                let limit_orders = limit_orders.clone();
                scope.spawn(move || {
                    let outcome = catch_unwind(AssertUnwindSafe(|| {
                        apex_solver::run_apex_with_config(
                            snapshot.apex_tokens.clone(),
                            run_config.scale_prices(&snapshot.initial_prices),
                            limit_orders,
                            HashMap::new(),
                            snapshot.pools.clone(),
                            run_config.apex_config(deadline, max_iterations),
                        )
                    }));
                    match outcome {
                        Ok(Ok(result)) => Some((ix, result)),
                        Ok(Err(error)) => {
                            warn!(%error, config = run_config.label, "apex run failed");
                            None
                        }
                        Err(_) => {
                            warn!(config = run_config.label, "apex run panicked");
                            None
                        }
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or(None))
            .collect()
    });

    let mut best: Option<(usize, ApexResult)> = None;
    let mut best_volume = 0.0f64;
    let mut best_count = 0usize;
    for entry in results.into_iter().flatten() {
        let volume = cleared_volume_eth(&entry.1, snapshot);
        let count = entry.1.limit_order_clearings.len();
        // The volumes are sums of the same float terms, so exact ties happen only when the
        // clearings are value-identical — the count then breaks the tie.
        let better =
            best.is_none() || volume > best_volume || (volume >= best_volume && count > best_count);
        if better {
            best_volume = volume;
            best_count = count;
            best = Some(entry);
        }
    }
    best.map(|(ix, result)| SolveOutcome { result, winning_config: RUN_CONFIGS[ix].label })
}

/// ETH value of everything a result's limit-order clearings bought — the panel's winner metric
/// (Turbine uses USDC volume; ETH is our valuation unit).
fn cleared_volume_eth(result: &ApexResult, snapshot: &Snapshot) -> f64 {
    let mut total = 0.0;
    for clearing in &result.limit_order_clearings {
        let meta = snapshot.meta(alloy_addr(clearing.buy_token));
        let raw = scale_down_floor(clearing.bought_amount, meta.decimals);
        total += eth_value(raw, meta.eth_per_atomic);
    }
    total
}

fn clearings_by_id(result: &ApexResult) -> HashMap<&str, &LimitOrderClearing> {
    let mut map = HashMap::new();
    for clearing in &result.limit_order_clearings {
        map.insert(clearing.id.as_str(), clearing);
    }
    map
}

/// Build one order's record from its (possible) clearing. Amounts scale down to raw native
/// units: sold ceils (what the user sends is never understated), bought floors (what the user
/// receives is never overstated).
fn order_record(
    block: u64,
    run: &'static str,
    variant: Variant,
    trades: &[DecodedTrade],
    snapshot: &Snapshot,
    prepared: &PreparedOrder,
    clearing: Option<&LimitOrderClearing>,
) -> OrderRecord {
    let trade = &trades[prepared.trade_ix];
    let sell_meta = snapshot.meta(prepared.sell_token);
    let buy_meta = snapshot.meta(prepared.buy_token);

    let (status, sold_scaled, bought_scaled) = match clearing {
        None => (InclusionStatus::Unfilled, ApexU256::ZERO, ApexU256::ZERO),
        Some(clearing) => {
            let status = if clearing.sold_amount >= prepared.scaled_sell {
                InclusionStatus::Cleared
            } else if clearing.sold_amount.is_zero() {
                InclusionStatus::Unfilled
            } else {
                InclusionStatus::Partial
            };
            (status, clearing.sold_amount, clearing.bought_amount)
        }
    };
    let apex_sold = scale_down_ceil(sold_scaled, sell_meta.decimals);
    let apex_bought = scale_down_floor(bought_scaled, buy_meta.decimals);
    let apex_bought_eth = eth_value(apex_bought, buy_meta.eth_per_atomic);
    // On a partial fill the batcher acts as the missing liquidity source completing the order:
    // the user executes their full size at the clearing price, the batcher receives the
    // remaining sell amount and supplies the buy-token remainder (full clearing-price output
    // minus what APEX cleared). The user is never partially executed (originals are
    // fill-or-kill).
    let (batcher_sold, batcher_bought) = match status {
        InclusionStatus::Partial => {
            let (supply_scaled, receive_scaled) =
                top_up(bought_scaled, sold_scaled, prepared.scaled_sell);
            (
                scale_down_ceil(supply_scaled, buy_meta.decimals),
                scale_down_floor(receive_scaled, sell_meta.decimals),
            )
        }
        InclusionStatus::Cleared | InclusionStatus::OutOfUniverse | InclusionStatus::Unfilled => {
            (ApexU256::ZERO, ApexU256::ZERO)
        }
    };

    OrderRecord {
        schema: 2,
        block,
        run,
        variant: variant.as_str(),
        tx_hash: format!("{:#x}", trade.tx_hash),
        tx_index: trade.tx_index,
        order_id: prepared.permissive.id.clone(),
        sender: format!("{:#x}", trade.sender),
        venue: trade.venue.clone(),
        solver: trade.solver.clone(),
        sell_token: format!("{:#x}", prepared.sell_token),
        buy_token: format!("{:#x}", prepared.buy_token),
        sell_symbol: sell_meta.symbol,
        buy_symbol: buy_meta.symbol,
        sell_decimals: sell_meta.decimals,
        buy_decimals: buy_meta.decimals,
        amount_in: dec(trade.amount_in),
        settled_amount_out: dec(trade.amount_out),
        amount_in_eth: eth_value(super::apex_u256(trade.amount_in), sell_meta.eth_per_atomic),
        settled_amount_out_eth: eth_value(
            super::apex_u256(trade.amount_out),
            buy_meta.eth_per_atomic,
        ),
        status,
        apex_sold: dec(alloy_u256(apex_sold)),
        apex_bought: dec(alloy_u256(apex_bought)),
        batcher_sold: dec(alloy_u256(batcher_sold)),
        batcher_bought: dec(alloy_u256(batcher_bought)),
        limit_source: if variant == Variant::UserLimit { prepared.limit_source } else { "" },
        apex_bought_eth,
        batcher_sold_eth: eth_value(batcher_sold, buy_meta.eth_per_atomic),
        batcher_bought_eth: eth_value(batcher_bought, sell_meta.eth_per_atomic),
    }
}

/// ETH value of a raw amount at the token's derived price. Amounts far beyond f64 precision are
/// junk-token dust; approximate is fine for aggregation.
fn eth_value(raw_amount: ApexU256, eth_per_atomic: f64) -> f64 {
    let amount: f64 = raw_amount
        .to_string()
        .parse()
        .unwrap_or(0.0);
    amount * eth_per_atomic
}

/// The batcher's top-up of a partial fill, in scaled units: it supplies the buy-token remainder
/// `Y − Y'` — where `Y = scaled_sell × (Y'/X')` is the full-size output at the slice's clearing
/// price — and receives the unsold `X − X'` of the sell token. Computed through `BigUint` so the
/// product cannot overflow.
fn top_up(
    bought_scaled: ApexU256,
    sold_scaled: ApexU256,
    scaled_sell: ApexU256,
) -> (ApexU256, ApexU256) {
    if sold_scaled.is_zero() {
        return (ApexU256::ZERO, ApexU256::ZERO);
    }
    let bought = num_bigint::BigUint::from_bytes_le(&bought_scaled.to_le_bytes::<32>());
    let sold = num_bigint::BigUint::from_bytes_le(&sold_scaled.to_le_bytes::<32>());
    let full_sell = num_bigint::BigUint::from_bytes_le(&scaled_sell.to_le_bytes::<32>());
    let full_bought = &bought * &full_sell / &sold;
    let supply = full_bought - &bought;
    let supply_bytes = supply.to_bytes_le();
    let supply_scaled = if supply_bytes.len() > 32 {
        ApexU256::MAX
    } else {
        ApexU256::from_le_slice(&supply_bytes)
    };
    (supply_scaled, scaled_sell.saturating_sub(sold_scaled))
}

fn out_of_universe_record(
    block: u64,
    run: &'static str,
    variant: Variant,
    trade: &DecodedTrade,
    snapshot: &Snapshot,
) -> OrderRecord {
    let (sell, buy) = super::experiment_tokens(trade);
    let sell_meta = snapshot.meta(sell);
    let buy_meta = snapshot.meta(buy);
    OrderRecord {
        schema: 2,
        block,
        run,
        variant: variant.as_str(),
        tx_hash: format!("{:#x}", trade.tx_hash),
        tx_index: trade.tx_index,
        order_id: format!("{:#x}-oou-{}", trade.tx_hash, trade.tx_index),
        sender: format!("{:#x}", trade.sender),
        venue: trade.venue.clone(),
        solver: trade.solver.clone(),
        sell_token: format!("{sell:#x}"),
        buy_token: format!("{buy:#x}"),
        sell_symbol: sell_meta.symbol,
        buy_symbol: buy_meta.symbol,
        sell_decimals: sell_meta.decimals,
        buy_decimals: buy_meta.decimals,
        amount_in: dec(trade.amount_in),
        settled_amount_out: dec(trade.amount_out),
        amount_in_eth: eth_value(super::apex_u256(trade.amount_in), sell_meta.eth_per_atomic),
        settled_amount_out_eth: eth_value(
            super::apex_u256(trade.amount_out),
            buy_meta.eth_per_atomic,
        ),
        status: InclusionStatus::OutOfUniverse,
        apex_sold: "0".to_string(),
        apex_bought: "0".to_string(),
        batcher_sold: "0".to_string(),
        batcher_bought: "0".to_string(),
        limit_source: "",
        apex_bought_eth: 0.0,
        batcher_sold_eth: 0.0,
        batcher_bought_eth: 0.0,
    }
}

/// S2's AMM legs, scaled down to raw native units, for the CoW-share metric.
fn pool_volumes(result: &ApexResult, snapshot: &Snapshot) -> Vec<PoolVolumeRecord> {
    let mut volumes = Vec::with_capacity(result.pool_clearings.len());
    for clearing in &result.pool_clearings {
        let sold = clearing
            .pair
            .sell_token
            .decrease_precision(clearing.sold_amount);
        let bought = clearing
            .pair
            .buy_token
            .decrease_precision(clearing.bought_amount);
        let sell_meta = snapshot.meta(alloy_addr(clearing.pair.sell_token.address));
        let buy_meta = snapshot.meta(alloy_addr(clearing.pair.buy_token.address));
        volumes.push(PoolVolumeRecord {
            address: hex_addr(clearing.address),
            sell_token: hex_addr(clearing.pair.sell_token.address),
            buy_token: hex_addr(clearing.pair.buy_token.address),
            sold: dec(alloy_u256(sold)),
            bought: dec(alloy_u256(bought)),
            sold_eth: eth_value(sold, sell_meta.eth_per_atomic),
            bought_eth: eth_value(bought, buy_meta.eth_per_atomic),
        });
    }
    volumes
}

fn alloy_addr(address: apex_solver::types::Address) -> alloy::primitives::Address {
    alloy::primitives::Address::from(address.0)
}

fn hex_addr(address: apex_solver::types::Address) -> String {
    format!("{:#x}", alloy::primitives::Address::from(address.0))
}

/// JSON projection of one APEX solve's full result, dumped so every clearing can be explored
/// offline. Amounts and prices are in the solver's 18-decimal scaled convention, as decimal
/// strings; token and pool addresses are hex.
#[derive(Serialize)]
struct SolveResultJson {
    /// Which run config of the panel won this solve.
    winning_config: String,
    deadline_fired: bool,
    clearing_prices: BTreeMap<String, String>,
    limit_order_clearings: Vec<LimitClearingJson>,
    pool_clearings: Vec<PoolClearingJson>,
}

#[derive(Serialize)]
struct LimitClearingJson {
    id: String,
    sell_token: String,
    buy_token: String,
    sold_scaled: String,
    bought_scaled: String,
}

#[derive(Serialize)]
struct PoolClearingJson {
    address: String,
    sell_token: String,
    buy_token: String,
    sold_scaled: String,
    bought_scaled: String,
    surplus_scaled: String,
    fee: Option<String>,
}

impl SolveResultJson {
    fn from_result(result: &ApexResult, winning_config: &str) -> Self {
        let mut clearing_prices = BTreeMap::new();
        for (token, price) in &result.clearing_prices {
            clearing_prices.insert(hex_addr(*token), price.to_string());
        }
        let limit_order_clearings = result
            .limit_order_clearings
            .iter()
            .map(|c| LimitClearingJson {
                id: c.id.clone(),
                sell_token: hex_addr(c.sell_token),
                buy_token: hex_addr(c.buy_token),
                sold_scaled: c.sold_amount.to_string(),
                bought_scaled: c.bought_amount.to_string(),
            })
            .collect();
        let pool_clearings = result
            .pool_clearings
            .iter()
            .map(|c| PoolClearingJson {
                address: hex_addr(c.address),
                sell_token: hex_addr(c.pair.sell_token.address),
                buy_token: hex_addr(c.pair.buy_token.address),
                sold_scaled: c.sold_amount.to_string(),
                bought_scaled: c.bought_amount.to_string(),
                surplus_scaled: c.surplus.to_string(),
                fee: c.fee.map(|f| f.to_string()),
            })
            .collect();
        Self {
            winning_config: winning_config.to_string(),
            deadline_fired: result.deadline_fired,
            clearing_prices,
            limit_order_clearings,
            pool_clearings,
        }
    }
}

/// Persist a variant's full solve results — the S2 batch and every S1 single-order solve — as
/// `apex_result_<block>_<variant>.json`. A dump failure is logged, not fatal.
fn dump_results(
    block: u64,
    variant: Variant,
    s2_result: &ApexResult,
    s2_winning_config: &str,
    s1_results: BTreeMap<String, SolveResultJson>,
    results_dir: &Path,
) {
    #[derive(Serialize)]
    struct ResultsFile {
        block: u64,
        variant: &'static str,
        s2: SolveResultJson,
        /// Keyed by order id.
        s1: BTreeMap<String, SolveResultJson>,
    }
    let file = ResultsFile {
        block,
        variant: variant.as_str(),
        s2: SolveResultJson::from_result(s2_result, s2_winning_config),
        s1: s1_results,
    };
    let path = results_dir.join(format!("apex_result_{block}_{}.json", variant.as_str()));
    let outcome = std::fs::File::create(&path)
        .map_err(anyhow::Error::from)
        .and_then(|f| serde_json::to_writer(f, &file).map_err(anyhow::Error::from));
    if let Err(error) = outcome {
        warn!(%error, path = %path.display(), "failed to dump apex results");
    }
}

/// Persist the block's full APEX input for offline replay and debugging. A dump failure is
/// logged, not fatal — the live records are the experiment's primary output.
fn dump_input(block: u64, snapshot: &Snapshot, inputs_dir: &Path) {
    let input = ApexInputData {
        batch_id: block,
        tokens: snapshot.apex_tokens.clone(),
        initial_prices: snapshot.initial_prices.clone(),
        limit_orders: orders_by_pair(&snapshot.prepared, Variant::Permissive),
        market_orders: HashMap::new(),
        // The serializer emits `Pool::Apex` entries itself (as `"type": "custom"`, via
        // `ApexPool::to_snapshot_json`), so the full pool set goes in as-is.
        pools: snapshot.pools.clone(),
        custom_pools: Vec::new(),
    };
    let path = inputs_dir.join(format!("apex_input_{block}.json"));
    let result = std::fs::File::create(&path)
        .map_err(anyhow::Error::from)
        .and_then(|file| serde_json::to_writer(file, &input).map_err(anyhow::Error::from));
    if let Err(error) = result {
        warn!(%error, path = %path.display(), "failed to dump apex input");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_top_up_completes_fill_at_clearing_price() {
        // User sells 100 (scaled), APEX cleared 40 for 80: clearing price 2.0. The batcher
        // receives the unsold 60 and supplies the remaining output 200 - 80 = 120.
        let (supply, receive) =
            top_up(ApexU256::from(80u64), ApexU256::from(40u64), ApexU256::from(100u64));
        assert_eq!(supply, ApexU256::from(120u64));
        assert_eq!(receive, ApexU256::from(60u64));
    }

    #[test]
    fn test_top_up_zero_fill_is_empty() {
        let (supply, receive) = top_up(ApexU256::ZERO, ApexU256::ZERO, ApexU256::from(100u64));
        assert_eq!(supply, ApexU256::ZERO);
        assert_eq!(receive, ApexU256::ZERO);
    }
}

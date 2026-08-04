//! Stage 3 of the batching ladder: real APEX on decoded Base trades against CURRENT AMM state.
//!
//! STATE-DRIFT-CONTAMINATED BY DESIGN: the orders are up to a day old, the pool state is now.
//! This is the integration + mechanism milestone (pools, real decimals, real derived prices,
//! internalization) — the clean surplus number is stage 4's block-state run.
//!
//! Two cells per window/limit so the pool uplift is attributable: `pools` (the native-only
//! 2-hop subset) and `no_pools` (identical inputs, empty pool set). Both share the same real
//! decimals, the same derived-price map, and the same component partitioning discipline.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use apex_batch::{
    adapter::{PoolAddressBook, TychoApexPool},
    dataset::{load_day_headline, u256_to_f64, Intent},
    prices::{batch_value_wei, build_apex_prices, TokenPriceInput, MAX_PRECISION_INCREASES},
    scaling::{Scaled18, TokenScale},
    subset::{select_pool_subset, PoolCandidate},
};
use apex_solver::{
    core::{
        pools::{custom::ApexPool, Pool, PoolMetadata},
        ApexConfig, Fraction, LimitOrder, Token as ApexToken, TradingPair,
    },
    run_apex_with_config,
    types::{Address as ApexAddress, U256 as ApexU256},
};
use clap::Parser;
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_simulation::tycho_common::{
    models::{token::Token as TychoToken, Chain},
    simulation::protocol_sim::ProtocolSim,
};

const WINDOWS: [u64; 5] = [1, 5, 15, 30, 150];
const SOLVE_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(about = "APEX on decoded Base trades against current live AMM state (stage 3)")]
struct Args {
    #[arg(long, default_value = "tycho-base-beta.propellerheads.xyz")]
    tycho_url: String,
    /// Chain RPC for the gas fetcher the solver build requires; snapshotting never queries it
    /// heavily.
    #[arg(long, env = "RPC_URL", default_value = "https://mainnet.base.org")]
    rpc_url: String,
    #[arg(long, default_value_t = 100.0)]
    min_tvl: f64,
    /// Cap on the pool subset (class-priority order: direct > adjacent > linking).
    #[arg(long, default_value_t = 400)]
    max_pools: usize,
    #[arg(
        long,
        default_value = "/Users/pistomat/Projects/propeller-heads/fynd/data/hindsight/base-comparisons"
    )]
    data_dir: PathBuf,
    #[arg(long, default_value = "docs/analysis/2026-08-base-cow-phase0/stage3-apex-with-amms")]
    out_dir: PathBuf,
    /// Days of decoded trades to replay (drift grows with age; newest day is the default).
    #[arg(long, default_values_t = vec!["2026-08-03".to_string()])]
    days: Vec<String>,
    /// Limit cells in basis points below the settled amount.
    #[arg(long, default_values_t = vec![50u32, 100, 200])]
    limit_bps: Vec<u32>,
    /// How long to wait for the first complete market + derived-price computation.
    #[arg(long, default_value_t = 900)]
    ready_timeout_secs: u64,
    /// Load a previously persisted snapshot instead of connecting to tycho.
    #[arg(long)]
    snapshot: Option<PathBuf>,
    /// Where the live run persists its snapshot (zstd JSON).
    #[arg(long, default_value = "/Users/pistomat/Projects/propeller-heads/fynd/data/apex-snapshots")]
    snapshot_out: PathBuf,
    /// Cap on per-order fynd quotes taken at the live state before disconnecting (cell-b
    /// baseline); orders beyond the cap are sampled evenly and the share is reported.
    #[arg(long, default_value_t = 2500)]
    fynd_quote_cap: usize,
    /// Worker-pool config for the in-process solver (the fynd-quote baseline needs real pools).
    #[arg(long, env = "WORKER_POOLS_CONFIG", default_value = "worker_pools.toml")]
    worker_pools_config: PathBuf,
}

/// Floor anchoring for a cell (the drift axis): `Original` anchors floors to trade-time
/// economics (settled amounts), `Current` re-anchors them to the snapshot state (pool-implied
/// quotes) — the (a)−(b) gap is the measured drift cost.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Anchor {
    Original,
    Current,
}

/// Everything solving needs from the one live-state snapshot, detached from the solver.
struct StateSnapshot {
    state_label: String,
    price_block: Option<u64>,
    pools: Vec<SnapshotPool>,
    price_inputs: HashMap<ApexAddress, TokenPriceInput>,
    token_meta: HashMap<ApexAddress, (String, u8)>,
    subset_dropped_by_cap: u64,
    clone_box_ms: u128,
    arc_from_ms: u128,
    /// Component ids whose state could not serialize (UniswapV4State's hook field) — present
    /// in-memory on a live run, absent after a from-disk load.
    v4_dropped: Vec<String>,
    /// Per-order fynd quotes taken at this state before disconnecting (raw token_out units),
    /// persisted with the snapshot so from-disk reruns keep the cell-b baseline.
    fynd_quotes: HashMap<String, alloy::primitives::U256>,
    fynd_quote_sample_share: f64,
}

/// The persisted snapshot file: everything a from-disk rerun needs, states typetag-encoded.
#[derive(Serialize, Deserialize)]
struct PersistedSnapshot {
    state_label: String,
    price_block: Option<u64>,
    pools: Vec<PersistedPool>,
    v4_dropped: Vec<String>,
    price_rationals: Vec<(String, String, String, u8)>,
    token_meta: Vec<(String, String, u8)>,
    fynd_quotes: Vec<(String, String)>,
    fynd_quote_sample_share: f64,
}

#[derive(Serialize, Deserialize)]
struct PersistedPool {
    component_id: String,
    protocol: String,
    tokens: Vec<(String, String, u8)>,
    state: serde_json::Value,
}

fn hex_addr(address: ApexAddress) -> String {
    format!("0x{}", alloy::primitives::hex::encode(address.0))
}

struct SnapshotPool {
    component_id: String,
    apex_address: ApexAddress,
    token_0: ApexAddress,
    token_1: ApexAddress,
    adapter: Arc<TychoApexPool>,
    /// Whether the inner state serialized — false = v4-style, lost on a from-disk rerun.
    persisted: bool,
}

#[derive(Debug, Default, Clone, Serialize)]
struct Counters {
    orders_in: u64,
    filled: u64,
    partially_filled: u64,
    unfilled_at_limit: u64,
    cluster_cut: u64,
    component_errored: u64,
    component_errors: BTreeMap<String, u64>,
    solver_panics: u64,
    deadline_fired_batches: u64,
    singles_skipped: u64,
    wash_orders_excluded: u64,
    token_unpriced: u64,
    price_underflow: u64,
    unknown_decimals: u64,
    scale_overflow: u64,
    zero_limit_excluded: u64,
    pool_unpriced: u64,
    pools_in_scope: u64,
    negative_fill_gaps: u64,
    negative_gap_usd: f64,
    components_solved: u64,
    components_multi_order: u64,
    /// Orders with no snapshot-state quote to anchor a Current floor to.
    no_current_quote: u64,
    fynd_compared: u64,
    fynd_uncompared: u64,
}

impl Counters {
    fn absorb(&mut self, other: &Counters) {
        self.orders_in += other.orders_in;
        self.filled += other.filled;
        self.partially_filled += other.partially_filled;
        self.unfilled_at_limit += other.unfilled_at_limit;
        self.cluster_cut += other.cluster_cut;
        self.component_errored += other.component_errored;
        for (kind, count) in &other.component_errors {
            *self
                .component_errors
                .entry(kind.clone())
                .or_default() += count;
        }
        self.solver_panics += other.solver_panics;
        self.deadline_fired_batches += other.deadline_fired_batches;
        self.singles_skipped += other.singles_skipped;
        self.wash_orders_excluded += other.wash_orders_excluded;
        self.token_unpriced += other.token_unpriced;
        self.price_underflow += other.price_underflow;
        self.unknown_decimals += other.unknown_decimals;
        self.scale_overflow += other.scale_overflow;
        self.zero_limit_excluded += other.zero_limit_excluded;
        self.pool_unpriced += other.pool_unpriced;
        self.pools_in_scope += other.pools_in_scope;
        self.negative_fill_gaps += other.negative_fill_gaps;
        self.negative_gap_usd += other.negative_gap_usd;
        self.components_solved += other.components_solved;
        self.components_multi_order += other.components_multi_order;
        self.no_current_quote += other.no_current_quote;
        self.fynd_compared += other.fynd_compared;
        self.fynd_uncompared += other.fynd_uncompared;
    }
}

#[derive(Default, Clone, Serialize)]
struct CellResult {
    window_blocks: u64,
    limit_bps: u32,
    cell: String,
    anchor: String,
    pool_scope: String,
    intent_usd: f64,
    apex_matched_usd: f64,
    apex_matched_pct: f64,
    apex_surplus_usd: f64,
    pool_cleared_wei: f64,
    order_notional_wei: f64,
    internalization_share: Option<f64>,
    solve_ms_p50: u128,
    solve_ms_p90: u128,
    solve_ms_max: u128,
    fynd_compared: u64,
    fynd_median_bps: f64,
    fynd_mean_bps: f64,
    fynd_apex_ge_share: f64,
    fynd_usd_delta: f64,
    counters: Counters,
    wall_ms: u128,
}

#[derive(Serialize)]
struct SnapshotMeta {
    state_label: String,
    price_block: Option<u64>,
    pool_count: usize,
    priced_tokens: usize,
    subset_dropped_by_cap: u64,
    clone_box_ms: u128,
    arc_from_ms: u128,
    v4_dropped: Vec<String>,
    fynd_quotes_taken: usize,
    fynd_quote_sample_share: f64,
    current_quotes: usize,
}

/// Per-token NET pool exposure valued in wei, over one solve's pool clearings.
///
/// A pool clearing's `sold_amount` leaves the pool in `pair.sell_token`; `bought_amount` enters
/// it in `pair.buy_token`, both in 18-dec space. `internalization = 1 − Σ|net| / (2 × order
/// notional)`: a batch cleared purely order-against-order nets to zero pool exposure (share 1),
/// while a single order routed entirely through pools has |net| = 2 × its notional (share 0) —
/// per-token netting keeps multi-hop intermediates out of the sum (grill r2 F5).
fn net_pool_exposure_wei(
    clearings: impl Iterator<Item = (ApexAddress, ApexAddress, ApexU256, ApexU256)>,
    wei_per_unit18: &HashMap<ApexAddress, f64>,
) -> f64 {
    let mut net: HashMap<ApexAddress, f64> = HashMap::new();
    for (sell_token, buy_token, sold, bought) in clearings {
        *net.entry(sell_token).or_default() -= u256_to_f64(alloy_u256(sold));
        *net.entry(buy_token).or_default() += u256_to_f64(alloy_u256(bought));
    }
    net.into_iter()
        .filter_map(|(token, amount18)| Some(amount18.abs() * wei_per_unit18.get(&token)?))
        .sum()
}

fn alloy_u256(value: ApexU256) -> alloy::primitives::U256 {
    alloy::primitives::U256::from_le_bytes(value.to_le_bytes::<32>())
}

fn to_apex_u256(value: alloy::primitives::U256) -> ApexU256 {
    ApexU256::from_le_bytes(value.to_le_bytes::<32>())
}

fn biguint_from_apex(value: ApexU256) -> BigUint {
    BigUint::from_bytes_le(&value.to_le_bytes::<32>())
}

/// Wei per 18-dec unit of `token`, from its tycho rational: `10^(dec−18) · den/num` as f64 —
/// reporting-precision only, never fed back into APEX.
fn wei_per_unit18(input: &TokenPriceInput) -> f64 {
    let num = biguint_f64(&input.numerator);
    if num == 0.0 {
        return 0.0;
    }
    biguint_f64(&input.denominator) / num * 10f64.powi(input.decimals as i32 - 18)
}

fn biguint_f64(value: &BigUint) -> f64 {
    value.to_string().parse().unwrap_or(0.0)
}

struct BatchOutcome {
    matched_usd: f64,
    surplus_usd: f64,
    pool_cleared_wei: f64,
    order_notional_wei: f64,
    counters: Counters,
    fynd_bps: Vec<f64>,
    fynd_usd_delta: f64,
}

#[allow(clippy::too_many_arguments)]
fn solve_batch(
    intents: &[Intent],
    snapshot: &StateSnapshot,
    day_price: &HashMap<ApexAddress, f64>,
    limit_bps: u32,
    with_pools: bool,
    anchor: Anchor,
    serializable_only: bool,
    current_quotes: &HashMap<String, alloy::primitives::U256>,
    solve_times: &mut Vec<u128>,
) -> BatchOutcome {
    let mut counters = Counters::default();
    let mut matched_usd = 0.0f64;
    let mut surplus_usd = 0.0f64;
    let mut pool_cleared_wei = 0.0f64;
    let mut order_notional_wei = 0.0f64;
    let mut fynd_bps: Vec<f64> = Vec::new();
    let mut fynd_usd_delta = 0.0f64;

    let mut orders: Vec<&Intent> = Vec::with_capacity(intents.len());
    for intent in intents {
        counters.orders_in += 1;
        if intent.is_wash {
            counters.wash_orders_excluded += 1;
            continue;
        }
        let known = snapshot
            .token_meta
            .contains_key(&intent.token_in) &&
            snapshot
                .token_meta
                .contains_key(&intent.token_out);
        if !known {
            counters.unknown_decimals += 1;
            continue;
        }
        let priced = snapshot
            .price_inputs
            .contains_key(&intent.token_in) &&
            snapshot
                .price_inputs
                .contains_key(&intent.token_out);
        if !priced {
            counters.token_unpriced += 1;
            continue;
        }
        orders.push(intent);
    }
    if orders.is_empty() {
        return BatchOutcome {
            matched_usd,
            surplus_usd,
            pool_cleared_wei,
            order_notional_wei,
            counters,
            fynd_bps,
            fynd_usd_delta,
        };
    }

    // Union-find over orders ∪ subset pools: a pool edge merges its two tokens' components, so
    // partitioning is identical for both cells only through the pool topology — the no-pools
    // cell reuses the SAME partitioning (comparability invariant).
    let mut token_index: HashMap<ApexAddress, usize> = HashMap::new();
    for order in &orders {
        for token in [order.token_in, order.token_out] {
            let next = token_index.len();
            token_index.entry(token).or_insert(next);
        }
    }
    let order_token_count = token_index.len();
    let mut parent: Vec<usize> = (0..order_token_count).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for order in &orders {
        let a = token_index[&order.token_in];
        let b = token_index[&order.token_out];
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        parent[ra] = rb;
    }
    let relevant_pools: Vec<&SnapshotPool> = snapshot
        .pools
        .iter()
        .filter(|pool| {
            token_index.contains_key(&pool.token_0) || token_index.contains_key(&pool.token_1)
        })
        .collect();
    for pool in &relevant_pools {
        if let (Some(&a), Some(&b)) =
            (token_index.get(&pool.token_0), token_index.get(&pool.token_1))
        {
            let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
            parent[ra] = rb;
        }
    }

    let mut components: HashMap<usize, Vec<&Intent>> = HashMap::new();
    for order in &orders {
        let root = find(&mut parent, token_index[&order.token_in]);
        components
            .entry(root)
            .or_default()
            .push(order);
    }

    for (root, component_orders) in components {
        if component_orders.len() < 2 && !with_pools {
            counters.singles_skipped += 1;
            continue;
        }
        counters.components_multi_order += u64::from(component_orders.len() >= 2);

        // The component's pools: both tokens in this component (or one token shared and the
        // other joining the closure). Skipped pools with an unpriced token drop out counted.
        let mut component_pools: Vec<&SnapshotPool> = Vec::new();
        if with_pools {
            for pool in &relevant_pools {
                let in_component = [pool.token_0, pool.token_1]
                    .iter()
                    .any(|t| {
                        token_index
                            .get(t)
                            .is_some_and(|&i| find(&mut parent, i) == root)
                    });
                if !in_component {
                    continue;
                }
                let priced = snapshot
                    .price_inputs
                    .contains_key(&pool.token_0) &&
                    snapshot
                        .price_inputs
                        .contains_key(&pool.token_1) &&
                    snapshot
                        .token_meta
                        .contains_key(&pool.token_0) &&
                    snapshot
                        .token_meta
                        .contains_key(&pool.token_1);
                if !priced {
                    counters.pool_unpriced += 1;
                    continue;
                }
                component_pools.push(pool);
            }
            counters.pools_in_scope += component_pools.len() as u64;
        }

        // Token closure = order tokens ∪ pool tokens, all priced (precondition r2 F1/F12).
        let mut closure: HashSet<ApexAddress> = HashSet::new();
        for order in &component_orders {
            closure.insert(order.token_in);
            closure.insert(order.token_out);
        }
        for pool in &component_pools {
            closure.insert(pool.token_0);
            closure.insert(pool.token_1);
        }

        let price_inputs: HashMap<ApexAddress, TokenPriceInput> = closure
            .iter()
            .filter_map(|t| {
                snapshot
                    .price_inputs
                    .get(t)
                    .map(|p| (*t, p.clone()))
            })
            .collect();
        let batch_wei = batch_value_wei(
            component_orders
                .iter()
                .map(|o| (o.token_in, biguint_from_apex(o.amount_in))),
            &price_inputs,
        );
        let price_map = build_apex_prices(&price_inputs, &batch_wei);
        let unpriced_or_underflow: HashSet<ApexAddress> = price_map
            .price_underflow
            .iter()
            .chain(price_map.unpriced.iter())
            .copied()
            .collect();

        let wei_prices: HashMap<ApexAddress, f64> = price_inputs
            .iter()
            .map(|(t, p)| (*t, wei_per_unit18(p)))
            .collect();

        let mut apex_tokens: HashMap<ApexAddress, ApexToken> = HashMap::new();
        for token in &closure {
            if unpriced_or_underflow.contains(token) {
                continue;
            }
            let (symbol, decimals) = &snapshot.token_meta[token];
            apex_tokens.insert(*token, ApexToken::new(*token, symbol, *decimals));
        }

        let mut limit_orders: HashMap<(ApexAddress, ApexAddress), Vec<LimitOrder>> = HashMap::new();
        let mut order_inputs: HashMap<String, (&Intent, f64)> = HashMap::new();
        for order in &component_orders {
            if unpriced_or_underflow.contains(&order.token_in) ||
                unpriced_or_underflow.contains(&order.token_out)
            {
                counters.price_underflow += 1;
                continue;
            }
            let scale_in = TokenScale::new(snapshot.token_meta[&order.token_in].1)
                .expect("token_meta only holds ≤18-dec tokens");
            let scale_out = TokenScale::new(snapshot.token_meta[&order.token_out].1)
                .expect("token_meta only holds ≤18-dec tokens");
            let floor_basis = match anchor {
                Anchor::Original => alloy_u256(order.settled_out),
                Anchor::Current => match current_quotes.get(&order.id) {
                    Some(quote) if !quote.is_zero() => *quote,
                    _ => {
                        counters.no_current_quote += 1;
                        continue;
                    }
                },
            };
            let min_out_raw = floor_basis *
                alloy::primitives::U256::from(10_000 - limit_bps as u64) /
                alloy::primitives::U256::from(10_000u64);
            let (Ok(amount18), Ok(min_out18)) =
                (scale_in.scale_up(alloy_u256(order.amount_in)), scale_out.scale_up(min_out_raw))
            else {
                counters.scale_overflow += 1;
                continue;
            };
            if min_out18.0.is_zero() {
                counters.zero_limit_excluded += 1;
                continue;
            }
            assert!(
                order_inputs
                    .insert(order.id.clone(), (order, u256_to_f64(amount18.0)))
                    .is_none(),
                "duplicate order id {} in one batch",
                order.id
            );
            let pair =
                TradingPair::new(apex_tokens[&order.token_in], apex_tokens[&order.token_out]);
            limit_orders
                .entry(pair.addresses())
                .or_default()
                .push(LimitOrder::new(
                    to_apex_u256(amount18.0),
                    Fraction::new(to_apex_u256(min_out18.0), to_apex_u256(amount18.0)),
                    order.id.clone(),
                    ApexAddress([0u8; 20]),
                ));
            order_notional_wei += u256_to_f64(amount18.0) *
                wei_prices
                    .get(&order.token_in)
                    .copied()
                    .unwrap_or(0.0);
        }
        if order_inputs.is_empty() || (order_inputs.len() < 2 && component_pools.is_empty()) {
            counters.singles_skipped += order_inputs.len() as u64;
            continue;
        }

        let pools: Vec<Pool> = component_pools
            .iter()
            .filter(|pool| !serializable_only || pool.persisted)
            .filter(|pool| {
                !unpriced_or_underflow.contains(&pool.token_0) &&
                    !unpriced_or_underflow.contains(&pool.token_1)
            })
            .map(|pool| {
                Pool::Apex(
                    PoolMetadata {
                        address: pool.apex_address,
                        token_0: pool.token_0,
                        token_1: pool.token_1,
                    },
                    pool.adapter.clone() as Arc<dyn ApexPool>,
                )
            })
            .collect();

        let mut config = ApexConfig {
            enable_two_hops: with_pools,
            max_workers: 1,
            collect_metrics: false,
            deadline: Some(Instant::now() + SOLVE_DEADLINE),
            ..ApexConfig::default()
        };
        config
            .price_search_config
            .max_precision_increases = MAX_PRECISION_INCREASES;

        counters.components_solved += 1;
        let tokens: Vec<ApexToken> = apex_tokens.values().copied().collect();
        let solve_started = Instant::now();
        let solve = catch_unwind(AssertUnwindSafe(|| {
            run_apex_with_config(
                tokens,
                price_map
                    .prices
                    .iter()
                    .map(|(k, v)| (*k, *v))
                    .collect(),
                limit_orders.clone(),
                HashMap::new(),
                pools,
                config,
            )
        }));
        solve_times.push(solve_started.elapsed().as_millis());
        let result = match solve {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                counters.component_errored += order_inputs.len() as u64;
                let kind = match &error {
                    apex_solver::core::ApexError::InvalidInput(_) => "invalid_input",
                    apex_solver::core::ApexError::MetricsCollectionError(_) => "metrics",
                    apex_solver::core::ApexError::TradeSolverError(_) => "trade_solver",
                    apex_solver::core::ApexError::MarketRouterError(_) => "market_router",
                    apex_solver::core::ApexError::ClearingUnderLimitPrice(_, _) => {
                        "clearing_under_limit"
                    }
                    apex_solver::core::ApexError::NegativeBalanceDelta(_, _) => {
                        "negative_balance_delta"
                    }
                    _ => "other",
                };
                *counters
                    .component_errors
                    .entry(kind.to_string())
                    .or_default() += 1;
                continue;
            }
            Err(_) => {
                counters.solver_panics += 1;
                counters.component_errored += order_inputs.len() as u64;
                continue;
            }
        };
        if result.deadline_fired {
            counters.deadline_fired_batches += 1;
        }

        pool_cleared_wei += net_pool_exposure_wei(
            result.pool_clearings.iter().map(|c| {
                (
                    c.pair.sell_token.address,
                    c.pair.buy_token.address,
                    c.sold_amount,
                    c.bought_amount,
                )
            }),
            &wei_prices,
        );

        let clearings: HashMap<&str, _> = result
            .limit_order_clearings
            .iter()
            .map(|c| (c.id.as_str(), c))
            .collect();
        for (id, (order, amount18)) in &order_inputs {
            match clearings.get(id.as_str()) {
                Some(clearing) if !clearing.sold_amount.is_zero() => {
                    let fill_ratio =
                        u256_to_f64(alloy_u256(clearing.sold_amount)) / amount18.max(1.0);
                    if fill_ratio < 1.0 - 1e-9 {
                        counters.partially_filled += 1;
                    } else {
                        counters.filled += 1;
                    }
                    matched_usd += order.usd * fill_ratio.min(1.0);
                    let scale_out = TokenScale::new(snapshot.token_meta[&order.token_out].1)
                        .expect("token_meta only holds ≤18-dec tokens");
                    let bought_raw =
                        scale_out.scale_down_floor(Scaled18(alloy_u256(clearing.bought_amount)));
                    let settled_pro_rata =
                        u256_to_f64(alloy_u256(order.settled_out)) * fill_ratio.min(1.0);
                    let gap_raw = u256_to_f64(bought_raw) - settled_pro_rata;
                    let usd_per_raw = day_price
                        .get(&order.token_out)
                        .copied()
                        .unwrap_or(0.0);
                    if gap_raw > 0.0 {
                        surplus_usd += gap_raw * usd_per_raw;
                    } else if gap_raw < 0.0 {
                        counters.negative_fill_gaps += 1;
                        counters.negative_gap_usd += -gap_raw * usd_per_raw;
                    }
                }
                Some(_) => counters.unfilled_at_limit += 1,
                None if result.deadline_fired => counters.cluster_cut += 1,
                None => counters.unfilled_at_limit += 1,
            }
            // Engine-inclusive fynd baseline, state-consistent per anchor: Original compares
            // against the dataset's trade-time fynd quote; Current against the fynd quote taken
            // at the persisted snapshot state (plan item L).
            if let Some(clearing) = clearings.get(id.as_str()) {
                if !clearing.sold_amount.is_zero() {
                    let fill_ratio =
                        u256_to_f64(alloy_u256(clearing.sold_amount)) / amount18.max(1.0);
                    let scale_out = TokenScale::new(snapshot.token_meta[&order.token_out].1)
                        .expect("token_meta only holds ≤18-dec tokens");
                    let bought_raw = u256_to_f64(
                        scale_out.scale_down_floor(Scaled18(alloy_u256(clearing.bought_amount))),
                    );
                    let baseline_raw = match anchor {
                        Anchor::Original => order.fynd_out.map(alloy_u256),
                        Anchor::Current => snapshot.fynd_quotes.get(&order.id).copied(),
                    };
                    match baseline_raw {
                        Some(fynd_out) if !fynd_out.is_zero() => {
                            counters.fynd_compared += 1;
                            let fynd_pro_rata = u256_to_f64(fynd_out) * fill_ratio.min(1.0);
                            let relative_gap = (bought_raw - fynd_pro_rata) / fynd_pro_rata;
                            fynd_bps.push(10_000.0 * relative_gap);
                            // Valued against the order's own quarantined USD notional — see the
                            // stage2 comment.
                            fynd_usd_delta += relative_gap * order.usd * fill_ratio.min(1.0);
                        }
                        _ => counters.fynd_uncompared += 1,
                    }
                }
            }
        }
    }
    BatchOutcome {
        matched_usd,
        surplus_usd,
        pool_cleared_wei,
        order_notional_wei,
        counters,
        fynd_bps,
        fynd_usd_delta,
    }
}

/// Pool-implied output for one order's amount at the snapshot state: best of direct pools and
/// 2-hop routes through the subset, in raw `token_out` units. The Current anchor's floor basis.
fn pool_implied_quote(
    order: &Intent,
    snapshot: &StateSnapshot,
    pools_by_token: &HashMap<ApexAddress, Vec<usize>>,
) -> Option<alloy::primitives::U256> {
    let scale_in = TokenScale::new(snapshot.token_meta.get(&order.token_in)?.1).ok()?;
    let scale_out = TokenScale::new(snapshot.token_meta.get(&order.token_out)?.1).ok()?;
    let amount18 = scale_in
        .scale_up(alloy_u256(order.amount_in))
        .ok()?;
    let empty = Vec::new();
    let in_pools = pools_by_token
        .get(&order.token_in)
        .unwrap_or(&empty);

    let mut best18 = ApexU256::ZERO;
    // Direct pools.
    for &index in in_pools {
        let pool = &snapshot.pools[index];
        let other =
            if pool.token_0 == order.token_in { pool.token_1 } else { pool.token_0 };
        if other != order.token_out {
            continue;
        }
        let out = pool.adapter.get_amount_out(
            order.token_in,
            order.token_out,
            to_apex_u256(amount18.0),
            ApexU256::ZERO,
        );
        best18 = best18.max(out);
    }
    // Two hops through any shared intermediate.
    for &first_index in in_pools {
        let first = &snapshot.pools[first_index];
        let mid = if first.token_0 == order.token_in { first.token_1 } else { first.token_0 };
        if mid == order.token_out {
            continue;
        }
        let mid_out = first.adapter.get_amount_out(
            order.token_in,
            mid,
            to_apex_u256(amount18.0),
            ApexU256::ZERO,
        );
        if mid_out.is_zero() {
            continue;
        }
        for &second_index in pools_by_token.get(&mid).unwrap_or(&empty) {
            let second = &snapshot.pools[second_index];
            let far = if second.token_0 == mid { second.token_1 } else { second.token_0 };
            if far != order.token_out {
                continue;
            }
            let out =
                second
                    .adapter
                    .get_amount_out(mid, order.token_out, mid_out, ApexU256::ZERO);
            best18 = best18.max(out);
        }
    }
    if best18.is_zero() {
        return None;
    }
    Some(scale_out.scale_down_floor(Scaled18(alloy_u256(best18))))
}

/// Bootstrap the in-process solver, wait for the first complete market + derived prices, take
/// per-order fynd quotes at that state, and detach everything solving needs. The solver is shut
/// down before this returns, and the snapshot is persisted to disk.
async fn take_snapshot(
    args: &Args,
    order_tokens: &HashSet<[u8; 20]>,
    orders: &[Intent],
) -> Result<StateSnapshot> {
    let api_key = std::env::var("TYCHO_API_KEY").ok();
    if api_key.is_none() {
        bail!(
            "TYCHO_API_KEY is not set. Run:\n  TYCHO_API_KEY=… RPC_URL=… cargo run --release -p \
             apex-batch --bin stage3"
        );
    }
    let protocols = fynd_rpc::protocols::resolve_protocols(
        &args.tycho_url,
        api_key.as_deref(),
        true,
        Chain::Base,
        &["native_onchain".to_string()],
    )
    .await
    .context("resolving native protocol list from tycho")?;
    eprintln!("native protocols: {protocols:?}");

    let mut builder = fynd_core::solver::FyndBuilder::new(
        Chain::Base,
        &args.tycho_url,
        &args.rpc_url,
        protocols,
        args.min_tvl,
    );
    if let Some(key) = api_key.as_deref() {
        builder = builder.tycho_api_key(key);
    }
    let pools_config =
        fynd_rpc::config::WorkerPoolsConfig::load_from_file(&args.worker_pools_config)
            .with_context(|| {
                format!("loading worker pools from {}", args.worker_pools_config.display())
            })?;
    for (name, pool) in pools_config.pools() {
        builder = builder
            .add_pool(name, pool)
            .map_err(|e| anyhow::anyhow!("failed to add worker pool {name}: {e}"))?;
    }
    eprintln!("building in-process solver (token loading takes minutes)…");
    let solver = builder
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build solver: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(args.ready_timeout_secs);
    let market = solver.market_data();
    let derived = solver.derived_data();
    let (state_label, price_block) = loop {
        if Instant::now() > deadline {
            solver.shutdown();
            bail!(
                "market/derived data not ready within {}s — check TYCHO_API_KEY and the tycho \
                 endpoint",
                args.ready_timeout_secs
            );
        }
        let market_ready = {
            let view = market.read().await;
            view.base_market_state()
                .last_updated()
                .is_some()
        };
        let prices_ready = {
            let guard = derived.read().await;
            guard.token_prices().is_some()
        };
        if market_ready && prices_ready {
            let view = market.read().await;
            let guard = derived.read().await;
            break (view.base_market_state().label().clone(), guard.token_prices_block());
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    eprintln!("state ready at label {state_label:?}, price block {price_block:?}");

    // Everything below runs under one read lock: candidates, subset, clone, prices, metadata.
    let view = market.read().await;
    let state = view.base_market_state();
    let mut candidates: Vec<PoolCandidate> = Vec::new();
    for (component_id, token_addresses) in state.component_topology() {
        let Some(component) = state.get_component(&component_id) else { continue };
        candidates.push(PoolCandidate {
            component_id: component_id.clone(),
            protocol_system: component.protocol_system.clone(),
            tokens: token_addresses
                .iter()
                .filter_map(|a| {
                    let bytes: &[u8] = a.as_ref();
                    (bytes.len() == 20).then(|| {
                        let mut out = [0u8; 20];
                        out.copy_from_slice(bytes);
                        out
                    })
                })
                .collect(),
        });
    }
    let selection = select_pool_subset(&candidates, order_tokens, args.max_pools);
    eprintln!(
        "subset: kept={} dropped_by_cap={} (of {} candidates)",
        selection.kept.len(),
        selection.dropped_by_cap,
        candidates.len()
    );

    let mut address_book = PoolAddressBook::default();
    let mut pools = Vec::new();
    let mut clone_box_ms = 0u128;
    let mut arc_from_ms = 0u128;
    let candidate_by_id: HashMap<&str, &PoolCandidate> = candidates
        .iter()
        .map(|c| (c.component_id.as_str(), c))
        .collect();
    for component_id in &selection.kept {
        let candidate = candidate_by_id[component_id.as_str()];
        if candidate.tokens.len() != 2 {
            continue;
        }
        let Some(simulation) = state.get_simulation_state(component_id) else { continue };
        let apex_address = address_book
            .register(component_id)
            .map_err(|other| {
                anyhow::anyhow!("pool address collision: {component_id} vs {other}")
            })?;
        let started = Instant::now();
        let boxed = simulation.clone_box();
        clone_box_ms += started.elapsed().as_millis();
        let persisted = serde_json::to_value(&*boxed as &dyn ProtocolSim).is_ok();
        let started = Instant::now();
        let arc: Arc<dyn ProtocolSim> = Arc::from(boxed);
        arc_from_ms += started.elapsed().as_millis();

        let mut token_map: HashMap<ApexAddress, TychoToken> = HashMap::new();
        for token_bytes in &candidate.tokens {
            let address = tycho_simulation::tycho_common::Bytes::from(token_bytes.to_vec());
            let Some(token) = state.get_token(&address) else { continue };
            token_map.insert(ApexAddress(*token_bytes), token.clone());
        }
        if token_map.len() != 2 {
            continue;
        }
        pools.push(SnapshotPool {
            component_id: component_id.clone(),
            apex_address,
            token_0: ApexAddress(candidate.tokens[0]),
            token_1: ApexAddress(candidate.tokens[1]),
            adapter: Arc::new(TychoApexPool {
                protocol: candidate.protocol_system.clone(),
                tokens: token_map,
                pool: arc,
            }),
            persisted,
        });
    }

    // Price rationals + token metadata for every token the run can touch.
    let mut price_inputs = HashMap::new();
    let mut token_meta = HashMap::new();
    let mut wanted: HashSet<[u8; 20]> = order_tokens.clone();
    for pool in &pools {
        wanted.insert(pool.token_0.0);
        wanted.insert(pool.token_1.0);
    }
    let derived_guard = derived.read().await;
    let token_prices = derived_guard
        .token_prices()
        .expect("readiness loop saw token prices");
    for token_bytes in wanted {
        let address = tycho_simulation::tycho_common::Bytes::from(token_bytes.to_vec());
        let Some(token) = state.get_token(&address) else { continue };
        let Ok(decimals) = u8::try_from(token.decimals) else { continue };
        token_meta.insert(ApexAddress(token_bytes), (token.symbol.clone(), decimals));
        if let Some(price) = token_prices.get(&address) {
            price_inputs.insert(
                ApexAddress(token_bytes),
                TokenPriceInput {
                    numerator: price.numerator.clone(),
                    denominator: price.denominator.clone(),
                    decimals,
                },
            );
        }
    }
    drop(derived_guard);
    drop(view);

    // Per-order fynd quotes at this exact state (cell-b baseline), evenly sampled to the cap.
    use fynd_core::types::{
        EncodingOptions, Order as FyndOrder, OrderSide, QuoteOptions, QuoteRequest,
    };
    let stride = orders.len().div_ceil(args.fynd_quote_cap.max(1));
    let sampled: Vec<&Intent> = orders.iter().step_by(stride.max(1)).collect();
    let sample_share = sampled.len() as f64 / orders.len().max(1) as f64;
    eprintln!(
        "taking {} fynd quotes at the snapshot state (stride {stride}, {:.0}% of orders)…",
        sampled.len(),
        100.0 * sample_share
    );
    let mut fynd_quotes: HashMap<String, alloy::primitives::U256> = HashMap::new();
    for (index, order) in sampled.iter().enumerate() {
        let request = QuoteRequest::new(
            vec![FyndOrder::new(
                tycho_simulation::tycho_core::models::Address::from(order.token_in.0),
                tycho_simulation::tycho_core::models::Address::from(order.token_out.0),
                num_bigint::BigUint::from_bytes_le(&alloy_u256(order.amount_in).to_le_bytes::<32>()),
                OrderSide::Sell,
                tycho_simulation::tycho_core::models::Address::from([0x11u8; 20]),
            )],
            QuoteOptions::default()
                .with_timeout_ms(500)
                .with_encoding_options(EncodingOptions::new(0.005)),
        );
        if let Ok(quote) = solver.quote(request).await {
            if let Some(order_quote) = quote.orders().first() {
                let bytes = order_quote.amount_out().to_bytes_le();
                if bytes.len() <= 32 {
                    fynd_quotes
                        .insert(order.id.clone(), alloy::primitives::U256::from_le_slice(&bytes));
                }
            }
        }
        if index % 250 == 0 {
            eprintln!("  fynd quotes: {index}/{}", sampled.len());
        }
    }
    solver.shutdown();

    let snapshot = StateSnapshot {
        state_label,
        price_block,
        v4_dropped: pools
            .iter()
            .filter(|pool| !pool.persisted)
            .map(|pool| pool.component_id.clone())
            .collect(),
        pools,
        price_inputs,
        token_meta,
        subset_dropped_by_cap: selection.dropped_by_cap,
        clone_box_ms,
        arc_from_ms,
        fynd_quotes,
        fynd_quote_sample_share: sample_share,
    };
    persist_snapshot(&snapshot, &args.snapshot_out)?;
    Ok(snapshot)
}

/// Persist the snapshot as zstd JSON: serializable pool states verbatim (typetag-encoded),
/// v4-dropped components listed by id, prices/metadata/fynd-quotes alongside.
fn persist_snapshot(snapshot: &StateSnapshot, out_dir: &std::path::Path) -> Result<()> {
    let persisted = PersistedSnapshot {
        state_label: snapshot.state_label.clone(),
        price_block: snapshot.price_block,
        pools: snapshot
            .pools
            .iter()
            .filter(|pool| pool.persisted)
            .filter_map(|pool| {
                let state = serde_json::to_value(&*pool.adapter.pool as &dyn ProtocolSim).ok()?;
                Some(PersistedPool {
                    component_id: pool.component_id.clone(),
                    protocol: pool.adapter.protocol.clone(),
                    tokens: pool
                        .adapter
                        .tokens
                        .iter()
                        .map(|(address, token)| {
                            (
                                hex_addr(*address),
                                token.symbol.clone(),
                                u8::try_from(token.decimals).unwrap_or(0),
                            )
                        })
                        .collect(),
                    state,
                })
            })
            .collect(),
        v4_dropped: snapshot.v4_dropped.clone(),
        price_rationals: snapshot
            .price_inputs
            .iter()
            .map(|(address, input)| {
                (
                    hex_addr(*address),
                    input.numerator.to_string(),
                    input.denominator.to_string(),
                    input.decimals,
                )
            })
            .collect(),
        token_meta: snapshot
            .token_meta
            .iter()
            .map(|(address, (symbol, decimals))| (hex_addr(*address), symbol.clone(), *decimals))
            .collect(),
        fynd_quotes: snapshot
            .fynd_quotes
            .iter()
            .map(|(id, out)| (id.clone(), out.to_string()))
            .collect(),
        fynd_quote_sample_share: snapshot.fynd_quote_sample_share,
    };
    std::fs::create_dir_all(out_dir)?;
    let label = snapshot
        .state_label
        .replace(['/', ':'], "_");
    let path = out_dir.join(format!("base-native-{label}.json.zst"));
    let file = std::fs::File::create(&path)?;
    let mut encoder = zstd::Encoder::new(file, 3)?;
    serde_json::to_writer(&mut encoder, &persisted)?;
    encoder.finish()?;
    eprintln!("persisted snapshot to {}", path.display());
    Ok(())
}

/// Rebuild a `StateSnapshot` from a persisted file — no live connection, v4 pools absent (their
/// ids are in the manifest), fynd quotes carried over.
fn load_snapshot(path: &std::path::Path) -> Result<StateSnapshot> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening snapshot {}", path.display()))?;
    let persisted: PersistedSnapshot = serde_json::from_reader(zstd::Decoder::new(file)?)?;

    let mut address_book = PoolAddressBook::default();
    let mut pools = Vec::new();
    for pool in persisted.pools {
        let state: Box<dyn ProtocolSim> = serde_json::from_value(pool.state)
            .map_err(|e| anyhow::anyhow!("deserializing pool {}: {e}", pool.component_id))?;
        let apex_address = address_book
            .register(&pool.component_id)
            .map_err(|other| {
                anyhow::anyhow!("pool address collision: {} vs {other}", pool.component_id)
            })?;
        let mut token_map: HashMap<ApexAddress, TychoToken> = HashMap::new();
        let mut token_addresses = Vec::new();
        for (address_hex, symbol, decimals) in &pool.tokens {
            let Some(address) = apex_batch::dataset::parse_address(address_hex) else { continue };
            token_addresses.push(address);
            token_map.insert(
                address,
                TychoToken::new(
                    &tycho_simulation::tycho_common::Bytes::from(address.0.to_vec()),
                    symbol,
                    *decimals as u32,
                    0,
                    &[Some(60_000)],
                    Chain::Base,
                    100,
                ),
            );
        }
        if token_addresses.len() != 2 {
            continue;
        }
        pools.push(SnapshotPool {
            component_id: pool.component_id,
            apex_address,
            token_0: token_addresses[0],
            token_1: token_addresses[1],
            adapter: Arc::new(TychoApexPool {
                protocol: pool.protocol,
                tokens: token_map,
                pool: Arc::from(state),
            }),
            persisted: true,
        });
    }
    let mut price_inputs = HashMap::new();
    for (address_hex, numerator, denominator, decimals) in persisted.price_rationals {
        let Some(address) = apex_batch::dataset::parse_address(&address_hex) else { continue };
        let (Ok(numerator), Ok(denominator)) = (numerator.parse(), denominator.parse()) else {
            continue;
        };
        price_inputs.insert(address, TokenPriceInput { numerator, denominator, decimals });
    }
    let mut token_meta = HashMap::new();
    for (address_hex, symbol, decimals) in persisted.token_meta {
        let Some(address) = apex_batch::dataset::parse_address(&address_hex) else { continue };
        token_meta.insert(address, (symbol, decimals));
    }
    let mut fynd_quotes = HashMap::new();
    for (id, amount) in persisted.fynd_quotes {
        if let Ok(amount) = amount.parse() {
            fynd_quotes.insert(id, amount);
        }
    }
    Ok(StateSnapshot {
        state_label: persisted.state_label,
        price_block: persisted.price_block,
        pools,
        price_inputs,
        token_meta,
        subset_dropped_by_cap: 0,
        clone_box_ms: 0,
        arc_from_ms: 0,
        v4_dropped: persisted.v4_dropped,
        fynd_quotes,
        fynd_quote_sample_share: persisted.fynd_quote_sample_share,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let mut day_files: Vec<PathBuf> = std::fs::read_dir(&args.data_dir)
        .with_context(|| format!("listing {}", args.data_dir.display()))?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e == "jsonl")
        })
        .filter(|p| {
            args.days
                .iter()
                .any(|d| p.to_string_lossy().contains(d.as_str()))
        })
        .collect();
    day_files.sort();
    if day_files.is_empty() {
        bail!("no day files matched {:?} in {}", args.days, args.data_dir.display());
    }
    let days: Vec<(Vec<Intent>, HashMap<ApexAddress, f64>)> = day_files
        .iter()
        .map(|path| load_day_headline(path))
        .collect::<Result<_>>()?;
    let order_tokens: HashSet<[u8; 20]> = days
        .iter()
        .flat_map(|(intents, _)| intents.iter())
        .flat_map(|i| [i.token_in.0, i.token_out.0])
        .collect();
    eprintln!(
        "days={} intents={} distinct order tokens={}",
        days.len(),
        days.iter()
            .map(|(i, _)| i.len())
            .sum::<usize>(),
        order_tokens.len()
    );

    let all_orders: Vec<Intent> = days
        .iter()
        .flat_map(|(intents, _)| intents.iter().cloned())
        .collect();
    let snapshot = match &args.snapshot {
        Some(path) => {
            let snapshot = load_snapshot(path)?;
            eprintln!(
                "loaded persisted snapshot {} ({} pools, v4 dropped: {})",
                path.display(),
                snapshot.pools.len(),
                snapshot.v4_dropped.len()
            );
            snapshot
        }
        None => take_snapshot(&args, &order_tokens, &all_orders).await?,
    };
    eprintln!(
        "snapshot: {} pools, {} priced tokens, clone_box={}ms arc_from={}ms",
        snapshot.pools.len(),
        snapshot.price_inputs.len(),
        snapshot.clone_box_ms,
        snapshot.arc_from_ms
    );

    let intent_usd: f64 = days
        .iter()
        .flat_map(|(intents, _)| intents.iter())
        .map(|i| i.usd)
        .sum();
    // Pool-implied quotes at the snapshot state anchor the Current cells' floors.
    let mut pools_by_token: HashMap<ApexAddress, Vec<usize>> = HashMap::new();
    for (index, pool) in snapshot.pools.iter().enumerate() {
        pools_by_token
            .entry(pool.token_0)
            .or_default()
            .push(index);
        pools_by_token
            .entry(pool.token_1)
            .or_default()
            .push(index);
    }
    let quote_started = Instant::now();
    let current_quotes: HashMap<String, alloy::primitives::U256> = all_orders
        .iter()
        .filter_map(|order| {
            pool_implied_quote(order, &snapshot, &pools_by_token).map(|q| (order.id.clone(), q))
        })
        .collect();
    eprintln!(
        "pool-implied current quotes: {} of {} orders ({}ms)",
        current_quotes.len(),
        all_orders.len(),
        quote_started.elapsed().as_millis()
    );

    // v4-inclusion delta: a live run solves each cell twice — full in-memory subset vs the
    // serializable-only subset a from-disk rerun would see. From-disk runs only have the latter.
    let pool_scopes: Vec<bool> = if snapshot.v4_dropped.is_empty() || args.snapshot.is_some() {
        vec![true]
    } else {
        vec![false, true]
    };

    let mut cells: Vec<CellResult> = Vec::new();
    for window in WINDOWS {
        for &limit_bps in &args.limit_bps {
            for anchor in [Anchor::Original, Anchor::Current] {
                for &serializable_only in &pool_scopes {
                    for with_pools in [true, false] {
                        if !with_pools && serializable_only != *pool_scopes.last().expect("nonempty")
                        {
                            // The no-pools cell is scope-invariant; run it once per anchor.
                            continue;
                        }
                        let started = Instant::now();
                        let mut counters = Counters::default();
                        let mut matched = 0.0f64;
                        let mut surplus = 0.0f64;
                        let mut pool_wei = 0.0f64;
                        let mut notional_wei = 0.0f64;
                        let mut fynd_bps: Vec<f64> = Vec::new();
                        let mut fynd_usd_delta = 0.0f64;
                        let mut solve_times: Vec<u128> = Vec::new();
                        for (intents, day_price) in &days {
                            let mut by_window: BTreeMap<u64, Vec<Intent>> = BTreeMap::new();
                            for intent in intents {
                                by_window
                                    .entry(intent.block / window)
                                    .or_default()
                                    .push(intent.clone());
                            }
                            for batch in by_window.into_values() {
                                let outcome = solve_batch(
                                    &batch,
                                    &snapshot,
                                    day_price,
                                    limit_bps,
                                    with_pools,
                                    anchor,
                                    serializable_only,
                                    &current_quotes,
                                    &mut solve_times,
                                );
                                matched += outcome.matched_usd;
                                surplus += outcome.surplus_usd;
                                pool_wei += outcome.pool_cleared_wei;
                                notional_wei += outcome.order_notional_wei;
                                fynd_bps.extend(outcome.fynd_bps);
                                fynd_usd_delta += outcome.fynd_usd_delta;
                                counters.absorb(&outcome.counters);
                            }
                        }
                        solve_times.sort_unstable();
                        fynd_bps.sort_by(|a, b| {
                            a.partial_cmp(b)
                                .expect("finite bps")
                        });
                        let percentile = |p: f64| -> u128 {
                            if solve_times.is_empty() {
                                0
                            } else {
                                solve_times[((solve_times.len() - 1) as f64 * p) as usize]
                            }
                        };
                        let internalization = (notional_wei > 0.0 && with_pools)
                            .then(|| (1.0 - pool_wei / (2.0 * notional_wei)).clamp(0.0, 1.0));
                        let cell = CellResult {
                            window_blocks: window,
                            limit_bps,
                            cell: if with_pools { "pools" } else { "no_pools" }.to_string(),
                            anchor: match anchor {
                                Anchor::Original => "original",
                                Anchor::Current => "current",
                            }
                            .to_string(),
                            pool_scope: if serializable_only { "serializable" } else { "full" }
                                .to_string(),
                            intent_usd,
                            apex_matched_usd: matched,
                            apex_matched_pct: 100.0 * matched / intent_usd,
                            apex_surplus_usd: surplus,
                            pool_cleared_wei: pool_wei,
                            order_notional_wei: notional_wei,
                            internalization_share: internalization,
                            solve_ms_p50: percentile(0.5),
                            solve_ms_p90: percentile(0.9),
                            solve_ms_max: percentile(1.0),
                            fynd_compared: counters.fynd_compared,
                            fynd_median_bps: if fynd_bps.is_empty() {
                                0.0
                            } else {
                                fynd_bps[fynd_bps.len() / 2]
                            },
                            fynd_mean_bps: if fynd_bps.is_empty() {
                                0.0
                            } else {
                                fynd_bps.iter().sum::<f64>() / fynd_bps.len() as f64
                            },
                            fynd_apex_ge_share: if fynd_bps.is_empty() {
                                0.0
                            } else {
                                fynd_bps
                                    .iter()
                                    .filter(|b| **b >= 0.0)
                                    .count() as f64 /
                                    fynd_bps.len() as f64
                            },
                            fynd_usd_delta,
                            counters,
                            wall_ms: started.elapsed().as_millis(),
                        };
                        eprintln!(
                            "w={window:>3} bps={limit_bps:>3} {}/{}/{}: matched=${:>11.0} \
                             ({:.3}%) surplus=${:>9.2} intern={:?} fynd(n={} med={:+.1}bps) \
                             solves p50/p90/max={}/{}/{}ms",
                            cell.anchor,
                            cell.cell,
                            cell.pool_scope,
                            cell.apex_matched_usd,
                            cell.apex_matched_pct,
                            cell.apex_surplus_usd,
                            cell.internalization_share,
                            cell.fynd_compared,
                            cell.fynd_median_bps,
                            cell.solve_ms_p50,
                            cell.solve_ms_p90,
                            cell.solve_ms_max,
                        );
                        cells.push(cell);
                    }
                }
            }
        }
    }

    std::fs::create_dir_all(&args.out_dir)?;
    let meta = SnapshotMeta {
        state_label: snapshot.state_label.clone(),
        price_block: snapshot.price_block,
        pool_count: snapshot.pools.len(),
        priced_tokens: snapshot.price_inputs.len(),
        subset_dropped_by_cap: snapshot.subset_dropped_by_cap,
        clone_box_ms: snapshot.clone_box_ms,
        arc_from_ms: snapshot.arc_from_ms,
        v4_dropped: snapshot.v4_dropped.clone(),
        fynd_quotes_taken: snapshot.fynd_quotes.len(),
        fynd_quote_sample_share: snapshot.fynd_quote_sample_share,
        current_quotes: current_quotes.len(),
    };
    std::fs::write(
        args.out_dir.join("stage3_results.json"),
        serde_json::to_vec_pretty(&serde_json::json!({ "snapshot": meta, "cells": cells }))?,
    )?;
    eprintln!(
        "wrote {}",
        args.out_dir
            .join("stage3_results.json")
            .display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single order routed A→C through two pools: per-token netting must value the pool
    /// exposure at ≈ 2 × notional, i.e. internalization ≈ 0 — the intermediate token B nets
    /// out (grill r2 F5's unit test).
    #[test]
    fn test_multi_hop_single_order_internalization_is_zero() {
        let (a, b, c) = (ApexAddress([1u8; 20]), ApexAddress([2u8; 20]), ApexAddress([3u8; 20]));
        // Pool 1 sells B for A; pool 2 sells C for B. Equal values: 100 A = 200 B = 50 C.
        let clearings = vec![
            (b, a, ApexU256::from(200u64), ApexU256::from(100u64)),
            (c, b, ApexU256::from(50u64), ApexU256::from(200u64)),
        ];
        let wei_prices = HashMap::from([(a, 1.0f64), (b, 0.5), (c, 2.0)]);
        let exposure = net_pool_exposure_wei(clearings.into_iter(), &wei_prices);
        // Net B = +200 − 200 = 0; net A = +100 (into pools); net C = −50 (out of pools).
        // |100|·1.0 + |50|·2.0 = 200 wei = 2 × the order's 100-wei notional.
        assert!((exposure - 200.0).abs() < 1e-9, "{exposure}");
        let internalization = (1.0f64 - exposure / (2.0 * 100.0)).clamp(0.0, 1.0);
        assert!(internalization.abs() < 1e-9, "{internalization}");
    }

    /// Two perfectly crossing orders never touch pools: exposure 0, internalization 1.
    #[test]
    fn test_pure_cross_internalization_is_one() {
        let exposure = net_pool_exposure_wei(std::iter::empty(), &HashMap::new());
        assert_eq!(exposure, 0.0);
        let internalization = (1.0f64 - exposure / (2.0 * 100.0)).clamp(0.0, 1.0);
        assert!((internalization - 1.0).abs() < 1e-9);
    }
}

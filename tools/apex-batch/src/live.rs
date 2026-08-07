//! The live batch solve the hindsight monitor dispatches at its pre-advance seam.
//!
//! This is the plan's "one shared input-builder": the monitor maps its decoded trades and cloned
//! pool states into [`LiveBatchInput`] and every APEX-facing decision — component partitioning,
//! admission preconditions, price scaling, limit construction, per-component solving, and
//! reconciliation — lives here, in the same crate the offline runners use. One code path, two
//! state sources.
//!
//! The solve contract mirrors the offline reference (`bin/stage3.rs::solve_batch`), with the
//! reporting inverted: offline aggregates dollars, live reports per-order statuses and raw
//! amounts and leaves economics to the offline join against the comparisons JSONL (which already
//! carries each order's Fynd quote at the same two block states — the comparability invariant).
//!
//! Every precondition failure is a counted decline, never a panic: APEX indexes its token maps
//! directly and divides by prices, so the input contract (full token closure priced, nonzero
//! amounts and limits, unique ids) is enforced before the call. The per-component `catch_unwind`
//! is the last resort the plan allows, and the stage's worker-level catch above it is the backstop.

use std::{
    collections::{HashMap, HashSet},
    panic::{catch_unwind, AssertUnwindSafe},
    sync::Arc,
    time::{Duration, Instant},
};

use alloy::primitives::U256;
use apex_solver::{
    core::{
        pools::{custom::ApexPool, Pool, PoolMetadata},
        ApexConfig, Fraction, LimitOrder, StepStrategy, Token as ApexToken, TradingPair,
    },
    run_apex_with_config,
    types::Address as ApexAddress,
};
use num_bigint::BigUint;

use crate::{
    adapter::{from_apex_u256, to_apex_u256, TychoApexPool},
    prices::{batch_value_wei, build_apex_prices, TokenPriceInput, MAX_PRECISION_INCREASES},
    scaling::{Scaled18, TokenScale},
};

/// One decoded trade as a live batch order. Amounts are raw (native-decimal) units; the lift into
/// APEX's 18-decimal space happens inside the solve, through the declining scaling module.
#[derive(Debug, Clone)]
pub struct LiveOrder {
    /// `{tx_hash}:{tx_index}` — joins the result back to the comparisons JSONL record.
    pub id: String,
    pub token_in: ApexAddress,
    pub token_out: ApexAddress,
    pub amount_in_raw: U256,
    /// The order's floor: the extracted on-chain limit where the decoder found one, else the
    /// capture module's synthetic fallback. Zero declines the order (`zero_limit`).
    pub min_out_raw: U256,
}

/// One cloned pool state, already wrapped for APEX. The monitor clones at block time, so queue
/// delay never changes what state a solve sees.
#[derive(Clone)]
pub struct LivePool {
    /// The tycho component id this pool came from. `apex_address` is keccak-truncated for
    /// non-address component ids (32-byte Uniswap v4 pool ids), so it cannot be resolved back to
    /// a pool after the fact — the recorded clearings carry this instead.
    pub component_id: String,
    pub apex_address: ApexAddress,
    pub token_0: ApexAddress,
    pub token_1: ApexAddress,
    pub adapter: Arc<TychoApexPool>,
}

impl std::fmt::Debug for LivePool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LivePool")
            .field("component_id", &self.component_id)
            .field("apex_address", &self.apex_address)
            .field("token_0", &self.token_0)
            .field("token_1", &self.token_1)
            .finish_non_exhaustive()
    }
}

/// Everything one bracket's solve needs, cloned from live state at dispatch time.
#[derive(Debug, Clone, Default)]
pub struct LiveBatchInput {
    pub orders: Vec<LiveOrder>,
    pub pools: Vec<LivePool>,
    /// Exact price rationals for (at least) the token closure, from the solver's derived data.
    pub price_inputs: HashMap<ApexAddress, TokenPriceInput>,
    /// Symbol and decimals per token, from the solver's token metadata.
    pub token_meta: HashMap<ApexAddress, (String, u8)>,
}

/// Where one order ended up, per the input-vs-clearing reconciliation.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderStatus {
    Filled {
        bought_raw: U256,
        fill_ratio: f64,
    },
    PartiallyFilled {
        bought_raw: U256,
        fill_ratio: f64,
    },
    /// APEX returned a clearing with zero sold amount, or none from a solve that converged — the
    /// limit was the binding constraint at prices the search had finished refining.
    UnfilledAtLimit,
    /// Evaluated, but only against best-so-far prices: the deadline fired mid-cluster, APEX
    /// cleared anyway, and this order did not cross. Distinct from [`Self::UnfilledAtLimit`]
    /// because the rejection is provisional — converged prices might have crossed it — and
    /// distinct from [`Self::ClusterCut`] because the order WAS priced.
    UnfilledAtBestSoFar,
    /// The order's trading cluster never started: the deadline landed between clusters, so nothing
    /// about this order was evaluated at all. Only reachable when APEX partitions a component into
    /// several trading clusters; a single-cluster component always clears at best-so-far instead.
    ClusterCut,
    /// The order's component errored or panicked inside APEX; nothing about this order's own
    /// economics can be concluded.
    ComponentErrored,
    /// Declined before the solve; the reason names the admission counter it incremented.
    Excluded(&'static str),
}

/// A single-order control solve (same partitioning, same pools, own budget).
#[derive(Debug, Clone)]
pub struct SingleResult {
    pub id: String,
    /// Raw bought amount when the single-order solve filled; `None` when it did not (unfilled,
    /// errored, or the component had no pools to fill against).
    pub bought_raw: Option<U256>,
}

/// Admission and solve counters for one bracket solve — mirrors the offline runner's taxonomy.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct LiveCounters {
    pub orders_in: u64,
    pub unknown_decimals: u64,
    pub token_unpriced: u64,
    pub price_underflow: u64,
    pub scale_overflow: u64,
    pub zero_limit_excluded: u64,
    pub duplicate_order_id: u64,
    pub singles_skipped: u64,
    pub pool_unpriced: u64,
    pub pools_in_scope: u64,
    pub components_solved: u64,
    pub component_errored_orders: u64,
    pub component_errors: HashMap<String, u64>,
    pub solver_panics: u64,
    pub deadline_fired_components: u64,
    pub filled: u64,
    pub partially_filled: u64,
    pub unfilled_at_limit: u64,
    /// Priced and rejected, but only against best-so-far prices — see
    /// [`OrderStatus::UnfilledAtBestSoFar`]. Tracked apart from `unfilled_at_limit` so a
    /// provisional rejection is never read as a converged one.
    pub unfilled_at_best_so_far: u64,
    pub cluster_cut: u64,
}

/// What one component's APEX solve cleared, recorded verbatim so the economics can be recomputed
/// offline without re-solving. APEX has no encoder, so the clearing-price vector plus the
/// per-pool and per-order clearings are the recoverable equivalent of a settlement's calldata.
#[derive(Debug, Clone, Default)]
pub struct ComponentClearing {
    /// Whether THIS component's price search hit its deadline. The bracket-level counter cannot
    /// answer that for a multi-component job, which makes any offline split of surplus by
    /// convergence impossible — and the deadline population is where nearly all the notional is.
    pub deadline_fired: bool,
    /// How many orders and pools the search had to price, so a split by convergence can be read
    /// against component size rather than confounded by it.
    pub orders_in: usize,
    pub pools_in_scope: usize,
    /// The uniform clearing price per token, in APEX's 18-decimal price space.
    pub clearing_prices: Vec<(ApexAddress, U256)>,
    pub pool_clearings: Vec<PoolClearingRecord>,
    pub order_clearings: Vec<OrderClearingRecord>,
    /// What the solve actually spent its budget on. APEX fills these regardless of
    /// `collect_metrics` (which only controls its CSV dump), and they are the difference between
    /// knowing a solve was slow and knowing why.
    pub solve_metrics: ComponentSolveMetrics,
}

/// Where one component solve's time went, from APEX's own instrumentation.
#[derive(Debug, Clone, Default)]
pub struct ComponentSolveMetrics {
    /// Parallel `register_supply` calls — effectively the price-search iteration count.
    pub supply_calls: u64,
    pub supply_wall_ms: f64,
    /// Summed per-worker busy time over the section's wall time. ~1 means the supply
    /// registration ran serially however many workers were configured.
    pub effective_parallelism_avg: f64,
    /// Rayon pool construction, which happens per cluster once `max_workers > 1`.
    pub pool_builds: u64,
    pub pool_build_ms: f64,
    pub workers: u64,
    pub supply_cache_hits: u64,
    pub supply_cache_misses: u64,
}

/// One pool's leg of a component's clearing, with the pool resolved back to its tycho identity.
#[derive(Debug, Clone)]
pub struct PoolClearingRecord {
    pub apex_address: ApexAddress,
    /// The tycho component id, when the solve's pool set still knows this address. Absent only if
    /// APEX returned a clearing for a pool that was not handed to it.
    pub component_id: Option<String>,
    pub protocol: Option<String>,
    pub sell_token: ApexAddress,
    pub buy_token: ApexAddress,
    pub sold_amount: U256,
    pub bought_amount: U256,
    pub surplus: U256,
    pub fee: Option<U256>,
}

/// One limit order's leg of a component's clearing, keyed by the order id the monitor assigned
/// (`{tx_hash}:{tx_index}`), so it joins the comparisons stream directly.
#[derive(Debug, Clone)]
pub struct OrderClearingRecord {
    pub id: String,
    pub owner: ApexAddress,
    pub sell_token: ApexAddress,
    pub buy_token: ApexAddress,
    pub sold_amount: U256,
    pub bought_amount: U256,
}

/// One bracket's outcome: per-order statuses, the singles control, counters, and per-component
/// solve wall times (the shadow run's primary series).
#[derive(Debug, Default)]
pub struct LiveBatchReport {
    pub statuses: Vec<(String, OrderStatus)>,
    pub singles: Vec<SingleResult>,
    pub counters: LiveCounters,
    pub component_solve_ms: Vec<u128>,
    /// One entry per component APEX actually returned a result for, in solve order. The singles
    /// control's own solves are not recorded here — they are a per-order counterfactual, not part
    /// of the batch's clearing.
    pub components: Vec<ComponentClearing>,
    /// Σ|per-token net pool exposure| in wei, and the notional actually sold into clearings, both
    /// summed over this bracket's components. Their ratio is the internalization share:
    /// `1 − pool_cleared_wei / (2 × filled_notional_wei)`. The denominator is the REALIZED
    /// notional, not the submitted one — with submitted notional the share degrades into a
    /// fill-rate proxy, since every skipped or cut component would sit in the denominator with no
    /// pool flow able to reach the numerator.
    pub pool_cleared_wei: f64,
    pub filled_notional_wei: f64,
}

/// Whether a block's decoded trades can form a batch worth dispatching: at least two orders
/// sharing a token (the connectivity pre-check's definition of an eligible block). Pool-mediated
/// linking is deliberately not consulted here — it would make nearly every block eligible and
/// the study's eligibility notion is order connectivity.
pub fn batch_eligible(order_token_pairs: &[(ApexAddress, ApexAddress)]) -> bool {
    if order_token_pairs.len() < 2 {
        return false;
    }
    let mut token_order_counts: HashMap<ApexAddress, usize> = HashMap::new();
    for (token_in, token_out) in order_token_pairs {
        // A token appearing twice inside ONE order (in == out) must not make the block eligible.
        for token in HashSet::from([*token_in, *token_out]) {
            *token_order_counts
                .entry(token)
                .or_default() += 1;
        }
    }
    token_order_counts
        .values()
        .any(|&count| count >= 2)
}

/// Solve one bracket: partition, admit, solve per component, reconcile.
///
/// `component_budget` is APEX's search deadline per component solve, stamped at each component's
/// solve start (the plan's 1 s live budget). `single_budget` caps each single-order control solve.
/// The worker-level stage budget above this call is an occupancy envelope, not the solve budget.
pub fn solve_live_batch(
    input: &LiveBatchInput,
    component_budget: Duration,
    single_budget: Duration,
    run_singles: bool,
) -> LiveBatchReport {
    let mut report = LiveBatchReport::default();
    let counters = &mut report.counters;

    // Admission: representable decimals and priced tokens, before any partitioning.
    let mut admitted: Vec<&LiveOrder> = Vec::with_capacity(input.orders.len());
    for order in &input.orders {
        counters.orders_in += 1;
        let representable = |token: &ApexAddress| {
            input
                .token_meta
                .get(token)
                .is_some_and(|(_, decimals)| TokenScale::new(*decimals).is_ok())
        };
        if !representable(&order.token_in) || !representable(&order.token_out) {
            counters.unknown_decimals += 1;
            report
                .statuses
                .push((order.id.clone(), OrderStatus::Excluded("unknown_decimals")));
            continue;
        }
        if !input
            .price_inputs
            .contains_key(&order.token_in) ||
            !input
                .price_inputs
                .contains_key(&order.token_out)
        {
            counters.token_unpriced += 1;
            report
                .statuses
                .push((order.id.clone(), OrderStatus::Excluded("token_unpriced")));
            continue;
        }
        admitted.push(order);
    }
    if admitted.is_empty() {
        return report;
    }

    // Union-find over order tokens ∪ pool edges: identical partitioning for the batch and the
    // singles control (comparability invariant, plan v3.1 item C).
    let mut token_index: HashMap<ApexAddress, usize> = HashMap::new();
    for order in &admitted {
        for token in [order.token_in, order.token_out] {
            let next = token_index.len();
            token_index.entry(token).or_insert(next);
        }
    }
    let mut parent: Vec<usize> = (0..token_index.len()).collect();
    fn find(parent: &mut [usize], mut index: usize) -> usize {
        while parent[index] != index {
            parent[index] = parent[parent[index]];
            index = parent[index];
        }
        index
    }
    for order in &admitted {
        let in_index = token_index[&order.token_in];
        let out_index = token_index[&order.token_out];
        let (in_root, out_root) = (find(&mut parent, in_index), find(&mut parent, out_index));
        parent[in_root] = out_root;
    }
    let relevant_pools: Vec<&LivePool> = input
        .pools
        .iter()
        .filter(|pool| {
            token_index.contains_key(&pool.token_0) || token_index.contains_key(&pool.token_1)
        })
        .collect();
    for pool in &relevant_pools {
        if let (Some(&token_0_index), Some(&token_1_index)) =
            (token_index.get(&pool.token_0), token_index.get(&pool.token_1))
        {
            let (root_0, root_1) =
                (find(&mut parent, token_0_index), find(&mut parent, token_1_index));
            parent[root_0] = root_1;
        }
    }

    let mut components: std::collections::BTreeMap<usize, Vec<&LiveOrder>> =
        std::collections::BTreeMap::new();
    for order in &admitted {
        let root = find(&mut parent, token_index[&order.token_in]);
        components
            .entry(root)
            .or_default()
            .push(order);
    }

    let mut seen_ids: HashSet<String> = HashSet::new();
    for (root, component_orders) in components {
        let component_pools: Vec<&LivePool> = relevant_pools
            .iter()
            .filter(|pool| {
                [pool.token_0, pool.token_1]
                    .iter()
                    .any(|t| {
                        token_index
                            .get(t)
                            .is_some_and(|&i| find(&mut parent, i) == root)
                    })
            })
            .filter(|pool| {
                let priced = input
                    .price_inputs
                    .contains_key(&pool.token_0) &&
                    input
                        .price_inputs
                        .contains_key(&pool.token_1) &&
                    input
                        .token_meta
                        .contains_key(&pool.token_0) &&
                    input
                        .token_meta
                        .contains_key(&pool.token_1);
                if !priced {
                    counters.pool_unpriced += 1;
                }
                priced
            })
            .copied()
            .collect();
        counters.pools_in_scope += component_pools.len() as u64;

        if component_orders.len() < 2 && component_pools.is_empty() {
            counters.singles_skipped += component_orders.len() as u64;
            for order in &component_orders {
                report
                    .statuses
                    .push((order.id.clone(), OrderStatus::Excluded("single_component")));
            }
            continue;
        }

        let exposure = solve_component(
            &component_orders,
            &component_pools,
            input,
            component_budget,
            &mut seen_ids,
            &mut report.statuses,
            counters,
            &mut report.component_solve_ms,
            &mut report.components,
        );
        report.pool_cleared_wei += exposure.pool_cleared_wei;
        report.filled_notional_wei += exposure.filled_notional_wei;

        if run_singles {
            let mut singled_ids: HashSet<&str> = HashSet::new();
            for order in &component_orders {
                // Singles only for orders the batch cell admitted — the pairing rule needs both
                // cells' verdicts on the same order set. The admission lookup is by id, so a
                // duplicate instance of an admitted id must not earn a second control solve.
                let admitted_to_batch = report
                    .statuses
                    .iter()
                    .any(|(id, status)| {
                        id == &order.id && !matches!(status, OrderStatus::Excluded(_))
                    });
                if !admitted_to_batch || !singled_ids.insert(order.id.as_str()) {
                    continue;
                }
                let mut single_statuses = Vec::new();
                let mut single_counters = LiveCounters::default();
                let mut single_times = Vec::new();
                let mut single_clearings = Vec::new();
                // The singles control's own exposure is not part of the batch's internalization,
                // and its clearings are a counterfactual rather than the batch's own.
                let _ = solve_component(
                    &[order],
                    &component_pools,
                    input,
                    single_budget,
                    &mut HashSet::new(),
                    &mut single_statuses,
                    &mut single_counters,
                    &mut single_times,
                    &mut single_clearings,
                );
                let bought_raw = single_statuses
                    .into_iter()
                    .find(|(id, _)| id == &order.id)
                    .and_then(|(_, status)| match status {
                        OrderStatus::Filled { bought_raw, .. } |
                        OrderStatus::PartiallyFilled { bought_raw, .. } => Some(bought_raw),
                        _ => None,
                    });
                report
                    .singles
                    .push(SingleResult { id: order.id.clone(), bought_raw });
            }
        }
    }
    report
}

/// Workers APEX may use *inside* one component solve.
///
/// This was 1, which meant `MarketRouter::setup_workers` early-returned and every price-search
/// iteration registered supply over all ~280 pools serially on one thread. Measured consequence: a
/// deadline-exited search completed ~39 iterations at a 1.5 s budget and ~36 at 20 s — the budget
/// never bought iterations because the cost is per-iteration, not per-search. The stage's own
/// worker count is lowered to match, so total threads stay near the cgroup's core allowance.
const APEX_MAX_WORKERS: usize = 4;

/// Iteration cap for the price search, raised from APEX's default 1000 to match turbine's
/// production tuning — the mixed strategy below only pays off if it is allowed to keep stepping.
const PRICE_SEARCH_MAX_ITERATIONS: u32 = 3_000;

/// Iterations allowed at the minimum step size before the search gives up, lowered from APEX's
/// default 30. Once the step cannot shrink further, extra iterations thrash rather than converge;
/// the budget they free is better spent on the `Top(n)` fallbacks.
const PRICE_SEARCH_MAX_IT_AT_MIN_STEP: u32 = 10;

/// The step ladder turbine runs in production (`mixed_strategy`): try a full-vector step first,
/// and when it stops improving fall back to moving only the 2 — then 1 — tokens with the largest
/// supply/demand imbalance. APEX's default is `AllTokens` alone, which on a component carrying
/// hundreds of tokens gets stuck in local minima the `Top(n)` steps escape.
fn mixed_step_strategies() -> Vec<StepStrategy> {
    vec![StepStrategy::AllTokens, StepStrategy::Top(2), StepStrategy::Top(1)]
}

/// One component's contribution to the bracket's internalization accounting.
#[derive(Debug, Default, Clone, Copy)]
struct ComponentExposure {
    pool_cleared_wei: f64,
    filled_notional_wei: f64,
}

/// Build and run one APEX call for one component; push per-order statuses.
#[allow(clippy::too_many_arguments)]
fn solve_component(
    component_orders: &[&LiveOrder],
    component_pools: &[&LivePool],
    input: &LiveBatchInput,
    budget: Duration,
    seen_ids: &mut HashSet<String>,
    statuses: &mut Vec<(String, OrderStatus)>,
    counters: &mut LiveCounters,
    solve_ms: &mut Vec<u128>,
    clearings_out: &mut Vec<ComponentClearing>,
) -> ComponentExposure {
    let mut exposure = ComponentExposure::default();
    // Token closure = order tokens ∪ pool tokens, all priced (the two_hops precondition).
    let mut closure: HashSet<ApexAddress> = HashSet::new();
    for order in component_orders {
        closure.insert(order.token_in);
        closure.insert(order.token_out);
    }
    for pool in component_pools {
        closure.insert(pool.token_0);
        closure.insert(pool.token_1);
    }
    let price_inputs: HashMap<ApexAddress, TokenPriceInput> = closure
        .iter()
        .filter_map(|token| {
            input
                .price_inputs
                .get(token)
                .map(|price| (*token, price.clone()))
        })
        .collect();
    let batch_wei = batch_value_wei(
        component_orders.iter().map(|order| {
            (order.token_in, BigUint::from_bytes_le(&order.amount_in_raw.to_le_bytes::<32>()))
        }),
        &price_inputs,
    );
    let price_map = build_apex_prices(&price_inputs, &batch_wei);
    let unpriced_or_underflow: HashSet<ApexAddress> = price_map
        .price_underflow
        .iter()
        .chain(price_map.unpriced.iter())
        .copied()
        .collect();

    let mut apex_tokens: HashMap<ApexAddress, ApexToken> = HashMap::new();
    for token in &closure {
        if unpriced_or_underflow.contains(token) {
            continue;
        }
        let Some((symbol, decimals)) = input.token_meta.get(token) else { continue };
        apex_tokens.insert(*token, ApexToken::new(*token, symbol, *decimals));
    }

    let mut limit_orders: HashMap<(ApexAddress, ApexAddress), Vec<LimitOrder>> = HashMap::new();
    let mut order_amount18: Vec<(&LiveOrder, f64)> = Vec::new();
    for order in component_orders {
        if unpriced_or_underflow.contains(&order.token_in) ||
            unpriced_or_underflow.contains(&order.token_out)
        {
            counters.price_underflow += 1;
            statuses.push((order.id.clone(), OrderStatus::Excluded("price_underflow")));
            continue;
        }
        // Admission already proved these constructible; a failure here is a race with nothing,
        // so decline defensively rather than unwrap.
        let (Ok(scale_in), Ok(scale_out)) = (
            TokenScale::new(input.token_meta[&order.token_in].1),
            TokenScale::new(input.token_meta[&order.token_out].1),
        ) else {
            counters.unknown_decimals += 1;
            statuses.push((order.id.clone(), OrderStatus::Excluded("unknown_decimals")));
            continue;
        };
        let (Ok(amount18), Ok(min_out18)) =
            (scale_in.scale_up(order.amount_in_raw), scale_out.scale_up(order.min_out_raw))
        else {
            counters.scale_overflow += 1;
            statuses.push((order.id.clone(), OrderStatus::Excluded("scale_overflow")));
            continue;
        };
        if amount18.0.is_zero() || min_out18.0.is_zero() {
            counters.zero_limit_excluded += 1;
            statuses.push((order.id.clone(), OrderStatus::Excluded("zero_amount_or_limit")));
            continue;
        }
        if !seen_ids.insert(order.id.clone()) {
            counters.duplicate_order_id += 1;
            statuses.push((order.id.clone(), OrderStatus::Excluded("duplicate_order_id")));
            continue;
        }
        let pair = TradingPair::new(apex_tokens[&order.token_in], apex_tokens[&order.token_out]);
        limit_orders
            .entry(pair.addresses())
            .or_default()
            .push(LimitOrder::new(
                to_apex_u256(amount18.0),
                Fraction::new(to_apex_u256(min_out18.0), to_apex_u256(amount18.0)),
                order.id.clone(),
                ApexAddress([0u8; 20]),
            ));
        order_amount18.push((order, u256_to_f64(amount18.0)));
    }
    if order_amount18.is_empty() {
        return exposure;
    }

    let pools: Vec<Pool> = component_pools
        .iter()
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
        enable_two_hops: !pools.is_empty(),
        max_workers: APEX_MAX_WORKERS,
        collect_metrics: false,
        // The search deadline starts at THIS component's solve start, never earlier — an
        // already-expired absolute deadline makes APEX return silently empty.
        deadline: Some(Instant::now() + budget),
        ..ApexConfig::default()
    };
    config
        .price_search_config
        .max_precision_increases = MAX_PRECISION_INCREASES;
    config
        .price_search_config
        .max_iterations = PRICE_SEARCH_MAX_ITERATIONS;
    config
        .price_search_config
        .max_it_at_min_step = PRICE_SEARCH_MAX_IT_AT_MIN_STEP;
    config
        .price_search_config
        .iteration_strategies = mixed_step_strategies();

    counters.components_solved += 1;
    let tokens: Vec<ApexToken> = apex_tokens.values().copied().collect();
    let solve_started = Instant::now();
    let solve = catch_unwind(AssertUnwindSafe(|| {
        run_apex_with_config(
            tokens,
            price_map
                .prices
                .iter()
                .map(|(token, price)| (*token, *price))
                .collect(),
            limit_orders.clone(),
            HashMap::new(),
            pools,
            config,
        )
    }));
    solve_ms.push(solve_started.elapsed().as_millis());
    let result = match solve {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            counters.component_errored_orders += order_amount18.len() as u64;
            let kind = component_error_kind(&error);
            *counters
                .component_errors
                .entry(kind.to_string())
                .or_default() += 1;
            for (order, _) in &order_amount18 {
                statuses.push((order.id.clone(), OrderStatus::ComponentErrored));
            }
            return exposure;
        }
        Err(_) => {
            counters.solver_panics += 1;
            counters.component_errored_orders += order_amount18.len() as u64;
            for (order, _) in &order_amount18 {
                statuses.push((order.id.clone(), OrderStatus::ComponentErrored));
            }
            return exposure;
        }
    };
    if result.deadline_fired {
        counters.deadline_fired_components += 1;
    }

    // The clearing itself, recorded before it is collapsed into the internalization scalars.
    // APEX addresses pools by a keccak-truncated id, so each clearing is resolved back to the
    // tycho component that produced it while that mapping is still in scope.
    let pool_identity: HashMap<ApexAddress, &LivePool> = component_pools
        .iter()
        .map(|pool| (pool.apex_address, *pool))
        .collect();
    clearings_out.push(ComponentClearing {
        deadline_fired: result.deadline_fired,
        orders_in: order_amount18.len(),
        pools_in_scope: component_pools.len(),
        clearing_prices: result
            .clearing_prices
            .iter()
            .map(|(token, price)| (*token, from_apex_u256(*price)))
            .collect(),
        pool_clearings: result
            .pool_clearings
            .iter()
            .map(|clearing| {
                let identity = pool_identity.get(&clearing.address);
                PoolClearingRecord {
                    apex_address: clearing.address,
                    component_id: identity.map(|pool| pool.component_id.clone()),
                    protocol: identity.map(|pool| pool.adapter.protocol.clone()),
                    sell_token: clearing.pair.sell_token.address,
                    buy_token: clearing.pair.buy_token.address,
                    sold_amount: from_apex_u256(clearing.sold_amount),
                    bought_amount: from_apex_u256(clearing.bought_amount),
                    surplus: from_apex_u256(clearing.surplus),
                    fee: clearing.fee.map(from_apex_u256),
                }
            })
            .collect(),
        order_clearings: result
            .limit_order_clearings
            .iter()
            .map(|clearing| OrderClearingRecord {
                id: clearing.id.clone(),
                owner: clearing.owner,
                sell_token: clearing.sell_token,
                buy_token: clearing.buy_token,
                sold_amount: from_apex_u256(clearing.sold_amount),
                bought_amount: from_apex_u256(clearing.bought_amount),
            })
            .collect(),
        solve_metrics: ComponentSolveMetrics {
            supply_calls: result.metrics.supply_calls,
            supply_wall_ms: result.metrics.supply_wall_ms,
            effective_parallelism_avg: result
                .metrics
                .effective_parallelism
                .average,
            pool_builds: result.metrics.pool_builds,
            pool_build_ms: result.metrics.pool_build_ms,
            workers: result.metrics.workers,
            supply_cache_hits: result.metrics.supply_cache_hits,
            supply_cache_misses: result.metrics.supply_cache_misses,
        },
    });

    // Internalization inputs: net pool exposure over this solve's per-hop clearings, valued in
    // wei, against the notional the orders actually sold (accumulated in the fill arm below).
    let wei_prices: HashMap<ApexAddress, f64> = price_inputs
        .iter()
        .map(|(token, price)| (*token, crate::prices::wei_per_unit18(price)))
        .collect();
    exposure.pool_cleared_wei += crate::prices::net_pool_exposure_wei(
        result
            .pool_clearings
            .iter()
            .map(|clearing| {
                (
                    clearing.pair.sell_token.address,
                    clearing.pair.buy_token.address,
                    clearing.sold_amount,
                    clearing.bought_amount,
                )
            }),
        &wei_prices,
    );

    let clearings: HashMap<&str, _> = result
        .limit_order_clearings
        .iter()
        .map(|clearing| (clearing.id.as_str(), clearing))
        .collect();
    for (order, amount18) in &order_amount18 {
        let status = match clearings.get(order.id.as_str()) {
            Some(clearing) if !clearing.sold_amount.is_zero() => {
                let fill_ratio =
                    u256_to_f64(from_apex_u256(clearing.sold_amount)) / amount18.max(1.0);
                let Ok(scale_out) = TokenScale::new(input.token_meta[&order.token_out].1) else {
                    counters.unknown_decimals += 1;
                    statuses.push((order.id.clone(), OrderStatus::Excluded("unknown_decimals")));
                    continue;
                };
                let bought_raw =
                    scale_out.scale_down_floor(Scaled18(from_apex_u256(clearing.bought_amount)));
                exposure.filled_notional_wei += u256_to_f64(from_apex_u256(clearing.sold_amount)) *
                    wei_prices
                        .get(&order.token_in)
                        .copied()
                        .unwrap_or(0.0);
                if fill_ratio < 1.0 - 1e-9 {
                    counters.partially_filled += 1;
                    OrderStatus::PartiallyFilled { bought_raw, fill_ratio }
                } else {
                    counters.filled += 1;
                    OrderStatus::Filled { bought_raw, fill_ratio: fill_ratio.min(1.0) }
                }
            }
            Some(_) => {
                counters.unfilled_at_limit += 1;
                OrderStatus::UnfilledAtLimit
            }
            // A fired deadline alone does NOT mean this order went unseen. APEX skips a cluster
            // only when the deadline lands *between* clusters; a deadline inside a cluster still
            // clears it at best-so-far prices, and the whole cluster's price vector comes back.
            // So a priced token means the order was evaluated and did not cross — provisionally,
            // at unconverged prices — while an unpriced one means its cluster never ran.
            None if result.deadline_fired => {
                if result
                    .clearing_prices
                    .contains_key(&order.token_in)
                {
                    counters.unfilled_at_best_so_far += 1;
                    OrderStatus::UnfilledAtBestSoFar
                } else {
                    counters.cluster_cut += 1;
                    OrderStatus::ClusterCut
                }
            }
            None => {
                counters.unfilled_at_limit += 1;
                OrderStatus::UnfilledAtLimit
            }
        };
        statuses.push((order.id.clone(), status));
    }
    exposure
}

fn component_error_kind(error: &apex_solver::core::ApexError) -> &'static str {
    match error {
        apex_solver::core::ApexError::InvalidInput(_) => "invalid_input",
        apex_solver::core::ApexError::MetricsCollectionError(_) => "metrics",
        apex_solver::core::ApexError::TradeSolverError(_) => "trade_solver",
        apex_solver::core::ApexError::MarketRouterError(_) => "market_router",
        apex_solver::core::ApexError::ClearingUnderLimitPrice(_, _) => "clearing_under_limit",
        apex_solver::core::ApexError::NegativeBalanceDelta(_, _) => "negative_balance_delta",
        _ => "other",
    }
}

/// Lossy above 2^53, used only for fill-ratio classification — the raw amounts in the statuses
/// stay exact.
fn u256_to_f64(value: U256) -> f64 {
    crate::dataset::u256_to_f64(value)
}

#[cfg(test)]
mod tests {
    use tycho_simulation::{
        evm::protocol::uniswap_v2::state::UniswapV2State,
        tycho_common::{
            models::{token::Token as TychoToken, Chain},
            Bytes,
        },
    };

    use super::*;

    fn token(index: u8) -> ApexAddress {
        ApexAddress([index; 20])
    }

    fn wei(value: u64, exp: u32) -> BigUint {
        BigUint::from(value) * BigUint::from(10u32).pow(exp)
    }

    fn raw(value: u64, exp: u32) -> U256 {
        U256::from(value) * U256::from(10u64).pow(U256::from(exp))
    }

    /// An input holding 18-dec tokens A=1, B=2 priced 1:1 against the gas token.
    fn two_token_input(orders: Vec<LiveOrder>, pools: Vec<LivePool>) -> LiveBatchInput {
        let one_to_one =
            || TokenPriceInput { numerator: wei(1, 18), denominator: wei(1, 18), decimals: 18 };
        LiveBatchInput {
            orders,
            pools,
            price_inputs: HashMap::from([(token(1), one_to_one()), (token(2), one_to_one())]),
            token_meta: HashMap::from([
                (token(1), ("AAA".to_string(), 18)),
                (token(2), ("BBB".to_string(), 18)),
            ]),
        }
    }

    fn order(id: &str, token_in: ApexAddress, token_out: ApexAddress, min_out: U256) -> LiveOrder {
        LiveOrder {
            id: id.to_string(),
            token_in,
            token_out,
            amount_in_raw: raw(1, 18),
            min_out_raw: min_out,
        }
    }

    const BUDGET: Duration = Duration::from_secs(2);

    #[test]
    fn test_batch_eligible_requires_two_orders_sharing_a_token() {
        assert!(!batch_eligible(&[(token(1), token(2))]), "one trade is never a batch");
        assert!(
            !batch_eligible(&[(token(1), token(2)), (token(3), token(4))]),
            "disjoint trades share nothing"
        );
        assert!(batch_eligible(&[(token(1), token(2)), (token(2), token(3))]));
        assert!(
            !batch_eligible(&[(token(1), token(1)), (token(3), token(4))]),
            "a token twice within one order is not shared across orders"
        );
    }

    #[test]
    fn test_crossing_orders_fill_and_singles_cannot() {
        // A→B and B→A at 1:1 prices with permissive limits: the batch crosses; each single,
        // having no pools, has no counterparty.
        let input = two_token_input(
            vec![
                order("a:0", token(1), token(2), raw(9, 17)),
                order("b:0", token(2), token(1), raw(9, 17)),
            ],
            Vec::new(),
        );
        let report = solve_live_batch(&input, BUDGET, BUDGET, true);
        assert_eq!(report.counters.filled, 2, "{report:?}");
        assert!(report
            .statuses
            .iter()
            .all(|(_, status)| matches!(status, OrderStatus::Filled { .. })));
        assert_eq!(report.singles.len(), 2);
        assert!(
            report
                .singles
                .iter()
                .all(|single| single.bought_raw.is_none()),
            "poolless singles have no counterparty: {:?}",
            report.singles
        );
        assert_eq!(report.component_solve_ms.len(), 1, "one component, one batch solve");
    }

    #[test]
    fn test_crossing_orders_record_prices_and_order_clearings() {
        // APEX has no encoder, so the clearing-price vector and the per-order clearings are what
        // makes a solve reconstructible offline. Both must survive onto the report.
        let input = two_token_input(
            vec![
                order("a:0", token(1), token(2), raw(9, 17)),
                order("b:0", token(2), token(1), raw(9, 17)),
            ],
            Vec::new(),
        );
        let report = solve_live_batch(&input, BUDGET, BUDGET, true);
        assert_eq!(report.components.len(), 1, "one batch component, singles excluded");
        let component = &report.components[0];
        let priced: HashSet<ApexAddress> = component
            .clearing_prices
            .iter()
            .map(|(token, _)| *token)
            .collect();
        assert_eq!(priced, HashSet::from([token(1), token(2)]), "{:?}", component.clearing_prices);
        assert!(component
            .clearing_prices
            .iter()
            .all(|(_, price)| !price.is_zero()));
        let mut cleared_ids: Vec<&str> = component
            .order_clearings
            .iter()
            .map(|clearing| clearing.id.as_str())
            .collect();
        cleared_ids.sort_unstable();
        assert_eq!(cleared_ids, vec!["a:0", "b:0"]);
    }

    #[test]
    fn test_single_order_component_without_pools_is_excluded() {
        let input = two_token_input(vec![order("a:0", token(1), token(2), raw(9, 17))], Vec::new());
        let report = solve_live_batch(&input, BUDGET, BUDGET, true);
        assert_eq!(report.counters.singles_skipped, 1);
        assert_eq!(
            report.statuses,
            vec![("a:0".to_string(), OrderStatus::Excluded("single_component"))]
        );
        assert!(report.singles.is_empty(), "excluded orders get no singles control");
    }

    #[test]
    fn test_duplicate_order_id_declined_not_fatal() {
        let input = two_token_input(
            vec![
                order("a:0", token(1), token(2), raw(9, 17)),
                order("b:0", token(2), token(1), raw(9, 17)),
                order("a:0", token(1), token(2), raw(9, 17)),
            ],
            Vec::new(),
        );
        let report = solve_live_batch(&input, BUDGET, BUDGET, false);
        assert_eq!(report.counters.duplicate_order_id, 1);
        assert_eq!(
            report
                .statuses
                .iter()
                .filter(|(_, s)| *s == OrderStatus::Excluded("duplicate_order_id"))
                .count(),
            1
        );
        assert_eq!(report.counters.filled, 2, "the first two orders still cross: {report:?}");
    }

    #[test]
    fn test_unreachable_limit_reconciles_without_fills() {
        // A→B demands 2.0 out per 1.0 in at 1:1 prices; B→A is permissive. The limits cannot
        // cross (2.0 × 0.9 > 1), so no order fills — and the reconciliation must still hand
        // every order exactly one non-fill status rather than dropping either.
        let input = two_token_input(
            vec![
                order("a:0", token(1), token(2), raw(2, 18)),
                order("b:0", token(2), token(1), raw(9, 17)),
            ],
            Vec::new(),
        );
        let report = solve_live_batch(&input, BUDGET, BUDGET, false);
        assert_eq!(report.counters.filled + report.counters.partially_filled, 0, "{report:?}");
        assert_eq!(report.statuses.len(), 2);
        for (id, status) in &report.statuses {
            // Never ClusterCut: the deadline can only skip a cluster it has not started, and a
            // component this small yields a single cluster that always gets cleared — at
            // converged prices here, at best-so-far prices under time pressure.
            assert!(
                matches!(
                    status,
                    OrderStatus::UnfilledAtLimit |
                        OrderStatus::UnfilledAtBestSoFar |
                        OrderStatus::ComponentErrored
                ),
                "{id} ended as {status:?}"
            );
        }
        assert_eq!(report.counters.cluster_cut, 0, "{report:?}");
    }

    #[test]
    fn test_admission_exclusions_report_one_status_each() {
        // c has no metadata (unknown_decimals), d has metadata but no price (token_unpriced),
        // e's zero floor declines inside the solve (zero_amount_or_limit); a and b still cross.
        // Reconciliation invariant: every order id appears exactly once in the statuses.
        let mut input = two_token_input(
            vec![
                order("a:0", token(1), token(2), raw(9, 17)),
                order("b:0", token(2), token(1), raw(9, 17)),
                order("c:0", token(3), token(2), raw(9, 17)),
                order("d:0", token(4), token(2), raw(9, 17)),
                order("e:0", token(1), token(2), U256::ZERO),
            ],
            Vec::new(),
        );
        input
            .token_meta
            .insert(token(4), ("DDD".to_string(), 18));
        let report = solve_live_batch(&input, BUDGET, BUDGET, false);

        let status_for = |wanted: &str| {
            let matches: Vec<&OrderStatus> = report
                .statuses
                .iter()
                .filter(|(id, _)| id == wanted)
                .map(|(_, status)| status)
                .collect();
            assert_eq!(matches.len(), 1, "{wanted} must appear exactly once: {matches:?}");
            matches[0].clone()
        };
        assert_eq!(status_for("c:0"), OrderStatus::Excluded("unknown_decimals"));
        assert_eq!(status_for("d:0"), OrderStatus::Excluded("token_unpriced"));
        assert_eq!(status_for("e:0"), OrderStatus::Excluded("zero_amount_or_limit"));
        assert!(matches!(status_for("a:0"), OrderStatus::Filled { .. }), "{report:?}");
        assert!(matches!(status_for("b:0"), OrderStatus::Filled { .. }), "{report:?}");
        assert_eq!(report.statuses.len(), 5);
    }

    #[test]
    fn test_singles_control_runs_once_per_admitted_id() {
        // The duplicate a:0 is excluded from the batch cell, so the singles control must not
        // solve it either — one single per admitted id, or the pairing rule double-counts.
        let input = two_token_input(
            vec![
                order("a:0", token(1), token(2), raw(9, 17)),
                order("b:0", token(2), token(1), raw(9, 17)),
                order("a:0", token(1), token(2), raw(9, 17)),
            ],
            Vec::new(),
        );
        let report = solve_live_batch(&input, BUDGET, BUDGET, true);
        let mut single_ids: Vec<&str> = report
            .singles
            .iter()
            .map(|single| single.id.as_str())
            .collect();
        single_ids.sort_unstable();
        assert_eq!(single_ids, vec!["a:0", "b:0"], "{:?}", report.singles);
    }

    #[test]
    fn test_single_order_fills_through_a_pool() {
        // One order, one deep 1:1 v2 pool covering the pair: the single-order component keeps its
        // pool, so the order fills against pool supply instead of being skipped.
        let tycho_a = TychoToken::new(
            &Bytes::from([1u8; 20].to_vec()),
            "AAA",
            18,
            0,
            &[Some(60_000)],
            Chain::Base,
            100,
        );
        let tycho_b = TychoToken::new(
            &Bytes::from([2u8; 20].to_vec()),
            "BBB",
            18,
            0,
            &[Some(60_000)],
            Chain::Base,
            100,
        );
        let state =
            UniswapV2State::new(to_apex_u256(raw(1_000_000, 18)), to_apex_u256(raw(1_000_000, 18)));
        let pool = LivePool {
            component_id: "0x0909090909090909090909090909090909090909".to_string(),
            apex_address: token(9),
            token_0: token(1),
            token_1: token(2),
            adapter: Arc::new(TychoApexPool {
                protocol: "uniswap_v2".to_string(),
                tokens: HashMap::from([(token(1), tycho_a), (token(2), tycho_b)]),
                pool: Arc::new(state),
            }),
        };
        let input = two_token_input(vec![order("a:0", token(1), token(2), raw(9, 17))], vec![pool]);
        let report = solve_live_batch(&input, BUDGET, BUDGET, true);
        assert_eq!(
            report.counters.filled + report.counters.partially_filled,
            1,
            "the pool serves the order: {report:?}"
        );
        assert_eq!(report.singles.len(), 1);
        assert!(
            report.singles[0].bought_raw.is_some(),
            "the singles control fills through the same pool: {:?}",
            report.singles
        );

        // The pool leg resolves back to its tycho component id, which the APEX address alone
        // cannot do once a component id is hashed rather than parsed.
        let pool_clearings = &report.components[0].pool_clearings;
        assert_eq!(pool_clearings.len(), 1, "{pool_clearings:?}");
        assert_eq!(
            pool_clearings[0]
                .component_id
                .as_deref(),
            Some("0x0909090909090909090909090909090909090909")
        );
        assert_eq!(pool_clearings[0].protocol.as_deref(), Some("uniswap_v2"));
        assert!(!pool_clearings[0].sold_amount.is_zero());
    }
}

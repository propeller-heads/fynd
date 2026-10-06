//! Computes token prices relative to the gas token after market component changes.
//! Quotes read stored prices without waiting. A price can be several market events old because
//! the count cap and `min_pass_interval` limit which tokens each block prices.
//!
//! # Computing a price
//!
//! A pricing pass uses one market snapshot for every simulation. A buy pass runs Bellman-Ford
//! with `probe_amount` of the gas token to find each selected token's best buy route and bought
//! amount. A reverse sell swaps that amount back along the buy route. Both include fees and
//! slippage. Gas-aware scoring is off because it reads the prices this computation produces.
//! Pricing leaves out pAMMs: a pAMM's quote ladder can pay almost nothing back at the probe size
//! for a block, and its spot prices do not show it.
//!
//! The price is the arithmetic mean of the buy and sell rates, kept as an exact fraction.
//! Both rates are in token units per gas token unit: the bought amount divided by `probe_amount`
//! or by the gas amount returned, respectively. This mean values a token lower as its round-trip
//! loss grows. An exact fraction cannot always represent the geometric mean. The next section
//! gives the one exception.
//!
//! # Flagged pools and sell solves
//!
//! A reverse sell checks each pool before swapping through it. It flags the first pool whose
//! spot price lookup fails, whose directional spot price product is outside
//! `VALID_SPOT_PRODUCT_RANGE`, or whose reverse swap fails or returns zero. Such a pool can
//! quote a buy rate that the swap back cannot deliver. Each hop's spot price check runs once
//! per pricing pass, cached by component and direction.
//!
//! The first buy pass leaves out earlier flagged components that are in the pass's subgraph.
//! If it finishes without reaching a token outside the subgraph, pricing marks that token
//! unreachable without running an extra buy pass. For a token inside the subgraph, the buy pass
//! through every pool can supply a route for a sell solve. It runs at most once, and only if the
//! first buy pass left pools out.
//!
//! Tokens whose reverse sell flags a pool get the second buy pass, which leaves out all pools
//! flagged so far or in earlier pricing passes. Each reached token gets another reverse sell.
//! If the second buy pass finishes without reaching a token, or its reverse sell flags another
//! pool, a sell solve uses the amount the first buy pass bought. Missing simulation state or
//! token metadata also requires a sell solve, using the route that lacked it and its bought
//! amount; it does not flag a pool. So does a reverse sell that returns less than
//! `min_sell_out_bps` of `probe_amount`: a hop on the route pays far too little back, and the
//! sell solve finds a route that does not use it. Any buy pass timeout, including the second buy
//! pass without flagged pools, leaves tokens it did not reach unattempted.
//!
//! A sell solve searches for a route back to the gas token. It can use flagged pools because it
//! ranks routes by simulated output. A token needs a nonzero sell result to get a price: a buy
//! rate alone would overvalue a token that is expensive to sell.
//!
//! When the bought amount comes from a route through a flagged pool, the price is the sell rate
//! alone: the amount sold divided by the gas token amount returned. The flagged pool can quote a
//! wrong buy amount, and the mean would carry half of that error into the price.
//!
//! `PassHistory` keeps stamps, pending arrivals, timing, and flags across pricing passes.
//! `record_pass` stores flags. Each flag excludes its pool from the first buy pass of the next
//! `FLAGGED_POOL_PASSES` pricing passes, then expires so the pool can be checked again.
//!
//! # Dependencies and stored prices
//!
//! A price's `path_components` hold its buy route's pools and, for a sell solve, its sell route's
//! pools. A change to these dependencies makes the token eligible for a new price. A rival pool
//! is not a dependency, so an improved rival route changes the price only after another change or
//! after the price is stale.
//!
//! `PassHistory` keeps each stored price's pools in swap order, the bought amount, and the gas
//! token amount the sell returned. A pass that moves a stored price by `TOKEN_PRICE_JUMP_WARN_BPS`
//! (default 10%) or more logs a warning with the old and new routes and amounts.
//!
//! An attempted token that cannot be priced loses its old price and dependencies. Tokens with
//! no buy route count as unreachable; tokens with no sell route produce failed items.
//! Unattempted tokens keep their price, dependencies, and stamp. If no snapshot subgraph exists
//! around the gas token, all selected tokens stay unattempted.
//!
//! # Cost and time limits
//!
//! Sell solves cost more than reverse sells and run last, in selection order. Each pricing pass
//! stops sell solves at `max_sell_solves_per_pass` (default 200) or the deadline. Tokens left over
//! stay unattempted.
//!
//! The `pass_budget` deadline starts after the first buy pass. Snapshot creation also runs
//! outside this budget. The deadline is checked before each token from the first buy pass,
//! before the second buy pass, and before each sell solve. It does not interrupt running work.
//! The second buy pass and its reverse sells have no deadline check between tokens. Buy passes
//! and sell solves have their own algorithm timeout.
//!
//! Pricing runs on a blocking thread. Token prices and spot prices share a computation stage;
//! the manager stores no output until the stage finishes. Slow pricing delays stored spot prices,
//! component depth computation, and handling of the next market event.
//!
//! # Selecting tokens
//!
//! The gas token needs no selection. A market token is eligible if it has no price, a dependency
//! changed, its price is stale, or it is an arrival: a token of an added component. Arrivals come
//! first because no stored dependency names a new component, and quotes need token prices.
//! Arrivals keep this priority until attempted. A price is stale when no pass attempted its token
//! in the last `MAX_PRICE_AGE_PASSES` pricing passes.
//!
//! Each token's stamp records the last pricing pass that attempted it, including failures.
//! Never-attempted tokens have stamp zero. Within each priority group, the smallest stamp comes
//! first, so failures move behind older eligible tokens. A priced token left out by the cap needs
//! another dependency change or a stale price to become eligible again, unless it keeps arrival
//! priority.
//!
//! `max_tokens_per_pass` caps selection in every pricing pass except seeding, including passes
//! for arrivals only. Selection precedes snapshot creation so the snapshot covers routes toward
//! selected tokens. A deadline cannot choose these tokens, so a count cap does.
//!
//! # Spacing pricing passes
//!
//! `min_pass_interval` defaults to 1 s. The interval starts with a pricing pass over all eligible
//! tokens, not one for arrivals only. Such a pass also waits for a new block. Before expiry, added
//! components allow a capped pricing pass for pending arrivals without prices; it does not restart
//! the interval. Otherwise, `compute` returns stored prices, or seeds prices if no price map
//! exists.
//!
//! # Seeding prices
//!
//! Seeding rebuilds the price and dependency maps at startup or when either map is missing.
//! `ChangedComponents::is_full_recompute` also forces seeding and restarts the interval.
//! Seeding selects every market token except the gas token without a token count cap because
//! `derived_data_ready` does not wait for every token to have a price. The sell solve cap and
//! deadline still apply. Unattempted tokens keep their previous price and dependencies.
//! The gas token gets a price of one and no dependencies.

use std::{
    ops::RangeInclusive,
    sync::{Arc, LazyLock, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};
use petgraph::graph::NodeIndex;
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{debug, instrument, trace, warn, Span};
use tycho_simulation::{
    tycho_common::models::{token::Token, Address},
    tycho_core::simulation::protocol_sim::{Price, ProtocolSim},
};

use crate::{
    algorithm::{
        bellman_ford::{BellmanFordContext, FindRouteOptions, ReachOutcome, ReachedToken},
        sim_guard::GuardedProtocolSim,
        Algorithm, AlgorithmConfig, BellmanFordAlgorithm,
    },
    derived::{
        computation::{
            ComputationId, ComputationOutput, ComputationRequirements, DerivedComputation,
            FailedItem, FailedItemError,
        },
        error::ComputationError,
        manager::{ChangedComponents, SharedDerivedDataRef},
        store::DerivedData,
        types::{TokenGasPrices, TokenPriceEntry, TokenPricesWithDeps},
    },
    fallback::is_pamm,
    feed::{component_filter::remove_components, market_data::MarketData},
    graph::{GraphManager, PetgraphStableDiGraphManager},
    types::{ComponentId, Order, OrderSide, RouteExclusions},
};

/// The range of `spot(a→b) * spot(b→a)` for a pool whose two spot prices agree. A pool with no
/// fee gives 1, and a fee moves the product away from 1 by about twice the fee. Token pricing
/// flags a pool outside the range. The check cannot remove the fee itself, because
/// `ProtocolSim::fee` panics for several protocols.
const VALID_SPOT_PRODUCT_RANGE: RangeInclusive<f64> = 0.9..=1.5;

/// A graph's edges: token node → (the node it swaps into, the pool that swaps it).
type Adjacency = FxHashMap<NodeIndex, Vec<(NodeIndex, ComponentId)>>;

/// The state of one pricing pass: a single market snapshot, and the buy passes run on it.
///
/// The snapshot is built once around the gas token, and every buy pass and sell solve in the
/// pricing pass reads it.
struct PricingPassState<'a> {
    /// The Bellman-Ford algorithm. Its `max_hops` limits every buy route, and the subgraph each
    /// sell solve re-roots to.
    algorithm: &'a BellmanFordAlgorithm,
    graph: &'a <BellmanFordAlgorithm as Algorithm>::GraphType,
    /// The shared snapshot. Each buy pass and each sell solve sets its adjacency and endpoints.
    ctx: BellmanFordContext,
    /// The computation whose parameters — gas token, probe amount, budget — the pass solves with.
    computation: &'a TokenGasPriceComputation,
    /// The snapshot's adjacency with every pool in it. Each buy pass starts from a copy.
    full_adjacency: Adjacency,
    /// The components that earlier pricing passes flagged and that are in this pass's subgraph.
    /// The first buy pass leaves them out.
    earlier_flagged_components: FxHashSet<ComponentId>,
    /// The components that this pricing pass flagged.
    new_flagged_components: FxHashSet<ComponentId>,
    /// The spot price check of each hop a reverse sell ran: `None` when the pool passed, else
    /// why it failed. The check does not depend on the amount, so each hop runs it once.
    spot_checks: FxHashMap<(ComponentId, NodeIndex, NodeIndex), Option<PoolFlagReason>>,
    /// The buy pass through every pool, flagged ones included. The pricing pass runs it the first
    /// time the buy pass without earlier flagged components misses a token, and reuses it after.
    buys_through_all_pools: Option<ReachOutcome>,
    /// The gas token's node, which every buy pass starts from.
    gas_node: NodeIndex,
    /// The hop count from each node to the gas token. Every sell solve prunes its subgraph with
    /// it.
    hops_to_gas: FxHashMap<NodeIndex, usize>,
    /// Token address → graph node, inverted once from the context, for re-rooting sells.
    token_nodes: FxHashMap<Address, NodeIndex>,
}

/// The pricing result for one token in a pricing pass.
enum TokenPricingOutcome {
    /// The token's price, the components used to compute it, and the route behind it.
    Priced(PricedToken),
    /// The error from a sell solve that produces no price.
    Failed(FailedItemError),
    /// Pricing is incomplete; the token keeps its previous price, dependencies, and stamp.
    Unattempted,
    /// No buy pass that finished reached the token.
    Unreachable,
}

/// The tokens a reverse sell could not price, each with the buy route a later step uses.
#[derive(Default)]
struct QueuedTokens {
    /// Tokens whose reverse sell reached a flagged pool, with the buy route that reached it.
    on_flagged_routes: FxHashMap<Address, ReachedToken>,
    /// Tokens to price with a sell solve, with the buy route whose amount the sell solve sells.
    for_sell_solve: FxHashMap<Address, SellSolveBuyLeg>,
}

/// The buy route whose bought amount a sell solve sells.
enum SellSolveBuyLeg {
    /// A route with no flagged pool. The price is the mean of the buy and sell rates.
    Trusted(ReachedToken),
    /// A route through a flagged pool. Its bought amount can be wrong, so the price is the sell
    /// rate alone.
    ThroughFlaggedPool(ReachedToken),
}

/// The result of a reverse sell along a token's buy route.
enum ReverseSellOutcome {
    /// The nonzero gas token amount returned by the reverse sell.
    Sold(BigUint),
    /// The first pool that fails a reverse sell check and the reason to flag it.
    FlaggedPool(ComponentId, PoolFlagReason),
    /// A hop lacks simulation state or token metadata; this does not flag the pool.
    MissingHopData,
    /// The reverse sell returned less than `min_sell_out_bps` of the probe amount.
    LowSellOut,
}

/// The reason a reverse sell flags a pool.
#[derive(Debug, Clone, Copy)]
enum PoolFlagReason {
    /// A spot price lookup fails in at least one direction.
    SpotPriceFailed,
    /// The product of the two directional spot prices falls outside `VALID_SPOT_PRODUCT_RANGE`.
    SpotProductOutOfRange,
    /// The reverse swap simulation fails.
    SwapFailed,
    /// The reverse swap simulation returns zero.
    ZeroOutput,
}

/// A token's price with the route behind it.
struct PricedToken {
    /// The stored price and its dependencies.
    entry: TokenPriceEntry,
    /// The pools and amounts that produced the price.
    route: PriceRoute,
}

/// The pools and amounts behind a token's price, kept for the price jump warning.
#[derive(Debug, Clone)]
struct PriceRoute {
    /// The components of the routes that priced the token, in swap order: the buy route, then
    /// the sell route of a sell solve.
    components: Vec<ComponentId>,
    /// The amount of the token that the buy route bought with the probe amount of gas token.
    buy_amount_out: BigUint,
    /// The gas token amount that selling `buy_amount_out` back returned.
    sell_amount_out: BigUint,
}

/// Builds the price of a token priced from `buy_leg` and the gas token amount `sell_out` that
/// selling its bought amount back returned. The buy route's components are its dependencies.
fn build_priced_token(price: Price, buy_leg: &ReachedToken, sell_out: BigUint) -> PricedToken {
    let components: Vec<ComponentId> = buy_leg
        .hops
        .iter()
        .map(|(_, _, component_id)| component_id.clone())
        .collect();
    let entry = TokenPriceEntry { price, path_components: components.iter().cloned().collect() };
    let route = PriceRoute {
        components,
        buy_amount_out: buy_leg.amount_out.clone(),
        sell_amount_out: sell_out,
    };
    PricedToken { entry, route }
}

/// Prices a token at the sell rate alone: the amount sold divided by the gas token amount
/// returned. Stores the buy route's components as its dependencies.
fn build_sell_rate_entry(buy_leg: &ReachedToken, sell_out: BigUint) -> PricedToken {
    let sell_rate = Price { numerator: buy_leg.amount_out.clone(), denominator: sell_out.clone() };
    build_priced_token(sell_rate, buy_leg, sell_out)
}

/// Environment variable that sets how far a token's price may move in one pricing pass before
/// the pass logs a warning, in basis points of the smaller price.
const PRICE_JUMP_WARN_BPS_ENV: &str = "TOKEN_PRICE_JUMP_WARN_BPS";

/// Default price move that logs a warning: 10%.
const DEFAULT_PRICE_JUMP_WARN_BPS: u32 = 1_000;

/// How far a token's price may move in one pricing pass before the pass logs a warning, as the
/// ratio of the larger price to the smaller. Read once from `PRICE_JUMP_WARN_BPS_ENV`.
static PRICE_JUMP_WARN_RATIO: LazyLock<f64> =
    LazyLock::new(|| 1.0 + f64::from(price_jump_warn_bps_from_env()) / f64::from(BPS_DENOMINATOR));

/// Reads the warning threshold from `PRICE_JUMP_WARN_BPS_ENV`, falling back to
/// `DEFAULT_PRICE_JUMP_WARN_BPS` when the variable is unset or not a whole number.
fn price_jump_warn_bps_from_env() -> u32 {
    let Ok(raw) = std::env::var(PRICE_JUMP_WARN_BPS_ENV) else {
        return DEFAULT_PRICE_JUMP_WARN_BPS;
    };
    match raw.trim().parse::<u32>() {
        Ok(bps) => bps,
        Err(_) => {
            warn!(
                value = %raw,
                default_bps = DEFAULT_PRICE_JUMP_WARN_BPS,
                "{PRICE_JUMP_WARN_BPS_ENV} must be a whole number of basis points; using the \
                 default"
            );
            DEFAULT_PRICE_JUMP_WARN_BPS
        }
    }
}

/// Returns `new / stored` when the price moved by `PRICE_JUMP_WARN_RATIO` or more in either
/// direction, else `None`. A zero price never counts as a jump.
fn price_jump_ratio(stored: &Price, new: &Price) -> Option<f64> {
    let ratio = price_to_f64(new)? / price_to_f64(stored)?;
    let warn_ratio = *PRICE_JUMP_WARN_RATIO;
    (ratio >= warn_ratio || ratio.recip() >= warn_ratio).then_some(ratio)
}

/// Logs a warning with both routes and their amounts for each price in `solved` that moved by
/// `PRICE_JUMP_WARN_RATIO` or more from its price in `stored`. A token with no stored price or
/// no stored route is skipped.
fn warn_on_price_jumps(
    stored: &TokenPricesWithDeps,
    stored_routes: &FxHashMap<Address, PriceRoute>,
    solved: &PricingPassOutcome,
) {
    for (token, new) in &solved.prices {
        let (Some(stored), Some(stored_route), Some(new_route)) =
            (stored.get(token), stored_routes.get(token), solved.routes.get(token))
        else {
            continue;
        };
        let Some(ratio) = price_jump_ratio(&stored.price, &new.price) else {
            continue;
        };
        warn!(
            %token,
            block = solved.block,
            ratio,
            stored_price = price_to_f64(&stored.price),
            new_price = price_to_f64(&new.price),
            stored_route = ?stored_route.components,
            new_route = ?new_route.components,
            stored_buy_amount_out = %stored_route.buy_amount_out,
            new_buy_amount_out = %new_route.buy_amount_out,
            stored_sell_amount_out = %stored_route.sell_amount_out,
            new_sell_amount_out = %new_route.sell_amount_out,
            "token price jumped"
        );
    }
}

/// Returns `price` as a float, or `None` when it is zero or does not fit.
fn price_to_f64(price: &Price) -> Option<f64> {
    let value = price.numerator.to_f64()? / price.denominator.to_f64()?;
    (value.is_finite() && value > 0.0).then_some(value)
}

/// Returns why the pool's two spot prices between `token_in` and `token_out` fail the check, or
/// `None` when they agree.
fn check_spot_prices(
    sim: &dyn ProtocolSim,
    token_in: &Token,
    token_out: &Token,
) -> Option<PoolFlagReason> {
    let (Ok(forward_spot), Ok(reverse_spot)) =
        (sim.spot_price(token_out, token_in), sim.spot_price(token_in, token_out))
    else {
        return Some(PoolFlagReason::SpotPriceFailed);
    };
    if !VALID_SPOT_PRODUCT_RANGE.contains(&(forward_spot * reverse_spot)) {
        return Some(PoolFlagReason::SpotProductOutOfRange);
    }
    None
}

impl<'a> PricingPassState<'a> {
    fn new(
        algorithm: &'a BellmanFordAlgorithm,
        graph: &'a <BellmanFordAlgorithm as Algorithm>::GraphType,
        mut ctx: BellmanFordContext,
        computation: &'a TokenGasPriceComputation,
        earlier_flagged_components: FxHashSet<ComponentId>,
    ) -> Self {
        let full_adjacency = std::mem::take(&mut ctx.adj);
        let subgraph_components: FxHashSet<&ComponentId> = full_adjacency
            .values()
            .flatten()
            .map(|(_, component_id)| component_id)
            .collect();
        let earlier_flagged_components = earlier_flagged_components
            .into_iter()
            .filter(|component_id| subgraph_components.contains(component_id))
            .collect();
        let gas_node = ctx.token_in_node;
        let token_nodes = ctx
            .node_address
            .iter()
            .map(|(&node, address)| (address.clone(), node))
            .collect();
        // Pricing carries no request, so nothing is excluded and the gas token stands in for
        // both exempt endpoints.
        let hops_to_gas = BellmanFordAlgorithm::get_hops_to_reach(
            graph,
            gas_node,
            gas_node,
            algorithm.max_hops(),
            &RouteExclusions::default(),
        );
        Self {
            algorithm,
            graph,
            ctx,
            computation,
            full_adjacency,
            earlier_flagged_components,
            new_flagged_components: FxHashSet::default(),
            spot_checks: FxHashMap::default(),
            buys_through_all_pools: None,
            gas_node,
            hops_to_gas,
            token_nodes,
        }
    }

    /// Prices every token the budget allows. Pure CPU work: callers run it on a blocking thread.
    ///
    /// The pricing pass runs a buy pass without the earlier flagged components, then works in
    /// three steps: a reverse sell for each token, a buy pass without any flagged component for
    /// the tokens whose reverse sell reached a flagged pool, then a sell solve for each token still
    /// without a price. Sell solves are the expensive step, so they run last, in the order of
    /// `tokens_to_price`, up to `max_sell_solves_per_pass` of them. The deadline is checked before
    /// each reverse sell of the first step, before the second step, and before each sell solve. So
    /// a pricing pass that the deadline or the cap cuts short prices the front of that order, and
    /// the caller puts the tokens that must not be dropped there. A token that no step prices is
    /// unattempted.
    fn price_tokens(&mut self, tokens_to_price: Vec<Address>, block: u64) -> PricingPassOutcome {
        let adjacency = self.build_adjacency_without(&self.earlier_flagged_components);
        let mut buys = self.run_buy_pass(adjacency);
        let deadline = Instant::now() + self.computation.pass_budget;
        let mut outcomes = FxHashMap::default();
        let mut queued = QueuedTokens::default();
        for token in &tokens_to_price {
            if Instant::now() >= deadline {
                break;
            }
            if let Some(outcome) = self.price_with_reverse_sell(token, &mut buys, &mut queued) {
                outcomes.insert(token.clone(), outcome);
            }
        }
        let QueuedTokens { on_flagged_routes, mut for_sell_solve } = queued;
        if Instant::now() < deadline {
            self.price_without_flagged_pools(on_flagged_routes, &mut outcomes, &mut for_sell_solve);
        }
        let mut sell_solves = 0;
        for token in &tokens_to_price {
            if Instant::now() >= deadline ||
                sell_solves >=
                    self.computation
                        .max_sell_solves_per_pass
            {
                break;
            }
            if let Some(buy_leg) = for_sell_solve.remove(token) {
                sell_solves += 1;
                outcomes.insert(token.clone(), self.price_with_sell_solve(token, &buy_leg));
            }
        }
        let token_outcomes = tokens_to_price
            .into_iter()
            .map(|token| {
                let outcome = outcomes
                    .remove(&token)
                    .unwrap_or(TokenPricingOutcome::Unattempted);
                (token, outcome)
            })
            .collect();
        self.build_outcome(token_outcomes, buys.timed_out, block)
    }

    /// Prices `token` with a reverse sell along its route in `buys`. Returns `None` when it queues
    /// the token in `queued`: when the reverse sell reaches a flagged pool or misses hop data, and
    /// when only a flagged pool reaches the token.
    fn price_with_reverse_sell(
        &mut self,
        token: &Address,
        buys: &mut ReachOutcome,
        queued: &mut QueuedTokens,
    ) -> Option<TokenPricingOutcome> {
        let Some(buy_leg) = buys.reached.remove(token) else {
            // A buy pass that timed out says nothing about reachability, so the token keeps its
            // price and dependencies.
            if buys.timed_out {
                return Some(TokenPricingOutcome::Unattempted);
            }
            return self.queue_sell_solve_through_flagged_pools(token, queued);
        };
        match self.reverse_sell(&buy_leg) {
            ReverseSellOutcome::Sold(sell_out) => {
                Some(TokenPricingOutcome::Priced(self.build_price_entry(token, &buy_leg, sell_out)))
            }
            ReverseSellOutcome::FlaggedPool(component_id, reason) => {
                self.flag(component_id, reason);
                queued
                    .on_flagged_routes
                    .insert(token.clone(), buy_leg);
                None
            }
            ReverseSellOutcome::MissingHopData | ReverseSellOutcome::LowSellOut => {
                queued
                    .for_sell_solve
                    .insert(token.clone(), SellSolveBuyLeg::Trusted(buy_leg));
                None
            }
        }
    }

    /// Looks for a route to `token` in the buy pass through every pool, for a token that the buy
    /// pass without the earlier flagged components did not reach. A flagged component can be the
    /// only way to reach a token. Such a token is queued in `queued` for a sell solve on that
    /// route, because a reverse sell through a flagged pool gives no trusted price, and the
    /// function returns `None`.
    ///
    /// Many tokens are unreachable, so the pricing pass counts them instead of allocating,
    /// logging and sending a failed item per token on every block.
    fn queue_sell_solve_through_flagged_pools(
        &mut self,
        token: &Address,
        queued: &mut QueuedTokens,
    ) -> Option<TokenPricingOutcome> {
        // With no earlier flagged component in the subgraph, the buy pass already went through
        // every pool. A token outside the subgraph has no route in any buy pass.
        if self
            .earlier_flagged_components
            .is_empty() ||
            !self.token_nodes.contains_key(token)
        {
            return Some(TokenPricingOutcome::Unreachable);
        }
        let mut buys = match self.buys_through_all_pools.take() {
            Some(buys) => buys,
            None => self.run_buy_pass(self.full_adjacency.clone()),
        };
        let outcome = match buys.reached.remove(token) {
            Some(buy_leg) => {
                queued
                    .for_sell_solve
                    .insert(token.clone(), SellSolveBuyLeg::ThroughFlaggedPool(buy_leg));
                None
            }
            None if buys.timed_out => Some(TokenPricingOutcome::Unattempted),
            None => Some(TokenPricingOutcome::Unreachable),
        };
        self.buys_through_all_pools = Some(buys);
        outcome
    }

    /// Returns the snapshot's adjacency without the edges of the `excluded` components.
    fn build_adjacency_without(&self, excluded: &FxHashSet<ComponentId>) -> Adjacency {
        let mut adjacency = self.full_adjacency.clone();
        if excluded.is_empty() {
            return adjacency;
        }
        for edges in adjacency.values_mut() {
            edges.retain(|(_, component_id)| !excluded.contains(component_id));
        }
        adjacency
    }

    /// Runs a buy pass from the gas token over `adjacency`.
    fn run_buy_pass(&mut self, adjacency: Adjacency) -> ReachOutcome {
        self.ctx.adj = adjacency;
        self.ctx.token_in_node = self.gas_node;
        self.ctx.token_out_node = None;
        self.algorithm
            .reach_from_source_token(&self.ctx, &self.computation.probe_amount)
    }

    /// Flags `component_id` for the rest of this pricing pass and for the pricing passes after it.
    fn flag(&mut self, component_id: ComponentId, reason: PoolFlagReason) {
        if !self
            .new_flagged_components
            .contains(&component_id)
        {
            debug!(component_id, ?reason, "token pricing flagged a pool");
            self.new_flagged_components
                .insert(component_id);
        }
    }

    /// Runs a buy pass without any flagged component, and prices each token in
    /// `on_flagged_routes` with a reverse sell along its route in that buy pass. The price goes to
    /// `outcomes`. A token that this buy pass does not reach, or whose route reaches a flagged pool
    /// too, goes to `for_sell_solve` with its route in `on_flagged_routes`. A token whose route
    /// misses hop data goes there with its route in this buy pass. When this buy pass times out,
    /// a token it did not reach stays unattempted.
    fn price_without_flagged_pools(
        &mut self,
        on_flagged_routes: FxHashMap<Address, ReachedToken>,
        outcomes: &mut FxHashMap<Address, TokenPricingOutcome>,
        for_sell_solve: &mut FxHashMap<Address, SellSolveBuyLeg>,
    ) {
        if on_flagged_routes.is_empty() {
            return;
        }
        let excluded = self
            .earlier_flagged_components
            .union(&self.new_flagged_components)
            .cloned()
            .collect();
        let adjacency = self.build_adjacency_without(&excluded);
        let mut buys = self.run_buy_pass(adjacency);

        for (token, flagged_route) in on_flagged_routes {
            let Some(buy_leg) = buys.reached.remove(&token) else {
                if !buys.timed_out {
                    for_sell_solve
                        .insert(token, SellSolveBuyLeg::ThroughFlaggedPool(flagged_route));
                }
                continue;
            };
            match self.reverse_sell(&buy_leg) {
                ReverseSellOutcome::Sold(sell_out) => {
                    let entry = self.build_price_entry(&token, &buy_leg, sell_out);
                    outcomes.insert(token, TokenPricingOutcome::Priced(entry));
                }
                ReverseSellOutcome::FlaggedPool(component_id, reason) => {
                    self.flag(component_id, reason);
                    for_sell_solve
                        .insert(token, SellSolveBuyLeg::ThroughFlaggedPool(flagged_route));
                }
                ReverseSellOutcome::MissingHopData | ReverseSellOutcome::LowSellOut => {
                    for_sell_solve.insert(token, SellSolveBuyLeg::Trusted(buy_leg));
                }
            }
        }
    }

    /// Sorts the token outcomes into the pricing pass's outcome, and logs how the pricing pass
    /// went.
    fn build_outcome(
        &mut self,
        outcomes: Vec<(Address, TokenPricingOutcome)>,
        buy_pass_timed_out: bool,
        block: u64,
    ) -> PricingPassOutcome {
        let mut prices = FxHashMap::default();
        let mut routes = FxHashMap::default();
        let mut failed_items = Vec::new();
        let mut unattempted = FxHashSet::default();
        let mut unreachable_tokens = 0usize;
        for (token, outcome) in outcomes {
            match outcome {
                TokenPricingOutcome::Priced(PricedToken { entry, route }) => {
                    prices.insert(token.clone(), entry);
                    routes.insert(token, route);
                }
                TokenPricingOutcome::Failed(error) => {
                    failed_items.push(FailedItem { key: token.to_string(), error });
                }
                TokenPricingOutcome::Unattempted => {
                    unattempted.insert(token);
                }
                TokenPricingOutcome::Unreachable => unreachable_tokens += 1,
            }
        }
        if unattempted.is_empty() {
            debug!(
                priced = prices.len(),
                failed = failed_items.len(),
                unreachable = unreachable_tokens,
                new_flagged_components = self.new_flagged_components.len(),
                block,
                "token pricing pass complete"
            );
        } else {
            warn!(
                priced = prices.len(),
                failed = failed_items.len(),
                unreachable = unreachable_tokens,
                unattempted = unattempted.len(),
                buy_pass_timed_out,
                block,
                "token pricing pass cut short; unattempted tokens keep previous prices"
            );
        }
        let new_flagged_components = std::mem::take(&mut self.new_flagged_components);
        PricingPassOutcome {
            prices,
            routes,
            block,
            failed_items,
            unattempted,
            new_flagged_components,
        }
    }

    /// Prices one token as the arithmetic mean of its buy price and its sell price, kept as an
    /// exact fraction, and stores the buy route's components as its dependencies. The mean prices a
    /// token low, most on thin pairs; the module doc says why.
    fn build_price_entry(
        &self,
        token: &Address,
        buy_leg: &ReachedToken,
        sell_out: BigUint,
    ) -> PricedToken {
        trace!(%token, buy_out = %buy_leg.amount_out, sell_out = %sell_out, "token priced");
        let mid_price = Price {
            numerator: &buy_leg.amount_out * (&self.computation.probe_amount + &sell_out),
            denominator: BigUint::from(2u8) * &self.computation.probe_amount * &sell_out,
        };
        build_priced_token(mid_price, buy_leg, sell_out)
    }

    /// Prices a token with a sell solve of its bought amount back to the gas token. The stored
    /// components are the pools of the buy route and of the sell route. A token the sell solve
    /// cannot sell back gets an error, not a price; the module doc says why.
    fn price_with_sell_solve(
        &mut self,
        token: &Address,
        buy_leg: &SellSolveBuyLeg,
    ) -> TokenPricingOutcome {
        let (SellSolveBuyLeg::Trusted(route) | SellSolveBuyLeg::ThroughFlaggedPool(route)) =
            buy_leg;
        let (sell_out, sell_route) = match self.solve_sell_leg(token, route.amount_out.clone()) {
            Ok(sell_leg) => sell_leg,
            Err(error) => return TokenPricingOutcome::Failed(error),
        };
        let mut priced = match buy_leg {
            SellSolveBuyLeg::Trusted(route) => self.build_price_entry(token, route, sell_out),
            SellSolveBuyLeg::ThroughFlaggedPool(route) => build_sell_rate_entry(route, sell_out),
        };
        priced
            .entry
            .path_components
            .extend(sell_route.iter().cloned());
        priced
            .route
            .components
            .extend(sell_route);
        TokenPricingOutcome::Priced(priced)
    }

    /// Sells the bought amount back along the buy route, hop by hop in reverse. Returns
    /// `FlaggedPool` with the first pool whose spot price fails, whose spot product is outside
    /// `VALID_SPOT_PRODUCT_RANGE`, or whose swap fails or returns zero.
    fn reverse_sell(&mut self, buy_leg: &ReachedToken) -> ReverseSellOutcome {
        let mut amount = buy_leg.amount_out.clone();
        for (sold_node, bought_node, component_id) in buy_leg.hops.iter().rev() {
            let flagged = |reason| ReverseSellOutcome::FlaggedPool(component_id.clone(), reason);
            let (Some(sim), Some(token_in), Some(token_out)) = (
                self.ctx
                    .market_data
                    .get_simulation_state(component_id),
                self.ctx.token_map.get(bought_node),
                self.ctx.token_map.get(sold_node),
            ) else {
                return ReverseSellOutcome::MissingHopData;
            };
            let spot_check = self
                .spot_checks
                .entry((component_id.clone(), *sold_node, *bought_node))
                .or_insert_with(|| check_spot_prices(sim, token_in, token_out));
            if let Some(reason) = *spot_check {
                return flagged(reason);
            }
            match sim.get_amount_out_guarded(amount, token_in, token_out) {
                Ok(result) if !result.amount.is_zero() => amount = result.amount,
                Ok(_) => return flagged(PoolFlagReason::ZeroOutput),
                Err(_) => return flagged(PoolFlagReason::SwapFailed),
            }
        }
        let min_sell_out =
            &self.computation.probe_amount * self.computation.min_sell_out_bps / BPS_DENOMINATOR;
        if amount < min_sell_out {
            return ReverseSellOutcome::LowSellOut;
        }
        ReverseSellOutcome::Sold(amount)
    }

    /// Re-roots the pass's shared snapshot at `token`, solves the route selling `amount` of
    /// `token` back to the gas token, and returns what it delivers with its components in swap
    /// order. Returns `MissingSellRoute` with the reason to help explain failures on the same
    /// block.
    fn solve_sell_leg(
        &mut self,
        token: &Address,
        amount: BigUint,
    ) -> Result<(BigUint, Vec<ComponentId>), FailedItemError> {
        let token_node = *self
            .token_nodes
            .get(token)
            .ok_or_else(|| {
                FailedItemError::MissingSellRoute("token is not in the pass subgraph".into())
            })?;
        let rerooted = self.ctx.reroot_toward(
            self.graph,
            token_node,
            self.gas_node,
            &self.hops_to_gas,
            self.algorithm.max_hops(),
        );
        if !rerooted {
            return Err(FailedItemError::MissingSellRoute(
                "no pruned subgraph toward the gas token".into(),
            ));
        }
        let order = Order::new(
            token.clone(),
            self.computation.gas_token.clone(),
            amount,
            OrderSide::Sell,
            Address::zero(20),
        );
        let result = self
            .algorithm
            .find_single_route(&self.ctx, &order, FindRouteOptions::default())
            .map_err(|error| FailedItemError::MissingSellRoute(error.to_string()))?;
        let route = result.route();
        let amount_out = route.amount_out(&self.computation.gas_token);
        if amount_out.is_zero() {
            return Err(FailedItemError::MissingSellRoute("the sell route returns zero".into()));
        }
        let components = route
            .swaps()
            .iter()
            .map(|swap| swap.component_id().to_string())
            .collect();
        Ok((amount_out, components))
    }
}

/// One pass's output: what was priced, against which block, and what was not.
struct PricingPassOutcome {
    /// Priced tokens with the components that must re-price them when they change.
    prices: FxHashMap<Address, TokenPriceEntry>,
    /// The route behind each priced token's price.
    routes: FxHashMap<Address, PriceRoute>,
    /// The block the market snapshot was taken at.
    block: u64,
    /// Tokens that were attempted and could not be priced: bought, but no sell route back.
    failed_items: Vec<FailedItem>,
    /// Tokens never attempted — the cap left them out, the deadline expired first, or the pass
    /// bailed out before solving anything. They keep their previous price and their stamp:
    /// unlike a failure, nothing is known about them this block.
    unattempted: FxHashSet<Address>,
    /// The components that this pricing pass flagged. The first buy pass of the next
    /// `FLAGGED_POOL_PASSES` pricing passes leaves them out.
    new_flagged_components: FxHashSet<ComponentId>,
}

/// Drops the stored failures of the tokens `solved` attempted, and of the tokens that left the
/// market. Persisting the pass stores its new failures. An incremental update otherwise drops a
/// failure only when its token gets a price, so the failure of a token that became unreachable or
/// left the market would stay for the life of the process.
fn drop_stale_failures(store: &mut DerivedData, solved: &PricingPassOutcome) {
    store.retain_token_price_failures(|token| solved.unattempted.contains(token));
}

/// Computes token prices relative to the gas token from the routes that trade it.
#[derive(Debug, Clone)]
pub struct TokenGasPriceComputation {
    /// The gas token address (e.g., ETH).
    gas_token: Address,
    /// Longest route the algorithm may build.
    max_hops: usize,
    /// Amount of gas token each probe buys with (affects slippage).
    probe_amount: BigUint,
    /// Wall-clock budget for the steps of a pricing pass after its first buy pass. The snapshot
    /// and the first buy pass run outside it, bounded only by the algorithm's timeout. Tokens not
    /// attempted before it expires keep their previous price; the module's "Cost and time
    /// limits" section says which steps check it and what a slow pricing pass delays.
    pass_budget: Duration,
    /// Most tokens one pass attempts. This is what bounds a pass; see `select_pass_tokens`.
    max_tokens_per_pass: usize,
    /// Most sell solves one pass runs. A sell solve costs far more than a reverse sell, so this
    /// bounds a pass in which many tokens fall back to one.
    max_sell_solves_per_pass: usize,
    /// How long after a pass starts the next one may start. It is a lower bound on the gap
    /// between two passes, not a schedule: a pass runs when this time has elapsed *and* the
    /// market gives it something to price. The cap bounds what one pass costs; this bounds how
    /// often one runs. See the module's "Spacing pricing passes" section.
    min_pass_interval: Duration,
    /// The least gas token a reverse sell must return, in basis points of `probe_amount`. A
    /// reverse sell that returns less goes to a sell solve.
    min_sell_out_bps: u32,
    /// What earlier pricing passes left behind: when to run the next one, which tokens have
    /// waited longest, and which components are flagged.
    ///
    /// Shared because `compute` takes `&self`, and because the struct derives `Clone` for the
    /// `spawn_blocking` handoff.
    pass_history: Arc<Mutex<PassHistory>>,
}

/// What earlier pricing passes left behind for the next one.
#[derive(Debug, Default)]
struct PassHistory {
    /// Passes that have run. The number a pass takes stamps every token it attempts, and the
    /// next pass orders by that stamp; see the module's "Selecting tokens" section.
    passes: u64,
    /// When the last whole pass started, for `min_pass_interval`.
    last_pass_started: Option<Instant>,
    /// The block of the last whole pass.
    last_pass_block: Option<u64>,
    /// The pass each token was last attempted in, whether or not the attempt priced it. This
    /// is the only stamp a token that cannot be priced has, and it is what stops such a token
    /// from holding a slot in every pass.
    last_attempted: FxHashMap<Address, u64>,
    /// Tokens a component brought that no pass has attempted since. A component that arrives is
    /// in no stored dependency set, so once its tokens lose the arrived rank nothing points at
    /// them again. They keep the rank until a pass attempts them.
    pending_arrivals: FxHashSet<Address>,
    /// The flagged components, each with the number of the pricing pass that flagged it. The first
    /// buy pass of the next `FLAGGED_POOL_PASSES` pricing passes leaves the component out.
    flagged_components: FxHashMap<ComponentId, u64>,
    /// The route behind each stored price, for the price jump warning.
    routes: FxHashMap<Address, PriceRoute>,
}

impl PassHistory {
    /// Stores the routes of the prices a pass produced, and forgets the routes of the tokens
    /// that have no price in `stored`.
    fn record_routes(
        &mut self,
        routes: &FxHashMap<Address, PriceRoute>,
        stored: &TokenPricesWithDeps,
    ) {
        for (token, route) in routes {
            self.routes
                .insert(token.clone(), route.clone());
        }
        self.routes
            .retain(|token, _| stored.contains_key(token));
    }
}

/// How much of a pass the interval allows right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PassSlot {
    /// The interval has elapsed on a new block. A pass over every candidate runs and starts the
    /// interval again.
    Due,
    /// Inside the interval, but a component arrived. Only the tokens it carries that have no
    /// price are priced, and the interval keeps running: a chain that lists a pool on most
    /// blocks would otherwise price a full rank of candidates on most blocks, and the interval
    /// would bound nothing.
    ArrivalsOnly,
    /// Inside the interval with nothing that cannot wait for it.
    Deferred,
}

/// Which tokens a pass that is going to run may attempt. Both scopes are capped by
/// `max_tokens_per_pass`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PassScope {
    /// Every candidate in the market, ranked.
    Whole,
    /// Only the tokens that components brought and that have no price yet.
    ArrivalsOnly,
}

/// What a pass knows about the tokens it is about to attempt, which decides their order.
///
/// A snapshot, not a borrow of the computation's state: `select_pass_tokens` stays a plain
/// function of what it is given, and no lock is held over the solving that follows.
#[derive(Debug, Default)]
pub(crate) struct PassPriority {
    /// Tokens carried by components that arrived this block.
    arrived: FxHashSet<Address>,
    /// Tokens that have a stored price. A token that is absent has none, which is what makes it
    /// a candidate whether or not a change points at it.
    priced: FxHashSet<Address>,
    /// The pass each token was last attempted in, from `PassHistory`. Absent means never
    /// attempted, which ranks the token ahead of every attempted one.
    last_attempted: FxHashMap<Address, u64>,
    /// The number of the last pass that ran, from `PassHistory`.
    passes: u64,
}

impl PassPriority {
    /// Drops the arrived tokens that already have a price.
    fn keep_unpriced_arrivals(&mut self) {
        let priced = &self.priced;
        self.arrived
            .retain(|token| !priced.contains(token));
    }

    /// The pass that last attempted `token`, or zero for one no pass has reached yet.
    fn stamp(&self, token: &Address) -> u64 {
        self.last_attempted
            .get(token)
            .copied()
            .unwrap_or(0)
    }

    /// Whether the last pass that attempted `token` is `MAX_PRICE_AGE_PASSES` or more passes old.
    fn is_stale(&self, token: &Address) -> bool {
        self.passes
            .saturating_sub(self.stamp(token)) >=
            MAX_PRICE_AGE_PASSES
    }
}

/// Chooses and orders the tokens one pass attempts, and caps how many it takes.
///
/// The cap is what makes a pass bounded, and it has to be applied here rather than left to
/// `pass_budget`: the market snapshot is pruned toward the tokens the pass will attempt, so the
/// set has to be known before any solving starts. The budget stays a backstop for a pathological
/// token, not the thing that decides the size of a pass.
///
/// A token is a candidate when a change points at it, when a component carrying it arrived, when
/// its price is stale, or when it has no price. That last case is the one a selection built from
/// stored dependencies cannot express: an unpriced token is in no dependency set, so nothing would
/// ever point at it and it would stay unpriced for as long as the process ran.
///
/// Rank, in order: arrived, then the tokens a change points at, the stale ones, and the ones with
/// no price.
/// Within a rank the token whose last attempt is oldest comes first, so a cap smaller than the
/// candidates rotates over them instead of starving the tail, and a token that no pass has
/// attempted yet comes before every token that has a price.
/// `PassScope::ArrivalsOnly` offers the arrived tokens only. What is in that set is the caller's
/// choice: inside the interval `update_prices` first drops the arrivals that already have a
/// price, so only a token that cannot be quoted at all reaches this function.
fn select_pass_tokens(
    universe: &FxHashSet<Address>,
    changed: Option<&FxHashSet<Address>>,
    priority: &PassPriority,
    scope: PassScope,
    max_tokens: usize,
) -> Vec<Address> {
    const ARRIVED: u8 = 0;
    const ROTATING: u8 = 1;

    let mut ranked: Vec<(u8, u64, Address)> = Vec::with_capacity(universe.len());
    for token in universe {
        // Both ranks order by the pass that last *attempted* the token, not the pass that last
        // priced it. A token that cannot be priced has no other stamp, and ordering it by a
        // price it never got would sort it at zero for ever and hold a slot in every pass. On
        // Base at `min-tvl 1` that was 47 of every 100 slots, re-failing the same tokens.
        //
        // Within the arrived rank the same key puts a token that has never been attempted, and
        // so has no price, ahead of one that arrived with a price already. A token cannot be
        // quoted at all until it has a price, and the cap can cut this rank short.
        let stamp = priority.stamp(token);
        if priority.arrived.contains(token) {
            ranked.push((ARRIVED, stamp, token.clone()));
            continue;
        }
        // A pass the interval did not grant carries the arrivals and nothing else. Leaving the
        // rest of the market out here rather than relying on the rank to keep them under the
        // cap is what makes an arrival that is not in the universe cost nothing.
        if scope == PassScope::ArrivalsOnly {
            continue;
        }
        // A priced token is offered only when a change points at it or its price is stale; an
        // unpriced one always is.
        if !priority.priced.contains(token) ||
            changed.is_none_or(|changed| changed.contains(token)) ||
            priority.is_stale(token)
        {
            ranked.push((ROTATING, stamp, token.clone()));
        }
    }
    // Within a rank, the smaller pass number is the token that has waited longest.
    ranked.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    ranked.truncate(max_tokens);
    ranked
        .into_iter()
        .map(|(_, _, token)| token)
        .collect()
}

/// How many pricing passes a flagged pool stays out of the first buy pass. A pool can recover, so
/// the flag expires, and the next pricing pass that routes through the pool checks it again.
/// Passes for the tokens of added components run inside `min_pass_interval` and count too, so a
/// flag can expire sooner than `FLAGGED_POOL_PASSES` intervals.
const FLAGGED_POOL_PASSES: u64 = 100;

/// How many pricing passes a priced token waits before a pass offers it with no change on its
/// route. A token's dependencies leave out rival pools, so an improved rival route changes the
/// price only through this refresh. Passes for the tokens of added components count too.
const MAX_PRICE_AGE_PASSES: u64 = 100;

/// Default wall-clock budget for a pricing pass after its first buy pass.
///
/// `DEFAULT_MAX_TOKENS_PER_PASS` is what sizes a pass. This only stops one pathological token
/// from holding the derived chain, so it stays generous.
const DEFAULT_PASS_BUDGET: Duration = Duration::from_secs(30);

/// Default time a pass waits for the pass before it.
///
/// A reverse sell makes a capped pass cost tens of milliseconds, so a pass can run on most
/// blocks. The interval keeps a chain with sub-second blocks from pricing on every one of them.
const DEFAULT_MIN_PASS_INTERVAL: Duration = Duration::from_secs(1);

/// Default cap on the tokens one pass attempts.
///
/// At 500 tokens per pass, the whole market of about 2200 tokens on Base at `min-tvl 1` refreshes
/// in 5 passes.
const DEFAULT_MAX_TOKENS_PER_PASS: usize = 500;

/// Default cap on the sell solves one pass runs. At about 12 ms each, 200 sell solves take about
/// 2.4 s.
const DEFAULT_MAX_SELL_SOLVES_PER_PASS: usize = 200;

/// Basis points in a whole.
const BPS_DENOMINATOR: u32 = 10_000;

/// Default `min_sell_out_bps`: a reverse sell must return half the probe amount.
const DEFAULT_MIN_SELL_OUT_BPS: u32 = 5_000;

impl Default for TokenGasPriceComputation {
    fn default() -> Self {
        Self {
            gas_token: Address::zero(20), // ETH address
            max_hops: crate::solver::defaults::PRICING_MAX_HOPS,
            probe_amount: BigUint::from(10u64).pow(18), // 1 ETH
            pass_budget: DEFAULT_PASS_BUDGET,
            max_tokens_per_pass: DEFAULT_MAX_TOKENS_PER_PASS,
            max_sell_solves_per_pass: DEFAULT_MAX_SELL_SOLVES_PER_PASS,
            min_pass_interval: DEFAULT_MIN_PASS_INTERVAL,
            min_sell_out_bps: DEFAULT_MIN_SELL_OUT_BPS,
            pass_history: Arc::new(Mutex::new(PassHistory::default())),
        }
    }
}

impl TokenGasPriceComputation {
    /// Creates a computation with explicit parameters.
    ///
    /// The pass interval is off. A test drives `compute` directly, one call per block it wants
    /// priced, so the deployment default would skip most of them; a test that means to exercise
    /// the interval sets its own.
    #[cfg(test)]
    pub fn new(gas_token: Address, max_hops: usize, probe_amount: BigUint) -> Self {
        Self {
            gas_token,
            max_hops,
            probe_amount,
            min_pass_interval: Duration::ZERO,
            ..Self::default()
        }
    }

    /// Sets the wall-clock budget for a pricing pass after its first buy pass.
    pub fn with_pass_budget(self, pass_budget: Duration) -> Self {
        Self { pass_budget, ..self }
    }

    /// Sets how many tokens one pass may attempt.
    pub fn with_max_tokens_per_pass(self, max_tokens_per_pass: usize) -> Self {
        Self { max_tokens_per_pass, ..self }
    }

    /// Sets how many sell solves one pass may run.
    pub fn with_max_sell_solves_per_pass(self, max_sell_solves_per_pass: usize) -> Self {
        Self { max_sell_solves_per_pass, ..self }
    }

    /// Sets how long after a pass starts the next one may start. `Duration::ZERO` lets a pass
    /// run on every market event.
    pub fn with_min_pass_interval(self, min_pass_interval: Duration) -> Self {
        Self { min_pass_interval, ..self }
    }

    /// Takes the pass history's lock.
    ///
    /// A poisoned lock is taken anyway: the guarded value only schedules passes, a panic cannot
    /// leave it half-written, and refusing to price tokens over it would be worse.
    fn lock_pass_history(&self) -> MutexGuard<'_, PassHistory> {
        match self.pass_history.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Says how much of a pass may run now, and starts the interval again for a whole one.
    ///
    /// A whole pass also waits for a block other than `last_pass_block`, unless the interval is
    /// zero.
    ///
    /// `must_solve` runs a whole pass whatever the interval says, for a caller that has nothing
    /// stored to serve instead. `arrivals` only earns the tokens that arrived: those cannot be
    /// quoted until they are priced, but pricing them is not a reason to start the interval
    /// again or to rank the rest of the market again.
    fn start_pass(&self, block: u64, arrivals: bool, must_solve: bool) -> PassSlot {
        let mut state = self.lock_pass_history();
        let now = Instant::now();
        let interval_elapsed = state
            .last_pass_started
            .is_none_or(|started| now.duration_since(started) >= self.min_pass_interval);
        let new_block = self.min_pass_interval.is_zero() ||
            state
                .last_pass_block
                .is_none_or(|last_block| last_block != block);
        if (interval_elapsed && new_block) || must_solve {
            state.last_pass_started = Some(now);
            state.last_pass_block = Some(block);
            return PassSlot::Due;
        }
        if arrivals {
            return PassSlot::ArrivalsOnly;
        }
        PassSlot::Deferred
    }

    /// Snapshots what the next pass ranks its candidates by, and holds this block's arrivals
    /// until a pass attempts them.
    ///
    /// `priced` says which tokens have a stored price; the stamps come from the pass history. The
    /// arrived rank covers the tokens of every component that has arrived since the last pass
    /// attempted them, not only this block's: the cap can cut the rank short, and a component
    /// that arrives is in no stored dependency set, so a token it carries that loses the rank
    /// has nothing left to point at it.
    fn pass_priority(
        &self,
        arrived: FxHashSet<Address>,
        priced: FxHashSet<Address>,
    ) -> PassPriority {
        let mut state = self.lock_pass_history();
        state.pending_arrivals.extend(arrived);
        PassPriority {
            arrived: state.pending_arrivals.clone(),
            priced,
            last_attempted: state.last_attempted.clone(),
            passes: state.passes,
        }
    }

    /// Takes the number of the pass that just ran, stamps every token it attempted, drops the
    /// attempted tokens from the arrived rank, and forgets the tokens that left the market so
    /// neither set can grow without bound. Stores the components the pass flagged under its
    /// number, and forgets the flags that are `FLAGGED_POOL_PASSES` passes old.
    ///
    /// Attempted means selected and not carried: a token the cap or the deadline left out comes
    /// back as unattempted and keeps whatever stamp it had. Priced and failed tokens are stamped
    /// alike, because the stamp answers "when did a pass last look at this token", which is what
    /// rotation needs from it.
    fn record_pass(
        &self,
        selected: &FxHashSet<Address>,
        outcome: &PricingPassOutcome,
        universe: &FxHashSet<Address>,
    ) {
        let mut state = self.lock_pass_history();
        let pass = state.passes.wrapping_add(1);
        state.passes = pass;
        for token in selected {
            if outcome.unattempted.contains(token) {
                continue;
            }
            state
                .last_attempted
                .insert(token.clone(), pass);
            state.pending_arrivals.remove(token);
        }
        state
            .last_attempted
            .retain(|token, _| universe.contains(token));
        state
            .pending_arrivals
            .retain(|token| universe.contains(token));
        for component_id in &outcome.new_flagged_components {
            state
                .flagged_components
                .insert(component_id.clone(), pass);
        }
        state
            .flagged_components
            .retain(|_, flagged_in| pass.wrapping_sub(*flagged_in) < FLAGGED_POOL_PASSES);
    }

    /// Returns the components the next pricing pass leaves out of its first buy pass.
    fn read_flagged_components(&self) -> FxHashSet<ComponentId> {
        self.lock_pass_history()
            .flagged_components
            .keys()
            .cloned()
            .collect()
    }

    /// Sets the longest route the algorithm may build.
    pub fn with_max_hops(self, max_hops: usize) -> Self {
        Self { max_hops, ..self }
    }

    /// Sets the gas token address.
    pub fn with_gas_token(self, gas_token: Address) -> Self {
        Self { gas_token, ..self }
    }

    /// Solves one capped pass, ranking every token in the market and attempting the best
    /// `max_tokens_per_pass` of them.
    ///
    /// `changed` ranks rather than narrows; `select_pass_tokens` says why.
    ///
    /// Tokens that were bought but found no sell route back come back as failed items. Tokens
    /// the gas token cannot reach at all are only counted (logged at debug): unreachable is the
    /// normal state for much of the topology. Tokens the pass did not attempt, whether the cap
    /// or the deadline left them out, come back as unattempted so callers keep their previous
    /// prices.
    async fn solve_token_prices(
        &self,
        market: &MarketData,
        changed: Option<&FxHashSet<Address>>,
        priority: &PassPriority,
        scope: PassScope,
        max_tokens: usize,
    ) -> Result<PricingPassOutcome, ComputationError> {
        let (topology, block) = {
            let guard = market.read().await;
            let block = guard
                .last_updated()
                .map(|b| b.number())
                .unwrap_or(0);
            let topology =
                remove_components(guard.base_market_state(), guard.component_topology(), &is_pamm);
            (topology, block)
        };

        let universe = self.tokens_to_price(&topology);
        let ordered = select_pass_tokens(&universe, changed, priority, scope, max_tokens);
        // Nothing to attempt: every token either has a price that no change points at, or is
        // already stamped and ranked behind one. Returning before the graph is built keeps a
        // quiet block from cloning the topology and walking a subgraph toward an empty target
        // set, which yields no subgraph at all and would look like a failure in the log.
        if ordered.is_empty() {
            return Ok(PricingPassOutcome {
                prices: FxHashMap::default(),
                routes: FxHashMap::default(),
                block,
                failed_items: Vec::new(),
                unattempted: universe,
                new_flagged_components: FxHashSet::default(),
            });
        }

        let mut graph_manager = PetgraphStableDiGraphManager::new();
        graph_manager.initialize_graph(&topology);

        // Gas-aware scoring would need the prices this computation produces, so it stays off.
        //
        // The timeout bounds one solve (the buy pass, or one token's sell), not the whole run.
        // It is deliberately not the quote timeout: this runs in the background, and a solve
        // that needs a few hundred milliseconds should price its token, not vary with machine
        // load. One second is a pathological-case bound — typical solves finish in
        // milliseconds — so one degenerate token cannot stall the block's derived chain.
        let config = AlgorithmConfig::new(1, self.max_hops, Duration::from_secs(1), None)
            .map_err(|error| ComputationError::InvalidConfiguration(error.to_string()))?
            .with_gas_aware(false);
        let algorithm = BellmanFordAlgorithm::with_config(config);

        // Tokens the cap left out are unattempted, exactly like ones the deadline cuts: they
        // keep their previous price and stay visible to the next pass, which ranks them by the
        // pass that last touched them.
        let selected: FxHashSet<Address> = ordered.iter().cloned().collect();
        let mut capped_out: FxHashSet<Address> = FxHashSet::default();
        for token in &universe {
            if !selected.contains(token) {
                capped_out.insert(token.clone());
            }
        }
        let graph = graph_manager.graph();

        // One snapshot serves every buy pass and every sell solve. The subgraph is walked one hop
        // beyond `max_hops`: a sell route of `max_hops` hops can start from a token that far
        // from the gas token, and the walk must include that token's outgoing edges. On a
        // filtered (incremental) run the walk is also pruned toward the filter tokens, so
        // re-solving a handful of tokens snapshots their candidate routes, not the market.
        let Some(ctx) = algorithm
            .build_context_from_source_token(
                graph,
                market.clone(),
                &self.gas_token,
                self.max_hops + 1,
                Some(&selected),
            )
            .await
        else {
            // No subgraph around the gas token means nothing was attempted this block: the
            // tokens come back unattempted so they keep their previous prices, exactly as if
            // the deadline had cut them off.
            warn!(unattempted = universe.len(), "no subgraph around the gas token");
            return Ok(PricingPassOutcome {
                prices: FxHashMap::default(),
                routes: FxHashMap::default(),
                block,
                failed_items: Vec::new(),
                unattempted: universe,
                new_flagged_components: FxHashSet::default(),
            });
        };

        // Stamp the result with the snapshot's block, not the earlier topology read — the feed
        // can advance between the two locks, and every price is computed against the snapshot.
        let block = ctx
            .market_data
            .last_updated()
            .map_or(block, |b| b.number());

        // A pricing pass is pure CPU work: every step simulates swaps against the owned snapshot
        // and never awaits. It runs on a blocking thread so that it does not hold a runtime worker
        // for its whole length.
        let computation = self.clone();
        let flagged_components = self.read_flagged_components();
        let span = Span::current();
        let mut outcome = tokio::task::spawn_blocking(move || {
            let _entered = span.enter();
            let mut pass = PricingPassState::new(
                &algorithm,
                graph_manager.graph(),
                ctx,
                &computation,
                flagged_components,
            );
            pass.price_tokens(ordered, block)
        })
        .await
        .map_err(|join_error| {
            ComputationError::Internal(format!("token pricing pass did not complete: {join_error}"))
        })?;
        outcome.unattempted.extend(capped_out);
        self.record_pass(&selected, &outcome, &universe);
        Ok(outcome)
    }

    /// Every token in the graph but the gas token.
    fn tokens_to_price(
        &self,
        topology: &FxHashMap<ComponentId, Vec<Address>>,
    ) -> FxHashSet<Address> {
        topology
            .values()
            .flatten()
            .filter(|token| *token != &self.gas_token)
            .cloned()
            .collect()
    }

    /// Offers the pass the tokens a change could have moved, ranked so the cap cuts the least
    /// valuable last.
    ///
    /// The selection is not a bound. On a dense market it is almost every priced token, for the
    /// reason the module doc gives. `max_tokens_per_pass` is the bound, and the rank decides
    /// which tokens get under it.
    ///
    /// `Ok(None)` when there is nothing stored to select from, so seeding is needed.
    async fn update_prices(
        &self,
        market: &MarketData,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
        scope: PassScope,
    ) -> Result<Option<ComputationOutput<TokenGasPrices>>, ComputationError> {
        // The dependency map holds one `path_components` set per priced token, so cloning it to
        // change a handful of entries is this pass's dominant cost on a large market. Read it by
        // reference here, and edit the stored map in place further down.
        let (tokens_to_recompute, new_tokens, mut priority, existing_prices) = {
            let store_guard = store.read().await;
            let Some(existing_deps) = store_guard.token_prices_deps() else {
                return Ok(None);
            };
            let Some(existing_prices) = store_guard.token_prices().cloned() else {
                return Ok(None);
            };

            let changed_components = changed.all_changed_ids();
            let mut tokens_to_recompute: FxHashSet<Address> = existing_deps
                .iter()
                .filter(|(_, entry)| {
                    !entry
                        .path_components
                        .is_disjoint(&changed_components)
                })
                .map(|(addr, _)| addr.clone())
                .collect();

            // Every token of a component that arrived, whether or not it already has a price. A
            // new component is in no stored dependency set, so the filter above cannot find the
            // tokens whose routes it may have just improved, and a token it introduces cannot be
            // quoted until it has a price. These rank ahead of everything else in the pass.
            let mut arrived: FxHashSet<Address> = FxHashSet::default();
            let mut new_tokens = 0usize;
            for token in changed.added.values().flatten() {
                // Every added pool carries the gas token, which no pass ever attempts.
                if *token == self.gas_token || !arrived.insert(token.clone()) {
                    continue;
                }
                if !existing_deps.contains_key(token) {
                    new_tokens += 1;
                }
                tokens_to_recompute.insert(token.clone());
            }
            let priced = existing_deps.keys().cloned().collect();
            let priority = self.pass_priority(arrived, priced);
            (tokens_to_recompute, new_tokens, priority, existing_prices)
        };

        // Only a token with no price breaks the interval. Such a token cannot be quoted at all
        // until it is priced. A new pool for a token that already has a price can only improve
        // that price, and the token keeps the arrived rank until a pass attempts it, so the
        // next whole pass takes it: worth a pass, not worth one of its own.
        if scope == PassScope::ArrivalsOnly {
            priority.keep_unpriced_arrivals();
            if priority.arrived.is_empty() {
                Span::current().record("updated_token_prices", existing_prices.len());
                return Ok(Some(ComputationOutput::with_failures(existing_prices, Vec::new())));
            }
        }

        // No early return on an empty change set. The pass is capped, and its spare ranks go to
        // the tokens that have gone longest without a price — including any that have none at
        // all, which no change would ever point at.
        debug!(
            affected_tokens = tokens_to_recompute.len(),
            new_tokens,
            total_tokens = existing_prices.len(),
            "incremental token price recomputation"
        );

        // Every pass is capped, whatever earned it. A Tycho protocol resync reports that
        // protocol's whole pool set as new on one block, and lag recovery coalesces the
        // arrivals of every drained event into one change set, so the size of an arrivals pass
        // is not bounded by anything the feed is willing to promise.
        let solved = self
            .solve_token_prices(
                market,
                Some(&tokens_to_recompute),
                &priority,
                scope,
                self.max_tokens_per_pass,
            )
            .await?;

        let mut result = existing_prices;
        drop_stale_failures(&mut *store.write().await, &solved);
        let edited = store
            .write()
            .await
            .edit_token_prices_deps(solved.block, |deps| {
                // Everything the pass priced, not only what the change pointed at: the pass
                // ranks over the whole market, so it reaches tokens this block's change never
                // named — which is the only way a token that has no price gets one.
                let mut history = self.lock_pass_history();
                warn_on_price_jumps(deps, &history.routes, &solved);
                for (token, entry) in &solved.prices {
                    result.insert(token.clone(), entry.price.clone());
                    deps.insert(token.clone(), entry.clone());
                }
                // Attempted and produced nothing: the routes are gone, so the price is too.
                // A token the cap or the deadline left out is unattempted and keeps its entry —
                // nothing is known about it this block, and dropping it would lose its stamp.
                // The gas token is in neither set because no pass ever attempts it.
                let dropped: Vec<Address> = deps
                    .keys()
                    .filter(|token| {
                        **token != self.gas_token &&
                            !solved.prices.contains_key(*token) &&
                            !solved.unattempted.contains(*token)
                    })
                    .cloned()
                    .collect();
                for token in dropped {
                    result.remove(&token);
                    deps.remove(&token);
                }
                history.record_routes(&solved.routes, deps);
            });
        if !edited {
            warn!("token price dependencies vanished between the read and the write");
            // Returning `None` sends `compute` to seeding, which rebuilds them.
            return Ok(None);
        }
        Span::current().record("updated_token_prices", result.len());

        Ok(Some(ComputationOutput::with_failures(result, solved.failed_items)))
    }

    /// Prices the whole market and writes the stored set from scratch.
    ///
    /// Runs only when there is nothing stored to select from, which in practice means startup.
    /// It is the one path that can create the dependency map and seed the gas token, which is
    /// why it is separate from `update_prices` rather than a flag on it.
    async fn seed_all_prices(
        &self,
        market: &MarketData,
        store: &SharedDerivedDataRef,
    ) -> Result<ComputationOutput<TokenGasPrices>, ComputationError> {
        // Seeding is cut off by the same budget as any other pass, so it needs the same rank:
        // unpriced tokens first, then the ones that have gone longest without a pass. On a market
        // where one budget cannot price everything, that is what makes a lag-recovery seed cover
        // the set instead of re-pricing the same head. Nothing arrives on this path, but a
        // seeding pass offers every token anyway, so the rank is all this changes.
        let priority = {
            let store_guard = store.read().await;
            let priced = store_guard
                .token_prices_deps()
                .map(|deps| deps.keys().cloned().collect())
                .unwrap_or_default();
            self.pass_priority(FxHashSet::default(), priced)
        };
        // No cap. Seeding only runs when there is nothing stored to select from, which is
        // startup, and `derived_data_ready` flips as soon as it stores anything.
        // Capping it would report a pod ready with a fraction of the market priced and leave the
        // rest to fill over the following passes. `pass_budget` still bounds it.
        let solved = self
            .solve_token_prices(market, None, &priority, PassScope::Whole, usize::MAX)
            .await?;
        drop_stale_failures(&mut *store.write().await, &solved);

        let mut token_prices_with_deps = TokenPricesWithDeps::default();
        let mut token_prices = TokenGasPrices::default();
        for (token, entry) in solved.prices {
            token_prices.insert(token.clone(), entry.price.clone());
            token_prices_with_deps.insert(token, entry);
        }

        // Tokens the deadline cut off keep their previous entry, dependencies included: they
        // stay served and stay visible to `update_prices`, which re-prices them when one of
        // their pools changes or when the unpriced rank reaches them. Dropping them here would
        // leave them unpriced with nothing pointing at them.
        if !solved.unattempted.is_empty() {
            let store_guard = store.read().await;
            if let Some(previous) = store_guard.token_prices_deps() {
                for token in &solved.unattempted {
                    let Some(entry) = previous.get(token) else {
                        continue;
                    };
                    token_prices_with_deps.insert(token.clone(), entry.clone());
                    token_prices.insert(token.clone(), entry.price.clone());
                }
            }
        }

        // The gas token is 1:1 with itself and needs no route.
        let gas_token_price =
            Price { numerator: self.probe_amount.clone(), denominator: self.probe_amount.clone() };
        token_prices_with_deps.insert(
            self.gas_token.clone(),
            TokenPriceEntry {
                price: gas_token_price.clone(),
                path_components: FxHashSet::default(),
            },
        );
        token_prices.insert(self.gas_token.clone(), gas_token_price);

        self.lock_pass_history()
            .record_routes(&solved.routes, &token_prices_with_deps);
        store
            .write()
            .await
            .set_token_prices_deps(token_prices_with_deps, solved.block);

        debug!(priced = token_prices.len() - 1, "token price computation complete");
        Span::current().record("updated_token_prices", token_prices.len());

        Ok(ComputationOutput::with_failures(token_prices, solved.failed_items))
    }
}

#[async_trait]
impl DerivedComputation for TokenGasPriceComputation {
    type Output = TokenGasPrices;

    const ID: ComputationId = "token_prices";

    fn requirements(&self) -> ComputationRequirements {
        // Reads no derived data, so no other computation has to precede this one.
        ComputationRequirements::none()
    }

    fn persist(
        store: &mut DerivedData,
        output: ComputationOutput<Self::Output>,
        block: u64,
        is_full_recompute: bool,
    ) {
        store.set_token_prices(output.data, output.failed_items, block, is_full_recompute);
    }

    #[instrument(
        level = "debug",
        skip_all,
        fields(computation_id = Self::ID, updated_token_prices)
    )]
    async fn compute(
        &self,
        market: &MarketData,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
    ) -> Result<ComputationOutput<Self::Output>, ComputationError> {
        let block = market
            .read()
            .await
            .last_updated()
            .map_or(0, |block| block.number());
        // A component arriving earns a pass inside the interval, but only for the tokens it
        // carries; a full recompute earns a whole one, having nothing stored to serve instead.
        let scope =
            match self.start_pass(block, !changed.added.is_empty(), changed.is_full_recompute) {
                PassSlot::Due => PassScope::Whole,
                PassSlot::ArrivalsOnly => PassScope::ArrivalsOnly,
                PassSlot::Deferred => {
                    let stored = {
                        let store_guard = store.read().await;
                        store_guard.token_prices().cloned()
                    };
                    // Nothing stored means nothing to serve, so the interval cannot defer this
                    // block: seeding runs instead.
                    let Some(prices) = stored else {
                        return self
                            .seed_all_prices(market, store)
                            .await;
                    };
                    Span::current().record("updated_token_prices", prices.len());
                    return Ok(ComputationOutput::with_failures(prices, Vec::new()));
                }
            };

        // Startup and lag recovery have nothing stored to select from, so they offer every token
        // to the pass. So does a block whose selection finds nothing stored. Every other block
        // selects, and the cap bounds what the pass gets through, whatever the block brought:
        // the tokens of an added component are ranked first, not exempted from the cap.
        if !changed.is_full_recompute {
            if let Some(result) = self
                .update_prices(market, store, changed, scope)
                .await?
            {
                return Ok(result);
            }
        }

        self.seed_all_prices(market, store)
            .await
    }
}

#[cfg(test)]
mod tests {
    use num_traits::ToPrimitive;
    use tycho_simulation::tycho_core::{
        models::token::Token, simulation::protocol_sim::ProtocolSim,
    };

    use super::*;
    use crate::{
        algorithm::test_utils::{
            component, component_with_protocol, setup_market_weighted, setup_market_weighted_boxed,
            token, MockProtocolSim,
        },
        derived::store::DerivedData,
        fallback::FALLBACK_PREFIX,
    };

    const PROBE_AMOUNT: u128 = 1_000_000_000_000_000_000;

    fn computation_for(gas_token: &Address) -> TokenGasPriceComputation {
        TokenGasPriceComputation::new(gas_token.clone(), 3, BigUint::from(PROBE_AMOUNT))
    }

    fn ratio(price: &Price) -> f64 {
        let numerator = price
            .numerator
            .to_f64()
            .expect("price numerator fits in f64");
        let denominator = price
            .denominator
            .to_f64()
            .expect("price denominator fits in f64");
        numerator / denominator
    }

    async fn prices_for(
        gas_token: &Token,
        pools: Vec<(&str, &Token, &Token, MockProtocolSim)>,
    ) -> TokenGasPrices {
        let (market, _) = setup_market_weighted(pools);
        let store = DerivedData::new_shared();
        computation_for(&gas_token.address)
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail")
            .data
    }

    #[tokio::test]
    async fn test_price_via_direct_pool() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // A fee-free symmetric pool buys and sells back at the same rate, so the mean is that
        // rate exactly.
        let prices =
            prices_for(&eth, vec![("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0))]).await;

        assert!((ratio(&prices[&usdc.address]) - 2000.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_gas_token_price() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let prices =
            prices_for(&eth, vec![("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0))]).await;

        // The gas token needs no route: its entry is the probe amount over itself, not merely
        // any equal pair.
        let eth_price = prices
            .get(&eth.address)
            .expect("gas token should be priced");
        assert_eq!(eth_price.numerator, BigUint::from(PROBE_AMOUNT));
        assert_eq!(eth_price.denominator, BigUint::from(PROBE_AMOUNT));
    }

    #[tokio::test]
    async fn test_price_with_pool_fee() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // A 1% fee splits the two implied rates apart:
        //   buy_out  = 1e18 * 2000 * 0.99          → buy_price  = 1980
        //   sell_out = buy_out / 2000 * 0.99       → sell_price = 2000 / 0.99
        // so the fee's round-trip cost shows up in the price.
        let prices = prices_for(
            &eth,
            vec![("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0).with_fee(0.01))],
        )
        .await;

        let expected_mean = (1980.0 + 2000.0 / 0.99) / 2.0;
        assert!((ratio(&prices[&usdc.address]) - expected_mean).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_price_via_parallel_pools() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Two pools on the same pair. The fee-free pool has the tighter spread, but the
        // 1%-fee pool delivers more output on the buy — a ranking by spread would pick
        // "tight", a ranking by output must pick "wide". The sell goes back through "wide":
        //   buy:  1e18 ETH * 2500 * 0.99         → 2475e18 USDC
        //   sell: 2475e18 USDC / 2500 * 0.99     → 0.9801e18 ETH
        //   mid = 2475 * (1 + 0.9801) / (2 * 0.9801)
        let prices = prices_for(
            &eth,
            vec![
                ("tight", &eth, &usdc, MockProtocolSim::new(2000.0)),
                ("wide", &eth, &usdc, MockProtocolSim::new(2500.0).with_fee(0.01)),
            ],
        )
        .await;

        assert!((ratio(&prices[&usdc.address]) - 2_500.126_262_626).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_price_via_multi_hop_route() {
        let eth = token(0, "ETH");
        let mid = token(2, "MID");
        let target = token(3, "TARGET");

        let prices = prices_for(
            &eth,
            vec![
                ("eth_mid", &eth, &mid, MockProtocolSim::new(2.0)),
                ("mid_target", &mid, &target, MockProtocolSim::new(3.0)),
            ],
        )
        .await;

        // 1 ETH buys 2 MID buys 6 TARGET, and the fee-free reverse returns the ETH, so the
        // mean is 6.
        assert!((ratio(&prices[&target.address]) - 6.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_price_at_exactly_max_hops() {
        // FAR sits exactly max_hops (3) from ETH. Its sell needs FAR's outgoing edges, which
        // lie one hop beyond the buy reach, so the shared snapshot must be walked one hop
        // further than the algorithm routes.
        let eth = token(0, "ETH");
        let mid = token(2, "MID");
        let next = token(3, "NEXT");
        let far = token(4, "FAR");

        let prices = prices_for(
            &eth,
            vec![
                ("eth_mid", &eth, &mid, MockProtocolSim::new(2.0)),
                ("mid_next", &mid, &next, MockProtocolSim::new(2.0)),
                ("next_far", &next, &far, MockProtocolSim::new(2.0)),
            ],
        )
        .await;

        // 1 ETH buys 8 FAR over three fee-free doublings, and the reverse returns the ETH.
        assert!((ratio(&prices[&far.address]) - 8.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_seeding_pass_past_deadline() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");
        let (market, _) =
            setup_market_weighted(vec![("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0))]);
        let store = DerivedData::new_shared();
        computation_for(&eth.address)
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");

        // A full recompute whose deadline expires immediately attempts nothing; every token
        // must keep its previous price rather than vanish with no later pass to restore it.
        let output = computation_for(&eth.address)
            .with_pass_budget(Duration::ZERO)
            .compute(
                &market,
                &store,
                &ChangedComponents { is_full_recompute: true, ..ChangedComponents::default() },
            )
            .await
            .expect("pricing must not fail");

        assert!((ratio(&output.data[&usdc.address]) - 2000.0).abs() < 1e-6);
        assert!(output.failed_items.is_empty(), "an unattempted token is not a failure");
        let guard = store.read().await;
        assert!(
            guard
                .token_prices_deps()
                .expect("deps are stored")
                .contains_key(&usdc.address),
            "carried tokens must stay visible to incremental invalidation"
        );
    }

    #[tokio::test]
    async fn test_vanished_gas_subgraph() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");
        let aaa = token(2, "AAA");
        let (market, _) = setup_market_weighted(vec![
            ("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0)),
            ("usdc_aaa", &usdc, &aaa, MockProtocolSim::new(1.0)),
        ]);
        let store = DerivedData::new_shared();
        computation_for(&eth.address)
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");

        // The gas token's only pool disappears: the pass cannot start, so it must report the
        // still-listed tokens as unattempted — keeping their previous prices — rather than as
        // attempted and failed, which would drop them.
        market
            .write()
            .await
            .remove_components(["eth_usdc".to_string()].iter());
        let output = computation_for(&eth.address)
            .compute(
                &market,
                &store,
                &ChangedComponents { is_full_recompute: true, ..ChangedComponents::default() },
            )
            .await
            .expect("pricing must not fail");

        assert!((ratio(&output.data[&usdc.address]) - 2000.0).abs() < 1e-6);
        assert!((ratio(&output.data[&aaa.address]) - 2000.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_incremental_solve_past_deadline() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");
        let (market, _) =
            setup_market_weighted(vec![("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0))]);
        let store = DerivedData::new_shared();
        let full = computation_for(&eth.address)
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        // The manager persists between runs; without this `update_prices` bails out on the
        // missing stored prices and the test would exercise seeding twice.
        TokenGasPriceComputation::persist(&mut *store.write().await, full, 1, true);

        // The pool's state changes, marking USDC for re-pricing, but the deadline expires
        // before it is attempted: the previous price must survive.
        let output = computation_for(&eth.address)
            .with_pass_budget(Duration::ZERO)
            .compute(
                &market,
                &store,
                &ChangedComponents {
                    updated: vec!["eth_usdc".to_string()],
                    ..ChangedComponents::default()
                },
            )
            .await
            .expect("pricing must not fail");

        assert!((ratio(&output.data[&usdc.address]) - 2000.0).abs() < 1e-6);
        let guard = store.read().await;
        assert!(
            guard
                .token_prices_deps()
                .expect("deps are stored")
                .contains_key(&usdc.address),
            "a carried token must stay visible to incremental invalidation"
        );
    }

    #[tokio::test]
    async fn test_incremental_resolves_only_affected_tokens() {
        use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

        // Each token's buy route is its own pool, so each token's price depends on exactly that
        // pool.
        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        let bbb = token(2, "BBB");
        // Rates whose reciprocals are exact in the mock's 1e12 fixed-point scaling, so the
        // sell leg introduces no rounding.
        let (market, _) = setup_market_weighted(vec![
            ("eth_aaa", &eth, &aaa, MockProtocolSim::new(2000.0)),
            ("eth_bbb", &eth, &bbb, MockProtocolSim::new(2500.0)),
        ]);
        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 1, BigUint::from(PROBE_AMOUNT));
        let store = DerivedData::new_shared();
        let full = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        // The manager persists between runs; the incremental path reads the stored prices.
        TokenGasPriceComputation::persist(&mut *store.write().await, full, 1, true);

        // Both pools move, but only eth_aaa is reported as changed: AAA must re-price
        // against the new state while BBB keeps its stored price.
        market.write().await.update_states([
            ("eth_aaa".to_string(), Box::new(MockProtocolSim::new(4000.0)) as Box<dyn ProtocolSim>),
            ("eth_bbb".to_string(), Box::new(MockProtocolSim::new(5000.0)) as Box<dyn ProtocolSim>),
        ]);
        let output = computation
            .compute(
                &market,
                &store,
                &ChangedComponents {
                    updated: vec!["eth_aaa".to_string()],
                    ..ChangedComponents::default()
                },
            )
            .await
            .expect("pricing must not fail");

        assert!((ratio(&output.data[&aaa.address]) - 4000.0).abs() < 1e-6, "AAA re-solved");
        assert!((ratio(&output.data[&bbb.address]) - 2500.0).abs() < 1e-6, "BBB untouched");
    }

    /// An arrival re-prices the tokens it carries and nothing else.
    ///
    /// The market moves under both tokens here. Only the token of the arrived component is a
    /// candidate, so only that token picks the move up.
    #[tokio::test]
    async fn test_arrived_component_reprices_only_its_own_tokens() {
        use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        let bbb = token(2, "BBB");
        let (market, _) = setup_market_weighted(vec![
            ("eth_aaa", &eth, &aaa, MockProtocolSim::new(2000.0)),
            ("eth_bbb", &eth, &bbb, MockProtocolSim::new(2500.0)),
        ]);
        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 1, BigUint::from(PROBE_AMOUNT));
        let store = DerivedData::new_shared();
        let full = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, full, 1, true);

        market.write().await.update_states([
            ("eth_aaa".to_string(), Box::new(MockProtocolSim::new(4000.0)) as Box<dyn ProtocolSim>),
            ("eth_bbb".to_string(), Box::new(MockProtocolSim::new(5000.0)) as Box<dyn ProtocolSim>),
        ]);

        // A component arrives carrying AAA, which already has a price. It is re-priced anyway:
        // a new component can be the better route for a token that already has one, and nothing
        // else in the selection points at it. BBB's component did not change, so BBB is not
        // selected and keeps what it had, even though its market moved too.
        let mut added = FxHashMap::default();
        added.insert("eth_aaa_v2".to_string(), vec![eth.address.clone(), aaa.address.clone()]);
        let output = computation
            .compute(&market, &store, &ChangedComponents { added, ..ChangedComponents::default() })
            .await
            .expect("pricing must not fail");

        assert!(
            (ratio(&output.data[&aaa.address]) - 4000.0).abs() < 1e-6,
            "the arrived component's token is re-priced"
        );
        assert!(
            (ratio(&output.data[&bbb.address]) - 2500.0).abs() < 1e-6,
            "a token no change points at keeps its stored price"
        );
    }

    #[tokio::test]
    async fn test_added_token_is_priced_immediately() {
        use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        let ccc = token(2, "CCC");
        let (market, _) =
            setup_market_weighted(vec![("eth_aaa", &eth, &aaa, MockProtocolSim::new(2000.0))]);
        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 1, BigUint::from(PROBE_AMOUNT));
        let store = DerivedData::new_shared();
        let full = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, full, 1, true);
        assert!(!store
            .read()
            .await
            .token_prices()
            .expect("priced")
            .contains_key(&ccc.address));

        // CCC's pool is listed now, exactly as the feed would deliver it.
        {
            let mut guard = market.write().await;
            guard.upsert_tokens([ccc.clone()]);
            guard.upsert_components([component("eth_ccc", &[eth.clone(), ccc.clone()])]);
            guard.update_states([(
                "eth_ccc".to_string(),
                Box::new(MockProtocolSim::new(5000.0)) as Box<dyn ProtocolSim>,
            )]);
        }

        let mut added = FxHashMap::default();
        added.insert("eth_ccc".to_string(), vec![eth.address.clone(), ccc.address.clone()]);
        let output = computation
            .compute(&market, &store, &ChangedComponents { added, ..ChangedComponents::default() })
            .await
            .expect("pricing must not fail");

        assert!(
            (ratio(&output.data[&ccc.address]) - 5000.0).abs() < 1e-6,
            "a token arriving with a new component is priced on the block it arrives"
        );
        assert!(
            (ratio(&output.data[&aaa.address]) - 2000.0).abs() < 1e-6,
            "an unrelated token is left alone"
        );
    }

    #[tokio::test]
    async fn test_removed_component_unprices_its_token_incrementally() {
        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        let bbb = token(2, "BBB");
        let (market, _) = setup_market_weighted(vec![
            ("eth_aaa", &eth, &aaa, MockProtocolSim::new(2000.0)),
            ("eth_bbb", &eth, &bbb, MockProtocolSim::new(2500.0)),
        ]);
        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 1, BigUint::from(PROBE_AMOUNT));
        let store = DerivedData::new_shared();
        let full = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, full, 1, true);

        market
            .write()
            .await
            .remove_components([&"eth_aaa".to_string()]);

        let output = computation
            .compute(
                &market,
                &store,
                &ChangedComponents {
                    removed: vec!["eth_aaa".to_string()],
                    ..ChangedComponents::default()
                },
            )
            .await
            .expect("pricing must not fail");

        assert!(!output.data.contains_key(&aaa.address), "AAA is unpriced, not stale");
        assert!(
            !store
                .read()
                .await
                .token_prices_deps()
                .expect("deps stored")
                .contains_key(&aaa.address),
            "AAA's dependencies are dropped with its price"
        );
        assert!((ratio(&output.data[&bbb.address]) - 2500.0).abs() < 1e-6, "BBB untouched");
    }

    /// The rank a pass is cut against: arrived, then unpriced, then what a change points at.
    #[test]
    fn test_select_pass_tokens_ranks_arrived_unpriced_then_changed() {
        let arrived_token = token(1, "AAA").address;
        let unpriced = token(2, "BBB").address;
        let changed_token = token(3, "CCC").address;
        let untouched = token(4, "DDD").address;
        let universe: FxHashSet<Address> =
            [arrived_token.clone(), unpriced.clone(), changed_token.clone(), untouched.clone()]
                .into_iter()
                .collect();
        // The arrived token is also the most recently attempted, and the untouched one is the
        // stalest, so rank has to beat staleness in both directions.
        let priority = PassPriority {
            arrived: [arrived_token.clone()]
                .into_iter()
                .collect(),
            priced: [arrived_token.clone(), changed_token.clone(), untouched.clone()]
                .into_iter()
                .collect(),
            last_attempted: [
                (arrived_token.clone(), 9),
                (changed_token.clone(), 5),
                (untouched.clone(), 1),
            ]
            .into_iter()
            .collect(),
            passes: 9,
        };
        let changed: FxHashSet<Address> = [changed_token.clone()]
            .into_iter()
            .collect();

        let ordered =
            select_pass_tokens(&universe, Some(&changed), &priority, PassScope::Whole, usize::MAX);

        assert_eq!(
            ordered,
            vec![arrived_token, unpriced, changed_token],
            "arrived first, then the unpriced, then what the change points at; a priced token \
             no change points at is not a candidate however stale it is"
        );
    }

    /// A newly listed token cannot be quoted until it has a price, so it goes before an arrived
    /// token that already has one. The cap can cut the arrived rank short, which is when the
    /// order inside that rank decides which token waits.
    #[test]
    fn test_select_pass_tokens_ranks_unpriced_arrivals_first() {
        let with_price = token(1, "AAA").address;
        let without_price = token(2, "BBB").address;
        let universe: FxHashSet<Address> = [with_price.clone(), without_price.clone()]
            .into_iter()
            .collect();
        let priority = PassPriority {
            arrived: [with_price.clone(), without_price.clone()]
                .into_iter()
                .collect(),
            priced: [with_price.clone()]
                .into_iter()
                .collect(),
            last_attempted: [(with_price.clone(), 7)]
                .into_iter()
                .collect(),
            passes: 0,
        };

        let ordered =
            select_pass_tokens(&universe, None, &priority, PassScope::ArrivalsOnly, usize::MAX);

        assert_eq!(
            ordered,
            vec![without_price, with_price],
            "the arrival with no price is attempted first"
        );
    }

    /// The cap is what sizes a pass, so it must drop the lowest ranks and keep the rest.
    #[test]
    fn test_select_pass_tokens_caps_the_selection() {
        let unpriced = token(1, "AAA").address;
        let stale = token(2, "BBB").address;
        let fresh = token(3, "CCC").address;
        let universe: FxHashSet<Address> = [unpriced.clone(), stale.clone(), fresh.clone()]
            .into_iter()
            .collect();
        let priority = PassPriority {
            arrived: FxHashSet::default(),
            priced: [stale.clone(), fresh.clone()]
                .into_iter()
                .collect(),
            last_attempted: [(stale.clone(), 1), (fresh.clone(), 8)]
                .into_iter()
                .collect(),
            passes: 0,
        };

        let ordered = select_pass_tokens(&universe, None, &priority, PassScope::Whole, 2);

        assert_eq!(ordered, vec![unpriced, stale], "the cap keeps the two highest ranks");
    }

    #[test]
    fn test_select_pass_tokens_stale_price() {
        let stale = token(1, "AAA").address;
        let recent = token(2, "BBB").address;
        let universe: FxHashSet<Address> = [stale.clone(), recent.clone()]
            .into_iter()
            .collect();
        let passes = MAX_PRICE_AGE_PASSES + 10;
        let priority = PassPriority {
            arrived: FxHashSet::default(),
            priced: universe.clone(),
            last_attempted: [(stale.clone(), 10), (recent.clone(), 11)]
                .into_iter()
                .collect(),
            passes,
        };

        let ordered = select_pass_tokens(
            &universe,
            Some(&FxHashSet::default()),
            &priority,
            PassScope::Whole,
            usize::MAX,
        );

        assert_eq!(ordered, vec![stale]);
    }

    /// A block inside the interval serves the stored prices instead of running a pass.
    #[tokio::test]
    async fn test_a_pass_inside_the_interval_serves_the_stored_prices() {
        use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        let (market, _) =
            setup_market_weighted(vec![("eth_aaa", &eth, &aaa, MockProtocolSim::new(2000.0))]);
        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 1, BigUint::from(PROBE_AMOUNT))
                .with_min_pass_interval(Duration::from_secs(3600));
        let store = DerivedData::new_shared();
        let full = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, full, 1, true);

        market.write().await.update_states([(
            "eth_aaa".to_string(),
            Box::new(MockProtocolSim::new(4000.0)) as Box<dyn ProtocolSim>,
        )]);

        let changed = ChangedComponents {
            updated: vec!["eth_aaa".to_string()],
            ..ChangedComponents::default()
        };
        let output = computation
            .compute(&market, &store, &changed)
            .await
            .expect("pricing must not fail");

        assert!(
            (ratio(&output.data[&aaa.address]) - 2000.0).abs() < 1e-6,
            "the block inside the interval serves the stored price, not the moved market"
        );
    }

    /// An arriving component runs a pass however recently the last one ran, because the tokens
    /// it brings cannot be quoted until they have a price.
    #[tokio::test]
    async fn test_an_arriving_component_runs_a_pass_inside_the_interval() {
        use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        let ccc = token(2, "CCC");
        let (market, _) =
            setup_market_weighted(vec![("eth_aaa", &eth, &aaa, MockProtocolSim::new(2000.0))]);
        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 1, BigUint::from(PROBE_AMOUNT))
                .with_min_pass_interval(Duration::from_secs(3600));
        let store = DerivedData::new_shared();
        let full = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, full, 1, true);

        {
            let mut guard = market.write().await;
            guard.upsert_tokens([ccc.clone()]);
            guard.upsert_components([component("eth_ccc", &[eth.clone(), ccc.clone()])]);
            guard.update_states([(
                "eth_ccc".to_string(),
                Box::new(MockProtocolSim::new(5000.0)) as Box<dyn ProtocolSim>,
            )]);
        }

        let mut added = FxHashMap::default();
        added.insert("eth_ccc".to_string(), vec![eth.address.clone(), ccc.address.clone()]);
        let output = computation
            .compute(&market, &store, &ChangedComponents { added, ..ChangedComponents::default() })
            .await
            .expect("pricing must not fail");

        assert!(
            (ratio(&output.data[&ccc.address]) - 5000.0).abs() < 1e-6,
            "an arriving token is priced although the interval has not elapsed"
        );
    }

    /// A new pool for a token that already has a price does not break the interval.
    ///
    /// It can improve that token's price, which the next whole pass does. Running a pass for it
    /// on the block it arrives would let a chain that lists pools on most blocks set the pace.
    #[tokio::test]
    async fn test_a_pool_for_a_priced_token_waits_for_the_interval() {
        use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        let (market, _) =
            setup_market_weighted(vec![("eth_aaa", &eth, &aaa, MockProtocolSim::new(2000.0))]);
        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 1, BigUint::from(PROBE_AMOUNT))
                .with_min_pass_interval(Duration::from_secs(3600));
        let store = DerivedData::new_shared();
        let seeded = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, seeded, 1, true);

        let mut added = FxHashMap::default();
        {
            let mut guard = market.write().await;
            guard.upsert_components([component("eth_aaa_2", &[eth.clone(), aaa.clone()])]);
            guard.update_states([(
                "eth_aaa_2".to_string(),
                Box::new(MockProtocolSim::new(9000.0)) as Box<dyn ProtocolSim>,
            )]);
            added.insert("eth_aaa_2".to_string(), vec![eth.address.clone(), aaa.address.clone()]);
        }

        let output = computation
            .compute(&market, &store, &ChangedComponents { added, ..ChangedComponents::default() })
            .await
            .expect("pricing must not fail");

        assert!(
            (ratio(&output.data[&aaa.address]) - 2000.0).abs() < 1e-6,
            "AAA has a price already, so its new pool runs no pass inside the interval"
        );
    }

    /// An arrived token the cap cuts keeps the arrived rank until a pass attempts it.
    ///
    /// The component that carried it is in no stored dependency set, so nothing else would ever
    /// point at the token again.
    #[tokio::test]
    async fn test_a_cut_arrival_is_attempted_by_the_next_pass() {
        use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        let ccc = token(2, "CCC");
        let (market, _) =
            setup_market_weighted(vec![("eth_aaa", &eth, &aaa, MockProtocolSim::new(2000.0))]);
        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 1, BigUint::from(PROBE_AMOUNT))
                .with_max_tokens_per_pass(1);
        let store = DerivedData::new_shared();
        let seeded = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, seeded, 1, true);

        // A new pool prices AAA far better, and a new token arrives on the same block. The cap
        // of one takes the token with no price, so AAA is cut.
        let mut added = FxHashMap::default();
        {
            let mut guard = market.write().await;
            guard.upsert_tokens([ccc.clone()]);
            guard.upsert_components([
                component("eth_aaa_2", &[eth.clone(), aaa.clone()]),
                component("eth_ccc", &[eth.clone(), ccc.clone()]),
            ]);
            guard.update_states([
                (
                    "eth_aaa_2".to_string(),
                    Box::new(MockProtocolSim::new(9000.0)) as Box<dyn ProtocolSim>,
                ),
                (
                    "eth_ccc".to_string(),
                    Box::new(MockProtocolSim::new(5000.0)) as Box<dyn ProtocolSim>,
                ),
            ]);
            added.insert("eth_aaa_2".to_string(), vec![eth.address.clone(), aaa.address.clone()]);
            added.insert("eth_ccc".to_string(), vec![eth.address.clone(), ccc.address.clone()]);
        }
        let cut = computation
            .compute(&market, &store, &ChangedComponents { added, ..ChangedComponents::default() })
            .await
            .expect("pricing must not fail");
        assert!(
            (ratio(&cut.data[&aaa.address]) - 2000.0).abs() < 1e-6,
            "the cap took the token with no price, so AAA keeps its old price"
        );
        TokenGasPriceComputation::persist(&mut *store.write().await, cut, 2, false);

        // Nothing changes on this block. Only the arrived rank can reach AAA: its stored
        // dependencies do not name the new pool.
        let next = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");

        // The new pool only carries the buy leg: selling 9000 AAA back returns more through
        // the deeper old pool, so the mean of the two rates lands between them.
        assert!(
            ratio(&next.data[&aaa.address]) > 2000.0,
            "the next pass attempts the cut arrival and re-prices it through its new pool"
        );
    }

    /// An arrivals pass is capped like any other.
    ///
    /// A Tycho protocol resync reports that protocol's whole pool set as new on one block, and
    /// lag recovery coalesces the arrivals of every drained event into one change set, so
    /// "one pass per arriving token" is not a bound.
    #[tokio::test]
    async fn test_an_arrivals_pass_is_capped() {
        use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        let arriving = [token(2, "BBB"), token(3, "CCC"), token(4, "DDD")];
        let (market, _) =
            setup_market_weighted(vec![("eth_aaa", &eth, &aaa, MockProtocolSim::new(2000.0))]);
        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 1, BigUint::from(PROBE_AMOUNT))
                .with_min_pass_interval(Duration::from_secs(3600))
                .with_max_tokens_per_pass(2);
        let store = DerivedData::new_shared();
        let seeded = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, seeded, 1, true);

        let mut added = FxHashMap::default();
        {
            let mut guard = market.write().await;
            for arrival in &arriving {
                let id = format!("eth_{}", arrival.symbol.to_lowercase());
                guard.upsert_tokens([arrival.clone()]);
                guard.upsert_components([component(&id, &[eth.clone(), arrival.clone()])]);
                guard.update_states([(
                    id.clone(),
                    Box::new(MockProtocolSim::new(5000.0)) as Box<dyn ProtocolSim>,
                )]);
                added.insert(id, vec![eth.address.clone(), arrival.address.clone()]);
            }
        }

        let output = computation
            .compute(&market, &store, &ChangedComponents { added, ..ChangedComponents::default() })
            .await
            .expect("pricing must not fail");

        let priced = arriving
            .iter()
            .filter(|arrival| {
                output
                    .data
                    .contains_key(&arrival.address)
            })
            .count();
        assert_eq!(
            priced, 2,
            "three components arrive inside the interval and the cap of two holds; the third              waits for the next pass"
        );
    }

    /// An arrival earns a pass inside the interval but does not restart it.
    ///
    /// Restarting it would let the feed set the pace: on a chain that lists a pool most blocks,
    /// every block would become a whole pass and the interval would bound nothing.
    ///
    /// This drives the rule directly rather than through `compute`, because the difference only
    /// shows when the arrival lands *inside* the interval and reaching that through a pricing
    /// pass needs sleeps long enough to be worth more than the case is.
    #[test]
    fn test_an_arrival_does_not_restart_the_interval() {
        let eth = token(0, "ETH").address;
        let computation = TokenGasPriceComputation::new(eth, 1, BigUint::from(PROBE_AMOUNT))
            .with_min_pass_interval(Duration::from_millis(400));

        assert_eq!(
            computation.start_pass(1, false, false),
            PassSlot::Due,
            "the first pass is due, nothing has run"
        );
        assert_eq!(
            computation.start_pass(2, false, false),
            PassSlot::Deferred,
            "a block straight after one inside the interval waits"
        );

        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            computation.start_pass(3, true, false),
            PassSlot::ArrivalsOnly,
            "an arrival inside the interval earns a pass for its own tokens"
        );

        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            computation.start_pass(4, false, false),
            PassSlot::Due,
            "450ms after the only whole pass the interval has elapsed, so the arrival in the \
             middle of it did not restart it"
        );
    }

    /// A caller with nothing stored to serve gets a whole pass whatever the interval says.
    #[test]
    fn test_a_must_solve_pass_ignores_the_interval() {
        let eth = token(0, "ETH").address;
        let computation = TokenGasPriceComputation::new(eth, 1, BigUint::from(PROBE_AMOUNT))
            .with_min_pass_interval(Duration::from_secs(3600));

        assert_eq!(computation.start_pass(1, false, false), PassSlot::Due);
        assert_eq!(computation.start_pass(2, false, false), PassSlot::Deferred);
        assert_eq!(
            computation.start_pass(2, false, true),
            PassSlot::Due,
            "a full recompute has nothing to serve instead, so it runs a whole pass"
        );
    }

    #[test]
    fn test_a_whole_pass_waits_for_a_new_block() {
        let eth = token(0, "ETH").address;
        let computation = TokenGasPriceComputation::new(eth, 1, BigUint::from(PROBE_AMOUNT))
            .with_min_pass_interval(Duration::from_millis(1));

        assert_eq!(computation.start_pass(5, false, false), PassSlot::Due);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(computation.start_pass(5, false, false), PassSlot::Deferred);
        assert_eq!(computation.start_pass(6, false, false), PassSlot::Due);
    }

    /// Tokens that can never be priced must not hold a slot in every pass.
    ///
    /// An unpriced token is a candidate on every pass, because nothing else would point at it.
    /// Ranking those at a fixed 0 put them ahead of every priced token for ever, so a market
    /// with more unpriceable tokens than the cap would never refresh a price again. Measured on
    /// Base at `min-tvl 1` before the fix: 47 of every 100 slots went to the same failing
    /// tokens. Stamping the pass that attempted them is what makes them rotate.
    #[tokio::test]
    async fn test_unpriceable_tokens_do_not_hold_a_slot_every_pass() {
        use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        // Three tokens that are bought but never sellable, against a cap of two. Ranked at a
        // fixed 0 they would fill every pass and AAA would never be re-priced.
        let dead = |n: u8| {
            (
                format!("eth_dead{n}"),
                token(10 + n, "DEAD"),
                MockProtocolSim::new(0.5).with_liquidity(600_000_000_000_000_000),
            )
        };
        let (dead0, dead1, dead2) = (dead(0), dead(1), dead(2));
        let (market, _) = setup_market_weighted(vec![
            ("eth_aaa", &eth, &aaa, MockProtocolSim::new(2000.0)),
            (dead0.0.as_str(), &eth, &dead0.1, dead0.2.clone()),
            (dead1.0.as_str(), &eth, &dead1.1, dead1.2.clone()),
            (dead2.0.as_str(), &eth, &dead2.1, dead2.2.clone()),
        ]);
        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 1, BigUint::from(PROBE_AMOUNT))
                .with_max_tokens_per_pass(2);
        let store = DerivedData::new_shared();

        let seeded = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, seeded, 1, true);

        // AAA's pool moves. It has to be re-priced within a few passes rather than waiting
        // behind three tokens that fail every time.
        market.write().await.update_states([(
            "eth_aaa".to_string(),
            Box::new(MockProtocolSim::new(4000.0)) as Box<dyn ProtocolSim>,
        )]);
        let mut repriced = false;
        for block in 2..=5 {
            let changed = ChangedComponents {
                updated: vec!["eth_aaa".to_string()],
                ..ChangedComponents::default()
            };
            let output = computation
                .compute(&market, &store, &changed)
                .await
                .expect("pricing must not fail");
            if (ratio(&output.data[&aaa.address]) - 4000.0).abs() < 1e-6 {
                repriced = true;
                break;
            }
            TokenGasPriceComputation::persist(&mut *store.write().await, output, block, false);
        }

        assert!(repriced, "the priced token is refreshed rather than starved by failing tokens");
    }

    /// A token with no price must stay reachable when no change points at it.
    ///
    /// A token that fails to price has no stored dependency set, so the intersection that drives
    /// the incremental path can never name it again. A selection built from stored dependencies
    /// alone would leave it unpriced for as long as the process ran: a Base run at `min-tvl 1`
    /// stopped at 97 tokens of 2211 that way. Only the unpriced rank reaches it.
    #[tokio::test]
    async fn test_unpriced_tokens_are_reached_without_a_change_pointing_at_them() {
        use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        let oneway = token(2, "ONEWAY");
        // ONEWAY's pool caps output per direction, so it can be bought but not sold back and
        // the first pass cannot price it. AAA prices normally.
        let (market, _) = setup_market_weighted(vec![
            ("eth_aaa", &eth, &aaa, MockProtocolSim::new(2000.0)),
            (
                "eth_oneway",
                &eth,
                &oneway,
                MockProtocolSim::new(0.5).with_liquidity(600_000_000_000_000_000),
            ),
        ]);
        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 1, BigUint::from(PROBE_AMOUNT));
        let store = DerivedData::new_shared();

        let full = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, full, 1, true);
        assert!(
            !store
                .read()
                .await
                .token_prices()
                .expect("prices are stored")
                .contains_key(&oneway.address),
            "the unsellable token starts with no price and no dependency set"
        );

        // Its pool gains liquidity and becomes sellable. Nothing in the stored dependency map
        // names ONEWAY, so only the unpriced rank can offer it to the pass.
        market.write().await.update_states([(
            "eth_oneway".to_string(),
            Box::new(MockProtocolSim::new(3000.0)) as Box<dyn ProtocolSim>,
        )]);
        let changed = ChangedComponents {
            updated: vec!["eth_oneway".to_string()],
            ..ChangedComponents::default()
        };
        let output = computation
            .compute(&market, &store, &changed)
            .await
            .expect("pricing must not fail");

        assert!(
            output
                .data
                .contains_key(&oneway.address),
            "a token that had no price is priced once it becomes sellable"
        );
        assert!(
            (ratio(&output.data[&aaa.address]) - 2000.0).abs() < 1e-6,
            "the already-priced token keeps its price"
        );
    }

    /// The seeding pass runs without the cap, so readiness means the market is priced rather
    /// than one pass worth of it.
    #[tokio::test]
    async fn test_the_seeding_pass_is_not_capped() {
        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        let bbb = token(2, "BBB");
        let ccc = token(3, "CCC");
        let (market, _) = setup_market_weighted(vec![
            ("eth_aaa", &eth, &aaa, MockProtocolSim::new(2000.0)),
            ("eth_bbb", &eth, &bbb, MockProtocolSim::new(2500.0)),
            ("eth_ccc", &eth, &ccc, MockProtocolSim::new(3000.0)),
        ]);
        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 1, BigUint::from(PROBE_AMOUNT))
                .with_max_tokens_per_pass(1);
        let store = DerivedData::new_shared();

        let output = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");

        for (name, address) in [("AAA", &aaa.address), ("BBB", &bbb.address), ("CCC", &ccc.address)]
        {
            assert!(
                output.data.contains_key(address),
                "{name} is priced by the startup solve although the cap is 1"
            );
        }
    }

    /// The stamp a pass leaves is what the next pass ranks by, so it must tell a token the pass
    /// re-priced apart from one it left alone. Without that, a budget-limited pass has nothing
    /// to rotate on and would attempt the same head every time.
    #[tokio::test]
    async fn test_a_pass_stamps_only_the_tokens_it_reprices() {
        fn stamp(computation: &TokenGasPriceComputation, token: &Address) -> u64 {
            computation
                .lock_pass_history()
                .last_attempted
                .get(token)
                .copied()
                .expect("the token was attempted")
        }

        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        let bbb = token(2, "BBB");
        let (market, _) = setup_market_weighted(vec![
            ("eth_aaa", &eth, &aaa, MockProtocolSim::new(2000.0)),
            ("eth_bbb", &eth, &bbb, MockProtocolSim::new(2500.0)),
        ]);
        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 1, BigUint::from(PROBE_AMOUNT));
        let store = DerivedData::new_shared();

        let full = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, full, 1, true);
        let first_pass = stamp(&computation, &aaa.address);
        assert_eq!(stamp(&computation, &bbb.address), first_pass, "one pass priced both");

        // Only AAA's pool changed, so only AAA is selected.
        let changed = ChangedComponents {
            updated: vec!["eth_aaa".to_string()],
            ..ChangedComponents::default()
        };
        let incremental = computation
            .compute(&market, &store, &changed)
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, incremental, 2, false);

        assert!(
            stamp(&computation, &aaa.address) > first_pass,
            "the re-priced token carries the newer pass"
        );
        assert_eq!(
            stamp(&computation, &bbb.address),
            first_pass,
            "a token the pass left alone keeps its stamp, so it now ranks ahead of AAA"
        );
    }

    #[tokio::test]
    async fn test_incremental_with_disjoint_change_keeps_all_prices() {
        use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");
        let (market, _) =
            setup_market_weighted(vec![("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0))]);
        let computation = computation_for(&eth.address);
        let store = DerivedData::new_shared();
        let full = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, full, 1, true);

        // The pool's state moves, but the changed set names no stored dependency, so the
        // incremental path must return the stored prices without re-solving anything.
        market.write().await.update_states([(
            "eth_usdc".to_string(),
            Box::new(MockProtocolSim::new(9000.0)) as Box<dyn ProtocolSim>,
        )]);
        let output = computation
            .compute(
                &market,
                &store,
                &ChangedComponents {
                    updated: vec!["unrelated_pool".to_string()],
                    ..ChangedComponents::default()
                },
            )
            .await
            .expect("pricing must not fail");

        assert!((ratio(&output.data[&usdc.address]) - 2000.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_token_without_sell_route_is_a_failed_item() {
        let eth = token(0, "ETH");
        let oneway = token(1, "ONEWAY");

        // The mock's liquidity caps output per direction, making the pool one-way: buying
        // 1 ETH outputs 0.5e18 ONEWAY (under the cap), selling that back would output
        // 1e18 ETH (over it). Bought but not sellable must be reported, not counted.
        let (market, _) = setup_market_weighted(vec![(
            "eth_oneway",
            &eth,
            &oneway,
            MockProtocolSim::new(0.5).with_liquidity(600_000_000_000_000_000),
        )]);
        let store = DerivedData::new_shared();
        let output = computation_for(&eth.address)
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");

        assert!(
            !output
                .data
                .contains_key(&oneway.address),
            "an unsellable token has no price"
        );
        assert_eq!(output.failed_items.len(), 1);
        assert_eq!(output.failed_items[0].key, oneway.address.to_string());
        let FailedItemError::MissingSellRoute(reason) = &output.failed_items[0].error else {
            panic!("expected MissingSellRoute, got {:?}", output.failed_items[0].error);
        };
        assert!(!reason.is_empty(), "the failure carries why the sell solve failed");
    }

    #[tokio::test]
    async fn test_incremental_removes_unsellable_token() {
        use tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim;

        let eth = token(0, "ETH");
        let oneway = token(1, "ONEWAY");
        let (market, _) =
            setup_market_weighted(vec![("eth_oneway", &eth, &oneway, MockProtocolSim::new(0.5))]);
        let store = DerivedData::new_shared();
        let computation = computation_for(&eth.address);
        let full = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, full, 1, true);

        // The pool turns one-way: the liquidity cap lets the 0.5e18 buy through but blocks the
        // 1e18 sell back. A token that was priced and then lost its sell route must stop being
        // served — dropped from the prices and from the dependency map both.
        market.write().await.update_states([(
            "eth_oneway".to_string(),
            Box::new(MockProtocolSim::new(0.5).with_liquidity(600_000_000_000_000_000))
                as Box<dyn ProtocolSim>,
        )]);
        let output = computation
            .compute(
                &market,
                &store,
                &ChangedComponents {
                    updated: vec!["eth_oneway".to_string()],
                    ..ChangedComponents::default()
                },
            )
            .await
            .expect("pricing must not fail");

        assert!(
            !output
                .data
                .contains_key(&oneway.address),
            "an unsellable token has no price"
        );
        let guard = store.read().await;
        assert!(
            !guard
                .token_prices_deps()
                .expect("deps are stored")
                .contains_key(&oneway.address),
            "a dropped token must leave the dependency map too"
        );
        assert!(
            !computation
                .lock_pass_history()
                .routes
                .contains_key(&oneway.address),
            "a dropped token must leave the stored routes too"
        );
    }

    #[rstest::rstest]
    #[case::floor_on(DEFAULT_MIN_SELL_OUT_BPS, (2.0 + 2.0 / (2.0 / 1.9)) / 2.0)]
    #[case::floor_off(0, (2.0 + 2.0 / 0.25) / 2.0)]
    #[tokio::test]
    async fn test_low_sell_out(#[case] min_sell_out_bps: u32, #[case] expected: f64) {
        // "lossy" buys the most X, 2 per ETH, but its 50% fee sells those 2 X back for 0.25 ETH.
        // Below the floor, a sell solve sells them through "fair" for 2 / 1.9 ETH instead.
        let eth = token(0, "ETH");
        let x = token(1, "X");
        let (market, _) = setup_market_weighted(vec![
            ("lossy", &eth, &x, MockProtocolSim::new(4.0).with_fee(0.5)),
            ("fair", &eth, &x, MockProtocolSim::new(1.9)),
        ]);
        let store = DerivedData::new_shared();
        let computation =
            TokenGasPriceComputation { min_sell_out_bps, ..computation_for(&eth.address) };

        let prices = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail")
            .data;

        assert!((ratio(&prices[&x.address]) - expected).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_pamm_left_out() {
        // The pAMM quotes 3 X per ETH, the plain pool 2. Pricing leaves the pAMM out.
        let eth = token(0, "ETH");
        let x = token(1, "X");
        let (market, _) = setup_market_weighted(vec![
            ("pamm", &eth, &x, MockProtocolSim::new(3.0)),
            ("plain", &eth, &x, MockProtocolSim::new(2.0)),
        ]);
        market
            .write()
            .await
            .upsert_components([component_with_protocol(
                "pamm",
                &format!("{FALLBACK_PREFIX}fermiswap"),
                &[eth.clone(), x.clone()],
            )]);
        let store = DerivedData::new_shared();

        let prices = computation_for(&eth.address)
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail")
            .data;

        assert!((ratio(&prices[&x.address]) - 2.0).abs() < 1e-6);
        let guard = store.read().await;
        let deps = &guard
            .token_prices_deps()
            .expect("deps are stored")[&x.address]
            .path_components;
        assert_eq!(deps, &FxHashSet::from_iter(["plain".to_string()]));
    }

    #[tokio::test]
    async fn test_deps_rival_route() {
        // USDC prices via the direct pool, bought and sold back through it. The worse
        // ETH->MID->USDC route priced nothing, so its pools are not dependencies.
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");
        let mid = token(2, "MID");

        let (market, _) = setup_market_weighted(vec![
            ("direct", &eth, &usdc, MockProtocolSim::new(2000.0)),
            ("eth_mid", &eth, &mid, MockProtocolSim::new(1.0)),
            ("mid_usdc", &mid, &usdc, MockProtocolSim::new(1500.0)),
        ]);
        let store = DerivedData::new_shared();
        computation_for(&eth.address)
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");

        let guard = store.read().await;
        let deps = &guard
            .token_prices_deps()
            .expect("deps are stored")[&usdc.address]
            .path_components;
        assert_eq!(deps, &FxHashSet::from_iter(["direct".to_string()]));
    }

    #[tokio::test]
    async fn test_route_and_amounts() {
        // USDC is reachable only through ETH->MID->USDC. Fee-free pools buy 1500 USDC with one
        // ETH probe and sell it back for the whole probe. A mock pool quotes its rate in address
        // order, so MID takes the lower address.
        let eth = token(0, "ETH");
        let mid = token(1, "MID");
        let usdc = token(2, "USDC");
        let (market, _) = setup_market_weighted(vec![
            ("eth_mid", &eth, &mid, MockProtocolSim::new(1.0)),
            ("mid_usdc", &mid, &usdc, MockProtocolSim::new(1500.0)),
        ]);
        let store = DerivedData::new_shared();
        let computation = computation_for(&eth.address);
        computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");

        let history = computation.lock_pass_history();
        let route = &history.routes[&usdc.address];
        assert_eq!(route.components, vec!["eth_mid".to_string(), "mid_usdc".to_string()]);
        let buy_amount_out = route
            .buy_amount_out
            .to_f64()
            .expect("amount fits in f64");
        let sell_amount_out = route
            .sell_amount_out
            .to_f64()
            .expect("amount fits in f64");
        assert!((buy_amount_out / PROBE_AMOUNT as f64 - 1500.0).abs() < 1e-6);
        assert!((sell_amount_out / PROBE_AMOUNT as f64 - 1.0).abs() < 1e-6);
    }

    #[rstest::rstest]
    #[case::up_ten_percent(100, 110, Some(1.1))]
    #[case::down_ten_percent(110, 100, Some(100.0 / 110.0))]
    #[case::up_five_percent(100, 105, None)]
    #[case::unchanged(100, 100, None)]
    #[case::stored_zero(0, 100, None)]
    #[case::new_zero(100, 0, None)]
    fn test_price_jump_ratio(
        #[case] stored_numerator: u64,
        #[case] new_numerator: u64,
        #[case] expected: Option<f64>,
    ) {
        let price = |numerator: u64| Price {
            numerator: BigUint::from(numerator),
            denominator: BigUint::from(100u64),
        };

        let jump = price_jump_ratio(&price(stored_numerator), &price(new_numerator));

        match (jump, expected) {
            (Some(jump), Some(expected)) => assert!((jump - expected).abs() < 1e-9),
            (None, None) => {}
            (jump, expected) => panic!("expected {expected:?}, got {jump:?}"),
        }
    }

    /// A pool that fails the reverse sell check: one whose output cap stops the reverse sell, or
    /// one whose two spot prices disagree.
    fn flagged_pool(kind: &str, spot_price: f64) -> Box<dyn ProtocolSim> {
        match kind {
            // Pays out at most 0.75 ETH, so the X that one probe buys cannot sell back for 1 ETH.
            "output_cap" => {
                Box::new(MockProtocolSim::new(spot_price).with_liquidity(PROBE_AMOUNT / 4 * 3))
            }
            "skewed_spot" => {
                Box::new(MockProtocolSim::new(spot_price).with_reverse_spot_factor(0.5))
            }
            _ => unreachable!("no flagged pool kind {kind}"),
        }
    }

    #[rstest::rstest]
    #[case::output_cap("output_cap")]
    #[case::skewed_spot("skewed_spot")]
    #[tokio::test]
    async fn test_flagged_pool(#[case] kind: &str) {
        // One probe of 1 ETH buys 0.5 X through the flagged pool, and 0.4 X through
        // ETH->MID->X. The pass flags the pool and prices X through ETH->MID->X at 0.4.
        let eth = token(0, "ETH");
        let x = token(1, "X");
        let mid = token(2, "MID");
        let (market, _) = setup_market_weighted_boxed(vec![
            ("flagged", &eth, &x, flagged_pool(kind, 0.5)),
            ("eth_mid", &eth, &mid, Box::new(MockProtocolSim::new(1.0))),
            ("mid_x", &mid, &x, Box::new(MockProtocolSim::new(2.5))),
        ]);
        let store = DerivedData::new_shared();

        let prices = computation_for(&eth.address)
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail")
            .data;

        assert!((ratio(&prices[&x.address]) - 0.4).abs() < 1e-9);
        let guard = store.read().await;
        let deps = &guard
            .token_prices_deps()
            .expect("deps are stored")[&x.address]
            .path_components;
        assert_eq!(deps, &FxHashSet::from_iter(["eth_mid".to_string(), "mid_x".to_string()]));
    }

    #[tokio::test]
    async fn test_failure_of_removed_token() {
        // ONEWAY cannot sell back, so the first pass stores its failure. Its only pool then
        // leaves the market, and the next pass drops the failure with it.
        let eth = token(0, "ETH");
        let oneway = token(1, "ONEWAY");
        let bbb = token(2, "BBB");
        let (market, _) = setup_market_weighted(vec![
            (
                "eth_oneway",
                &eth,
                &oneway,
                MockProtocolSim::new(0.5).with_liquidity(600_000_000_000_000_000),
            ),
            ("eth_bbb", &eth, &bbb, MockProtocolSim::new(2500.0)),
        ]);
        let store = DerivedData::new_shared();
        let computation = computation_for(&eth.address);
        let full = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, full, 1, true);
        let failure_before = store
            .read()
            .await
            .token_price_failure(&oneway.address)
            .is_some();

        market
            .write()
            .await
            .remove_components([&"eth_oneway".to_string()]);
        let removed =
            ChangedComponents { removed: vec!["eth_oneway".to_string()], ..Default::default() };
        let output = computation
            .compute(&market, &store, &removed)
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, output, 2, false);

        assert!(failure_before);
        assert!(store
            .read()
            .await
            .token_price_failure(&oneway.address)
            .is_none());
    }

    #[tokio::test]
    async fn test_token_only_a_flagged_pool_reaches() {
        // The skewed pool is the only route to Y and its swaps work, so a sell solve through it
        // prices Y at 0.5 on both pricing passes. The second pricing pass's first buy pass leaves
        // the pool out, so it must run the buy pass through every pool to reach Y.
        let eth = token(0, "ETH");
        let y = token(1, "Y");
        let (market, _) = setup_market_weighted_boxed(vec![(
            "skewed",
            &eth,
            &y,
            flagged_pool("skewed_spot", 0.5),
        )]);
        let store = DerivedData::new_shared();
        let computation = computation_for(&eth.address);
        let skewed_changed =
            ChangedComponents { updated: vec!["skewed".to_string()], ..Default::default() };

        let first = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail")
            .data;
        let second = computation
            .compute(&market, &store, &skewed_changed)
            .await
            .expect("pricing must not fail")
            .data;

        assert!((ratio(&first[&y.address]) - 0.5).abs() < 1e-9);
        assert!((ratio(&second[&y.address]) - 0.5).abs() < 1e-9);
        let guard = store.read().await;
        let deps = &guard
            .token_prices_deps()
            .expect("deps are stored")[&y.address]
            .path_components;
        assert_eq!(deps, &FxHashSet::from_iter(["skewed".to_string()]));
        let history = computation.lock_pass_history();
        assert_eq!(
            history.routes[&y.address].components,
            vec!["skewed".to_string(), "skewed".to_string()]
        );
    }

    #[tokio::test]
    async fn test_sell_solve_cap() {
        // Each skewed pool is the only route to its token, so both tokens need a sell solve, and
        // the cap of one leaves one of them unpriced.
        let eth = token(0, "ETH");
        let y = token(1, "Y");
        let z = token(2, "Z");
        let (market, _) = setup_market_weighted_boxed(vec![
            ("skewed_y", &eth, &y, flagged_pool("skewed_spot", 0.5)),
            ("skewed_z", &eth, &z, flagged_pool("skewed_spot", 0.5)),
        ]);
        let store = DerivedData::new_shared();

        let prices = computation_for(&eth.address)
            .with_max_sell_solves_per_pass(1)
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail")
            .data;

        let priced = [&y, &z]
            .iter()
            .filter(|token| prices.contains_key(&token.address))
            .count();
        assert_eq!(priced, 1);
    }

    #[test]
    fn test_flag_expiry() {
        let computation = computation_for(&token(0, "ETH").address);
        let record_pass = |new_flagged_components: FxHashSet<ComponentId>| {
            let outcome = PricingPassOutcome {
                prices: FxHashMap::default(),
                routes: FxHashMap::default(),
                block: 0,
                failed_items: Vec::new(),
                unattempted: FxHashSet::default(),
                new_flagged_components,
            };
            computation.record_pass(&FxHashSet::default(), &outcome, &FxHashSet::default());
        };
        let flagged = FxHashSet::from_iter(["pool".to_string()]);
        record_pass(flagged.clone());
        let run_passes = |count: u64| {
            for _ in 0..count {
                record_pass(FxHashSet::default());
            }
        };

        run_passes(FLAGGED_POOL_PASSES - 1);
        let before_expiry = computation.read_flagged_components();
        run_passes(1);
        let after_expiry = computation.read_flagged_components();

        assert_eq!(before_expiry, flagged);
        assert!(after_expiry.is_empty());
    }

    #[tokio::test]
    async fn test_empty_market() {
        let eth = token(0, "ETH");

        let prices = prices_for(&eth, vec![]).await;

        // No components means nothing to price, but never an error: the gas token is 1:1
        // with itself unconditionally, and that must be the whole map.
        assert_eq!(prices.len(), 1);
        assert!((ratio(&prices[&eth.address]) - 1.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn test_gas_token_outside_graph() {
        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        let bbb = token(2, "BBB");

        // Pools exist but none trades the gas token, so no subgraph can be built around it:
        // every token counts as unreachable, nothing is a failed item, and only the gas
        // token's unconditional 1:1 entry is served.
        let (market, _) =
            setup_market_weighted(vec![("aaa_bbb", &aaa, &bbb, MockProtocolSim::new(1.0))]);
        let store = DerivedData::new_shared();
        let output = computation_for(&eth.address)
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");

        assert_eq!(output.data.len(), 1);
        assert!((ratio(&output.data[&eth.address]) - 1.0).abs() < 1e-9);
        assert!(output.failed_items.is_empty(), "unreachable tokens are not failures");
    }

    #[tokio::test]
    async fn test_sell_rate_price_through_flagged_pool() {
        // The skewed pool takes a 50% fee each way and is the only route to Y. One probe of
        // 1 ETH buys 0.25 Y, and the sell solve sells that back for 0.25 ETH. The sell rate is
        // 1 Y per ETH on both pricing passes; the mean of the two rates would be 0.625.
        let eth = token(0, "ETH");
        let y = token(1, "Y");
        let skewed_pool = MockProtocolSim::new(0.5)
            .with_reverse_spot_factor(0.5)
            .with_fee(0.5);
        let (market, _) = setup_market_weighted(vec![("skewed", &eth, &y, skewed_pool)]);
        let store = DerivedData::new_shared();
        let computation = computation_for(&eth.address);
        let skewed_changed =
            ChangedComponents { updated: vec!["skewed".to_string()], ..Default::default() };

        let first = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail")
            .data;
        let second = computation
            .compute(&market, &store, &skewed_changed)
            .await
            .expect("pricing must not fail")
            .data;

        assert!((ratio(&first[&y.address]) - 1.0).abs() < 1e-9);
        assert!((ratio(&second[&y.address]) - 1.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn test_unreachable_token() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");
        let island = token(4, "ISLAND");
        let other = token(5, "OTHER");

        // ISLAND and OTHER trade only with each other, so no route reaches them from the gas token.
        let (market, _) = setup_market_weighted(vec![
            ("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0)),
            ("island_other", &island, &other, MockProtocolSim::new(1.0)),
        ]);
        let store = DerivedData::new_shared();
        let output = computation_for(&eth.address)
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");

        assert!(output.data.contains_key(&usdc.address));
        assert!(
            !output
                .data
                .contains_key(&island.address),
            "an unreachable token has no price"
        );
        assert!(
            output.failed_items.is_empty(),
            "unreachable tokens are counted, not reported as failed items"
        );
    }
}

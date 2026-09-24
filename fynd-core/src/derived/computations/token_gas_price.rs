//! Computes token prices relative to a gas token (e.g., ETH).
//!
//! Runs once per block, in the background chain of derived computations — never on the quoting
//! path. Quotes read whatever the last completed run stored, so prices lag the chain head by at
//! most the block or two a run is in flight. A token that cannot be priced is absent from the
//! map: bought but unsellable is reported as a failed item, unreachable is only counted.
//!
//! # Algorithm
//!
//! Routes are found with the same Bellman-Ford algorithm the solvers use to answer quotes, so a
//! price reflects what a trade would actually get, slippage and fees included. Each token is
//! bought with a fixed amount of gas token and sold back, and its price is the mean of the buy
//! price and the sell price — fees and slippage in, gas out, per the next paragraph. The mean's
//! round-trip bias is one-sided: it only ever understates a token's value, negligibly for deep
//! pairs and heavily for thin ones. A geometric mean would be exact under symmetric loss, but it
//! is irrational and prices are exact fractions.
//!
//! The algorithm runs with gas-aware scoring off. Off is what keeps this non-circular: gas-aware
//! scoring converts a route's gas into output-token terms, which needs the prices this computation
//! produces. Nothing here reads derived data, so token prices depend on no other computation.
//!
//! # Cost
//!
//! Buying is cheap: one pass over the graph finds the buy route to every token at once. Selling
//! dominates: each token needs its own relaxation, because each sell starts from a different
//! amount and slippage makes routes amount-dependent. All of it — the buy pass and every sell —
//! runs against one market snapshot taken when the pass starts, so both legs of every price and
//! the block the result is stored under agree. Token prices run in the same stage as spot prices
//! and a stage's outputs are stored once every computation in it returns, so a slow pass delays
//! that block's spot prices as well as its component depths and the start of the next block's
//! computations. A deadline over the sell loop, where nearly all of a pass's time goes, bounds
//! that delay: tokens it cuts off keep their previous price and stay visible to invalidation.
//! The deadline is a backstop, not what sizes a pass. The next section says what does, and why
//! the set of tokens a change points at cannot.
//!
//! # Why the pass is capped, spaced and rotated
//!
//! "Only the tokens a change affects" does not bound anything on a dense market. Every route
//! ends at the gas token, the gas token's own pools are the ones that trade every block, and
//! `path_components` names every candidate route rather than only the chosen one. The changed
//! set therefore intersects almost every stored dependency set. Measured on Base at `min-tvl 1`
//! on 2026-09-23: 46 consecutive incremental passes selected 2126 to 2134 tokens out of 2130,
//! and pricing ran at a 97.8% duty cycle, indistinguishable from re-pricing everything.
//!
//! So a pass is sized by `max_tokens_per_pass`, and `pass_budget` is only a backstop against one
//! pathological token. The cap has to be what sizes it: the market snapshot is pruned toward the
//! tokens the pass will attempt, so the set is fixed before any solving starts, and a deadline
//! cannot shape a set it only ever interrupts.
//!
//! A token is a candidate when a change points at it, when a component carrying it arrived, or
//! when it has no price. Attempted in that order, longest-unpriced first within the last group:
//!
//! 1. Tokens of components that arrived this block. A new component is in no stored dependency set,
//!    so nothing else would point at the tokens whose routes it may have just improved, and a token
//!    it introduces cannot be quoted until it has a price.
//! 2. Tokens with no price yet. This rank is what a selection built from stored dependencies cannot
//!    express: an unpriced token is in no dependency set, so no change would ever name it. Without
//!    it a capped first pass leaves the rest of the market unpriced for as long as the process runs
//!    — 97 tokens of 2211 on the Base run that found this.
//! 3. Tokens a change points at, longest-unpriced first. This is what makes a cap smaller than the
//!    candidate set safe: it rotates the tail forward instead of re-pricing one head.
//!
//! A priced token that no change points at is not a candidate at all. Its stored dependency set
//! already names every candidate route, so a rival pool becoming better does point at it.
//!
//! Tokens the cap leaves out are reported exactly like ones the deadline cut off: they keep
//! their price, their dependencies and their stamp, and they rank by that stamp next time.
//!
//! Seeding the whole market is therefore only needed when there is nothing stored to select
//! from at all, which in practice means startup: `ChangedComponents::is_full_recompute` is
//! never set outside tests, so seeding is reached through `update_prices` finding an empty
//! store rather than through the flag. Nothing else gets it, including a topology change,
//! because rank 1 covers what a topology change would once have been a full solve for. A seeding
//! pass runs without the cap, bounded only by `pass_budget`: readiness flips as soon as anything
//! is stored, so a capped one would report a pod ready with a fraction of the market priced.
//!
//! The cap bounds what one pass costs; `min_pass_interval` bounds how often one runs. Both are
//! needed. The manager starts a computation per market event, and on Base that is several a
//! second, so capping alone left pricing at a 94% duty cycle: passes 46 times cheaper, simply
//! run 22 times more often. A block inside the interval serves the stored prices and does no
//! pricing work.
//!
//! Two things run a pass whatever the interval says. A full recompute has nothing stored to
//! serve instead. A component arriving brings tokens that cannot be quoted at all until they
//! are priced, so a newly listed token is priced on the block it appears rather than at the end
//! of the interval.

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::Zero;
use petgraph::graph::NodeIndex;
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{debug, instrument, trace, warn, Span};
use tycho_simulation::{
    tycho_common::models::Address, tycho_core::simulation::protocol_sim::Price,
};

use crate::{
    algorithm::{
        bellman_ford::{BellmanFordContext, FindRouteOptions, ReachOutcome, ReachedToken},
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
    feed::market_data::MarketData,
    graph::{GraphManager, PetgraphStableDiGraphManager},
    types::{ComponentId, Order, OrderSide, RouteExclusions},
};

/// One pricing pass's solving state: a single market snapshot re-rooted for every sell.
///
/// The context is built once around the gas token, and every solve — the buy pass and each
/// token's sell — runs against it. One snapshot replaces a per-token lock and state clone,
/// and it makes the pass consistent: both legs of every price read the same block's states.
///
/// Each sell still walks its own subgraph, pruned toward the gas token — relaxation simulates
/// every edge it relaxes, and unpruned that is most of the market per token — but the pruning
/// map (`hops_to_gas`) is a single BFS shared by all of them.
struct PricingPass<'a> {
    /// The solving algorithm; its `max_hops` bounds route length and each sell's pruned walk.
    algorithm: &'a BellmanFordAlgorithm,
    graph: &'a <BellmanFordAlgorithm as Algorithm>::GraphType,
    /// The shared snapshot, re-rooted and re-pruned per sell.
    ctx: BellmanFordContext,
    /// The computation whose parameters — gas token, probe amount, budget — the pass solves with.
    computation: &'a TokenGasPriceComputation,
    /// The buy pass's result, solved at construction: every token one probe of gas token
    /// reaches, with what the best route delivers there.
    buys: ReachOutcome,
    /// The gas token's node, saved before the first reroot moves `ctx` off it.
    gas_node: NodeIndex,
    /// Hops from each node to the gas token, computed once, pruning every sell's walk.
    hops_to_gas: FxHashMap<NodeIndex, usize>,
    /// Token address → graph node, inverted once from the context, for re-rooting sells.
    token_nodes: FxHashMap<Address, NodeIndex>,
}

/// One sell leg's result: what the route delivers and what the price depends on.
struct SellLeg {
    /// What selling back to the gas token returns; never zero.
    amount_out: BigUint,
    /// Every component on any candidate route between the token and the gas token, plus the
    /// chosen route's own, defensively.
    components: FxHashSet<ComponentId>,
}

impl<'a> PricingPass<'a> {
    /// Builds the pass and runs its buy pass. Construction owns the buy pass because it is only
    /// valid before the first reroot replaces the context's subgraph — a pass in hand always
    /// carries its buys.
    fn new(
        algorithm: &'a BellmanFordAlgorithm,
        graph: &'a <BellmanFordAlgorithm as Algorithm>::GraphType,
        ctx: BellmanFordContext,
        computation: &'a TokenGasPriceComputation,
    ) -> Self {
        let buys = algorithm.reach_from_source_token(&ctx, &computation.probe_amount);
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
        Self { algorithm, graph, ctx, computation, buys, gas_node, hops_to_gas, token_nodes }
    }

    /// Prices every token the budget allows, one sell relaxation each — the pass's dominant
    /// cost. Pure CPU work: callers run it on a blocking thread.
    /// `tokens_to_price` is an order, not a set: the caller puts the tokens that must not be
    /// dropped by the deadline at the front.
    fn sell_loop(&mut self, tokens_to_price: Vec<Address>, block: u64) -> PricingPassOutcome {
        let deadline = Instant::now() + self.computation.pass_budget;
        let mut prices = FxHashMap::default();
        let mut failed_items = Vec::new();
        let mut unattempted = FxHashSet::default();
        let mut unreachable_tokens = 0usize;
        let mut remaining = tokens_to_price.into_iter();
        for token in &mut remaining {
            if Instant::now() >= deadline {
                unattempted.insert(token);
                break;
            }
            // A token the buy pass never reached is counted, not failed: unreachable is the
            // normal state of much of the topology, and a failed item each would be allocated,
            // logged, and broadcast to every worker every block. But a cut-short buy pass says
            // nothing about reachability, so its missing tokens are carried exactly like a
            // deadline cut-off — price and dependencies intact.
            let Some(buy_leg) = self.buys.reached.remove(&token) else {
                if self.buys.timed_out {
                    unattempted.insert(token);
                } else {
                    unreachable_tokens += 1;
                }
                continue;
            };
            match self.price_token(&token, &buy_leg) {
                Ok(priced) => {
                    prices.insert(token, priced);
                }
                Err(error) => failed_items.push(FailedItem { key: token.to_string(), error }),
            }
        }
        unattempted.extend(remaining);
        if unattempted.is_empty() {
            debug!(
                priced = prices.len(),
                failed = failed_items.len(),
                unreachable = unreachable_tokens,
                block,
                "token pricing pass complete"
            );
        } else {
            warn!(
                priced = prices.len(),
                failed = failed_items.len(),
                unreachable = unreachable_tokens,
                unattempted = unattempted.len(),
                buy_pass_timed_out = self.buys.timed_out,
                block,
                "token pricing pass cut short; unattempted tokens keep previous prices"
            );
        }

        PricingPassOutcome { prices, block, failed_items, unattempted }
    }

    /// Prices one token as the arithmetic mean of its buy price and its sell price, kept as an
    /// exact fraction, with the components that must re-price it when they change. The mean's
    /// round-trip bias only ever prices a token low, hardest on thin pairs — see the module doc.
    ///
    /// The component set covers every candidate route between the token and the gas token, not
    /// just the two chosen ones: a rival pool can move and become the better route, and only a
    /// full recompute would ever notice if it were not in the set.
    ///
    /// A token that cannot be sold back is an error, not a price: a buy rate alone would flatter
    /// a token that is expensive to exit, and prices must stay comparable across tokens.
    fn price_token(
        &mut self,
        token: &Address,
        buy_leg: &ReachedToken,
    ) -> Result<TokenPriceEntry, FailedItemError> {
        let SellLeg { amount_out: sell_out, mut components } =
            self.solve_sell_leg(token, buy_leg.amount_out.clone())?;
        // The legs are discarded after the mean; this is the only place their divergence —
        // sell_out under the probe amount is the round-trip loss — can be observed.
        trace!(%token, buy_out = %buy_leg.amount_out, sell_out = %sell_out, "token priced");
        // The buy path is a candidate path, so extending is defensive: it keeps the stored
        // dependencies correct even if the walk and the relaxation ever disagree.
        components.extend(buy_leg.components.iter().cloned());

        let mid_price = Price {
            numerator: &buy_leg.amount_out * (&self.computation.probe_amount + &sell_out),
            denominator: BigUint::from(2u8) * &self.computation.probe_amount * sell_out,
        };
        Ok(TokenPriceEntry { price: mid_price, path_components: components })
    }

    /// Solves the route selling `amount` of `token` back to the gas token, re-rooting the
    /// pass's shared context at `token` first. Fails as `MissingSellRoute` carrying why: on a
    /// block where many tokens fail at once, the distribution of reasons is the signal.
    fn solve_sell_leg(
        &mut self,
        token: &Address,
        amount: BigUint,
    ) -> Result<SellLeg, FailedItemError> {
        let token_node = *self
            .token_nodes
            .get(token)
            .ok_or_else(|| {
                FailedItemError::MissingSellRoute("token is not in the pass subgraph".into())
            })?;
        let candidate_components = self
            .ctx
            .reroot_toward(
                self.graph,
                token_node,
                self.gas_node,
                &self.hops_to_gas,
                self.algorithm.max_hops(),
            )
            .ok_or_else(|| {
                FailedItemError::MissingSellRoute("no pruned subgraph toward the gas token".into())
            })?;
        let mut components: FxHashSet<ComponentId> = candidate_components
            .into_iter()
            .cloned()
            .collect();

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
        components.extend(
            route
                .swaps()
                .iter()
                .map(|swap| swap.component_id().to_string()),
        );
        Ok(SellLeg { amount_out, components })
    }
}

/// One pass's output: what was priced, against which block, and what was not.
struct PricingPassOutcome {
    /// Priced tokens with the components that must re-price them when they change.
    prices: FxHashMap<Address, TokenPriceEntry>,
    /// The block the market snapshot was taken at.
    block: u64,
    /// Tokens that were attempted and could not be priced: bought, but no sell route back.
    failed_items: Vec<FailedItem>,
    /// Tokens never attempted — the cap left them out, the deadline expired first, or the pass
    /// bailed out before solving anything. They keep their previous price and their stamp:
    /// unlike a failure, nothing is known about them this block.
    unattempted: FxHashSet<Address>,
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
    /// Wall-clock budget for a pass's per-token sell loop, where nearly all of its time goes.
    /// The window opens when the sell loop starts and is checked before each token's sell — the
    /// snapshot and the buy pass ahead of the loop run outside it, bounded only by the per-solve
    /// timeout. Tokens not attempted before it expires keep their previous price; the module's
    /// Cost section says what a slow pass would otherwise delay.
    pass_budget: Duration,
    /// Most tokens one pass attempts. This is what bounds a pass; see `select_pass_tokens`.
    max_tokens_per_pass: usize,
    /// Shortest time between two passes. The cap bounds what one pass costs; this bounds how
    /// often one runs. See the module's "Why the pass is capped, spaced and rotated" section.
    min_pass_interval: Duration,
    /// Everything the schedule of a pass is decided from: when to run one, and which tokens
    /// have waited longest.
    ///
    /// Shared because `compute` takes `&self`, and because the struct derives `Clone` for the
    /// `spawn_blocking` handoff.
    pass_state: Arc<Mutex<PassState>>,
}

/// What one pass leaves behind for the next one to schedule from.
#[derive(Debug, Default)]
struct PassState {
    /// Passes that have run. The number a pass takes stamps every token it attempts, and the
    /// next pass orders by that stamp; see the module's "Why the pass is capped, spaced and
    /// rotated" section.
    passes: u64,
    /// When the last whole pass started, for `min_pass_interval`.
    last_pass_started: Option<Instant>,
    /// The pass each token was last attempted in, whether or not the attempt priced it. This
    /// is the only stamp a token that cannot be priced has, and it is what stops such a token
    /// from holding a slot in every pass.
    last_attempted: FxHashMap<Address, u64>,
}

/// How much of a pass the interval allows right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PassSlot {
    /// The interval has elapsed. A pass of up to `max_tokens_per_pass` runs and restarts it.
    Due,
    /// Inside the interval, but a component arrived. Only the tokens it carries are priced, and
    /// the interval is left running: a chain that lists a pool on most blocks would otherwise
    /// turn every block into a full pass and the interval would bound nothing.
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
    /// Only the tokens that components brought this block.
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
    /// The pass each token was last attempted in, from `PassState`. Absent means never
    /// attempted, which ranks the token ahead of every attempted one.
    last_attempted: FxHashMap<Address, u64>,
}

impl PassPriority {
    /// The pass that last attempted `token`, or zero for one no pass has reached yet.
    fn stamp(&self, token: &Address) -> u64 {
        self.last_attempted
            .get(token)
            .copied()
            .unwrap_or(0)
    }
}

/// Chooses and orders the tokens one pass attempts, and caps how many it takes.
///
/// The cap is what makes a pass bounded, and it has to be applied here rather than left to
/// `pass_budget`: the market snapshot is pruned toward the tokens the pass will attempt, so the
/// set has to be known before any solving starts. The budget stays a backstop for a pathological
/// token, not the thing that decides the size of a pass.
///
/// A token is a candidate when a change points at it, when a component carrying it arrived, or
/// when it has no price. That last case is the one a selection built from stored dependencies
/// cannot express: an unpriced token is in no dependency set, so nothing would ever point at it
/// and it would stay unpriced for as long as the process ran.
///
/// Rank, in order: arrived, then the tokens a change points at and the ones with no price,
/// longest-unattempted first within each rank, so a cap smaller than the candidates rotates over
/// them instead of starving the tail. `PassScope::ArrivalsOnly` offers the arrived tokens only.
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
        if priority.arrived.contains(token) {
            ranked.push((ARRIVED, 0, token.clone()));
            continue;
        }
        // A pass the interval did not grant carries the arrivals and nothing else. Leaving the
        // rest of the market out here rather than relying on the rank to keep them under the
        // cap is what makes an arrival that is not in the universe cost nothing.
        if scope == PassScope::ArrivalsOnly {
            continue;
        }
        // The rank orders by the pass that last *attempted* the token, not the pass that last
        // priced it. A token that cannot be priced has no other stamp, and ordering it by a
        // price it never got would sort it at zero for ever and hold a slot in every pass. On
        // Base at `min-tvl 1` that was 47 of every 100 slots, re-failing the same tokens.
        let stamp = priority.stamp(token);
        // A priced token is offered only when a change points at it; an unpriced one always is.
        if !priority.priced.contains(token) || changed.is_none_or(|changed| changed.contains(token))
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

/// Default wall-clock backstop for a pass's sell loop.
///
/// `DEFAULT_MAX_TOKENS_PER_PASS` is what sizes a pass. This only stops one pathological token
/// from holding the derived chain, so it stays generous.
const DEFAULT_PASS_BUDGET: Duration = Duration::from_secs(30);

/// Default shortest time between two passes.
///
/// A capped pass costs about 0.6s on Base at `min-tvl 1`, and the manager starts another as soon
/// as one ends, so the cap alone left pricing at a 94% duty cycle. Two seconds puts that near
/// 30% and still refreshes the whole market in about 44s, against the 30s an uncapped pass took.
const DEFAULT_MIN_PASS_INTERVAL: Duration = Duration::from_secs(2);

/// Default cap on the tokens one pass attempts.
///
/// Measured on Base at `min-tvl 1`, a sell costs about 12ms, so 100 tokens is a pass of roughly
/// 1.2s against a 2s block. The whole market of about 2200 tokens refreshes in some 23 passes,
/// which is the same staleness the uncapped pass had when it ran for 25 to 30 seconds at a time,
/// for about 40% of the CPU.
const DEFAULT_MAX_TOKENS_PER_PASS: usize = 100;

impl Default for TokenGasPriceComputation {
    fn default() -> Self {
        Self {
            gas_token: Address::zero(20), // ETH address
            // The builder overrides this with the deepest configured pool's max_hops; the
            // default matches the default pool, so a bare computation never prices deeper
            // than a default pool routes.
            max_hops: crate::solver::defaults::POOL_MAX_HOPS,
            probe_amount: BigUint::from(10u64).pow(18), // 1 ETH
            pass_budget: DEFAULT_PASS_BUDGET,
            max_tokens_per_pass: DEFAULT_MAX_TOKENS_PER_PASS,
            min_pass_interval: DEFAULT_MIN_PASS_INTERVAL,
            pass_state: Arc::new(Mutex::new(PassState::default())),
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

    /// Sets the wall-clock backstop for a pass's sell loop.
    pub fn with_pass_budget(self, pass_budget: Duration) -> Self {
        Self { pass_budget, ..self }
    }

    /// Sets how many tokens one pass may attempt.
    pub fn with_max_tokens_per_pass(self, max_tokens_per_pass: usize) -> Self {
        Self { max_tokens_per_pass, ..self }
    }

    /// Sets the shortest time between two passes. `Duration::ZERO` runs one per block.
    pub fn with_min_pass_interval(self, min_pass_interval: Duration) -> Self {
        Self { min_pass_interval, ..self }
    }

    /// Takes the pass state's lock.
    ///
    /// A poisoned lock is taken anyway: the guarded value only schedules passes, a panic cannot
    /// leave it half-written, and refusing to price tokens over it would be worse.
    fn lock_pass_state(&self) -> MutexGuard<'_, PassState> {
        match self.pass_state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Returns how much of a pass may run now, and restarts the interval for a whole one.
    ///
    /// `must_solve` runs a whole pass whatever the interval says, for a caller that has nothing
    /// stored to serve instead. `arrivals` only earns the tokens that arrived: those cannot be
    /// quoted until they are priced, but pricing them is not a reason to restart the interval or
    /// to re-rank the rest of the market.
    fn claim_pass(&self, arrivals: bool, must_solve: bool) -> PassSlot {
        let mut state = self.lock_pass_state();
        let now = Instant::now();
        let due = state
            .last_pass_started
            .is_none_or(|started| now.duration_since(started) >= self.min_pass_interval);
        if due || must_solve {
            state.last_pass_started = Some(now);
            return PassSlot::Due;
        }
        if arrivals {
            return PassSlot::ArrivalsOnly;
        }
        PassSlot::Deferred
    }

    /// Snapshots what the next pass ranks its candidates by.
    ///
    /// `priced` says which tokens have a stored price; the stamps come from the pass state.
    fn pass_priority(
        &self,
        arrived: FxHashSet<Address>,
        priced: FxHashSet<Address>,
    ) -> PassPriority {
        PassPriority {
            arrived,
            priced,
            last_attempted: self
                .lock_pass_state()
                .last_attempted
                .clone(),
        }
    }

    /// Takes the number of the pass that just ran, stamps every token it attempted, and forgets
    /// the ones the market no longer holds so the map cannot grow without bound.
    ///
    /// Attempted means selected and not carried: a token the cap or the deadline left out comes
    /// back as unattempted and keeps whatever stamp it had. Priced and failed tokens are stamped
    /// alike, because the stamp answers "when did a pass last look at this token", which is what
    /// rotation needs from it.
    fn record_attempts(
        &self,
        selected: &FxHashSet<Address>,
        outcome: &PricingPassOutcome,
        universe: &FxHashSet<Address>,
    ) {
        let mut state = self.lock_pass_state();
        let pass = state.passes.wrapping_add(1);
        state.passes = pass;
        for token in selected {
            if outcome.unattempted.contains(token) {
                continue;
            }
            state
                .last_attempted
                .insert(token.clone(), pass);
        }
        state
            .last_attempted
            .retain(|token, _| universe.contains(token));
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
            (guard.component_topology(), block)
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
                block,
                failed_items: Vec::new(),
                unattempted: universe,
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

        // One snapshot serves the buy pass and every sell. The subgraph is walked one hop
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
                block,
                failed_items: Vec::new(),
                unattempted: universe,
            });
        };

        // Stamp the result with the snapshot's block, not the earlier topology read — the feed
        // can advance between the two locks, and every price is computed against the snapshot.
        let block = ctx
            .market_data
            .last_updated()
            .map_or(block, |b| b.number());

        // The buy pass and the sell loop are pure CPU work — every step simulates swaps against
        // the owned snapshot and never awaits — so they run on a blocking thread instead of
        // pinning one of the shared runtime's workers for the whole pass.
        let computation = self.clone();
        let span = Span::current();
        let mut outcome = tokio::task::spawn_blocking(move || {
            let _entered = span.enter();
            let mut sell = PricingPass::new(&algorithm, graph_manager.graph(), ctx, &computation);
            sell.sell_loop(ordered, block)
        })
        .await
        .map_err(|join_error| {
            ComputationError::Internal(format!("token pricing pass did not complete: {join_error}"))
        })?;
        outcome.unattempted.extend(capped_out);
        self.record_attempts(&selected, &outcome, &universe);
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

    /// Offers the pass the tokens a change could have moved, ranked so the budget cuts the
    /// least valuable last.
    ///
    /// The selection is not a bound — on a dense market it is almost every priced token, for
    /// the reason the module doc gives. `max_tokens_per_pass` is the bound, and the rank
    /// decides who gets under it.
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
        let (tokens_to_recompute, new_tokens, priority, existing_prices) = {
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
        let edited = store
            .write()
            .await
            .edit_token_prices_deps(solved.block, |deps| {
                // Everything the pass priced, not only what the change pointed at: the pass
                // ranks over the whole market, so it reaches tokens this block's change never
                // named — which is the only way a token that has no price gets one.
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
            });
        if !edited {
            warn!("token price dependencies vanished between the read and the write");
            // Returning `None` sends `compute` to seeding, which rebuilds them.
            return Ok(None);
        }
        Span::current().record("updated_token_prices", result.len());

        Ok(Some(ComputationOutput::with_failures(result, solved.failed_items)))
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
        // A component arriving earns a pass inside the interval, but only for the tokens it
        // carries; a full recompute earns a whole one, having nothing stored to serve instead.
        let scope = match self.claim_pass(!changed.added.is_empty(), changed.is_full_recompute) {
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
        // selects, and the cap bounds what the pass gets through — a topology change included,
        // since its tokens are what the selection ranks first.
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

impl TokenGasPriceComputation {
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
        // the set instead of re-pricing the same head. Nothing has arrived here — a seeding pass
        // offers every token anyway.
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

        store
            .write()
            .await
            .set_token_prices_deps(token_prices_with_deps, solved.block);

        debug!(priced = token_prices.len() - 1, "token price computation complete");
        Span::current().record("updated_token_prices", token_prices.len());

        Ok(ComputationOutput::with_failures(token_prices, solved.failed_items))
    }
}

#[cfg(test)]
mod tests {
    use num_traits::ToPrimitive;
    use tycho_simulation::tycho_core::models::token::Token;

    use super::*;
    use crate::{
        algorithm::test_utils::{component, setup_market_weighted, token, MockProtocolSim},
        derived::store::DerivedData,
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
    async fn test_parallel_pools_price_via_best_output() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Two pools on the same pair. The fee-free pool has the tighter spread, but the
        // 1%-fee pool delivers more output on the buy — a ranking by spread would pick
        // "tight", a ranking by output must pick "wide":
        //   buy  (wide):  1e18 ETH * 2500 * 0.99          → 2475e18 USDC (tight: 2000e18)
        //   sell (tight): 2475e18 USDC / 2000             → 1.2375e18 ETH (wide: 0.9801e18)
        // Each leg independently takes the pool that outputs more, so
        //   mid = (2475 + 2475/1.2375) / 2 = (2475 + 2000) / 2 = 2237.5
        let prices = prices_for(
            &eth,
            vec![
                ("tight", &eth, &usdc, MockProtocolSim::new(2000.0)),
                ("wide", &eth, &usdc, MockProtocolSim::new(2500.0).with_fee(0.01)),
            ],
        )
        .await;

        assert!((ratio(&prices[&usdc.address]) - 2237.5).abs() < 1e-6);
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

        // max_hops = 1 keeps the two pools out of each other's candidate sets, so each
        // token's price depends on exactly its own pool.
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
    /// Any add or remove used to force a full solve, so both tokens here would pick up the
    /// moved market. Only the arrived component's token does now.
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
        };

        let ordered = select_pass_tokens(&universe, None, &priority, PassScope::Whole, 2);

        assert_eq!(ordered, vec![unpriced, stale], "the cap keeps the two highest ranks");
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
            computation.claim_pass(false, false),
            PassSlot::Due,
            "the first pass is due, nothing has run"
        );
        assert_eq!(
            computation.claim_pass(false, false),
            PassSlot::Deferred,
            "a block straight after one inside the interval waits"
        );

        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            computation.claim_pass(true, false),
            PassSlot::ArrivalsOnly,
            "an arrival inside the interval earns a pass for its own tokens"
        );

        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            computation.claim_pass(false, false),
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

        assert_eq!(computation.claim_pass(false, false), PassSlot::Due);
        assert_eq!(computation.claim_pass(false, false), PassSlot::Deferred);
        assert_eq!(
            computation.claim_pass(false, true),
            PassSlot::Due,
            "a full recompute has nothing to serve instead, so it runs a whole pass"
        );
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
                .lock_pass_state()
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
    }

    #[tokio::test]
    async fn test_deps_cover_rival_routes() {
        // USDC prices via the direct pool, but the worse ETH->MID->USDC route is a candidate:
        // its pools must be in USDC's dependency set, or a state change that makes it the
        // better route would leave the stored price stale until a full recompute.
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
        for component in ["direct", "eth_mid", "mid_usdc"] {
            assert!(deps.contains(component), "{component} must invalidate USDC's price");
        }
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

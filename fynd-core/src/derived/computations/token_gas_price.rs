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
//! That deadline is the only thing that bounds a pass. The next section says why the set of
//! tokens a pass selects does not.
//!
//! # Why the pass is capped and rotated
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
//! A full solve is therefore only needed when there is nothing stored to select from at all:
//! startup, and lag recovery. Nothing else gets one, including a topology change, because rank 1
//! covers what a topology change would have been a full solve for.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
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
    /// Stamped on every token this pass prices, so the next pass can order by it.
    pass: u64,
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
        pass: u64,
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
        Self { algorithm, graph, ctx, computation, buys, gas_node, hops_to_gas, token_nodes, pass }
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
        Ok(TokenPriceEntry {
            price: mid_price,
            path_components: components,
            last_priced_pass: self.pass,
        })
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
    /// Tokens never attempted — the deadline expired first, or the pass bailed out before
    /// solving anything. They keep their previous price: unlike a failure, nothing is known
    /// about them this block.
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
    /// Counts the passes that have run, and stamps every token a pass prices. Ordering by the
    /// stamp is what rotates a budget-limited pass over the whole token set; see the module's
    /// "Why the pass is bounded and rotated" section.
    ///
    /// Shared because `compute` takes `&self`, and because the struct derives `Clone` for the
    /// `spawn_blocking` handoff.
    pass_counter: Arc<AtomicU64>,
}

/// What a pass knows about the tokens it is about to attempt, which decides their order.
#[derive(Debug, Default)]
pub(crate) struct PassPriority {
    /// Tokens carried by components that arrived this block.
    arrived: FxHashSet<Address>,
    /// The pass each token was last priced in. A token that is absent has no price yet.
    last_priced: FxHashMap<Address, u64>,
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
/// Rank, in order: arrived, unpriced, then the tokens a change points at, longest-unpriced
/// first, so a cap smaller than the candidates rotates over them instead of starving the tail.
fn select_pass_tokens(
    universe: &FxHashSet<Address>,
    changed: Option<&FxHashSet<Address>>,
    priority: &PassPriority,
    max_tokens: usize,
) -> Vec<Address> {
    const ARRIVED: u8 = 0;
    const UNPRICED: u8 = 1;
    const CHANGED: u8 = 2;

    let mut ranked: Vec<(u8, u64, Address)> = Vec::with_capacity(universe.len());
    for token in universe {
        if priority.arrived.contains(token) {
            ranked.push((ARRIVED, 0, token.clone()));
            continue;
        }
        let Some(last) = priority.last_priced.get(token) else {
            ranked.push((UNPRICED, 0, token.clone()));
            continue;
        };
        if changed.is_none_or(|changed| changed.contains(token)) {
            ranked.push((CHANGED, *last, token.clone()));
        }
    }
    // Within a rank, the smaller pass number is the token that has gone longest without one.
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
            pass_counter: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl TokenGasPriceComputation {
    /// Creates a computation with explicit parameters.
    #[cfg(test)]
    pub fn new(gas_token: Address, max_hops: usize, probe_amount: BigUint) -> Self {
        Self { gas_token, max_hops, probe_amount, ..Self::default() }
    }

    /// Sets the wall-clock backstop for a pass's sell loop.
    pub fn with_pass_budget(self, pass_budget: Duration) -> Self {
        Self { pass_budget, ..self }
    }

    /// Sets how many tokens one pass may attempt.
    pub fn with_max_tokens_per_pass(self, max_tokens_per_pass: usize) -> Self {
        Self { max_tokens_per_pass, ..self }
    }

    /// Claims the number of the pass that is about to run.
    ///
    /// Every token the pass prices is stamped with it, and the next pass orders by that stamp.
    /// The number is claimed before the pass runs, so a pass that then fails does not reuse the
    /// number of the one before it.
    fn next_pass(&self) -> u64 {
        self.pass_counter
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
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
    ) -> Result<PricingPassOutcome, ComputationError> {
        let (topology, block) = {
            let guard = market.read().await;
            let block = guard
                .last_updated()
                .map(|b| b.number())
                .unwrap_or(0);
            (guard.component_topology(), block)
        };

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

        let universe = self.tokens_to_price(&topology);
        let ordered = select_pass_tokens(&universe, changed, priority, self.max_tokens_per_pass);
        // Tokens the cap left out are unattempted, exactly like ones the deadline cuts: they
        // keep their previous price and stay visible to the next pass, which ranks them by the
        // pass that last priced them.
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

        let pass = self.next_pass();
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
            let mut sell =
                PricingPass::new(&algorithm, graph_manager.graph(), ctx, &computation, pass);
            sell.sell_loop(ordered, block)
        })
        .await
        .map_err(|join_error| {
            ComputationError::Internal(format!("token pricing pass did not complete: {join_error}"))
        })?;
        outcome.unattempted.extend(capped_out);
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
    /// the reason the module doc gives. `pass_budget` is the bound, and the rank decides who
    /// gets under it.
    ///
    /// `Ok(None)` when there is nothing stored to select from, so a full solve is needed.
    async fn try_incremental_compute(
        &self,
        market: &MarketData,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
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
                if !arrived.insert(token.clone()) {
                    continue;
                }
                if !existing_deps.contains_key(token) {
                    new_tokens += 1;
                }
                tokens_to_recompute.insert(token.clone());
            }
            let last_priced = existing_deps
                .iter()
                .map(|(token, entry)| (token.clone(), entry.last_priced_pass))
                .collect();
            let priority = PassPriority { arrived, last_priced };
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

        let solved = self
            .solve_token_prices(market, Some(&tokens_to_recompute), &priority)
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
            // Returning `None` sends `compute` to a full solve, which rebuilds them.
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

    #[instrument(level = "debug", skip(market, store, changed), fields(computation_id = Self::ID, updated_token_prices))]
    async fn compute(
        &self,
        market: &MarketData,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
    ) -> Result<ComputationOutput<Self::Output>, ComputationError> {
        // Startup and lag recovery have nothing stored to select from, so they offer every token
        // to the pass. So does a block whose selection finds nothing stored. Every other block
        // selects, and `pass_budget` bounds what the pass gets through — a topology change
        // included, since its tokens are what the selection ranks first.
        if !changed.is_full_recompute {
            if let Some(result) = self
                .try_incremental_compute(market, store, changed)
                .await?
            {
                return Ok(result);
            }
        }

        self.full_solve(market, store).await
    }
}

impl TokenGasPriceComputation {
    /// Prices every token the budget allows and replaces the stored set.
    async fn full_solve(
        &self,
        market: &MarketData,
        store: &SharedDerivedDataRef,
    ) -> Result<ComputationOutput<TokenGasPrices>, ComputationError> {
        // A full solve is cut off by the same budget as any other pass, so it needs the same
        // rank: unpriced tokens first, then the ones that have gone longest without a pass. On
        // a market where one budget cannot price everything, this is what makes repeated full
        // solves cover the set instead of re-pricing the same head each time. Nothing has
        // arrived here — a full solve offers every token anyway.
        let priority = {
            let store_guard = store.read().await;
            let last_priced = store_guard
                .token_prices_deps()
                .map(|deps| {
                    deps.iter()
                        .map(|(token, entry)| (token.clone(), entry.last_priced_pass))
                        .collect()
                })
                .unwrap_or_default();
            PassPriority { arrived: FxHashSet::default(), last_priced }
        };
        let solved = self
            .solve_token_prices(market, None, &priority)
            .await?;

        let mut token_prices_with_deps = TokenPricesWithDeps::default();
        let mut token_prices = TokenGasPrices::default();
        for (token, entry) in solved.prices {
            token_prices.insert(token.clone(), entry.price.clone());
            token_prices_with_deps.insert(token, entry);
        }

        // Tokens the deadline cut off keep their previous entry, dependencies included: they
        // stay served and stay visible to the incremental path, which re-prices them when one
        // of their pools changes. Dropping them would unprice them until the next full solve.
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
                // The gas token is 1:1 with itself whatever the market does, so no pass ever
                // prices it. `u64::MAX` says "never due" rather than claiming a pass priced it.
                last_priced_pass: u64::MAX,
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
    async fn test_full_solve_past_deadline() {
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
        // must keep its previous price rather than vanish until the next full solve.
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
        // The manager persists between runs; without this the incremental path bails out on
        // the missing stored prices and the test would exercise the full solve twice.
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
        // The arrived token is also the most recently priced, and the untouched one is the
        // stalest, so rank has to beat staleness in both directions.
        let priority = PassPriority {
            arrived: [arrived_token.clone()]
                .into_iter()
                .collect(),
            last_priced: [
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

        let ordered = select_pass_tokens(&universe, Some(&changed), &priority, usize::MAX);

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
            last_priced: [(stale.clone(), 1), (fresh.clone(), 8)]
                .into_iter()
                .collect(),
        };

        let ordered = select_pass_tokens(&universe, None, &priority, 2);

        assert_eq!(ordered, vec![unpriced, stale], "the cap keeps the two highest ranks");
    }

    /// A token with no price must stay reachable when no change points at it.
    ///
    /// The incremental path takes its changed set from the stored dependency map, which by
    /// construction holds only tokens that already have a price. A pass that selected from that
    /// map alone could never offer an unpriced token again, and a capped first pass leaves most
    /// of the market unpriced. A Base run at `min-tvl 1` stopped at 97 of 2211 tokens that way.
    #[tokio::test]
    async fn test_unpriced_tokens_are_reached_without_a_change_pointing_at_them() {
        let eth = token(0, "ETH");
        let aaa = token(1, "AAA");
        let bbb = token(2, "BBB");
        let ccc = token(3, "CCC");
        let (market, _) = setup_market_weighted(vec![
            ("eth_aaa", &eth, &aaa, MockProtocolSim::new(2000.0)),
            ("eth_bbb", &eth, &bbb, MockProtocolSim::new(2500.0)),
            ("eth_ccc", &eth, &ccc, MockProtocolSim::new(3000.0)),
        ]);
        // One token per pass, so the first pass can only ever reach a third of the market.
        let computation =
            TokenGasPriceComputation::new(eth.address.clone(), 1, BigUint::from(PROBE_AMOUNT))
                .with_max_tokens_per_pass(1);
        let store = DerivedData::new_shared();

        let first = computation
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");
        TokenGasPriceComputation::persist(&mut *store.write().await, first, 1, true);
        let priced_after_first = store
            .read()
            .await
            .token_prices()
            .expect("prices are stored")
            .len();
        assert_eq!(priced_after_first, 2, "the cap holds: one token plus the gas token");

        // Only AAA's pool ever changes. Nothing points at BBB or CCC, and neither is in the
        // stored dependency map, so only a selection over the whole market can reach them.
        for block in 2..=6 {
            let changed = ChangedComponents {
                updated: vec!["eth_aaa".to_string()],
                ..ChangedComponents::default()
            };
            let output = computation
                .compute(&market, &store, &changed)
                .await
                .expect("pricing must not fail");
            TokenGasPriceComputation::persist(&mut *store.write().await, output, block, false);
        }

        let prices = store
            .read()
            .await
            .token_prices()
            .expect("prices are stored")
            .clone();
        for (name, address) in [("AAA", &aaa.address), ("BBB", &bbb.address), ("CCC", &ccc.address)]
        {
            assert!(prices.contains_key(address), "{name} must be priced by a later pass");
        }
    }

    /// The stamp a pass leaves is what the next pass ranks by, so it must tell a token the pass
    /// re-priced apart from one it left alone. Without that, a budget-limited pass has nothing
    /// to rotate on and would attempt the same head every time.
    #[tokio::test]
    async fn test_a_pass_stamps_only_the_tokens_it_reprices() {
        async fn stamp(store: &SharedDerivedDataRef, token: &Address) -> u64 {
            store
                .read()
                .await
                .token_prices_deps()
                .expect("deps are stored")
                .get(token)
                .expect("token is priced")
                .last_priced_pass
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
        let first_pass = stamp(&store, &aaa.address).await;
        assert_eq!(stamp(&store, &bbb.address).await, first_pass, "one pass priced both");

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
            stamp(&store, &aaa.address).await > first_pass,
            "the re-priced token carries the newer pass"
        );
        assert_eq!(
            stamp(&store, &bbb.address).await,
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

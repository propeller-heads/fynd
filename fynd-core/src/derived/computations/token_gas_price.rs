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
//! price reflects what a trade would actually get, slippage and fees included. Each token is bought
//! with a fixed amount of gas token and sold back, and its price is the mean of the buy price and
//! the sell price. The mean includes the round trip's fees and slippage — never gas, per the
//! next paragraph — and its bias is one-sided: with a
//! symmetric per-leg loss factor `k` the two implied rates are `p·k` and `p/k`, whose arithmetic
//! mean `p·(k + 1/k)/2` is never below the loss-free rate `p`. In raw-token-units-per-gas-unit
//! terms that understates a token's value, never overstates it — negligibly for deep pairs
//! (+0.005% at `k` = 0.99), heavily for thin ones (+25% at `k` = 0.5). A geometric mean would be
//! exact under symmetric loss, but it is irrational and cannot be an exact fraction.
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
//! the block the result is stored under agree. A slow pass delays that block's component depths
//! and the start of the next block's computations — spot prices run first and are unaffected —
//! and a pass-wide deadline bounds that delay: tokens it cuts off keep their previous price and
//! stay visible to invalidation. After the first full solve, recomputation is incremental: only
//! tokens whose stored routes ran through a changed component are re-solved, which bounds the
//! steady-state cost.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use num_bigint::BigUint;
#[cfg(test)]
use num_traits::ToPrimitive;
use num_traits::Zero;
use petgraph::graph::NodeIndex;
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{debug, instrument, trace, warn, Span};
use tycho_simulation::{
    tycho_common::models::Address, tycho_core::simulation::protocol_sim::Price,
};

use crate::{
    algorithm::{
        bellman_ford::{BellmanFordContext, FindRouteOptions, ReachedToken},
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
    types::{ComponentId, Order, OrderSide},
};

type AlgorithmGraph = <BellmanFordAlgorithm as Algorithm>::GraphType;

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
    /// The solving algorithm; its `max_hops` bounds route length within the wider subgraph.
    algorithm: &'a BellmanFordAlgorithm,
    graph: &'a AlgorithmGraph,
    /// The shared snapshot, re-rooted and re-pruned per sell.
    ctx: BellmanFordContext,
    /// The gas token's node, saved before the first reroot moves `ctx` off it.
    gas_node: NodeIndex,
    /// Hops from each node to the gas token, computed once, pruning every sell's walk.
    hops_to_gas: FxHashMap<NodeIndex, usize>,
    /// Token address → graph node, inverted once from the context, for re-rooting sells.
    token_nodes: FxHashMap<Address, NodeIndex>,
}

/// One pass's output: what was priced, against which block, and what was not.
struct SolvedPrices {
    /// Priced tokens with the components that must re-price them when they change.
    prices: FxHashMap<Address, TokenPriceEntry>,
    /// The block the market snapshot was taken at.
    block: u64,
    /// Tokens that were attempted and could not be priced: bought, but no sell route back.
    failed_items: Vec<FailedItem>,
    /// Tokens never attempted because the pass deadline expired first. They keep their
    /// previous price: unlike a failure, nothing is known about them this block.
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
    /// Wall-clock budget for one whole pass. A full Ethereum-sized solve measures ~6 s, so
    /// 30 s is margin, not target: it exists to stop a pathological block — per-solve
    /// timeouts alone allow ~1 s per token — from stalling the derived chain for minutes.
    /// Tokens not attempted before it expires keep their previous price.
    pass_budget: Duration,
}

impl Default for TokenGasPriceComputation {
    fn default() -> Self {
        Self {
            gas_token: Address::zero(20), // ETH address
            max_hops: 3,
            probe_amount: BigUint::from(10u64).pow(18), // 1 ETH
            pass_budget: Duration::from_secs(30),
        }
    }
}

impl TokenGasPriceComputation {
    /// Creates a computation with explicit parameters.
    #[cfg(test)]
    pub fn new(gas_token: Address, max_hops: usize, probe_amount: BigUint) -> Self {
        Self { gas_token, max_hops, probe_amount, ..Self::default() }
    }

    /// Sets the wall-clock budget for one pass.
    #[cfg(test)]
    pub fn with_pass_budget(self, pass_budget: Duration) -> Self {
        Self { pass_budget, ..self }
    }

    /// Sets the longest route the algorithm may build.
    pub fn with_max_hops(self, max_hops: usize) -> Self {
        Self { max_hops, ..self }
    }

    /// Sets the gas token address.
    pub fn with_gas_token(self, gas_token: Address) -> Self {
        Self { gas_token, ..self }
    }

    /// Solves every token, or only `filter_tokens` when given, within one wall-clock budget.
    ///
    /// Tokens that were bought but found no sell route back come back as failed items. Tokens
    /// the gas token cannot reach at all are only counted (logged at debug): unreachable is the
    /// normal state for much of the topology, and every full solve re-attempts them anyway.
    /// Tokens the deadline cut off come back as unattempted, so callers can keep their
    /// previous prices.
    async fn solve_token_prices(
        &self,
        market: &MarketData,
        filter_tokens: Option<&FxHashSet<Address>>,
    ) -> Result<SolvedPrices, ComputationError> {
        let deadline = Instant::now() + self.pass_budget;
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

        let tokens_to_price = self.tokens_to_price(&topology, filter_tokens);
        if tokens_to_price.is_empty() {
            return Ok(SolvedPrices {
                prices: FxHashMap::default(),
                block,
                failed_items: Vec::new(),
                unattempted: FxHashSet::default(),
            });
        }
        let graph = graph_manager.graph();

        // One snapshot serves the buy pass and every sell. The subgraph is walked one hop
        // beyond `max_hops`: a sell route of `max_hops` hops can start from a token that far
        // from the gas token, and the walk must include that token's outgoing edges.
        let Some(ctx) = algorithm
            .build_context_from_source_token(
                graph,
                market.clone(),
                &self.gas_token,
                self.max_hops + 1,
            )
            .await
        else {
            // No subgraph around the gas token means nothing is priceable this block.
            debug!(unreachable_tokens = tokens_to_price.len(), "no subgraph around the gas token");
            return Ok(SolvedPrices {
                prices: FxHashMap::default(),
                block,
                failed_items: Vec::new(),
                unattempted: FxHashSet::default(),
            });
        };
        // Stamp the result with the snapshot's block, not the earlier topology read — the feed
        // can advance between the two locks, and every price is computed against the snapshot.
        let block = ctx
            .market_data
            .last_updated()
            .map_or(block, |b| b.number());

        let buys = algorithm.reach_from_source_token(&ctx, &self.probe_amount);

        let gas_node = ctx.token_in_node;
        let token_nodes = ctx
            .node_address
            .iter()
            .map(|(&node, address)| (address.clone(), node))
            .collect();
        let hops_to_gas = BellmanFordAlgorithm::get_hops_to_reach(graph, gas_node, self.max_hops);
        let mut pass =
            PricingPass { algorithm: &algorithm, graph, ctx, gas_node, hops_to_gas, token_nodes };

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
            match self.price_token(&mut pass, &token, buys.get(&token)) {
                Ok(priced) => {
                    prices.insert(token, priced);
                }
                // Tokens with no route from the gas token are counted, not reported: they are
                // the normal state of much of the topology, and a failed item each would be
                // allocated, logged, and broadcast to every worker every block.
                Err(FailedItemError::MissingBuyRoute) => unreachable_tokens += 1,
                Err(error) => failed_items.push(FailedItem { key: token.to_string(), error }),
            }
        }
        unattempted.extend(remaining);
        if !unattempted.is_empty() {
            warn!(
                unattempted = unattempted.len(),
                priced = prices.len(),
                "token pricing pass hit its deadline; unattempted tokens keep previous prices"
            );
        }
        debug!(
            priced = prices.len(),
            failed = failed_items.len(),
            unreachable = unreachable_tokens,
            unattempted = unattempted.len(),
            block,
            "token pricing pass complete"
        );

        Ok(SolvedPrices { prices, block, failed_items, unattempted })
    }

    /// Every token in the graph but the gas token, narrowed to `filter_tokens` when given.
    fn tokens_to_price(
        &self,
        topology: &FxHashMap<ComponentId, Vec<Address>>,
        filter_tokens: Option<&FxHashSet<Address>>,
    ) -> FxHashSet<Address> {
        topology
            .values()
            .flatten()
            .filter(|token| *token != &self.gas_token)
            .filter(|token| filter_tokens.is_none_or(|filter| filter.contains(*token)))
            .cloned()
            .collect()
    }

    /// Prices one token as the arithmetic mean of its buy price and its sell price, kept as an
    /// exact fraction, with the components that must re-price it when they change. The mean's
    /// round-trip bias only ever prices a token low, hardest on thin pairs — see the module doc.
    ///
    /// The component set covers every candidate route between the token and the gas token, not
    /// just the two chosen ones: a rival pool can move and become the better route, and only a
    /// full recompute would ever notice if it were not in the set.
    ///
    /// A token missing either route is an error, not a price: a buy rate alone would flatter a
    /// token that is expensive to exit, and prices must stay comparable across tokens.
    fn price_token(
        &self,
        pass: &mut PricingPass<'_>,
        token: &Address,
        buy_leg: Option<&ReachedToken>,
    ) -> Result<TokenPriceEntry, FailedItemError> {
        let buy_leg = buy_leg.ok_or(FailedItemError::MissingBuyRoute)?;

        let (sell_out, mut components) = self.sell_leg(pass, token, buy_leg.amount_out.clone())?;
        // The legs are discarded after the mean; this is the only place their divergence —
        // sell_out under the probe amount is the round-trip loss — can be observed.
        trace!(%token, buy_out = %buy_leg.amount_out, sell_out = %sell_out, "token priced");
        // The buy path is a candidate path, so extending is defensive: it keeps the stored
        // dependencies correct even if the walk and the relaxation ever disagree.
        components.extend(buy_leg.components.iter().cloned());

        let mid_price = Price {
            numerator: &buy_leg.amount_out * (&self.probe_amount + &sell_out),
            denominator: BigUint::from(2u8) * &self.probe_amount * sell_out,
        };
        Ok(TokenPriceEntry { price: mid_price, path_components: components })
    }

    /// What selling `amount` of `token` back to the gas token returns (never zero), paired
    /// with the components the price depends on: every component on any candidate route
    /// between the two — the walk's component set, which pool edge pairs make the buy
    /// direction's candidates too — plus the chosen sell route's own, defensively. Fails as
    /// `MissingSellRoute` carrying why: on a block where many
    /// tokens fail at once, the distribution of reasons is the signal.
    ///
    /// Re-roots the pass's shared context at `token` behind a freshly pruned adjacency before
    /// solving.
    fn sell_leg(
        &self,
        pass: &mut PricingPass<'_>,
        token: &Address,
        amount: BigUint,
    ) -> Result<(BigUint, FxHashSet<ComponentId>), FailedItemError> {
        let token_node = *pass
            .token_nodes
            .get(token)
            .ok_or_else(|| {
                FailedItemError::MissingSellRoute("token is not in the pass subgraph".into())
            })?;
        let (adj, _, candidate_components) = BellmanFordAlgorithm::get_subgraph_with_hop_map(
            pass.graph,
            token_node,
            Some(&pass.hops_to_gas),
            self.max_hops,
        )
        .ok_or_else(|| {
            FailedItemError::MissingSellRoute("no pruned subgraph toward the gas token".into())
        })?;
        let mut components: FxHashSet<ComponentId> = candidate_components
            .into_iter()
            .cloned()
            .collect();
        pass.ctx.adj = adj;
        pass.ctx
            .reroot(token_node, Some(pass.gas_node));

        let order = Order::new(
            token.clone(),
            self.gas_token.clone(),
            amount,
            OrderSide::Sell,
            Address::zero(20),
        );
        let result = pass
            .algorithm
            .find_single_route(&pass.ctx, &order, FindRouteOptions::default())
            .map_err(|error| FailedItemError::MissingSellRoute(error.to_string()))?;
        let route = result.route();
        let sell_out = route.amount_out(&self.gas_token);
        if sell_out.is_zero() {
            return Err(FailedItemError::MissingSellRoute("the sell route returns zero".into()));
        }
        components.extend(
            route
                .swaps()
                .iter()
                .map(|swap| swap.component_id().to_string()),
        );
        Ok((sell_out, components))
    }

    /// Re-solves only the tokens whose stored routes ran through a changed component.
    ///
    /// `Ok(None)` when there is nothing stored to narrow by, so a full solve is needed.
    async fn try_incremental_compute(
        &self,
        market: &MarketData,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
    ) -> Result<Option<ComputationOutput<TokenGasPrices>>, ComputationError> {
        let (existing_deps, existing_prices) = {
            let store_guard = store.read().await;
            let Some(existing_deps) = store_guard.token_prices_deps().cloned() else {
                return Ok(None);
            };
            let Some(existing_prices) = store_guard.token_prices().cloned() else {
                return Ok(None);
            };
            (existing_deps, existing_prices)
        };

        let changed_components = changed.all_changed_ids();
        let tokens_to_recompute: FxHashSet<Address> = existing_deps
            .iter()
            .filter(|(_, entry)| {
                !entry
                    .path_components
                    .is_disjoint(&changed_components)
            })
            .map(|(addr, _)| addr.clone())
            .collect();

        if tokens_to_recompute.is_empty() {
            return Ok(Some(ComputationOutput::success(existing_prices)));
        }

        debug!(
            affected_tokens = tokens_to_recompute.len(),
            total_tokens = existing_prices.len(),
            "incremental token price recomputation"
        );

        let solved = self
            .solve_token_prices(market, Some(&tokens_to_recompute))
            .await?;

        let mut result = existing_prices;
        let mut new_deps = existing_deps;

        for token in &tokens_to_recompute {
            if let Some(entry) = solved.prices.get(token) {
                result.insert(token.clone(), entry.price.clone());
                new_deps.insert(token.clone(), entry.clone());
            } else if !solved.unattempted.contains(token) {
                // Attempted and failed: the routes are gone, so the price is too. A token the
                // deadline cut off keeps its entry instead — nothing is known about it this
                // block, and dropping it would hide it from this incremental path for good.
                result.remove(token);
                new_deps.remove(token);
            }
        }

        store
            .write()
            .await
            .set_token_prices_deps(new_deps, solved.block);
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
        if !changed.is_full_recompute && !changed.is_topology_change() {
            if let Some(result) = self
                .try_incremental_compute(market, store, changed)
                .await?
            {
                return Ok(result);
            }
        }

        let solved = self
            .solve_token_prices(market, None)
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
    use tycho_simulation::tycho_core::models::token::Token;

    use super::*;
    use crate::{
        algorithm::test_utils::{setup_market_weighted, token, MockProtocolSim},
        derived::store::DerivedData,
    };

    const PROBE_AMOUNT: u128 = 1_000_000_000_000_000_000;

    fn computation_for(gas_token: &Address) -> TokenGasPriceComputation {
        TokenGasPriceComputation::new(gas_token.clone(), 3, BigUint::from(PROBE_AMOUNT))
    }

    fn ratio(price: &Price) -> f64 {
        let (Some(numerator), Some(denominator)) =
            (price.numerator.to_f64(), price.denominator.to_f64())
        else {
            return f64::NAN;
        };
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

        let prices =
            prices_for(&eth, vec![("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0))]).await;

        // The gas token is 1:1 with itself. A fee-free symmetric pool buys and sells back at the
        // same rate, so the mean is that rate exactly.
        let eth_price = prices
            .get(&eth.address)
            .expect("gas token should be priced");
        assert_eq!(eth_price.numerator, eth_price.denominator);
        assert!((ratio(&prices[&usdc.address]) - 2000.0).abs() < 1e-6);
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
    async fn test_expired_deadline_keeps_previous_prices_on_full_solve() {
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
    async fn test_expired_deadline_keeps_prices_on_incremental_solve() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");
        let (market, _) =
            setup_market_weighted(vec![("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0))]);
        let store = DerivedData::new_shared();
        computation_for(&eth.address)
            .compute(&market, &store, &ChangedComponents::default())
            .await
            .expect("pricing must not fail");

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
        // sell leg introduces no rounding and prices compare exactly.
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
    async fn test_empty_market_prices_only_the_gas_token() {
        let eth = token(0, "ETH");

        let prices = prices_for(&eth, vec![]).await;

        // No components means nothing to price, but never an error: the gas token is 1:1
        // with itself unconditionally, and that must be the whole map.
        assert_eq!(prices.len(), 1);
        assert!((ratio(&prices[&eth.address]) - 1.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn test_gas_token_outside_the_graph_prices_only_itself() {
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

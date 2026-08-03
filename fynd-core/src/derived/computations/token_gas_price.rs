//! Computes the `mid_price` of tokens relative to a gas token (e.g., ETH) as the median of what a
//! token's deepest paths quote, each from a full simulation of both buy and sell directions.
//!
//! # Algorithm
//!
//! 1. **Path Discovery (DFS)**: Enumerate all paths from gas_token to each reachable token, keeping
//!    the depth of each path's thinnest hop.
//!
//! 2. **Sort**: Order paths per token by depth, deepest first, and keep the first
//!    `PRICED_PATHS_PER_TOKEN`.
//!
//! 3. **Round-Robin Simulation**: For each token, simulate every kept path. From three quotes up
//!    the token's price is their median, so a pool quoting a rate no other path agrees with is
//!    outvoted; below three it is the deepest path's. Spread ranking could do neither: a pool
//!    holding one side and almost none of the other quotes a wrong rate with a tight spread and
//!    deep liquidity, and nothing about that pool on its own says so.
//!
//! # Price Formulas
//!
//! For a path P from gas_token to target:
//! - `buy_out` = simulate(P, probe_amount) → tokens received
//! - `sell_out` = simulate(reverse(P), buy_out) → gas_token received back
//! - `buy_price` = buy_out / (probe_amount + gas_cost)
//! - `sell_price` = buy_out / (sell_out - gas_cost)
//! - `mid_price` = (buy_price + sell_price) / 2
//! - `spread` = |sell_price - buy_price|
//!
//! # Dependencies
//!
//! Needs `SpotPrices` and `PoolDepths` in `DerivedData`, so `SpotPriceComputation` and
//! `PoolDepthComputation` must run first.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use petgraph::{graph::NodeIndex, prelude::EdgeRef};
use tracing::{debug, instrument, trace, Span};
use tycho_simulation::{
    tycho_common::models::Address, tycho_core::simulation::protocol_sim::Price,
};

use crate::{
    derived::{
        computation::{
            ComputationId, ComputationOutput, ComputationRequirements, DerivedComputation,
            FailedItem, FailedItemError,
        },
        computations::{pool_depth::PoolDepthComputation, spot_price::SpotPriceComputation},
        error::ComputationError,
        manager::{ChangedComponents, SharedDerivedDataRef},
        store::DerivedData,
        types::{
            PoolDepthKey, PoolDepths, SpotPriceKey, SpotPrices, TokenGasPrices, TokenPriceEntry,
            TokenPricesWithDeps,
        },
    },
    feed::market_data::{MarketData, MarketState},
    graph::{GraphManager, Path, PetgraphStableDiGraphManager},
    types::ComponentId,
    MostLiquidAlgorithm,
};

/// A candidate path with the depth of its thinnest hop, in gas-token units.
#[derive(Clone)]
struct CandidatePath<'a> {
    path: Path<'a, ()>,
    depth: f64,
}

/// What one simulated path returned, and the two numbers used to choose between paths.
struct Quote {
    price: Price,
    /// `price` as a float, for ordering quotes by what they say the token is worth.
    ratio: f64,
    /// Depth of the path's thinnest hop, in gas-token units.
    depth: f64,
    components: HashSet<ComponentId>,
}

/// A price as a float, for ordering quotes against each other. Non-finite when the denominator is
/// zero or either side overflows f64, which keeps the quote out of the median.
fn ratio_of(price: &Price) -> f64 {
    let numerator = price
        .numerator
        .to_f64()
        .unwrap_or(f64::NAN);
    let denominator = price
        .denominator
        .to_f64()
        .unwrap_or(f64::NAN);
    numerator / denominator
}

/// How many quotes a token's price is chosen from. That is quotes, not attempts: a path whose
/// simulation fails does not count against it, so a token is not left unpriced by having several
/// deep paths that do not simulate. It is also the number of independent quotes a single wrong pool
/// has to outvote.
const PRICED_PATHS_PER_TOKEN: usize = 5;

/// Computes token prices relative to the gas token from what its deepest paths quote.
///
/// Uses DFS to discover paths, pool depths for ranking, and full simulation for accurate output
/// amounts.
#[derive(Debug, Clone)]
pub struct TokenGasPriceComputation {
    /// The gas token address (e.g., ETH).
    gas_token: Address,
    /// Maximum path length to explore.
    max_hops: usize,
    /// Amount of gas token to simulate with (affects slippage).
    simulation_amount: BigUint,
}

impl Default for TokenGasPriceComputation {
    fn default() -> Self {
        Self {
            gas_token: Address::zero(20), // ETH address
            max_hops: 2,
            simulation_amount: BigUint::from(10u64).pow(18), // 1 ETH
        }
    }
}

impl TokenGasPriceComputation {
    #[cfg(test)]
    pub fn new(gas_token: Address, max_hops: usize, simulation_amount: BigUint) -> Self {
        Self { gas_token, max_hops, simulation_amount }
    }

    /// Sets the maximum number of hops to explore.
    pub fn with_max_hops(self, max_hops: usize) -> Self {
        Self { max_hops, ..self }
    }

    /// Sets the gas token address.
    pub fn with_gas_token(self, gas_token: Address) -> Self {
        Self { gas_token, ..self }
    }

    /// DFS to discover all paths from gas_token, each carrying the depth of its thinnest hop.
    ///
    /// Every path a token has is kept. Depth ranks them rather than admitting them, so a token
    /// whose pools are all thin still gets a price — a rough one beats none, and the price of a
    /// token nothing much trades against cannot be sharp anyway.
    fn discover_paths<'a>(
        &self,
        graph_manager: &'a PetgraphStableDiGraphManager<()>,
        spot_prices: &SpotPrices,
        pool_depths: &PoolDepths,
    ) -> Result<HashMap<Address, Vec<CandidatePath<'a>>>, ComputationError> {
        let graph = graph_manager.graph();

        // If gas token has no pools, it won't be in the graph → no paths to discover
        let Ok(entry_node) = graph_manager.find_node(&self.gas_token) else {
            return Ok(HashMap::new());
        };

        let mut paths_by_token: HashMap<Address, Vec<CandidatePath>> = HashMap::new();

        // DFS state
        struct DfsFrame<'a> {
            token_node: NodeIndex,
            path: Path<'a, ()>,
            forward_spot: f64,
            /// Depth of the thinnest hop taken so far, in gas-token units.
            depth: f64,
        }

        let mut stack = vec![DfsFrame {
            token_node: entry_node,
            path: Path::new(),
            forward_spot: 1.0,
            depth: f64::INFINITY,
        }];

        while let Some(frame) = stack.pop() {
            // Token that we reached in this frame
            let token_reached = &graph[frame.token_node];

            // Record non-empty paths (skip the starting node's empty path)
            if !frame.path.is_empty() {
                paths_by_token
                    .entry(token_reached.clone())
                    .or_default()
                    .push(CandidatePath { path: frame.path.clone(), depth: frame.depth });
            }

            // Stop exploring further if max depth reached
            if frame.path.len() >= self.max_hops {
                continue;
            }

            // Explore neighbors
            for edge in graph.edges(frame.token_node) {
                let next_node = edge.target();
                let next_token = &graph[next_node];

                let mut new_path = frame.path.clone();
                new_path.add_hop(token_reached, edge.weight(), next_token);

                let component_id = edge.weight().component_id.clone();

                // Look up spot prices for this edge
                let fwd_key: SpotPriceKey =
                    (component_id.clone(), token_reached.clone(), next_token.clone());
                let rev_key: SpotPriceKey =
                    (component_id.clone(), next_token.clone(), token_reached.clone());

                // Skip edges with missing spot prices (pool may have failed spot price computation)
                let Some(&fwd_spot) = spot_prices.get(&fwd_key) else {
                    continue;
                };
                if !spot_prices.contains_key(&rev_key) {
                    continue;
                }

                let Some(hop_depth) = Self::hop_depth_in_gas_units(
                    pool_depths,
                    &component_id,
                    token_reached,
                    next_token,
                    frame.forward_spot,
                ) else {
                    continue;
                };

                stack.push(DfsFrame {
                    token_node: next_node,
                    path: new_path,
                    forward_spot: frame.forward_spot * fwd_spot,
                    depth: frame.depth.min(hop_depth),
                });
            }
        }

        Ok(paths_by_token)
    }

    /// A hop's depth in gas-token units, or `None` when it has none recorded or the conversion has
    /// nothing to work from.
    ///
    /// `PoolDepthComputation` reports depth as the largest input a pool takes before its price
    /// moves further than the slippage threshold, measured in `token_in`.
    /// `gas_to_token_in_spot` is how many `token_in` one gas token buys, so dividing converts
    /// the depth into gas-token units.
    ///
    /// A hop whose depth never computed reads as `None` rather than as deep: the alternative is
    /// pricing off a pool whose liquidity is unknown.
    fn hop_depth_in_gas_units(
        pool_depths: &PoolDepths,
        component_id: &ComponentId,
        token_in: &Address,
        token_out: &Address,
        gas_to_token_in_spot: f64,
    ) -> Option<f64> {
        if !gas_to_token_in_spot.is_finite() || gas_to_token_in_spot <= 0.0 {
            return None;
        }
        let key: PoolDepthKey = (component_id.clone(), token_in.clone(), token_out.clone());
        let depth = pool_depths
            .get(&key)
            .and_then(ToPrimitive::to_f64)
            .unwrap_or(0.0);
        Some(depth / gas_to_token_in_spot)
    }

    /// The quote a token's price comes from: the median by price once three paths have quoted,
    /// and the deepest path before that.
    ///
    /// Two quotes cannot outvote each other, so depth decides instead of an arbitrary middle.
    /// Either way the price is one a path actually returned rather than an average of several,
    /// so it keeps exact numerator and denominator along with the components it came from.
    fn chosen_quote(mut quotes: Vec<Quote>) -> Option<Quote> {
        if quotes.len() < 3 {
            quotes.sort_by(|a, b| a.depth.total_cmp(&b.depth));
            return quotes.pop();
        }
        quotes.sort_by(|a, b| a.ratio.total_cmp(&b.ratio));
        let middle = quotes.len() / 2;
        quotes.drain(middle..=middle).next()
    }

    /// Compute the spread and mid_price for a given path by simulating both directions.
    ///
    /// Returns (spread_ratio, mid_price, path_components) where:
    /// - spread_ratio: |sell - buy|, lower = more reliable
    /// - mid_price: precise Price struct
    /// - path_components: component IDs used in this path (for incremental invalidation)
    fn compute_spread_and_mid_price(
        &self,
        path: Path<()>,
        market: &MarketState,
        gas_price: &BigUint,
    ) -> Result<(f64, Price, HashSet<ComponentId>), ComputationError> {
        // Extract component IDs from path edges for dependency tracking
        let path_components: HashSet<ComponentId> = path
            .edge_data
            .iter()
            .map(|edge| edge.component_id.clone())
            .collect();
        // Forward: gas_token → target_token
        let buy_result =
            MostLiquidAlgorithm::simulate_path(&path, market, None, self.simulation_amount.clone())
                .map_err(|e| {
                    ComputationError::SimulationFailed(format!("buy simulation failed: {}", e))
                })?;
        let buy_gas_units = buy_result.route().total_gas();
        let buy_gas_cost = &buy_gas_units * gas_price; // Convert gas units to actual cost
        let buy_out = buy_result
            .into_route()
            .into_swaps()
            .into_iter()
            .last()
            .ok_or(ComputationError::Internal("no output from buy simulation".into()))?
            .amount_out()
            .clone();

        // Reverse: target_token → gas_token
        let reversed_path = path.reversed();

        let sell_result =
            MostLiquidAlgorithm::simulate_path(&reversed_path, market, None, buy_out.clone())
                .map_err(|e| {
                    ComputationError::SimulationFailed(format!("sell simulation failed: {}", e))
                })?;
        let sell_gas_units = sell_result.route().total_gas();
        let sell_gas_cost = &sell_gas_units * gas_price; // Convert gas units to actual cost
        let sell_out = sell_result
            .into_route()
            .into_swaps()
            .into_iter()
            .last()
            .ok_or(ComputationError::Internal("no output from sell simulation".into()))?
            .amount_out()
            .clone();

        // Convert to f64 for mid_price calculation
        let buy_out_f = buy_out
            .to_f64()
            .ok_or(ComputationError::Internal("overflow computing buy_out".into()))?;
        let sell_out_f = sell_out
            .to_f64()
            .ok_or(ComputationError::Internal("overflow computing sell_out".into()))?;
        let buy_gas_cost_f = buy_gas_cost
            .to_f64()
            .ok_or(ComputationError::Internal("overflow computing buy_gas_cost".into()))?;
        let sell_gas_cost_f = sell_gas_cost
            .to_f64()
            .ok_or(ComputationError::Internal("overflow computing sell_gas_cost".into()))?;
        let sim_amount_f = self
            .simulation_amount
            .to_f64()
            .ok_or(ComputationError::Internal("overflow computing simulation_amount".into()))?;

        // Guard: if gas cost exceeds sell output, this path is not viable
        if sell_gas_cost >= sell_out {
            return Err(ComputationError::SimulationFailed(
                "gas cost exceeds sell output - path not viable".into(),
            ));
        }

        // buy_price: tokens received per (gas_token spent + gas cost)
        let buy_price = buy_out_f / (sim_amount_f + buy_gas_cost_f);

        // sell_price: tokens we had / (gas_token received - gas cost)
        let sell_price = buy_out_f / (sell_out_f - sell_gas_cost_f);

        let spread = (sell_price - buy_price).abs();

        // Compute mid_price in numerator/denominator form (precise BigUint arithmetic)
        // numerator = buy_out * (sell_out - sell_gas_cost) + buy_out * (sim_amount + buy_gas_cost)
        // denominator = 2 * (sim_amount + buy_gas_cost) * (sell_out - sell_gas_cost)
        let sell_out_net = &sell_out - &sell_gas_cost; // Safe: checked above
        let buy_price_precise = Price {
            numerator: &buy_out * &sell_out_net +
                &buy_out * (&self.simulation_amount + &buy_gas_cost),
            denominator: BigUint::from(2u8) *
                (&self.simulation_amount + &buy_gas_cost) *
                sell_out_net,
        };

        Ok((spread, buy_price_precise, path_components))
    }

    /// Core simulation logic: discovers paths, runs round-robin simulation,
    /// returns best prices with dependency tracking and block number.
    ///
    /// Takes two brief read locks on market:
    /// 1. Clone topology + gas_price + block (cheap)
    /// 2. `extract_subset` with only the components on candidate paths
    ///
    /// Path discovery (cheap DFS) runs twice to avoid holding borrows across await
    /// points. The expensive part — EVM simulation — runs lock-free on the subset.
    ///
    /// # Arguments
    ///
    /// * `market`: The market data to simulate token prices on.
    /// * `spot_prices`: The spot prices to use for the simulation.
    /// * `pool_depths`: The depths that decide which hops are deep enough to price through.
    /// * `filter_tokens`: An optional set of tokens to filter the simulation by. If None, all
    ///   tokens are simulated.
    ///
    /// # Returns
    ///
    /// A tuple containing the best prices and the block number.
    #[allow(clippy::type_complexity)]
    async fn simulate_token_prices(
        &self,
        market: &MarketData,
        spot_prices: &SpotPrices,
        pool_depths: &PoolDepths,
        filter_tokens: Option<&HashSet<Address>>,
    ) -> Result<
        (HashMap<Address, (Price, HashSet<ComponentId>)>, u64, Vec<FailedItem>),
        ComputationError,
    > {
        // Brief lock 1: topology + gas_price + block (all cheap clones)
        let (topology, gas_price, block) = {
            let guard = market.read().await;
            let topology = guard.component_topology();
            let block = guard
                .last_updated()
                .map(|b| b.number())
                .unwrap_or(0);
            let gas_price = guard
                .gas_price()
                .ok_or(ComputationError::MissingDependency("gas_price"))?
                .effective_gas_price();
            (topology, gas_price, block)
        };

        // Discover paths to find which components candidate paths need (cheap DFS)
        let needed_component_ids = {
            let mut graph_manager = PetgraphStableDiGraphManager::new();
            graph_manager.initialize_graph(&topology);
            let mut paths = self.discover_paths(&graph_manager, spot_prices, pool_depths)?;
            if let Some(tokens) = filter_tokens {
                paths.retain(|token, _| tokens.contains(token));
            }
            paths
                .values()
                .flatten()
                .flat_map(|c| {
                    c.path
                        .edge_data
                        .iter()
                        .map(|e| e.component_id.clone())
                })
                .collect::<HashSet<ComponentId>>()
        };

        // Brief lock 2: extract only the simulation states we need
        let subset = {
            market
                .read()
                .await
                .extract_subset(&needed_component_ids)
        };

        // Rediscover paths from subset + simulate (no lock, expensive EVM simulation)
        let mut graph_manager = PetgraphStableDiGraphManager::new();
        graph_manager.initialize_graph(&subset.component_topology());
        let mut paths_by_token = self.discover_paths(&graph_manager, spot_prices, pool_depths)?;

        // Optionally filter to only requested tokens
        if let Some(tokens) = filter_tokens {
            paths_by_token.retain(|token, _| tokens.contains(token));
        }

        // Collect all component IDs from every candidate path per token.
        // This ensures path_components captures any pool that could flip which path is best,
        // not just pools on the currently-selected path.
        let all_candidate_components: HashMap<Address, HashSet<ComponentId>> = paths_by_token
            .iter()
            .map(|(token, candidates)| {
                let components = candidates
                    .iter()
                    .flat_map(|c| {
                        c.path
                            .edge_data
                            .iter()
                            .map(|e| e.component_id.clone())
                    })
                    .collect::<HashSet<_>>();
                (token.clone(), components)
            })
            .collect();

        // Order each token's candidates shallowest first, so popping takes the deepest. A
        // non-finite depth cannot rank a path and would panic a partial_cmp-based sort, so drop
        // those candidates and sort with the float total order.
        for paths in paths_by_token.values_mut() {
            paths.retain(|path| path.depth.is_finite());
            paths.sort_by(|a, b| a.depth.total_cmp(&b.depth));
        }

        // Round-robin: pop one candidate per token each round, collecting every quote that
        // simulates, then reduce each token's quotes to their median.
        let mut quotes: HashMap<Address, Vec<Quote>> = HashMap::new();
        let mut candidates_exhausted = false;

        while !candidates_exhausted {
            candidates_exhausted = true;

            for (token, candidate_paths) in paths_by_token.iter_mut() {
                // Enough quotes for this token; drop its remaining candidates so the loop ends.
                if quotes.get(token).map_or(0, Vec::len) >= PRICED_PATHS_PER_TOKEN {
                    candidate_paths.clear();
                    continue;
                }
                let Some(candidate) = candidate_paths.pop() else {
                    continue;
                };
                candidates_exhausted = false;

                // A non-finite spread means the round trip was degenerate, so the price it came
                // with does not belong in the median.
                let depth = candidate.depth;
                let Ok((spread, price, components)) =
                    self.compute_spread_and_mid_price(candidate.path, &subset, &gas_price)
                else {
                    continue;
                };
                let ratio = ratio_of(&price);
                if spread.is_finite() && ratio.is_finite() {
                    quotes
                        .entry(token.clone())
                        .or_default()
                        .push(Quote { price, ratio, depth, components });
                }
            }
        }

        let mut best_prices: HashMap<Address, (Price, HashSet<ComponentId>)> = HashMap::new();
        for (token, token_quotes) in quotes {
            let priced_paths = token_quotes.len();
            if let Some(quote) = Self::chosen_quote(token_quotes) {
                trace!(token = ?token, priced_paths, "chose from the token's priced paths");
                best_prices.insert(token, (quote.price, quote.components));
            }
        }

        // Extend each token's path_components with all candidate path components so
        // incremental recomputation fires when any competing path's pool changes.
        for (token, (_, components)) in best_prices.iter_mut() {
            if let Some(all_comps) = all_candidate_components.get(token) {
                components.extend(all_comps.iter().cloned());
            }
        }

        // Tokens with discovered paths but no successful simulation
        let failed_items: Vec<FailedItem> = paths_by_token
            .keys()
            .filter(|token| !best_prices.contains_key(*token))
            .map(|token| FailedItem {
                key: token.to_string(),
                error: FailedItemError::AllSimulationPathsFailed,
            })
            .collect();

        Ok((best_prices, block, failed_items))
    }

    /// Attempts incremental recomputation for state-only changes.
    ///
    /// Only recomputes token prices whose dependency paths intersect with changed components.
    /// Returns `Ok(Some(prices))` if incremental recomputation succeeded,
    /// `Ok(None)` if full recomputation is needed (e.g., no dependencies stored yet),
    /// or `Err` if computation failed.
    async fn try_incremental_compute(
        &self,
        market: &MarketData,
        store: &SharedDerivedDataRef,
        changed: &ChangedComponents,
    ) -> Result<Option<ComputationOutput<TokenGasPrices>>, ComputationError> {
        // Read all needed data from store in a single lock acquisition.
        let (existing_deps, existing_prices, spot_prices, pool_depths) = {
            let store_guard = store.read().await;

            // Need existing deps to do incremental computation.
            let Some(existing_deps) = store_guard.token_prices_deps().cloned() else {
                return Ok(None); // No deps stored yet, need full compute
            };
            let Some(existing_prices) = store_guard.token_prices().cloned() else {
                return Ok(None);
            };
            let spot_prices = store_guard
                .spot_prices()
                .ok_or(ComputationError::MissingDependency("spot_prices"))?
                .clone();
            let pool_depths = store_guard
                .pool_depths()
                .ok_or(ComputationError::MissingDependency("pool_depths"))?
                .clone();

            (existing_deps, existing_prices, spot_prices, pool_depths)
        };

        let changed_components = changed.all_changed_ids();

        // Find tokens whose paths intersect with changed components.
        let tokens_to_recompute: HashSet<Address> = existing_deps
            .iter()
            .filter(|(_, entry)| {
                !entry
                    .path_components
                    .is_disjoint(&changed_components)
            })
            .map(|(addr, _)| addr.clone())
            .collect();

        if tokens_to_recompute.is_empty() {
            return Ok(Some(ComputationOutput::success(existing_prices.clone())));
        }

        debug!(
            affected_tokens = tokens_to_recompute.len(),
            total_tokens = existing_prices.len(),
            "incremental token price recomputation"
        );

        let (best_prices, block, _) = self
            .simulate_token_prices(market, &spot_prices, &pool_depths, Some(&tokens_to_recompute))
            .await?;

        // Merge results into existing prices and deps
        let mut result = existing_prices;
        let mut new_deps = existing_deps;
        let mut failed_items: Vec<FailedItem> = Vec::new();

        for token in &tokens_to_recompute {
            if let Some((price, components)) = best_prices.get(token) {
                new_deps.insert(
                    token.clone(),
                    TokenPriceEntry { price: price.clone(), path_components: components.clone() },
                );
                result.insert(token.clone(), price.clone());
            } else {
                result.remove(token);
                new_deps.remove(token);
                failed_items.push(FailedItem {
                    key: token.to_string(),
                    error: FailedItemError::AllSimulationPathsFailed,
                });
            }
        }

        store
            .write()
            .await
            .set_token_prices_deps(new_deps, block);
        Span::current().record("updated_token_prices", result.len());

        Ok(Some(ComputationOutput::with_failures(result, failed_items)))
    }
}

#[async_trait]
impl DerivedComputation for TokenGasPriceComputation {
    type Output = TokenGasPrices;

    const ID: ComputationId = "token_prices";

    fn requirements(&self) -> ComputationRequirements {
        ComputationRequirements::fresh([SpotPriceComputation::ID, PoolDepthComputation::ID])
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
        // For topology changes or full recompute, do a full computation
        // For state-only changes, use incremental computation
        if !changed.is_full_recompute && !changed.is_topology_change() {
            // Try incremental computation if we have existing path dependencies
            if let Some(result) = self
                .try_incremental_compute(market, store, changed)
                .await?
            {
                return Ok(result);
            }
            // Fall through to full compute if incremental is not possible
        }

        // Read spot prices and depths from store (independent of market lock).
        let (spot_prices, pool_depths) = {
            let store_guard = store.read().await;
            let spot_prices = store_guard
                .spot_prices()
                .ok_or(ComputationError::MissingDependency("spot_prices"))?
                .clone();
            let pool_depths = store_guard
                .pool_depths()
                .ok_or(ComputationError::MissingDependency("pool_depths"))?
                .clone();
            (spot_prices, pool_depths)
        };

        let (best_prices, block, failed_items) = self
            .simulate_token_prices(market, &spot_prices, &pool_depths, None)
            .await?;

        // Build token prices with dependencies for incremental computation
        let mut token_prices_with_deps = TokenPricesWithDeps::new();
        let mut token_prices = TokenGasPrices::new();

        for (token, (price, path_components)) in best_prices {
            token_prices_with_deps
                .insert(token.clone(), TokenPriceEntry { price: price.clone(), path_components });
            token_prices.insert(token, price);
        }

        // Add the gas token itself with price 1:1 (no path dependencies since it's the root)
        let gas_token_price = Price {
            numerator: self.simulation_amount.clone(),
            denominator: self.simulation_amount.clone(),
        };
        token_prices_with_deps.insert(
            self.gas_token.clone(),
            TokenPriceEntry { price: gas_token_price.clone(), path_components: HashSet::new() },
        );
        token_prices.insert(self.gas_token.clone(), gas_token_price);

        store
            .write()
            .await
            .set_token_prices_deps(token_prices_with_deps, block);

        debug!(priced = token_prices.len() - 1, "token price computation complete");

        Span::current().record("updated_token_prices", token_prices.len());

        Ok(ComputationOutput::with_failures(token_prices, failed_items))
    }
}

#[cfg(test)]
mod tests {
    use tycho_simulation::tycho_core::models::token::Token;

    use super::*;
    use crate::{
        algorithm::test_utils::{
            component, market_read, setup_market_weighted, token, MockProtocolSim,
        },
        derived::{computations::spot_price::SpotPriceComputation, store::DerivedData},
    };
    // ==================== Constants ====================

    /// Standard simulation amount: 1 ETH = 10^18 wei.
    const SIM_AMOUNT: u128 = 1_000_000_000_000_000_000;

    /// Gas price set by setup_market_weighted: 100 wei/gas.
    const GAS_PRICE: u64 = 100;

    // ==================== Test Helpers ====================

    /// Depth written for every pool direction by `setup_test_env`, ten times the probe so a test
    /// that says nothing about depth prices as it always did. Tests about the depth check itself
    /// call `set_depths` to overwrite specific directions.
    const DEEP: u64 = 10_000_000_000_000_000_000;

    /// Sets up a complete test environment: market with pools + precomputed spot prices, and a
    /// depth above the probe for every pool direction.
    /// Returns (market_guard, store) ready for computation.
    async fn setup_test_env(
        pools: Vec<(&str, &Token, &Token, MockProtocolSim)>,
    ) -> (MarketData, SharedDerivedDataRef) {
        let (wrapped_market, _) = setup_market_weighted(pools.clone());

        let wrapped_store = DerivedData::new_shared();
        let spot_comp = SpotPriceComputation::new();
        let changed = ChangedComponents {
            added: pools
                .iter()
                .map(|(id, t1, t2, _)| {
                    (id.to_string(), vec![t1.address.clone(), t2.address.clone()])
                })
                .collect(),
            removed: vec![],
            updated: vec![],
            is_full_recompute: true,
        };
        let spot_prices_output = spot_comp
            .compute(&wrapped_market, &wrapped_store, &changed)
            .await
            .expect("spot price computation should succeed");
        let mut pool_depths = PoolDepths::new();
        for (id, t1, t2, _) in &pools {
            pool_depths.insert(
                (id.to_string(), t1.address.clone(), t2.address.clone()),
                BigUint::from(DEEP),
            );
            pool_depths.insert(
                (id.to_string(), t2.address.clone(), t1.address.clone()),
                BigUint::from(DEEP),
            );
        }

        {
            let mut store_guard = wrapped_store.try_write().unwrap();
            store_guard.set_spot_prices(spot_prices_output.data, vec![], 0, true);
            store_guard.set_pool_depths(pool_depths, vec![], 0, true);
        }

        (wrapped_market, wrapped_store)
    }

    /// A tenth of the probe, for the hop a test wants path discovery to skip.
    const BELOW_PROBE: u64 = 100_000_000_000_000_000;

    /// Overwrites the depth of one pool direction, for tests that drive the depth check.
    fn set_depths(
        store: &SharedDerivedDataRef,
        depths: Vec<(&str, &Address, &Address, u64)>,
    ) -> PoolDepths {
        let mut store_guard = store.try_write().unwrap();
        let mut pool_depths = store_guard
            .pool_depths()
            .cloned()
            .unwrap_or_default();
        for (id, token_in, token_out, depth) in depths {
            pool_depths.insert(
                (id.to_string(), token_in.clone(), token_out.clone()),
                BigUint::from(depth),
            );
        }
        store_guard.set_pool_depths(pool_depths.clone(), vec![], 0, true);
        pool_depths
    }

    async fn setup_graph_and_derived(
        pools: Vec<(&str, &Token, &Token, MockProtocolSim)>,
    ) -> (PetgraphStableDiGraphManager<()>, SpotPrices, PoolDepths) {
        let (market, derived) = setup_test_env(pools).await;
        let market = market_read(&market);

        let mut graph = PetgraphStableDiGraphManager::new();
        graph.initialize_graph(&market.component_topology());

        let guard = derived.try_write().unwrap();
        let spot_prices = guard.spot_prices().unwrap().clone();
        let pool_depths = guard.pool_depths().unwrap().clone();
        (graph, spot_prices, pool_depths)
    }

    /// Creates a computation configured for the given gas token with standard settings.
    fn computation_for(gas_token: &Address) -> TokenGasPriceComputation {
        TokenGasPriceComputation::new(gas_token.clone(), 2, BigUint::from(SIM_AMOUNT))
    }

    // ==================== discover_paths tests ====================

    #[tokio::test]
    async fn test_discover_paths_single_hop() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let (graph_manager, spot_prices, pool_depths) =
            setup_graph_and_derived(vec![("pool", &eth, &usdc, MockProtocolSim::new(2000.0))])
                .await;

        let computation = computation_for(&eth.address);
        let paths = computation
            .discover_paths(&graph_manager, &spot_prices, &pool_depths)
            .unwrap();

        // Exactly 1 path to USDC (single hop via "pool")
        let usdc_paths = &paths[&usdc.address];
        assert_eq!(usdc_paths.len(), 1, "should have exactly 1 path to USDC");

        let path = &usdc_paths[0];
        assert_eq!(path.path.len(), 1, "path should be single hop");
        assert_eq!(path.path.edge_data[0].component_id, "pool");

        // Depth is the thinnest hop's, converted to gas-token units; a single hop out of the gas
        // token needs no conversion, so it is what setup_test_env wrote.
        assert_eq!(path.depth, DEEP as f64);
    }

    #[tokio::test]
    async fn test_discover_paths_multi_hop() {
        let eth = token(0, "ETH");
        let mid = token(2, "MID");
        let target = token(3, "TARGET");

        let (graph, spot_prices, pool_depths) = setup_graph_and_derived(vec![
            ("hop1", &eth, &mid, MockProtocolSim::new(2.0)),
            ("hop2", &mid, &target, MockProtocolSim::new(3.0)),
        ])
        .await;

        let computation = computation_for(&eth.address);
        let paths = computation
            .discover_paths(&graph, &spot_prices, &pool_depths)
            .unwrap();

        // MID: exactly 1 path (1-hop via hop1)
        let mid_paths = &paths[&mid.address];
        assert_eq!(mid_paths.len(), 1, "should have exactly 1 path to MID");
        assert_eq!(mid_paths[0].path.len(), 1, "MID path should be 1 hop");
        assert_eq!(mid_paths[0].path.edge_data[0].component_id, "hop1");
        assert_eq!(mid_paths[0].depth, DEEP as f64);

        // TARGET: exactly 1 path (2-hop via hop1 → hop2)
        let target_paths = &paths[&target.address];
        assert_eq!(target_paths.len(), 1, "should have exactly 1 path to TARGET");
        assert_eq!(target_paths[0].path.len(), 2, "TARGET path should be 2 hops");
        assert_eq!(target_paths[0].path.edge_data[0].component_id, "hop1");
        assert_eq!(target_paths[0].path.edge_data[1].component_id, "hop2");
        // hop2's depth is in MID, worth 2 gas units each, so it is the thinner of the two.
        assert_eq!(target_paths[0].depth, DEEP as f64 / 2.0);
    }

    #[tokio::test]
    async fn non_finite_spot_prices_cannot_panic_or_reach_the_median() {
        // Degenerate pool math can yield NaN spot prices. Ranking must not panic on them, and a hop
        // whose depth cannot be converted out of its input token must not be walked: converting
        // needs the spot product from the gas token, and NaN carries no amount.
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");
        let dai = token(2, "DAI");

        let (market, _) = setup_market_weighted(vec![
            ("nan_pool", &eth, &usdc, MockProtocolSim::new(2000.0)),
            ("usdc_dai", &usdc, &dai, MockProtocolSim::new(1.0)),
        ]);
        let mut spot_prices = SpotPrices::default();
        for (component, from, to) in [
            ("nan_pool", &eth.address, &usdc.address),
            ("nan_pool", &usdc.address, &eth.address),
            ("usdc_dai", &usdc.address, &dai.address),
            ("usdc_dai", &dai.address, &usdc.address),
        ] {
            spot_prices.insert((component.to_string(), from.clone(), to.clone()), f64::NAN);
        }
        let mut pool_depths = PoolDepths::new();
        for (component, from, to) in
            [("nan_pool", &eth.address, &usdc.address), ("usdc_dai", &usdc.address, &dai.address)]
        {
            pool_depths
                .insert((component.to_string(), from.clone(), to.clone()), BigUint::from(DEEP));
        }

        let computation = computation_for(&eth.address);
        let (prices, _, _) = computation
            .simulate_token_prices(&market, &spot_prices, &pool_depths, None)
            .await
            .expect("NaN spot prices must not fail the computation");

        // USDC is one hop from the gas token, so its depth needs no conversion and it still prices.
        assert!(prices.contains_key(&usdc.address), "the first hop needs no spot product");
        // DAI's hop is denominated in USDC, and the product that would convert it is NaN.
        assert!(!prices.contains_key(&dai.address), "a NaN conversion must not yield a price");
    }

    #[tokio::test]
    async fn test_compute_outvotes_the_bsc_pool_that_mispriced_usdt() {
        // The case this change was written for, at its measured rates. On BSC a WBNB-USDT pool
        // quoting 0.0464 USDT per BNB priced USDT 12191x under the 565.21 that USDC and BUSD
        // independently agreed on (live monitor, 2026-07-29). The pool holds one side and almost
        // none of the other, so it quotes that rate with deep liquidity and a tight round trip:
        // neither depth nor spread rules it out. Only the disagreement does.
        const AGREED: f64 = 565.21;
        const LOPSIDED: f64 = AGREED / 12191.0;

        let wbnb = token(0, "WBNB");
        let usdt = token(1, "USDT");

        let (market, derived) = setup_test_env(vec![
            ("pancake_v2", &wbnb, &usdt, MockProtocolSim::new(AGREED).with_fee(0.0025)),
            ("pancake_v3", &wbnb, &usdt, MockProtocolSim::new(AGREED).with_fee(0.0005)),
            ("biswap", &wbnb, &usdt, MockProtocolSim::new(AGREED).with_fee(0.003)),
            ("lopsided", &wbnb, &usdt, MockProtocolSim::new(LOPSIDED)),
        ])
        .await;

        let computation = computation_for(&wbnb.address);
        let prices = computation
            .compute(&market, &derived, &ChangedComponents::default())
            .await
            .unwrap()
            .data;

        let usdt_price = prices
            .get(&usdt.address)
            .expect("USDT should have price");
        let ratio =
            usdt_price.numerator.to_f64().unwrap() / usdt_price.denominator.to_f64().unwrap();
        assert!(
            (550.0..580.0).contains(&ratio),
            "the three agreeing pools should outvote the lopsided one (~{AGREED}), got {ratio}"
        );
    }

    #[tokio::test]
    async fn test_discover_paths_respects_max_hops() {
        let eth = token(0, "ETH");
        let a = token(2, "A");
        let b = token(3, "B");
        let c = token(4, "C");

        let (graph, spot_prices, pool_depths) = setup_graph_and_derived(vec![
            ("eth_a", &eth, &a, MockProtocolSim::new(2.0)),
            ("a_b", &a, &b, MockProtocolSim::new(2.0)),
            ("b_c", &b, &c, MockProtocolSim::new(2.0)),
        ])
        .await;

        // max_hops = 2
        let computation = computation_for(&eth.address);
        let paths = computation
            .discover_paths(&graph, &spot_prices, &pool_depths)
            .unwrap();

        // A: exactly 1 path (1 hop via eth_a)
        let a_paths = &paths[&a.address];
        assert_eq!(a_paths.len(), 1, "should have exactly 1 path to A");
        assert_eq!(a_paths[0].path.len(), 1, "A path should be 1 hop");
        assert_eq!(a_paths[0].path.edge_data[0].component_id, "eth_a");

        // B: exactly 1 path (2 hops via eth_a → a_b)
        let b_paths = &paths[&b.address];
        assert_eq!(b_paths.len(), 1, "should have exactly 1 path to B");
        assert_eq!(b_paths[0].path.len(), 2, "B path should be 2 hops");
        assert_eq!(b_paths[0].path.edge_data[0].component_id, "eth_a");
        assert_eq!(b_paths[0].path.edge_data[1].component_id, "a_b");

        // C: not reachable (would require 3 hops, exceeds max_hops=2)
        assert!(!paths.contains_key(&c.address), "C should NOT be reachable (3 hops)");
    }

    #[tokio::test]
    async fn test_discover_paths_returns_multiple_candidates() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Two pools with different spot prices
        let (graph, spot_prices, pool_depths) = setup_graph_and_derived(vec![
            ("pool_low", &eth, &usdc, MockProtocolSim::new(1000.0)),
            ("pool_high", &eth, &usdc, MockProtocolSim::new(2000.0)),
        ])
        .await;

        let computation = computation_for(&eth.address);
        let paths = computation
            .discover_paths(&graph, &spot_prices, &pool_depths)
            .unwrap();

        // Exactly 2 paths to USDC (one via each pool)
        let usdc_paths = &paths[&usdc.address];
        assert_eq!(usdc_paths.len(), 2, "should have exactly 2 paths to USDC");

        for path in usdc_paths {
            assert_eq!(path.path.len(), 1, "path should be single hop");
        }

        // Verify both pools are discovered (order is arbitrary when scores are equal)
        let component_ids: Vec<_> = usdc_paths
            .iter()
            .map(|p| {
                p.path.edge_data[0]
                    .component_id
                    .as_str()
            })
            .collect();
        assert!(component_ids.contains(&"pool_low"));
        assert!(component_ids.contains(&"pool_high"));
    }

    #[tokio::test]
    async fn test_discover_paths_records_the_thinnest_hop_depth() {
        // Two pools price the same pair. Both are symmetric, so spread cannot tell them apart.
        // Each path carries its own depth, which is what ranks them later.
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let (market, derived) = setup_test_env(vec![
            ("deep", &eth, &usdc, MockProtocolSim::new(2000.0)),
            ("shallow", &eth, &usdc, MockProtocolSim::new(1.0)),
        ])
        .await;
        let pool_depths =
            set_depths(&derived, vec![("shallow", &eth.address, &usdc.address, BELOW_PROBE)]);
        let spot_prices = derived
            .try_write()
            .unwrap()
            .spot_prices()
            .unwrap()
            .clone();

        let mut graph = PetgraphStableDiGraphManager::new();
        graph.initialize_graph(&market_read(&market).component_topology());

        let computation = computation_for(&eth.address);
        let paths = computation
            .discover_paths(&graph, &spot_prices, &pool_depths)
            .unwrap();

        let mut usdc_paths: Vec<(&str, f64)> = paths[&usdc.address]
            .iter()
            .map(|c| {
                (
                    c.path.edge_data[0]
                        .component_id
                        .as_str(),
                    c.depth,
                )
            })
            .collect();
        usdc_paths.sort_by(|a, b| a.1.total_cmp(&b.1));
        assert_eq!(usdc_paths, vec![("shallow", BELOW_PROBE as f64), ("deep", DEEP as f64)]);
    }

    #[tokio::test]
    async fn test_discover_paths_reads_an_unmeasured_hop_as_zero_depth() {
        // A pool whose depth never computed still yields a path, at a depth that ranks it last.
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let (market, derived) =
            setup_test_env(vec![("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0))]).await;
        let spot_prices = {
            let mut store_guard = derived.try_write().unwrap();
            store_guard.set_pool_depths(PoolDepths::new(), vec![], 0, true);
            store_guard
                .spot_prices()
                .unwrap()
                .clone()
        };

        let mut graph = PetgraphStableDiGraphManager::new();
        graph.initialize_graph(&market_read(&market).component_topology());

        let computation = computation_for(&eth.address);
        let paths = computation
            .discover_paths(&graph, &spot_prices, &PoolDepths::new())
            .unwrap();

        assert_eq!(paths[&usdc.address][0].depth, 0.0, "unmeasured depth reads as zero");
    }

    // ==================== compute_spread_and_mid_price tests ====================

    #[tokio::test]
    async fn test_compute_spread_and_mid_price_with_gas_and_fee() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Non-trivial setup: 10% fee + significant gas (10% of sim_amount)
        // gas_units = 1e15, gas_cost = 1e15 * 100 = 1e17 (10% of 1e18)
        //
        // Forward (ETH→USDC):
        //   buy_out = 1e18 * 2000 * 0.9 = 1.8e21
        //   buy_gas_cost = 1e17
        //
        // Reverse (USDC→ETH):
        //   sell_out = 1.8e21 / 2000 * 0.9 = 8.1e17
        //   sell_gas_cost = 1e17
        //
        // buy_price = buy_out / (sim_amount + buy_gas_cost)
        //           = 1.8e21 / (1e18 + 1e17) = 1.8e21 / 1.1e18 = 18000/11 ≈ 1636.36
        //
        // sell_price = buy_out / (sell_out - sell_gas_cost)
        //            = 1.8e21 / (8.1e17 - 1e17) = 1.8e21 / 7.1e17 = 180000/71 ≈ 2535.21
        //
        // spread = |sell_price - buy_price| = 180000/71 - 18000/11 = 702000/781 ≈ 898.85
        // mid_price = (buy_price + sell_price) / 2 ≈ 2085.79
        let gas_units: u64 = 1_000_000_000_000_000; // 1e15
        let (market, _) = setup_test_env(vec![(
            "pool",
            &eth,
            &usdc,
            MockProtocolSim::new(2000.0)
                .with_gas(gas_units)
                .with_fee(0.1),
        )])
        .await;
        let market = market_read(&market);

        // Build path manually using graph
        let mut graph = PetgraphStableDiGraphManager::new();
        graph.initialize_graph(&market.component_topology());

        let eth_node = graph.find_node(&eth.address).unwrap();
        let path_edges: Vec<_> = graph.graph().edges(eth_node).collect();
        assert_eq!(path_edges.len(), 1);

        let edge = path_edges[0].weight();
        let mut path = Path::new();
        path.add_hop(&eth.address, edge, &usdc.address);

        let gas_price = BigUint::from(GAS_PRICE);
        let computation = computation_for(&eth.address);
        let (spread, mid_price, _path_components) = computation
            .compute_spread_and_mid_price(path, market.base_market_state(), &gas_price)
            .unwrap();

        // Expected values from exact fractions
        let buy_price = 18000.0 / 11.0; // 1636.363636...
        let sell_price = 180000.0 / 71.0; // 2535.211267...
        let expected_spread = sell_price - buy_price; // ~898.85
        let expected_mid = (buy_price + sell_price) / 2.0; // ~2085.79

        assert!(
            (spread - expected_spread).abs() < 1e-5,
            "spread should be {expected_spread}, got {spread}"
        );

        let ratio = mid_price.numerator.to_f64().unwrap() / mid_price.denominator.to_f64().unwrap();
        assert!(
            (ratio - expected_mid).abs() < 1e-5,
            "mid_price should be {expected_mid}, got {ratio}"
        );
    }

    // ==================== compute tests ====================

    #[tokio::test]
    async fn test_compute_single_hop_mid_price() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let spot_price: f64 = 2000.0;
        let gas_units: u64 = 50_000;

        let (market, derived) = setup_test_env(vec![(
            "eth_usdc",
            &eth,
            &usdc,
            MockProtocolSim::new(spot_price).with_gas(gas_units),
        )])
        .await;
        let changed = ChangedComponents::default();

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market, &derived, &changed)
            .await
            .unwrap()
            .data;

        // Exactly 2 prices: ETH (gas token) and USDC
        assert_eq!(prices.len(), 2, "should have exactly 2 token prices");

        // Gas token (ETH) should have exact 1:1 price
        let eth_price = prices
            .get(&eth.address)
            .expect("ETH should have price");
        assert_eq!(
            eth_price.numerator, eth_price.denominator,
            "gas token must have exact 1:1 price"
        );
        assert_eq!(
            eth_price.numerator,
            BigUint::from(SIM_AMOUNT),
            "gas token numerator should equal simulation amount"
        );

        // USDC mid-price should be 2000 (symmetric pool, no fee)
        // Small deviation due to gas cost adjustment in buy_price/sell_price
        let usdc_price = prices
            .get(&usdc.address)
            .expect("USDC should have price");
        let ratio =
            usdc_price.numerator.to_f64().unwrap() / usdc_price.denominator.to_f64().unwrap();
        assert!((ratio - 2000.0).abs() < 1e-6, "mid-price should be ~2000, got {ratio}");
    }

    #[tokio::test]
    async fn test_compute_prefers_the_deeper_of_two_pools() {
        // Two quotes cannot outvote each other, so the deeper path decides. The real pool charges a
        // fee and the shallow one does not, so on spread alone the shallow one — quoting a rate
        // 2000x off — would have won.
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let (market, derived) = setup_test_env(vec![
            ("deep", &eth, &usdc, MockProtocolSim::new(2000.0).with_fee(0.01)),
            ("shallow", &eth, &usdc, MockProtocolSim::new(1.0)),
        ])
        .await;
        set_depths(&derived, vec![("shallow", &eth.address, &usdc.address, BELOW_PROBE)]);

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market, &derived, &ChangedComponents::default())
            .await
            .unwrap()
            .data;

        let usdc_price = prices
            .get(&usdc.address)
            .expect("USDC should have price");
        let ratio =
            usdc_price.numerator.to_f64().unwrap() / usdc_price.denominator.to_f64().unwrap();
        assert!(
            (1900.0..2100.0).contains(&ratio),
            "price should come from the deep pool (~2000), got {ratio}"
        );
    }

    #[tokio::test]
    async fn test_compute_prices_a_forked_token_from_the_median_path() {
        // Diamond topology: two paths to C
        //
        //     A (10% fee on eth_a)
        //    / \
        // ETH   C
        //    \ /
        //     B (5% fee on eth_b)
        //
        // Only first hops have fees; second hops (a_c, b_c) are fee-free.
        // Gas = 0 to simplify calculations.
        //
        // Path via A (eth_a=10% fee, a_c=0% fee):
        //   Forward: 1e18 * 2 * 0.9 * 5 = 9e18
        //   Reverse: 9e18 / 5 / 2 * 0.9 = 0.81e18
        //   buy_price = 9, sell_price = 9/0.81 = 100/9
        //   spread_A = |100/9 - 9| = 19/9 ≈ 2.11
        //
        // Path via B (eth_b=5% fee, b_c=0% fee):
        //   Forward: 1e18 * 3 * 0.95 * 2 = 5.7e18 = (57/10)e18
        //   Reverse: 5.7e18 / 2 / 3 * 0.95 = 0.9025e18 = (361/400)e18
        //   buy_price = 57/10, sell_price = (57/10)/(361/400) = 2280/361
        //   spread_B = |2280/361 - 57/10| = 2223/3610 ≈ 0.62
        //
        // spread_B < spread_A → Path via B selected.
        let eth = token(0, "ETH");
        let a = token(2, "A");
        let b = token(3, "B");
        let c = token(4, "C");

        let (market, derived) = setup_test_env(vec![
            (
                "eth_a",
                &eth,
                &a,
                MockProtocolSim::new(2.0)
                    .with_fee(0.1)
                    .with_gas(0),
            ),
            ("a_c", &a, &c, MockProtocolSim::new(5.0).with_gas(0)),
            (
                "eth_b",
                &eth,
                &b,
                MockProtocolSim::new(3.0)
                    .with_fee(0.05)
                    .with_gas(0),
            ),
            ("b_c", &b, &c, MockProtocolSim::new(2.0).with_gas(0)),
        ])
        .await;
        let changed = ChangedComponents::default();

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market, &derived, &changed)
            .await
            .unwrap()
            .data;

        assert_eq!(prices.len(), 4, "should have prices for ETH, A, B, C");

        // A: 1-hop from ETH with 10% fee
        // buy_out = 1e18 * 2 * 0.9 = 1.8e18 = (9/5)e18
        // sell_out = 1.8e18 / 2 * 0.9 = 0.81e18 = (81/100)e18
        // buy_price = 9/5, sell_price = (9/5)/(81/100) = 9*100/(5*81) = 20/9
        // mid_price = (9/5 + 20/9) / 2 = (81 + 100) / 90 = 181/90
        let a_price = prices
            .get(&a.address)
            .expect("A should have price");
        let a_ratio = a_price.numerator.to_f64().unwrap() / a_price.denominator.to_f64().unwrap();
        let expected_a = 181.0 / 90.0;
        assert!(
            (a_ratio - expected_a).abs() < 1e-10,
            "A mid_price should be 181/90 = {expected_a}, got {a_ratio}"
        );

        // B: 1-hop from ETH with 5% fee
        // buy_out = 1e18 * 3 * 0.95 = 2.85e18 = (57/20)e18
        // sell_out = 2.85e18 / 3 * 0.95 = 0.9025e18 = (361/400)e18
        // buy_price = 57/20, sell_price = (57/20)/(361/400) = 57*400/(20*361) = 1140/361
        // mid_price = (57/20 + 1140/361) / 2 = (57*361 + 1140*20) / (2*20*361)
        //           = (20577 + 22800) / 14440 = 43377/14440
        let b_price = prices
            .get(&b.address)
            .expect("B should have price");
        let b_ratio = b_price.numerator.to_f64().unwrap() / b_price.denominator.to_f64().unwrap();
        let expected_b = 43377.0 / 14440.0;
        assert!(
            (b_ratio - expected_b).abs() < 1e-10,
            "B mid_price should be 43377/14440 = {expected_b}, got {b_ratio}"
        );

        // C has two paths, so the median of two takes the upper middle — the one via A.
        // buy_out = 1e18 * 2 * 0.9 * 5 = 9e18, so buy_price = 9
        // sell_out = 9e18 / 5 / 2 * 0.95... = (81/100)e18, so sell_price = 9 / (81/100) = 100/9
        // mid_price = (9 + 100/9) / 2 = 181/18
        let c_price = prices
            .get(&c.address)
            .expect("C should have price");
        let c_ratio = c_price.numerator.to_f64().unwrap() / c_price.denominator.to_f64().unwrap();
        let expected_c = 181.0 / 18.0;
        assert!(
            (c_ratio - expected_c).abs() < 1e-10,
            "C mid_price should be 181/18 = {expected_c} (via A), got {c_ratio}"
        );
    }

    #[tokio::test]
    async fn test_compute_missing_spot_prices_returns_error() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Create market without spot prices set
        let (market, _) =
            setup_market_weighted(vec![("pool", &eth, &usdc, MockProtocolSim::new(2000.0))]);
        let derived = DerivedData::new_shared(); // No spot prices
        let changed = ChangedComponents::default();

        let computation = computation_for(&eth.address);
        let result = computation
            .compute(&market, &derived, &changed)
            .await;

        assert!(
            matches!(result, Err(ComputationError::MissingDependency("spot_prices"))),
            "should return MissingDependency for spot_prices"
        );
    }

    #[tokio::test]
    async fn test_compute_gas_token_with_no_pools_returns_only_self() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");
        let dai = token(2, "DAI");

        // Create a pool that doesn't include ETH (gas token)
        let (market, derived) =
            setup_test_env(vec![("usdc_dai", &usdc, &dai, MockProtocolSim::new(1.0))]).await;
        let changed = ChangedComponents::default();

        let computation = computation_for(&eth.address);
        let prices = computation
            .compute(&market, &derived, &changed)
            .await
            .unwrap()
            .data;

        // Only the gas token itself should have a price (1:1)
        assert_eq!(prices.len(), 1, "should only have gas token price");
        let eth_price = prices
            .get(&eth.address)
            .expect("ETH should have price");
        assert_eq!(
            eth_price.numerator, eth_price.denominator,
            "gas token must have exact 1:1 price"
        );
    }

    #[tokio::test]
    async fn test_path_components_includes_all_candidate_paths() {
        // Diamond topology: two paths to token_a
        //
        //   pool_direct: ETH → token_a  (fee-free, ratio=2, lower spread → selected)
        //   pool_indirect_1 + pool_indirect_2: ETH → token_b → token_a (higher spread)
        //
        // After full compute, token_a's path_components must include all three pool IDs
        // even though only pool_direct is on the best path.
        let eth = token(0, "ETH");
        let token_a = token(1, "A");
        let token_b = token(2, "B");

        let (market, derived) = setup_test_env(vec![
            ("pool_direct", &eth, &token_a, MockProtocolSim::new(2.0).with_gas(0)),
            (
                "pool_indirect_1",
                &eth,
                &token_b,
                MockProtocolSim::new(3.0)
                    .with_fee(0.1)
                    .with_gas(0),
            ),
            ("pool_indirect_2", &token_b, &token_a, MockProtocolSim::new(1.0).with_gas(0)),
        ])
        .await;
        let changed = ChangedComponents::default();

        let computation = computation_for(&eth.address);
        computation
            .compute(&market, &derived, &changed)
            .await
            .unwrap();

        // Inspect stored deps to verify path_components
        let store = derived.read().await;
        let deps = store
            .token_prices_deps()
            .expect("deps should be stored");
        let entry = deps
            .get(&token_a.address)
            .expect("token_a should have deps");

        assert!(
            entry
                .path_components
                .contains("pool_direct"),
            "path_components should contain pool_direct (best path)"
        );
        assert!(
            entry
                .path_components
                .contains("pool_indirect_1"),
            "path_components should contain pool_indirect_1 (competing path)"
        );
        assert!(
            entry
                .path_components
                .contains("pool_indirect_2"),
            "path_components should contain pool_indirect_2 (competing path)"
        );
    }

    #[tokio::test]
    async fn test_incremental_recompute_triggered_by_competing_path_pool() {
        // Same diamond topology as above.
        // After full compute, changing pool_indirect_1 (not on best path) must
        // put token_a in tokens_to_recompute because it's now in path_components.
        let eth = token(0, "ETH");
        let token_a = token(1, "A");
        let token_b = token(2, "B");

        let (market, derived) = setup_test_env(vec![
            ("pool_direct", &eth, &token_a, MockProtocolSim::new(2.0).with_gas(0)),
            (
                "pool_indirect_1",
                &eth,
                &token_b,
                MockProtocolSim::new(3.0)
                    .with_fee(0.1)
                    .with_gas(0),
            ),
            ("pool_indirect_2", &token_b, &token_a, MockProtocolSim::new(1.0).with_gas(0)),
        ])
        .await;

        // Full compute to store deps
        let full_changed = ChangedComponents::default();
        let computation = computation_for(&eth.address);
        computation
            .compute(&market, &derived, &full_changed)
            .await
            .unwrap();

        // Incremental change: only pool_indirect_1 updated
        let incremental_changed = ChangedComponents {
            added: HashMap::new(),
            removed: vec![],
            updated: vec!["pool_indirect_1".to_string()],
            is_full_recompute: false,
        };

        let store = derived.read().await;
        let deps = store
            .token_prices_deps()
            .expect("deps should be stored");
        let changed_ids = incremental_changed.all_changed_ids();

        let tokens_to_recompute: HashSet<Address> = deps
            .iter()
            .filter(|(_, entry)| {
                !entry
                    .path_components
                    .is_disjoint(&changed_ids)
            })
            .map(|(addr, _)| addr.clone())
            .collect();

        assert!(
            tokens_to_recompute.contains(&token_a.address),
            "token_a should be scheduled for recomputation when pool_indirect_1 changes"
        );
        assert!(
            tokens_to_recompute.contains(&token_b.address),
            "token_b should be scheduled for recomputation when pool_indirect_1 changes"
        );
    }

    #[tokio::test]
    async fn test_compute_missing_gas_price_returns_error() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        // Create market without gas price set
        let mut market_inner = MarketState::new();
        let comp = component("pool", &[eth.clone(), usdc.clone()]);
        market_inner.upsert_components(std::iter::once(comp));
        market_inner
            .update_states([("pool".to_string(), Box::new(MockProtocolSim::new(2000.0)) as _)]);
        market_inner.upsert_tokens([eth.clone(), usdc.clone()]);
        let market = MarketData::new(std::sync::Arc::new(tokio::sync::RwLock::new(market_inner)));

        // Compute spot prices
        let derived = DerivedData::new_shared();
        let changed = ChangedComponents {
            added: std::collections::HashMap::from([(
                "pool".to_string(),
                vec![eth.address.clone(), usdc.address.clone()],
            )]),
            removed: vec![],
            updated: vec![],
            is_full_recompute: true,
        };

        let spot_comp = SpotPriceComputation::new();
        let spot_output = spot_comp
            .compute(&market, &derived, &changed)
            .await
            .unwrap();
        let mut pool_depths = PoolDepths::new();
        pool_depths.insert(
            ("pool".to_string(), eth.address.clone(), usdc.address.clone()),
            BigUint::from(DEEP),
        );
        {
            let mut store_guard = derived.try_write().unwrap();
            store_guard.set_spot_prices(spot_output.data, vec![], 0, true);
            // Present so the missing gas price is the only dependency left to fail on.
            store_guard.set_pool_depths(pool_depths, vec![], 0, true);
        }

        let computation = computation_for(&eth.address);
        let result = computation
            .compute(&market, &derived, &changed)
            .await;

        assert!(
            matches!(result, Err(ComputationError::MissingDependency("gas_price"))),
            "should return MissingDependency for gas_price"
        );
    }

    #[tokio::test]
    async fn test_all_paths_fail_reported() {
        // gas_units = 1e16, gas_price = 100 (set by setup_market_weighted)
        // sell_gas_cost = 1e16 * 100 = 1e18 = sell_out (1e18 ETH) → path not viable
        // → all paths for USDC fail → USDC lands in failed_items
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let (market, derived) = setup_test_env(vec![(
            "eth_usdc",
            &eth,
            &usdc,
            MockProtocolSim::new(2000.0).with_gas(10_000_000_000_000_000u64), // 1e16 gas units
        )])
        .await;
        let changed = ChangedComponents::default();

        let computation = computation_for(&eth.address);
        let output = computation
            .compute(&market, &derived, &changed)
            .await
            .unwrap();

        // USDC has a discovered path but all simulations fail
        assert!(
            output
                .failed_items
                .iter()
                .any(|item| item.key == usdc.address.to_string()),
            "USDC should appear in failed_items when all simulation paths fail"
        );
        // Gas token always has a 1:1 price
        assert!(output.data.contains_key(&eth.address), "gas token should always have price");
        // USDC should not have a price
        assert!(
            !output.data.contains_key(&usdc.address),
            "USDC should not have a price when all simulation paths fail"
        );
    }
}

//! Computes the `mid_price` of tokens relative to a gas token (e.g., ETH) by solving a route to the
//! token and back, with the same router that answers quotes.
//!
//! # Algorithm
//!
//! For each token in the graph:
//!
//! One relaxation from the gas token yields the best route to every token it reaches, and a token's
//! price is what its route returns over what it was given: `buy_out / simulation_amount`.
//!
//! The router runs with gas-aware scoring off. Off is what keeps this non-circular: gas-aware
//! scoring converts a route's gas into output-token terms, which needs the prices this computation
//! produces. Nothing here reads derived data, so token prices depend on no other computation.
//!
//! A router already ranks paths by what they return, so pricing needs no ranking rule of its own.
//!
//! # Limitation: the price is what you pay to buy, not what you would realise selling
//!
//! Selling back is not solved. Those solves are many sources into one destination, each starting
//! from its own buy output, and slippage makes the relaxation amount-dependent, so they cannot
//! share the single pass the buy side gets — one per token does not fit in a block. So the price
//! omits what leaving the token would cost: about a fee for an ordinary token, more for one with a
//! transfer tax or a thin exit.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use num_bigint::BigUint;
#[cfg(test)]
use num_traits::ToPrimitive;
use num_traits::Zero;
use tracing::{debug, instrument, Span};
use tycho_simulation::{
    tycho_common::models::Address, tycho_core::simulation::protocol_sim::Price,
};

use crate::{
    algorithm::{bellman_ford::FindRouteOptions, Algorithm, AlgorithmConfig, BellmanFordAlgorithm},
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
    types::{ComponentId, Order, OrderSide, Route, Swap},
};

/// The graph the router walks.
type RouterGraph = <BellmanFordAlgorithm as Algorithm>::GraphType;

/// What a route delivers in `token_out`, summed over the swaps that end there so a split route
/// reports its whole output rather than one leg's.
fn route_output(route: &Route, token_out: &Address) -> BigUint {
    route
        .swaps()
        .iter()
        .filter(|swap| swap.token_out() == token_out)
        .map(Swap::amount_out)
        .sum()
}

/// Computes token prices relative to the gas token by solving a buy and a sell for each token.
#[derive(Debug, Clone)]
pub struct TokenGasPriceComputation {
    /// The gas token address (e.g., ETH).
    gas_token: Address,
    /// Longest path the router may use.
    max_hops: usize,
    /// Amount of gas token to buy with (affects slippage).
    simulation_amount: BigUint,
}

impl Default for TokenGasPriceComputation {
    fn default() -> Self {
        Self {
            gas_token: Address::zero(20), // ETH address
            max_hops: 3,
            simulation_amount: BigUint::from(10u64).pow(18), // 1 ETH
        }
    }
}

impl TokenGasPriceComputation {
    #[cfg(test)]
    pub fn new(gas_token: Address, max_hops: usize, simulation_amount: BigUint) -> Self {
        Self { gas_token, max_hops, simulation_amount }
    }

    /// Sets the longest path the router may use.
    pub fn with_max_hops(self, max_hops: usize) -> Self {
        Self { max_hops, ..self }
    }

    /// Sets the gas token address.
    pub fn with_gas_token(self, gas_token: Address) -> Self {
        Self { gas_token, ..self }
    }

    /// Solves a buy and a sell for every token, or for `filter_tokens` when given.
    ///
    /// Returns the priced tokens, the block the market was read at, and one failed item per token
    /// that could not be solved in both directions.
    #[allow(clippy::type_complexity)]
    async fn solve_token_prices(
        &self,
        market: &MarketData,
        filter_tokens: Option<&HashSet<Address>>,
    ) -> Result<
        (HashMap<Address, (Price, HashSet<ComponentId>)>, u64, Vec<FailedItem>),
        ComputationError,
    > {
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
        let config =
            AlgorithmConfig::new(1, self.max_hops, AlgorithmConfig::default().timeout(), None)
                .map_err(|error| ComputationError::InvalidConfiguration(error.to_string()))?
                .with_gas_aware(false);
        let router = BellmanFordAlgorithm::with_config(config);

        let wanted = self.tokens_to_price(&topology, filter_tokens);
        let graph = graph_manager.graph();

        // One relaxation from the gas token yields the buy route for every token it reaches, so the
        // buy side costs a single pass however many tokens are priced.
        let buys = self
            .buy_routes(&router, graph, market, &wanted)
            .await?;

        let mut prices = HashMap::new();
        let mut failed_items = Vec::new();
        for token in wanted {
            match self.price_from_buy(&token, buys.get(&token)) {
                Some(priced) => {
                    prices.insert(token, priced);
                }
                None => failed_items.push(FailedItem {
                    key: token.to_string(),
                    error: FailedItemError::AllSimulationPathsFailed,
                }),
            }
        }

        Ok((prices, block, failed_items))
    }

    /// The buy route to every token the gas token reaches, from one relaxation.
    ///
    /// The context is built from the gas token, so its subgraph is everything within `max_hops` of
    /// it. `probe_destination` only satisfies the order's shape — the returned routes cover every
    /// destination, not that one.
    async fn buy_routes(
        &self,
        router: &BellmanFordAlgorithm,
        graph: &RouterGraph,
        market: &MarketData,
        wanted: &HashSet<Address>,
    ) -> Result<HashMap<Address, Route>, ComputationError> {
        let Some(probe_destination) = wanted.iter().next() else {
            return Ok(HashMap::new());
        };
        let order = Order::new(
            self.gas_token.clone(),
            probe_destination.clone(),
            self.simulation_amount.clone(),
            OrderSide::Sell,
            Address::zero(20),
        );
        let Ok(ctx) = router
            .build_context(graph, market.clone(), None, None, &order)
            .await
        else {
            // No subgraph around the gas token means nothing is priceable this block.
            return Ok(HashMap::new());
        };
        Ok(router.find_routes_from_source(&ctx, &order, FindRouteOptions::default()))
    }

    /// Every token in the graph but the gas token, narrowed to `filter_tokens` when given.
    fn tokens_to_price(
        &self,
        topology: &HashMap<ComponentId, Vec<Address>>,
        filter_tokens: Option<&HashSet<Address>>,
    ) -> HashSet<Address> {
        topology
            .values()
            .flatten()
            .filter(|token| *token != &self.gas_token)
            .filter(|token| filter_tokens.is_none_or(|wanted| wanted.contains(*token)))
            .cloned()
            .collect()
    }

    /// The price a buy route implies: what it returned over what it was given.
    fn price_from_buy(
        &self,
        token: &Address,
        buy: Option<&Route>,
    ) -> Option<(Price, HashSet<ComponentId>)> {
        let buy = buy?;
        let buy_out = route_output(buy, token);
        if buy_out.is_zero() {
            return None;
        }
        let price = Price { numerator: buy_out, denominator: self.simulation_amount.clone() };
        let components = buy
            .swaps()
            .iter()
            .map(|swap| swap.component_id().to_string())
            .collect();
        Some((price, components))
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

        let (solved, block, _) = self
            .solve_token_prices(market, Some(&tokens_to_recompute))
            .await?;

        let mut result = existing_prices;
        let mut new_deps = existing_deps;
        let mut failed_items: Vec<FailedItem> = Vec::new();

        for token in &tokens_to_recompute {
            if let Some((price, components)) = solved.get(token) {
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
        // The router runs gas-blind and reads no derived data, so nothing has to precede this.
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

        let (solved, block, failed_items) = self
            .solve_token_prices(market, None)
            .await?;

        let mut token_prices_with_deps = TokenPricesWithDeps::new();
        let mut token_prices = TokenGasPrices::new();
        for (token, (price, path_components)) in solved {
            token_prices_with_deps
                .insert(token.clone(), TokenPriceEntry { price: price.clone(), path_components });
            token_prices.insert(token, price);
        }

        // The gas token is 1:1 with itself and needs no route.
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
        algorithm::test_utils::{setup_market_weighted, token, MockProtocolSim},
        derived::store::DerivedData,
    };

    const SIM_AMOUNT: u128 = 1_000_000_000_000_000_000;

    fn computation_for(gas_token: &Address) -> TokenGasPriceComputation {
        TokenGasPriceComputation::new(gas_token.clone(), 3, BigUint::from(SIM_AMOUNT))
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
    async fn test_prices_a_token_from_the_route_that_buys_it() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");

        let prices =
            prices_for(&eth, vec![("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0))]).await;

        // The gas token is 1:1 with itself, and a fee-free pool returns its rate exactly.
        let eth_price = prices
            .get(&eth.address)
            .expect("gas token should be priced");
        assert_eq!(eth_price.numerator, eth_price.denominator);
        assert!((ratio(&prices[&usdc.address]) - 2000.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_prices_a_token_the_router_reaches_only_through_a_hop() {
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

        // 1 ETH buys 2 MID buys 6 TARGET, and the reverse returns the ETH, so the mean is 6.
        assert!((ratio(&prices[&target.address]) - 6.0).abs() < 1e-6);
    }

    #[tokio::test]
    async fn test_leaves_a_token_unpriced_when_the_router_finds_no_route() {
        let eth = token(0, "ETH");
        let usdc = token(1, "USDC");
        let island = token(4, "ISLAND");
        let other = token(5, "OTHER");

        // ISLAND and OTHER trade only with each other, so no route reaches them from the gas token.
        let prices = prices_for(
            &eth,
            vec![
                ("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000.0)),
                ("island_other", &island, &other, MockProtocolSim::new(1.0)),
            ],
        )
        .await;

        assert!(prices.contains_key(&usdc.address));
        assert!(!prices.contains_key(&island.address), "an unreachable token has no price");
    }
}

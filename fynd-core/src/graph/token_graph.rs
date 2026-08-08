//! A market graph with one edge per token pair. The pools that trade the pair are the edge weight.
//!
//! [`super::petgraph`] holds the same market with one edge per pool. Use that one to walk or relax
//! pools. Use this one to find routes as sequences of tokens.

use std::ops::Deref;

use async_trait::async_trait;
use petgraph::{
    graph::{EdgeIndex, NodeIndex},
    stable_graph,
};
use rustc_hash::{FxHashMap, FxHashSet};
use tracing::{debug, trace};
use tycho_simulation::tycho_common::models::Address;

use super::{EdgeData, GraphError, GraphManager};
use crate::{
    feed::{
        events::{EventError, MarketEvent, MarketEventHandler},
        market_data::MarketDataView,
    },
    types::ComponentId,
};

/// The pools that trade one directed token pair.
#[derive(Debug, Clone)]
pub struct PairEdge<D> {
    /// In the order they were added, so a search reads them the same way on every run.
    pools: Vec<EdgeData<D>>,
}

impl<D> PairEdge<D> {
    /// The pools serving this pair.
    pub fn pools(&self) -> &[EdgeData<D>] {
        &self.pools
    }

    /// Adds a pool, or does nothing if this component already serves the pair.
    fn insert(&mut self, component_id: &ComponentId) {
        if self
            .pools
            .iter()
            .any(|pool| &pool.component_id == component_id)
        {
            return;
        }
        self.pools
            .push(EdgeData::new(component_id.clone()));
    }

    /// Drops a pool. Returns whether the pair has any left.
    fn remove(&mut self, component_id: &ComponentId) -> bool {
        self.pools
            .retain(|pool| &pool.component_id != component_id);
        !self.pools.is_empty()
    }

    /// The pool this component runs on this pair.
    #[cfg(test)]
    fn pool_mut(&mut self, component_id: &ComponentId) -> Option<&mut EdgeData<D>> {
        self.pools
            .iter_mut()
            .find(|pool| &pool.component_id == component_id)
    }
}

/// Tokens as nodes, one edge per directed token pair.
pub type TokenGraph<D> = stable_graph::StableDiGraph<Address, PairEdge<D>>;

/// A [`TokenGraph`] with a lookup from a token pair to its edge.
///
/// Derefs to the graph, so the usual `petgraph` reads work on it directly.
pub struct TopologyGraph<D> {
    /// Taken mutably to change edge weights, which does not change which tokens trade and so
    /// leaves `pair_index` correct. Adding or removing an edge must go through `add_component` or
    /// `remove_component`, which keep it up to date.
    graph: TokenGraph<D>,
    /// The node holding each token.
    tokens: FxHashMap<Address, NodeIndex>,
    /// The edge for each directed token pair. Only changes when an edge is added or removed, which
    /// only happens when a pair gains its first pool or loses its last.
    pair_index: FxHashMap<(NodeIndex, NodeIndex), EdgeIndex>,
}

impl<D> TopologyGraph<D> {
    /// The pools trading `from` for `to`, empty if the pair is not connected.
    pub fn pools_between(&self, from: NodeIndex, to: NodeIndex) -> &[EdgeData<D>] {
        self.pair_index
            .get(&(from, to))
            .and_then(|&edge| self.graph.edge_weight(edge))
            .map_or(&[], PairEdge::pools)
    }

    /// The node holding `token`, or `None` if the market has no such token.
    pub fn get_token_ix(&self, token: &Address) -> Option<NodeIndex> {
        self.tokens.get(token).copied()
    }

    /// The edge between two tokens, or `None` if they do not trade.
    fn get_edge_ix(&self, from: NodeIndex, to: NodeIndex) -> Option<EdgeIndex> {
        self.pair_index
            .get(&(from, to))
            .copied()
    }

    /// Records that `component_id` trades `from` for `to`, adding the pair's edge if it is the
    /// first pool to do so.
    fn add_component(&mut self, from: NodeIndex, to: NodeIndex, component_id: &ComponentId) {
        match self.get_edge_ix(from, to) {
            Some(edge) => {
                if let Some(pair) = self.graph.edge_weight_mut(edge) {
                    pair.insert(component_id);
                }
            }
            None => {
                let pair = PairEdge { pools: vec![EdgeData::new(component_id.clone())] };
                let edge = self.graph.add_edge(from, to, pair);
                self.pair_index.insert((from, to), edge);
            }
        }
    }

    /// Records that `component_id` no longer trades `from` for `to`, removing the pair's edge if
    /// it was the last pool doing so.
    fn remove_component(&mut self, from: NodeIndex, to: NodeIndex, component_id: &ComponentId) {
        let Some(edge) = self.get_edge_ix(from, to) else {
            return;
        };
        let still_traded = self
            .graph
            .edge_weight_mut(edge)
            .is_some_and(|pair| pair.remove(component_id));
        if !still_traded {
            self.graph.remove_edge(edge);
            self.pair_index.remove(&(from, to));
        }
    }

    /// Adds a token as a node and returns its index, or returns the index it already has.
    fn add_token(&mut self, address: Address) -> NodeIndex {
        if let Some(index) = self.get_token_ix(&address) {
            return index;
        }
        let index = self.graph.add_node(address.clone());
        self.tokens.insert(address, index);
        index
    }
}

impl<D> Deref for TopologyGraph<D> {
    type Target = TokenGraph<D>;

    fn deref(&self) -> &Self::Target {
        &self.graph
    }
}

impl<D> Default for TopologyGraph<D> {
    fn default() -> Self {
        Self {
            graph: TokenGraph::default(),
            tokens: FxHashMap::default(),
            pair_index: FxHashMap::default(),
        }
    }
}

/// Builds a [`TopologyGraph`] from the market and keeps it up to date as components come and go.
///
/// One per worker.
pub struct TopologyGraphManager<D: Clone> {
    graph: TopologyGraph<D>,
    /// The token pairs each component trades, so removing a component does not mean searching
    /// every edge for it.
    component_pairs: FxHashMap<ComponentId, Vec<(NodeIndex, NodeIndex)>>,
}

impl<D: Clone> TopologyGraphManager<D> {
    /// Creates an empty manager.
    pub fn new() -> Self {
        Self { graph: TopologyGraph::default(), component_pairs: FxHashMap::default() }
    }

    /// Adds an edge each way between every pair of the component's tokens.
    fn add_component_edges(&mut self, component_id: &ComponentId, nodes: &[NodeIndex]) {
        let pairs: Vec<(NodeIndex, NodeIndex)> = nodes
            .iter()
            .enumerate()
            .flat_map(|(i, &from)| {
                nodes
                    .iter()
                    .skip(i + 1)
                    .flat_map(move |&to| [(from, to), (to, from)])
            })
            .collect();

        for &(from, to) in &pairs {
            self.graph
                .add_component(from, to, component_id);
        }
        self.component_pairs
            .insert(component_id.clone(), pairs);
    }

    /// Adds components to the topology graph.
    ///
    /// # Errors
    ///
    /// [`GraphError::InvalidComponents`] naming the ones with fewer than two tokens. The rest were
    /// added.
    fn add_components(
        &mut self,
        components: &FxHashMap<ComponentId, Vec<Address>>,
    ) -> Result<(), GraphError> {
        let mut invalid = Vec::new();
        let mut skipped = 0usize;

        // Sorted, so nodes and edges land in the same order whatever the map's iteration order.
        let mut sorted: Vec<_> = components.iter().collect();
        sorted.sort_by_key(|(id, _)| *id);

        for (component_id, tokens) in sorted {
            if self
                .component_pairs
                .contains_key(component_id)
            {
                trace!(component_id = %component_id, "skipping already-tracked component");
                skipped += 1;
                continue;
            }
            if tokens.len() < 2 {
                invalid.push(component_id.clone());
                continue;
            }

            let mut sorted_tokens: Vec<&Address> = tokens.iter().collect();
            sorted_tokens.sort();
            let nodes: Vec<NodeIndex> = sorted_tokens
                .iter()
                .map(|token| self.graph.add_token((*token).clone()))
                .collect();
            self.add_component_edges(component_id, &nodes);
        }

        if skipped > 0 {
            debug!(skipped_duplicates = skipped, "skipped duplicate components during add");
        }
        if !invalid.is_empty() {
            return Err(GraphError::InvalidComponents(invalid));
        }
        Ok(())
    }

    /// Removes components.
    ///
    /// # Errors
    ///
    /// [`GraphError::ComponentsNotFound`] naming the ones the graph does not hold. The rest were
    /// removed.
    fn remove_components(&mut self, components: &[ComponentId]) -> Result<(), GraphError> {
        let mut missing = Vec::new();

        for component_id in components {
            let Some(pairs) = self
                .component_pairs
                .remove(component_id)
            else {
                missing.push(component_id.clone());
                continue;
            };

            for (from, to) in pairs {
                self.graph
                    .remove_component(from, to, component_id);
            }
        }

        if !missing.is_empty() {
            return Err(GraphError::ComponentsNotFound(missing));
        }
        Ok(())
    }

    /// Sets one pool's weight, for tests that need a weight without running the derived data.
    #[cfg(test)]
    pub(crate) fn set_pool_weight(
        &mut self,
        component_id: &ComponentId,
        token_in: &Address,
        token_out: &Address,
        data: D,
        bidirectional: bool,
    ) -> Result<(), GraphError> {
        let from = self
            .get_token_ix(token_in)
            .ok_or_else(|| GraphError::TokenNotFound(token_in.clone()))?;
        let to = self
            .get_token_ix(token_out)
            .ok_or_else(|| GraphError::TokenNotFound(token_out.clone()))?;

        let mut directions = vec![(from, to)];
        if bidirectional {
            directions.push((to, from));
        }

        let mut updated = false;
        for (source, target) in directions {
            let Some(edge) = self.graph.get_edge_ix(source, target) else {
                continue;
            };
            if let Some(pool) = self
                .graph
                .graph
                .edge_weight_mut(edge)
                .and_then(|pair| pair.pool_mut(component_id))
            {
                pool.data = Some(data.clone());
                updated = true;
            }
        }

        if !updated {
            return Err(GraphError::MissingComponentBetweenTokens(
                token_in.clone(),
                token_out.clone(),
                component_id.clone(),
            ));
        }
        Ok(())
    }
}

impl<D: Clone + super::EdgeWeightFromSimAndDerived> super::EdgeWeightUpdaterWithDerived
    for TopologyGraphManager<D>
{
    /// Recomputes every pool's weight from this block's simulation states and derived data.
    ///
    /// Returns how many pools ended up with a weight. A pool whose derived data is missing has its
    /// weight cleared, so no one reads last block's.
    fn update_edge_weights_with_derived(
        &mut self,
        market: MarketDataView<'_>,
        derived: &crate::derived::DerivedData,
    ) -> usize {
        let tokens = market.token_registry_ref();
        let mut updated = 0usize;

        for edge in self
            .graph
            .edge_indices()
            .collect::<Vec<_>>()
        {
            let Some((source, target)) = self.graph.edge_endpoints(edge) else {
                continue;
            };
            // Both borrow the token registry, not the graph, so the graph is free to be taken
            // mutably below without either being cloned.
            let (Some(token_in), Some(token_out)) =
                (tokens.get(&self.graph[source]), tokens.get(&self.graph[target]))
            else {
                continue;
            };

            let Some(pair) = self.graph.graph.edge_weight_mut(edge) else {
                continue;
            };
            for pool in &mut pair.pools {
                pool.data = market
                    .get_simulation_state(&pool.component_id)
                    .and_then(|state| {
                        D::from_sim_and_derived(
                            state,
                            &pool.component_id,
                            token_in,
                            token_out,
                            derived,
                        )
                    });
                if pool.data.is_some() {
                    updated += 1;
                }
            }
        }

        updated
    }
}

impl<D: Clone> Default for TopologyGraphManager<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: Clone + Send + Sync> GraphManager<TopologyGraph<D>> for TopologyGraphManager<D> {
    fn initialize_graph(&mut self, component_topology: &FxHashMap<ComponentId, Vec<Address>>) {
        self.graph = TopologyGraph::default();
        self.component_pairs.clear();

        // Sorted, so the same topology gives the same node indices in every process. Hash iteration
        // order is seeded per process and would otherwise vary run to run.
        let mut tokens: Vec<Address> = component_topology
            .values()
            .flat_map(|addresses| addresses.iter())
            .cloned()
            .collect::<FxHashSet<_>>()
            .into_iter()
            .collect();
        tokens.sort();

        for token in tokens {
            self.graph.add_token(token);
        }

        // The same path an incremental update takes, so a graph built here and a graph grown by
        // events end up identical. A component with fewer than two tokens forms no edge; there is
        // no caller to report that to at startup, so it is logged.
        if let Err(e) = self.add_components(component_topology) {
            debug!(error = %e, "components skipped while building the graph");
        }
    }

    fn graph(&self) -> &TopologyGraph<D> {
        &self.graph
    }
}

#[async_trait]
impl<D: Clone + Send> MarketEventHandler for TopologyGraphManager<D> {
    async fn handle_event(&mut self, event: &MarketEvent) -> Result<(), EventError> {
        match event {
            MarketEvent::MarketUpdated { added_components, removed_components, .. } => {
                let mut errors = Vec::new();
                if let Err(e) = self.add_components(added_components) {
                    errors.push(e);
                }
                if let Err(e) = self.remove_components(removed_components) {
                    errors.push(e);
                }
                match errors.len() {
                    0 => Ok(()),
                    _ => Err(EventError::GraphErrors(errors)),
                }
            }
        }
    }
}

/// Lets the manager be read as its graph, for callers that only want to look.
impl<D: Clone> Deref for TopologyGraphManager<D> {
    type Target = TopologyGraph<D>;

    fn deref(&self) -> &Self::Target {
        &self.graph
    }
}

#[cfg(test)]
mod tests {
    use rustc_hash::FxHashMap;

    use super::*;
    use crate::{
        algorithm::{
            most_liquid::DepthAndPrice,
            test_utils::fixtures::{addrs, linear_graph},
        },
        graph::{EdgeWeightUpdaterWithDerived, GraphManager},
    };

    fn addr(byte: u8) -> Address {
        Address::from([byte; 20])
    }

    /// A pair has one edge however many pools trade it. The edge appears with the first pool and
    /// goes with the last.
    ///
    /// Adding or removing a pool in between must leave both the graph and `pair_index` alone. The
    /// last step checks that removing the edge also drops its index entry, since a leftover entry
    /// would point at an edge that no longer exists.
    #[tokio::test]
    async fn test_pair_edge_lives_from_first_pool_to_last() {
        use crate::feed::events::{MarketEvent, MarketEventHandler};

        let (a, b) = (addr(0x0A), addr(0x0B));
        let mut manager = TopologyGraphManager::<()>::new();
        manager.initialize_graph(&FxHashMap::from_iter([(
            "first".to_string(),
            vec![a.clone(), b.clone()],
        )]));

        let (from, to) = (manager.get_token_ix(&a).unwrap(), manager.get_token_ix(&b).unwrap());
        assert_eq!(manager.graph().edge_count(), 2, "one edge each way");
        assert_eq!(
            manager
                .graph()
                .pools_between(from, to)
                .len(),
            1
        );

        let added = |id: &str| MarketEvent::MarketUpdated {
            added_components: FxHashMap::from_iter([(id.to_string(), vec![a.clone(), b.clone()])]),
            removed_components: vec![],
            updated_components: vec![],
        };
        let removed = |id: &str| MarketEvent::MarketUpdated {
            added_components: FxHashMap::default(),
            removed_components: vec![id.to_string()],
            updated_components: vec![],
        };

        // A second pool on the same pair rides on the edge that is already there.
        manager
            .handle_event(&added("second"))
            .await
            .unwrap();
        assert_eq!(manager.graph().edge_count(), 2, "a second pool is not a second edge");
        assert_eq!(
            manager
                .graph()
                .pools_between(from, to)
                .len(),
            2
        );

        // Taking it away again leaves the pair trading, so the edge stays.
        manager
            .handle_event(&removed("second"))
            .await
            .unwrap();
        assert_eq!(manager.graph().edge_count(), 2, "the pair still trades");
        assert_eq!(
            manager
                .graph()
                .pools_between(from, to)
                .len(),
            1
        );

        // Taking the last one disconnects the tokens.
        manager
            .handle_event(&removed("first"))
            .await
            .unwrap();
        assert_eq!(manager.graph().edge_count(), 0);
        assert!(manager
            .graph()
            .pools_between(from, to)
            .is_empty());

        // And the index must have let go with it, or a later pair would read a dead edge.
        manager
            .handle_event(&added("third"))
            .await
            .unwrap();
        assert_eq!(manager.graph().edge_count(), 2);
        assert_eq!(
            manager
                .graph()
                .pools_between(from, to)
                .len(),
            1
        );
        assert_eq!(manager.graph().pools_between(from, to)[0].component_id, "third");

        // A pool between tokens the graph has never seen brings its nodes with it.
        let (c, d) = (addr(0x0C), addr(0x0D));
        manager
            .handle_event(&MarketEvent::MarketUpdated {
                added_components: FxHashMap::from_iter([(
                    "fourth".to_string(),
                    vec![c.clone(), d.clone()],
                )]),
                removed_components: vec![],
                updated_components: vec![],
            })
            .await
            .unwrap();
        assert_eq!(manager.graph().node_count(), 4);
        assert_eq!(manager.graph().edge_count(), 4);
    }

    #[test]
    fn test_edge_weight_cleared_on_spot_price_miss() {
        // Regression: when spot price computation fails, stale edge weights must be cleared so the
        // component is excluded from path scoring rather than routed with an outdated price.
        use num_bigint::BigUint;
        use num_traits::One;
        use tycho_simulation::tycho_core::simulation::protocol_sim::Price;

        use crate::{
            algorithm::test_utils::{market_read, setup_market_weighted, token, MockProtocolSim},
            derived::{types::TokenGasPrices, DerivedData},
        };

        let token_a = token(0x01, "A");
        let token_b = token(0x02, "B");
        let (market, mut manager) = setup_market_weighted(vec![(
            "component1",
            &token_a,
            &token_b,
            MockProtocolSim::new(2.0),
        )]);

        assert!(
            manager
                .graph()
                .edge_indices()
                .all(|e| manager
                    .graph()
                    .edge_weight(e)
                    .unwrap()
                    .pools()
                    .iter()
                    .all(|pool| pool.data.is_some())),
            "edges should have weight data after setup"
        );

        let mut token_prices = TokenGasPrices::default();
        for addr in [&token_a.address, &token_b.address] {
            token_prices.insert(
                addr.clone(),
                Price { numerator: BigUint::one(), denominator: BigUint::one() },
            );
        }
        let mut derived = DerivedData::new();
        derived.set_spot_prices(Default::default(), vec![], 10, true);
        derived.set_component_depths(Default::default(), vec![], 10, true);
        derived.set_token_prices(token_prices, vec![], 10, true);

        manager.update_edge_weights_with_derived(market_read(&market), &derived);

        assert!(
            manager
                .graph()
                .edge_indices()
                .all(|e| manager
                    .graph()
                    .edge_weight(e)
                    .unwrap()
                    .pools()
                    .iter()
                    .all(|pool| pool.data.is_none())),
            "stale edge weights must be cleared when spot price is unavailable"
        );
    }

    /// A component needs two tokens to trade. One with fewer is named in the error, and the
    /// components that were fine are still added.
    #[tokio::test]
    async fn test_add_components_reports_the_ones_with_too_few_tokens() {
        let (a, b) = (addr(0x0A), addr(0x0B));
        let mut manager = TopologyGraphManager::<()>::new();
        manager.initialize_graph(&FxHashMap::default());

        let result = manager.add_components(&FxHashMap::from_iter([
            ("solo".to_string(), vec![a.clone()]),
            ("pair".to_string(), vec![a.clone(), b.clone()]),
        ]));

        match result {
            Err(GraphError::InvalidComponents(invalid)) => {
                assert_eq!(invalid, vec!["solo".to_string()]);
            }
            other => panic!("expected InvalidComponents, got {other:?}"),
        }
        assert_eq!(manager.graph().edge_count(), 2, "the valid pair was still added");
    }

    /// Removing a component the graph never held is reported, and the ones it did hold still go.
    #[tokio::test]
    async fn test_remove_components_reports_the_ones_it_does_not_hold() {
        let (a, b) = (addr(0x0A), addr(0x0B));
        let mut manager = TopologyGraphManager::<()>::new();
        manager.initialize_graph(&FxHashMap::from_iter([(
            "pair".to_string(),
            vec![a.clone(), b.clone()],
        )]));

        let result = manager.remove_components(&["pair".to_string(), "ghost".to_string()]);

        match result {
            Err(GraphError::ComponentsNotFound(missing)) => {
                assert_eq!(missing, vec!["ghost".to_string()]);
            }
            other => panic!("expected ComponentsNotFound, got {other:?}"),
        }
        assert_eq!(manager.graph().edge_count(), 0, "the component it did hold was removed");
    }

    /// A weight can only be set on a pair a component actually trades.
    #[test]
    fn test_set_pool_weight_rejects_a_pair_the_component_does_not_trade() {
        let (a, b, c, _) = addrs();
        let mut manager = linear_graph();

        let unknown_token = manager.set_pool_weight(
            &"ab".to_string(),
            &a,
            &addr(0x99),
            DepthAndPrice::new(1.0, 1.0),
            false,
        );
        assert!(matches!(unknown_token, Err(GraphError::TokenNotFound(_))));

        let wrong_pair =
            manager.set_pool_weight(&"ab".to_string(), &b, &c, DepthAndPrice::new(1.0, 1.0), false);
        assert!(matches!(wrong_pair, Err(GraphError::MissingComponentBetweenTokens(..))));
    }
}

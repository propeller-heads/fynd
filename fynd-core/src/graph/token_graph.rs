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
use smallvec::SmallVec;
use tracing::{debug, trace};
use tycho_simulation::tycho_common::models::Address;

use super::{
    EdgeData, GraphError, GraphManager, GraphQueryFilter, Path, INLINE_EDGES, INLINE_TOKENS,
};
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
    #[cfg(any(test, feature = "test-utils"))]
    fn pool_mut(&mut self, component_id: &ComponentId) -> Option<&mut EdgeData<D>> {
        self.pools
            .iter_mut()
            .find(|pool| &pool.component_id == component_id)
    }
}

/// A route as a sequence of tokens, before the pools serving each leg are chosen.
pub type TokenPath = SmallVec<[NodeIndex; INLINE_TOKENS]>;

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

    /// Every route between two tokens, as token sequences.
    ///
    /// See [`TopologyGraph::paths_between_ix`], which this resolves the addresses for.
    ///
    /// # Errors
    ///
    /// [`GraphError::TokenNotFound`] naming whichever of the two the market does not hold.
    pub fn paths_between(
        &self,
        from: &Address,
        to: &Address,
        filter: &GraphQueryFilter,
    ) -> Result<Vec<TokenPath>, GraphError> {
        let from_ix = self
            .get_token_ix(from)
            .ok_or_else(|| GraphError::TokenNotFound(from.clone()))?;
        let to_ix = self
            .get_token_ix(to)
            .ok_or_else(|| GraphError::TokenNotFound(to.clone()))?;
        Ok(self.paths_between_ix(from_ix, to_ix, filter))
    }

    /// Every token path from `from` to `to` within the filter's hop bounds.
    ///
    /// Empty when there is no route. Check [`TopologyGraph::expand_path`] for expanded pool paths.
    ///
    /// Routes come back shortest first. Within a length they are in no particular order.
    pub fn paths_between_ix(
        &self,
        from: NodeIndex,
        to: NodeIndex,
        filter: &GraphQueryFilter,
    ) -> Vec<TokenPath> {
        if filter.min_hops == 0 || filter.min_hops > filter.max_hops {
            return Vec::new();
        }
        if from == to {
            self.circular_token_paths(to, filter)
        } else {
            self.bidirectional_search(from, to, filter)
        }
    }

    /// Writes out one route per combination of pools along `token_path`.
    ///
    /// A token sequence stands for as many routes as the product of the pools on each of its legs.
    /// They are enumerated by counting: the rightmost leg advances first, and carries into the leg
    /// to its left when it wraps.
    ///
    /// That product is unbounded — four legs of twenty pools is 160,000 routes — so `max_paths`
    /// caps how many are written. The ones past the cap are dropped in counting order, which no
    /// ranking has seen yet, so a cap trades routes the caller might have wanted for a bound on
    /// what one sequence can allocate. `None` writes them all.
    pub fn expand_path(
        &self,
        token_path: &[NodeIndex],
        max_paths: Option<usize>,
    ) -> Vec<Path<'_, D>> {
        // Inline, like everything else in the search: this runs once per token sequence found, and
        // a route has at most `max_hops` legs.
        let legs: SmallVec<[&[EdgeData<D>]; INLINE_EDGES]> = token_path
            .windows(2)
            .map(|pair| self.pools_between(pair[0], pair[1]))
            .collect();

        // A sequence of one token names no leg, and a leg with no pool names a pair the graph
        // does not connect -- the walk disagreeing with the graph rather than a routing outcome.
        // Either way there is no route to write, and an empty product would otherwise write one
        // route with no hops.
        if legs.is_empty() || legs.iter().any(|leg| leg.is_empty()) {
            return Vec::new();
        }

        let combinations: usize = legs
            .iter()
            .map(|leg| leg.len())
            .product();
        let wanted = max_paths.map_or(combinations, |cap| cap.min(combinations));
        let mut chosen: SmallVec<[usize; INLINE_EDGES]> = SmallVec::from_elem(0, legs.len());

        let mut out = Vec::with_capacity(wanted);
        for _ in 0..wanted {
            let mut path = Path::new();
            for (leg, (pair, &pick)) in legs
                .iter()
                .zip(token_path.windows(2).zip(chosen.iter()))
            {
                let pool = leg
                    .get(pick)
                    .expect("odometer holds every leg inside its own pool count");
                path.add_hop(&self[pair[0]], pool, &self[pair[1]]);
            }
            out.push(path);

            for (pick, leg) in chosen.iter_mut().zip(legs.iter()).rev() {
                *pick += 1;
                if *pick < leg.len() {
                    break;
                }
                *pick = 0;
            }
        }

        out
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

    /// Every route from `from` to `to`, found by meeting in the middle.
    ///
    /// Each length between `min_hops` and `max_hops` is searched on its own. A route of `n` hops is
    /// split in two: a walk of `ceil(n/2)` hops out of `from` and a walk of `n - ceil(n/2)` hops
    /// out of `to`. The two are joined wherever they end on the same token, so the deepest
    /// level is reached by matching rather than by walking. With a branching factor of `b` that
    /// costs about `b^(n/2)` instead of `b^n`.
    ///
    /// Both halves are walked breadth-first by [`TopologyGraph::walk_tokens`]. The tail starts at
    /// the destination and walks outwards, so it comes back pointing the wrong way and is reversed
    /// before joining. Only the order needs fixing -- pools are chosen per leg during expansion,
    /// from the token pair itself, so no edge is ever carried in the wrong direction.
    ///
    /// Searching one length at a time keeps shorter routes ahead of longer ones in the result.
    ///
    /// Consecutive lengths ask for the same half-walks -- lengths 3 and 4 both take a 2-hop head,
    /// lengths 4 and 5 both take a 2-hop tail -- so each side is walked once to its deepest and
    /// every length reads the level it needs.
    fn bidirectional_search(
        &self,
        from: NodeIndex,
        to: NodeIndex,
        filter: &GraphQueryFilter,
    ) -> Vec<TokenPath> {
        let connector_tokens = filter.connector_tokens.as_ref();
        let endpoints = (from, to);
        let head_levels =
            self.walk_levels(from, filter.max_hops.div_ceil(2), endpoints, connector_tokens);
        let tail_levels = self.walk_levels(to, filter.max_hops / 2, endpoints, connector_tokens);

        // Grouping a tail level by its midpoint is worth doing once per depth, not once per length.
        let mut midpoint_index: Vec<Option<FxHashMap<NodeIndex, Vec<TokenPath>>>> =
            vec![None; tail_levels.len()];
        let mut token_paths = Vec::new();

        for length in filter.min_hops..=filter.max_hops {
            let head_hops = length.div_ceil(2);
            let tail_hops = length - head_hops;
            let (Some(heads), Some(tails)) =
                (head_levels.get(head_hops), tail_levels.get(tail_hops))
            else {
                continue;
            };
            if heads.is_empty() || tails.is_empty() {
                continue;
            }

            let tails_by_midpoint = midpoint_index[tail_hops].get_or_insert_with(|| {
                let mut index: FxHashMap<NodeIndex, Vec<TokenPath>> = FxHashMap::default();
                for tail in tails {
                    let Some(&midpoint) = tail.last() else {
                        continue;
                    };
                    index
                        .entry(midpoint)
                        .or_default()
                        .push(tail.iter().rev().copied().collect());
                }
                index
            });

            for head in heads {
                let Some(&midpoint) = head.last() else {
                    continue;
                };
                let Some(candidates) = tails_by_midpoint.get(&midpoint) else {
                    continue;
                };
                for tail in candidates {
                    // Each half avoids revisits on its own, but together they can name a token
                    // twice. The midpoint is the only one they are allowed to share.
                    let collides = head
                        .iter()
                        .any(|token| *token != midpoint && tail.contains(token));
                    if collides {
                        continue;
                    }

                    let mut joined = TokenPath::from_slice(head);
                    joined.extend_from_slice(&tail[1..]);
                    token_paths.push(joined);
                }
            }
        }

        token_paths
    }

    /// The token sequences leaving `start`, one level per hop count: index `k` holds every
    /// sequence of exactly `k` edges, up to `hops`. Level 0 is `start` on its own.
    ///
    /// Walking breadth-first produces every level on the way to the deepest, so they are all kept
    /// rather than the deepest alone.
    ///
    /// `endpoints` is the route's own `(from, to)`. The connector filter never applies to those
    /// two; every other token a walk passes through is an intermediate and must be allowed.
    fn walk_levels(
        &self,
        start: NodeIndex,
        hops: usize,
        endpoints: (NodeIndex, NodeIndex),
        connector_tokens: Option<&FxHashSet<Address>>,
    ) -> Vec<Vec<TokenPath>> {
        let (from, to) = endpoints;
        let mut levels = vec![vec![TokenPath::from_slice(&[start])]];

        for hop in 0..hops {
            let mut next = Vec::new();
            for sequence in &levels[hop] {
                let Some(&last) = sequence.last() else {
                    continue;
                };
                for neighbor in self.neighbors(last) {
                    if sequence.contains(&neighbor) {
                        continue;
                    }
                    if neighbor != from && neighbor != to {
                        if let Some(allowed) = connector_tokens {
                            if !allowed.contains(&self[neighbor]) {
                                continue;
                            }
                        }
                    }

                    let mut extended = TokenPath::from_slice(sequence);
                    extended.push(neighbor);
                    next.push(extended);
                }
            }
            levels.push(next);
        }

        levels
    }

    /// Token sequences that begin and end on the same token.
    ///
    /// Such a route closes a cycle, so it has no midpoint to split on -- both halves would have to
    /// start and end on that token, which the no-revisit rule forbids. Searched from the one end
    /// instead, with the closing hop exempt from that rule.
    fn circular_token_paths(&self, target: NodeIndex, filter: &GraphQueryFilter) -> Vec<TokenPath> {
        let mut token_paths = Vec::new();
        let mut frontier = vec![TokenPath::from_slice(&[target])];

        for hops in 1..=filter.max_hops {
            let mut next = Vec::new();
            for sequence in &frontier {
                let Some(&last) = sequence.last() else {
                    continue;
                };
                for neighbor in self.neighbors(last) {
                    if neighbor == target {
                        if hops >= filter.min_hops {
                            let mut closed = TokenPath::from_slice(sequence);
                            closed.push(neighbor);
                            token_paths.push(closed);
                        }
                        // A closed cycle is a finished route. Walking on from it would put the
                        // start token in the middle of a longer one, which is not a route the
                        // executor can take.
                        continue;
                    }
                    if sequence.contains(&neighbor) {
                        continue;
                    }
                    if let Some(allowed) = filter.connector_tokens.as_ref() {
                        if !allowed.contains(&self[neighbor]) {
                            continue;
                        }
                    }

                    let mut extended = TokenPath::from_slice(sequence);
                    extended.push(neighbor);
                    next.push(extended);
                }
            }
            frontier = next;
        }

        token_paths
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
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn set_pool_weight(
        &mut self,
        component_id: &ComponentId,
        token_in: &Address,
        token_out: &Address,
        data: D,
        bidirectional: bool,
    ) -> Result<(), GraphError> {
        let from = self
            .graph
            .get_token_ix(token_in)
            .ok_or_else(|| GraphError::TokenNotFound(token_in.clone()))?;
        let to = self
            .graph
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
                if errors.is_empty() {
                    Ok(())
                } else {
                    Err(EventError::GraphErrors(errors))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rustc_hash::{FxHashMap, FxHashSet};

    use super::*;
    use crate::{
        algorithm::{
            most_liquid::DepthAndPrice,
            test_utils::fixtures::{addrs, diamond_graph, linear_graph, parallel_graph},
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

        let (from, to) = (
            manager
                .graph()
                .get_token_ix(&a)
                .unwrap(),
            manager
                .graph()
                .get_token_ix(&b)
                .unwrap(),
        );
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

    /// Every route between two tokens, as one entry per combination of pools.
    ///
    /// The graph works in nodes; these cases were written in addresses, so this resolves them and
    /// runs both halves of what the algorithms do -- search, then expand.
    fn routes<'a>(
        graph: &'a TopologyGraph<DepthAndPrice>,
        from: &Address,
        to: &Address,
        min_hops: usize,
        max_hops: usize,
        connector_tokens: Option<FxHashSet<Address>>,
    ) -> Vec<Path<'a, DepthAndPrice>> {
        let (Some(from), Some(to)) = (graph.get_token_ix(from), graph.get_token_ix(to)) else {
            return Vec::new();
        };
        let filter = GraphQueryFilter { min_hops, max_hops, connector_tokens };
        graph
            .paths_between_ix(from, to, &filter)
            .iter()
            .flat_map(|token_path| graph.expand_path(token_path, None))
            .collect()
    }

    fn all_ids(paths: Vec<Path<'_, DepthAndPrice>>) -> FxHashSet<Vec<&str>> {
        paths
            .iter()
            .map(|p| {
                p.iter()
                    .map(|(_, e, _)| e.component_id.as_str())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn test_find_paths_linear_forward_and_reverse() {
        let (a, b, c, d) = addrs();
        let m = linear_graph();
        let g = m.graph();

        // Forward: A->B (1 hop), A->C (2 hops), A->D (3 hops)
        let p = routes(g, &a, &b, 1, 1, None);
        assert_eq!(all_ids(p), FxHashSet::from_iter([vec!["ab"]]));

        let p = routes(g, &a, &c, 1, 2, None);
        assert_eq!(all_ids(p), FxHashSet::from_iter([vec!["ab", "bc"]]));

        let p = routes(g, &a, &d, 1, 3, None);
        assert_eq!(all_ids(p), FxHashSet::from_iter([vec!["ab", "bc", "cd"]]));

        // Reverse: D->A (bidirectional components)
        let p = routes(g, &d, &a, 1, 3, None);
        assert_eq!(all_ids(p), FxHashSet::from_iter([vec!["cd", "bc", "ab"]]));
    }

    #[test]
    fn test_find_paths_respects_hop_bounds() {
        let (a, _, c, d) = addrs();
        let m = linear_graph();
        let g = m.graph();

        // A->D needs 3 hops, max_hops=2 finds nothing
        assert!(routes(g, &a, &d, 1, 2, None).is_empty());

        // A->C is 2 hops, min_hops=3 finds nothing
        assert!(routes(g, &a, &c, 3, 3, None).is_empty());
    }

    #[test]
    fn test_find_paths_parallel_components() {
        let (a, b, c, _) = addrs();
        let m = parallel_graph();
        let g = m.graph();

        // A->B: 3 parallel components = 3 paths
        let p = routes(g, &a, &b, 1, 1, None);
        assert_eq!(all_ids(p), FxHashSet::from_iter([vec!["ab1"], vec!["ab2"], vec!["ab3"]]));

        // A->C: 3 A->B components × 2 B->C components = 6 paths
        let p = routes(g, &a, &c, 1, 2, None);
        assert_eq!(
            all_ids(p),
            FxHashSet::from_iter([
                vec!["ab1", "bc1"],
                vec!["ab1", "bc2"],
                vec!["ab2", "bc1"],
                vec!["ab2", "bc2"],
                vec!["ab3", "bc1"],
                vec!["ab3", "bc2"],
            ])
        );
    }

    #[test]
    fn test_find_paths_diamond_multiple_routes() {
        let (a, _, _, d) = addrs();
        let m = diamond_graph();
        let g = m.graph();

        // A->D: two 2-hop paths
        let p = routes(g, &a, &d, 1, 2, None);
        assert_eq!(all_ids(p), FxHashSet::from_iter([vec!["ab", "bd"], vec!["ac", "cd"]]));
    }

    #[test]
    fn test_find_paths_no_intermediate_cycles() {
        let (a, b, _, _) = addrs();
        let m = linear_graph();
        let g = m.graph();

        // A->B with max_hops=3: only the direct 1-hop path is valid.
        // Revisit paths like A->B->C->B or A->B->B->B are pruned because
        // they create intermediate cycles unsupported by Tycho execution
        // (only first == last cycles are allowed, i.e. from == to).
        let p = routes(g, &a, &b, 1, 3, None);
        assert_eq!(all_ids(p), FxHashSet::from_iter([vec!["ab"]]));
    }

    #[test]
    fn test_find_paths_cyclic_same_source_dest() {
        let (a, _, _, _) = addrs();
        // Use parallel_graph with 3 A<->B components to verify all combinations
        let m = parallel_graph();
        let g = m.graph();

        // A->A (cyclic path) with 2 hops: should find all 9 combinations (3 components × 3
        // components) Note: min_hops=2 because cyclic paths require at least 2 hops
        let p = routes(g, &a, &a, 2, 2, None);
        assert_eq!(
            all_ids(p),
            FxHashSet::from_iter([
                vec!["ab1", "ab1"],
                vec!["ab1", "ab2"],
                vec!["ab1", "ab3"],
                vec!["ab2", "ab1"],
                vec!["ab2", "ab2"],
                vec!["ab2", "ab3"],
                vec!["ab3", "ab1"],
                vec!["ab3", "ab2"],
                vec!["ab3", "ab3"],
            ])
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

    /// Hop bounds that name no length return nothing rather than searching.
    #[rstest]
    #[case::zero_min(0, 3)]
    #[case::min_above_max(3, 1)]
    fn test_paths_between_rejects_impossible_hop_bounds(
        #[case] min_hops: usize,
        #[case] max_hops: usize,
    ) {
        let (a, b, _, _) = addrs();
        let m = linear_graph();
        let g = m.graph();
        let (from, to) = (g.get_token_ix(&a).unwrap(), g.get_token_ix(&b).unwrap());

        let filter = GraphQueryFilter { min_hops, max_hops, connector_tokens: None };

        assert!(g
            .paths_between_ix(from, to, &filter)
            .is_empty());
        assert!(
            g.paths_between_ix(from, from, &filter)
                .is_empty(),
            "the cyclic search is bound by the same rule"
        );
    }

    /// A closed cycle is a finished route, so the walk must not carry on from it. Carrying on
    /// produces routes that pass through the start token in the middle, which the executor cannot
    /// take: `A -> B -> A -> C -> A` visits A three times.
    #[test]
    fn test_cyclic_routes_never_pass_through_the_start_token() {
        let (a, _, _, _) = addrs();
        let m = diamond_graph();
        let g = m.graph();
        let start = g.get_token_ix(&a).unwrap();

        let cycles = g.paths_between_ix(
            start,
            start,
            &GraphQueryFilter { min_hops: 1, max_hops: 4, connector_tokens: None },
        );

        assert!(!cycles.is_empty(), "the diamond closes cycles through B and through C");
        for cycle in &cycles {
            assert_eq!(cycle.first(), Some(&start), "a cycle starts on its token");
            assert_eq!(cycle.last(), Some(&start), "a cycle ends on its token");
            assert!(
                !cycle[1..cycle.len() - 1].contains(&start),
                "the start token must appear only at the two ends, got {cycle:?}"
            );
        }
    }

    /// S and T, with parallel pools stacked on every leg of the long way round.
    ///
    /// ```text
    ///   S ==[sx1,sx2]== X ==[xy1,xy2,xy3]== Y ==[yt1,yt2]== T
    ///   S --[sy1]------------------------- Y
    ///   S --[st1]--------------------------------------------- T
    /// ```
    ///
    /// The other fixtures carry parallel pools on at most one leg, so they never multiply. Here the
    /// three-hop route is 2 x 3 x 2, which is what the expansion has to reproduce from a single
    /// token sequence.
    fn stacked_graph() -> TopologyGraphManager<DepthAndPrice> {
        let (s, x, y, t) = (addr(0x51), addr(0x58), addr(0x59), addr(0x54));
        let mut topology = FxHashMap::default();
        for (id, from, to) in [
            ("sx1", &s, &x),
            ("sx2", &s, &x),
            ("xy1", &x, &y),
            ("xy2", &x, &y),
            ("xy3", &x, &y),
            ("yt1", &y, &t),
            ("yt2", &y, &t),
            ("sy1", &s, &y),
            ("st1", &s, &t),
        ] {
            topology.insert(id.to_string(), vec![from.clone(), to.clone()]);
        }

        let mut manager = TopologyGraphManager::<DepthAndPrice>::new();
        manager.initialize_graph(&topology);
        manager
    }

    #[test]
    fn test_find_paths_expands_every_pool_combination() {
        let (s, x, y, t) = (addr(0x51), addr(0x58), addr(0x59), addr(0x54));
        let manager = stacked_graph();
        let graph = manager.graph();

        // 1 hop: st1.
        assert_eq!(routes(graph, &s, &t, 1, 1, None).len(), 1);
        // 2 hops: sy1 x {yt1, yt2}.
        assert_eq!(routes(graph, &s, &t, 2, 2, None).len(), 2);
        // 3 hops: {sx1, sx2} x {xy1, xy2, xy3} x {yt1, yt2}.
        assert_eq!(routes(graph, &s, &t, 3, 3, None).len(), 12);

        let paths = routes(graph, &s, &t, 1, 3, None);
        assert_eq!(paths.len(), 15, "every combination, and none of them twice");

        // The twelve long routes must name twelve distinct pool triples, not one triple twelve
        // times -- a counting slip in the expansion would still produce the right total.
        let long: FxHashSet<Vec<&str>> = paths
            .iter()
            .filter(|path| path.len() == 3)
            .map(|path| {
                path.edge_iter()
                    .iter()
                    .map(|edge| edge.component_id.as_str())
                    .collect()
            })
            .collect();
        assert_eq!(long.len(), 12);

        for path in &paths {
            assert_eq!(path.tokens.first().copied(), Some(&s));
            assert_eq!(path.tokens.last().copied(), Some(&t));
            assert_eq!(path.tokens.len(), path.len() + 1);
        }

        // Barring Y as an intermediate leaves only the direct pool.
        let allowed = FxHashSet::from_iter([x]);
        let filtered = routes(graph, &s, &t, 1, 3, Some(allowed.clone()));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].edge_iter()[0].component_id, "st1");

        let allowed = FxHashSet::from_iter([y]);
        assert_eq!(
            routes(graph, &s, &t, 1, 3, Some(allowed.clone())).len(),
            3,
            "st1, plus sy1 over each of the two Y-T pools"
        );
    }

    #[test]
    fn test_find_paths_bfs_ordering() {
        // Build a graph with 1-hop, 2-hop, and 3-hop paths to E:
        //   A --[ae]--> E                          (1-hop)
        //   A --[ab]--> B --[be]--> E              (2-hop)
        //   A --[ac]--> C --[cd]--> D --[de]--> E  (3-hop)
        let (a, b, c, d) = addrs();
        let e = addr(0x0E);
        let mut m = TopologyGraphManager::<DepthAndPrice>::new();
        let mut t = FxHashMap::default();
        t.insert("ae".into(), vec![a.clone(), e.clone()]);
        t.insert("ab".into(), vec![a.clone(), b.clone()]);
        t.insert("be".into(), vec![b, e.clone()]);
        t.insert("ac".into(), vec![a.clone(), c.clone()]);
        t.insert("cd".into(), vec![c, d.clone()]);
        t.insert("de".into(), vec![d, e.clone()]);
        m.initialize_graph(&t);
        let g = m.graph();

        let p = routes(g, &a, &e, 1, 3, None);

        // BFS guarantees paths are ordered by hop count
        assert_eq!(p.len(), 3, "Expected 3 paths total");
        assert_eq!(p[0].len(), 1, "First path should be 1-hop");
        assert_eq!(p[1].len(), 2, "Second path should be 2-hop");
        assert_eq!(p[2].len(), 3, "Third path should be 3-hop");
    }

    #[test]
    fn test_connector_tokens_blocks_disallowed_intermediate() {
        // Diamond: A->B->D, A->C->D. Only C in allowlist → only A->C->D survives.
        let (a, b, c, d) = addrs();
        let m = diamond_graph();
        let g = m.graph();
        let allowed: FxHashSet<Address> = FxHashSet::from_iter([c.clone()]);
        let paths = routes(g, &a, &d, 1, 2, Some(allowed.clone()));
        let intermediates: FxHashSet<&Address> = paths
            .iter()
            .flat_map(|p| p.iter().map(|(node, _, _)| node))
            .filter(|addr| *addr != &a && *addr != &d)
            .collect();
        // B must not appear; C must appear
        assert!(!intermediates.contains(&b), "B should be blocked");
        assert!(intermediates.contains(&c), "C should be allowed");
    }

    #[test]
    fn test_connector_tokens_allows_endpoints_even_if_not_listed() {
        // Allowlist contains neither token_in nor token_out, but a 1-hop route should still work.
        let (a, b, _, _) = addrs();
        let m = linear_graph();
        let g = m.graph();
        // Empty allowlist, so no token may serve as an intermediate. A 1-hop route reaches the
        // destination directly and has none.
        let allowed: FxHashSet<Address> = FxHashSet::default();
        let paths = routes(g, &a, &b, 1, 1, Some(allowed.clone()));
        assert!(!paths.is_empty(), "1-hop direct route should survive empty allowlist");
    }

    #[test]
    fn test_connector_tokens_none_is_unrestricted() {
        // None allowlist → both paths in diamond graph returned
        let (a, b, c, d) = addrs();
        let m = diamond_graph();
        let g = m.graph();
        let paths = routes(g, &a, &d, 1, 2, None);
        let intermediates: FxHashSet<&Address> = paths
            .iter()
            .flat_map(|p| p.iter().map(|(node, _, _)| node))
            .filter(|addr| *addr != &a && *addr != &d)
            .collect();
        assert!(intermediates.contains(&b), "B should appear with no restriction");
        assert!(intermediates.contains(&c), "C should appear with no restriction");
    }
}

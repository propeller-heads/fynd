use std::collections::HashMap;

use num_bigint::BigUint;

/// Market graph operation error types
#[derive(Debug)]
pub enum MarketGraphError {
    Config(String),
    InvalidInput(String),
    GraphOperation(String),
    DataInconsistency(String),
    External(String),
}

impl std::fmt::Display for MarketGraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "Configuration error: {}", msg),
            Self::InvalidInput(msg) => write!(f, "Invalid input: {}", msg),
            Self::GraphOperation(msg) => write!(f, "Graph operation failed: {}", msg),
            Self::DataInconsistency(msg) => write!(f, "Data inconsistency: {}", msg),
            Self::External(msg) => write!(f, "External service error: {}", msg),
        }
    }
}

impl std::error::Error for MarketGraphError {}

use petgraph::{
    graph::UnGraph,
    stable_graph::{EdgeIndex, NodeIndex},
};
use tycho_simulation::{
    tycho_common::{
        models::{protocol::ProtocolComponent, token::Token},
        simulation::protocol_sim::ProtocolSim,
    },
    tycho_core::Bytes,
};

use crate::models::Route;

#[derive(Clone)]
struct Path {
    start: NodeIndex,
    edges: Vec<EdgeIndex>,
    end: NodeIndex,
    price: BigUint,
}

impl Path {
    pub fn id(&self) -> String {
        // come up with a unique id for the path
        todo!()
    }
}

#[derive(Clone)]
pub struct MarketGraph {
    max_hops: usize,
    /// The underlying graph data structure. Token addresses as vertices, component ids as edges
    graph: UnGraph<Bytes, Bytes>,
    /// A map of token addresses to Node Index and Token structs
    tokens: HashMap<Bytes, (NodeIndex, Token)>,
    /// A map of state ids to ProtocolSim structs and corresponding token addresses
    states: HashMap<Bytes, Box<dyn ProtocolSim>>,
    /// A map of state ids to ProtocolComponent structs (needed for Swap creation)
    components: HashMap<Bytes, ProtocolComponent>,
    /// A cache map of all paths between two tokens (with a maximum of max_hops). String is the
    /// path id
    paths_cache: HashMap<String, Path>,
    /// A map of state ids to Path ids that the state is a member of
    paths_memberships: HashMap<Bytes, Vec<String>>,
}

impl MarketGraph {
    pub fn new(max_hops: usize) -> Self {
        MarketGraph {
            max_hops,
            tokens: HashMap::new(),
            states: HashMap::new(),
            components: HashMap::new(),
            graph: UnGraph::new_undirected(),
            paths_cache: HashMap::new(),
            paths_memberships: HashMap::new(),
        }
    }
    pub fn insert(
        &mut self,
        state_id: Bytes,
        component: ProtocolComponent,
        state: Box<dyn ProtocolSim>,
    ) {
        // Store both component and state for Swap creation
        self.components
            .insert(state_id.clone(), component.clone());
        self.states
            .insert(state_id.clone(), state);

        // TODO: Implement graph building logic
        // loop through component.tokens
        //   check if token is already a vertex -> if not:
        //     add it to self.tokens
        //     add it to self.graph.add_node(token.address)

        // for every token pair in tokens:
        // add state as an edge
        // retrieve the node index of the tokens
        // self.graph.add_edge(token0_node_index, token1_node_index, state.id)

        // loop through paths_cache to see if there is already an entry for the tokens
        //   if there is, call self.build_paths(token0, token1) and replace it
        //   if there isn't -> do nothing
        // NOTE: this assumes that when a new pool is added we won't be able to consider it in
        // already cached paths as a middle hop. Is this good enough? if this isn't enough,
        // we could rebuild the paths for all the entries already present in the paths_cache

        println!("Inserting into MarketGraph: component {} stored", component.id);
    }
    pub fn delete(&mut self, state_id: Bytes, _protocol_component: ProtocolComponent) {
        // Remove both component and state
        self.components.remove(&state_id);
        self.states.remove(&state_id);

        // TODO: Implement graph removal logic
        // loop through all token combinations
        // for each token get the corresponding edge with self.tokens
        // use self.graph.edges_connecting(t0, t1) and filter by the state id to find the edge to
        // remove self.graph.remove_edge(edge)
        // use self.paths_memberships to get the path ids that need to be deleted
        // possibly delete the vertices too if they are dangling now

        println!("Deleting from MarketGraph: state {:?} removed", state_id);
    }
    pub fn update(&mut self, state_id: Bytes, new_state: Box<dyn ProtocolSim>) {
        // Update the state, keep component unchanged (component doesn't change in updates)
        self.states
            .insert(state_id.clone(), new_state);

        // TODO: Update path prices in cache
        // loop through self.paths_memberships and update the price on the existing paths
        println!("Updating MarketGraph: state {:?} updated", state_id);
    }
    pub fn build_paths(&mut self, _token_in: Bytes, _token_out: Bytes) {
        // TODO: Implement proper path finding algorithm
        // search for all paths between the two tokens with self.max_hops
        // store the paths in self.paths_cache (use self.graph.edge_weight(*edge_idx) get the pool
        // ids) compute the path price using the ProtocolSim.spot_prices()
        // look at the basic petgraph path finding algorithms
        println!("Building MarketGraph");
    }
    pub fn get_routes_between_two_tokens(&self, _token_in: Bytes, _token_out: Bytes) -> Vec<Route> {
        println!("Getting routes from MarketGraph between two tokens");
        // Use self.tokens to get the node indexes of the tokens
        // loop through self.paths_cache and select only the paths that start with token_in and end
        // with token_out   if there are no paths, call self.build_paths(token_in,
        // token_out) and then get the paths

        // For each path, convert to Route using Swap objects:
        // For each edge in path.edges:
        //   let state_id = self.graph.edge_weight(edge);
        //   let component = self.components.get(state_id).unwrap();
        //   let protocol_state = self.states.get(state_id).unwrap();
        //   Create Swap {
        //       component: component.clone(),
        //       token_in: edge_token_in.address,
        //       token_out: edge_token_out.address,
        //       split: 1.0,
        //       user_data: None,
        //       protocol_state: Some(protocol_state.clone().into()),
        //       estimated_amount_in: None,
        //   }

        todo!("Implement route building with proper Swap creation using self.components and self.states")
    }
}

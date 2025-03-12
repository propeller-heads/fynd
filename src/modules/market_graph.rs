use crate::models::Route;
use petgraph::graph::UnGraph;
use petgraph::stable_graph::{EdgeIndex, NodeIndex};
use std::collections::HashMap;
use tycho_simulation::models::Token;
use tycho_simulation::protocol::state::ProtocolSim;
use tycho_simulation::tycho_core::Bytes;

#[derive(Clone)]
struct Path {
    start: NodeIndex,
    edges: Vec<EdgeIndex>,
    end: NodeIndex,
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
    /// The underlying graph data structure. Token addresses as vertices state ids as edges
    graph: UnGraph<Bytes, Bytes>,
    /// A map of token addresses to Token structs
    tokens: HashMap<Bytes, (NodeIndex, Token)>,
    /// A map of state ids to ProtocolSim structs and corresponding token addresses
    states: HashMap<Bytes, (Box<dyn ProtocolSim>, (Bytes, Bytes))>,
    /// A cache map of all paths between two tokens (with a maximum of max_hops). String is the path id
    paths_cache: HashMap<String, Path>,
    /// A map of state ids to Path ids that the state is a member of
    paths_memberships: HashMap<Bytes, Vec<String>>,
    // cyclical: bool, // TODO: this would be for the searcher use case where we would only need to search for cycles
}

impl MarketGraph {
    pub fn new(max_hops: usize) -> Self {
        MarketGraph {
            max_hops,
            tokens: HashMap::new(),
            states: HashMap::new(),
            graph: UnGraph::new_undirected(),
            paths_cache: HashMap::new(),
            paths_memberships: HashMap::new(),
        }
    }
    pub fn insert(&self, tokens: Vec<Token>, state_id: Bytes, state: Box<dyn ProtocolSim>) {
        // loop through tokens
        //   check if token is already a vertice -> if not:
        //     add it to self.tokens
        //     add it to self.graph.add_node(token.address)

        // if state is a EVMPoolState, we need to set spot prices
        // add state as an edge
        // retrieve the node index of the tokens
        // self.graph.add_edge(token0_node_index, token1_node_index, state.id)

        // add state to self.states

        // loop through paths_cache to see if there is already an entry for the tokens
        //   if there is, call self.build_paths(token0, token1) and replace it
        //   if there isn't -> do nothing
        // NOTE: this assumes that when a new pool is added we won't be able to consider it in already cached paths as a middle hop. Is this good enough?
        // if this isn't enough, we could rebuild the paths for all the entries already present in the paths_cache

        println!("Inserting into MarketGraph");
    }
    pub fn delete(&self, state_id: Bytes) {
        // remove the edge from the graph
        // use self.states to get the tokens
        // use self.tokens to get the node indexes of the tokens
        // use self.graph.edges_connecting(t0, t1) and filter by the state id to find the edge to remove
        // self.graph.remove_edge(edge)
        // remove the state from self.states
        // use self.paths_memberships to get the path ids that need to be deleted
        println!("Deleting from MarketGraph");
    }
    pub fn update(&self, state_id: Bytes, new_state: Box<dyn ProtocolSim>) {
        // get state from self.states and update it with the new state
        println!("Updating MarketGraph");
    }
    pub fn build_paths(&self, token_in: Bytes, token_out: Bytes) {
        // search for all paths between the two tokens
        // store the paths in self.paths_cache (use self.graph.edge_weight(*edge_idx) get the pool ids

        println!("Building MarketGraph");
    }
    pub fn get_routes(&self, token_in: Bytes, token_out: Bytes) -> Vec<Route> {
        println!("Getting routes from MarketGraph between two tokens");
        // Use self.tokens to get the node indexes of the tokens
        // loop through self.paths_cache and select only the paths that start with token_in and end with token_out
        //   if there are paths, for each path:
        //     build the Route using self.tokens and self.states
        //   if there are no paths, call self.build_paths(token_in, token_out) and then get the routes
        todo!()
    }
}

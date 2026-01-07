//! Lightweight route graph for path finding.
//!
//! This graph stores only the topology (tokens and pool connections),
//! not the actual pool states. It's designed to be cloned cheaply
//! by solvers who may want to prune or optimize their local copy.

use std::collections::{HashMap, HashSet, VecDeque};

use alloy::primitives::Address;

use crate::types::{PoolId, ProtocolSystem};

/// A lightweight, clonable graph representing the market topology.
///
/// This graph stores only:
/// - Token connections (which tokens can be swapped)
/// - Pool identifiers (which pools connect them)
/// - Protocol types (for gas estimation)
///
/// It does NOT store pool states (reserves, etc.) - those are in SharedMarketData.
#[derive(Debug, Clone, Default)]
pub struct RouteGraph {
    /// Adjacency list: token -> list of edges (outgoing connections)
    adjacency: HashMap<Address, Vec<Edge>>,
    /// Reverse mapping: pool_id -> tokens it connects
    pool_tokens: HashMap<PoolId, Vec<Address>>,
    /// All tokens in the graph
    tokens: HashSet<Address>,
}

/// An edge in the route graph representing a possible swap.
#[derive(Debug, Clone)]
pub struct Edge {
    /// The pool that enables this swap.
    pub pool_id: PoolId,
    /// The output token of this swap.
    pub token_out: Address,
    /// The protocol system (for gas estimation).
    pub protocol_system: ProtocolSystem,
}

/// A path through the graph (sequence of edges).
#[derive(Debug, Clone)]
pub struct Path {
    /// The edges in this path, in order.
    pub edges: Vec<Edge>,
    /// The tokens in this path, including start and end.
    pub tokens: Vec<Address>,
}

impl Path {
    /// Returns the number of hops (swaps) in this path.
    pub fn hop_count(&self) -> usize {
        self.edges.len()
    }

    /// Returns the starting token.
    pub fn start_token(&self) -> Option<Address> {
        self.tokens.first().copied()
    }

    /// Returns the ending token.
    pub fn end_token(&self) -> Option<Address> {
        self.tokens.last().copied()
    }
}

impl RouteGraph {
    /// Creates a new empty route graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of tokens in the graph.
    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Returns the number of pools in the graph.
    pub fn pool_count(&self) -> usize {
        self.pool_tokens.len()
    }

    /// Adds a pool to the graph.
    ///
    /// This creates edges between all token pairs in the pool.
    pub fn add_pool(&mut self, pool_id: PoolId, tokens: &[Address], protocol_system: ProtocolSystem) {
        // Store the pool -> tokens mapping
        self.pool_tokens
            .insert(pool_id.clone(), tokens.to_vec());

        // Add all tokens
        for token in tokens {
            self.tokens.insert(*token);
        }

        // Create edges for all token pairs (bidirectional for most pools)
        for (i, &token_in) in tokens.iter().enumerate() {
            for (j, &token_out) in tokens.iter().enumerate() {
                if i != j {
                    let edge = Edge {
                        pool_id: pool_id.clone(),
                        token_out,
                        protocol_system,
                    };
                    self.adjacency.entry(token_in).or_default().push(edge);
                }
            }
        }
    }

    /// Removes a pool from the graph.
    pub fn remove_pool(&mut self, pool_id: &PoolId) {
        // Get the tokens this pool connects
        let Some(tokens) = self.pool_tokens.remove(pool_id) else {
            return;
        };

        // Remove edges for this pool
        for token in &tokens {
            if let Some(edges) = self.adjacency.get_mut(token) {
                edges.retain(|e| &e.pool_id != pool_id);
            }
        }

        // Cleanup: remove tokens that have no remaining edges
        for token in tokens {
            if let Some(edges) = self.adjacency.get(&token) {
                if edges.is_empty() {
                    self.adjacency.remove(&token);
                    // Only remove from tokens set if no incoming edges either
                    let has_incoming = self
                        .adjacency
                        .values()
                        .any(|edges| edges.iter().any(|e| e.token_out == token));
                    if !has_incoming {
                        self.tokens.remove(&token);
                    }
                }
            }
        }
    }

    /// Returns the neighbors (outgoing edges) for a token.
    pub fn neighbors(&self, token: &Address) -> &[Edge] {
        self.adjacency.get(token).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Returns true if the token exists in the graph.
    pub fn contains_token(&self, token: &Address) -> bool {
        self.tokens.contains(token)
    }

    /// Returns true if the pool exists in the graph.
    pub fn contains_pool(&self, pool_id: &PoolId) -> bool {
        self.pool_tokens.contains_key(pool_id)
    }

    /// Finds all paths between two tokens up to max_hops.
    ///
    /// Uses BFS to find paths in order of increasing hop count.
    /// Returns paths sorted by hop count (shortest first).
    pub fn find_paths(&self, from: &Address, to: &Address, max_hops: usize) -> Vec<Path> {
        if !self.contains_token(from) || !self.contains_token(to) {
            return vec![];
        }

        if from == to {
            return vec![];
        }

        let mut paths = Vec::new();
        let mut queue: VecDeque<(Address, Vec<Edge>, Vec<Address>)> = VecDeque::new();

        // Start BFS from the source token
        queue.push_back((*from, vec![], vec![*from]));

        while let Some((current, edges, tokens)) = queue.pop_front() {
            // Check hop limit
            if edges.len() >= max_hops {
                continue;
            }

            // Explore neighbors
            for edge in self.neighbors(&current) {
                // Avoid cycles (don't revisit tokens)
                if tokens.contains(&edge.token_out) {
                    continue;
                }

                let mut new_edges = edges.clone();
                new_edges.push(edge.clone());

                let mut new_tokens = tokens.clone();
                new_tokens.push(edge.token_out);

                // Found a path to destination
                if edge.token_out == *to {
                    paths.push(Path {
                        edges: new_edges,
                        tokens: new_tokens,
                    });
                } else {
                    // Continue searching
                    queue.push_back((edge.token_out, new_edges, new_tokens));
                }
            }
        }

        paths
    }

    /// Prunes the graph to only include specified protocols.
    pub fn prune_protocols(&mut self, keep: &[ProtocolSystem]) {
        let keep_set: HashSet<_> = keep.iter().collect();

        // Remove pools not in the keep set
        let pools_to_remove: Vec<_> = self
            .pool_tokens
            .keys()
            .filter(|pool_id| {
                // Find the protocol system for this pool
                self.adjacency.values().flatten().find(|e| &e.pool_id == *pool_id).map(|e| !keep_set.contains(&e.protocol_system)).unwrap_or(true)
            })
            .cloned()
            .collect();

        for pool_id in pools_to_remove {
            self.remove_pool(&pool_id);
        }
    }

    /// Prunes the graph to remove specified tokens.
    pub fn prune_tokens(&mut self, remove: &[Address]) {
        let remove_set: HashSet<_> = remove.iter().collect();

        // Find pools that involve any of the removed tokens
        let pools_to_remove: Vec<_> = self
            .pool_tokens
            .iter()
            .filter(|(_, tokens)| tokens.iter().any(|t| remove_set.contains(t)))
            .map(|(id, _)| id.clone())
            .collect();

        for pool_id in pools_to_remove {
            self.remove_pool(&pool_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_and_find_paths() {
        let mut graph = RouteGraph::new();

        let token_a = Address::repeat_byte(0x0A);
        let token_b = Address::repeat_byte(0x0B);
        let token_c = Address::repeat_byte(0x0C);

        // A <-> B pool
        graph.add_pool(
            "pool_ab".to_string(),
            &[token_a, token_b],
            ProtocolSystem::UniswapV2,
        );

        // B <-> C pool
        graph.add_pool(
            "pool_bc".to_string(),
            &[token_b, token_c],
            ProtocolSystem::UniswapV2,
        );

        // Direct path A -> B
        let paths = graph.find_paths(&token_a, &token_b, 3);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].hop_count(), 1);

        // 2-hop path A -> B -> C
        let paths = graph.find_paths(&token_a, &token_c, 3);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].hop_count(), 2);

        // No path with 1 hop limit
        let paths = graph.find_paths(&token_a, &token_c, 1);
        assert!(paths.is_empty());
    }
}

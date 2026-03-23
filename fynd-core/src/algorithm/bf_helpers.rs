//! Shared helpers for Bellman-Ford based algorithms (routing and pricing).
//!
//! Both `bellman_ford.rs` (A-to-B routing) and `bellman_ford_pricing.rs` (one-to-all pricing)
//! use forbid-revisits SPFA and share predecessor-chain utilities.

use std::collections::{HashSet, VecDeque};

use petgraph::{graph::NodeIndex, prelude::EdgeRef};

use super::AlgorithmError;
use crate::{graph::petgraph::StableDiGraph, types::ComponentId};

/// Checks whether extending the path at `from` to `target_node` via `target_pool`
/// would create a node or pool revisit. Single walk of the predecessor chain.
pub(crate) fn path_has_conflict(
    from: NodeIndex,
    target_node: NodeIndex,
    target_pool: &ComponentId,
    predecessor: &[Option<(NodeIndex, ComponentId)>],
) -> bool {
    let mut current = from;
    loop {
        if current == target_node {
            return true;
        }
        match &predecessor[current.index()] {
            Some((prev, cid)) => {
                if cid == target_pool {
                    return true;
                }
                current = *prev;
            }
            None => return false,
        }
    }
}

/// Reconstructs the path from `dest` back to `source` by walking the predecessor array.
pub(crate) fn reconstruct_path(
    dest: NodeIndex,
    source: NodeIndex,
    predecessor: &[Option<(NodeIndex, ComponentId)>],
) -> Result<Vec<(NodeIndex, NodeIndex, ComponentId)>, AlgorithmError> {
    let mut path = Vec::new();
    let mut current = dest;
    let mut visited = HashSet::new();

    while current != source {
        if !visited.insert(current) {
            return Err(AlgorithmError::Other("cycle in predecessor chain".to_string()));
        }

        let idx = current.index();
        match predecessor
            .get(idx)
            .and_then(|p| p.as_ref())
        {
            Some((prev_node, component_id)) => {
                path.push((*prev_node, current, component_id.clone()));
                current = *prev_node;
            }
            None => {
                return Err(AlgorithmError::Other(format!(
                    "broken predecessor chain at node {idx}"
                )));
            }
        }
    }

    path.reverse();
    Ok(path)
}

/// Extracts subgraph edges via BFS from `start` up to `max_depth` hops.
pub(crate) fn extract_subgraph_edges(
    start: NodeIndex,
    max_depth: usize,
    graph: &StableDiGraph<()>,
) -> Vec<(NodeIndex, NodeIndex, ComponentId)> {
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    let mut edges = Vec::new();

    visited.insert(start);
    queue.push_back((start, 0usize));

    while let Some((node, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }

        for edge in graph.edges(node) {
            let target = edge.target();
            let component_id = &edge.weight().component_id;

            edges.push((node, target, component_id.clone()));

            if visited.insert(target) {
                queue.push_back((target, depth + 1));
            }
        }
    }

    edges
}

#[cfg(test)]
mod tests {
    use petgraph::graph::NodeIndex;

    use super::*;

    #[test]
    fn path_has_conflict_detects_node_and_pool() {
        // Path: 0 -[pool_a]-> 1 -[pool_b]-> 2
        let mut pred: Vec<Option<(NodeIndex, ComponentId)>> = vec![None; 4];
        pred[1] = Some((NodeIndex::new(0), "pool_a".into()));
        pred[2] = Some((NodeIndex::new(1), "pool_b".into()));

        // Node conflict: node 0 is in path
        assert!(path_has_conflict(NodeIndex::new(2), NodeIndex::new(0), &"any".into(), &pred));
        // No conflict: node 3 is not in path
        assert!(!path_has_conflict(NodeIndex::new(2), NodeIndex::new(3), &"any".into(), &pred));
        // Self-check: node 2 is itself in the path
        assert!(path_has_conflict(NodeIndex::new(2), NodeIndex::new(2), &"any".into(), &pred));
        // Pool conflict: pool_a used
        assert!(path_has_conflict(NodeIndex::new(2), NodeIndex::new(3), &"pool_a".into(), &pred));
        // Pool conflict: pool_b used
        assert!(path_has_conflict(NodeIndex::new(2), NodeIndex::new(3), &"pool_b".into(), &pred));
        // No pool conflict: pool_c not used
        assert!(!path_has_conflict(NodeIndex::new(2), NodeIndex::new(3), &"pool_c".into(), &pred));
    }

    #[test]
    fn reconstruct_path_simple() {
        // Path: 0 -[pool_a]-> 1 -[pool_b]-> 2
        let mut pred: Vec<Option<(NodeIndex, ComponentId)>> = vec![None; 3];
        pred[1] = Some((NodeIndex::new(0), "pool_a".into()));
        pred[2] = Some((NodeIndex::new(1), "pool_b".into()));

        let path = reconstruct_path(NodeIndex::new(2), NodeIndex::new(0), &pred).unwrap();
        assert_eq!(path.len(), 2);
        assert_eq!(path[0], (NodeIndex::new(0), NodeIndex::new(1), "pool_a".into()));
        assert_eq!(path[1], (NodeIndex::new(1), NodeIndex::new(2), "pool_b".into()));
    }

    #[test]
    fn reconstruct_path_broken_chain() {
        let pred: Vec<Option<(NodeIndex, ComponentId)>> = vec![None; 3];
        let result = reconstruct_path(NodeIndex::new(2), NodeIndex::new(0), &pred);
        assert!(result.is_err());
    }
}

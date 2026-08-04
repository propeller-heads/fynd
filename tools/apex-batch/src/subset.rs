//! The pool subset APEX sees: native protocols only, 2-hop closure around order tokens.
//!
//! The filter is the lever that makes an APEX budget plausible (grill r2 F19), and the study
//! restricts APEX to natively-simulated protocols (user decision — vm:* pools cost ~5× per
//! simulation and cannot serialize for capture). Selection classes, strongest first:
//!
//! 1. **direct** — both of a pool's relevant tokens are order tokens;
//! 2. **adjacent** — one token is an order token (opens one-intermediary routes);
//! 3. **linking** — the pool connects two tokens that are themselves adjacent to order tokens
//!    (closes the 2-hop paths `order → X → Y → order`).
//!
//! The `max_pools` cap keeps class order, then component-id order — deterministic across runs.

use std::collections::{HashMap, HashSet};

/// A pool candidate for subset selection: its component id, protocol system, and token set.
#[derive(Debug, Clone)]
pub struct PoolCandidate {
    pub component_id: String,
    pub protocol_system: String,
    pub tokens: Vec<[u8; 20]>,
}

/// Protocols APEX may see: natively simulated, non-RFQ. Everything else is dropped, counted.
pub fn is_native_protocol(protocol_system: &str) -> bool {
    !protocol_system.starts_with("vm:") && !protocol_system.starts_with("rfq:")
}

/// Selection outcome: kept component ids (class-ordered) plus drop counters.
#[derive(Debug, Default)]
pub struct SubsetSelection {
    pub kept: Vec<String>,
    pub dropped_non_native: u64,
    pub dropped_by_cap: u64,
}

/// Select the pool subset for one batch's order tokens.
pub fn select_pool_subset(
    candidates: &[PoolCandidate],
    order_tokens: &HashSet<[u8; 20]>,
    max_pools: usize,
) -> SubsetSelection {
    let mut selection = SubsetSelection::default();

    // Tokens one pool-hop away from an order token, for the linking class.
    let mut adjacent_tokens: HashSet<[u8; 20]> = HashSet::new();
    for pool in candidates {
        if !is_native_protocol(&pool.protocol_system) {
            continue;
        }
        if pool
            .tokens
            .iter()
            .any(|t| order_tokens.contains(t))
        {
            adjacent_tokens.extend(pool.tokens.iter().copied());
        }
    }

    // Class per pool: 0 = direct, 1 = adjacent, 2 = linking, none = out of scope.
    let mut classed: Vec<(u8, &PoolCandidate)> = Vec::new();
    for pool in candidates {
        if !is_native_protocol(&pool.protocol_system) {
            selection.dropped_non_native += 1;
            continue;
        }
        let order_hits = pool
            .tokens
            .iter()
            .filter(|t| order_tokens.contains(*t))
            .count();
        let adjacent_hits = pool
            .tokens
            .iter()
            .filter(|t| adjacent_tokens.contains(*t))
            .count();
        let class = if order_hits >= 2 {
            0
        } else if order_hits == 1 {
            1
        } else if adjacent_hits >= 2 {
            2
        } else {
            continue;
        };
        classed.push((class, pool));
    }
    classed.sort_by(|(class_a, pool_a), (class_b, pool_b)| {
        class_a.cmp(class_b).then_with(|| {
            pool_a
                .component_id
                .cmp(&pool_b.component_id)
        })
    });

    if classed.len() > max_pools {
        selection.dropped_by_cap = (classed.len() - max_pools) as u64;
        classed.truncate(max_pools);
    }
    selection.kept = classed
        .into_iter()
        .map(|(_, pool)| pool.component_id.clone())
        .collect();
    selection
}

/// Union of token addresses across the kept pools plus the order tokens — the token closure the
/// APEX call must fully price (grill r2 F1: every pool token enters the price search under
/// `two_hops`).
pub fn token_closure(
    candidates: &[PoolCandidate],
    kept: &HashMap<String, usize>,
    order_tokens: &HashSet<[u8; 20]>,
) -> HashSet<[u8; 20]> {
    let mut closure = order_tokens.clone();
    for pool in candidates {
        if kept.contains_key(&pool.component_id) {
            closure.extend(pool.tokens.iter().copied());
        }
    }
    closure
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(id: &str, protocol: &str, tokens: &[u8]) -> PoolCandidate {
        PoolCandidate {
            component_id: id.to_string(),
            protocol_system: protocol.to_string(),
            tokens: tokens
                .iter()
                .map(|&b| [b; 20])
                .collect(),
        }
    }

    #[test]
    fn test_classes_and_order() {
        // Orders touch tokens 1 and 2. Pool layout: direct (1,2), adjacent (2,3), linking (3,4)
        // where 4 is adjacent via pool (1,4), out-of-scope (5,6).
        let candidates = vec![
            pool("linking", "uniswap_v3", &[3, 4]),
            pool("adjacent", "uniswap_v2", &[2, 3]),
            pool("direct", "uniswap_v2", &[1, 2]),
            pool("adjacent2", "uniswap_v2", &[1, 4]),
            pool("faraway", "uniswap_v2", &[5, 6]),
        ];
        let order_tokens: HashSet<[u8; 20]> = [[1u8; 20], [2u8; 20]].into();
        let selection = select_pool_subset(&candidates, &order_tokens, 10);
        assert_eq!(selection.kept, vec!["direct", "adjacent", "adjacent2", "linking"]);
        assert_eq!(selection.dropped_non_native, 0);
        assert_eq!(selection.dropped_by_cap, 0);
    }

    #[test]
    fn test_vm_and_rfq_pools_dropped() {
        let candidates = vec![
            pool("vm-pool", "vm:curve", &[1, 2]),
            pool("rfq-pool", "rfq:bebop", &[1, 2]),
            pool("native", "aerodrome_v1", &[1, 2]),
        ];
        let order_tokens: HashSet<[u8; 20]> = [[1u8; 20], [2u8; 20]].into();
        let selection = select_pool_subset(&candidates, &order_tokens, 10);
        assert_eq!(selection.kept, vec!["native"]);
        assert_eq!(selection.dropped_non_native, 2);
    }

    #[test]
    fn test_cap_keeps_class_priority() {
        let candidates = vec![
            pool("adjacent", "uniswap_v2", &[2, 3]),
            pool("direct-b", "uniswap_v2", &[1, 2]),
            pool("direct-a", "uniswap_v2", &[1, 2]),
        ];
        let order_tokens: HashSet<[u8; 20]> = [[1u8; 20], [2u8; 20]].into();
        let selection = select_pool_subset(&candidates, &order_tokens, 2);
        assert_eq!(selection.kept, vec!["direct-a", "direct-b"]);
        assert_eq!(selection.dropped_by_cap, 1);
    }

    #[test]
    fn test_token_closure_covers_kept_pool_tokens() {
        let candidates = vec![pool("adjacent", "uniswap_v2", &[2, 3])];
        let kept: HashMap<String, usize> = HashMap::from([("adjacent".to_string(), 0)]);
        let order_tokens: HashSet<[u8; 20]> = [[1u8; 20], [2u8; 20]].into();
        let closure = token_closure(&candidates, &kept, &order_tokens);
        assert!(closure.contains(&[3u8; 20]), "pool token 3 must be priced");
        assert_eq!(closure.len(), 3);
    }
}

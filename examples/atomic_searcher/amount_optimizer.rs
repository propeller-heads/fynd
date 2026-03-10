//! Golden section search for optimal arbitrage input amount.
//!
//! Given a cycle, finds the input amount that maximizes net profit
//! (amount_out - amount_in - gas_cost). Uses the golden ratio to narrow
//! the search interval without requiring derivatives.
//!
//! Based on the optimization approach in Janos Tapolcai's tycho-searcher.

use std::collections::HashMap;

use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};
use petgraph::graph::NodeIndex;
use tracing::{debug, trace};
use tycho_simulation::tycho_core::models::token::Token;

use fynd::feed::market_data::SharedMarketData;
use fynd::graph::petgraph::StableDiGraph;

use crate::types::{CycleCandidate, EvaluatedCycle};

/// Golden ratio constant for the search.
const PHI: f64 = 1.618_033_988_749_895;

/// Evaluates a cycle at a given input amount.
///
/// Simulates each swap in sequence, returns (amount_out, total_gas).
/// Returns None if any simulation fails.
fn evaluate_cycle(
    edges: &[(petgraph::graph::NodeIndex, petgraph::graph::NodeIndex, String)],
    amount_in: &BigUint,
    market: &SharedMarketData,
    token_map: &HashMap<NodeIndex, Token>,
) -> Option<(BigUint, BigUint)> {
    let mut current_amount = amount_in.clone();
    let mut total_gas = BigUint::ZERO;

    for (from_node, to_node, component_id) in edges {
        let token_in = token_map.get(from_node)?;
        let token_out = token_map.get(to_node)?;
        let sim_state = market.get_simulation_state(component_id)?;

        let result = sim_state
            .get_amount_out(current_amount, token_in, token_out)
            .ok()?;

        total_gas += result.gas;
        current_amount = result.amount;
    }

    Some((current_amount, total_gas))
}

/// Computes net profit for a cycle at a given input amount.
///
/// Returns profit as f64 for GSS comparison (actual amounts use BigUint).
fn cycle_profit(
    edges: &[(NodeIndex, NodeIndex, String)],
    amount_in: &BigUint,
    market: &SharedMarketData,
    token_map: &HashMap<NodeIndex, Token>,
    gas_price_wei: &BigUint,
    gas_token_price_num: &BigUint,
    gas_token_price_den: &BigUint,
) -> Option<f64> {
    let (amount_out, total_gas) = evaluate_cycle(edges, amount_in, market, token_map)?;

    if amount_out <= *amount_in {
        return Some(f64::NEG_INFINITY);
    }

    let gross_profit = &amount_out - amount_in;

    // Gas cost in source token terms
    let gas_cost_wei = &total_gas * gas_price_wei;
    let gas_cost_in_token = &gas_cost_wei * gas_token_price_num / gas_token_price_den;

    let profit = gross_profit.to_f64().unwrap_or(0.0)
        - gas_cost_in_token.to_f64().unwrap_or(f64::INFINITY);

    Some(profit)
}

/// Optimizes the input amount for a cycle using golden section search.
///
/// The profit function for AMM cycles is concave: increasing input amounts
/// cause increasing slippage, so there's a single peak. GSS exploits this
/// without needing derivatives.
pub fn optimize_amount(
    candidate: &CycleCandidate,
    graph: &StableDiGraph<()>,
    market: &SharedMarketData,
    token_map: &HashMap<NodeIndex, Token>,
    gas_price_wei: &BigUint,
    gas_token_price_num: &BigUint,
    gas_token_price_den: &BigUint,
    seed_amount: &BigUint,
    tolerance: f64,
    max_iterations: usize,
) -> Option<EvaluatedCycle> {
    // Convert edge addresses to node indices for simulation
    let edge_nodes: Vec<(NodeIndex, NodeIndex, String)> = candidate
        .edges
        .iter()
        .filter_map(|(from_addr, to_addr, cid)| {
            let from_node = graph
                .node_indices()
                .find(|&n| graph[n] == *from_addr)?;
            let to_node = graph
                .node_indices()
                .find(|&n| graph[n] == *to_addr)?;
            Some((from_node, to_node, cid.clone()))
        })
        .collect();

    if edge_nodes.len() != candidate.edges.len() {
        return None;
    }

    let profit_fn = |amount: &BigUint| -> f64 {
        cycle_profit(
            &edge_nodes,
            amount,
            market,
            token_map,
            gas_price_wei,
            gas_token_price_num,
            gas_token_price_den,
        )
        .unwrap_or(f64::NEG_INFINITY)
    };

    // Phase 1: Find upper bound by doubling until profit decreases
    let mut lo = seed_amount / 10u32; // Start at 10% of seed
    if lo.is_zero() {
        lo = BigUint::from(1u64);
    }
    let mut hi = seed_amount.clone();

    let mut hi_profit = profit_fn(&hi);
    for _ in 0..20 {
        let doubled = &hi * 2u32;
        let doubled_profit = profit_fn(&doubled);
        if doubled_profit < hi_profit {
            break;
        }
        hi = doubled;
        hi_profit = doubled_profit;
    }

    // Phase 2: Golden section search
    let resp = 2.0 - PHI; // ~0.382
    let mut a = lo.to_f64().unwrap_or(0.0);
    let mut b = hi.to_f64().unwrap_or(1e18);

    let mut x1 = a + resp * (b - a);
    let mut x2 = b - resp * (b - a);
    let mut f1 = profit_fn(&BigUint::from(x1 as u128));
    let mut f2 = profit_fn(&BigUint::from(x2 as u128));

    for i in 0..max_iterations {
        if (b - a).abs() < tolerance * a.max(1.0) {
            trace!(iteration = i, "GSS converged");
            break;
        }

        if f1 < f2 {
            a = x1;
            x1 = x2;
            f1 = f2;
            x2 = b - resp * (b - a);
            f2 = profit_fn(&BigUint::from(x2 as u128));
        } else {
            b = x2;
            x2 = x1;
            f2 = f1;
            x1 = a + resp * (b - a);
            f1 = profit_fn(&BigUint::from(x1 as u128));
        }
    }

    let optimal_amount = BigUint::from(((a + b) / 2.0) as u128);

    // Final evaluation at optimal amount
    let (amount_out, total_gas) = evaluate_cycle(&edge_nodes, &optimal_amount, market, token_map)?;

    if amount_out <= optimal_amount {
        debug!("cycle not profitable at optimal amount");
        return Some(EvaluatedCycle {
            edges: candidate.edges.clone(),
            optimal_amount_in: optimal_amount.clone(),
            amount_out: amount_out.clone(),
            gross_profit: BigUint::ZERO,
            gas_cost: BigUint::ZERO,
            net_profit: 0,
            is_profitable: false,
        });
    }

    let gross_profit = &amount_out - &optimal_amount;
    let gas_cost_wei = &total_gas * gas_price_wei;
    let gas_cost = &gas_cost_wei * gas_token_price_num / gas_token_price_den;

    let net_profit = gross_profit
        .to_i128()
        .unwrap_or(i128::MAX)
        .saturating_sub(gas_cost.to_i128().unwrap_or(i128::MAX));

    let is_profitable = net_profit > 0;

    if is_profitable {
        debug!(
            net_profit,
            optimal_amount = %optimal_amount,
            amount_out = %amount_out,
            "profitable cycle found"
        );
    }

    Some(EvaluatedCycle {
        edges: candidate.edges.clone(),
        optimal_amount_in: optimal_amount,
        amount_out,
        gross_profit,
        gas_cost,
        net_profit,
        is_profitable,
    })
}


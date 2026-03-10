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
use tracing::{debug, trace, warn};
use tycho_simulation::tycho_core::models::token::Token;

use fynd::feed::market_data::SharedMarketData;
use fynd::graph::petgraph::StableDiGraph;

use crate::types::{CycleCandidate, EvaluatedCycle};

/// Golden ratio constant for the search.
const PHI: f64 = 1.618_033_988_749_895;

/// Maximum wei value for GSS search (~1000 ETH). Beyond this, f64 loses
/// integer precision and the `as u128` casts become unreliable.
const MAX_GSS_WEI: u128 = 1_000_000_000_000_000_000_000; // 1e21

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

    // Cap hi to avoid f64 precision loss beyond ~9e15 integer precision.
    let max_gss = BigUint::from(MAX_GSS_WEI);
    if hi > max_gss {
        warn!(
            hi_wei = %hi,
            cap_wei = MAX_GSS_WEI,
            "GSS upper bound exceeds f64 precision, capping at ~1000 ETH"
        );
        hi = max_gss;
        hi_profit = profit_fn(&hi);
    }

    // If upper bound profit is not finite, we cannot search meaningfully.
    if !hi_profit.is_finite() {
        debug!("GSS upper bound profit is not finite, skipping optimization");
        return Some(EvaluatedCycle {
            edges: candidate.edges.clone(),
            optimal_amount_in: seed_amount.clone(),
            amount_out: BigUint::ZERO,
            gross_profit: BigUint::ZERO,
            gas_cost: BigUint::ZERO,
            net_profit: 0,
            is_profitable: false,
        });
    }

    // Phase 2: Golden section search
    let resp = 2.0 - PHI; // ~0.382
    let mut a = lo.to_f64().unwrap_or(0.0);
    let mut b = hi.to_f64().unwrap_or(1e18);

    /// Safely converts an f64 to BigUint, returning None if the value is
    /// not finite, negative, or exceeds u128 range.
    fn safe_to_biguint(x: f64) -> Option<BigUint> {
        if !x.is_finite() || x < 0.0 || x > u128::MAX as f64 {
            return None;
        }
        Some(BigUint::from(x as u128))
    }

    let mut x1 = a + resp * (b - a);
    let mut x2 = b - resp * (b - a);
    let mut f1 = safe_to_biguint(x1)
        .map(|v| profit_fn(&v))
        .unwrap_or(f64::NEG_INFINITY);
    let mut f2 = safe_to_biguint(x2)
        .map(|v| profit_fn(&v))
        .unwrap_or(f64::NEG_INFINITY);

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
            f2 = safe_to_biguint(x2)
                .map(|v| profit_fn(&v))
                .unwrap_or(f64::NEG_INFINITY);
        } else {
            b = x2;
            x2 = x1;
            f2 = f1;
            x1 = a + resp * (b - a);
            f1 = safe_to_biguint(x1)
                .map(|v| profit_fn(&v))
                .unwrap_or(f64::NEG_INFINITY);
        }
    }

    let mid = (a + b) / 2.0;
    let optimal_amount = match safe_to_biguint(mid) {
        Some(v) => v,
        None => {
            debug!("GSS midpoint not convertible to BigUint");
            return None;
        }
    };

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

#[cfg(test)]
mod tests {
    use super::*;

    use std::any::Any;

    use chrono::NaiveDateTime;
    use num_bigint::BigUint;
    use tycho_simulation::tycho_core::{
        dto::ProtocolStateDelta,
        models::{protocol::ProtocolComponent, token::Token, Address, Chain},
        simulation::{
            errors::{SimulationError, TransitionError},
            protocol_sim::{
                Balances, GetAmountOutResult, PoolSwap, ProtocolSim,
                QueryPoolSwapParams,
            },
        },
        Bytes,
    };

    use fynd::{
        feed::market_data::SharedMarketData,
        graph::{petgraph::PetgraphStableDiGraphManager, GraphManager},
    };

    use crate::types::CycleCandidate;

    // ==================== Local MockProtocolSim ====================

    #[derive(Debug, Clone)]
    struct MockSim {
        spot_price: u32,
        gas: u64,
    }

    impl MockSim {
        fn new(spot_price: u32) -> Self {
            Self { spot_price, gas: 50_000 }
        }

        fn with_gas(mut self, gas: u64) -> Self {
            self.gas = gas;
            self
        }
    }

    impl ProtocolSim for MockSim {
        fn fee(&self) -> f64 {
            0.0
        }

        fn spot_price(
            &self,
            base: &Token,
            quote: &Token,
        ) -> Result<f64, SimulationError> {
            if base.address < quote.address {
                Ok(1.0 / self.spot_price as f64)
            } else {
                Ok(self.spot_price as f64)
            }
        }

        fn get_amount_out(
            &self,
            amount_in: BigUint,
            token_in: &Token,
            token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            let amount_out = if token_in.address < token_out.address {
                &amount_in * self.spot_price
            } else {
                &amount_in / self.spot_price
            };
            let new_state = Box::new(MockSim {
                spot_price: self.spot_price + 1,
                gas: self.gas,
            });
            Ok(GetAmountOutResult::new(
                amount_out,
                BigUint::from(self.gas),
                new_state,
            ))
        }

        fn query_pool_swap(
            &self,
            _params: &QueryPoolSwapParams,
        ) -> Result<PoolSwap, SimulationError> {
            unimplemented!()
        }

        fn get_limits(
            &self,
            _sell_token: Bytes,
            _buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            unimplemented!()
        }

        fn delta_transition(
            &mut self,
            _delta: ProtocolStateDelta,
            _tokens: &HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError<String>> {
            unimplemented!()
        }

        fn clone_box(&self) -> Box<dyn ProtocolSim> {
            Box::new(self.clone())
        }

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }

        fn eq(&self, other: &dyn ProtocolSim) -> bool {
            other
                .as_any()
                .downcast_ref::<Self>()
                .map(|o| o.spot_price == self.spot_price)
                .unwrap_or(false)
        }
    }

    // ==================== Test Helpers ====================

    fn addr(b: u8) -> Address {
        Address::from([b; 20])
    }

    fn token(addr_b: u8, symbol: &str) -> Token {
        Token {
            address: addr(addr_b),
            symbol: symbol.to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        }
    }

    fn component(id: &str, tokens: &[Token]) -> ProtocolComponent {
        ProtocolComponent::new(
            id,
            "uniswap_v2",
            "swap",
            Chain::Ethereum,
            tokens.iter().map(|t| t.address.clone()).collect(),
            vec![],
            HashMap::new(),
            Default::default(),
            Default::default(),
            NaiveDateTime::default(),
        )
    }

    /// Sets up a profitable triangle A(0x01)->B(0x02)->C(0x03)->A(0x01).
    ///
    /// Returns the market, graph manager, token map, and a CycleCandidate
    /// with address-based edges ready for `optimize_amount`.
    fn setup_profitable_triangle() -> (
        SharedMarketData,
        PetgraphStableDiGraphManager<()>,
        HashMap<NodeIndex, Token>,
        CycleCandidate,
    ) {
        // Spot prices: A->B *2, B->C *3, C->A /1
        // Product = 2*3/1 = 6 > 1
        let tok_a = token(0x01, "A");
        let tok_b = token(0x02, "B");
        let tok_c = token(0x03, "C");

        let comp_ab = component("pool_ab", &[tok_a.clone(), tok_b.clone()]);
        let comp_bc = component("pool_bc", &[tok_b.clone(), tok_c.clone()]);
        let comp_ca = component("pool_ca", &[tok_c.clone(), tok_a.clone()]);

        let mut market = SharedMarketData::new();
        market.upsert_components(
            [comp_ab, comp_bc, comp_ca].into_iter(),
        );
        market.update_states([
            (
                "pool_ab".to_string(),
                Box::new(MockSim::new(2).with_gas(100)) as Box<dyn ProtocolSim>,
            ),
            (
                "pool_bc".to_string(),
                Box::new(MockSim::new(3).with_gas(100)) as Box<dyn ProtocolSim>,
            ),
            (
                "pool_ca".to_string(),
                Box::new(MockSim::new(1).with_gas(100)) as Box<dyn ProtocolSim>,
            ),
        ]);
        market.upsert_tokens([tok_a.clone(), tok_b.clone(), tok_c.clone()]);

        let mut gm = PetgraphStableDiGraphManager::<()>::default();
        gm.initialize_graph(&market.component_topology());

        let graph = gm.graph();
        let token_map: HashMap<NodeIndex, Token> = graph
            .node_indices()
            .filter_map(|n| {
                market.get_token(&graph[n]).cloned().map(|t| (n, t))
            })
            .collect();

        // Build the CycleCandidate with address-based edges
        // (this is what find_cycles returns).
        let candidate = CycleCandidate {
            edges: vec![
                (tok_a.address.clone(), tok_b.address.clone(), "pool_ab".into()),
                (tok_b.address.clone(), tok_c.address.clone(), "pool_bc".into()),
                (tok_c.address.clone(), tok_a.address.clone(), "pool_ca".into()),
            ],
            relaxation_amount_out: BigUint::from(6000u64),
            _layer: 3,
        };

        (market, gm, token_map, candidate)
    }

    // ==================== Tests ====================

    #[test]
    fn test_gss_finds_optimal_amount() {
        let (market, gm, token_map, candidate) =
            setup_profitable_triangle();
        let graph = gm.graph();

        let seed = BigUint::from(1000u64);
        let gas_price = BigUint::from(1u64);
        let gas_num = BigUint::from(1u64);
        let gas_den = BigUint::from(1u64);

        let result = optimize_amount(
            &candidate,
            graph,
            &market,
            &token_map,
            &gas_price,
            &gas_num,
            &gas_den,
            &seed,
            0.001,
            30,
        );

        assert!(result.is_some(), "optimization should succeed");
        let evaluated = result.unwrap();

        // With product = 6 and no real slippage (linear mock),
        // any amount is "profitable" in gross terms.
        assert!(
            evaluated.amount_out > evaluated.optimal_amount_in,
            "amount_out ({}) should exceed amount_in ({})",
            evaluated.amount_out,
            evaluated.optimal_amount_in,
        );
        assert!(
            evaluated.gross_profit > BigUint::ZERO,
            "gross profit should be positive"
        );
    }

    #[test]
    fn test_gss_unprofitable_cycle() {
        // Triangle where product < 1:
        //   A->B: amount * 1   (sp=1)
        //   B->C: amount * 1   (sp=1)
        //   C->A: amount / 2   (sp=2)
        // Product = 1*1/2 = 0.5 < 1
        let tok_a = token(0x01, "A");
        let tok_b = token(0x02, "B");
        let tok_c = token(0x03, "C");

        let comp_ab = component("pool_ab", &[tok_a.clone(), tok_b.clone()]);
        let comp_bc = component("pool_bc", &[tok_b.clone(), tok_c.clone()]);
        let comp_ca = component("pool_ca", &[tok_c.clone(), tok_a.clone()]);

        let mut market = SharedMarketData::new();
        market.upsert_components(
            [comp_ab, comp_bc, comp_ca].into_iter(),
        );
        market.update_states([
            ("pool_ab".to_string(), Box::new(MockSim::new(1)) as Box<dyn ProtocolSim>),
            ("pool_bc".to_string(), Box::new(MockSim::new(1)) as Box<dyn ProtocolSim>),
            ("pool_ca".to_string(), Box::new(MockSim::new(2)) as Box<dyn ProtocolSim>),
        ]);
        market.upsert_tokens([tok_a.clone(), tok_b.clone(), tok_c.clone()]);

        let mut gm = PetgraphStableDiGraphManager::<()>::default();
        gm.initialize_graph(&market.component_topology());

        let graph = gm.graph();
        let token_map: HashMap<NodeIndex, Token> = graph
            .node_indices()
            .filter_map(|n| {
                market.get_token(&graph[n]).cloned().map(|t| (n, t))
            })
            .collect();

        let candidate = CycleCandidate {
            edges: vec![
                (tok_a.address.clone(), tok_b.address.clone(), "pool_ab".into()),
                (tok_b.address.clone(), tok_c.address.clone(), "pool_bc".into()),
                (tok_c.address.clone(), tok_a.address.clone(), "pool_ca".into()),
            ],
            relaxation_amount_out: BigUint::from(500u64), // less than seed
            _layer: 3,
        };

        let seed = BigUint::from(1000u64);
        let gas_price = BigUint::from(1u64);
        let gas_num = BigUint::from(1u64);
        let gas_den = BigUint::from(1u64);

        let result = optimize_amount(
            &candidate,
            graph,
            &market,
            &token_map,
            &gas_price,
            &gas_num,
            &gas_den,
            &seed,
            0.001,
            30,
        );

        assert!(result.is_some(), "should return Some even for unprofitable");
        let evaluated = result.unwrap();
        assert!(
            !evaluated.is_profitable,
            "unprofitable cycle should not be marked profitable"
        );
    }

    #[test]
    fn test_gss_handles_small_seed() {
        // Edge case: seed = 1 (very small).
        let (market, gm, token_map, candidate) =
            setup_profitable_triangle();
        let graph = gm.graph();

        let seed = BigUint::from(1u64);
        let gas_price = BigUint::from(0u64); // zero gas so we focus on amount math
        let gas_num = BigUint::from(1u64);
        let gas_den = BigUint::from(1u64);

        let result = optimize_amount(
            &candidate,
            graph,
            &market,
            &token_map,
            &gas_price,
            &gas_num,
            &gas_den,
            &seed,
            0.001,
            30,
        );

        // Should still produce a result (even if amounts are tiny).
        assert!(result.is_some(), "optimization should succeed even with seed=1");
    }

    #[test]
    fn test_evaluate_cycle_returns_correct_amounts() {
        // Directly test evaluate_cycle with known inputs.
        let tok_a = token(0x01, "A");
        let tok_b = token(0x02, "B");
        let tok_c = token(0x03, "C");

        let comp_ab = component("pool_ab", &[tok_a.clone(), tok_b.clone()]);
        let comp_bc = component("pool_bc", &[tok_b.clone(), tok_c.clone()]);
        let comp_ca = component("pool_ca", &[tok_c.clone(), tok_a.clone()]);

        let mut market = SharedMarketData::new();
        market.upsert_components(
            [comp_ab, comp_bc, comp_ca].into_iter(),
        );
        market.update_states([
            (
                "pool_ab".to_string(),
                Box::new(MockSim::new(2).with_gas(100)) as Box<dyn ProtocolSim>,
            ),
            (
                "pool_bc".to_string(),
                Box::new(MockSim::new(3).with_gas(200)) as Box<dyn ProtocolSim>,
            ),
            (
                "pool_ca".to_string(),
                Box::new(MockSim::new(1).with_gas(300)) as Box<dyn ProtocolSim>,
            ),
        ]);
        market.upsert_tokens([tok_a.clone(), tok_b.clone(), tok_c.clone()]);

        let mut gm = PetgraphStableDiGraphManager::<()>::default();
        gm.initialize_graph(&market.component_topology());

        let graph = gm.graph();
        let token_map: HashMap<NodeIndex, Token> = graph
            .node_indices()
            .filter_map(|n| {
                market.get_token(&graph[n]).cloned().map(|t| (n, t))
            })
            .collect();

        // Get node indices
        let node_a = graph
            .node_indices()
            .find(|&n| graph[n] == tok_a.address)
            .unwrap();
        let node_b = graph
            .node_indices()
            .find(|&n| graph[n] == tok_b.address)
            .unwrap();
        let node_c = graph
            .node_indices()
            .find(|&n| graph[n] == tok_c.address)
            .unwrap();

        let edges = vec![
            (node_a, node_b, "pool_ab".to_string()),
            (node_b, node_c, "pool_bc".to_string()),
            (node_c, node_a, "pool_ca".to_string()),
        ];

        let amount_in = BigUint::from(1000u64);
        let result = evaluate_cycle(&edges, &amount_in, &market, &token_map);

        assert!(result.is_some());
        let (amount_out, total_gas) = result.unwrap();

        // A->B: 1000 * 2 = 2000 (0x01 < 0x02)
        // B->C: 2000 * 3 = 6000 (0x02 < 0x03)
        // C->A: 6000 / 1 = 6000 (0x03 > 0x01)
        assert_eq!(amount_out, BigUint::from(6000u64));

        // Gas: 100 + 200 + 300 = 600
        assert_eq!(total_gas, BigUint::from(600u64));
    }

    #[test]
    fn test_candidate_with_missing_nodes_returns_none() {
        // Candidate references addresses not in the graph => None.
        let tok_a = token(0x01, "A");
        let tok_b = token(0x02, "B");

        let comp_ab = component("pool_ab", &[tok_a.clone(), tok_b.clone()]);

        let mut market = SharedMarketData::new();
        market.upsert_components([comp_ab].into_iter());
        market.update_states([(
            "pool_ab".to_string(),
            Box::new(MockSim::new(2)) as Box<dyn ProtocolSim>,
        )]);
        market.upsert_tokens([tok_a.clone(), tok_b.clone()]);

        let mut gm = PetgraphStableDiGraphManager::<()>::default();
        gm.initialize_graph(&market.component_topology());

        let graph = gm.graph();
        let token_map: HashMap<NodeIndex, Token> = graph
            .node_indices()
            .filter_map(|n| {
                market.get_token(&graph[n]).cloned().map(|t| (n, t))
            })
            .collect();

        // Reference a non-existent token address (0xFF)
        let bad_addr = addr(0xFF);
        let candidate = CycleCandidate {
            edges: vec![
                (tok_a.address.clone(), bad_addr.clone(), "pool_ab".into()),
                (bad_addr.clone(), tok_a.address.clone(), "pool_xx".into()),
            ],
            relaxation_amount_out: BigUint::from(100u64),
            _layer: 2,
        };

        let seed = BigUint::from(1000u64);
        let gas_price = BigUint::from(1u64);
        let gas_num = BigUint::from(1u64);
        let gas_den = BigUint::from(1u64);

        let result = optimize_amount(
            &candidate,
            graph,
            &market,
            &token_map,
            &gas_price,
            &gas_num,
            &gas_den,
            &seed,
            0.001,
            30,
        );

        assert!(
            result.is_none(),
            "should return None when edge addresses are not in graph"
        );
    }
}


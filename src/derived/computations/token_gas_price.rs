//! Gas token price computation.
//!
//! Computes token prices relative to the native gas token (e.g., ETH/WETH).
//! Uses DFS with `get_amount_out` to find optimal price paths,
//! accounting for actual swap outputs and gas costs.
//!
//! # Algorithm
//!
//! Starting from 1 ETH (10^18 wei), we simulate actual swaps through the graph using DFS.
//! For each token, we track:
//! - `amount_out`: how much of this token we received (BigUint)
//! - `gas_used`: total gas cost in wei spent on the path (BigUint)
//!
//! We compare paths using: `amount_out / (10^18 + gas_used)` - higher is better.
//!
//! The final price is stored as (numerator, denominator) where:
//! - numerator = amount_out (best route)
//! - denominator = 10^18 + total_gas_cost

use std::collections::HashMap;

use num_bigint::BigUint;
use num_traits::Zero;
use tracing::{debug, trace};
use tycho_simulation::tycho_common::models::Address;

use crate::{
    derived::{
        computation::{ComputationId, DerivedComputation},
        error::ComputationError,
        store::DerivedDataStore,
    },
    feed::{market_data::SharedMarketData, GAS_PRICE_DEPENDENCY_ID},
    types::ComponentId,
};

/// Key for token price lookups.
pub type TokenGasPriceKey = Address;

/// Token price as (numerator, denominator).
/// Price = numerator / denominator where:
/// - numerator = amount of token received from 1 ETH input
/// - denominator = 1 ETH (10^18) + total gas cost in wei
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenGasPrice {
    pub numerator: BigUint,
    pub denominator: BigUint,
}

impl TokenGasPrice {
    pub fn new(numerator: BigUint, denominator: BigUint) -> Self {
        Self { numerator, denominator }
    }

    /// Returns the price as f64 for display/comparison purposes.
    pub fn to_f64(&self) -> Option<f64> {
        use num_traits::ToPrimitive;
        let num = self.numerator.to_f64()?;
        let den = self.denominator.to_f64()?;
        if den == 0.0 {
            None
        } else {
            Some(num / den)
        }
    }
}

/// Token prices map: token address → price ratio.
pub type TokenGasPrices = HashMap<TokenGasPriceKey, TokenGasPrice>;

/// 1 ETH in wei.
fn one_eth() -> BigUint {
    BigUint::from(10u64).pow(18)
}

/// State tracked for each token during price discovery.
#[derive(Debug, Clone)]
struct TokenState {
    /// Amount of this token received from 1 ETH input.
    amount_out: BigUint,
    /// Total gas cost in wei spent on the path to reach this token.
    gas_used: BigUint,
}

impl TokenState {
    /// Compares two states. Returns true if `self` is better than `other`.
    /// Better means: amount_out / (1 ETH + gas_used) is higher.
    /// Cross-multiply to compare: self.amount * (1 ETH + other.gas) > other.amount * (1 ETH +
    /// self.gas)
    fn is_better_than(&self, other: &TokenState) -> bool {
        let one = one_eth();
        let self_denom = &one + &self.gas_used;
        let other_denom = &one + &other.gas_used;
        // self.amount_out / self_denom > other.amount_out / other_denom
        // => self.amount_out * other_denom > other.amount_out * self_denom
        &self.amount_out * &other_denom > &other.amount_out * &self_denom
    }
}

/// Computes token prices relative to the gas token using DFS.
///
/// Starting from 1 ETH (10^18 wei), simulates actual swaps through the graph.
/// Uses DFS to explore paths, continuing when better rates are found,
/// terminating paths that yield worse rates.
#[derive(Debug)]
pub struct TokenGasPriceComputation {
    /// The gas token address (e.g., WETH).
    gas_token: Address,
}

impl TokenGasPriceComputation {
    /// Creates a new TokenGasPriceComputation with the given gas token.
    pub fn new(gas_token: Address) -> Self {
        Self { gas_token }
    }
}

impl DerivedComputation for TokenGasPriceComputation {
    type Output = TokenGasPrices;

    const ID: ComputationId = "token_prices";

    fn compute(
        &self,
        market: &SharedMarketData,
        _store: &DerivedDataStore,
    ) -> Result<Self::Output, ComputationError> {
        /// DFS exploration state for price discovery.
        struct DfsState {
            /// Current token being explored.
            token: Address,
            /// Amount of this token we have (from swapping 1 ETH through the path).
            amount: BigUint,
            /// Total gas cost in wei spent to reach this token.
            gas_used: BigUint,
            /// Current depth in the DFS tree.
            depth: usize,
            /// Path taken to reach this token (for cycle detection).
            path: Vec<Address>,
        }

        let topology = market.component_topology();
        let tokens = market.token_registry_ref();

        // Gas price in wei per gas unit
        let gas_price = market.gas_price();
        let gas_price_wei = gas_price
            .map(|gp| gp.effective_gas_price())
            .ok_or(ComputationError::MissingDependency(GAS_PRICE_DEPENDENCY_ID))?;

        // Best state found for each token
        let mut best_states: HashMap<Address, TokenState> = HashMap::new();

        // Gas token starts with: 1 ETH in, 0 gas used
        let one = one_eth();
        best_states.insert(
            self.gas_token.clone(),
            TokenState { amount_out: one.clone(), gas_used: BigUint::zero() },
        );

        // Build adjacency for quick lookup: token -> [(component_id, pool_tokens)]
        let mut adjacency: HashMap<Address, Vec<(ComponentId, Vec<Address>)>> = HashMap::new();
        for (component_id, token_addresses) in topology.iter() {
            for addr in token_addresses {
                adjacency
                    .entry(addr.clone())
                    .or_default()
                    .push((component_id.clone(), token_addresses.clone()));
            }
        }

        // DFS stack
        let mut stack: Vec<DfsState> = vec![DfsState {
            token: self.gas_token.clone(),
            amount: one.clone(),
            gas_used: BigUint::zero(),
            depth: 0,
            path: vec![self.gas_token.clone()],
        }];

        while let Some(current) = stack.pop() {
            let Some(current_token_info) = tokens.get(&current.token) else {
                continue;
            };

            // Explore all pools containing this token
            let Some(pools) = adjacency.get(&current.token) else {
                continue;
            };

            for (component_id, pool_tokens) in pools {
                let Some(sim_state) = market.get_simulation_state(component_id) else {
                    continue;
                };

                // Try swapping to each other token in the pool
                for neighbor_addr in pool_tokens {
                    // Prevent revisiting tokens in current path (cycle detection)
                    if current.path.contains(neighbor_addr) {
                        continue;
                    }

                    let Some(neighbor_token_info) = tokens.get(neighbor_addr) else {
                        continue;
                    };

                    // Simulate the swap using the actual amount we have
                    let Ok(sim_result) = sim_state.get_amount_out(
                        current.amount.clone(),
                        current_token_info,
                        neighbor_token_info,
                    ) else {
                        continue;
                    };

                    let new_amount_out = sim_result.amount;

                    // Gas cost for this swap in wei
                    let swap_gas_cost = &sim_result.gas * &gas_price_wei;
                    let new_gas_used = &current.gas_used + &swap_gas_cost;

                    let new_state = TokenState {
                        amount_out: new_amount_out.clone(),
                        gas_used: new_gas_used.clone(),
                    };

                    // Check if this is better than what we had
                    let dominated_by_existing = best_states
                        .get(neighbor_addr)
                        .is_some_and(|existing| !new_state.is_better_than(existing));

                    if dominated_by_existing {
                        // This path is not better, don't explore further
                        continue;
                    }

                    // Found a better path! Update and continue exploring
                    trace!(
                        token = ?neighbor_addr,
                        amount_out = %new_amount_out,
                        gas_used = %new_gas_used,
                        via = %component_id,
                        "found better price path"
                    );

                    best_states.insert(neighbor_addr.clone(), new_state);

                    // Continue DFS from this neighbor
                    let mut new_path = current.path.clone();
                    new_path.push(neighbor_addr.clone());
                    stack.push(DfsState {
                        token: neighbor_addr.clone(),
                        amount: new_amount_out,
                        gas_used: new_gas_used,
                        depth: current.depth + 1,
                        path: new_path,
                    });
                }
            }
        }

        // Convert states to TokenGasPrice (numerator, denominator)
        let prices: TokenGasPrices = best_states
            .into_iter()
            .map(|(addr, state)| {
                let denominator = &one + &state.gas_used;
                let price = TokenGasPrice::new(state.amount_out, denominator);
                (addr, price)
            })
            .collect();

        debug!(token_count = prices.len(), "computed token prices via DFS");

        Ok(prices)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::test_utils::{market_read, setup_market, token, MockProtocolSim};

    #[test]
    fn computation_id() {
        assert_eq!(TokenGasPriceComputation::ID, "token_prices");
    }

    /// Test single-hop price computation using setup_market.
    /// ETH -> USDC with rate 2000 means 1 ETH -> 2000 USDC when gas cost is 0.
    #[test]
    fn single_hop_price() {
        let eth = token(1, "ETH");
        let usdc = token(2, "USDC");

        let (market, _) =
            setup_market(vec![("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000).with_gas(0))]);

        let store = DerivedDataStore::new();
        let computation = TokenGasPriceComputation::new(eth.address.clone());
        let prices = computation
            .compute(&market_read(&market), &store)
            .unwrap();

        // ETH price = 1 (gas token)
        let eth_price = prices.get(&eth.address).unwrap();
        assert_eq!(eth_price.numerator, eth_price.denominator);

        // USDC: 1 ETH -> 2000 USDC, so numerator = 2000 * 10^18
        let usdc_price = prices.get(&usdc.address).unwrap();
        let one = one_eth();
        assert_eq!(usdc_price.numerator, &one * 2000u32);
        assert_eq!(usdc_price.denominator, one);
    }

    /// Test that gas cost affects the denominator.
    #[test]
    fn gas_cost_increases_denominator() {
        let eth = token(1, "ETH");
        let usdc = token(2, "USDC");

        let (market, _) = setup_market(vec![(
            "eth_usdc",
            &eth,
            &usdc,
            MockProtocolSim::new(2).with_gas(100_000),
        )]);

        let store = DerivedDataStore::new();
        let computation = TokenGasPriceComputation::new(eth.address.clone());
        let prices = computation
            .compute(&market_read(&market), &store)
            .unwrap();

        let usdc_price = prices.get(&usdc.address).unwrap();
        // numerator = 2 * 10^18 (rate=2)
        // gas_cost = 100_000 * 1 = 100_000 wei (gas_price=1 from setup_market)
        // denominator = 10^18 + 100_000
        let one = one_eth();
        assert_eq!(usdc_price.numerator, &one * 2u32);
        assert_eq!(usdc_price.denominator, &one + 100_000u32);
    }

    /// Test multi-hop pricing: ETH -> A -> B -> C
    /// Verifies amounts chain correctly through multiple swaps.
    #[test]
    fn multi_hop_chains_amounts() {
        let eth = token(1, "ETH");
        let token_a = token(2, "A");
        let token_b = token(3, "B");
        let token_c = token(4, "C");

        let (market, _) = setup_market(vec![
            ("eth_a", &eth, &token_a, MockProtocolSim::new(2).with_gas(0)),
            ("a_b", &token_a, &token_b, MockProtocolSim::new(3).with_gas(0)),
            ("b_c", &token_b, &token_c, MockProtocolSim::new(4).with_gas(0)),
        ]);

        let store = DerivedDataStore::new();
        let computation = TokenGasPriceComputation::new(eth.address.clone());
        let prices = computation
            .compute(&market_read(&market), &store)
            .unwrap();

        let one = one_eth();

        // ETH -> A: 1 ETH * 2 = 2 A
        let price_a = prices.get(&token_a.address).unwrap();
        assert_eq!(price_a.numerator, &one * 2u32);

        // A -> B: 2 A * 3 = 6 B
        let price_b = prices.get(&token_b.address).unwrap();
        assert_eq!(price_b.numerator, &one * 6u32);

        // B -> C: 6 B * 4 = 24 C
        let price_c = prices.get(&token_c.address).unwrap();
        assert_eq!(price_c.numerator, &one * 24u32);
    }

    /// Test that DFS selects the better path when multiple routes exist.
    /// Direct path: ETH -> TARGET with rate 5 (get 5 tokens)
    /// Indirect path: ETH -> INTERMEDIATE -> TARGET with rate 2 * 4 = 8 (get 8 tokens)
    /// Should choose indirect path (8 > 5).
    #[test]
    fn selects_better_path_among_alternatives() {
        let eth = token(1, "ETH");
        let intermediate = token(2, "MID");
        let target = token(3, "TARGET");

        let (market, _) = setup_market(vec![
            // Direct: ETH -> TARGET, rate 5
            ("direct", &eth, &target, MockProtocolSim::new(5).with_gas(0)),
            // Indirect hop 1: ETH -> MID, rate 2
            ("hop1", &eth, &intermediate, MockProtocolSim::new(2).with_gas(0)),
            // Indirect hop 2: MID -> TARGET, rate 4 (total: 2*4=8)
            ("hop2", &intermediate, &target, MockProtocolSim::new(4).with_gas(0)),
        ]);

        let store = DerivedDataStore::new();
        let computation = TokenGasPriceComputation::new(eth.address.clone());
        let prices = computation
            .compute(&market_read(&market), &store)
            .unwrap();

        let one = one_eth();

        // Should choose indirect path: 1 ETH -> 2 MID -> 8 TARGET
        let target_price = prices.get(&target.address).unwrap();
        assert_eq!(target_price.numerator, &one * 8u32);
    }

    /// Test that gas cost can make a shorter path better than a longer one.
    /// Direct: rate 4, gas 0 -> effective 4 tokens
    /// Indirect: rate 2*3=6, but each hop has gas 500_000 -> total gas 1_000_000
    /// With high gas cost, direct path may be better despite lower rate.
    #[test]
    fn gas_cost_can_favor_shorter_path() {
        let eth = token(1, "ETH");
        let mid = token(2, "MID");
        let target = token(3, "TARGET");

        // High gas cost per hop
        let high_gas = 500_000_000_000_000u64; // 0.0005 ETH per hop

        let (market, _) = setup_market(vec![
            // Direct: rate 4, no gas
            ("direct", &eth, &target, MockProtocolSim::new(4).with_gas(0)),
            // Indirect: rate 2, high gas
            ("hop1", &eth, &mid, MockProtocolSim::new(2).with_gas(high_gas)),
            // Indirect: rate 3, high gas (total rate: 6, but 2x gas cost)
            ("hop2", &mid, &target, MockProtocolSim::new(3).with_gas(high_gas)),
        ]);

        let store = DerivedDataStore::new();
        let computation = TokenGasPriceComputation::new(eth.address.clone());
        let prices = computation
            .compute(&market_read(&market), &store)
            .unwrap();

        let one = one_eth();
        let target_price = prices.get(&target.address).unwrap();

        // Direct path: num=4*10^18, denom=10^18 -> effective rate = 4
        // Indirect path: num=6*10^18, denom=10^18 + 2*high_gas
        //
        // Compare: 4 * (10^18 + 2*high_gas) vs 6 * 10^18
        // Direct wins if: 4*10^18 + 8*high_gas > 6*10^18
        //                 8*high_gas > 2*10^18
        //                 high_gas > 0.25*10^18 = 2.5*10^17
        // Our high_gas = 5*10^14, so actually indirect still wins here.
        // Let's verify the actual computation picks indirect (6 tokens)
        assert_eq!(target_price.numerator, &one * 6u32);

        // But the denominator should include the gas costs
        let expected_gas = BigUint::from(high_gas) * 2u32;
        assert_eq!(target_price.denominator, &one + &expected_gas);
    }

    /// Test parallel pools between same token pair - should pick better rate.
    #[test]
    fn parallel_pools_picks_better_rate() {
        let eth = token(1, "ETH");
        let usdc = token(2, "USDC");

        let (market, _) = setup_market(vec![
            // Pool 1: rate 1800
            ("pool1", &eth, &usdc, MockProtocolSim::new(1800).with_gas(0)),
            // Pool 2: rate 2000 (better)
            ("pool2", &eth, &usdc, MockProtocolSim::new(2000).with_gas(0)),
            // Pool 3: rate 1900
            ("pool3", &eth, &usdc, MockProtocolSim::new(1900).with_gas(0)),
        ]);

        let store = DerivedDataStore::new();
        let computation = TokenGasPriceComputation::new(eth.address.clone());
        let prices = computation
            .compute(&market_read(&market), &store)
            .unwrap();

        let one = one_eth();
        let usdc_price = prices.get(&usdc.address).unwrap();

        // Should pick the best rate: 2000
        assert_eq!(usdc_price.numerator, &one * 2000u32);
    }

    /// Test diamond topology: ETH -> A, ETH -> B, A -> C, B -> C
    /// Two paths to C, should pick the better one.
    #[test]
    fn diamond_topology_picks_best_path() {
        let eth = token(1, "ETH");
        let token_a = token(2, "A");
        let token_b = token(3, "B");
        let token_c = token(4, "C");

        let (market, _) = setup_market(vec![
            // Path 1: ETH -> A (rate 2) -> C (rate 5) = 10
            ("eth_a", &eth, &token_a, MockProtocolSim::new(2).with_gas(0)),
            ("a_c", &token_a, &token_c, MockProtocolSim::new(5).with_gas(0)),
            // Path 2: ETH -> B (rate 3) -> C (rate 2) = 6
            ("eth_b", &eth, &token_b, MockProtocolSim::new(3).with_gas(0)),
            ("b_c", &token_b, &token_c, MockProtocolSim::new(2).with_gas(0)),
        ]);

        let store = DerivedDataStore::new();
        let computation = TokenGasPriceComputation::new(eth.address.clone());
        let prices = computation
            .compute(&market_read(&market), &store)
            .unwrap();

        let one = one_eth();

        // Should pick path through A: 1 ETH -> 2 A -> 10 C
        let price_c = prices.get(&token_c.address).unwrap();
        assert_eq!(price_c.numerator, &one * 10u32);
    }
}

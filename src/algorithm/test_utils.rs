//! Shared test utilities for algorithm tests.

use std::collections::HashMap;

use chrono::NaiveDateTime;
use num_bigint::BigUint;
use tycho_simulation::tycho_core::{
    dto::ProtocolStateDelta,
    models::{protocol::ProtocolComponent, token::Token, Address, Chain},
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
    Bytes,
};

use crate::{
    feed::market_data::{ComponentData, SharedMarketData},
    graph::{
        petgraph::{EdgeWeight, PetgraphStableDiGraphManager},
        GraphManager,
    },
    types::{solution::OrderSide, ComponentId, Order},
};

/// Use amounts in wei scale (10^18) to exceed gas costs in tests.
pub const ONE_ETH: u128 = 1_000_000_000_000_000_000;

// ==================== Mock ProtocolSim ====================

/// Mock ProtocolSim that multiplies input by a configurable factor.
///
/// Each call to `get_amount_out` returns a new state with an incremented multiplier,
/// simulating liquidity changes after a swap. This allows testing state override logic
/// when the same pool is used multiple times in a path.
///
/// Optionally supports liquidity limits - if `liquidity` is set, swaps exceeding it fail.
#[derive(Debug, Clone)]
pub struct MockProtocolSim {
    /// Output = input * multiplier
    pub multiplier: u32,
    /// Gas to report for each swap
    pub gas: u64,
    /// Optional liquidity limit - if amount_in exceeds this, simulation fails
    pub liquidity: Option<BigUint>,
}

impl MockProtocolSim {
    pub fn new(multiplier: u32) -> Self {
        Self { multiplier, gas: 100_000, liquidity: None }
    }

    pub fn with_gas(mut self, gas: u64) -> Self {
        self.gas = gas;
        self
    }

    pub fn with_liquidity(mut self, liquidity: u128) -> Self {
        self.liquidity = Some(BigUint::from(liquidity));
        self
    }
}

impl ProtocolSim for MockProtocolSim {
    fn fee(&self) -> f64 {
        0.003
    }

    fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
        Ok(self.multiplier as f64)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        _token_in: &Token,
        _token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        // Check liquidity limit if set
        if let Some(ref liq) = self.liquidity {
            if amount_in > *liq {
                return Err(SimulationError::InvalidInput(
                    "amount exceeds available liquidity".to_string(),
                    None,
                ));
            }
        }

        let amount_out = &amount_in * self.multiplier;
        // Return new state with incremented multiplier to simulate state change
        let new_state = Box::new(MockProtocolSim {
            multiplier: self.multiplier + 1,
            gas: self.gas,
            liquidity: self.liquidity.clone(),
        });
        Ok(GetAmountOutResult::new(amount_out, BigUint::from(self.gas), new_state))
    }

    fn get_limits(
        &self,
        _sell_token: Bytes,
        _buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        Ok((BigUint::from(u64::MAX), BigUint::from(u64::MAX)))
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError<String>> {
        Ok(())
    }

    fn clone_box(&self) -> Box<dyn ProtocolSim> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn eq(&self, other: &dyn ProtocolSim) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .map(|o| o.multiplier == self.multiplier)
            .unwrap_or(false)
    }
}

// ==================== Test Fixtures ====================

/// Creates a test token with the given address byte and symbol.
pub fn token(addr: u8, symbol: &str) -> Token {
    Token {
        address: Address::from([addr; 20]),
        symbol: symbol.to_string(),
        decimals: 18,
        tax: Default::default(),
        gas: vec![],
        chain: Chain::Ethereum,
        quality: 100,
    }
}

/// Creates a test ProtocolComponent with the given ID and tokens.
pub fn component(id: &str, tokens: &[Token]) -> ProtocolComponent {
    ProtocolComponent::new(
        id,
        "uniswap_v2",
        "swap",
        Chain::Ethereum,
        tokens
            .iter()
            .map(|t| t.address.clone())
            .collect(),
        vec![],
        HashMap::new(),
        Default::default(),
        Default::default(),
        NaiveDateTime::default(),
    )
}

/// Creates a sell order for testing.
pub fn sell_order(token_in: &Token, token_out: &Token, amount: u128) -> Order {
    Order {
        id: "test-order".to_string(),
        token_in: token_in.address.clone(),
        token_out: token_out.address.clone(),
        amount: BigUint::from(amount),
        side: OrderSide::Sell,
        sender: Address::default(),
        receiver: None,
    }
}

/// Creates a buy order for testing (exact-out).
pub fn buy_order(token_in: &Token, token_out: &Token, amount: u128) -> Order {
    Order {
        id: "test-order".to_string(),
        token_in: token_in.address.clone(),
        token_out: token_out.address.clone(),
        amount: BigUint::from(amount),
        side: OrderSide::Buy,
        sender: Address::default(),
        receiver: None,
    }
}

/// Sets up market with components and a graph. Returns (market, graph_manager).
///
/// Pools map ComponentId to (tokens, multiplier).
pub fn setup_market(
    pools: HashMap<ComponentId, (Vec<Token>, u32)>,
) -> (SharedMarketData, PetgraphStableDiGraphManager) {
    let mut market = SharedMarketData::new();
    let mut topology = HashMap::new();

    for (pool_id, (tokens, multiplier)) in pools {
        let comp = component(&pool_id, &tokens);
        let state = Box::new(MockProtocolSim::new(multiplier));
        let data = ComponentData { component: comp, state, tokens: tokens.clone() };

        topology.insert(
            pool_id,
            tokens
                .iter()
                .map(|t| t.address.clone())
                .collect(),
        );
        market.insert_component(data);
    }

    let mut graph_manager = PetgraphStableDiGraphManager::default();
    graph_manager.initialize_graph(&topology);

    (market, graph_manager)
}

/// Sets up market with components, graph, AND edge weights in one call.
///
/// Each pool is defined as (pool_id, tokens, multiplier, depth).
/// The spot_price for edge weights is derived from the multiplier.
/// Edge weights are set for the forward direction (tokens[0] -> tokens[1]).
pub fn setup_market_with_weights(
    pools: Vec<(&str, Vec<Token>, u32, f64)>,
) -> (SharedMarketData, PetgraphStableDiGraphManager) {
    let mut market = SharedMarketData::new();
    let mut topology = HashMap::new();
    let mut weights_to_set = Vec::new();

    for (pool_id, tokens, multiplier, depth) in pools {
        let comp = component(pool_id, &tokens);
        let state = Box::new(MockProtocolSim::new(multiplier));
        let data = ComponentData { component: comp, state, tokens: tokens.clone() };

        topology.insert(
            pool_id.to_string(),
            tokens
                .iter()
                .map(|t| t.address.clone())
                .collect(),
        );
        market.insert_component(data);

        if tokens.len() >= 2 {
            weights_to_set.push((
                pool_id.to_string(),
                tokens[0].address.clone(),
                tokens[1].address.clone(),
                multiplier as f64, // spot_price derived from multiplier
                depth,
            ));
        }
    }

    let mut graph_manager = PetgraphStableDiGraphManager::default();
    graph_manager.initialize_graph(&topology);

    // Set edge weights
    for (pool_id, from, to, spot_price, depth) in weights_to_set {
        let weight = EdgeWeight::new(spot_price, depth, 0.003);
        let _ = graph_manager.set_edge_weight(&pool_id, &from, &to, weight, false);
    }

    (market, graph_manager)
}

/// Adds a component to market without setting edge weights.
/// Useful for testing scenarios with missing weights.
pub fn add_component_to_market(
    market: &mut SharedMarketData,
    pool_id: &str,
    tokens: Vec<Token>,
    multiplier: u32,
) {
    let comp = component(pool_id, &tokens);
    let state = Box::new(MockProtocolSim::new(multiplier));
    let data = ComponentData { component: comp, state, tokens };
    market.insert_component(data);
}

/// Adds a component with limited liquidity to market.
/// Useful for testing insufficient liquidity scenarios.
pub fn add_component_with_liquidity(
    market: &mut SharedMarketData,
    pool_id: &str,
    tokens: Vec<Token>,
    multiplier: u32,
    liquidity: u128,
) {
    let comp = component(pool_id, &tokens);
    let state = Box::new(MockProtocolSim::new(multiplier).with_liquidity(liquidity));
    let data = ComponentData { component: comp, state, tokens };
    market.insert_component(data);
}

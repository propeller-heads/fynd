//! Shared test utilities for algorithm tests.

use std::{collections::HashMap, sync::Arc};

use chrono::NaiveDateTime;
use num_bigint::BigUint;
use tokio::sync::RwLock;
use tycho_simulation::{
    tycho_core::{
        dto::ProtocolStateDelta,
        models::{protocol::ProtocolComponent, token::Token, Address, Chain},
        simulation::{
            errors::{SimulationError, TransitionError},
            protocol_sim::{
                Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams,
                SwapConstraint,
            },
        },
        Bytes,
    },
    tycho_ethereum::gas::{BlockGasPrice, GasPrice},
};

use crate::{
    algorithm::most_liquid::DepthAndPrice,
    feed::market_data::SharedMarketData,
    graph::{petgraph::PetgraphStableDiGraphManager, GraphManager},
    types::{solution::OrderSide, BlockInfo, Order},
};

/// Use amounts in wei scale (10^18) to exceed gas costs in tests.
pub const ONE_ETH: u128 = 1_000_000_000_000_000_000;

// ==================== Mock ProtocolSim ====================

/// Mock ProtocolSim that multiplies input by a configurable factor.
///
/// Each call to `get_amount_out` returns a new state with an incremented spot_price,
/// simulating liquidity changes after a swap. This allows testing state override logic
/// when the same pool is used multiple times in a path.
// TODO: Consider moving MockProtocolSim to the tycho-common
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MockProtocolSim {
    /// Pre-fee exchange rate from smaller-address token to larger-address token
    /// (e.g., if token A address < token B address, amount_B = amount_A * spot_price * (1 - fee))
    pub spot_price: f64,
    /// Gas to report for each swap
    pub gas: u64,
    /// Liquidity limit
    pub liquidity: u128,
    /// Fee percentage
    pub fee: f64,
    /// Token decimals by address, used for decimal-aware get_limits scaling.
    /// When empty, get_limits assumes equal decimals (backward-compatible).
    #[serde(default)]
    pub token_decimals: HashMap<Bytes, u32>,
}

impl MockProtocolSim {
    pub fn new(spot_price: f64) -> Self {
        Self { spot_price, ..Default::default() }
    }

    pub fn with_spot_price(mut self, spot_price: f64) -> Self {
        self.spot_price = spot_price;
        self
    }

    pub fn with_gas(mut self, gas: u64) -> Self {
        self.gas = gas;
        self
    }

    pub fn with_liquidity(mut self, liquidity: u128) -> Self {
        self.liquidity = liquidity;
        self
    }

    pub fn with_fee(mut self, fee: f64) -> Self {
        self.fee = fee;
        self
    }

    pub fn with_tokens(mut self, tokens: &[Token]) -> Self {
        for token in tokens {
            self.token_decimals
                .insert(token.address.clone(), token.decimals);
        }
        self
    }
}

impl Default for MockProtocolSim {
    fn default() -> Self {
        Self {
            spot_price: 2.0,
            gas: 50_000,
            liquidity: u128::MAX,
            fee: 0.0,
            token_decimals: HashMap::new(),
        }
    }
}

#[typetag::serde]
impl ProtocolSim for MockProtocolSim {
    fn fee(&self) -> f64 {
        self.fee
    }

    /// Returns a direction-dependent spot price with fee markup applied.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let post_fee_spot_price = self.spot_price / (1.0 - self.fee);
        if base.address < quote.address {
            Ok(post_fee_spot_price)
        } else {
            Ok(1.0 / post_fee_spot_price)
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        // amount_out = amount_in * effective_price * 10^(dec_out - dec_in)
        // effective_price includes direction and fee
        const PRECISION: u64 = 1_000_000_000_000;
        let effective_price = if token_in.address < token_out.address {
            self.spot_price * (1.0 - self.fee)
        } else {
            (1.0 - self.fee) / self.spot_price
        };
        let price_scaled = (effective_price * PRECISION as f64) as u128;
        let amount_out = (&amount_in * price_scaled) / PRECISION;

        let decimal_diff = token_out.decimals as i32 - token_in.decimals as i32;
        let amount_out = if decimal_diff >= 0 {
            amount_out * BigUint::from(10u64).pow(decimal_diff as u32)
        } else {
            amount_out / BigUint::from(10u64).pow((-decimal_diff) as u32)
        };

        // Check liquidity limit against output (liquidity = max receivable tokens)
        if amount_out > BigUint::from(self.liquidity) {
            return Err(SimulationError::InvalidInput(
                "amount exceeds available liquidity".to_string(),
                None,
            ));
        }

        // Return new state with incremented spot_price to simulate state change
        let new_state = Box::new(MockProtocolSim {
            spot_price: self.spot_price + 1.0,
            gas: self.gas,
            liquidity: self.liquidity,
            fee: self.fee,
            token_decimals: self.token_decimals.clone(),
        });
        Ok(GetAmountOutResult::new(amount_out, BigUint::from(self.gas), new_state))
    }

    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        // liquidity represents max amount of tokens we can receive (max output)
        let buy_limit = BigUint::from(self.liquidity);

        const PRECISION: u64 = 1_000_000_000_000;
        let effective_price = if sell_token < buy_token {
            self.spot_price * (1.0 - self.fee)
        } else {
            (1.0 - self.fee) / self.spot_price
        };
        let price_scaled = (effective_price * PRECISION as f64) as u128;
        let sell_limit = (&buy_limit * PRECISION) / price_scaled;

        // Scale sell_limit by 10^(sell_dec - buy_dec) to convert from buy-token-scale
        // to sell-token-scale. Only applies when token decimals are known.
        let sell_limit = if let (Some(&sell_dec), Some(&buy_dec)) =
            (self.token_decimals.get(&sell_token), self.token_decimals.get(&buy_token))
        {
            let decimal_diff = sell_dec as i32 - buy_dec as i32;
            if decimal_diff >= 0 {
                sell_limit * BigUint::from(10u64).pow(decimal_diff as u32)
            } else {
                sell_limit / BigUint::from(10u64).pow((-decimal_diff) as u32)
            }
        } else {
            sell_limit
        };

        Ok((sell_limit, buy_limit))
    }

    fn query_pool_swap(&self, params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        let token_in = params.token_in();
        let token_out = params.token_out();

        match params.swap_constraint() {
            SwapConstraint::TradeLimitPrice { .. } => {
                let (sell_limit, _) =
                    self.get_limits(token_in.address.clone(), token_out.address.clone())?;
                let result = self.get_amount_out(sell_limit.clone(), token_in, token_out)?;
                Ok(PoolSwap::new(sell_limit, result.amount, result.new_state, None))
            }
            _ => Err(SimulationError::InvalidInput(
                "MockProtocolSim only supports TradeLimitPrice".to_string(),
                None,
            )),
        }
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError<String>> {
        unimplemented!("delta_transition not implemented in MockProtocolSim")
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
            .map(|o| (o.spot_price - self.spot_price).abs() < f64::EPSILON)
            .unwrap_or(false)
    }
}

// ==================== Test Fixtures ====================

/// Creates an address from a single byte (fills all 20 bytes with that value).
///
/// # Example
/// ```ignore
/// let a = addr(0x0A); // 0x0A0A0A0A...0A0A (20 bytes)
/// ```
pub fn addr(b: u8) -> Address {
    Address::from([b; 20])
}

/// Creates a test token with the given address byte and symbol (18 decimals).
pub fn token(addr_b: u8, symbol: &str) -> Token {
    token_with_decimals(addr_b, symbol, 18)
}

pub fn token_with_decimals(addr_b: u8, symbol: &str, decimals: u32) -> Token {
    Token {
        address: addr(addr_b),
        symbol: symbol.to_string(),
        decimals,
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

/// Creates an order for testing.
pub fn order(token_in: &Token, token_out: &Token, amount: u128, side: OrderSide) -> Order {
    Order::new(
        token_in.address.clone(),
        token_out.address.clone(),
        BigUint::from(amount),
        side,
        Address::default(),
    )
    .with_id("test-order".to_string())
}

/// Sets up market with components and a graph. Returns (market_lock, graph_manager).
///
/// The market is wrapped in `Arc<RwLock<...>>` for use with `find_best_route`.
/// Use `market_read(&market_lock)` to get a `SharedMarketData` for other tests.
pub fn setup_market(
    pools: Vec<(&str, &Token, &Token, MockProtocolSim)>,
) -> (Arc<RwLock<SharedMarketData>>, PetgraphStableDiGraphManager<DepthAndPrice>) {
    let mut market = SharedMarketData::new();
    let mut component_weights = HashMap::new();

    // Set gas_price = 1 wei/gas for simple calculations
    market.update_gas_price(BlockGasPrice {
        block_number: 1,
        block_hash: Default::default(),
        block_timestamp: 0,
        pricing: GasPrice::Legacy { gas_price: BigUint::from(100u64) },
    });
    market.update_last_updated(BlockInfo::new(1, "0x00".into(), 0));

    for (pool_id, token_in, token_out, state) in pools {
        let tokens = vec![token_in.clone(), token_out.clone()];
        let comp = component(pool_id, &tokens);
        let weight_to = DepthAndPrice::from_protocol_sim(&state, token_in, token_out).unwrap();
        let weight_from = DepthAndPrice::from_protocol_sim(&state, token_out, token_in).unwrap();

        // Insert component, state, and tokens separately using new API
        market.upsert_components(std::iter::once(comp));
        market.update_states([(pool_id.to_string(), Box::new(state) as Box<dyn ProtocolSim>)]);
        market.upsert_tokens(tokens);

        component_weights.insert(pool_id, (token_in, token_out, weight_to, weight_from));
    }

    let mut graph_manager = PetgraphStableDiGraphManager::default();
    graph_manager.initialize_graph(&market.component_topology());

    for (pool_id, (token_in, token_out, weight_to, weight_from)) in component_weights {
        graph_manager
            .set_edge_weight(
                &pool_id.to_string(),
                &token_in.address,
                &token_out.address,
                weight_to,
                false,
            )
            .unwrap();
        graph_manager
            .set_edge_weight(
                &pool_id.to_string(),
                &token_out.address,
                &token_in.address,
                weight_from,
                false,
            )
            .unwrap();
    }

    (Arc::new(RwLock::new(market)), graph_manager)
}

/// Helper to get a read guard for `simulate_path` tests that need `&SharedMarketData`.
/// Returns the guard which derefs to `&SharedMarketData`.
pub fn market_read(
    lock: &Arc<RwLock<SharedMarketData>>,
) -> tokio::sync::RwLockReadGuard<'_, SharedMarketData> {
    lock.try_read()
        .expect("lock should not be contested in test")
}

/// Common fixtures for tests.
pub mod fixtures {
    use super::*;

    /// Creates addresses A, B, C, D for use in graph tests.
    pub fn addrs() -> (Address, Address, Address, Address) {
        (addr(0x0A), addr(0x0B), addr(0x0C), addr(0x0D))
    }

    /// A <-> B <-> C <-> D linear chain (bidirectional).
    pub(crate) fn linear_graph() -> PetgraphStableDiGraphManager<DepthAndPrice> {
        let (a, b, c, d) = addrs();
        let mut m = PetgraphStableDiGraphManager::<DepthAndPrice>::new();
        let mut t = HashMap::new();
        t.insert("ab".into(), vec![a.clone(), b.clone()]);
        t.insert("bc".into(), vec![b.clone(), c.clone()]);
        t.insert("cd".into(), vec![c, d]);
        m.initialize_graph(&t);
        m
    }

    /// 3 parallel pools A<->B, 2 pools B<->C.
    pub(crate) fn parallel_graph() -> PetgraphStableDiGraphManager<DepthAndPrice> {
        let (a, b, c, _) = addrs();
        let mut m = PetgraphStableDiGraphManager::<DepthAndPrice>::new();
        let mut t = HashMap::new();
        t.insert("ab1".into(), vec![a.clone(), b.clone()]);
        t.insert("ab2".into(), vec![a.clone(), b.clone()]);
        t.insert("ab3".into(), vec![a, b.clone()]);
        t.insert("bc1".into(), vec![b.clone(), c.clone()]);
        t.insert("bc2".into(), vec![b, c]);
        m.initialize_graph(&t);
        m
    }

    /// Diamond: A->B->D, A->C->D (two 2-hop paths).
    pub(crate) fn diamond_graph() -> PetgraphStableDiGraphManager<DepthAndPrice> {
        let (a, b, c, d) = addrs();
        let mut m = PetgraphStableDiGraphManager::<DepthAndPrice>::new();
        let mut t = HashMap::new();
        t.insert("ab".into(), vec![a.clone(), b.clone()]);
        t.insert("ac".into(), vec![a, c.clone()]);
        t.insert("bd".into(), vec![b, d.clone()]);
        t.insert("cd".into(), vec![c, d]);
        m.initialize_graph(&t);
        m
    }
}

// ==================== Tests for MockProtocolSim ====================

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    /// Helper to create two tokens with specific address ordering.
    /// Returns (lower_addr_token, higher_addr_token).
    fn ordered_tokens() -> (Token, Token) {
        let token_low = token(0x01, "LOW");
        let token_high = token(0x02, "HIGH");
        assert!(token_low.address < token_high.address);
        (token_low, token_high)
    }

    fn ordered_tokens_with_decimals(dec_low: u32, dec_high: u32) -> (Token, Token) {
        let token_low = token_with_decimals(0x01, "LOW", dec_low);
        let token_high = token_with_decimals(0x02, "HIGH", dec_high);
        assert!(token_low.address < token_high.address);
        (token_low, token_high)
    }

    // ==================== Builder & Default Tests ====================

    #[test]
    fn default_values_are_as_expected() {
        let sim = MockProtocolSim::default();

        assert_eq!(sim.spot_price, 2.0);
        assert_eq!(sim.gas, 50_000);
        assert_eq!(sim.liquidity, u128::MAX);
        assert_eq!(sim.fee, 0.0);
    }

    #[test]
    fn new_sets_spot_price_with_defaults() {
        let sim = MockProtocolSim::new(10.0);

        assert_eq!(sim.spot_price, 10.0);
        assert_eq!(sim.gas, 50_000); // default
        assert_eq!(sim.liquidity, u128::MAX); // default
        assert_eq!(sim.fee, 0.0); // default
    }

    #[test]
    fn builder_methods_chain_correctly() {
        let sim = MockProtocolSim::new(1.0)
            .with_spot_price(5.0)
            .with_gas(100_000)
            .with_liquidity(1_000_000)
            .with_fee(0.003);

        assert_eq!(sim.spot_price, 5.0);
        assert_eq!(sim.gas, 100_000);
        assert_eq!(sim.liquidity, 1_000_000);
        assert_eq!(sim.fee, 0.003);
    }

    // ==================== fee() Tests ====================

    #[test]
    fn fee_returns_configured_value() {
        let sim = MockProtocolSim::default().with_fee(0.003);
        assert_eq!(sim.fee(), 0.003);
    }

    #[test]
    fn fee_returns_zero_by_default() {
        let sim = MockProtocolSim::default();
        assert_eq!(sim.fee(), 0.0);
    }

    // ==================== spot_price() Tests ====================

    #[test]
    fn spot_price_asymmetric_based_on_token_order() {
        let (token_low, token_high) = ordered_tokens();
        let sim = MockProtocolSim::new(4.0); // spot_price = 4, no fee

        // When base < quote: returns spot_price
        let price_low_to_high = sim
            .spot_price(&token_low, &token_high)
            .unwrap();
        assert_eq!(price_low_to_high, 4.0);

        // When base > quote: returns 1/spot_price
        let price_high_to_low = sim
            .spot_price(&token_high, &token_low)
            .unwrap();
        assert_eq!(price_high_to_low, 0.25); // 1/4
    }

    #[test]
    fn spot_price_accounts_for_fee() {
        let (token_low, token_high) = ordered_tokens();
        // spot_price = 2, fee = 50% (0.5) for easy calculation
        // post_fee_spot_price = 2 / (1 - 0.5) = 4
        let sim = MockProtocolSim::new(2.0).with_fee(0.5);

        // When base < quote: returns post_fee_spot_price = 4.0
        let price_low_to_high = sim
            .spot_price(&token_low, &token_high)
            .unwrap();
        assert_eq!(price_low_to_high, 4.0);

        // When base > quote: returns 1/post_fee_spot_price = 1/4 = 0.25
        let price_high_to_low = sim
            .spot_price(&token_high, &token_low)
            .unwrap();
        assert_eq!(price_high_to_low, 0.25);
    }

    // ==================== get_amount_out() Tests ====================

    #[test]
    fn get_amount_out_multiplies_by_spot_price_low_to_high() {
        let (token_low, token_high) = ordered_tokens();
        let sim = MockProtocolSim::new(3.0); // spot_price = 3, no fee

        // When token_in < token_out: amount_out = amount_in * spot_price
        let result = sim
            .get_amount_out(BigUint::from(1000u64), &token_low, &token_high)
            .unwrap();

        assert_eq!(result.amount, BigUint::from(3000u64));
    }

    #[test]
    fn get_amount_out_divides_by_spot_price_high_to_low() {
        let (token_low, token_high) = ordered_tokens();
        let sim = MockProtocolSim::new(4.0); // spot_price = 4, no fee

        // When token_in > token_out: amount_out = amount_in / spot_price
        let result = sim
            .get_amount_out(BigUint::from(1000u64), &token_high, &token_low)
            .unwrap();

        assert_eq!(result.amount, BigUint::from(250u64));
    }

    #[test]
    fn get_amount_out_applies_fee() {
        let (token_low, token_high) = ordered_tokens();
        // spot_price = 2, fee = 10% (0.1)
        // amount_out = amount_in * spot_price * (1 - fee) = 1000 * 2 * 0.9 = 1800
        let sim = MockProtocolSim::new(2.0).with_fee(0.1);

        let result = sim
            .get_amount_out(BigUint::from(1000u64), &token_low, &token_high)
            .unwrap();

        assert_eq!(result.amount, BigUint::from(1800u64));
    }

    #[test]
    fn get_amount_out_returns_configured_gas() {
        let (token_low, token_high) = ordered_tokens();
        let sim = MockProtocolSim::default().with_gas(75_000);

        let result = sim
            .get_amount_out(BigUint::from(1000u64), &token_low, &token_high)
            .unwrap();

        assert_eq!(result.gas, BigUint::from(75_000u64));
    }

    #[test]
    fn get_amount_out_returns_new_state_with_incremented_spot_price() {
        let (token_low, token_high) = ordered_tokens();
        let sim = MockProtocolSim::new(5.0)
            .with_gas(60_000)
            .with_fee(0.01);

        let result = sim
            .get_amount_out(BigUint::from(1000u64), &token_low, &token_high)
            .unwrap();

        // Downcast new_state to verify it's a MockProtocolSim with incremented spot_price
        let new_state = result
            .new_state
            .as_any()
            .downcast_ref::<MockProtocolSim>()
            .unwrap();
        assert_eq!(new_state.spot_price, 6.0); // incremented from 5
        assert_eq!(new_state.gas, 60_000); // preserved
        assert_eq!(new_state.fee, 0.01); // preserved
    }

    #[test]
    fn get_amount_out_fails_when_exceeding_liquidity() {
        let (token_low, token_high) = ordered_tokens();
        let sim = MockProtocolSim::default().with_liquidity(1000);

        let result = sim.get_amount_out(BigUint::from(1001u64), &token_low, &token_high);

        assert!(result.is_err());
        match result {
            Err(SimulationError::InvalidInput(msg, _)) => {
                assert!(msg.contains("liquidity"));
            }
            _ => panic!("Expected InvalidInput error"),
        }
    }

    // ==================== get_limits() Tests ====================

    #[test]
    fn get_limits_returns_liquidity_as_buy_limit() {
        let sim = MockProtocolSim::new(2.0).with_liquidity(10_000);

        let (_, buy_limit) = sim
            .get_limits(Bytes::default(), Bytes::default())
            .unwrap();

        assert_eq!(buy_limit, BigUint::from(10_000u64));
    }

    #[test]
    fn get_limits_calculates_sell_limit_from_buy_limit_and_spot_price() {
        // sell_limit = buy_limit / spot_price (when no fee)
        let sim = MockProtocolSim::new(4.0).with_liquidity(8_000);

        let (sell_limit, buy_limit) = sim
            .get_limits(addr(1), addr(2))
            .unwrap();

        assert_eq!(buy_limit, BigUint::from(8_000u64));
        assert_eq!(sell_limit, BigUint::from(2_000u64)); // 8000 / 4
    }

    #[test]
    fn get_limits_accounts_for_fee_in_sell_limit() {
        // With fee, sell_limit = buy_limit / spot_price / (1 - fee)
        // spot_price = 2, liquidity = 1000, fee = 0.5 (50%)
        // sell_limit = 1000 / 2 / 0.5 = 1000
        let sim = MockProtocolSim::new(2.0)
            .with_liquidity(1_000)
            .with_fee(0.5);

        let (sell_limit, buy_limit) = sim
            .get_limits(addr(1), addr(2))
            .unwrap();

        assert_eq!(buy_limit, BigUint::from(1_000u64));
        assert_eq!(sell_limit, BigUint::from(1_000u64));
    }

    // ==================== clone_box() & eq() Tests ====================

    #[test]
    fn clone_box_creates_independent_copy() {
        let sim = MockProtocolSim::new(7.0)
            .with_gas(80_000)
            .with_fee(0.02);
        let cloned: Box<dyn ProtocolSim> = sim.clone_box();

        let cloned_mock = cloned
            .as_any()
            .downcast_ref::<MockProtocolSim>()
            .unwrap();
        assert_eq!(cloned_mock.spot_price, 7.0);
        assert_eq!(cloned_mock.gas, 80_000);
        assert_eq!(cloned_mock.fee, 0.02);
    }

    #[test]
    fn eq_compares_spot_price_only() {
        let sim1 = MockProtocolSim::new(5.0).with_gas(100);
        let sim2 = MockProtocolSim::new(5.0).with_gas(200); // different gas, same spot_price
        let sim3 = MockProtocolSim::new(6.0).with_gas(100); // same gas, different spot_price

        assert!(sim1.eq(&sim2)); // same spot_price -> equal
        assert!(!sim1.eq(&sim3)); // different spot_price -> not equal
    }

    // ==================== Mixed-Decimal get_amount_out() Tests ====================

    #[rstest]
    #[case::high_to_low_decimals(18, 6, 2000.0, 1_000_000_000_000_000_000u128, 2_000_000_000u128)]
    #[case::low_to_high_decimals(6, 18, 2000.0, 1_000_000u128, 2_000_000_000_000_000_000_000u128)]
    #[case::same_decimals(
        18,
        18,
        2000.0,
        1_000_000_000_000_000_000u128,
        2_000_000_000_000_000_000_000u128
    )]
    fn get_amount_out_scales_by_decimals(
        #[case] dec_low: u32,
        #[case] dec_high: u32,
        #[case] spot_price: f64,
        #[case] amount_in: u128,
        #[case] expected_out: u128,
    ) {
        let (token_low, token_high) = ordered_tokens_with_decimals(dec_low, dec_high);
        let sim = MockProtocolSim::new(spot_price);

        let result = sim
            .get_amount_out(BigUint::from(amount_in), &token_low, &token_high)
            .unwrap();

        assert_eq!(result.amount, BigUint::from(expected_out));
    }

    #[rstest]
    #[case::high_to_low_decimals(18, 6, 4.0, 4_000_000u128, 1_000_000_000_000_000_000u128)]
    #[case::low_to_high_decimals(6, 18, 4.0, 4_000_000_000_000_000_000u128, 1_000_000u128)]
    #[case::same_decimals(
        18,
        18,
        4.0,
        4_000_000_000_000_000_000u128,
        1_000_000_000_000_000_000u128
    )]
    fn get_amount_out_reverse_direction_scales_by_decimals(
        #[case] dec_low: u32,
        #[case] dec_high: u32,
        #[case] spot_price: f64,
        #[case] amount_in: u128,
        #[case] expected_out: u128,
    ) {
        let (token_low, token_high) = ordered_tokens_with_decimals(dec_low, dec_high);
        let sim = MockProtocolSim::new(spot_price);

        // high→low: amount_out = amount_in / spot_price * 10^(dec_low - dec_high)
        let result = sim
            .get_amount_out(BigUint::from(amount_in), &token_high, &token_low)
            .unwrap();

        assert_eq!(result.amount, BigUint::from(expected_out));
    }

    // ==================== Mixed-Decimal get_limits() Tests ====================

    #[rstest]
    #[case::high_sell_low_buy(18, 6, 4.0, 8_000)]
    #[case::low_sell_high_buy(6, 18, 4.0, 8_000)]
    fn get_limits_scales_sell_limit_by_decimals(
        #[case] sell_dec: u32,
        #[case] buy_dec: u32,
        #[case] spot_price: f64,
        #[case] liquidity: u128,
    ) {
        let sell_token = token_with_decimals(0x01, "SELL", sell_dec);
        let buy_token = token_with_decimals(0x02, "BUY", buy_dec);
        let sim = MockProtocolSim::new(spot_price)
            .with_liquidity(liquidity)
            .with_tokens(&[sell_token.clone(), buy_token.clone()]);

        let (sell_limit, buy_limit) = sim
            .get_limits(sell_token.address, buy_token.address)
            .unwrap();

        assert_eq!(buy_limit, BigUint::from(liquidity));

        let base_sell_limit = BigUint::from((liquidity as f64 / spot_price) as u128);
        let decimal_diff = sell_dec as i32 - buy_dec as i32;
        let expected_sell_limit = if decimal_diff >= 0 {
            base_sell_limit * BigUint::from(10u64).pow(decimal_diff as u32)
        } else {
            base_sell_limit / BigUint::from(10u64).pow((-decimal_diff) as u32)
        };
        assert_eq!(sell_limit, expected_sell_limit);
    }

    #[test]
    fn get_limits_without_token_decimals_assumes_equal_decimals() {
        let sim = MockProtocolSim::new(4.0).with_liquidity(8_000);

        let (sell_limit, _) = sim
            .get_limits(addr(1), addr(2))
            .unwrap();

        assert_eq!(sell_limit, BigUint::from(2_000u64));
    }

    #[test]
    fn get_limits_uses_reciprocal_price_for_reverse_direction() {
        // spot_price=4 means low→high rate is 4, so high→low rate is 1/4
        // sell_limit = buy_limit / ((1-fee)/spot_price) = buy_limit * spot_price / (1-fee)
        let sim = MockProtocolSim::new(4.0).with_liquidity(8_000);

        let (sell_limit, buy_limit) = sim
            .get_limits(addr(2), addr(1))
            .unwrap();

        assert_eq!(buy_limit, BigUint::from(8_000u64));
        assert_eq!(sell_limit, BigUint::from(32_000u64)); // 8000 * 4
    }

    // ==================== Mock vs UniV2 Comparison Test ====================

    #[rstest]
    #[case::same_decimals(18, 18, 1000, 2000)]
    #[case::high_to_low(18, 6, 1000, 2_000_000)]
    #[case::low_to_high(6, 18, 2_000_000, 1000)]
    fn mock_matches_univ2_for_small_swaps(
        #[case] dec_in: u32,
        #[case] dec_out: u32,
        #[case] human_reserve_in: u64,
        #[case] human_reserve_out: u64,
    ) {
        use alloy::primitives::U256;
        use tycho_simulation::evm::protocol::uniswap_v2::state::UniswapV2State;

        let token_in = token_with_decimals(0x01, "IN", dec_in);
        let token_out = token_with_decimals(0x02, "OUT", dec_out);

        let reserve_in = U256::from(human_reserve_in) * U256::from(10u64).pow(U256::from(dec_in));
        let reserve_out =
            U256::from(human_reserve_out) * U256::from(10u64).pow(U256::from(dec_out));
        let univ2 = UniswapV2State::new(reserve_in, reserve_out);

        // Derive mock spot_price from UniV2's spot_price
        let univ2_spot = univ2
            .spot_price(&token_in, &token_out)
            .expect("UniV2 spot_price should succeed");
        let mock_spot_price = human_reserve_out as f64 / human_reserve_in as f64;
        let liquidity = human_reserve_out as u128 * 10u128.pow(dec_out);
        let sim = MockProtocolSim::new(mock_spot_price)
            .with_liquidity(liquidity)
            .with_tokens(&[token_in.clone(), token_out.clone()]);

        // 1) spot_price should match (within rounding tolerance)
        let mock_spot = sim
            .spot_price(&token_in, &token_out)
            .expect("mock spot_price should succeed");
        let spot_price_ratio = mock_spot / univ2_spot;
        assert!(
            (0.99..=1.01).contains(&spot_price_ratio),
            "spot_price mismatch: mock={mock_spot}, univ2={univ2_spot}"
        );

        // 2) get_amount_out for a small swap (0.1% of reserve) should be close
        let small_amount_in = BigUint::from(human_reserve_in) * BigUint::from(10u64).pow(dec_in) /
            BigUint::from(1000u64);

        let univ2_result = univ2
            .get_amount_out(small_amount_in.clone(), &token_in, &token_out)
            .expect("UniV2 get_amount_out should succeed");
        let mock_result = sim
            .get_amount_out(small_amount_in, &token_in, &token_out)
            .expect("mock get_amount_out should succeed");

        let univ2_out: f64 = univ2_result
            .amount
            .to_string()
            .parse()
            .unwrap();
        let mock_out: f64 = mock_result
            .amount
            .to_string()
            .parse()
            .unwrap();
        let amount_out_ratio = mock_out / univ2_out;
        assert!(
            (0.99..=1.01).contains(&amount_out_ratio),
            "get_amount_out mismatch: mock={mock_out}, univ2={univ2_out}"
        );

        // 3) get_limits: mock's sell_limit should be in the same order of magnitude as UniV2
        let univ2_limits = univ2
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .expect("UniV2 get_limits should succeed");
        let mock_limits = sim
            .get_limits(token_in.address.clone(), token_out.address.clone())
            .expect("mock get_limits should succeed");

        let univ2_sell: f64 = univ2_limits
            .0
            .to_string()
            .parse()
            .unwrap();
        let mock_sell: f64 = mock_limits
            .0
            .to_string()
            .parse()
            .unwrap();
        if univ2_sell > 0.0 && mock_sell > 0.0 {
            let sell_limit_ratio = mock_sell / univ2_sell;
            assert!(
                (0.1..=10.0).contains(&sell_limit_ratio),
                "get_limits sell_limit order of magnitude mismatch: mock={mock_sell}, \
                 univ2={univ2_sell}"
            );
        }
    }
}

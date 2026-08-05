//! Shared test fixtures for the decomposition port.
//!
//! Port of `defibot/solver/tests/algorithms/decomposition/utils.py` and `conftest.py`. defibot's
//! mocks subclass `SimpleRoute` itself; here the equivalent knob sits one level lower, in a
//! [`ProtocolSim`] that [`PoolRef`] wraps, so the production composition code under test is the
//! real one.
//!
//! Later tasks of the port (optimizers, solver) build their fixtures from here rather than
//! reinventing them.

use num_bigint::BigUint;
use num_traits::Zero;
use tycho_simulation::tycho_core::{
    dto::ProtocolStateDelta,
    models::{token::Token, Address},
    simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, PoolSwap, ProtocolSim, QueryPoolSwapParams},
    },
    Bytes,
};

use crate::{
    algorithm::{
        decomposition::components::{
            Branch, DecompositionError, Fraction, Hop, PoolRef, SellLimitKind, SequentialRoute,
            SolutionGraph,
        },
        test_utils::token_with_decimals,
    },
    types::ComponentId,
};

/// A pool that buys a fixed multiple of what it is sold.
///
/// Port of `utils.py:16-135` (`MockRoute`) and `utils.py:138-148` (`MockRouteLimit`): output is
/// `input * buy_multiple` (defibot: ten times), gas is a constant (defibot: one), and the amount
/// that can be sold is capped by `sell_limit`, which is what `get_limits` reports.
///
/// `spot_price` and `post_trade_spot_price` are reported verbatim rather than derived from
/// reserves, which is what makes the marginal-price cases of `test_routes.py:442-576` portable:
/// the pre- and post-trade prices are chosen independently. `post_trade_spot_price` only ever
/// shows up on the state a swap returns, so a pool that has not been sold on still has no
/// post-trade price at all.
///
/// The mock is directional only in `spot_price`, which is inverted for a descending token pair the
/// way every other simulator in the crate does it. `get_amount_out` and `get_limits` are
/// direction-independent, so a fixture must trade the pair in one direction only.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct FixedRateSim {
    /// Spot price of the higher-address token per unit of the lower-address token.
    spot_price: f64,
    /// Spot price of the state a swap returns.
    post_trade_spot_price: f64,
    /// Trading fee as a fraction of the input.
    fee: f64,
    /// Output per unit of input.
    buy_multiple: u64,
    /// Gas a swap reports.
    gas: u64,
    /// Largest input the pool accepts.
    sell_limit: BigUint,
    /// Input above which the pool math fails without the limit having been exceeded.
    ///
    /// Concentrated liquidity runs out of ticks well before `get_limits` says it should, which is
    /// the failure `recursive_solve_splits` recovers from by halving (`order_solver.py:619-625`).
    /// A pool that simply refuses the size raises a limit error instead and takes the other
    /// recovery path, so the two knobs have to be separate.
    simulation_failure_above: Option<BigUint>,
}

impl FixedRateSim {
    /// A pool that buys `buy_multiple` times what it is sold, at one gas and no fee.
    pub(crate) fn new(buy_multiple: u64) -> Self {
        Self {
            spot_price: buy_multiple as f64,
            post_trade_spot_price: buy_multiple as f64,
            fee: 0.0,
            buy_multiple,
            gas: 1,
            // defibot's `MockRoute` caps at 10^10 (`utils.py:48-51`), but its mock tokens convert
            // on-chain and human amounts with the identity function. Fynd's routes cast limits
            // back through real decimals, so a 10^10 cap on an 18-decimal intermediate token
            // rounds to nothing in 8-decimal WBTC. The cap is only there to be hit deliberately;
            // fixtures that want to hit it set their own.
            sell_limit: BigUint::from(10u8).pow(15),
            simulation_failure_above: None,
        }
    }

    /// Sets the pre-trade spot price, leaving the output multiple alone.
    pub(crate) fn with_spot_price(mut self, spot_price: f64) -> Self {
        self.spot_price = spot_price;
        self.post_trade_spot_price = spot_price;
        self
    }

    /// Sets the spot price of the state a swap returns.
    pub(crate) fn with_post_trade_spot_price(mut self, post_trade_spot_price: f64) -> Self {
        self.post_trade_spot_price = post_trade_spot_price;
        self
    }

    /// Sets the trading fee.
    pub(crate) fn with_fee(mut self, fee: f64) -> Self {
        self.fee = fee;
        self
    }

    /// Sets the largest input the pool accepts.
    pub(crate) fn with_sell_limit(mut self, sell_limit: u64) -> Self {
        self.sell_limit = BigUint::from(sell_limit);
        self
    }

    /// Makes the pool's math fail above `threshold` while still reporting its full limit.
    pub(crate) fn with_simulation_failure_above(mut self, threshold: u64) -> Self {
        self.simulation_failure_above = Some(BigUint::from(threshold));
        self
    }
}

#[typetag::serde]
impl ProtocolSim for FixedRateSim {
    fn fee(&self) -> f64 {
        self.fee
    }

    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        if base.address < quote.address {
            Ok(self.spot_price)
        } else {
            Ok(1.0 / self.spot_price)
        }
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        _token_in: &Token,
        _token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        if amount_in > self.sell_limit {
            return Err(SimulationError::InvalidInput(
                format!("amount {amount_in} exceeds the mock's sell limit {}", self.sell_limit),
                None,
            ));
        }
        if self
            .simulation_failure_above
            .as_ref()
            .is_some_and(|threshold| &amount_in > threshold)
        {
            return Err(SimulationError::FatalError(format!(
                "mock pool math failed for amount {amount_in}"
            )));
        }
        let new_state = Box::new(Self {
            spot_price: self.post_trade_spot_price,
            post_trade_spot_price: self.post_trade_spot_price,
            fee: self.fee,
            buy_multiple: self.buy_multiple,
            gas: self.gas,
            sell_limit: self.sell_limit.clone(),
            simulation_failure_above: self.simulation_failure_above.clone(),
        });
        let gas = if amount_in.is_zero() { BigUint::zero() } else { BigUint::from(self.gas) };
        Ok(GetAmountOutResult::new(amount_in * self.buy_multiple, gas, new_state))
    }

    fn get_limits(
        &self,
        _sell_token: Bytes,
        _buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        Ok((self.sell_limit.clone(), &self.sell_limit * self.buy_multiple))
    }

    fn query_pool_swap(&self, _params: &QueryPoolSwapParams) -> Result<PoolSwap, SimulationError> {
        unimplemented!("query_pool_swap is not needed by the decomposition fixtures")
    }

    fn delta_transition(
        &mut self,
        _delta: ProtocolStateDelta,
        _tokens: &std::collections::HashMap<Bytes, Token>,
        _balances: &Balances,
    ) -> Result<(), TransitionError> {
        unimplemented!("delta_transition is not needed by the decomposition fixtures")
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
            .is_some_and(|o| {
                o.spot_price == self.spot_price &&
                    o.buy_multiple == self.buy_multiple &&
                    o.sell_limit == self.sell_limit
            })
    }
}

// ===================== Tokens =====================

/// Sell-side token of the arithmetic fixtures, 18 decimals.
///
/// defibot's mocks convert on-chain and human amounts with the identity function, so its
/// `USDC -> USDT -> DAI -> WETH` chains carry no decimal scaling. Equal decimals here reproduce
/// that; the decimal-scaling rules have their own tests.
pub(crate) fn token_a() -> Token {
    token_with_decimals(0x0A, "A", 18)
}

/// First intermediate token of the arithmetic fixtures, 18 decimals.
pub(crate) fn token_b() -> Token {
    token_with_decimals(0x0B, "B", 18)
}

/// Second intermediate token of the arithmetic fixtures, 18 decimals.
pub(crate) fn token_c() -> Token {
    token_with_decimals(0x0C, "C", 18)
}

/// Buy-side token of the arithmetic fixtures, 18 decimals.
pub(crate) fn token_d() -> Token {
    token_with_decimals(0x0D, "D", 18)
}

/// WBTC with its mainnet decimals, for the diamond fixture.
pub(crate) fn wbtc() -> Token {
    token_with_decimals(0x01, "WBTC", 8)
}

/// WETH with its mainnet decimals, for the diamond fixture.
pub(crate) fn weth() -> Token {
    token_with_decimals(0x02, "WETH", 18)
}

/// USDC with its mainnet decimals, for the diamond fixture.
pub(crate) fn usdc() -> Token {
    token_with_decimals(0x03, "USDC", 6)
}

/// DAI with its mainnet decimals, for the diamond fixture.
pub(crate) fn dai() -> Token {
    token_with_decimals(0x04, "DAI", 18)
}

// ===================== Builders =====================

/// A pool wrapping `sim` under `id`, with no depth entry.
pub(crate) fn pool(id: &str, sim: FixedRateSim) -> PoolRef {
    PoolRef::new(id.to_string(), SellLimitKind::Enforced, Box::new(sim), None)
}

/// A pool that buys ten times what it is sold, the defibot `MockRoute` default.
pub(crate) fn tenfold_pool(id: &str) -> PoolRef {
    pool(id, FixedRateSim::new(10))
}

/// A hop over `pools`, unsolved.
pub(crate) fn hop(token_in: Token, token_out: Token, pools: Vec<PoolRef>) -> Hop {
    Hop::new(token_in, token_out, pools).expect("hop has pools")
}

/// A hop over `pools` with one split each.
pub(crate) fn solved_hop(
    token_in: Token,
    token_out: Token,
    pools: Vec<PoolRef>,
    splits: Vec<Fraction>,
) -> Hop {
    let mut hop = hop(token_in, token_out, pools);
    hop.set_splits(splits)
        .expect("one split per pool");
    hop
}

/// A single-pool hop carrying the whole input.
pub(crate) fn single_pool_hop(token_in: Token, token_out: Token, pool: PoolRef) -> Hop {
    solved_hop(token_in, token_out, vec![pool], vec![Fraction::one()])
}

/// A route over a token path.
pub(crate) fn route(tokens: Vec<Token>, hops: Vec<Hop>) -> SequentialRoute {
    SequentialRoute::new(tokens, hops).expect("route matches its token path")
}

/// A solution graph whose branches are one token path each — the ungrouped shape.
pub(crate) fn graph(routes: Vec<SequentialRoute>, outer_splits: Vec<Fraction>) -> SolutionGraph {
    SolutionGraph::from_routes(routes, outer_splits).expect("branches share endpoints")
}

/// A branch: one shared head hop, the tails hanging off it, and the split between them.
pub(crate) fn branch(head: Hop, tails: Vec<SequentialRoute>, tail_splits: Vec<Fraction>) -> Branch {
    Branch::new(head, tails, tail_splits).expect("tails hang off the head")
}

/// An exact split, panicking on a zero denominator.
pub(crate) fn split(numerator: i64, denominator: i64) -> Fraction {
    Fraction::from_ratio(numerator, denominator).expect("non-zero denominator")
}

/// A split from a float, panicking on a non-finite value.
pub(crate) fn split_f64(value: f64) -> Fraction {
    Fraction::from_f64(value).expect("finite value")
}

/// A route whose hops each hold one tenfold pool, named `{prefix}_{leg}`.
pub(crate) fn tenfold_route(prefix: &str, tokens: Vec<Token>) -> SequentialRoute {
    let mut hops = Vec::with_capacity(tokens.len() - 1);
    for (leg, pair) in tokens.windows(2).enumerate() {
        hops.push(single_pool_hop(
            pair[0].clone(),
            pair[1].clone(),
            tenfold_pool(&format!("{prefix}_{leg}")),
        ));
    }
    route(tokens, hops)
}

/// The four-branch WBTC -> USDC diamond of `conftest.py:87-135`.
///
/// ```text
///   |-----------------|
/// WBTC ---- WETH --- USDC
///   |-- DAI --|------|
/// ```
///
/// Branch order matches defibot's: the direct pool, `WBTC -> DAI -> USDC`,
/// `WBTC -> DAI -> WETH -> USDC`, and `WBTC -> WETH -> USDC`. Every branch carries a quarter of
/// the order.
///
/// defibot's four branches share pool objects (`WBTC/DAI` and `WETH/USDC` each appear twice), so
/// the same component id appears in more than one branch here too. Fynd's [`PoolRef`] owns its
/// simulation state, so the repeated ids are *separate* states: this fixture reproduces defibot's
/// topology, not its aliasing.
pub(crate) fn diamond_graph() -> SolutionGraph {
    let branches = vec![
        route(
            vec![wbtc(), usdc()],
            vec![single_pool_hop(wbtc(), usdc(), tenfold_pool("WBTC/USDC"))],
        ),
        route(
            vec![wbtc(), dai(), usdc()],
            vec![
                single_pool_hop(wbtc(), dai(), tenfold_pool("WBTC/DAI")),
                single_pool_hop(dai(), usdc(), tenfold_pool("DAI/USDC")),
            ],
        ),
        route(
            vec![wbtc(), dai(), weth(), usdc()],
            vec![
                single_pool_hop(wbtc(), dai(), tenfold_pool("WBTC/DAI")),
                single_pool_hop(dai(), weth(), tenfold_pool("DAI/WETH")),
                single_pool_hop(weth(), usdc(), tenfold_pool("WETH/USDC")),
            ],
        ),
        route(
            vec![wbtc(), weth(), usdc()],
            vec![
                single_pool_hop(wbtc(), weth(), tenfold_pool("WBTC/WETH")),
                single_pool_hop(weth(), usdc(), tenfold_pool("WETH/USDC")),
            ],
        ),
    ];
    graph(branches, vec![split(1, 4); 4])
}

/// Errors matching [`DecompositionError::SellAmountLimit`], unwrapped into limit, token and the
/// components responsible for it.
pub(crate) fn expect_sell_amount_limit(
    error: DecompositionError,
) -> (BigUint, Address, Vec<ComponentId>) {
    match error {
        DecompositionError::SellAmountLimit { limit, token, pools } => (limit, token, pools),
        other => panic!("expected SellAmountLimit, got {other:?}"),
    }
}

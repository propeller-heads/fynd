//! A small market and derived data for the tests of probes.

use num_bigint::BigUint;
use tycho_simulation::{
    tycho_common::simulation::protocol_sim::ProtocolSim,
    tycho_core::simulation::protocol_sim::Price,
};

use super::ProbeSizer;
use crate::{
    algorithm::test_utils::{
        addr, component, set_last_updated_block, token, token_with_decimals, MockProtocolSim,
    },
    derived::{DerivedData, TokenGasPrices},
    feed::market_data::MarketState,
};

/// A uniswap_v2 pool id the real swap encoder accepts: it must be a valid address.
pub(super) const POOL: &str = "0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc";

/// The block the market's state belongs to.
pub(super) const BLOCK: u64 = 100;

const ONE_ETH: u128 = 1_000_000_000_000_000_000;

/// USDC (0x01, 6 decimals) and WETH (0x02, 18 decimals) in one pool that pays 0.0005 WETH for
/// 1 USDC, at block [`BLOCK`].
pub(super) fn market() -> MarketState {
    let usdc = token_with_decimals(0x01, "USDC", 6);
    let weth = token(0x02, "WETH");
    let mut market = MarketState::new();
    market.upsert_components([component(POOL, &[usdc.clone(), weth.clone()])]);
    market.upsert_tokens([usdc, weth]);
    market.update_states([(
        POOL.to_string(),
        Box::new(MockProtocolSim::new(0.0005)) as Box<dyn ProtocolSim>,
    )]);
    set_last_updated_block(&mut market, BLOCK, 1_200);
    market
}

/// Token prices of an ETH worth 2,000 USD.
pub(super) fn derived() -> DerivedData {
    let mut derived = DerivedData::new();
    derived.set_token_prices(
        TokenGasPrices::from_iter([
            (addr(0x02), Price { numerator: BigUint::from(1u8), denominator: BigUint::from(1u8) }),
            (
                addr(0x01),
                Price {
                    numerator: BigUint::from(2_000_000_000u64),
                    denominator: BigUint::from(ONE_ETH),
                },
            ),
        ]),
        vec![],
        BLOCK,
    );
    derived
}

/// Probes worth 500 USD, capped at 1% of the depth.
pub(super) fn sizer() -> ProbeSizer {
    ProbeSizer::new(500.0, 0.01).expect("valid probe size")
}

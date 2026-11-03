//! The size of a probe swap: a fixed amount in USD, capped by the component's depth.

use std::{cmp::Reverse, sync::LazyLock};

use num_bigint::BigUint;
use num_traits::Zero;
use tycho_simulation::{
    tycho_common::models::Address, tycho_core::simulation::protocol_sim::Price,
};

use crate::{
    derived::{ComponentDepths, DerivedData, TokenGasPrices},
    feed::market_data::MarketState,
};

/// USD stablecoin symbols, most trusted first, shared with the price guard.
static USD_STABLECOINS: LazyLock<Vec<String>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../price_guard/stable_usd.json"))
        .expect("stable_usd.json is valid")
});

/// Parts per million in a share, so that the depth cap is computed in integers.
const SHARE_SCALE: u64 = 1_000_000;

/// Why a probe amount could not be set.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SizingError {
    /// The probe size in USD is not a positive finite number.
    #[error("the probe size must be a positive number of USD, got {0}")]
    InvalidUsd(String),
    /// The depth share is not in (0, 1].
    #[error("the depth share must be above 0 and at most 1, got {0}")]
    InvalidDepthShare(String),
    /// The derived data holds no token prices yet.
    #[error("the derived data has no token prices yet")]
    PricesNotReady,
    /// No USD stablecoin of the market has a derived price, so a USD amount has no token amount.
    #[error("no USD stablecoin of the market has a derived price to turn USD into tokens")]
    NoUsdReference,
    /// The token the probe sells has no derived price.
    #[error("token {0} has no derived price")]
    Unpriced(String),
    /// The amount, after the depth cap, is zero.
    #[error("the probe amount of token {0} rounds to zero")]
    AmountIsZero(String),
}

/// Sets the amount a probe swap sells: a fixed value in USD, capped at a share of the component's
/// depth.
///
/// The USD value turns into tokens through the derived token prices, which count each token in
/// raw units per raw unit of the chain's native asset. A USD stablecoin of the market is the
/// reference: for a token with price `p` and a stablecoin with price `s` and `d` decimals, one USD
/// is `10^d * p / s` raw units of the token. The stablecoin is taken as worth exactly one USD.
///
/// The reference is the most trusted symbol of `stable_usd.json` that has a derived price, such as
/// USDC; among tokens that share it, the one of the best quality. This is not a market price
/// feed: a probe that is a few percent off its USD target still tests the component.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeSizer {
    usd_cents: u64,
    depth_share_ppm: u64,
}

impl ProbeSizer {
    /// A sizer for probes worth `usd` dollars, capped at `depth_share` of the component's depth
    /// where the depth is known.
    ///
    /// # Errors
    ///
    /// [`SizingError::InvalidUsd`] when `usd` is not positive and finite, and
    /// [`SizingError::InvalidDepthShare`] when `depth_share` is not in (0, 1].
    pub fn new(usd: f64, depth_share: f64) -> Result<Self, SizingError> {
        let usd_cents = (usd * 100.0).round();
        if !usd_cents.is_finite() || usd_cents < 1.0 || usd_cents >= u64::MAX as f64 {
            return Err(SizingError::InvalidUsd(usd.to_string()));
        }
        let depth_share_ppm = (depth_share * SHARE_SCALE as f64).round();
        if !depth_share.is_finite() || depth_share_ppm < 1.0 || depth_share > 1.0 {
            return Err(SizingError::InvalidDepthShare(depth_share.to_string()));
        }
        Ok(Self { usd_cents: usd_cents as u64, depth_share_ppm: depth_share_ppm as u64 })
    }

    /// The amount of `token_in` that a probe through `component_id` sells to get `token_out`.
    ///
    /// # Errors
    ///
    /// A [`SizingError`] when the derived data cannot price the token or the USD, or when the
    /// capped amount is zero.
    pub fn amount_in(
        &self,
        market: &MarketState,
        derived: &DerivedData,
        component_id: &str,
        token_in: &Address,
        token_out: &Address,
    ) -> Result<BigUint, SizingError> {
        let prices = derived
            .token_prices()
            .ok_or(SizingError::PricesNotReady)?;
        let usd_amount = self.usd_amount(market, prices, token_in)?;
        let depth = derived
            .component_depths()
            .and_then(|depths: &ComponentDepths| {
                depths.get(&(component_id.to_string(), token_in.clone(), token_out.clone()))
            });
        let amount = match depth {
            Some(depth) => usd_amount.min(depth * self.depth_share_ppm / SHARE_SCALE),
            None => usd_amount,
        };
        if amount.is_zero() {
            return Err(SizingError::AmountIsZero(token_in.to_string()));
        }
        Ok(amount)
    }

    /// The raw units of `token_in` worth this sizer's USD value, before the depth cap.
    fn usd_amount(
        &self,
        market: &MarketState,
        prices: &TokenGasPrices,
        token_in: &Address,
    ) -> Result<BigUint, SizingError> {
        let (stable_decimals, stable_price) =
            usd_reference(market, prices).ok_or(SizingError::NoUsdReference)?;
        let token_price = prices
            .get(token_in)
            .filter(|price| !price.numerator.is_zero() && !price.denominator.is_zero())
            .ok_or_else(|| SizingError::Unpriced(token_in.to_string()))?;
        let numerator = BigUint::from(self.usd_cents) *
            BigUint::from(10u32).pow(stable_decimals) *
            &token_price.numerator *
            &stable_price.denominator;
        let denominator =
            BigUint::from(100u32) * &token_price.denominator * &stable_price.numerator;
        Ok(numerator / denominator)
    }
}

/// The decimals and derived price of the USD stablecoin that sizes probes, if the market has one
/// with a price.
fn usd_reference<'a>(market: &MarketState, prices: &'a TokenGasPrices) -> Option<(u32, &'a Price)> {
    let mut best: Option<(usize, Reverse<u32>, &Address)> = None;
    for (address, token) in market.token_registry_ref() {
        let Some(rank) = USD_STABLECOINS
            .iter()
            .position(|symbol| symbol.eq_ignore_ascii_case(&token.symbol))
        else {
            continue;
        };
        let has_price = prices
            .get(address)
            .is_some_and(|price| !price.numerator.is_zero() && !price.denominator.is_zero());
        if !has_price {
            continue;
        }
        let order = (rank, Reverse(token.quality), address);
        if best.is_none_or(|current| order < current) {
            best = Some(order);
        }
    }
    let (_, _, address) = best?;
    let decimals = market.get_token(address)?.decimals;
    Some((decimals, prices.get(address)?))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use rustc_hash::FxHashMap;

    use super::*;
    use crate::algorithm::test_utils::{addr, component, token, token_with_decimals};

    const ONE_ETH: u128 = 1_000_000_000_000_000_000;

    fn price(tokens_per_eth: u128) -> Price {
        Price { numerator: BigUint::from(tokens_per_eth), denominator: BigUint::from(ONE_ETH) }
    }

    /// WETH at 1, USDC (6 decimals) at 2,000 per ETH, and X (18 decimals) at 1,000 per ETH, so
    /// that ETH is 2,000 USD and X is 2 USD.
    fn market_and_prices() -> (MarketState, TokenGasPrices) {
        let weth = token(0x01, "WETH");
        let usdc = token_with_decimals(0x02, "USDC", 6);
        let x = token(0x03, "X");
        let mut market = MarketState::new();
        market.upsert_components([component("pool", &[x.clone(), usdc.clone()])]);
        market.upsert_tokens([weth, usdc, x]);
        let prices = TokenGasPrices::from_iter([
            (addr(0x01), price(ONE_ETH)),
            (addr(0x02), price(2_000_000_000)),
            (addr(0x03), price(1_000 * ONE_ETH)),
        ]);
        (market, prices)
    }

    fn derived_with(prices: TokenGasPrices, depths: &[(&str, u8, u8, u128)]) -> DerivedData {
        let mut derived = DerivedData::new();
        derived.set_token_prices(prices, vec![], 1);
        let depths = depths
            .iter()
            .map(|(pool, token_in, token_out, depth)| {
                ((pool.to_string(), addr(*token_in), addr(*token_out)), BigUint::from(*depth))
            })
            .collect::<FxHashMap<_, _>>();
        derived.set_component_depths(depths, vec![], 1);
        derived
    }

    fn sizer() -> ProbeSizer {
        ProbeSizer::new(500.0, 0.01).unwrap()
    }

    #[rstest]
    #[case::weth(0x01, ONE_ETH / 4)]
    #[case::usdc(0x02, 500_000_000)]
    #[case::x(0x03, 250 * ONE_ETH)]
    fn test_amount_in_is_the_usd_value(#[case] token_in: u8, #[case] expected: u128) {
        let (market, prices) = market_and_prices();
        let derived = derived_with(prices, &[]);

        let amount = sizer()
            .amount_in(&market, &derived, "pool", &addr(token_in), &addr(0x09))
            .unwrap();

        assert_eq!(amount, BigUint::from(expected));
    }

    #[rstest]
    #[case::below_the_cap(100_000 * ONE_ETH, 250 * ONE_ETH)]
    #[case::at_the_cap(25_000 * ONE_ETH, 250 * ONE_ETH)]
    #[case::above_the_cap(100 * ONE_ETH, ONE_ETH)]
    fn test_amount_in_is_capped_by_the_depth(#[case] depth: u128, #[case] expected: u128) {
        let (market, prices) = market_and_prices();
        let derived = derived_with(prices, &[("pool", 0x03, 0x02, depth)]);

        let amount = sizer()
            .amount_in(&market, &derived, "pool", &addr(0x03), &addr(0x02))
            .unwrap();

        assert_eq!(amount, BigUint::from(expected));
    }

    #[test]
    fn test_amount_in_ignores_the_depth_of_another_direction() {
        let (market, prices) = market_and_prices();
        let derived = derived_with(prices, &[("pool", 0x02, 0x03, 1)]);

        let amount = sizer()
            .amount_in(&market, &derived, "pool", &addr(0x03), &addr(0x02))
            .unwrap();

        assert_eq!(amount, BigUint::from(250 * ONE_ETH));
    }

    #[test]
    fn test_amount_in_prefers_the_most_trusted_stablecoin_of_the_best_quality() {
        let (mut market, mut prices) = market_and_prices();
        // A copy of USDC with a wrong price and poor quality, and a USDT that ranks lower.
        let mut fake = token_with_decimals(0x04, "USDC", 6);
        fake.quality = 50;
        let usdt = token_with_decimals(0x05, "USDT", 6);
        market.upsert_tokens([fake, usdt]);
        prices.insert(addr(0x04), price(1_000_000_000));
        prices.insert(addr(0x05), price(1_000_000_000));
        let derived = derived_with(prices, &[]);

        let amount = sizer()
            .amount_in(&market, &derived, "pool", &addr(0x01), &addr(0x02))
            .unwrap();

        assert_eq!(amount, BigUint::from(ONE_ETH / 4));
    }

    #[test]
    fn test_amount_in_without_prices() {
        let (market, _) = market_and_prices();

        let error = sizer()
            .amount_in(&market, &DerivedData::new(), "pool", &addr(0x03), &addr(0x02))
            .unwrap_err();

        assert_eq!(error, SizingError::PricesNotReady);
    }

    #[test]
    fn test_amount_in_without_a_priced_stablecoin() {
        let (market, mut prices) = market_and_prices();
        prices.remove(&addr(0x02));
        let derived = derived_with(prices, &[]);

        let error = sizer()
            .amount_in(&market, &derived, "pool", &addr(0x03), &addr(0x02))
            .unwrap_err();

        assert_eq!(error, SizingError::NoUsdReference);
    }

    #[test]
    fn test_amount_in_of_an_unpriced_token() {
        let (market, prices) = market_and_prices();
        let derived = derived_with(prices, &[]);

        let error = sizer()
            .amount_in(&market, &derived, "pool", &addr(0x09), &addr(0x02))
            .unwrap_err();

        assert_eq!(error, SizingError::Unpriced(addr(0x09).to_string()));
    }

    #[test]
    fn test_amount_in_with_no_depth_left() {
        let (market, prices) = market_and_prices();
        let derived = derived_with(prices, &[("pool", 0x03, 0x02, 0)]);

        let error = sizer()
            .amount_in(&market, &derived, "pool", &addr(0x03), &addr(0x02))
            .unwrap_err();

        assert_eq!(error, SizingError::AmountIsZero(addr(0x03).to_string()));
    }

    #[rstest]
    #[case::zero_usd(0.0, 0.01)]
    #[case::negative_usd(-5.0, 0.01)]
    #[case::fraction_of_a_cent(0.001, 0.01)]
    #[case::nan_usd(f64::NAN, 0.01)]
    #[case::infinite_usd(f64::INFINITY, 0.01)]
    #[case::zero_share(500.0, 0.0)]
    #[case::negative_share(500.0, -0.1)]
    #[case::share_above_one(500.0, 1.5)]
    #[case::nan_share(500.0, f64::NAN)]
    fn test_new_rejects(#[case] usd: f64, #[case] depth_share: f64) {
        assert!(ProbeSizer::new(usd, depth_share).is_err());
    }

    #[test]
    fn test_new_accepts_the_whole_depth() {
        assert!(ProbeSizer::new(0.01, 1.0).is_ok());
    }
}

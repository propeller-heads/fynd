//! A [`ProtocolSim`] that mirrors another pool's curve at a configurable, fee-free price.
//!
//! The `PropAMM` pool that will go live is an Ekubo V3 pool whose base fee is 0 and whose per-swap
//! fee Fynd signs. This type stands in for the fee-free half of that: it mirrors a real pool's live
//! curve and scales the price by [`MirrorPool::from_price_pct`]'s percentage. It charges no fee, so
//! whatever the router later finds above the public commitment is exactly the fee headroom the
//! signed extension could charge and still win the trade.
//!
//! Wrapping rather than patching the mirrored state keeps this protocol-agnostic. Every concrete
//! state (`UniswapV3State`, `EkuboV3State`, a `vm:` pool) stores its fee somewhere different, so
//! rewriting a fee field would mean per-protocol surgery that breaks whenever
//! `tycho-simulation` changes shape. Delegating and scaling the result works for all of them.
//!
//! `query_pool_swap` is deliberately *not* implemented, so the trait default's
//! `"query_pool_swap not implemented"` error makes `PoolDepthComputation` fall back to its Brent
//! solver, which goes through [`ProtocolSim::get_amount_out`] and therefore sees the scaled price.
//! An implementation that delegated would report the mirrored pool's unscaled depth.

use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use tycho_simulation::{
    tycho_common::{models::token::Token, Bytes},
    tycho_core::simulation::{
        errors::{SimulationError, TransitionError},
        protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
    },
};

/// Denominator of the price scale: the mirrored price is expressed in parts per million, so a
/// percentage resolves to 0.01 bps. `u32` throughout, which converts to `f64` losslessly.
const PRICE_SCALE: u32 = 1_000_000;

/// Mirrors `inner`'s curve at `price_ppm` parts per million of its price, charging no fee.
///
/// `price_ppm == PRICE_SCALE` mirrors the source exactly — the control case. Above it the mock
/// quotes better than the best real pool, below it worse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MirrorPool {
    /// The mirrored pool's live state. Typetag-serialized, so any concrete state works.
    inner: Box<dyn ProtocolSim>,
    /// The mock's fee-free price as parts per million of the mirrored pool's price.
    price_ppm: u32,
}

impl MirrorPool {
    /// Wraps `inner`, quoting at `price_pct` percent of its price.
    ///
    /// A percentage outside `[0, 400]` is clamped: the mock is a plausibility probe, and a price
    /// hundreds of times the market's would only produce routes no real pool could ever fill.
    pub(crate) fn from_price_pct(inner: Box<dyn ProtocolSim>, price_pct: f64) -> Self {
        Self { inner, price_ppm: price_pct_to_ppm(price_pct) }
    }

    /// The mock's fee-free price as a fraction of the mirrored pool's price.
    pub(crate) fn price_factor(&self) -> f64 {
        f64::from(self.price_ppm) / f64::from(PRICE_SCALE)
    }

    /// Scales an output amount by the configured price, rounding down.
    fn scale(&self, amount: &BigUint) -> BigUint {
        amount * BigUint::from(self.price_ppm) / BigUint::from(PRICE_SCALE)
    }
}

/// Converts a percentage of the mirrored price into parts per million, clamped to a sane range.
///
/// A non-finite input becomes `PRICE_SCALE` (mirror exactly), which is the safe default: it makes
/// the mock unable to win rather than able to win by an arbitrary amount.
// Truncation and sign loss are impossible after the clamp: the value is finite and in [0, 4e6].
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn price_pct_to_ppm(price_pct: f64) -> u32 {
    if !price_pct.is_finite() {
        return PRICE_SCALE;
    }
    let ppm = (price_pct / 100.0 * f64::from(PRICE_SCALE)).clamp(0.0, 4.0 * f64::from(PRICE_SCALE));
    ppm.round() as u32
}

#[typetag::serde]
impl ProtocolSim for MirrorPool {
    /// Zero: the mock is the fee-free curve, matching the live pool's base fee of 0. The per-swap
    /// fee is the headroom the router discovers afterwards, not something priced in here.
    ///
    /// Reporting zero also sidesteps `ProtocolSim::fee`'s documented panic on protocols with
    /// asymmetric fees (Uniswap V4, Rocketpool), which delegating to `inner` would inherit.
    fn fee(&self) -> f64 {
        0.0
    }

    /// The mirrored pool's spot price, scaled. `spot_price` is quote-per-base — a cost — so a
    /// better price is a smaller number, hence the division.
    fn spot_price(&self, base: &Token, quote: &Token) -> Result<f64, SimulationError> {
        let price = self.inner.spot_price(base, quote)?;
        let factor = self.price_factor();
        if factor <= 0.0 {
            return Err(SimulationError::FatalError(
                "mirrored price factor is zero; the mock pool cannot quote".to_string(),
            ));
        }
        Ok(price / factor)
    }

    fn get_amount_out(
        &self,
        amount_in: BigUint,
        token_in: &Token,
        token_out: &Token,
    ) -> Result<GetAmountOutResult, SimulationError> {
        let result = self
            .inner
            .get_amount_out(amount_in, token_in, token_out)?;
        Ok(GetAmountOutResult {
            amount: self.scale(&result.amount),
            gas: result.gas,
            new_state: Box::new(Self { inner: result.new_state, price_ppm: self.price_ppm }),
        })
    }

    /// The mirrored pool's limits, with the output bound scaled. The input bound is unchanged: the
    /// price scale is a price, not extra liquidity.
    fn get_limits(
        &self,
        sell_token: Bytes,
        buy_token: Bytes,
    ) -> Result<(BigUint, BigUint), SimulationError> {
        let (max_in, max_out) = self
            .inner
            .get_limits(sell_token, buy_token)?;
        Ok((max_in, self.scale(&max_out)))
    }

    fn delta_transition(
        &mut self,
        delta: tycho_simulation::tycho_core::dto::ProtocolStateDelta,
        tokens: &std::collections::HashMap<Bytes, Token>,
        balances: &Balances,
    ) -> Result<(), TransitionError> {
        self.inner
            .delta_transition(delta, tokens, balances)
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
            .is_some_and(|other| {
                self.price_ppm == other.price_ppm && self.inner.eq(other.inner.as_ref())
            })
    }
}

#[cfg(test)]
mod tests {
    use tycho_simulation::tycho_common::models::Chain;

    use super::*;

    /// A constant-price pool: one unit in, `rate` units out, so the price scale is the only thing
    /// that moves the output.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct FlatPool {
        rate: u32,
    }

    #[typetag::serde]
    impl ProtocolSim for FlatPool {
        fn fee(&self) -> f64 {
            0.003
        }

        fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
            Ok(f64::from(self.rate))
        }

        fn get_amount_out(
            &self,
            amount_in: BigUint,
            _token_in: &Token,
            _token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            Ok(GetAmountOutResult {
                amount: amount_in * BigUint::from(self.rate),
                gas: BigUint::from(100_000u64),
                new_state: Box::new(self.clone()),
            })
        }

        fn get_limits(
            &self,
            _sell_token: Bytes,
            _buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            Ok((BigUint::from(1_000u64), BigUint::from(1_000u64 * u64::from(self.rate))))
        }

        fn delta_transition(
            &mut self,
            _delta: tycho_simulation::tycho_core::dto::ProtocolStateDelta,
            _tokens: &std::collections::HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError> {
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
                .is_some_and(|other| self.rate == other.rate)
        }
    }

    fn token(symbol: &str) -> Token {
        Token {
            address: Bytes::from(vec![0x11; 20]),
            symbol: symbol.to_string(),
            decimals: 18,
            tax: 0,
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        }
    }

    fn mirror(price_pct: f64) -> MirrorPool {
        MirrorPool::from_price_pct(Box::new(FlatPool { rate: 1_000 }), price_pct)
    }

    #[test]
    fn test_get_amount_out_scales_by_the_configured_price() {
        // (price_pct, expected output for 1_000 in at rate 1_000)
        for (price_pct, expected) in [
            (100.0, 1_000_000u64),
            (100.01, 1_000_100),
            (100.05, 1_000_500),
            (100.3, 1_003_000),
            (99.9, 999_000),
        ] {
            let out = mirror(price_pct)
                .get_amount_out(BigUint::from(1_000u64), &token("A"), &token("B"))
                .expect("flat pool always quotes")
                .amount;
            assert_eq!(out, BigUint::from(expected), "price_pct = {price_pct}");
        }
    }

    #[test]
    fn test_hundred_percent_mirrors_the_source_exactly() {
        // The control case the harness relies on: at 100% the mock cannot strictly beat its source,
        // so the router must not select it over an equally-priced public route.
        let source = FlatPool { rate: 1_000 };
        let amount = BigUint::from(7_777u64);
        let mirrored = mirror(100.0)
            .get_amount_out(amount.clone(), &token("A"), &token("B"))
            .expect("mirror quotes")
            .amount;
        let direct = source
            .get_amount_out(amount, &token("A"), &token("B"))
            .expect("source quotes")
            .amount;
        assert_eq!(mirrored, direct);
    }

    #[test]
    fn test_fee_is_zero_so_headroom_is_measured_not_assumed() {
        // The mock is the fee-free curve; the fee it could charge is what the router finds above
        // the public commitment, not a number baked in here.
        assert!(mirror(100.5).fee().abs() < f64::EPSILON);
    }

    #[test]
    fn test_new_state_keeps_the_price_scale() {
        // Split routes chain swaps through `new_state`; if the scale were dropped there, only the
        // first leg would be repriced.
        let post_swap = mirror(100.5)
            .get_amount_out(BigUint::from(1_000u64), &token("A"), &token("B"))
            .expect("mirror quotes")
            .new_state;
        let out = post_swap
            .get_amount_out(BigUint::from(1_000u64), &token("A"), &token("B"))
            .expect("post-swap state quotes")
            .amount;
        assert_eq!(out, BigUint::from(1_005_000u64));
    }

    #[test]
    fn test_spot_price_improves_with_the_price_scale() {
        // spot_price is quote-per-base, so a better price is a lower number.
        let base = mirror(100.0)
            .spot_price(&token("A"), &token("B"))
            .expect("price");
        let scaled = mirror(101.0)
            .spot_price(&token("A"), &token("B"))
            .expect("price");
        assert!(scaled < base, "{scaled} should undercut {base}");
        assert!((scaled - base / 1.01).abs() < 1e-9);
    }

    #[test]
    fn test_spot_price_errs_at_a_zero_price() {
        // A zero price would divide by zero. Erroring keeps the pool out of routing instead of
        // producing an infinite spot price that poisons every edge weight derived from it.
        assert!(mirror(0.0)
            .spot_price(&token("A"), &token("B"))
            .is_err());
    }

    #[test]
    fn test_get_limits_scales_output_bound_only() {
        let (max_in, max_out) = mirror(101.0)
            .get_limits(Bytes::from(vec![0x11; 20]), Bytes::from(vec![0x22; 20]))
            .expect("limits");
        assert_eq!(max_in, BigUint::from(1_000u64));
        assert_eq!(max_out, BigUint::from(1_010_000u64));
    }

    #[test]
    fn test_price_pct_clamps_and_defaults_safely() {
        assert_eq!(price_pct_to_ppm(100.0), PRICE_SCALE);
        assert_eq!(price_pct_to_ppm(-5.0), 0, "a negative price floors at zero");
        assert_eq!(price_pct_to_ppm(10_000.0), 4 * PRICE_SCALE, "an absurd price is capped");
        // NaN must mirror exactly rather than win by an arbitrary amount.
        assert_eq!(price_pct_to_ppm(f64::NAN), PRICE_SCALE);
        assert_eq!(price_pct_to_ppm(f64::INFINITY), PRICE_SCALE);
    }

    #[test]
    fn test_query_pool_swap_reports_unimplemented() {
        // PoolDepthComputation matches this exact message to fall back to its Brent solver, which
        // reads amounts through get_amount_out and therefore sees the price scale. Delegating here
        // would instead report the mirrored pool's unscaled depth.
        use tycho_simulation::tycho_core::simulation::protocol_sim::{
            Price, QueryPoolSwapParams, SwapConstraint,
        };

        let params = QueryPoolSwapParams::new(
            token("A"),
            token("B"),
            SwapConstraint::TradeLimitPrice {
                limit: Price::new(BigUint::from(1u64), BigUint::from(1u64)),
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );

        let result = mirror(100.5).query_pool_swap(&params);
        assert!(
            matches!(&result, Err(SimulationError::FatalError(msg))
                if msg == "query_pool_swap not implemented"),
            "expected the trait default's fatal error"
        );
    }

    #[test]
    fn test_gas_is_the_mirrored_pools_gas() {
        // The PropAMM hop costs roughly what its source hop costs; inventing a gas number would
        // bias amount_out_net_gas and therefore the win/loss comparison.
        let result = mirror(105.0)
            .get_amount_out(BigUint::from(1u64), &token("A"), &token("B"))
            .expect("mirror quotes");
        assert_eq!(result.gas, BigUint::from(100_000u64));
    }
}

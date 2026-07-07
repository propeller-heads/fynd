//! Price-impact computation for quotes.
//!
//! The algorithm may report a price impact (see `RouteResult::price_impact`). When it does not,
//! the worker computes a fallback here from per-hop spot prices and endpoint token decimals.

use num_bigint::BigUint;

use crate::feed::market_data::MarketData;
use crate::types::Route;

/// Computes signed price impact from the product of per-hop spot prices.
///
/// `product` is Π of per-hop spot prices (human token_out per human token_in). `d_in`/`d_out`
/// are the decimals of the route's first input token and last output token. Returns `None` for
/// non-finite / non-positive references.
pub(crate) fn price_impact_from_spot_product(
    product: f64,
    amount_in: &BigUint,
    amount_out: &BigUint,
    d_in: u32,
    d_out: u32,
) -> Option<f64> {
    if !product.is_finite() || product <= 0.0 {
        return None;
    }
    let amount_in_f = amount_in.to_string().parse::<f64>().ok()?;
    let amount_out_f = amount_out.to_string().parse::<f64>().ok()?;

    let ideal_out = (amount_in_f / 10f64.powi(d_in as i32)) * product;
    if !ideal_out.is_finite() || ideal_out <= 0.0 {
        return None;
    }
    let exec_out = amount_out_f / 10f64.powi(d_out as i32);
    let impact = 1.0 - exec_out / ideal_out;
    if impact.is_finite() {
        Some(impact)
    } else {
        None
    }
}

/// Computes a fallback price impact for a route from each swap's live spot price.
///
/// Only handles simple linear routes (each hop's output feeds the next hop's input). Returns
/// `None` for split/parallel routes, missing tokens, or spot-price failures — the caller then
/// leaves `price_impact_bps` unset. Split routes are produced only by `path_frank_wolfe`, which
/// reports its own price impact, so this fallback never needs to handle them.
#[allow(dead_code)]
pub(crate) fn spot_price_impact(
    route: &Route,
    amount_in: &BigUint,
    amount_out: &BigUint,
    market: &MarketData,
) -> Option<f64> {
    let swaps = route.swaps();
    if swaps.is_empty() {
        return None;
    }
    // Reject anything that is not a simple linear chain.
    for pair in swaps.windows(2) {
        if pair[0].token_out() != pair[1].token_in() {
            return None;
        }
    }

    // We only read token decimals from the view; the per-hop spot price comes from each swap's
    // own `protocol_state`, so the overlay-skipping behaviour of `try_read_blocking` is
    // irrelevant here. If a writer holds the lock we get `None` and simply omit price impact
    // (fail-safe) rather than blocking the quote.
    let view = market.try_read_blocking()?;

    let mut product = 1.0_f64;
    for swap in swaps {
        let base = view.get_token(swap.token_in())?;
        let quote = view.get_token(swap.token_out())?;
        let sp = swap.protocol_state().spot_price(base, quote).ok()?;
        product *= sp;
    }

    let first = swaps.first()?;
    let last = swaps.last()?;
    let d_in = view.get_token(first.token_in())?.decimals;
    let d_out = view.get_token(last.token_out())?.decimals;

    price_impact_from_spot_product(product, amount_in, amount_out, d_in, d_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bu(s: &str) -> BigUint {
        s.parse().unwrap()
    }

    #[test]
    fn single_hop_near_one_to_one_is_tiny() {
        // 1_000_000 DAI (18dp) -> 999_843.73 USDC (6dp), spot 0.9998 USDC/DAI.
        let pi = price_impact_from_spot_product(
            0.9998,
            &bu("1000000000000000000000000"),
            &bu("999843730000"),
            18,
            6,
        )
        .unwrap();
        assert!(pi.abs() < 0.001, "expected ~0 impact, got {pi}");
    }

    #[test]
    fn multi_hop_uses_product() {
        // product 1.0; 1.0 in (18dp) -> 0.98 out (18dp) => impact ~0.02
        let pi = price_impact_from_spot_product(
            1.0,
            &bu("1000000000000000000"),
            &bu("980000000000000000"),
            18,
            18,
        )
        .unwrap();
        assert!((pi - 0.02).abs() < 1e-6, "got {pi}");
    }

    #[test]
    fn favorable_execution_is_negative() {
        let pi = price_impact_from_spot_product(
            1.0,
            &bu("1000000000000000000"),
            &bu("1010000000000000000"),
            18,
            18,
        )
        .unwrap();
        assert!(pi < 0.0, "got {pi}");
    }

    #[test]
    fn zero_product_returns_none() {
        assert_eq!(
            price_impact_from_spot_product(0.0, &bu("1"), &bu("1"), 18, 18),
            None
        );
    }

    #[test]
    fn mismatched_decimals_are_normalized() {
        // 1 WETH (18dp) at spot 2000 USDC/WETH -> ~2000 USDC (6dp): impact ~0
        let pi = price_impact_from_spot_product(
            2000.0,
            &bu("1000000000000000000"),
            &bu("2000000000"),
            18,
            6,
        )
        .unwrap();
        assert!(pi.abs() < 1e-6, "got {pi}");
    }

    #[test]
    fn absurdly_large_amount_does_not_overflow_to_garbage() {
        // Rust parses out-of-range floats to Ok(inf), not Err — the is_finite guards
        // must catch that and yield None rather than a bogus impact.
        let huge = "1".to_string() + &"0".repeat(400); // ~1e400 > f64::MAX
        let pi = price_impact_from_spot_product(1.0, &bu(&huge), &bu("1"), 0, 0);
        assert_eq!(pi, None);
    }
}

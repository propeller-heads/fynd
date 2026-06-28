//! Approximate USD valuation, anchored on stablecoin legs.
//!
//! Fynd exposes no public token→USD conversion, so Hindsight values a trade in USD only when one
//! side is a known stablecoin: a stablecoin amount is taken at its peg (1 token ≈ 1 USD), scaled
//! by its decimals. A large share of aggregator volume settles into a stablecoin, so this covers
//! the common case without an external price feed. Trades with no stablecoin leg are reported in
//! basis points and token amounts only.

use alloy::primitives::{address, Address, U256};

/// `(stablecoin address, decimals)` on Ethereum mainnet.
const STABLECOINS: &[(Address, u32)] = &[
    (address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"), 6),  // USDC
    (address!("0xdac17f958d2ee523a2206206994597c13d831ec7"), 6),  // USDT
    (address!("0x6b175474e89094c44da98b954eedeac495271d0f"), 18), // DAI
    (address!("0x4c9edd5852cd905f086c759e8383e09bff1e68b3"), 18), // USDe
    (address!("0xdc035d45d973e3ec169d2276ddab16f1e407384f"), 18), // USDS
];

/// USD value of `amount` of `token`, or `None` when `token` is not a known stablecoin.
pub(crate) fn stable_usd(token: Address, amount: U256) -> Option<f64> {
    let &(_, decimals) = STABLECOINS.iter().find(|(addr, _)| *addr == token)?;
    let amount: f64 = amount
        .to_string()
        .parse()
        .ok()?;
    Some(amount / 10f64.powi(decimals as i32))
}

/// Signed USD savings of Fynd's output vs the settled amount (positive = Fynd better), anchored on
/// whichever leg is a stablecoin.
///
/// - **Output leg stable:** both outputs are valued at peg and differenced directly.
/// - **Input leg stable (output not):** the input amount is the trade's USD notional, and the
///   output delta is valued at the settled trade's implied price
///   (`notional * (fynd_out - settled_out) / settled_out`).
///
/// Largest plausible relative output difference between Fynd and the settled trade. Real
/// aggregator routing differs by basis points to low single-digit percent; anything beyond this
/// is a mis-decode (e.g. a dust-tiny or wrong-token settled amount), not a genuine saving.
const MAX_PLAUSIBLE_GAIN: f64 = 1.0; // 100%

/// Signed USD savings of Fynd's output vs the settled amount (positive = Fynd better), anchored on
/// whichever leg is a stablecoin and valued from the relative output gain.
///
/// - **Output leg stable:** the gain is applied to the settled output's USD value (≡ valuing both
///   outputs at peg and differencing).
/// - **Input leg stable (output not):** the gain is applied to the input amount's USD notional.
///
/// Returns `None` when neither leg is a known stablecoin, the settled output is zero, or the
/// implied gain exceeds [`MAX_PLAUSIBLE_GAIN`] (a mis-decode rather than a real saving).
pub(crate) fn savings_usd(
    token_in: Address,
    amount_in: U256,
    token_out: Address,
    fynd_amount_out: U256,
    settled_amount_out: U256,
) -> Option<f64> {
    let fynd: f64 = fynd_amount_out
        .to_string()
        .parse()
        .ok()?;
    let settled: f64 = settled_amount_out
        .to_string()
        .parse()
        .ok()
        .filter(|&v: &f64| v > 0.0)?;

    let gain = (fynd - settled) / settled;
    if !gain.is_finite() || gain.abs() > MAX_PLAUSIBLE_GAIN {
        return None;
    }

    // Prefer the output leg (value at peg); else use the input leg's USD notional.
    if let Some(settled_usd) = stable_usd(token_out, settled_amount_out) {
        return Some(settled_usd * gain);
    }
    let notional = stable_usd(token_in, amount_in)?;
    Some(notional * gain)
}

#[cfg(test)]
mod tests {
    use super::*;

    const USDC: Address = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    const DAI: Address = address!("0x6b175474e89094c44da98b954eedeac495271d0f");
    const WETH: Address = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

    #[test]
    fn stable_usd_scales_by_decimals() {
        // 1,000 USDC (6 decimals) = $1000.
        assert_eq!(stable_usd(USDC, U256::from(1_000_000_000u64)), Some(1_000.0));
        // 1 DAI (18 decimals) = $1.
        assert_eq!(stable_usd(DAI, U256::from(10u64).pow(U256::from(18u64))), Some(1.0));
    }

    #[test]
    fn stable_usd_unknown_token_is_none() {
        assert_eq!(stable_usd(WETH, U256::from(1u64)), None);
    }

    #[test]
    fn savings_usd_output_leg_positive_when_fynd_better() {
        // WETH→USDC: Fynd 1010 USDC vs settled 1000 USDC → +$10 (output leg valued at peg).
        let s = savings_usd(
            WETH,
            U256::from(1u64),
            USDC,
            U256::from(1_010_000_000u64),
            U256::from(1_000_000_000u64),
        )
        .unwrap();
        assert!((s - 10.0).abs() < 1e-6, "expected +10, got {s}");
    }

    #[test]
    fn savings_usd_output_leg_negative_when_fynd_worse() {
        let s = savings_usd(
            WETH,
            U256::from(1u64),
            USDC,
            U256::from(990_000_000u64),
            U256::from(1_000_000_000u64),
        )
        .unwrap();
        assert!((s + 10.0).abs() < 1e-6, "expected -10, got {s}");
    }

    #[test]
    fn savings_usd_input_leg_uses_notional_and_implied_price() {
        // USDC→WETH: $1000 in. Fynd 1.01 WETH vs settled 1.00 WETH → +1% of $1000 = +$10.
        let one_weth = U256::from(10u64).pow(U256::from(18u64));
        let fynd_weth = one_weth * U256::from(101u64) / U256::from(100u64);
        let s = savings_usd(USDC, U256::from(1_000_000_000u64), WETH, fynd_weth, one_weth).unwrap();
        assert!((s - 10.0).abs() < 1e-3, "expected ~+10, got {s}");
    }

    #[test]
    fn savings_usd_input_leg_implausible_ratio_is_none() {
        // $1000 in, but a dust settled output (1 wei) → astronomically large implied savings → skip.
        let one_weth = U256::from(10u64).pow(U256::from(18u64));
        assert_eq!(
            savings_usd(USDC, U256::from(1_000_000_000u64), WETH, one_weth, U256::from(1u64)),
            None
        );
    }

    #[test]
    fn savings_usd_input_leg_zero_settled_is_none() {
        assert_eq!(
            savings_usd(USDC, U256::from(1_000_000_000u64), WETH, U256::from(1u64), U256::ZERO),
            None
        );
    }

    #[test]
    fn savings_usd_neither_leg_stable_is_none() {
        assert_eq!(
            savings_usd(WETH, U256::from(1u64), WETH, U256::from(2u64), U256::from(1u64)),
            None
        );
    }
}

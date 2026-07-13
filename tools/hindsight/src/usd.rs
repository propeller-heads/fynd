//! USD valuation from Fynd's own token prices.
//!
//! The in-process solver exposes, via its derived data, each token's mid-price relative to the gas
//! token (ETH): `price[token]` is the token's native-unit amount per 1 ETH-wei (tycho's
//! best-spread mid-price). Those prices are ETH-denominated, not USD, so to report USD we anchor
//! ETH→USD using the stablecoins' own entries in the same price map (a stablecoin is worth ~$1 per
//! `10^decimals` native units). This values any trade whose output token Fynd has priced — far
//! broader than the previous stablecoin-leg-only valuation — using the solver's self-consistent
//! price view. The prices are f64 approximations suitable for a USD estimate, not execution.

use std::collections::HashMap;

use alloy::primitives::{address, Address, U256};

/// Token prices snapshot from the solver's derived data: `price[token]` = token native-unit amount
/// per 1 ETH-wei. Native ETH is the zero address.
pub(crate) type PriceMap = HashMap<Address, f64>;

/// Wrapped ETH. Interchangeable with native ETH (the zero address) for pricing, since only one of
/// the two may carry a derived price depending on the solver's gas-token configuration.
const WETH: Address = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

/// `(stablecoin address, decimals)` on Ethereum mainnet, used only to anchor ETH→USD.
const STABLECOINS: &[(Address, u32)] = &[
    (address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"), 6), // USDC
    (address!("0xdac17f958d2ee523a2206206994597c13d831ec7"), 6), // USDT
    (address!("0x6b175474e89094c44da98b954eedeac495271d0f"), 18), // DAI
    (address!("0x4c9edd5852cd905f086c759e8383e09bff1e68b3"), 18), // USDe
    (address!("0xdc035d45d973e3ec169d2276ddab16f1e407384f"), 18), // USDS
];

/// Convert a `U256` token amount to `f64` via its four 64-bit limbs (little-endian).
///
/// Powers of 2 are exactly representable in f64, so the limb-scaling is lossless up to the
/// mantissa width. Precision is lost above ~2^53 (≈9e15 native units) — acceptable for USD
/// estimates.
pub(crate) fn u256_to_f64(amount: U256) -> f64 {
    let [a, b, c, d] = amount.as_limbs();
    (*a as f64)
        + (*b as f64) * 2f64.powi(64)
        + (*c as f64) * 2f64.powi(128)
        + (*d as f64) * 2f64.powi(192)
}

/// Positive, finite price of `token`, treating native ETH and WETH as interchangeable.
fn price_of(prices: &PriceMap, token: Address) -> Option<f64> {
    let direct = prices.get(&token).copied();
    let fallback = match token {
        t if t == Address::ZERO => prices.get(&WETH).copied(),
        t if t == WETH => prices.get(&Address::ZERO).copied(),
        _ => None,
    };
    direct
        .or(fallback)
        .filter(|p| *p > 0.0 && p.is_finite())
}

/// USD value of one ETH-wei, averaged over whichever anchor stablecoins are priced.
///
/// For a stablecoin with `d` decimals, `price[stable]` is its native-unit amount per ETH-wei, so
/// `price[stable] / 10^d` is USD per ETH-wei (one native unit ≈ `1/10^d` USD). Returns `None` when
/// no anchor stablecoin is priced.
fn usd_per_eth_wei(prices: &PriceMap) -> Option<f64> {
    let mut sum = 0.0;
    let mut count = 0u32;
    for &(stable, decimals) in STABLECOINS {
        if let Some(price) = price_of(prices, stable) {
            sum += price / 10f64.powi(decimals as i32);
            count += 1;
        }
    }
    (count > 0).then(|| sum / f64::from(count))
}

/// USD value of `amount` native units of `token`: `amount / price[token] * usd_per_eth_wei`.
///
/// Returns `None` when `token` is not priced, no anchor stablecoin is available, or the result is
/// not finite (e.g. an amount that overflows f64).
pub(crate) fn value_usd(token: Address, amount: U256, prices: &PriceMap) -> Option<f64> {
    let usd_per_eth_wei = usd_per_eth_wei(prices)?;
    let price = price_of(prices, token)?;
    let amount = u256_to_f64(amount);
    let usd = amount * usd_per_eth_wei / price;
    usd.is_finite().then_some(usd)
}

/// Convert a native gas cost (ETH-wei) into `token` native units at the snapshot price.
///
/// `price[token]` is the token's native-unit amount per ETH-wei, so the conversion is one
/// multiplication. Returns `None` when `token` is not priced. The f64 round-trip loses wei-level
/// precision, which is acceptable for a gas deduction — the cost itself is exact but its value in
/// the output token is an estimate by nature.
pub(crate) fn gas_in_token(gas_wei: U256, token: Address, prices: &PriceMap) -> Option<U256> {
    let price = price_of(prices, token)?;
    let gas: f64 = gas_wei.to_string().parse().ok()?;
    let units = gas * price;
    units
        .is_finite()
        .then(|| U256::from(units as u128))
}

/// Signed USD savings of Fynd's output vs the settled amount (positive = Fynd better).
///
/// Both amounts are `token_out` native units, valued in USD via [`value_usd`]; the savings is their
/// difference. Returns `None` when `token_out` is not priced or no anchor stablecoin is available.
pub(crate) fn savings_usd(
    token_out: Address,
    fynd_amount_out: U256,
    settled_amount_out: U256,
    prices: &PriceMap,
) -> Option<f64> {
    let fynd_usd = value_usd(token_out, fynd_amount_out, prices)?;
    let settled_usd = value_usd(token_out, settled_amount_out, prices)?;
    Some(fynd_usd - settled_usd)
}

#[cfg(test)]
mod tests {
    use super::*;

    const USDC: Address = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");

    /// ETH = $2000. price[token] = token native-units per ETH-wei.
    /// USDC (6 dp): 2000 USDC per ETH = 2000 * 1e6 native units / 1e18 wei = 2e-9.
    const USDC_PRICE: f64 = 2e-9;
    /// WETH (18 dp): ~1:1 with ETH → 1e18 native units / 1e18 wei = 1.0.
    const WETH_PRICE: f64 = 1.0;

    fn prices() -> PriceMap {
        PriceMap::from([(USDC, USDC_PRICE), (WETH, WETH_PRICE)])
    }

    #[test]
    fn output_leg_stable_positive_when_fynd_better() {
        // WETH→USDC: Fynd 1010 USDC vs settled 1000 USDC → +$10.
        let s = savings_usd(
            USDC,
            U256::from(1_010_000_000u64),
            U256::from(1_000_000_000u64),
            &prices(),
        )
        .unwrap();
        assert!((s - 10.0).abs() < 1e-6, "expected +10, got {s}");
    }

    #[test]
    fn output_leg_negative_when_fynd_worse() {
        let s =
            savings_usd(USDC, U256::from(990_000_000u64), U256::from(1_000_000_000u64), &prices())
                .unwrap();
        assert!((s + 10.0).abs() < 1e-6, "expected -10, got {s}");
    }

    #[test]
    fn output_leg_non_stable_token_priced_via_eth() {
        // USDC→WETH: Fynd 1.01 WETH vs settled 1.00 WETH → +0.01 ETH = +$20 (ETH = $2000).
        let one_weth = U256::from(10u64).pow(U256::from(18u64));
        let fynd_weth = one_weth * U256::from(101u64) / U256::from(100u64);
        let s = savings_usd(WETH, fynd_weth, one_weth, &prices()).unwrap();
        assert!((s - 20.0).abs() < 1e-3, "expected ~+20, got {s}");
    }

    #[test]
    fn native_eth_falls_back_to_weth_price() {
        // token_out is native ETH (zero address); only WETH is priced → use WETH's price.
        let one_eth = U256::from(10u64).pow(U256::from(18u64));
        let fynd_eth = one_eth * U256::from(101u64) / U256::from(100u64);
        let s = savings_usd(Address::ZERO, fynd_eth, one_eth, &prices()).unwrap();
        assert!((s - 20.0).abs() < 1e-3, "expected ~+20, got {s}");
    }

    #[test]
    fn large_gain_not_capped() {
        // Fynd 1500 USDC vs settled 1000 USDC → +$500, no plausibility cap.
        let s = savings_usd(
            USDC,
            U256::from(1_500_000_000u64),
            U256::from(1_000_000_000u64),
            &prices(),
        )
        .unwrap();
        assert!((s - 500.0).abs() < 1e-6, "expected +500, got {s}");
    }

    #[test]
    fn value_usd_scales_amount_by_price_and_anchor() {
        // 1000 USDC (6 dp) → $1000.
        let v = value_usd(USDC, U256::from(1_000_000_000u64), &prices()).unwrap();
        assert!((v - 1_000.0).abs() < 1e-6, "expected $1000, got {v}");
        // 1 WETH → $2000 (ETH = $2000).
        let one_weth = U256::from(10u64).pow(U256::from(18u64));
        let v = value_usd(WETH, one_weth, &prices()).unwrap();
        assert!((v - 2_000.0).abs() < 1e-3, "expected $2000, got {v}");
    }

    #[test]
    fn gas_in_token_converts_wei_at_snapshot_price() {
        // 0.001 ETH of gas, USDC at 2e-9 native units per wei (ETH = $2000) → 2 USDC.
        let gas_wei = U256::from(10u64).pow(U256::from(15u64));
        let got = gas_in_token(gas_wei, USDC, &prices()).unwrap();
        assert_eq!(got, U256::from(2_000_000u64));
    }

    #[test]
    fn gas_in_token_unpriced_token_is_none() {
        assert_eq!(gas_in_token(U256::from(1u64), Address::repeat_byte(0x42), &prices()), None);
    }

    #[test]
    fn value_usd_unpriced_or_no_anchor_is_none() {
        assert_eq!(value_usd(Address::repeat_byte(0x42), U256::from(1u64), &prices()), None);
        assert_eq!(value_usd(USDC, U256::from(1u64), &PriceMap::new()), None);
    }

    #[test]
    fn unpriced_output_token_is_none() {
        let unknown = Address::repeat_byte(0x42);
        assert_eq!(savings_usd(unknown, U256::from(2u64), U256::from(1u64), &prices()), None);
    }

    #[test]
    fn no_anchor_stablecoin_is_none() {
        // WETH priced but no stablecoin → cannot anchor ETH→USD.
        let prices = PriceMap::from([(WETH, WETH_PRICE)]);
        let one_weth = U256::from(10u64).pow(U256::from(18u64));
        assert_eq!(savings_usd(WETH, one_weth, one_weth, &prices), None);
    }

    #[test]
    fn empty_prices_is_none() {
        assert_eq!(savings_usd(USDC, U256::from(2u64), U256::from(1u64), &PriceMap::new()), None);
    }
}

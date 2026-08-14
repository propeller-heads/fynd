//! USD valuation from Fynd's own token prices.
//!
//! The in-process solver prices every token it knows relative to the chain's gas token (ETH on
//! mainnet): `price[token]` is the token's native-unit amount per 1 wei of gas token (tycho's
//! best-spread mid-price). Those prices are not USD, so the gas token itself is anchored to USD
//! via the stablecoins' own entries in the same price map — a stablecoin is worth ~$1 per
//! `10^decimals` native units. Which stablecoins anchor, and which wrapped-native token is
//! interchangeable with the gas token, comes from the chain's address book, so valuation carries
//! no chain knowledge of its own. Any trade whose output token Fynd has priced can be valued
//! this way, using the solver's self-consistent price view. The prices are f64 approximations —
//! fine for a USD estimate, not for execution.

use std::collections::HashMap;

use alloy::primitives::{Address, U256};

use crate::decoder::Registry;

/// Token-price snapshot from the solver's derived data, bundled with the chain's USD anchors:
/// `price[token]` is the token's native-unit amount per 1 wei of the gas token. The native token
/// is the zero address.
pub(crate) struct Prices {
    map: HashMap<Address, f64>,
    /// The chain's wrapped-native token. Interchangeable with native (the zero address) for
    /// pricing, since only one of the two may carry a derived price depending on the solver's
    /// gas-token configuration.
    wrapped_native: Address,
    /// `(stablecoin, decimals)` anchors pinning the gas token to USD, from the address book.
    stablecoins: Vec<(Address, u32)>,
}

impl Prices {
    /// An empty snapshot carrying `registry`'s USD anchors; fill it with `Prices::insert`.
    pub(crate) fn new(registry: &Registry) -> Self {
        Self {
            map: HashMap::new(),
            wrapped_native: registry.wrapped_native(),
            stablecoins: registry.stablecoin_anchors().to_vec(),
        }
    }

    pub(crate) fn insert(&mut self, token: Address, price: f64) {
        self.map.insert(token, price);
    }

    /// The raw derived price of `token`, without the wrapped-native fallback (for logging).
    pub(crate) fn get(&self, token: Address) -> Option<f64> {
        self.map.get(&token).copied()
    }

    /// Positive, finite price of `token`, treating the native token and its wrapped form as
    /// interchangeable.
    fn price_of(&self, token: Address) -> Option<f64> {
        let direct = self.map.get(&token).copied();
        let fallback = match token {
            t if t == Address::ZERO => self
                .map
                .get(&self.wrapped_native)
                .copied(),
            t if t == self.wrapped_native => self.map.get(&Address::ZERO).copied(),
            _ => None,
        };
        direct
            .or(fallback)
            .filter(|p| *p > 0.0 && p.is_finite())
    }

    /// USD value of one wei of the gas token, averaged over whichever anchor stablecoins are
    /// priced.
    ///
    /// For a stablecoin with `d` decimals, `price[stable]` is its native-unit amount per wei, so
    /// `price[stable] / 10^d` is USD per wei (one native unit ≈ `1/10^d` USD). Returns `None`
    /// when no anchor stablecoin is priced.
    fn usd_per_native_wei(&self) -> Option<f64> {
        let mut sum = 0.0;
        let mut count = 0u32;
        for &(stable, decimals) in &self.stablecoins {
            if let Some(price) = self.price_of(stable) {
                sum += price / 10f64.powi(decimals.cast_signed());
                count += 1;
            }
        }
        (count > 0).then(|| sum / f64::from(count))
    }

    /// USD value of `amount` native units of `token`: `amount / price[token] * usd_per_native_wei`.
    ///
    /// Returns `None` when `token` is not priced, no anchor stablecoin is available, or the result
    /// is not finite (e.g. an amount that overflows f64).
    pub(crate) fn value_usd(&self, token: Address, amount: U256) -> Option<f64> {
        let usd_per_native_wei = self.usd_per_native_wei()?;
        let price = self.price_of(token)?;
        let amount = u256_to_f64(amount);
        let usd = amount * usd_per_native_wei / price;
        usd.is_finite().then_some(usd)
    }

    /// Signed USD savings of Fynd's output vs the settled amount (positive = Fynd better).
    ///
    /// Both amounts are `token_out` native units, valued in USD via `Prices::value_usd`; the
    /// savings is their difference. Returns `None` when `token_out` is not priced or no anchor
    /// stablecoin is available.
    pub(crate) fn savings_usd(
        &self,
        token_out: Address,
        fynd_amount_out: U256,
        settled_amount_out: U256,
    ) -> Option<f64> {
        let fynd_usd = self.value_usd(token_out, fynd_amount_out)?;
        let settled_usd = self.value_usd(token_out, settled_amount_out)?;
        Some(fynd_usd - settled_usd)
    }
}

/// Convert a `U256` token amount to `f64` via its four 64-bit limbs (little-endian).
///
/// Powers of 2 are exactly representable in f64, so the limb-scaling is lossless up to the
/// mantissa width. Precision is lost above ~2^53 (≈9e15 native units) — acceptable for USD
/// estimates.
// Precision loss is intentional: this function exists specifically to approximate a U256 as f64.
#[expect(clippy::cast_precision_loss)]
pub(crate) fn u256_to_f64(amount: U256) -> f64 {
    let [a, b, c, d] = amount.as_limbs();
    (*a as f64) +
        (*b as f64) * 2f64.powi(64) +
        (*c as f64) * 2f64.powi(128) +
        (*d as f64) * 2f64.powi(192)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;

    const USDC: Address = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
    const WETH: Address = address!("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");

    /// ETH = $2000. price[token] = token native-units per ETH-wei.
    /// USDC (6 dp): 2000 USDC per ETH = 2000 * 1e6 native units / 1e18 wei = 2e-9.
    const USDC_PRICE: f64 = 2e-9;
    /// WETH (18 dp): ~1:1 with ETH → 1e18 native units / 1e18 wei = 1.0.
    const WETH_PRICE: f64 = 1.0;

    fn empty_prices() -> Prices {
        Prices::new(&Registry::ethereum())
    }

    fn prices() -> Prices {
        let mut prices = empty_prices();
        prices.insert(USDC, USDC_PRICE);
        prices.insert(WETH, WETH_PRICE);
        prices
    }

    #[test]
    fn test_anchors_against_the_address_book() {
        // USDC and WETH are anchors because the ethereum address book registers them, not
        // because this module knows them.
        let registry = Registry::ethereum();
        assert!(registry
            .stablecoin_anchors()
            .contains(&(USDC, 6)));
        assert_eq!(registry.wrapped_native(), WETH);
    }

    #[test]
    fn test_output_leg_stable_fynd_better() {
        // WETH→USDC: Fynd 1010 USDC vs settled 1000 USDC → +$10.
        let s = prices()
            .savings_usd(USDC, U256::from(1_010_000_000u64), U256::from(1_000_000_000u64))
            .unwrap();
        assert!((s - 10.0).abs() < 1e-6, "expected +10, got {s}");
    }

    #[test]
    fn test_output_leg_fynd_worse() {
        let s = prices()
            .savings_usd(USDC, U256::from(990_000_000u64), U256::from(1_000_000_000u64))
            .unwrap();
        assert!((s + 10.0).abs() < 1e-6, "expected -10, got {s}");
    }

    #[test]
    fn test_output_leg_non_stable_token_priced_via_eth() {
        // USDC→WETH: Fynd 1.01 WETH vs settled 1.00 WETH → +0.01 ETH = +$20 (ETH = $2000).
        let one_weth = U256::from(10u64).pow(U256::from(18u64));
        let fynd_weth = one_weth * U256::from(101u64) / U256::from(100u64);
        let s = prices()
            .savings_usd(WETH, fynd_weth, one_weth)
            .unwrap();
        assert!((s - 20.0).abs() < 1e-3, "expected ~+20, got {s}");
    }

    #[test]
    fn test_native_eth_with_weth_price() {
        // token_out is native ETH (zero address); only WETH is priced → use WETH's price.
        let one_eth = U256::from(10u64).pow(U256::from(18u64));
        let fynd_eth = one_eth * U256::from(101u64) / U256::from(100u64);
        let s = prices()
            .savings_usd(Address::ZERO, fynd_eth, one_eth)
            .unwrap();
        assert!((s - 20.0).abs() < 1e-3, "expected ~+20, got {s}");
    }

    #[test]
    fn test_large_gain() {
        // Fynd 1500 USDC vs settled 1000 USDC → +$500, no plausibility cap.
        let s = prices()
            .savings_usd(USDC, U256::from(1_500_000_000u64), U256::from(1_000_000_000u64))
            .unwrap();
        assert!((s - 500.0).abs() < 1e-6, "expected +500, got {s}");
    }

    #[test]
    fn test_value_usd_priced_and_anchored() {
        // 1000 USDC (6 dp) → $1000.
        let v = prices()
            .value_usd(USDC, U256::from(1_000_000_000u64))
            .unwrap();
        assert!((v - 1_000.0).abs() < 1e-6, "expected $1000, got {v}");
        // 1 WETH → $2000 (ETH = $2000).
        let one_weth = U256::from(10u64).pow(U256::from(18u64));
        let v = prices()
            .value_usd(WETH, one_weth)
            .unwrap();
        assert!((v - 2_000.0).abs() < 1e-3, "expected $2000, got {v}");
    }

    #[test]
    fn test_value_usd_unpriced_or_no_anchor() {
        assert_eq!(prices().value_usd(Address::repeat_byte(0x42), U256::from(1u64)), None);
        assert_eq!(empty_prices().value_usd(USDC, U256::from(1u64)), None);
    }

    #[test]
    fn test_unpriced_output_token() {
        let unknown = Address::repeat_byte(0x42);
        assert_eq!(prices().savings_usd(unknown, U256::from(2u64), U256::from(1u64)), None);
    }

    #[test]
    fn test_no_anchor_stablecoin() {
        // WETH priced but no stablecoin → cannot anchor ETH→USD.
        let mut prices = empty_prices();
        prices.insert(WETH, WETH_PRICE);
        let one_weth = U256::from(10u64).pow(U256::from(18u64));
        assert_eq!(prices.savings_usd(WETH, one_weth, one_weth), None);
    }

    #[test]
    fn test_savings_usd_with_empty_prices() {
        assert_eq!(empty_prices().savings_usd(USDC, U256::from(2u64), U256::from(1u64)), None);
    }
}

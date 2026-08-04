//! Fynd's derived token prices, transformed into APEX's per-batch `U256` price map.
//!
//! Input: the exact rationals `Solver::derived_data().token_prices()` computes — per token, a
//! tycho `Price { numerator, denominator }` meaning *raw token units received per wei of the gas
//! token spent* (the same data the experimental `GET /v1/prices` endpoint serves). Two things
//! must happen on the way into APEX, both silent-wrong-by-10^k if skipped (grill r3 F2):
//!
//! 1. **Inversion.** APEX wants value-per-token (a more valuable token has a *larger* price); the
//!    tycho rational is tokens-per-value. So the transform uses `den/num`.
//! 2. **Decimals lift.** APEX amounts live in 18-decimal space, so the price must be per 18-dec
//!    unit: `× 10^(dec_t − 18)`.
//!
//! The scale `S` is chosen per batch from the wrapping-overflow bound (grill r3 F1): APEX's
//! objective squares per-token value = amount₁₈ × price, every op wraps silently, and
//! `increase_precision` can multiply all prices by `10^max_precision_increases`. With `P`
//! pinned, `S` is the largest scale such that `batch_value_units × 10^P < 2^126`.

use std::collections::HashMap;

use apex_solver::types::{Address as ApexAddress, U256};
use num_bigint::BigUint;

/// `max_precision_increases` the whole study pins in `ApexConfig` — the overflow bound below is
/// only valid if the config uses the same value.
pub const MAX_PRECISION_INCREASES: u32 = 2;

/// Scaled prices below this many units are excluded (`price_underflow`): a one-digit price has
/// no room for tâtonnement steps below 100%.
pub const MIN_PRICE_UNITS: u64 = 1_000;

/// One token's inputs to the price map: the tycho rational and the token's decimals.
#[derive(Debug, Clone)]
pub struct TokenPriceInput {
    /// Raw token units received per wei of gas token (tycho `Price.numerator`).
    pub numerator: BigUint,
    /// Wei of gas token spent (tycho `Price.denominator`).
    pub denominator: BigUint,
    pub decimals: u8,
}

/// The per-batch APEX price map with its scale and exclusions.
#[derive(Debug, Default)]
pub struct ApexPriceMap {
    /// Price in scaled units per 18-dec token unit.
    pub prices: HashMap<ApexAddress, U256>,
    /// Tokens whose scaled price rounded below [`MIN_PRICE_UNITS`].
    pub price_underflow: Vec<ApexAddress>,
    /// Tokens with a zero or missing rational.
    pub unpriced: Vec<ApexAddress>,
}

/// Build the APEX price map for one batch.
///
/// `batch_value_wei` is the batch's total notional in wei of the gas token, in 18-dec amount
/// space (Σ over orders of `amount₁₈ × wei-per-unit₁₈`); the caller estimates it with the same
/// rationals passed here. The scale is derived from it so that the whole batch's value fits the
/// squared-objective bound even after `10^MAX_PRECISION_INCREASES` price inflation.
pub fn build_apex_prices(
    inputs: &HashMap<ApexAddress, TokenPriceInput>,
    batch_value_wei: &BigUint,
) -> ApexPriceMap {
    let mut map = ApexPriceMap::default();

    // Overflow budget: values are amount₁₈ × price_scaled; the squared objective needs
    // |value| < 2^127.5, kept with headroom at 2^126, divided by the worst-case precision
    // inflation. batch_value_wei × S must stay under it, so S = budget / batch_value_wei.
    let budget: BigUint = BigUint::from(1u8) << 126;
    let inflation = BigUint::from(10u32).pow(MAX_PRECISION_INCREASES);
    let denominator = batch_value_wei.max(&BigUint::from(1u8)) * &inflation;
    let scale = &budget / &denominator;
    if scale == BigUint::ZERO {
        // A batch too large for any scale: every token underflows, the caller declines it.
        map.price_underflow = inputs.keys().copied().collect();
        return map;
    }

    for (&token, input) in inputs {
        if input.numerator == BigUint::ZERO || input.denominator == BigUint::ZERO {
            map.unpriced.push(token);
            continue;
        }
        // price_scaled = S · 10^(dec−18) · den/num, all in integer arithmetic:
        // S · den · 10^dec / (num · 10^18).
        let numerator =
            &scale * &input.denominator * BigUint::from(10u32).pow(input.decimals as u32);
        let denominator = &input.numerator * BigUint::from(10u32).pow(18u32);
        let scaled = numerator / denominator;
        if scaled < BigUint::from(MIN_PRICE_UNITS) {
            map.price_underflow.push(token);
            continue;
        }
        let bytes = scaled.to_bytes_le();
        if bytes.len() > 32 {
            // Can't be represented; treat as unpriced rather than truncating.
            map.unpriced.push(token);
            continue;
        }
        map.prices
            .insert(token, U256::from_le_slice(&bytes));
    }
    map
}

/// The batch's total notional in wei, valued with the same rationals the price map uses:
/// Σ amount_raw × den/num per order's sell token. (Raw amounts: the rational already carries
/// the token's decimals, so no lift is needed for valuation.)
pub fn batch_value_wei(
    orders: impl Iterator<Item = (ApexAddress, BigUint)>,
    inputs: &HashMap<ApexAddress, TokenPriceInput>,
) -> BigUint {
    let mut total = BigUint::ZERO;
    for (token, amount_raw) in orders {
        let Some(input) = inputs.get(&token) else { continue };
        if input.numerator == BigUint::ZERO {
            continue;
        }
        total += amount_raw * &input.denominator / &input.numerator;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wei(value: u64, exp: u32) -> BigUint {
        BigUint::from(value) * BigUint::from(10u32).pow(exp)
    }

    /// Mirrors fynd-core's token_gas_price fixture: ETH at 2000 USDC. The tycho rational for
    /// USDC is "2000e6 raw USDC per 1e18 wei"; WETH is 1:1. The APEX prices must land at
    /// p(WETH) = 2000 × p(USDC) — per 18-dec UNIT, the decimals lift already folded in. (A
    /// missing lift would make the ratio 2000×10^12; a missing inversion would put USDC above
    /// WETH.)
    #[test]
    fn test_eth_2000_usdc_fixture_ratio() {
        let weth = ApexAddress([1u8; 20]);
        let usdc = ApexAddress([2u8; 20]);
        let inputs = HashMap::from([
            (
                weth,
                TokenPriceInput { numerator: wei(1, 18), denominator: wei(1, 18), decimals: 18 },
            ),
            (
                usdc,
                TokenPriceInput { numerator: wei(2000, 6), denominator: wei(1, 18), decimals: 6 },
            ),
        ]);
        // A $10k-ish batch: 2 WETH ≈ 2e18 wei of notional.
        let map = build_apex_prices(&inputs, &wei(2, 18));
        assert!(map.unpriced.is_empty() && map.price_underflow.is_empty(), "{map:?}");
        let p_weth = map.prices[&weth];
        let p_usdc = map.prices[&usdc];
        // Each price floors independently, so the exact statement is the truncation bound:
        // p_usdc = floor(p_weth_exact / 2000) ⇒ 2000·p_usdc ≤ p_weth < 2000·(p_usdc + 1).
        assert!(
            p_usdc * U256::from(2000u64) <= p_weth &&
                p_weth < (p_usdc + U256::from(1u64)) * U256::from(2000u64),
            "WETH must be worth 2000 USDC per 18-dec unit within flooring: {p_weth} vs {p_usdc}"
        );
    }

    /// The overflow bound: scale × batch value × 10^P stays under 2^126.
    #[test]
    fn test_scale_respects_overflow_budget() {
        let weth = ApexAddress([1u8; 20]);
        let inputs = HashMap::from([(
            weth,
            TokenPriceInput { numerator: wei(1, 18), denominator: wei(1, 18), decimals: 18 },
        )]);
        let batch = wei(1, 24); // a 1M-ETH batch
        let map = build_apex_prices(&inputs, &batch);
        let p_weth = map.prices[&weth];
        // p(WETH) IS the scale for an 18-dec 1:1 token. Reconstruct the bound.
        let scale = BigUint::from_bytes_le(&p_weth.to_le_bytes::<32>());
        assert!(
            scale * batch * BigUint::from(10u32).pow(MAX_PRECISION_INCREASES) <
                (BigUint::from(1u8) << 126),
            "scale must respect the wrapping-overflow budget"
        );
    }

    /// A $1e-9 memecoin: price rounds below the floor at any reasonable scale → excluded,
    /// counted, never a zero divisor inside APEX.
    #[test]
    fn test_dust_price_underflows() {
        let weth = ApexAddress([1u8; 20]);
        let meme = ApexAddress([3u8; 20]);
        let inputs = HashMap::from([
            (
                weth,
                TokenPriceInput { numerator: wei(1, 18), denominator: wei(1, 18), decimals: 18 },
            ),
            (
                // 4e21 raw meme units per wei → per-unit price ~2.5e-22 wei, deep underflow
                // at the scale a 1e30-wei batch allows.
                meme,
                TokenPriceInput { numerator: wei(4, 21), denominator: wei(1, 0), decimals: 18 },
            ),
        ]);
        let map = build_apex_prices(&inputs, &wei(1, 30));
        assert!(map.prices.contains_key(&weth));
        assert_eq!(map.price_underflow, vec![meme]);
    }

    #[test]
    fn test_zero_rational_is_unpriced() {
        let token = ApexAddress([4u8; 20]);
        let inputs = HashMap::from([(
            token,
            TokenPriceInput { numerator: BigUint::ZERO, denominator: wei(1, 18), decimals: 18 },
        )]);
        let map = build_apex_prices(&inputs, &wei(1, 18));
        assert!(map.prices.is_empty());
        assert_eq!(map.unpriced, vec![token]);
    }

    #[test]
    fn test_batch_value_sums_in_wei() {
        let usdc = ApexAddress([2u8; 20]);
        let inputs = HashMap::from([(
            usdc,
            TokenPriceInput { numerator: wei(2000, 6), denominator: wei(1, 18), decimals: 6 },
        )]);
        // 4000 USDC raw = 2 ETH of notional.
        let total = batch_value_wei([(usdc, wei(4000, 6))].into_iter(), &inputs);
        assert_eq!(total, wei(2, 18));
    }
}

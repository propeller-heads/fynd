//! The bridge between Tycho's simulated pools and APEX's solver types.
//!
//! Every pool enters the batch as `Pool::Apex(PoolMetadata, Arc<dyn ApexPool>)` wrapping a Tycho
//! `ProtocolSim` — no native APEX Uniswap models, no conversion of pool state. That keeps one
//! simulation implementation in play (the one Fynd's baseline also used), so a surplus difference
//! between the two sides is batch clearing and not two different views of the same pool.
//!
//! This mirrors turbine's `TychoApexPool`
//! (`~/Projects/propeller-heads/turbine/src/clearing_algorithm/apex/solver.rs:1114`), with the
//! scaling swapped for the declining [`crate::scaling`] module.

use std::{collections::HashMap, sync::Arc};

use alloy::primitives::{Address as AlloyAddress, U256};
use apex_solver::{
    core::{
        pools::custom::ApexPool, Fraction, MarketOrder, PairAddresses, Token as ApexToken,
        TradingPair,
    },
    types::{Address as ApexAddress, U256 as ApexU256},
};
use num_bigint::BigUint;
use tycho_simulation::tycho_common::{
    models::token::Token,
    simulation::protocol_sim::{Price, ProtocolSim, QueryPoolSwapParams, SwapConstraint},
};

use crate::{
    scaling::{Scaled18, TokenScale},
    snapshot::BlockBatchSnapshot,
};

/// A Tycho `ProtocolSim` presented to APEX as a pool.
///
/// APEX's `ApexPool` trait is the only pool interface the batch solver calls, and a blanket impl
/// on `dyn ProtocolSim` is impossible across crates — hence the wrapper. It carries only what the
/// two trait methods need: the token metadata (`ProtocolSim` takes `Token`s, not addresses) and
/// the state itself.
#[derive(Debug, Clone)]
pub struct TychoApexPool {
    /// Tycho's protocol id (`uniswap_v2`, `vm:curve`). Kept for the per-protocol coverage split
    /// the study reports, not read during simulation.
    pub protocol: String,
    /// The pool's tokens by APEX address. `ProtocolSim`'s methods take `Token` values, and the
    /// `ApexPool` trait hands us bare addresses, so the map is the lookup between them.
    pub tokens: HashMap<ApexAddress, Token>,
    /// The simulated pool state at the block being replayed.
    pub pool: Arc<dyn ProtocolSim>,
}

impl ApexPool for TychoApexPool {
    /// The largest amount the pool will sell at `swap_price`, wrapping `ProtocolSim`'s
    /// target-price query.
    ///
    /// Two conversions, both mirroring turbine's wrapper (`solver.rs:1121-1203`), and both easy
    /// to get backwards:
    ///
    /// 1. **Direction.** APEX's `swap_price` is `numerator = sell token, denominator = buy token`;
    ///    `ProtocolSim`'s `Price` is `token_out / token_in`. The two are inverses, so the buy side
    ///    supplies the numerator of the price handed to Tycho.
    /// 2. **Precision.** APEX's prices are already in its 18-decimal space, where a token with
    ///    fewer decimals has a counter-intuitively *larger* price (one unit of USDC is worth 10^12
    ///    atomic units of DAI). Each side of the price is lifted by its own token's scale before
    ///    Tycho sees it, and the returned amount is lifted back on the sell token's.
    fn query_supply(&self, pair: TradingPair, swap_price: Fraction) -> ApexU256 {
        let Some(sell_token) = self
            .tokens
            .get(&pair.sell_token.address)
        else {
            return ApexU256::ZERO;
        };
        let Some(buy_token) = self.tokens.get(&pair.buy_token.address) else {
            return ApexU256::ZERO;
        };
        let Some(sell_scale) = token_scale(sell_token) else {
            return ApexU256::ZERO;
        };

        // Tycho's `Price` is token_out/token_in with the pool selling `pair.sell_token`, so the
        // APEX fraction is inverted AND each side is lifted by its own token's precision — the
        // counter-intuitive direction: in 18-decimal space one unit of a 6-decimal token is worth
        // 10^12 atomic units of an 18-decimal one, so the low-decimal side's price is the larger
        // number. `Price::new` panics on zero, so zero components decline instead.
        let Some(target_numerator) =
            precision_lift(from_apex_u256(swap_price.denominator), buy_token.decimals)
        else {
            return ApexU256::ZERO;
        };
        let Some(target_denominator) =
            precision_lift(from_apex_u256(swap_price.numerator), sell_token.decimals)
        else {
            return ApexU256::ZERO;
        };
        if target_numerator.is_zero() || target_denominator.is_zero() {
            return ApexU256::ZERO;
        }

        let params = QueryPoolSwapParams::new(
            buy_token.clone(),
            sell_token.clone(),
            SwapConstraint::PoolTargetPrice {
                target: Price::new(
                    u256_to_biguint(target_numerator),
                    u256_to_biguint(target_denominator),
                ),
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );
        let Ok(swap) = self.pool.query_pool_swap(&params) else {
            return ApexU256::ZERO;
        };
        let Some(native_supply) = biguint_to_u256(swap.amount_out()) else {
            return ApexU256::ZERO;
        };
        match sell_scale.scale_up(native_supply) {
            Ok(scaled) => to_apex_u256(scaled.0),
            Err(_) => ApexU256::ZERO,
        }
    }

    /// The output for `amount_in`, wrapping `ProtocolSim::get_amount_out`.
    ///
    /// The input is descaled to the token's native decimals *before* the simulation call — Tycho
    /// simulates in native units, and handing it an 18-decimal amount for a 6-decimal token
    /// simulates a trade 10^12 times too large — and the output is scaled back up afterwards.
    /// Returns zero when the pool errors or the result is under `min_amount_out`, which is how
    /// `ApexPool` signals "this pool cannot serve this swap".
    fn get_amount_out(
        &self,
        token_in: ApexAddress,
        token_out: ApexAddress,
        amount_in: ApexU256,
        min_amount_out: ApexU256,
    ) -> ApexU256 {
        let Some(token_in) = self.tokens.get(&token_in) else {
            return ApexU256::ZERO;
        };
        let Some(token_out) = self.tokens.get(&token_out) else {
            return ApexU256::ZERO;
        };
        let (Some(scale_in), Some(scale_out)) = (token_scale(token_in), token_scale(token_out))
        else {
            return ApexU256::ZERO;
        };

        // Flooring the input can only simulate a smaller trade than APEX asked for, never a
        // larger one — the conservative direction for a supply answer.
        let native_in = scale_in.scale_down_floor(Scaled18(from_apex_u256(amount_in)));
        if native_in.is_zero() {
            return ApexU256::ZERO;
        }
        let Ok(result) = self
            .pool
            .get_amount_out(u256_to_biguint(native_in), token_in, token_out)
        else {
            return ApexU256::ZERO;
        };
        let Some(native_out) = biguint_to_u256(&result.amount) else {
            return ApexU256::ZERO;
        };
        let Ok(scaled_out) = scale_out.scale_up(native_out) else {
            return ApexU256::ZERO;
        };
        let amount_out = to_apex_u256(scaled_out.0);
        if amount_out >= min_amount_out {
            amount_out
        } else {
            ApexU256::ZERO
        }
    }
}

/// A tycho token's scaling rule, when APEX can represent it (≤ 18 decimals).
fn token_scale(token: &Token) -> Option<TokenScale> {
    let decimals = u8::try_from(token.decimals).ok()?;
    TokenScale::new(decimals).ok()
}

/// `value · 10^(18 − decimals)`, checked. Used on *price components*, where the lift direction is
/// counter-intuitive (see [`TychoApexPool::query_supply`]); amounts go through
/// [`TokenScale`] instead.
fn precision_lift(value: U256, decimals: u32) -> Option<U256> {
    let decimals = u8::try_from(decimals).ok()?;
    let scale = TokenScale::new(decimals).ok()?;
    scale
        .scale_up(value)
        .ok()
        .map(|scaled| scaled.0)
}

/// An alloy `U256` as APEX's `U256`. The two are the same ruint shape but may come from
/// different crate versions, so the conversion goes through the little-endian bytes.
fn to_apex_u256(value: U256) -> ApexU256 {
    ApexU256::from_le_bytes(value.to_le_bytes::<32>())
}

/// An APEX `U256` as alloy's `U256`.
fn from_apex_u256(value: ApexU256) -> U256 {
    U256::from_le_bytes(value.to_le_bytes::<32>())
}

/// An alloy `U256` as the `BigUint` tycho simulations take.
fn u256_to_biguint(value: U256) -> BigUint {
    BigUint::from_bytes_le(&value.to_le_bytes::<32>())
}

/// A simulation-returned `BigUint` as alloy `U256`. `None` when the value exceeds 256 bits —
/// a nonsensical pool output that declines the swap rather than truncating it.
fn biguint_to_u256(value: &BigUint) -> Option<U256> {
    let bytes = value.to_bytes_le();
    (bytes.len() <= 32).then(|| U256::from_le_slice(&bytes))
}

/// APEX's tâtonnement starting prices, from Fynd's derived per-token price map.
///
/// Fynd prices a token as native units per wei of the gas token; APEX wants an absolute per-token
/// price in its 18-decimal space. The conversion has to fold the token's own decimals in — the
/// same inversion documented on [`TychoApexPool::query_supply`] — or every non-18-decimal token
/// starts the search 10^k off and the batch converges somewhere useless.
///
/// A token whose price is missing, zero, or non-finite is left out: APEX substitutes its
/// configured `starting_price` for unpriced tokens, and the runner counts the omission as
/// `TokenUnpriced` rather than pretending a price existed.
pub fn initial_prices(
    _token_prices: &HashMap<AlloyAddress, f64>,
    _decimals: &HashMap<AlloyAddress, u8>,
) -> HashMap<ApexAddress, ApexU256> {
    todo!("convert each finite, positive Fynd price into an 18-decimal APEX price")
}

/// How an order's limit price is derived — the sensitivity axis the study holds in reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitPolicy {
    /// Use the captured `min_amount_out` as-is: the extracted on-chain floor where there is one,
    /// the capture's synthetic fallback where there is not. The core matrix runs on this.
    AsCaptured,
    /// Override every limit with the executed output less this many basis points. The 50/200 bps
    /// sensitivity band, run only to bound how much the synthetic fallback moves the answer.
    ExecutedLessBps(u32),
}

/// Turn a block's captured trades into APEX market orders, keyed by trading pair.
///
/// Should: for each captured trade, resolve both tokens' [`crate::scaling::TokenScale`], scale
/// `amount_in` and the policy's limit into APEX's 18-decimal space, and push a `MarketOrder`
/// (id = transaction hash, so a clearing can be joined back to its settled trade) onto its
/// pair's bucket. A trade whose limit is absent, whose token has more than 18 decimals, or whose
/// amount overflows scaling is skipped — the caller counts each with its
/// [`crate::runner::ExclusionReason`] so the run reports its own coverage.
pub fn build_orders(
    _snapshot: &BlockBatchSnapshot,
    _decimals: &HashMap<AlloyAddress, u8>,
    _limit_policy: LimitPolicy,
) -> HashMap<PairAddresses, Vec<MarketOrder>> {
    todo!("scale each captured trade into an APEX market order, bucketed by trading pair")
}

/// The APEX token set for a block: one [`ApexToken`] per address the batch's orders and pools
/// touch.
///
/// Should: collect every token referenced by the block's orders and pools, and build an
/// `ApexToken` from Tycho's metadata (address, symbol, decimals). APEX keys `truncate_to_precision`
/// and `validate_result` off these decimals, so a token missing here silently gets defaults.
pub fn apex_tokens(_tokens: &HashMap<AlloyAddress, Token>) -> Vec<ApexToken> {
    todo!("build one APEX token per referenced address from Tycho's token metadata")
}

/// An alloy address as APEX's own 20-byte address type.
///
/// The two are structurally identical but nominally distinct, and APEX's `Address: From<&str>`
/// silently yields the zero address for anything it cannot parse — so conversion goes through the
/// bytes, never through a hex string.
pub fn to_apex_address(address: AlloyAddress) -> ApexAddress {
    ApexAddress(address.into_array())
}

/// An APEX address back as an alloy address, for joining a clearing to its captured trade.
pub fn from_apex_address(address: ApexAddress) -> AlloyAddress {
    AlloyAddress::from(address.0)
}

#[cfg(test)]
mod tests {
    use apex_solver::core::Token as ApexCoreToken;
    use tycho_simulation::{
        evm::protocol::uniswap_v2::state::UniswapV2State,
        tycho_common::{models::Chain, Bytes},
    };

    use super::*;

    const USDC_DECIMALS: u32 = 6;
    const WETH_DECIMALS: u32 = 18;

    /// A tycho token at a deterministic address; low `index` sorts as token0.
    fn tycho_token(index: u8, symbol: &str, decimals: u32) -> Token {
        let mut bytes = [0u8; 20];
        bytes[19] = index;
        Token::new(
            &Bytes::from(bytes.to_vec()),
            symbol,
            decimals,
            0,
            &[Some(60_000)],
            Chain::Base,
            100,
        )
    }

    fn apex_address(token: &Token) -> ApexAddress {
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(token.address.as_ref());
        ApexAddress(bytes)
    }

    fn units(value: u64, decimals: u32) -> U256 {
        U256::from(value) * U256::from(10u64).pow(U256::from(decimals))
    }

    /// A USDC/WETH v2 pool (USDC = token0) at ~4000 USDC/WETH, wrapped for APEX.
    fn usdc_weth_pool() -> (TychoApexPool, Token, Token) {
        let usdc = tycho_token(1, "USDC", USDC_DECIMALS);
        let weth = tycho_token(2, "WETH", WETH_DECIMALS);
        let state = UniswapV2State::new(
            to_apex_u256(units(4_000_000, USDC_DECIMALS)),
            to_apex_u256(units(1_000, WETH_DECIMALS)),
        );
        let pool = TychoApexPool {
            protocol: "uniswap_v2".to_string(),
            tokens: HashMap::from([
                (apex_address(&usdc), usdc.clone()),
                (apex_address(&weth), weth.clone()),
            ]),
            pool: Arc::new(state),
        };
        (pool, usdc, weth)
    }

    /// The agreement property that step 0 exists to prove: an amount pushed through the adapter
    /// in APEX's 18-decimal space equals the direct `ProtocolSim` result lifted by the output
    /// token's scale — in both directions across a mixed-decimal pair. A silent 10^12 error in
    /// either conversion fails this test.
    #[test]
    fn test_direct_vs_adapter_agreement_mixed_decimals() {
        let (adapter, usdc, weth) = usdc_weth_pool();

        // USDC -> WETH: 1000 USDC in, WETH (18-dec output, lift is identity).
        let direct = adapter
            .pool
            .get_amount_out(u256_to_biguint(units(1_000, USDC_DECIMALS)), &usdc, &weth)
            .expect("v2 swap within reserves simulates");
        let through_adapter = adapter.get_amount_out(
            apex_address(&usdc),
            apex_address(&weth),
            to_apex_u256(units(1_000, 18)),
            ApexU256::ZERO,
        );
        let direct_native = biguint_to_u256(&direct.amount).expect("v2 output fits U256");
        assert!(!through_adapter.is_zero(), "the pool serves this trade");
        assert_eq!(
            from_apex_u256(through_adapter),
            direct_native,
            "18-dec output lift is identity"
        );

        // WETH -> USDC: 1 WETH in, USDC output must come back lifted by 10^12.
        let direct = adapter
            .pool
            .get_amount_out(u256_to_biguint(units(1, WETH_DECIMALS)), &weth, &usdc)
            .expect("v2 swap within reserves simulates");
        let through_adapter = adapter.get_amount_out(
            apex_address(&weth),
            apex_address(&usdc),
            to_apex_u256(units(1, 18)),
            ApexU256::ZERO,
        );
        let direct_native = biguint_to_u256(&direct.amount).expect("v2 output fits U256");
        assert_eq!(
            from_apex_u256(through_adapter),
            direct_native * U256::from(10u64).pow(U256::from(12u8)),
            "6-dec output is lifted by 10^12 into APEX space"
        );
    }

    /// `min_amount_out` is the trait's decline signal: an unmet floor returns zero, not the
    /// smaller amount.
    #[test]
    fn test_unmet_floor_declines() {
        let (adapter, usdc, weth) = usdc_weth_pool();
        let served = adapter.get_amount_out(
            apex_address(&usdc),
            apex_address(&weth),
            to_apex_u256(units(1_000, 18)),
            ApexU256::ZERO,
        );
        assert!(!served.is_zero());
        let declined = adapter.get_amount_out(
            apex_address(&usdc),
            apex_address(&weth),
            to_apex_u256(units(1_000, 18)),
            served + ApexU256::from(1u64),
        );
        assert!(declined.is_zero());
    }

    /// A token missing from the adapter's map declines instead of panicking — the runner's
    /// closure precondition should make this unreachable, and the adapter must not turn a
    /// precondition bug into a process abort.
    #[test]
    fn test_unknown_token_declines() {
        let (adapter, usdc, _) = usdc_weth_pool();
        let stranger = ApexAddress([0xAB; 20]);
        assert!(adapter
            .get_amount_out(
                apex_address(&usdc),
                stranger,
                to_apex_u256(units(1, 18)),
                ApexU256::ZERO
            )
            .is_zero());
        let ghost_pair = TradingPair::new(
            ApexCoreToken::new(stranger, "GHOST", 18),
            ApexCoreToken::new(apex_address(&usdc), "USDC", 6),
        );
        assert!(adapter
            .query_supply(ghost_pair, Fraction::new(ApexU256::from(1u64), ApexU256::from(1u64)))
            .is_zero());
    }

    /// `query_supply` agreement: the adapter's answer equals the direct `query_pool_swap` call
    /// with the price hand-converted per the documented rule (inverted, each side lifted by its
    /// own token), lifted to the sell token's 18-dec scale.
    #[test]
    fn test_query_supply_matches_direct_target_price_query() {
        let (adapter, usdc, weth) = usdc_weth_pool();
        // Pool sells WETH for USDC. Spot is 4000 USDC/WETH; ask it to move to 4100 (WETH leaves
        // until it costs 4100). APEX's fraction relates 18-dec AMOUNTS, so 4100 USDC-per-WETH is
        // 4100e18 / 1e18 — no per-token lift here; the adapter applies those in the conversion.
        let apex_price = Fraction::new(to_apex_u256(units(4_100, 18)), to_apex_u256(units(1, 18)));
        let pair = TradingPair::new(
            ApexCoreToken::new(apex_address(&weth), "WETH", 18),
            ApexCoreToken::new(apex_address(&usdc), "USDC", 6),
        );
        let through_adapter = adapter.query_supply(pair, apex_price);
        assert!(!through_adapter.is_zero(), "a 2.5% price move has supply behind it");

        // Direct: token_out/token_in with the pool selling WETH means the target is
        // WETH-per-USDC in native units, i.e. the inverse with each side in its own decimals.
        let direct_params = QueryPoolSwapParams::new(
            usdc.clone(),
            weth.clone(),
            SwapConstraint::PoolTargetPrice {
                target: Price::new(
                    u256_to_biguint(units(1, WETH_DECIMALS)),
                    u256_to_biguint(units(4_100, USDC_DECIMALS)),
                ),
                tolerance: 0.0,
                min_amount_in: None,
                max_amount_in: None,
            },
        );
        let direct = adapter
            .pool
            .query_pool_swap(&direct_params)
            .expect("v2 implements target-price queries");
        let direct_native = biguint_to_u256(direct.amount_out()).expect("fits U256");
        assert_eq!(
            from_apex_u256(through_adapter),
            direct_native,
            "18-dec sell token: lift is identity, so the two answers are equal"
        );
    }

    #[test]
    fn test_address_conversion_round_trips() {
        // APEX's `Address: From<&str>` returns the zero address on any parse failure, so a
        // string-based conversion would turn a typo into a real-looking token. Round-tripping
        // through the bytes is the property that rules that out.
        let weth: AlloyAddress = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
            .parse()
            .expect("a valid checksummed address");
        assert_eq!(from_apex_address(to_apex_address(weth)), weth);
        assert_ne!(to_apex_address(weth), ApexAddress::default());
    }
}

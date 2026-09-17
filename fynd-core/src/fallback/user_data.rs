//! The fallback pool as `TychoFallbackRouter` wants it: JSON on the tycho swap's `user_data`.
//!
//! `FallbackSwapEncoder` deserializes this into its own `FallbackProtocol`, then packs the
//! protocol byte and the protocol data the router's `_executeFallback` decodes. That enum is
//! private to tycho-execution, so this mirrors its wire shape rather than importing it; the tests
//! below assert the JSON against the encodings tycho's own tests expect.
//!
//! | protocol | fields | packed as |
//! |---|---|---|
//! | `uniswap_v2` | `pair`, `fee_bps` | `pair(20) ++ fee_bps(1)` |
//! | `uniswap_v3` | `pool` | `pool(20)` |
//! | `uniswap_v4` | `fee`, `tick_spacing`, `hook`, `hook_data` | `fee(3) ++ tick_spacing(3) ++ hook(20) ++ hook_data` |
//! | `curve` | `pool`, `pool_type`, `i`, `j` | `pool(20) ++ pool_type(1) ++ i(1) ++ j(1)` |
//! | `fluid_v1` | `dex`, `zero2one` | `dex(20) ++ zero2one(1)` |
//!
//! Swap direction on the Uniswap family comes from the sort order of the swap's own tokens, so it
//! is not carried here. Curve's coin indices and Fluid's `zero2one` are the pool's own ordering,
//! which no sort can recover, so both are read off the component.

use std::str::FromStr;

use serde::Serialize;
use tycho_simulation::tycho_common::{
    models::{protocol::ProtocolComponent, Address},
    Bytes,
};

use crate::{
    fallback::{FallbackError, HOOKS_ATTRIBUTE, UNISWAP_V4_SYSTEM},
    types::FallbackLeg,
};

/// The fee every canonical Uniswap V2 pool charges, in basis points.
///
/// `FALLBACK_PROTOCOL_SYSTEMS` admits `uniswap_v2` alone, not its forks, so every candidate pool
/// charges this. It is also the most the router accepts — it reverts above 30 — so admitting a
/// fork here would need the fork's own fee read off the component and checked against that cap.
const UNISWAP_V2_FEE_BPS: u8 = 30;

/// Curve's coin list, in the pool's own index order.
const CURVE_COINS_ATTRIBUTE: &str = "coins";

/// How Curve's math variant is grouped, which decides the `exchange` signature the router calls.
const CURVE_POOL_TYPE_ATTRIBUTE: &str = "pool_type";

/// The Uniswap V4 pool's LP fee, as tycho-simulation's V4 decoder names it.
const UNISWAP_V4_FEE_ATTRIBUTE: &str = "key_lp_fee";

/// The Uniswap V4 pool's tick spacing.
const UNISWAP_V4_TICK_SPACING_ATTRIBUTE: &str = "tick_spacing";

/// The fallback protocol and the data the router needs to run it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
enum FallbackProtocol {
    UniswapV2 { pair: Address, fee_bps: u8 },
    UniswapV3 { pool: Address },
    UniswapV4 { fee: u32, tick_spacing: i32, hook: Address, hook_data: Bytes },
    Curve { pool: Address, pool_type: u8, i: u8, j: u8 },
    FluidV1 { dex: Address, zero2one: bool },
}

/// The `user_data` JSON naming `leg`'s pool, for the tycho swap the pAMM leg encodes to.
///
/// `token_in` and `token_out` are the leg's own tokens. Curve indexes its `exchange` by them and
/// Fluid states its direction from `token_in`, and neither can be recovered from the pool alone.
///
/// # Errors
///
/// `MissingPoolData` when the pool's component does not carry what its protocol needs — a V4 pool
/// with no fee or tick spacing, a Curve pool whose coins do not name the pair, a protocol system
/// outside `FALLBACK_PROTOCOL_SYSTEMS`.
pub(crate) fn fallback_user_data(
    leg: &FallbackLeg,
    token_in: &Address,
    token_out: &Address,
) -> Result<String, FallbackError> {
    let component = leg.protocol_component();
    let protocol = fallback_protocol(component, token_in, token_out)?;
    serde_json::to_string(&protocol).map_err(|error| FallbackError::MissingPoolData {
        component_id: leg.component_id().to_string(),
        reason: error.to_string(),
    })
}

/// Whether `component` carries everything `TychoFallbackRouter` needs to run it as a fallback.
///
/// Selection calls this so a pool that could not be encoded is never chosen — the alternative is
/// a route that prices well and then fails at encoding time.
///
/// # Errors
///
/// `MissingPoolData`, with what the component is missing.
pub(super) fn check_encodable(
    component: &ProtocolComponent,
    token_in: &Address,
    token_out: &Address,
) -> Result<(), FallbackError> {
    fallback_protocol(component, token_in, token_out).map(|_| ())
}

/// Reads `component` into the variant its protocol system calls for.
fn fallback_protocol(
    component: &ProtocolComponent,
    token_in: &Address,
    token_out: &Address,
) -> Result<FallbackProtocol, FallbackError> {
    let pool = pool_address(component)?;
    match component.protocol_system.as_str() {
        "uniswap_v2" => Ok(FallbackProtocol::UniswapV2 { pair: pool, fee_bps: UNISWAP_V2_FEE_BPS }),
        "uniswap_v3" => Ok(FallbackProtocol::UniswapV3 { pool }),
        UNISWAP_V4_SYSTEM => uniswap_v4(component),
        "vm:curve" => curve(component, pool, token_in, token_out),
        "fluid_v1" => Ok(FallbackProtocol::FluidV1 {
            dex: pool,
            zero2one: component.tokens.first() == Some(token_in),
        }),
        other => Err(unencodable(component, format!("{other} is not a fallback protocol"))),
    }
}

/// A Uniswap V4 pool identifies itself by its key, not by an address, so its fee, tick spacing and
/// hook travel instead of a pool address.
fn uniswap_v4(component: &ProtocolComponent) -> Result<FallbackProtocol, FallbackError> {
    let fee = attribute_u32(component, UNISWAP_V4_FEE_ATTRIBUTE)?;
    let tick_spacing = i32::try_from(attribute_u32(component, UNISWAP_V4_TICK_SPACING_ATTRIBUTE)?)
        .map_err(|_| unencodable(component, "tick spacing does not fit int24".to_string()))?;
    let hook = component
        .static_attributes
        .get(HOOKS_ATTRIBUTE)
        .cloned()
        .unwrap_or_else(|| Address::from(vec![0u8; 20]));
    Ok(FallbackProtocol::UniswapV4 { fee, tick_spacing, hook, hook_data: Bytes::default() })
}

/// Curve's `exchange` takes the pair's positions in the pool's own coin list, and a `pool_type`
/// that decides which `exchange` signature the router calls.
fn curve(
    component: &ProtocolComponent,
    pool: Address,
    token_in: &Address,
    token_out: &Address,
) -> Result<FallbackProtocol, FallbackError> {
    let pool_type = u8::try_from(attribute_u32(component, CURVE_POOL_TYPE_ATTRIBUTE)?)
        .map_err(|_| unencodable(component, "pool type does not fit a byte".to_string()))?;
    let coins = curve_coins(component);
    let (Some(i), Some(j)) = (coin_index(&coins, token_in), coin_index(&coins, token_out)) else {
        return Err(unencodable(component, "coins do not name both tokens of the pair".to_string()));
    };
    Ok(FallbackProtocol::Curve { pool, pool_type, i, j })
}

/// The pool's coins in index order, falling back to its token list when it carries no `coins`
/// attribute — the two agree for a two-coin pool, and only a wider pool can order them differently.
fn curve_coins(component: &ProtocolComponent) -> Vec<Address> {
    component
        .static_attributes
        .get(CURVE_COINS_ATTRIBUTE)
        .and_then(|coins| serde_json::from_slice::<Vec<Address>>(coins.as_ref()).ok())
        .unwrap_or_else(|| component.tokens.clone())
}

/// Where `token` sits in the pool's coin list.
fn coin_index(coins: &[Address], token: &Address) -> Option<u8> {
    coins
        .iter()
        .position(|coin| coin == token)
        .and_then(|index| u8::try_from(index).ok())
}

/// The pool's on-chain address, which every protocol but Uniswap V4 keys its fallback on.
fn pool_address(component: &ProtocolComponent) -> Result<Address, FallbackError> {
    Address::from_str(&component.id).map_err(|_| {
        unencodable(component, format!("component id {} is not an address", component.id))
    })
}

/// Reads a static attribute as a big-endian unsigned integer.
fn attribute_u32(component: &ProtocolComponent, name: &str) -> Result<u32, FallbackError> {
    let raw = component
        .static_attributes
        .get(name)
        .ok_or_else(|| unencodable(component, format!("has no {name} static attribute")))?;
    let bytes = raw.as_ref();
    if bytes.len() > 4 {
        return Err(unencodable(component, format!("{name} is wider than four bytes")));
    }
    Ok(bytes
        .iter()
        .fold(0u32, |value, byte| (value << 8) | u32::from(*byte)))
}

fn unencodable(component: &ProtocolComponent, reason: String) -> FallbackError {
    FallbackError::MissingPoolData { component_id: component.id.clone(), reason }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::algorithm::test_utils as util;

    const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
    const USDC_WETH_USV3: &str = "0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640";

    fn address(hex: &str) -> Address {
        Address::from_str(hex).expect("valid address")
    }

    fn leg(id: &str, system: &str, attributes: &[(&str, Bytes)]) -> FallbackLeg {
        let mut component = util::component_with_protocol(
            id,
            system,
            &[util::token(1, "USDC"), util::token(2, "WETH")],
        );
        component.tokens = vec![address(USDC), address(WETH)];
        component.static_attributes = attributes
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect::<HashMap<_, _>>();
        FallbackLeg::new(
            id.to_string(),
            component,
            Box::new(util::MockProtocolSim::new(1.0)),
            num_bigint::BigUint::from(1u32),
        )
    }

    /// The shape tycho's `test_encode_uniswap_v3_fallback` packs to `…01{pool}`.
    #[test]
    fn test_uniswap_v3_user_data() {
        let json = fallback_user_data(
            &leg(USDC_WETH_USV3, "uniswap_v3", &[]),
            &address(USDC),
            &address(WETH),
        )
        .expect("encodable");

        assert_eq!(json, format!(r#"{{"protocol":"uniswap_v3","pool":"{USDC_WETH_USV3}"}}"#));
    }

    /// Canonical Uniswap V2 charges the 30 bps the router caps its fee at.
    #[test]
    fn test_uniswap_v2_user_data() {
        let pair = "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc";
        let json =
            fallback_user_data(&leg(pair, "uniswap_v2", &[]), &address(USDC), &address(WETH))
                .expect("encodable");

        assert_eq!(json, format!(r#"{{"protocol":"uniswap_v2","pair":"{pair}","fee_bps":30}}"#));
    }

    /// Fee and tick spacing come off the component, matching tycho's V4 decoder attributes.
    #[test]
    fn test_uniswap_v4_user_data() {
        let hook = "0x2222222222222222222222222222222222222222";
        let json = fallback_user_data(
            &leg(
                "0x1234",
                UNISWAP_V4_SYSTEM,
                &[
                    (UNISWAP_V4_FEE_ATTRIBUTE, Bytes::from(3000u32.to_be_bytes().to_vec())),
                    (UNISWAP_V4_TICK_SPACING_ATTRIBUTE, Bytes::from(vec![60u8])),
                    (HOOKS_ATTRIBUTE, address(hook)),
                ],
            ),
            &address(USDC),
            &address(WETH),
        )
        .expect("encodable");

        assert_eq!(
            json,
            format!(
                r#"{{"protocol":"uniswap_v4","fee":3000,"tick_spacing":60,"hook":"{hook}","hook_data":"0x"}}"#
            )
        );
    }

    /// A V4 pool with no fee attribute cannot be encoded, so it must not reach the router.
    #[test]
    fn test_uniswap_v4_without_fee() {
        let error = fallback_user_data(
            &leg(
                "0x1234",
                UNISWAP_V4_SYSTEM,
                &[(UNISWAP_V4_TICK_SPACING_ATTRIBUTE, Bytes::from(vec![60u8]))],
            ),
            &address(USDC),
            &address(WETH),
        )
        .expect_err("no fee");

        assert!(
            matches!(error, FallbackError::MissingPoolData { ref reason, .. } if reason.contains(UNISWAP_V4_FEE_ATTRIBUTE)),
            "unexpected error: {error}"
        );
    }

    /// Curve's indices are the pair's positions in the pool's own coin order, not the sort order.
    #[test]
    fn test_curve_user_data() {
        let pool = "0x3333333333333333333333333333333333333333";
        let coins = format!(r#"["{WETH}","{USDC}"]"#);
        let json = fallback_user_data(
            &leg(
                pool,
                "vm:curve",
                &[
                    (CURVE_POOL_TYPE_ATTRIBUTE, Bytes::from(vec![1u8])),
                    (CURVE_COINS_ATTRIBUTE, Bytes::from(coins.into_bytes())),
                ],
            ),
            &address(USDC),
            &address(WETH),
        )
        .expect("encodable");

        // USDC is the second coin, WETH the first, so the swap runs 1 -> 0.
        assert_eq!(
            json,
            format!(r#"{{"protocol":"curve","pool":"{pool}","pool_type":1,"i":1,"j":0}}"#)
        );
    }

    /// Fluid's direction is the dex's own token order, so the input token being token0 is `true`.
    #[test]
    fn test_fluid_v1_user_data() {
        let dex = "0x4444444444444444444444444444444444444444";
        let zero_to_one =
            fallback_user_data(&leg(dex, "fluid_v1", &[]), &address(USDC), &address(WETH))
                .expect("encodable");
        let one_to_zero =
            fallback_user_data(&leg(dex, "fluid_v1", &[]), &address(WETH), &address(USDC))
                .expect("encodable");

        assert_eq!(
            zero_to_one,
            format!(r#"{{"protocol":"fluid_v1","dex":"{dex}","zero2one":true}}"#)
        );
        assert_eq!(
            one_to_zero,
            format!(r#"{{"protocol":"fluid_v1","dex":"{dex}","zero2one":false}}"#)
        );
    }

    /// A system the router has no venue byte for is rejected rather than encoded as something else.
    #[test]
    fn test_unsupported_protocol_system() {
        let error =
            fallback_user_data(&leg("0x1234", "sushiswap_v2", &[]), &address(USDC), &address(WETH))
                .expect_err("not a fallback protocol");

        assert!(
            matches!(error, FallbackError::MissingPoolData { .. }),
            "unexpected error: {error}"
        );
    }
}

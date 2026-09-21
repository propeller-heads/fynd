//! The fallback pool as `TychoFallbackRouter` wants it: JSON on the tycho swap's `user_data`.
//!
//! `FallbackSwapEncoder` deserializes this into its own `FallbackSwapData`, then packs the
//! protocol byte and the protocol data the router's `_executeFallback` decodes. That enum is
//! private to tycho-execution, so this mirrors its wire shape; the tag is tycho's public
//! `FallbackProtocol::user_data_name`, and the tests below assert the JSON against the encodings
//! tycho's own tests expect.
//!
//! | protocol | fields | packed as |
//! |---|---|---|
//! | `uniswap_v2` | `pair`, `fee_bps` | `pair(20) ++ fee_bps(1)` |
//! | `uniswap_v3` | `pool` | `pool(20)` |
//! | `uniswap_v4` | `fee`, `tick_spacing` | `fee(3) ++ tick_spacing(3) ++ zero hook(20)` |
//! | `curve` | `pool`, `pool_type`, `i`, `j` | `pool(20) ++ pool_type(1) ++ i(1) ++ j(1)` |
//! | `fluid_v1` | `dex`, `zero2one` | `dex(20) ++ zero2one(1)` |
//! | `aerodrome_v1` | `pool` | `pool(20)` |
//!
//! Swap direction on the Uniswap family and Aerodrome comes from the sort order of the swap's own
//! tokens, so it is not carried here. Curve's coin indices and Fluid's `zero2one` are the pool's
//! own ordering, which no sort can recover, so both are read off the component. Hooked Uniswap V4
//! pools are not supported: `is_fallback_candidate` admits none, and no hook travels here.

use std::str::FromStr;

use serde::Serialize;
use tycho_execution::encoding::evm::swap_encoder::FallbackProtocol;
use tycho_simulation::{
    tycho_common::models::{protocol::ProtocolComponent, Address},
    tycho_core::simulation::protocol_sim::ProtocolSim,
};

use crate::{fallback::FallbackError, types::FallbackLeg};

/// The highest Uniswap V2 fee `TychoFallbackRouter` runs; it reverts above this.
const MAX_UNISWAP_V2_FEE_BPS: u8 = 30;

/// The fallback protocol and the data the router needs to run it. Each variant's snake-case name
/// is the protocol's `FallbackProtocol::user_data_name`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "fallback_protocol", rename_all = "snake_case")]
pub(super) enum FallbackSwapData {
    UniswapV2 { pair: Address, fee_bps: u8 },
    UniswapV3 { pool: Address },
    UniswapV4 { fee: u32, tick_spacing: i32 },
    Curve { pool: Address, pool_type: u8, i: u8, j: u8 },
    FluidV1 { dex: Address, zero2one: bool },
    AerodromeV1 { pool: Address },
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
/// no chain's router supports.
pub(crate) fn fallback_user_data(
    leg: &FallbackLeg,
    token_in: &Address,
    token_out: &Address,
) -> Result<String, FallbackError> {
    let component = leg.protocol_component();
    let protocol = fallback_protocol(component, leg.protocol_state(), token_in, token_out)?;
    serde_json::to_string(&protocol).map_err(|error| FallbackError::MissingPoolData {
        component_id: leg.component_id().to_string(),
        reason: error.to_string(),
    })
}

/// Reads `component` into the variant its protocol system calls for.
///
/// Selection calls this too, so a pool that could not be encoded is never chosen: the alternative
/// is a route that prices well and then fails at encoding time.
///
/// `FallbackProtocol::from_protocol_system` maps a fork to its base protocol, so the variant
/// serializes to the canonical name whatever the fork was called. `state` supplies the fee a V2
/// pool charges, which differs between forks.
///
/// # Errors
///
/// `MissingPoolData`, with what the component is missing or why the router would refuse it.
pub(super) fn fallback_protocol(
    component: &ProtocolComponent,
    state: &dyn ProtocolSim,
    token_in: &Address,
    token_out: &Address,
) -> Result<FallbackSwapData, FallbackError> {
    let protocol =
        FallbackProtocol::from_protocol_system(&component.protocol_system).ok_or_else(|| {
            FallbackError::MissingPoolData {
                component_id: component.id.clone(),
                reason: format!("{} is not a fallback protocol", component.protocol_system),
            }
        })?;
    let pool = pool_address(component)?;
    match protocol {
        FallbackProtocol::UniswapV2 => Ok(FallbackSwapData::UniswapV2 {
            pair: pool,
            fee_bps: uniswap_v2_fee_bps(component, state)?,
        }),
        FallbackProtocol::UniswapV3 => Ok(FallbackSwapData::UniswapV3 { pool }),
        FallbackProtocol::UniswapV4 => uniswap_v4_fallback(component),
        FallbackProtocol::Curve => curve_fallback(component, pool, token_in, token_out),
        FallbackProtocol::FluidV1 => Ok(FallbackSwapData::FluidV1 {
            dex: pool,
            zero2one: component.tokens.first() == Some(token_in),
        }),
        FallbackProtocol::AerodromeV1 => Ok(FallbackSwapData::AerodromeV1 { pool }),
    }
}

/// The fee a Uniswap V2 pool charges, in basis points, from the state tycho-simulation decoded.
///
/// Forks differ here (`pancakeswap_v2` charges 25 where `uniswap_v2` charges 30), and the router
/// reverts above `MAX_UNISWAP_V2_FEE_BPS`, so a pool over the cap is refused at selection rather
/// than failing on chain.
fn uniswap_v2_fee_bps(
    component: &ProtocolComponent,
    state: &dyn ProtocolSim,
) -> Result<u8, FallbackError> {
    let fee_bps = (state.fee() * 10_000.0).round();
    if fee_bps < 0.0 || fee_bps > f64::from(MAX_UNISWAP_V2_FEE_BPS) {
        return Err(FallbackError::MissingPoolData {
            component_id: component.id.clone(),
            reason: format!(
                "charges {fee_bps} bps, the fallback router accepts at most {MAX_UNISWAP_V2_FEE_BPS}"
            ),
        });
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(fee_bps as u8)
}

/// A Uniswap V4 pool identifies itself by its key, not by an address, so its fee and tick spacing
/// travel instead of a pool address. The key's hook is always zero: hooked pools are not
/// candidates.
fn uniswap_v4_fallback(component: &ProtocolComponent) -> Result<FallbackSwapData, FallbackError> {
    let fee = attribute_u32(component, "key_lp_fee")?;
    let tick_spacing = i32::try_from(attribute_u32(component, "tick_spacing")?).map_err(|_| {
        FallbackError::MissingPoolData {
            component_id: component.id.clone(),
            reason: "tick spacing does not fit int24".to_string(),
        }
    })?;
    Ok(FallbackSwapData::UniswapV4 { fee, tick_spacing })
}

/// Curve's `exchange` takes the pair's positions in the pool's own coin list, and a `pool_type`
/// that decides which `exchange` signature the router calls.
fn curve_fallback(
    component: &ProtocolComponent,
    pool: Address,
    token_in: &Address,
    token_out: &Address,
) -> Result<FallbackSwapData, FallbackError> {
    let pool_type = u8::try_from(attribute_u32(component, "pool_type")?).map_err(|_| {
        FallbackError::MissingPoolData {
            component_id: component.id.clone(),
            reason: "pool type does not fit a byte".to_string(),
        }
    })?;
    let coins = curve_coins(component);
    let (Some(i), Some(j)) = (coin_index(&coins, token_in), coin_index(&coins, token_out)) else {
        return Err(FallbackError::MissingPoolData {
            component_id: component.id.clone(),
            reason: "coins do not name both tokens of the pair".to_string(),
        });
    };
    Ok(FallbackSwapData::Curve { pool, pool_type, i, j })
}

/// The pool's coins, in the index order Curve's own `exchange` takes.
///
/// Curve calls them coins, not tokens, and indexes them by position in the pool. The component's
/// `tokens` carry no such promise, so they are only the fallback for a pool with no `coins`
/// attribute, where a two-coin pool leaves nothing to get wrong.
fn curve_coins(component: &ProtocolComponent) -> Vec<Address> {
    component
        .static_attributes
        .get("coins")
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
    Address::from_str(&component.id).map_err(|_| FallbackError::MissingPoolData {
        component_id: component.id.clone(),
        reason: format!("component id {} is not an address", component.id),
    })
}

/// Reads a static attribute as a big-endian unsigned integer.
fn attribute_u32(component: &ProtocolComponent, name: &str) -> Result<u32, FallbackError> {
    let raw = component
        .static_attributes
        .get(name)
        .ok_or_else(|| FallbackError::MissingPoolData {
            component_id: component.id.clone(),
            reason: format!("has no {name} static attribute"),
        })?;
    let bytes = raw.as_ref();
    if bytes.len() > 4 {
        return Err(FallbackError::MissingPoolData {
            component_id: component.id.clone(),
            reason: format!("{name} is wider than four bytes"),
        });
    }
    Ok(bytes
        .iter()
        .fold(0u32, |value, byte| (value << 8) | u32::from(*byte)))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tycho_simulation::tycho_common::Bytes;

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
        leg_with_fee(id, system, attributes, 0.003)
    }

    /// `leg` at a chosen fee, which only the Uniswap V2 family reads.
    fn leg_with_fee(id: &str, system: &str, attributes: &[(&str, Bytes)], fee: f64) -> FallbackLeg {
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
            component,
            Box::new(util::MockProtocolSim::new(1.0).with_fee(fee)),
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

        assert_eq!(
            json,
            format!(r#"{{"fallback_protocol":"uniswap_v3","pool":"{USDC_WETH_USV3}"}}"#)
        );
    }

    /// A V2 fork encodes as Uniswap V2 with its own fee: PancakeSwap's 25 bps rides through, and
    /// the variant serializes to the canonical name whatever the fork was called.
    #[test]
    fn test_uniswap_v2_fork_user_data() {
        let pair = "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc";
        let json = fallback_user_data(
            &leg_with_fee(pair, "pancakeswap_v2", &[], 0.0025),
            &address(USDC),
            &address(WETH),
        )
        .expect("encodable");

        assert_eq!(
            json,
            format!(r#"{{"fallback_protocol":"uniswap_v2","pair":"{pair}","fee_bps":25}}"#)
        );
    }

    /// A V2 pool over the router's 30 bps cap is refused here rather than reverting on chain.
    #[test]
    fn test_uniswap_v2_fee_above_cap() {
        let error = fallback_user_data(
            &leg_with_fee(
                "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc",
                "sushiswap_v2",
                &[],
                0.0031,
            ),
            &address(USDC),
            &address(WETH),
        )
        .expect_err("31 bps is over the cap");

        assert!(
            matches!(error, FallbackError::MissingPoolData { ref reason, .. } if reason.contains("31")),
            "unexpected error: {error}"
        );
    }

    /// Canonical Uniswap V2 charges the 30 bps the router caps its fee at.
    #[test]
    fn test_uniswap_v2_user_data() {
        let pair = "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc";
        let json =
            fallback_user_data(&leg(pair, "uniswap_v2", &[]), &address(USDC), &address(WETH))
                .expect("encodable");

        assert_eq!(
            json,
            format!(r#"{{"fallback_protocol":"uniswap_v2","pair":"{pair}","fee_bps":30}}"#)
        );
    }

    /// Fee and tick spacing come off the component, matching tycho's V4 decoder attributes. No
    /// hook travels: tycho's encoder writes the zero hook.
    #[test]
    fn test_uniswap_v4_user_data() {
        let json = fallback_user_data(
            &leg(
                "0x1234",
                "uniswap_v4",
                &[
                    ("key_lp_fee", Bytes::from(3000u32.to_be_bytes().to_vec())),
                    ("tick_spacing", Bytes::from(vec![60u8])),
                ],
            ),
            &address(USDC),
            &address(WETH),
        )
        .expect("encodable");

        assert_eq!(json, r#"{"fallback_protocol":"uniswap_v4","fee":3000,"tick_spacing":60}"#);
    }

    /// A V4 pool with no fee attribute cannot be encoded, so it must not reach the router.
    #[test]
    fn test_uniswap_v4_without_fee() {
        let error = fallback_user_data(
            &leg("0x1234", "uniswap_v4", &[("tick_spacing", Bytes::from(vec![60u8]))]),
            &address(USDC),
            &address(WETH),
        )
        .expect_err("no fee");

        assert!(
            matches!(error, FallbackError::MissingPoolData { ref reason, .. } if reason.contains("key_lp_fee")),
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
                    ("pool_type", Bytes::from(vec![1u8])),
                    ("coins", Bytes::from(coins.into_bytes())),
                ],
            ),
            &address(USDC),
            &address(WETH),
        )
        .expect("encodable");

        // USDC is the second coin, WETH the first, so the swap runs 1 -> 0.
        assert_eq!(
            json,
            format!(r#"{{"fallback_protocol":"curve","pool":"{pool}","pool_type":1,"i":1,"j":0}}"#)
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
            format!(r#"{{"fallback_protocol":"fluid_v1","dex":"{dex}","zero2one":true}}"#)
        );
        assert_eq!(
            one_to_zero,
            format!(r#"{{"fallback_protocol":"fluid_v1","dex":"{dex}","zero2one":false}}"#)
        );
    }

    /// The shape tycho's `test_encode_aerodrome_v1_fallback` packs to `…05{pool}`.
    #[test]
    fn test_aerodrome_v1_user_data() {
        let pool = "0x5555555555555555555555555555555555555555";
        let json =
            fallback_user_data(&leg(pool, "aerodrome_v1", &[]), &address(USDC), &address(WETH))
                .expect("encodable");

        assert_eq!(json, format!(r#"{{"fallback_protocol":"aerodrome_v1","pool":"{pool}"}}"#));
    }

    /// A Slipstream pool keeps Uniswap V3's `swap` and callback, so it encodes as `uniswap_v3`.
    #[test]
    fn test_slipstreams_user_data() {
        let json = fallback_user_data(
            &leg(USDC_WETH_USV3, "aerodrome_slipstreams", &[]),
            &address(USDC),
            &address(WETH),
        )
        .expect("encodable");

        assert_eq!(
            json,
            format!(r#"{{"fallback_protocol":"uniswap_v3","pool":"{USDC_WETH_USV3}"}}"#)
        );
    }

    /// Every protocol tycho's router runs encodes here, under the tag tycho's encoder expects.
    #[test]
    fn test_every_fallback_protocol_encodes_under_tychos_tag() {
        let systems = [
            "uniswap_v2",
            "sushiswap_v2",
            "uniswap_v3",
            "aerodrome_slipstreams",
            "uniswap_v4",
            "vm:curve",
            "fluid_v1",
            "aerodrome_v1",
        ];
        for system in systems {
            let attributes: Vec<(&str, Bytes)> = match system {
                "uniswap_v4" => vec![
                    ("key_lp_fee", Bytes::from(3000u32.to_be_bytes().to_vec())),
                    ("tick_spacing", Bytes::from(vec![60u8])),
                ],
                "vm:curve" => vec![("pool_type", Bytes::from(vec![1u8]))],
                _ => Vec::new(),
            };
            let leg = leg(USDC_WETH_USV3, system, &attributes);

            let json: serde_json::Value = serde_json::from_str(
                &fallback_user_data(&leg, &address(USDC), &address(WETH))
                    .unwrap_or_else(|error| panic!("{system} has no encoding: {error}")),
            )
            .expect("valid JSON");
            let expected = FallbackProtocol::from_protocol_system(system)
                .expect("a fallback protocol")
                .user_data_name();
            assert_eq!(json["fallback_protocol"], expected, "{system}");
        }
    }

    /// A system the router has no protocol byte for is rejected rather than encoded as something
    /// else.
    #[test]
    fn test_unsupported_protocol_system() {
        let error = fallback_user_data(
            &leg("0x1234", "vm:balancer_v2", &[]),
            &address(USDC),
            &address(WETH),
        )
        .expect_err("not a fallback protocol");

        assert!(
            matches!(error, FallbackError::MissingPoolData { .. }),
            "unexpected error: {error}"
        );
    }
}

use std::{collections::HashMap, env, fmt, time::Duration};

use tracing::{info, warn};
use tycho_simulation::{
    evm::{
        engine_db::tycho_db::PreCachedDB,
        protocol::{
            aerodrome_slipstreams::state::AerodromeSlipstreamsState,
            aerodrome_v1::state::AerodromeV1State,
            curve::CurveState,
            ekubo::state::EkuboState,
            ekubo_v3::state::EkuboV3State,
            erc4626::state::ERC4626State,
            filters::{
                balancer_v2_pool_filter, curve_filter, ekubo_v3_extension_filter,
                ekubo_v3_extension_filter_with_signed_exclusive_swap, erc4626_filter,
                fluid_v1_paused_pools_filter,
            },
            fluid::FluidV1,
            lunarbase::state::LunarBaseState,
            pancakeswap_v2::state::PancakeswapV2State,
            uniswap_v2::state::UniswapV2State,
            uniswap_v3::state::UniswapV3State,
            uniswap_v4::state::UniswapV4State,
            vm::state::EVMPoolState,
        },
        stream::ProtocolStreamBuilder,
        tycho_models::Chain,
    },
    rfq::{
        protocols::{
            bebop::{client_builder::BebopClientBuilder, state::BebopState},
            hashflow::{client_builder::HashflowClientBuilder, state::HashflowState},
        },
        stream::RFQStreamBuilder,
    },
    tycho_client::feed::component_tracker::ComponentFilter,
    tycho_core::Bytes,
};

use super::DataFeedError;

/// Opts a protocol into streaming its exclusive pools, e.g. `exclusive:ekubo_v3`.
///
/// Fynd-side only: stripped before registration, so Tycho sees the bare system name.
const EXCLUSIVE_PREFIX: &str = "exclusive:";

/// Protocol systems that offer an exclusive-liquidity stream variant, i.e. the ones that may be
/// requested with the `exclusive:` prefix.
const EXCLUSIVE_CAPABLE_PROTOCOLS: &[&str] = &["ekubo_v3"];

/// The `exclusive:` prefix was applied to a protocol system that has no exclusive variant.
#[derive(Debug, thiserror::Error)]
#[error(
    "protocol '{requested}' has no exclusive-liquidity variant; '{EXCLUSIVE_PREFIX}' is only \
     supported for: {supported}",
    supported = EXCLUSIVE_CAPABLE_PROTOCOLS.join(", ")
)]
pub struct UnsupportedExclusiveProtocol {
    /// The protocol system the prefix was applied to.
    requested: String,
}

/// A requested protocol system together with the liquidity variant to stream for it.
///
/// `parse` and the `Display` impl round-trip: displaying one yields a `--protocols` entry that
/// parses back to the same value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolSpec {
    /// Tycho protocol system name. Never carries the `exclusive:` prefix.
    pub system: String,
    /// Whether to register the filter that also admits exclusive pools.
    pub exclusive: bool,
}

impl ProtocolSpec {
    /// A protocol system streaming public liquidity only.
    pub fn public(system: impl Into<String>) -> Self {
        Self { system: system.into(), exclusive: false }
    }

    /// Parses a single `--protocols` entry.
    ///
    /// # Errors
    ///
    /// Returns `UnsupportedExclusiveProtocol` when the `exclusive:` prefix is applied to a protocol
    /// system that has no exclusive variant. Unrecognised protocol systems without the prefix are
    /// accepted here and skipped with a warning during registration.
    pub fn parse(entry: &str) -> Result<Self, UnsupportedExclusiveProtocol> {
        let Some(system) = entry.strip_prefix(EXCLUSIVE_PREFIX) else {
            return Ok(Self::public(entry));
        };
        if !EXCLUSIVE_CAPABLE_PROTOCOLS.contains(&system) {
            return Err(UnsupportedExclusiveProtocol { requested: system.to_string() });
        }
        Ok(Self { system: system.to_string(), exclusive: true })
    }
}

impl fmt::Display for ProtocolSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.exclusive {
            write!(f, "{EXCLUSIVE_PREFIX}{}", self.system)
        } else {
            f.write_str(&self.system)
        }
    }
}

/// Register DEX protocol decoders for test tooling (record-market).
///
/// Wrapper over [`register_exchanges`] so the recorder builds the same protocol stream as
/// production without exposing the crate-private `DataFeedError`.
#[cfg(feature = "test-utils")]
pub fn register_exchanges_for_recording(
    builder: ProtocolStreamBuilder,
    tvl_filter: ComponentFilter,
    entries: &[String],
) -> Result<ProtocolStreamBuilder, String> {
    register_exchanges(builder, tvl_filter, entries).map_err(|e| e.to_string())
}

/// Parses every `--protocols` entry, rejecting a list with no unambiguous reading.
///
/// Registration is keyed by protocol system, so naming one system both with and without the
/// `exclusive:` prefix would silently keep whichever entry came last. Callers that expand a
/// protocol list (`fynd_rpc::protocols::resolve_protocols`) merge the variants before getting here;
/// a hand-assembled list gets an error instead of an order-dependent stream.
fn parse_protocols(entries: &[String]) -> Result<Vec<ProtocolSpec>, DataFeedError> {
    let mut protocols = Vec::with_capacity(entries.len());
    for entry in entries {
        protocols
            .push(ProtocolSpec::parse(entry).map_err(|e| DataFeedError::Config(e.to_string()))?);
    }

    let mut variants: HashMap<&str, bool> = HashMap::new();
    for protocol in &protocols {
        if variants
            .insert(protocol.system.as_str(), protocol.exclusive)
            .is_some_and(|previous| previous != protocol.exclusive)
        {
            return Err(DataFeedError::Config(format!(
                "protocol '{}' requested both with and without the '{EXCLUSIVE_PREFIX}' prefix",
                protocol.system
            )));
        }
    }
    Ok(protocols)
}

/// Register DEX protocol decoders on a [`ProtocolStreamBuilder`].
///
/// Entries may carry the `exclusive:` prefix to select the protocol's exclusive-liquidity stream
/// variant; doing so for a protocol without one is a configuration error, as is naming one protocol
/// both with and without the prefix.
pub(crate) fn register_exchanges(
    mut builder: ProtocolStreamBuilder,
    tvl_filter: ComponentFilter,
    entries: &[String],
) -> Result<ProtocolStreamBuilder, DataFeedError> {
    for protocol in parse_protocols(entries)? {
        match protocol.system.as_str() {
            "uniswap_v2" => {
                builder =
                    builder.exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None);
            }
            "sushiswap_v2" => {
                builder =
                    builder.exchange::<UniswapV2State>("sushiswap_v2", tvl_filter.clone(), None);
            }
            "pancakeswap_v2" => {
                builder = builder.exchange::<PancakeswapV2State>(
                    "pancakeswap_v2",
                    tvl_filter.clone(),
                    None,
                );
            }
            "uniswap_v3" => {
                builder =
                    builder.exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None);
            }
            "pancakeswap_v3" => {
                builder =
                    builder.exchange::<UniswapV3State>("pancakeswap_v3", tvl_filter.clone(), None);
            }
            "vm:balancer_v2" => {
                builder = builder.exchange::<EVMPoolState<PreCachedDB>>(
                    "vm:balancer_v2",
                    tvl_filter.clone(),
                    Some(balancer_v2_pool_filter),
                );
            }
            "uniswap_v4" => {
                builder =
                    builder.exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None);
            }
            "ekubo_v2" => {
                builder = builder.exchange::<EkuboState>("ekubo_v2", tvl_filter.clone(), None);
            }
            "vm:curve" => {
                // The hybrid CurveState with tycho-simulation's own curve_filter, which drops
                // the components CurveState cannot quote correctly (oracle/rate-bearing/rebasing
                // coins) — the source of the overestimation that forced the temporary
                // full-EVM fallback (see #318); fixed upstream in tycho-simulation 0.338.0.
                builder = builder.exchange::<CurveState>(
                    "vm:curve",
                    tvl_filter.clone(),
                    Some(curve_filter),
                );
            }
            "uniswap_v4_hooks" => {
                builder = builder.exchange::<UniswapV4State>(
                    "uniswap_v4_hooks",
                    tvl_filter.clone(),
                    None,
                );
            }
            "vm:maverick_v2" => {
                builder = builder.exchange::<EVMPoolState<PreCachedDB>>(
                    "vm:maverick_v2",
                    tvl_filter.clone(),
                    None,
                );
            }
            "vm:bopamm" => {
                builder = builder.exchange::<EVMPoolState<PreCachedDB>>(
                    "vm:bopamm",
                    tvl_filter.clone(),
                    None,
                );
            }
            "vm:fermiswap" => {
                builder = builder.exchange::<EVMPoolState<PreCachedDB>>(
                    "vm:fermiswap",
                    tvl_filter.clone(),
                    None,
                );
            }
            "fluid_v1" => {
                builder = builder.exchange::<FluidV1>(
                    "fluid_v1",
                    tvl_filter.clone(),
                    Some(fluid_v1_paused_pools_filter),
                );
            }
            "aerodrome_v1" => {
                builder =
                    builder.exchange::<AerodromeV1State>("aerodrome_v1", tvl_filter.clone(), None);
            }
            "aerodrome_slipstreams" => {
                builder = builder.exchange::<AerodromeSlipstreamsState>(
                    "aerodrome_slipstreams",
                    tvl_filter.clone(),
                    None,
                );
            }
            "erc4626" => {
                builder = builder.exchange::<ERC4626State>(
                    "erc4626",
                    tvl_filter.clone(),
                    Some(erc4626_filter),
                );
            }
            "velodrome_slipstreams" => {
                builder = builder.exchange::<AerodromeSlipstreamsState>(
                    "velodrome_slipstreams",
                    tvl_filter.clone(),
                    None,
                );
            }
            "ekubo_v3" => {
                // SignedExclusiveSwap pools need a controller signature per swap, so they are
                // only streamed when the deployment explicitly opts in.
                let filter = if protocol.exclusive {
                    info!("Including exclusive liquidity for ekubo_v3");
                    ekubo_v3_extension_filter_with_signed_exclusive_swap
                } else {
                    ekubo_v3_extension_filter
                };
                builder =
                    builder.exchange::<EkuboV3State>("ekubo_v3", tvl_filter.clone(), Some(filter));
            }
            "quickswap_v2" => {
                builder =
                    builder.exchange::<UniswapV2State>("quickswap_v2", tvl_filter.clone(), None);
            }
            "blazeswap_v2" => {
                builder =
                    builder.exchange::<UniswapV2State>("blazeswap_v2", tvl_filter.clone(), None);
            }
            "sparkdex_v3" => {
                builder =
                    builder.exchange::<UniswapV3State>("sparkdex_v3", tvl_filter.clone(), None);
            }
            "enosys_v3" => {
                builder = builder.exchange::<UniswapV3State>("enosys_v3", tvl_filter.clone(), None);
            }
            "lunarbase" => {
                builder = builder.exchange::<LunarBaseState>("lunarbase", tvl_filter.clone(), None);
            }
            p if p.starts_with("rfq:") => {
                // RFQ protocols are handled in register_rfq
                continue;
            }
            _ => {
                warn!("Skipping unknown protocol: {}", protocol);
            }
        }
    }
    Ok(builder)
}

pub(crate) fn register_rfq(
    mut rfq_stream_builder: RFQStreamBuilder,
    chain: Chain,
    min_tvl: f64,
    protocols: &[String],
    rfq_tokens: std::collections::HashSet<Bytes>,
) -> Result<RFQStreamBuilder, DataFeedError> {
    for protocol in protocols {
        match protocol.as_str() {
            "rfq:bebop" => {
                let key = get_env("BEBOP_KEY")?;
                info!("Adding {protocol} RFQ client...");
                let bebop_client = BebopClientBuilder::new(chain, key)
                    .tokens(rfq_tokens.clone())
                    .tvl_threshold(min_tvl)
                    .build()
                    .map_err(|e| DataFeedError::StreamError(e.to_string()))?;
                rfq_stream_builder =
                    rfq_stream_builder.add_client::<BebopState>("bebop", Box::new(bebop_client));
            }
            "rfq:hashflow" => {
                let user = get_env("HASHFLOW_USER")?;
                let key = get_env("HASHFLOW_KEY")?;
                info!("Adding {protocol} RFQ client...");
                let hashflow_client = HashflowClientBuilder::new(chain, user, key)
                    .tokens(rfq_tokens.clone())
                    .tvl_threshold(min_tvl)
                    .poll_time(Duration::from_secs(30))
                    .build()
                    .map_err(|e| DataFeedError::StreamError(e.to_string()))?;
                rfq_stream_builder = rfq_stream_builder
                    .add_client::<HashflowState>("hashflow", Box::new(hashflow_client));
            }
            p if p.starts_with("rfq:") => {
                warn!("Skipping unknown RFQ protocol: {}", p);
            }
            _ => {}
        }
    }
    Ok(rfq_stream_builder)
}

fn get_env(var: &str) -> Result<String, DataFeedError> {
    env::var(var).map_err(|_| DataFeedError::Config(format!("{} env var not set", var)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn register(entries: &[&str]) -> Result<ProtocolStreamBuilder, DataFeedError> {
        register_exchanges(
            ProtocolStreamBuilder::new("localhost:0", Chain::Ethereum),
            ComponentFilter::with_tvl_range(1.0, 10.0),
            &entries
                .iter()
                .map(|entry| (*entry).to_string())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn test_parse_plain_protocol() {
        let protocol = ProtocolSpec::parse("uniswap_v3").unwrap();
        assert_eq!(protocol, ProtocolSpec { system: "uniswap_v3".to_string(), exclusive: false });
    }

    #[test]
    fn test_parse_exclusive_protocol() {
        let protocol = ProtocolSpec::parse("exclusive:ekubo_v3").unwrap();
        assert_eq!(protocol, ProtocolSpec { system: "ekubo_v3".to_string(), exclusive: true });
    }

    #[test]
    fn test_parse_exclusive_unsupported_protocol() {
        let err = ProtocolSpec::parse("exclusive:uniswap_v3").unwrap_err();
        assert!(err
            .to_string()
            .contains("has no exclusive-liquidity variant"));
    }

    #[test]
    fn test_parse_exclusive_without_protocol() {
        assert!(ProtocolSpec::parse("exclusive:").is_err());
    }

    #[test]
    fn test_parse_leaves_other_prefixes_intact() {
        assert_eq!(
            ProtocolSpec::parse("rfq:bebop").unwrap(),
            ProtocolSpec { system: "rfq:bebop".to_string(), exclusive: false }
        );
        assert_eq!(
            ProtocolSpec::parse("vm:curve").unwrap(),
            ProtocolSpec { system: "vm:curve".to_string(), exclusive: false }
        );
    }

    #[test]
    fn test_display_round_trips() {
        for entry in ["uniswap_v3", "exclusive:ekubo_v3", "rfq:bebop", "vm:curve"] {
            let protocol = ProtocolSpec::parse(entry).unwrap();
            assert_eq!(protocol.to_string(), entry);
            assert_eq!(ProtocolSpec::parse(&protocol.to_string()).unwrap(), protocol);
        }
    }

    #[test]
    fn test_register_exchanges_accepts_exclusive_ekubo_v3() {
        assert!(register(&["uniswap_v3", "exclusive:ekubo_v3"]).is_ok());
    }

    #[test]
    fn test_register_exchanges_rejects_unsupported_exclusive() {
        let Err(err) = register(&["exclusive:uniswap_v3"]) else {
            panic!("expected `exclusive:uniswap_v3` to be rejected");
        };
        assert!(matches!(err, DataFeedError::Config(_)), "expected a config error, got {err:?}");
    }

    #[test]
    fn test_register_exchanges_skips_unknown_protocol() {
        assert!(register(&["not_a_protocol"]).is_ok());
    }

    #[test]
    fn test_register_exchanges_rejects_conflicting_variants() {
        for protocols in [["ekubo_v3", "exclusive:ekubo_v3"], ["exclusive:ekubo_v3", "ekubo_v3"]] {
            let Err(err) = register(&protocols) else {
                panic!("expected {protocols:?} to be rejected");
            };
            assert!(
                err.to_string()
                    .contains("both with and without"),
                "unexpected error for {protocols:?}: {err}"
            );
        }
    }

    #[test]
    fn test_register_exchanges_allows_repeated_protocol() {
        assert!(register(&["uniswap_v3", "uniswap_v3"]).is_ok());
    }
}

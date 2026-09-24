use std::{collections::HashMap, env, fmt, time::Duration};

use tokio_stream::Stream;
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
            ramses_v3::state::RamsesV3State,
            uniswap_v2::state::UniswapV2State,
            uniswap_v3::state::UniswapV3State,
            uniswap_v4::state::UniswapV4State,
            vm::state::EVMPoolState,
        },
        stream::ProtocolStreamBuilder,
        tycho_models::Chain,
    },
    price_level_stream::{config::default_served_pamms, stream::PriceLevelStreamBuilder},
    protocol::models::Update,
    rfq::{
        protocols::{
            bebop::{client_builder::BebopClientBuilder, state::BebopState},
            hashflow::{client_builder::HashflowClientBuilder, state::HashflowState},
            metric::{client_builder::MetricClientBuilder, state::MetricState},
        },
        stream::RFQStreamBuilder,
    },
    tycho_client::feed::{component_tracker::ComponentFilter, synchronizer::ComponentWithState},
    tycho_common::models::token::Token,
    tycho_core::Bytes,
};

use super::DataFeedError;
use crate::fallback::FALLBACK_PREFIX;

/// Opts a protocol into streaming its exclusive pools, e.g. `exclusive:ekubo_v3`.
///
/// Fynd-side only: stripped before registration, so Tycho sees the bare system name.
const EXCLUSIVE_PREFIX: &str = "exclusive:";

/// Protocol systems that offer an exclusive-liquidity stream variant, i.e. the ones that may be
/// requested with the `exclusive:` prefix.
const EXCLUSIVE_CAPABLE_PROTOCOLS: &[&str] = &["ekubo_v3"];

/// Marks a `--protocols` entry served from the Titan pAMM price level stream rather than from
/// Tycho, e.g. `pricelevelstream:fermiswap`.
const PRICE_LEVEL_STREAM_PREFIX: &str = "pricelevelstream:";

/// Marks a `--protocols` entry served from an RFQ client rather than from Tycho, e.g.
/// `rfq:bebop`.
const RFQ_PREFIX: &str = "rfq:";

/// Marks a `--protocols` entry that drops a protocol system from the list rather than adding one,
/// e.g. `exclude:vm:fermiswap`.
pub const EXCLUDE_PREFIX: &str = "exclude:";

/// Uniswap V4 hook contracts whose pools are dropped from the stream.
///
/// Compared against the component's `hooks` static attribute, so one entry covers every pool
/// the hook is attached to; V4 component IDs are pool IDs, which the component blocklist would
/// need one at a time.
///
/// All entries so far are one family of dynamic-fee hooks: `owner()` returns
/// `0x80390a818c9390ea6190160bd7fddff2cdbdc0ab` and each exposes the same
/// `getFeeConfig()` / `gasThreshold()` interface, with bytecode that grows from deployment to
/// deployment. Listed in first-pool block order.
const BLOCKED_UNISWAP_V4_HOOKS: &[&str] = &[
    "0x051c99a4583a7137833ad048af442909426d00c4",
    "0xfa439315b015a4c283ded9815a4af6cef0b90880",
    "0x1b3b249ee8afdfdc7af0f06a0765de1a49cf80c4",
    "0x74a7fd29718c6d0124011116d05e62090eff4880",
    "0xe502d9798d60d4302e46786ff9fbfb548266c880",
    "0x32514d03d9f73383ad43c6257ad7f4d4588640c4",
    "0xaebe208bb46e005321b3ea9ade08dc57b90cc0c4",
    "0x1e000786dc0a1c80eef758b485e90c7193d9c0c4",
    "0xbf1a0a8608593db7580044b6501fefb310c280c4",
];

/// Keeps a Uniswap V4 component unless its hook is in [`BLOCKED_UNISWAP_V4_HOOKS`].
fn uniswap_v4_hook_filter(component: &ComponentWithState) -> bool {
    let Some(hook) = component
        .component
        .static_attributes
        .get("hooks")
    else {
        return true;
    };
    let hook = hook.to_string();
    if BLOCKED_UNISWAP_V4_HOOKS.contains(&hook.as_str()) {
        info!(
            component_id = %component.component.id,
            hook,
            "dropping Uniswap V4 component with blocked hook"
        );
        return false;
    }
    true
}

/// The only chain the Titan pAMM price level stream serves.
///
/// Tracks tycho-simulation's `default_served_pamms`, whose venue addresses are all Ethereum
/// mainnet deployments; it carries no chain of its own, so this has to move when it gains a venue
/// elsewhere.
const PRICE_LEVEL_STREAM_CHAIN: Chain = Chain::Ethereum;

/// The chains Metric's executor is deployed on.
///
/// Tracks tycho-execution's `executor_addresses.json`. A Metric leg on any other chain would price
/// from the RFQ stream and then fail to encode, so the entry is rejected at registration instead.
const METRIC_CHAINS: &[Chain] = &[Chain::Base, Chain::Robinhood];

/// Whether a `--protocols` entry names a Tycho protocol system.
///
/// The RFQ clients and the pAMM price level stream each connect to their own endpoint, so their
/// entries never appear among the protocol systems Tycho serves.
pub fn is_tycho_system(entry: &str) -> bool {
    !entry.starts_with(RFQ_PREFIX) && !entry.starts_with(PRICE_LEVEL_STREAM_PREFIX)
}

/// Whether any requested protocol is streamed from Tycho.
///
/// A list naming only RFQ or price level stream entries needs no Tycho protocol stream at all.
pub(crate) fn has_tycho_protocols(protocols: &[String]) -> bool {
    protocols
        .iter()
        .any(|protocol| is_tycho_system(protocol))
}

/// Whether the components labelled `protocol_system` are the ones a `--protocols` entry asked for.
///
/// Most entries name their own label. An `exclusive:{system}` entry selects the system's
/// exclusive-liquidity stream variant, and the prefix is stripped before registration, so its
/// components arrive under the bare system name. A `pricelevelstream:{pamm}` entry names the pAMM
/// to stream, and its components arrive labelled `fallback:{pamm}`: the stream serves every venue
/// through the `TychoFallbackRouter`, so both prefixes answer for the same entry.
pub fn matches_streamed_system(entry: &str, protocol_system: &str) -> bool {
    let entry = entry
        .strip_prefix(EXCLUSIVE_PREFIX)
        .unwrap_or(entry);
    if entry == protocol_system {
        return true;
    }
    match (
        entry.strip_prefix(PRICE_LEVEL_STREAM_PREFIX),
        protocol_system.strip_prefix(FALLBACK_PREFIX),
    ) {
        (Some(requested_venue), Some(streamed_venue)) => requested_venue == streamed_venue,
        _ => false,
    }
}

/// Whether any requested protocol is served by an RFQ client.
pub(crate) fn has_rfq_protocols(protocols: &[String]) -> bool {
    protocols
        .iter()
        .any(|protocol| protocol.starts_with(RFQ_PREFIX))
}

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

/// Parses a `--protocols` entry that drops a protocol system, e.g. `exclude:vm:fermiswap`.
///
/// Returns `None` for an entry that names a protocol to stream instead. The part after the prefix
/// goes through [`ProtocolSpec::parse`], so `exclude:ekubo_v3` and `exclude:exclusive:ekubo_v3`
/// both name the system `ekubo_v3` and a malformed exclusion fails the same way a malformed
/// request does. An entry naming nothing (`exclude:`) yields an empty system for the caller to
/// reject.
///
/// # Errors
///
/// Returns `UnsupportedExclusiveProtocol` when the excluded entry carries the `exclusive:` prefix
/// for a protocol system that has no exclusive variant.
pub fn parse_exclusion(entry: &str) -> Option<Result<String, UnsupportedExclusiveProtocol>> {
    let excluded = entry.strip_prefix(EXCLUDE_PREFIX)?;
    Some(ProtocolSpec::parse(excluded).map(|protocol| protocol.system))
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

/// Registers the production protocol decoders on a [`ProtocolStreamBuilder`] for test tooling
/// (the bench-harness live capture).
///
/// Wrapper over `register_exchanges` that returns the error as a `String`, so the crate-private
/// `DataFeedError` stays private.
#[cfg(feature = "test-utils")]
pub fn register_exchanges_for_live_capture(
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
pub fn register_exchanges(
    mut builder: ProtocolStreamBuilder,
    tvl_filter: ComponentFilter,
    entries: &[String],
) -> Result<ProtocolStreamBuilder, DataFeedError> {
    for protocol in parse_protocols(entries)? {
        if !is_tycho_system(&protocol.system) {
            // Handled by register_rfq and open_price_level_stream, which stream from their
            // own endpoints rather than from Tycho.
            continue;
        }
        builder = match register_exchange(builder, &protocol, &tvl_filter) {
            Registration::Registered(registered) => registered,
            Registration::NoDecoder(unchanged) => {
                warn!("Skipping unknown protocol: {}", protocol);
                unchanged
            }
        };
    }
    Ok(builder)
}

/// What [`register_exchange`] did with one protocol system.
enum Registration {
    /// fynd has a decoder for the system, and the builder now carries it.
    Registered(ProtocolStreamBuilder),
    /// fynd has no decoder for the system; the builder is unchanged.
    NoDecoder(ProtocolStreamBuilder),
}

/// Registers the decoder for one Tycho protocol system.
fn register_exchange(
    builder: ProtocolStreamBuilder,
    protocol: &ProtocolSpec,
    tvl_filter: &ComponentFilter,
) -> Registration {
    let registered = match protocol.system.as_str() {
        "uniswap_v2" => builder.exchange::<UniswapV2State>("uniswap_v2", tvl_filter.clone(), None),
        "sushiswap_v2" => {
            builder.exchange::<UniswapV2State>("sushiswap_v2", tvl_filter.clone(), None)
        }
        "pancakeswap_v2" => {
            builder.exchange::<PancakeswapV2State>("pancakeswap_v2", tvl_filter.clone(), None)
        }
        "uniswap_v3" => builder.exchange::<UniswapV3State>("uniswap_v3", tvl_filter.clone(), None),
        "sushiswap_v3" => {
            builder.exchange::<UniswapV3State>("sushiswap_v3", tvl_filter.clone(), None)
        }
        "robinswap_v3" => {
            builder.exchange::<UniswapV3State>("robinswap_v3", tvl_filter.clone(), None)
        }
        "ramses_v3" => builder.exchange::<RamsesV3State>("ramses_v3", tvl_filter.clone(), None),
        "pancakeswap_v3" => {
            builder.exchange::<UniswapV3State>("pancakeswap_v3", tvl_filter.clone(), None)
        }
        "vm:balancer_v2" => builder.exchange::<EVMPoolState<PreCachedDB>>(
            "vm:balancer_v2",
            tvl_filter.clone(),
            Some(balancer_v2_pool_filter),
        ),
        "uniswap_v4" => builder.exchange::<UniswapV4State>("uniswap_v4", tvl_filter.clone(), None),
        "ekubo_v2" => builder.exchange::<EkuboState>("ekubo_v2", tvl_filter.clone(), None),
        "vm:curve" => {
            // The hybrid CurveState with tycho-simulation's own curve_filter, which drops
            // the components CurveState cannot quote correctly (oracle/rate-bearing/rebasing
            // coins) — the source of the overestimation that forced the temporary
            // full-EVM fallback (see #318); fixed upstream in tycho-simulation 0.338.0.
            builder.exchange::<CurveState>("vm:curve", tvl_filter.clone(), Some(curve_filter))
        }
        "uniswap_v4_hooks" => builder.exchange::<UniswapV4State>(
            "uniswap_v4_hooks",
            tvl_filter.clone(),
            Some(uniswap_v4_hook_filter),
        ),
        "vm:maverick_v2" => builder.exchange::<EVMPoolState<PreCachedDB>>(
            "vm:maverick_v2",
            tvl_filter.clone(),
            None,
        ),
        "vm:bopamm" => {
            builder.exchange::<EVMPoolState<PreCachedDB>>("vm:bopamm", tvl_filter.clone(), None)
        }
        "vm:fermiswap" => {
            builder.exchange::<EVMPoolState<PreCachedDB>>("vm:fermiswap", tvl_filter.clone(), None)
        }
        "fluid_v1" => builder.exchange::<FluidV1>(
            "fluid_v1",
            tvl_filter.clone(),
            Some(fluid_v1_paused_pools_filter),
        ),
        "aerodrome_v1" => {
            builder.exchange::<AerodromeV1State>("aerodrome_v1", tvl_filter.clone(), None)
        }
        "aerodrome_slipstreams" => builder.exchange::<AerodromeSlipstreamsState>(
            "aerodrome_slipstreams",
            tvl_filter.clone(),
            None,
        ),
        "erc4626" => {
            builder.exchange::<ERC4626State>("erc4626", tvl_filter.clone(), Some(erc4626_filter))
        }
        "velodrome_slipstreams" => builder.exchange::<AerodromeSlipstreamsState>(
            "velodrome_slipstreams",
            tvl_filter.clone(),
            None,
        ),
        "up_v3" => {
            // Up is a Slipstream fork: its pools carry the same `tick_spacing` and
            // `default_fee` static attributes Aerodrome's decoder reads.
            builder.exchange::<AerodromeSlipstreamsState>("up_v3", tvl_filter.clone(), None)
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
            builder.exchange::<EkuboV3State>("ekubo_v3", tvl_filter.clone(), Some(filter))
        }
        "quickswap_v2" => {
            builder.exchange::<UniswapV2State>("quickswap_v2", tvl_filter.clone(), None)
        }
        "lunarbase" => builder.exchange::<LunarBaseState>("lunarbase", tvl_filter.clone(), None),
        _ => return Registration::NoDecoder(builder),
    };
    Registration::Registered(registered)
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
            "rfq:metric" => {
                if !METRIC_CHAINS.contains(&chain) {
                    return Err(DataFeedError::Config(format!(
                        "{protocol} is only deployed on {}, but this feed runs on {chain}",
                        METRIC_CHAINS
                            .iter()
                            .map(|chain| chain.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
                let api_key = get_env("METRIC_API_KEY")?;
                info!("Adding {protocol} RFQ client...");
                let metric_client = MetricClientBuilder::new(chain)
                    .tokens(rfq_tokens.clone())
                    .tvl_threshold(min_tvl)
                    .api_key(Some(api_key))
                    .build()
                    .map_err(|e| DataFeedError::StreamError(e.to_string()))?;
                rfq_stream_builder =
                    rfq_stream_builder.add_client::<MetricState>("metric", Box::new(metric_client));
            }
            p if p.starts_with(RFQ_PREFIX) => {
                warn!("Skipping unknown RFQ protocol: {}", p);
            }
            _ => {}
        }
    }
    Ok(rfq_stream_builder)
}

/// Opens the Titan pAMM price level stream for the requested `pricelevelstream:` venues.
///
/// Returns `None` when no entry names the stream. Every named venue must be one of the venues
/// tycho-simulation knows how to execute against ([`default_served_pamms`]); a name outside that
/// set is a configuration error rather than a warning, because these entries are always written
/// by hand and a typo would otherwise silently stream nothing.
///
/// The stream reconnects on its own for as long as it is polled, so — unlike the RFQ clients —
/// it needs no supervising task.
///
/// A venue served here may also be integrated as a Tycho protocol system (FermiSwap is also
/// `vm:fermiswap`), in which case both price the same maker inventory. Streaming both
/// double-counts that liquidity, so drop the Tycho one from `--protocols` instead.
///
/// # Errors
///
/// Returns [`DataFeedError::Config`] if the chain is not [`PRICE_LEVEL_STREAM_CHAIN`], or if an
/// entry names a venue that is not served.
pub(crate) fn open_price_level_stream(
    chain: Chain,
    protocols: &[String],
    tokens: &HashMap<Bytes, Token>,
) -> Result<Option<impl Stream<Item = Update> + Send>, DataFeedError> {
    let venues: Vec<&str> = protocols
        .iter()
        .filter_map(|protocol| protocol.strip_prefix(PRICE_LEVEL_STREAM_PREFIX))
        .collect();
    if venues.is_empty() {
        return Ok(None);
    }
    if chain != PRICE_LEVEL_STREAM_CHAIN {
        return Err(DataFeedError::Config(format!(
            "the pAMM price level stream serves {PRICE_LEVEL_STREAM_CHAIN} only, but this feed \
             runs on {chain}"
        )));
    }

    let served = default_served_pamms();
    let mut builder = PriceLevelStreamBuilder::new().with_tokens(tokens.clone());
    for venue in venues {
        let Some(config) = served
            .iter()
            .find(|config| config.protocol == venue)
        else {
            return Err(DataFeedError::Config(format!(
                "unknown pAMM '{venue}' for the price level stream; served venues are: {}",
                served
                    .iter()
                    .map(|config| config.protocol.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        info!("Adding {PRICE_LEVEL_STREAM_PREFIX}{venue} price level venue...");
        builder = builder.add_pamm(config.clone());
    }
    Ok(Some(builder.build()))
}

/// The raw Tycho messages [`open_recording_stream`] hands out.
#[cfg(feature = "test-utils")]
pub type RecordedMessages = tokio::sync::mpsc::Receiver<
    Result<
        tycho_simulation::tycho_client::feed::FeedMessage,
        tycho_simulation::tycho_client::feed::BlockSynchronizerError,
    >,
>;

/// Opens the raw Tycho stream a market recording stores.
///
/// Subscribes to every Tycho protocol system in `entries` that fynd has a decoder for, so a
/// recording holds only what [`decode_recorded_messages`] can decode, and applies the component
/// blocklist `ProtocolStreamBuilder` applies. A system with no decoder is logged once and left
/// out. The stream hands out raw messages because a decoded state is not always serializable.
///
/// # Errors
///
/// Fails when the protocol list is invalid, names no system fynd decodes, or the stream cannot
/// start.
#[cfg(feature = "test-utils")]
pub async fn open_recording_stream(
    tycho_url: &str,
    chain: Chain,
    auth_key: String,
    tvl_filter: ComponentFilter,
    entries: &[String],
) -> Result<(tokio::task::JoinHandle<()>, RecordedMessages), String> {
    use tycho_simulation::{tycho_client::stream::TychoStreamBuilder, utils::default_blocklist};

    let systems = select_decodable_systems(chain, entries).map_err(|e| e.to_string())?;
    if systems.is_empty() {
        return Err(format!("no protocol system in {entries:?} has a decoder"));
    }
    let mut stream_builder = TychoStreamBuilder::new(tycho_url, chain)
        .blocklisted_ids(default_blocklist())
        .auth_key(Some(auth_key));
    for system in &systems {
        stream_builder = stream_builder.exchange(system, tvl_filter.clone());
    }
    stream_builder
        .build()
        .await
        .map_err(|e| e.to_string())
}

/// Selects the Tycho protocol systems in `entries` that `register_exchanges` has a decoder for.
///
/// Strips the `exclusive:` prefix and drops RFQ and price level stream entries, which Tycho does
/// not serve. Logs each system with no decoder.
#[cfg(feature = "test-utils")]
fn select_decodable_systems(
    chain: Chain,
    entries: &[String],
) -> Result<Vec<String>, DataFeedError> {
    let mut systems = Vec::new();
    for protocol in parse_protocols(entries)? {
        if !is_tycho_system(&protocol.system) {
            continue;
        }
        // Registration on a builder that never connects is how to ask for a decoder: the match
        // in `register_exchange` is the one list of what fynd decodes.
        let probe = ProtocolStreamBuilder::new("probe", chain);
        match register_exchange(probe, &protocol, &ComponentFilter::with_tvl_range(0.0, 0.0)) {
            Registration::Registered(_) => systems.push(protocol.system),
            Registration::NoDecoder(_) => {
                warn!(system = %protocol.system, "no decoder for this protocol system; not recorded")
            }
        }
    }
    Ok(systems)
}

/// Decodes raw Tycho feed messages into the [`Update`]s a live feed would produce from them.
///
/// The decoder is configured as `TychoFeed` configures it: the same exchanges and filters through
/// `register_exchanges`, state decode failures skipped, and tokens under `min_token_quality`
/// ignored. Like the live stream, the first update also carries the chain's native wrapper
/// component.
///
/// All messages are decoded before the call returns. The decoder writes contract storage into
/// tycho-simulation's process-wide `SHARED_TYCHO_DB`, which holds one block, so afterwards it holds
/// the last message's storage. Pools that read that storage at simulation time (VM-backed and
/// Uniswap v4 hook pools) then simulate every update against the last block. A caller that applies
/// every update before it solves, as `Solver::from_recording` does, is consistent; a caller that
/// measures block by block is not, for those pools.
///
/// Replay installs no state-override providers. A live feed installs the Titan provider for
/// `vm:fermiswap` and `vm:bopamm`, so replay quotes those pools from their Tycho state only.
///
/// # Errors
///
/// Fails when the protocol list is invalid, the hook handlers cannot be set up, or a message
/// does not decode (e.g. it carries no block).
#[cfg(feature = "test-utils")]
pub async fn decode_recorded_messages(
    chain: Chain,
    protocols: &[String],
    min_token_quality: u32,
    tokens: impl IntoIterator<Item = Token>,
    messages: impl IntoIterator<Item = tycho_simulation::tycho_client::feed::FeedMessage>,
) -> Result<Vec<Update>, String> {
    use tycho_simulation::evm::protocol::{
        native_wrapper::state::NativeWrapperState,
        uniswap_v4::hooks::hook_handler_creator::initialize_hook_handlers,
    };

    initialize_hook_handlers().map_err(|e| format!("cannot set up the hook handlers: {e:?}"))?;
    // Decoding never reads the `ComponentFilter`; only a live subscription uses it.
    let builder = register_exchanges(
        ProtocolStreamBuilder::new("replay", chain),
        ComponentFilter::with_tvl_range(0.0, 0.0),
        protocols,
    )
    .map_err(|e| e.to_string())?
    .skip_state_decode_failures(true)
    .min_token_quality(min_token_quality)
    .set_tokens(
        tokens
            .into_iter()
            .map(|token| (token.address.clone(), token))
            .collect(),
    )
    .await;
    let decoder = builder.get_decoder();

    let mut updates = Vec::new();
    for message in messages {
        let block = message
            .state_msgs
            .values()
            .next()
            .map_or_else(|| "unknown".to_string(), |state_msg| state_msg.header.number.to_string());
        let update = decoder
            .decode(&message)
            .await
            .map_err(|e| format!("cannot decode the message for block {block}: {e}"))?;
        updates.push(update);
    }

    if let (Some(first), Some(native_wrapper)) =
        (updates.first_mut(), NativeWrapperState::new(chain))
    {
        let component = native_wrapper.component();
        let id = component.id.to_string();
        first
            .new_pairs
            .insert(id.clone(), component);
        first
            .states
            .insert(id, Box::new(native_wrapper));
    }
    Ok(updates)
}

/// Opens the pAMM price level stream for test tooling (the benchmark's live capture).
///
/// Wrapper over `open_price_level_stream` so the capture serves the same venues as
/// production without exposing the crate-private `DataFeedError`.
#[cfg(feature = "test-utils")]
pub fn open_price_level_stream_for_live_capture(
    chain: Chain,
    protocols: &[String],
    tokens: &HashMap<Bytes, Token>,
) -> Result<Option<impl Stream<Item = Update> + Send>, String> {
    open_price_level_stream(chain, protocols, tokens).map_err(|e| e.to_string())
}

fn get_env(var: &str) -> Result<String, DataFeedError> {
    env::var(var).map_err(|_| DataFeedError::Config(format!("{} env var not set", var)))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tycho_simulation::price_level_stream::config::PRICE_LEVEL_STREAM_FAMILY;

    use super::*;

    /// A writer that keeps every byte a subscriber formats, so a test can assert on the
    /// rendered log lines.
    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("log buffer poisoned")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
        type Writer = CapturedLogs;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Registers `entries` under a capturing subscriber and returns the protocol systems named
    /// in `Skipping unknown protocol` warnings.
    fn skipped_unknown_protocols(entries: &[&str]) -> Vec<String> {
        let logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .compact()
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let _ = register(entries);
        });
        let rendered = String::from_utf8(std::mem::take(
            &mut *logs
                .0
                .lock()
                .expect("log buffer poisoned"),
        ))
        .expect("utf-8");
        rendered
            .lines()
            .filter_map(|line| line.split_once("Skipping unknown protocol:"))
            .map(|(_, payload)| payload.trim().to_string())
            .collect()
    }

    fn uniswap_v4_component(hook: Option<&str>) -> ComponentWithState {
        let mut static_attributes = HashMap::new();
        if let Some(hook) = hook {
            static_attributes.insert("hooks".to_string(), Bytes::from(hook));
        }
        ComponentWithState {
            state: tycho_simulation::tycho_common::models::protocol::ProtocolComponentState {
                component_id: "0xpool".to_string(),
                attributes: HashMap::new(),
                balances: HashMap::new(),
            },
            component: tycho_simulation::tycho_common::models::protocol::ProtocolComponent {
                id: "0xpool".to_string(),
                protocol_system: "uniswap_v4_hooks".to_string(),
                protocol_type_name: "uniswap_v4_pool".to_string(),
                chain: Chain::Ethereum,
                tokens: vec![],
                static_attributes,
                contract_addresses: vec![],
                change: Default::default(),
                creation_tx: Bytes::default(),
                created_at: Default::default(),
            },
            component_tvl: None,
            entrypoints: vec![],
        }
    }

    #[rstest::rstest]
    #[case::blocked_hook(Some("0x051c99a4583a7137833ad048af442909426d00c4"), false)]
    #[case::blocked_hook_uppercase(Some("0x051C99A4583A7137833AD048AF442909426D00C4"), false)]
    #[case::blocked_hook_latest(Some("0xbf1a0a8608593db7580044b6501fefb310c280c4"), false)]
    #[case::other_hook(Some("0x0000000000000000000000000000000000000001"), true)]
    #[case::no_hook(None, true)]
    fn test_uniswap_v4_hook_filter(#[case] hook: Option<&str>, #[case] kept: bool) {
        assert_eq!(uniswap_v4_hook_filter(&uniswap_v4_component(hook)), kept);
    }

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

    fn register_rfq_entries(
        chain: Chain,
        entries: &[&str],
    ) -> Result<RFQStreamBuilder, DataFeedError> {
        register_rfq(
            RFQStreamBuilder::new(),
            chain,
            1.0,
            &entries
                .iter()
                .map(|entry| (*entry).to_string())
                .collect::<Vec<_>>(),
            std::collections::HashSet::new(),
        )
    }

    fn price_level_stream(
        chain: Chain,
        entries: &[&str],
    ) -> Result<Option<impl Stream<Item = Update> + Send>, DataFeedError> {
        open_price_level_stream(
            chain,
            &entries
                .iter()
                .map(|entry| (*entry).to_string())
                .collect::<Vec<_>>(),
            &HashMap::new(),
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
    fn test_parse_exclusion() {
        assert_eq!(
            parse_exclusion("exclude:vm:fermiswap")
                .unwrap()
                .unwrap(),
            "vm:fermiswap"
        );
    }

    #[test]
    fn test_parse_exclusion_strips_the_exclusive_prefix() {
        assert_eq!(
            parse_exclusion("exclude:exclusive:ekubo_v3")
                .unwrap()
                .unwrap(),
            "ekubo_v3"
        );
    }

    #[test]
    fn test_parse_exclusion_rejects_unsupported_exclusive() {
        assert!(parse_exclusion("exclude:exclusive:uniswap_v3")
            .unwrap()
            .is_err());
    }

    #[test]
    fn test_parse_exclusion_without_protocol() {
        assert_eq!(
            parse_exclusion("exclude:")
                .unwrap()
                .unwrap(),
            ""
        );
    }

    #[test]
    fn test_parse_exclusion_of_a_plain_entry() {
        assert!(parse_exclusion("uniswap_v3").is_none());
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
    fn test_register_exchanges_registers_every_robinhood_protocol() {
        let robinhood_protocols = [
            "sushiswap_v3",
            "uniswap_v4",
            "robinswap_v3",
            "uniswap_v3",
            "ramses_v3",
            "uniswap_v2",
            "ekubo_v3",
            "up_v3",
        ];
        let skipped = skipped_unknown_protocols(&robinhood_protocols);
        assert!(
            skipped.is_empty(),
            "expected every Robinhood protocol to register, but got unknown-protocol warnings \
             for: {skipped:?}"
        );
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
    #[test]
    fn test_price_level_stream_prefix_matches_family() {
        assert_eq!(PRICE_LEVEL_STREAM_PREFIX, format!("{PRICE_LEVEL_STREAM_FAMILY}:"));
    }

    #[test]
    fn test_matches_streamed_system() {
        assert!(matches_streamed_system("uniswap_v3", "uniswap_v3"));
        assert!(matches_streamed_system(
            "pricelevelstream:fermiswap",
            "pricelevelstream:fermiswap"
        ));
        // The venue arrives under the fallback router's family for the same entry.
        assert!(matches_streamed_system("pricelevelstream:fermiswap", "fallback:fermiswap"));
        // The prefix is stripped before registration, so the components carry the bare system.
        assert!(matches_streamed_system("exclusive:ekubo_v3", "ekubo_v3"));
    }

    #[test]
    fn test_matches_streamed_system_rejects_another_venue() {
        assert!(!matches_streamed_system("pricelevelstream:fermiswap", "fallback:kipseli"));
        assert!(!matches_streamed_system("pricelevelstream:fermiswap", "vm:fermiswap"));
        assert!(!matches_streamed_system("uniswap_v3", "fallback:fermiswap"));
        assert!(!matches_streamed_system("vm:fermiswap", "fallback:fermiswap"));
        assert!(!matches_streamed_system("exclusive:ekubo_v3", "ekubo_v2"));
    }

    #[test]
    fn test_has_tycho_protocols() {
        assert!(has_tycho_protocols(&["uniswap_v3".to_string()]));
        assert!(has_tycho_protocols(&["rfq:bebop".to_string(), "uniswap_v3".to_string()]));
        assert!(!has_tycho_protocols(&[
            "rfq:bebop".to_string(),
            "pricelevelstream:fermiswap".to_string(),
        ]));
        assert!(!has_tycho_protocols(&[]));
    }

    #[test]
    fn test_register_exchanges_skips_price_level_entries() {
        assert!(register(&["uniswap_v3", "pricelevelstream:fermiswap"]).is_ok());
    }

    #[test]
    fn test_open_price_level_stream_without_entries() {
        let Ok(None) = price_level_stream(Chain::Ethereum, &["uniswap_v3", "rfq:bebop"]) else {
            panic!("expected no price level stream without a `pricelevelstream:` entry");
        };
    }

    #[test]
    fn test_open_price_level_stream_served_venue() {
        let Ok(Some(_)) = price_level_stream(Chain::Ethereum, &["pricelevelstream:fermiswap"])
        else {
            panic!("expected fermiswap to be served");
        };
    }

    #[test]
    fn test_open_price_level_stream_several_venues() {
        let Ok(Some(_)) = price_level_stream(
            Chain::Ethereum,
            &["pricelevelstream:fermiswap", "pricelevelstream:kipseli"],
        ) else {
            panic!("expected both venues to be served");
        };
    }

    #[test]
    fn test_open_price_level_stream_unknown_venue() {
        for entries in [
            vec!["pricelevelstream:nope"],
            vec!["pricelevelstream:fermiswap", "pricelevelstream:nope"],
        ] {
            let Err(err) = price_level_stream(Chain::Ethereum, &entries) else {
                panic!("expected an unserved venue to be rejected in {entries:?}");
            };
            assert!(
                err.to_string()
                    .contains("unknown pAMM 'nope'"),
                "got {err}"
            );
        }
    }

    #[test]
    fn test_open_price_level_stream_without_entries_off_ethereum() {
        let Ok(None) = price_level_stream(Chain::Base, &["uniswap_v3"]) else {
            panic!("expected chains without a `pricelevelstream:` entry to be left alone");
        };
    }

    #[test]
    fn test_metric_sources_serve_disjoint_chains() {
        assert!(
            !METRIC_CHAINS.contains(&PRICE_LEVEL_STREAM_CHAIN),
            "a chain served both ways would stream the same Metric inventory twice"
        );
    }

    #[test]
    fn test_register_rfq_metric_off_supported_chains() {
        let Err(err) = register_rfq_entries(Chain::Ethereum, &["rfq:metric"]) else {
            panic!("expected rfq:metric to be rejected where Metric has no executor");
        };
        assert!(
            err.to_string()
                .contains("only deployed on base, robinhood"),
            "got {err}"
        );
    }

    #[test]
    fn test_register_rfq_metric_requires_api_key() {
        env::remove_var("METRIC_API_KEY");
        let Err(err) = register_rfq_entries(Chain::Base, &["rfq:metric"]) else {
            panic!("expected rfq:metric to require METRIC_API_KEY");
        };
        assert!(
            err.to_string()
                .contains("METRIC_API_KEY"),
            "got {err}"
        );
    }

    #[test]
    fn test_open_price_level_stream_other_chain() {
        let Err(err) = price_level_stream(Chain::Base, &["pricelevelstream:fermiswap"]) else {
            panic!("expected the price level stream to be rejected off Ethereum");
        };
        assert!(
            err.to_string()
                .contains("serves ethereum only"),
            "got {err}"
        );
    }

    #[cfg(feature = "test-utils")]
    #[test]
    fn test_select_decodable_systems() {
        let entries: Vec<String> = [
            "uniswap_v3",
            "exclusive:ekubo_v3",
            "rfq:bebop",
            "pricelevelstream:fermiswap",
            "rocketpool",
        ]
        .map(String::from)
        .to_vec();

        let systems = select_decodable_systems(Chain::Ethereum, &entries).expect("valid entries");

        // The prefix is stripped, the non-Tycho entries are dropped, and rocketpool has no
        // decoder in fynd.
        assert_eq!(systems, ["uniswap_v3", "ekubo_v3"]);
    }

    #[cfg(feature = "test-utils")]
    mod decode_recorded_messages {
        use std::collections::HashMap as StdHashMap;

        use tycho_simulation::tycho_client::feed::{
            synchronizer::StateSyncMessage, BlockHeader, FeedMessage,
        };

        use super::{super::decode_recorded_messages, *};

        fn empty_block(number: u64) -> FeedMessage {
            let header = BlockHeader { number, ..Default::default() };
            FeedMessage {
                state_msgs: StdHashMap::from([(
                    "uniswap_v2".to_string(),
                    StateSyncMessage { header, ..Default::default() },
                )]),
                sync_states: StdHashMap::new(),
            }
        }

        #[tokio::test]
        async fn test_native_wrapper_first_update() {
            let updates = decode_recorded_messages(
                Chain::Ethereum,
                &["uniswap_v2".to_string()],
                100,
                Vec::new(),
                vec![empty_block(1), empty_block(2)],
            )
            .await
            .expect("empty blocks decode");

            assert_eq!(updates.len(), 2);
            assert_eq!(updates[0].block_number_or_timestamp, 1);
            assert_eq!(updates[0].new_pairs.len(), 1, "the native wrapper, like a live stream");
            assert_eq!(updates[0].states.len(), 1);
            assert!(updates[1].new_pairs.is_empty());
        }

        #[tokio::test]
        async fn test_message_without_block() {
            let error = decode_recorded_messages(
                Chain::Ethereum,
                &["uniswap_v2".to_string()],
                100,
                Vec::new(),
                vec![FeedMessage { state_msgs: StdHashMap::new(), sync_states: StdHashMap::new() }],
            )
            .await
            .expect_err("a message with no block cannot decode");

            assert!(error.contains("cannot decode"), "{error}");
        }
    }
}

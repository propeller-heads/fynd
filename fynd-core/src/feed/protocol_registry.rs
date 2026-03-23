use std::{collections::HashSet, env, time::Duration};

use tracing::{info, warn};
use tycho_simulation::{
    evm::{
        engine_db::tycho_db::PreCachedDB,
        protocol::{
            aerodrome_slipstreams::state::AerodromeSlipstreamsState,
            ekubo::state::EkuboState,
            erc4626::state::ERC4626State,
            filters::{balancer_v2_pool_filter, erc4626_filter, fluid_v1_paused_pools_filter},
            fluid::FluidV1,
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

pub fn register_exchanges(
    mut builder: ProtocolStreamBuilder,
    tvl_filter: ComponentFilter,
    protocols: &[String],
) -> Result<ProtocolStreamBuilder, DataFeedError> {
    for protocol in protocols {
        match protocol.as_str() {
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
                builder = builder.exchange::<EVMPoolState<PreCachedDB>>(
                    "vm:curve",
                    tvl_filter.clone(),
                    None,
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
            "fluid_v1" => {
                builder = builder.exchange::<FluidV1>(
                    "fluid_v1",
                    tvl_filter.clone(),
                    Some(fluid_v1_paused_pools_filter),
                );
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
    rfq_tokens: HashSet<Bytes>,
) -> Result<RFQStreamBuilder, DataFeedError> {
    for protocol in protocols {
        match protocol.as_str() {
            "rfq:bebop" => {
                let user = get_env("BEBOP_USER")?;
                let key = get_env("BEBOP_KEY")?;
                info!("Adding {protocol} RFQ client...");
                let bebop_client = BebopClientBuilder::new(chain, user, key)
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

//! Tycho feed for keeping market data synchronized.
//!
//! The TychoFeed connects to Tycho's WebSocket API and:
//! - Receives component/state updates
//! - Updates SharedMarketData (exclusive write access)
//! - Broadcasts MarketEvents to Solvers

use std::{collections::HashSet, sync::Arc};

use tokio::sync::{
    broadcast::{self, Receiver, Sender},
    RwLock,
};
use tokio_stream::StreamExt;
use tracing::{debug, info, instrument, span, trace, Instrument, Level};
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
            rocketpool::state::RocketpoolState,
            uniswap_v2::state::UniswapV2State,
            uniswap_v3::state::UniswapV3State,
            uniswap_v4::state::UniswapV4State,
            vm::state::EVMPoolState,
        },
        stream::ProtocolStreamBuilder,
    },
    protocol::models::Update,
    tycho_client::feed::{component_tracker::ComponentFilter, SynchronizerState},
    utils::load_all_tokens,
};

use crate::{
    api::HealthTracker,
    feed::{events::MarketEvent, market_data::SharedMarketData, TychoFeedConfig, TychoFeedError},
    types::BlockInfo,
    SharedMarketDataRef,
};

/// The Tycho indexer that keeps market data synchronized.
///
/// # Responsibilities
///
/// - Connect to Tycho WebSocket and maintain connection
/// - Process incoming component/state updates
/// - Update SharedMarketData (holds exclusive write access)
/// - Broadcast MarketEvents to all subscribed Solvers
/// - Periodically refresh gas prices from RPC
pub struct TychoFeed {
    /// Configuration.
    config: TychoFeedConfig,
    /// Shared market data (we have write access).
    market_data: Arc<RwLock<SharedMarketData>>,
    /// Event broadcaster.
    event_tx: Sender<MarketEvent>,
    #[allow(dead_code)]
    /// Health tracker for API health checks.
    health_tracker: HealthTracker,
}

impl TychoFeed {
    /// Creates a new TychoFeed.
    ///
    /// # Arguments
    ///
    /// * `config` - Indexer configuration
    /// * `market_data` - Shared market data reference
    /// * `health_tracker` - Health tracker for API health checks
    ///
    /// # Returns
    ///
    /// A tuple of (TychoFeed, Receiver) - the receiver can be
    /// used to subscribe additional consumers before calling `run()`.
    pub fn new(
        config: TychoFeedConfig,
        market_data: SharedMarketDataRef,
        health_tracker: HealthTracker,
    ) -> (Self, Receiver<MarketEvent>) {
        let (event_tx, event_rx) = broadcast::channel(1024);

        (Self { config, market_data, event_tx, health_tracker }, event_rx)
    }

    /// Returns a new subscriber for market events.
    pub fn subscribe(&self) -> Receiver<MarketEvent> {
        self.event_tx.subscribe()
    }

    /// Returns an additional event sender.
    pub fn event_sender_clone(&self) -> Sender<MarketEvent> {
        self.event_tx.clone()
    }

    /// Runs the indexer event loop.
    ///
    /// This method runs indefinitely, reconnecting on failures.
    /// It is recommended to call this in a dedicated tokio task.
    pub async fn run(self) -> Result<(), TychoFeedError> {
        info!(
            tycho_url = %self.config.tycho_url,
            protocols = ?self.config.protocols,
            "Starting Data Feed..."
        );

        let all_tokens = load_all_tokens(
            self.config.tycho_url.as_str(),
            false,
            Some(self.config.tycho_api_key.as_str()),
            true,
            self.config.chain,
            None,
            None,
        )
        .await
        .map_err(|e| TychoFeedError::Config(e.to_string()))?; //TODO: handle this error better

        debug!("Loaded {} tokens from Tycho", all_tokens.len());

        let mut protocol_stream = register_exchanges(
            ProtocolStreamBuilder::new(&self.config.tycho_url, self.config.chain),
            ComponentFilter::with_tvl_range(
                self.config.min_tvl,
                self.config.min_tvl * self.config.tvl_buffer_multiplier,
            ),
            &self.config.protocols,
        )?
        .auth_key(Some(self.config.tycho_api_key.clone()))
        .skip_state_decode_failures(true)
        .set_tokens(all_tokens)
        .await
        .build()
        .await
        .map_err(|e| TychoFeedError::StreamError(e.to_string()))?;

        // Loop through block updates
        while let Some(msg) = protocol_stream.next().await {
            trace!("Received message from Tycho stream {:?}", msg);
            let msg = msg.map_err(|e| TychoFeedError::StreamError(e.to_string()))?;
            self.handle_tycho_message(msg).await?;
            self.refresh_gas_price().await?;
            self.health_tracker.update();
        }

        Ok(())
    }

    /// Handles a message from Tycho stream.
    #[instrument(skip(self, msg))]
    async fn handle_tycho_message(&self, msg: Update) -> Result<(), TychoFeedError> {
        // Collect variables for market shared data update
        let added_components = msg.new_pairs.clone();
        let removed_components = msg.removed_pairs.clone();
        let updated_or_new_states = msg.states.clone();
        let updated_components_ids: HashSet<_> = updated_or_new_states
            .keys()
            .filter(|id| !added_components.contains_key(id.as_str())) // TODO: Should we still emit as updated if the component is new?
            .cloned()
            .collect();

        let maybe_new_tokens = msg
            .new_pairs
            .values()
            .flat_map(|component| component.tokens.iter().cloned());
        // TODO: how do we handle delayed and stale states? Should the feed or the solvers handle
        // this?
        let sync_states = msg.sync_states.clone();
        let latest_block_info = msg
            .sync_states
            .values()
            .filter_map(|status| {
                if let SynchronizerState::Ready(header) = status {
                    Some(BlockInfo {
                        number: header.number,
                        hash: header.hash.to_string(),
                        timestamp: header.timestamp,
                    })
                } else {
                    None
                }
            })
            .max_by_key(|b| b.number);

        debug!("Received message from with {} new components, {} removed components, and {} updated components", added_components.len(), removed_components.len(), updated_or_new_states.len());
        trace!("Updating market data");
        // Update market data. We should only hold the write lock inside this code block.
        {
            let mut market_data = self
                .market_data
                .write()
                .instrument(span!(Level::DEBUG, "data_feed_write_lock"))
                .await;

            market_data.upsert_components(
                added_components
                    .clone()
                    .into_values()
                    .map(|component| {
                        // We can't use From<ProtocolComponent> because it removes "0x" prefix from
                        // the id
                        tycho_simulation::tycho_common::models::protocol::ProtocolComponent {
                            id: component.id.to_string(),
                            protocol_system: component.protocol_system,
                            protocol_type_name: component.protocol_type_name,
                            chain: component.chain,
                            tokens: component
                                .tokens
                                .into_iter()
                                .map(|t| t.address)
                                .collect(),
                            static_attributes: component.static_attributes,
                            change: Default::default(),
                            creation_tx: component.creation_tx,
                            created_at: component.created_at,
                            contract_addresses: component.contract_ids,
                        }
                    }),
            );
            market_data.remove_components(removed_components.keys().cloned());
            market_data.upsert_tokens(maybe_new_tokens);
            market_data.update_states(updated_or_new_states);
            market_data.update_protocol_sync_status(sync_states);

            // Update the last updated block info if one of the protocols reported "Ready" status.
            if let Some(block_info) = latest_block_info {
                market_data.update_last_updated(block_info);
            }
        }
        trace!("Market data updated");

        // Only broadcast event if there are actual changes
        if !added_components.is_empty() ||
            !removed_components.is_empty() ||
            !updated_components_ids.is_empty()
        {
            let market_update_event = MarketEvent::MarketUpdated {
                added_components: added_components
                    .into_iter()
                    .map(|(id, component)| {
                        (
                            id,
                            component
                                .tokens
                                .iter()
                                .map(|token| token.address.clone())
                                .collect(),
                        )
                    })
                    .collect(),
                removed_components: removed_components.into_keys().collect(),
                updated_components: updated_components_ids
                    .into_iter()
                    .collect(),
            };

            self.event_tx
                .send(market_update_event)
                .map_err(|e| TychoFeedError::EventChannelError(e.to_string()))?;
        }

        Ok(())
    }

    #[allow(dead_code)]
    /// Updates gas price from RPC.
    async fn refresh_gas_price(&self) -> Result<(), TychoFeedError> {
        // TODO: Triggers gas price refresh from fetcher
        Ok(())
    }
}

//TODO: make this public in tycho_simulation and import it here?
fn register_exchanges(
    mut builder: ProtocolStreamBuilder,
    tvl_filter: ComponentFilter,
    protocols: &[String],
) -> Result<ProtocolStreamBuilder, TychoFeedError> {
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
            "rocketpool" => {
                builder =
                    builder.exchange::<RocketpoolState>("rocketpool", tvl_filter.clone(), None);
            }
            "velodrome_slipstreams" => {
                builder = builder.exchange::<AerodromeSlipstreamsState>(
                    "velodrome_slipstreams",
                    tvl_filter.clone(),
                    None,
                );
            }
            _ => {
                return Err(TychoFeedError::Config(format!("Unknown protocol: {}", protocol)));
            }
        }
    }
    Ok(builder)
}

}

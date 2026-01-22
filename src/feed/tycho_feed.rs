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

    /// Returns an additional event sender. Currently only used for testing.
    #[cfg(test)]
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
            !self.config.use_tls,
            self.config.tycho_api_key.as_deref(),
            true,
            self.config.chain,
            Some(self.config.min_token_quality),
            None,
        )
        .await
        .map_err(|e| TychoFeedError::StreamError(e.to_string()))?;

        debug!("Loaded {} tokens from Tycho", all_tokens.len());

        let mut protocol_stream = register_exchanges(
            ProtocolStreamBuilder::new(&self.config.tycho_url, self.config.chain),
            ComponentFilter::with_tvl_range(
                self.config.min_tvl,
                self.config.min_tvl * self.config.tvl_buffer_multiplier,
            ),
            &self.config.protocols,
        )?
        .auth_key(self.config.tycho_api_key.clone())
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
        let Update {
            new_pairs: added_components,
            removed_pairs: removed_components,
            states: updated_or_new_states,
            sync_states,
            ..
        } = msg;

        let updated_components_ids: HashSet<_> = updated_or_new_states
            .keys()
            .filter(|id| !added_components.contains_key(id.as_str())) // TODO: Should we still emit as updated if the component is new?
            .cloned()
            .collect();

        let maybe_new_tokens = added_components
            .values()
            .flat_map(|component| component.tokens.iter().cloned());
        // TODO: how do we handle delayed and stale states? Should the feed or the solvers handle
        // this?
        let latest_block_info = sync_states
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
            market_data.remove_components(removed_components.keys());
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
                                .into_iter()
                                .map(|token| token.address)
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

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, env};

    use num_bigint::BigUint;
    use tycho_simulation::{
        protocol::models::{ProtocolComponent, Update},
        tycho_common::{
            models::{token::Token, Chain},
            Bytes,
        },
        tycho_core::simulation::{
            errors::{SimulationError, TransitionError},
            protocol_sim::{Balances, GetAmountOutResult, ProtocolSim},
        },
    };

    use super::*;
    use crate::{
        api::HealthTracker,
        feed::{market_data::new_shared_market_data, TychoFeedConfig},
    };

    #[derive(Debug, Clone)]
    struct MockProtocolSim {
        id: f64,
    }

    impl MockProtocolSim {
        fn new(id: f64) -> Self {
            Self { id }
        }
    }

    impl ProtocolSim for MockProtocolSim {
        fn get_amount_out(
            &self,
            amount_in: BigUint,
            _token_in: &Token,
            _token_out: &Token,
        ) -> Result<GetAmountOutResult, SimulationError> {
            Ok(GetAmountOutResult {
                amount: amount_in,
                gas: BigUint::ZERO,
                new_state: Box::new(self.clone()),
            })
        }

        fn fee(&self) -> f64 {
            // We use .fee() to get the id of the MockProtocolSim in the tests for our assertions.
            self.id
        }

        fn spot_price(&self, _base: &Token, _quote: &Token) -> Result<f64, SimulationError> {
            Ok(0.0)
        }

        fn get_limits(
            &self,
            _sell_token: Bytes,
            _buy_token: Bytes,
        ) -> Result<(BigUint, BigUint), SimulationError> {
            Ok((BigUint::ZERO, BigUint::ZERO))
        }

        fn delta_transition(
            &mut self,
            _delta: tycho_simulation::tycho_core::dto::ProtocolStateDelta,
            _tokens: &std::collections::HashMap<Bytes, Token>,
            _balances: &Balances,
        ) -> Result<(), TransitionError<String>> {
            Ok(())
        }

        fn clone_box(&self) -> Box<dyn ProtocolSim> {
            Box::new(self.clone())
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }

        fn eq(&self, _other: &dyn ProtocolSim) -> bool {
            true
        }
    }

    // Helper function to create a test config
    fn create_test_config() -> TychoFeedConfig {
        TychoFeedConfig::new(
            "ws://test.tycho.io".to_string(),
            Chain::Ethereum,
            Some("test_api_key".to_string()),
            false, // no TLS for test
            vec!["uniswap_v2".to_string()],
            10.0,
            "http://test.rpc".to_string(),
        )
    }

    // Helper to create a test token
    fn create_test_token(address: &str, symbol: &str) -> Token {
        Token {
            address: Bytes::from(address),
            symbol: symbol.to_string(),
            decimals: 18,
            tax: Default::default(),
            gas: vec![],
            chain: Chain::Ethereum,
            quality: 100,
        }
    }

    // Helper to create a test component
    fn create_test_component(id: &str, tokens: Vec<Token>) -> ProtocolComponent {
        let id_bytes = Bytes::from(id);

        ProtocolComponent::new(
            id_bytes.clone(),
            "uniswap_v2".to_string(),
            "uniswap_v2_pool".to_string(),
            Chain::Ethereum,
            tokens,
            vec![],
            HashMap::new(),
            Bytes::from(vec![0x12, 0x34]),
            chrono::DateTime::from_timestamp(1234567890, 0)
                .unwrap()
                .naive_utc(),
        )
    }

    #[tokio::test]
    async fn test_event_resubscription() {
        let config = create_test_config();
        let market_data = new_shared_market_data();
        let health_tracker = HealthTracker::new();

        let (feed, _initial_rx) = TychoFeed::new(config, market_data, health_tracker);

        // Subscribe multiple times to verify multiple subscribers can be created
        let mut sub1 = feed.subscribe();
        let mut sub2 = feed.subscribe();

        // Get event sender
        let sender = feed.event_sender_clone();

        sender
            .send(MarketEvent::MarketUpdated {
                added_components: HashMap::new(),
                removed_components: Vec::new(),
                updated_components: Vec::new(),
            })
            .expect("Failed to send event");

        let event_1 = sub1.recv().await.unwrap();
        let event_2 = sub2.recv().await.unwrap();
        assert_eq!(event_1, event_2);
        assert_eq!(
            event_1,
            MarketEvent::MarketUpdated {
                added_components: HashMap::new(),
                removed_components: Vec::new(),
                updated_components: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn test_handle_message_adds_new_components() {
        let market_data = new_shared_market_data();
        let (feed, mut event_rx) =
            TychoFeed::new(create_test_config(), market_data.clone(), HealthTracker::new());

        // Create a new component
        let component_id = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let token1 = create_test_token("0x1111111111111111111111111111111111111111", "TKN1");
        let token2 = create_test_token("0x2222222222222222222222222222222222222222", "TKN2");
        let test_component =
            create_test_component(component_id, vec![token1.clone(), token2.clone()]);

        let mut new_pairs = HashMap::new();
        new_pairs.insert(component_id.to_string(), test_component.clone());

        let update = Update::new(12345, HashMap::new(), new_pairs);

        // Handle the message
        feed.handle_tycho_message(update)
            .await
            .expect("Failed to handle message");

        // Verify component was added to market data
        let data = market_data.read().await;

        let component = data
            .get_component(component_id)
            .expect("Component should be in market data");
        assert_eq!(
            component.clone(),
            tycho_simulation::tycho_common::models::protocol::ProtocolComponent {
                id: component_id.to_string(),
                protocol_system: "uniswap_v2".to_string(),
                protocol_type_name: "uniswap_v2_pool".to_string(),
                chain: Chain::Ethereum,
                tokens: vec![token1.address.clone(), token2.address.clone()],
                static_attributes: HashMap::new(),
                contract_addresses: vec![],
                change: Default::default(),
                creation_tx: Bytes::from(vec![0x12, 0x34]),
                created_at: chrono::DateTime::from_timestamp(1234567890, 0)
                    .unwrap()
                    .naive_utc(),
            }
        );
        drop(data);

        // Verify event was broadcast
        let event = event_rx
            .try_recv()
            .expect("Should receive event");
        assert_eq!(
            event,
            MarketEvent::MarketUpdated {
                added_components: HashMap::from([(
                    component_id.to_string(),
                    vec![token1.address, token2.address]
                )]),
                removed_components: Vec::new(),
                updated_components: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn test_handle_message_removes_components() {
        let market_data = new_shared_market_data();

        let (feed, mut event_rx) =
            TychoFeed::new(create_test_config(), market_data.clone(), HealthTracker::new());

        let component_id = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let token1 = create_test_token("0x1111111111111111111111111111111111111111", "TKN1");
        let token2 = create_test_token("0x2222222222222222222222222222222222222222", "TKN2");

        // First, add a component
        let mut new_pairs = HashMap::new();
        new_pairs.insert(
            component_id.to_string(),
            create_test_component(component_id, vec![token1.clone(), token2.clone()]),
        );

        let update = Update::new(12345, HashMap::new(), new_pairs);
        feed.handle_tycho_message(update)
            .await
            .expect("Failed to add component");

        // Verify it was added
        {
            let data = market_data.read().await;
            assert!(
                data.get_component(component_id)
                    .is_some(),
                "Component should exist before removal"
            );
        }

        let mut removed_pairs = HashMap::new();
        removed_pairs.insert(
            component_id.to_string(),
            create_test_component(component_id, vec![token1.clone(), token2.clone()]),
        );

        let update =
            Update::new(12345, HashMap::new(), HashMap::new()).set_removed_pairs(removed_pairs);

        feed.handle_tycho_message(update)
            .await
            .expect("Failed to handle removal");

        // Verify component was removed
        let data = market_data.read().await;
        assert!(
            data.get_component(component_id)
                .is_none(),
            "Component should be removed from market data"
        );
        drop(data);

        // Verify both events were broadcast
        let event_1 = event_rx
            .try_recv()
            .expect("Should receive event");
        let event_2 = event_rx
            .try_recv()
            .expect("Should receive event");
        assert_eq!(
            event_1,
            MarketEvent::MarketUpdated {
                added_components: HashMap::from([(
                    component_id.to_string(),
                    vec![token1.address, token2.address]
                )]),
                removed_components: Vec::new(),
                updated_components: Vec::new(),
            }
        );
        assert_eq!(
            event_2,
            MarketEvent::MarketUpdated {
                added_components: HashMap::new(),
                removed_components: vec![component_id.to_string()],
                updated_components: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn test_handle_message_updates_states() {
        let market_data = new_shared_market_data();
        let (feed, mut event_rx) =
            TychoFeed::new(create_test_config(), market_data.clone(), HealthTracker::new());

        let component_id = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let token1 = create_test_token("0x1111111111111111111111111111111111111111", "TKN1");
        let token2 = create_test_token("0x2222222222222222222222222222222222222222", "TKN2");

        // First, add a component
        let mut new_pairs = HashMap::new();
        new_pairs.insert(
            component_id.to_string(),
            create_test_component(component_id, vec![token1.clone(), token2.clone()]),
        );

        // Create an update with state information
        let mut states = HashMap::new();
        states.insert(
            component_id.to_string(),
            Box::new(MockProtocolSim::new(1.0)) as Box<dyn ProtocolSim>,
        );

        let update = Update::new(12345, states.clone(), new_pairs);
        feed.handle_tycho_message(update)
            .await
            .expect("Failed to add component");

        // Verify state was updated
        {
            let data = market_data.read().await;
            assert_eq!(
                data.get_component(component_id)
                    .expect("Component should be in market data")
                    .clone(),
                tycho_simulation::tycho_common::models::protocol::ProtocolComponent {
                    id: component_id.to_string(),
                    protocol_system: "uniswap_v2".to_string(),
                    protocol_type_name: "uniswap_v2_pool".to_string(),
                    chain: Chain::Ethereum,
                    tokens: vec![token1.address.clone(), token2.address.clone()],
                    static_attributes: HashMap::new(),
                    contract_addresses: vec![],
                    change: Default::default(),
                    creation_tx: Bytes::from(vec![0x12, 0x34]),
                    created_at: chrono::DateTime::from_timestamp(1234567890, 0)
                        .unwrap()
                        .naive_utc(),
                },
                "Component should be in market data"
            );
            assert_eq!(
                data.get_simulation_state(component_id)
                    .expect("Component should be in market data")
                    .fee(),
                1.0,
                "Component state fee should be 1.0"
            );
        }

        // Now update its state

        // Create an update with state information
        let new_state = Box::new(MockProtocolSim::new(2.0)) as Box<dyn ProtocolSim>;
        let update = Update::new(
            12345,
            HashMap::from([(component_id.to_string(), new_state)]),
            HashMap::new(),
        );
        feed.handle_tycho_message(update)
            .await
            .expect("Failed to add component");

        // Verify state was updated
        {
            let data = market_data.read().await;
            assert_eq!(
                data.get_simulation_state(component_id)
                    .expect("Component should be in market data")
                    .fee(),
                2.0,
                "Component state fee should be 2.0"
            );
        }

        // Verify event was broadcast
        let event_1 = event_rx
            .try_recv()
            .expect("Should receive event");
        let event_2 = event_rx
            .try_recv()
            .expect("Should receive event");
        assert_eq!(
            event_1,
            MarketEvent::MarketUpdated {
                added_components: HashMap::from([(
                    component_id.to_string(),
                    vec![token1.address, token2.address]
                )]),
                removed_components: Vec::new(),
                updated_components: vec![],
            }
        );
        assert_eq!(
            event_2,
            MarketEvent::MarketUpdated {
                added_components: HashMap::new(),
                removed_components: Vec::new(),
                updated_components: vec![component_id.to_string()],
            }
        );
    }

    #[tokio::test]
    async fn test_handle_message_multiple_operations() {
        let market_data = new_shared_market_data();

        let (feed, mut event_rx) =
            TychoFeed::new(create_test_config(), market_data.clone(), HealthTracker::new());

        let old_component_id = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let new_component_id = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let old_token1 = create_test_token("0x0000000000000000000000000000000000000001", "OLD1");
        let old_token2 = create_test_token("0x0000000000000000000000000000000000000002", "OLD2");
        let new_token1 = create_test_token("0x1111111111111111111111111111111111111111", "NEW1");
        let new_token2 = create_test_token("0x2222222222222222222222222222222222222222", "NEW2");

        // First, add an old component
        let mut new_pairs = HashMap::new();
        new_pairs.insert(
            old_component_id.to_string(),
            create_test_component(old_component_id, vec![old_token1.clone(), old_token2.clone()]),
        );

        let update = Update::new(12345, HashMap::new(), new_pairs);
        feed.handle_tycho_message(update)
            .await
            .expect("Failed to add old component");

        // Verify the old component was added
        {
            let data = market_data.read().await;
            assert!(
                data.get_component(old_component_id)
                    .is_some(),
                "Old component should exist before removal"
            );
        }

        // Now add a new one and remove the old one in the same message
        let mut new_pairs = HashMap::new();
        new_pairs.insert(
            new_component_id.to_string(),
            create_test_component(new_component_id, vec![new_token1.clone(), new_token2.clone()]),
        );

        let mut removed_pairs = HashMap::new();
        removed_pairs.insert(
            old_component_id.to_string(),
            create_test_component(old_component_id, vec![old_token1.clone(), old_token2.clone()]),
        );

        let update = Update::new(12345, HashMap::new(), new_pairs).set_removed_pairs(removed_pairs);

        feed.handle_tycho_message(update)
            .await
            .expect("Failed to handle complex update");

        // Verify both operations succeeded
        {
            let data = market_data.read().await;
            assert!(
                data.get_component(new_component_id)
                    .is_some(),
                "New component should be added"
            );
            assert!(
                data.get_component(old_component_id)
                    .is_none(),
                "Old component should be removed"
            );
        }

        // Verify we receive both events in the correct order
        let event_1 = event_rx
            .try_recv()
            .expect("Should receive first event");
        let event_2 = event_rx
            .try_recv()
            .expect("Should receive second event");

        // First event: old component added
        assert_eq!(
            event_1,
            MarketEvent::MarketUpdated {
                added_components: HashMap::from([(
                    old_component_id.to_string(),
                    vec![old_token1.address.clone(), old_token2.address.clone()]
                )]),
                removed_components: Vec::new(),
                updated_components: Vec::new(),
            }
        );

        // Second event: new component added AND old component removed
        assert_eq!(
            event_2,
            MarketEvent::MarketUpdated {
                added_components: HashMap::from([(
                    new_component_id.to_string(),
                    vec![new_token1.address, new_token2.address]
                )]),
                removed_components: vec![old_component_id.to_string()],
                updated_components: Vec::new(),
            }
        );

        // Verify no more events
        match event_rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                // Expected - no more events
            }
            Ok(event) => panic!("Unexpected extra event: {:?}", event),
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    #[tokio::test]
    async fn test_handle_message_empty_update() {
        let config = create_test_config();
        let market_data = new_shared_market_data();
        let health_tracker = HealthTracker::new();

        let (feed, mut event_rx) = TychoFeed::new(config, market_data.clone(), health_tracker);

        // Send an empty update
        let update = Update::new(12345, HashMap::new(), HashMap::new());

        feed.handle_tycho_message(update)
            .await
            .expect("Failed to handle empty update");

        // Verify no event was broadcast (empty updates should not trigger events)
        match event_rx.try_recv() {
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                // Expected - no event should be broadcast for empty updates
            }
            Ok(_) => panic!("Should not broadcast event for empty update"),
            Err(e) => panic!("Unexpected error: {:?}", e),
        }
    }

    #[tokio::test(flavor = "multi_thread")] // Multi-thread needed because tycho decoder does some blocking operations
    #[ignore]
    async fn test_real_feed() {
        let rpc_url = env::var("RPC_URL").expect("RPC_URL must be set");
        let tycho_api_key = env::var("TYCHO_API_KEY").expect("TYCHO_API_KEY must be set");
        let tycho_url = env::var("TYCHO_URL").expect("TYCHO_URL must be set");
        let config = TychoFeedConfig::new(
            tycho_url,
            Chain::Ethereum,
            Some(tycho_api_key),
            true, // Use TLS for real feed test
            vec!["uniswap_v2".to_string()],
            100.0,
            rpc_url,
        );

        let mut message_count = 5;

        let market_data = new_shared_market_data();
        let health_tracker = HealthTracker::new();

        let (feed, mut event_rx) = TychoFeed::new(config, market_data.clone(), health_tracker);

        // Start Tycho feed in background
        let feed_handle = tokio::spawn(async move {
            if let Err(e) = feed.run().await {
                panic!("Failed to run feed: {:?}", e);
            }
        });

        while let Ok(event) = event_rx.recv().await {
            message_count -= 1;
            if message_count == 0 {
                break;
            }
            dbg!(&event);
        }

        feed_handle.abort();
    }
}

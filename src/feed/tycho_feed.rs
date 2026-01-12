//! Tycho feed for keeping market data synchronized.
//!
//! The TychoFeed connects to Tycho's WebSocket API and:
//! - Receives component/state updates
//! - Updates SharedMarketData (exclusive write access)
//! - Broadcasts MarketEvents to Solvers

use std::{sync::Arc, time::Duration};

use tokio::sync::{broadcast, RwLock};
use tracing::{error, info};
use tycho_simulation::tycho_core::models::Address;

use crate::{
    api::HealthTracker,
    feed::{
        events::{ComponentSummary, MarketEvent},
        market_data::SharedMarketData,
        TychoFeedConfig, TychoFeedError,
    },
    types::{ComponentId, GasPrice, ProtocolSystem},
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
    event_tx: broadcast::Sender<MarketEvent>,
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
    /// A tuple of (TychoFeed, broadcast::Receiver) - the receiver can be
    /// used to subscribe additional consumers before calling `run()`.
    pub fn new(
        config: TychoFeedConfig,
        market_data: SharedMarketDataRef,
        health_tracker: HealthTracker,
    ) -> (Self, broadcast::Receiver<MarketEvent>) {
        let (event_tx, event_rx) = broadcast::channel(1024);

        (Self { config, market_data, event_tx, health_tracker }, event_rx)
    }

    /// Returns a new subscriber for market events.
    pub fn subscribe(&self) -> broadcast::Receiver<MarketEvent> {
        self.event_tx.subscribe()
    }

    /// Returns the event sender for creating additional subscribers.
    pub fn event_sender(&self) -> broadcast::Sender<MarketEvent> {
        self.event_tx.clone()
    }

    /// Runs the indexer event loop.
    ///
    /// This method runs indefinitely, reconnecting on failures.
    /// Call this in a dedicated tokio task.
    pub async fn run(self) -> Result<(), TychoFeedError> {
        info!(
            tycho_url = %self.config.tycho_url,
            protocols = ?self.config.protocols,
            "starting tycho indexer"
        );

        // TODO: Implement actual Tycho connection
        // For now, this is a skeleton that shows the structure

        loop {
            match self.connect_and_stream().await {
                Ok(()) => {
                    info!("indexer stream ended normally");
                    break;
                }
                Err(e) => {
                    error!(error = %e, "indexer connection error, reconnecting...");
                    tokio::time::sleep(self.config.reconnect_delay).await;
                }
            }
        }

        Ok(())
    }

    /// Connects to Tycho and processes the event stream.
    async fn connect_and_stream(&self) -> Result<(), TychoFeedError> {
        // TODO: Implement actual WebSocket connection to Tycho
        //
        // 1. Connect to Tycho WebSocket
        // 2. Subscribe to configured protocols
        // 3. Process messages in a loop:
        //    - Component added -> update market_data, broadcast ComponentAdded
        //    - Component removed -> update market_data, broadcast ComponentRemoved
        //    - State update -> update market_data, broadcast StateUpdated
        // 4. Handle disconnects gracefully

        info!("connected to tycho (placeholder)");

        // Placeholder: send initial snapshot
        self.send_initial_snapshot().await?;

        // Placeholder: simulate staying connected
        // In real implementation, this would be the message processing loop
        tokio::time::sleep(Duration::from_secs(3600)).await;

        Ok(())
    }

    /// Sends an initial snapshot to subscribers.
    async fn send_initial_snapshot(&self) -> Result<(), TychoFeedError> {
        let market = self.market_data.read().await;

        let components: Vec<ComponentSummary> = market
            .components()
            .filter_map(|(id, data)| {
                match data
                    .component
                    .protocol_system
                    .as_str()
                    .try_into()
                {
                    Ok(protocol_system) => Some(ComponentSummary {
                        id: id.clone(),
                        tokens: data.component.tokens.clone(),
                        protocol_system,
                    }),
                    Err(e) => {
                        tracing::warn!("Skipping component {} with unknown protocol: {}", id, e);
                        None
                    }
                }
            })
            .collect();

        let gas_price = market.gas_price().clone();

        drop(market);

        let _ = self
            .event_tx
            .send(MarketEvent::Snapshot { components, gas_price });

        Ok(())
    }

    #[allow(dead_code)]
    /// Handles a component added event from Tycho.
    async fn handle_component_added(
        &self,
        id: ComponentId,
        tokens: Vec<Address>,
        protocol_system: ProtocolSystem,
    ) -> Result<(), TychoFeedError> {
        // Update shared market data
        {
            let mut market = self.market_data.write().await;
            market.add_component_topology(id.clone(), tokens.clone());
        }

        // Update health tracker
        self.health_tracker.update();

        // Broadcast event
        let _ = self
            .event_tx
            .send(MarketEvent::ComponentAdded { component_id: id, tokens, protocol_system });

        Ok(())
    }

    #[allow(dead_code)]
    /// Handles a component removed event from Tycho.
    async fn handle_component_removed(
        &self,
        component_id: ComponentId,
    ) -> Result<(), TychoFeedError> {
        // Update shared market data
        {
            let mut market = self.market_data.write().await;
            market.remove_component(&component_id);
        }

        // Update health tracker
        self.health_tracker.update();

        // Broadcast event
        let _ = self
            .event_tx
            .send(MarketEvent::ComponentRemoved { component_id });

        Ok(())
    }

    #[allow(dead_code)]
    /// Handles a state update event from Tycho.
    async fn handle_state_updated(&self, component_id: ComponentId) -> Result<(), TychoFeedError> {
        // TODO: Update component state in market_data
        // The actual state (reserves, etc.) would come from Tycho

        // Update health tracker
        self.health_tracker.update();

        // Broadcast event
        let _ = self
            .event_tx
            .send(MarketEvent::StateUpdated { component_id });

        Ok(())
    }

    #[allow(dead_code)]
    /// Updates gas price from RPC.
    async fn refresh_gas_price(&self) -> Result<(), TychoFeedError> {
        // TODO: Fetch gas price from RPC
        // For now, use placeholder values

        let gas_price = GasPrice::new(
            num_bigint::BigUint::from(30_000_000_000u64), // 30 gwei
            num_bigint::BigUint::from(1_000_000_000u64),  // 1 gwei
        );

        // Update shared market data
        {
            let mut market = self.market_data.write().await;
            market.update_gas_price(gas_price.clone());
        }

        // Update health tracker
        self.health_tracker.update();

        // Broadcast event
        let _ = self
            .event_tx
            .send(MarketEvent::GasPriceUpdated { gas_price });

        Ok(())
    }
}

//! Computation manager for derived data.
//!
//! The ComputationManager:
//! - Subscribes to MarketEvents from TychoFeed
//! - Runs derived computations (token prices, spot prices, pool depths)
//! - Updates DerivedDataStore (exclusive write access)
//! - Provides read access to workers via shared store reference

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, info, trace, warn};
use tycho_simulation::tycho_common::models::Address;

use super::{
    computation::DerivedComputation,
    computations::{PoolDepthComputation, SpotPriceComputation, TokenGasPriceComputation},
    error::ComputationError,
    store::DerivedDataStore,
};
use crate::{
    feed::events::{EventError, MarketEvent, MarketEventHandler},
    SharedMarketDataRef,
};

/// Thread-safe handle to shared derived data store.
pub type SharedDerivedDataStore = Arc<RwLock<DerivedDataStore>>;

/// Configuration for the ComputationManager.
///
/// TODO: Consider making this a registry of computation configs using `Box<dyn ComputationConfig>`
/// to support dynamic computation registration. This would allow adding new computation types
/// without modifying this struct. For now, we hardcode the three computation types.
#[derive(Debug, Clone)]
pub struct ComputationManagerConfig {
    /// Gas token address (e.g., WETH) for token price computation.
    pub gas_token: Address,
    /// Slippage threshold for pool depth computation (0.0 < threshold < 1.0).
    pub depth_slippage_threshold: f64,
}

impl ComputationManagerConfig {
    /// Creates a new configuration with the given gas token.
    pub fn new(gas_token: Address) -> Self {
        Self { gas_token, depth_slippage_threshold: 0.01 }
    }

    /// Sets the slippage threshold for pool depth computation.
    pub fn with_depth_slippage_threshold(mut self, threshold: f64) -> Self {
        self.depth_slippage_threshold = threshold;
        self
    }
}

/// Manages derived data computations triggered by market events.
pub struct ComputationManager {
    /// Reference to shared market data (read access).
    market_data: SharedMarketDataRef,
    /// Shared derived data store (write access).
    store: SharedDerivedDataStore,
    /// Token gas price computation.
    token_price_computation: TokenGasPriceComputation,
    /// Spot price computation.
    spot_price_computation: SpotPriceComputation,
    /// Pool depth computation.
    pool_depth_computation: PoolDepthComputation,
}

impl ComputationManager {
    /// Creates a new ComputationManager.
    pub fn new(
        config: ComputationManagerConfig,
        market_data: SharedMarketDataRef,
    ) -> Result<Self, ComputationError> {
        let pool_depth_computation = PoolDepthComputation::new(config.depth_slippage_threshold)?;

        Ok(Self {
            market_data,
            store: Arc::new(RwLock::new(DerivedDataStore::new())),
            token_price_computation: TokenGasPriceComputation::new(config.gas_token),
            spot_price_computation: SpotPriceComputation::new(),
            pool_depth_computation,
        })
    }

    /// Returns a reference to the shared derived data store.
    pub fn store(&self) -> SharedDerivedDataStore {
        Arc::clone(&self.store)
    }

    /// Runs the computation manager's main loop.
    ///
    /// Processes market events and updates the derived data store until
    /// shutdown is signaled or the event channel closes.
    pub async fn run(
        mut self,
        mut event_rx: broadcast::Receiver<MarketEvent>,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) {
        info!("computation manager started");

        loop {
            tokio::select! {
                biased;

                _ = shutdown_rx.recv() => {
                    info!("computation manager shutting down");
                    break;
                }

                event_result = event_rx.recv() => {
                    match event_result {
                        Ok(event) => {
                            if let Err(e) = self.handle_event(&event).await {
                                warn!(error = ?e, "failed to handle market event");
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("event channel closed, computation manager shutting down");
                            break;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!(
                                skipped,
                                "computation manager lagged, skipped {} events. Recomputing from current state.",
                                skipped
                            );
                            self.compute_all().await;
                        }
                    }
                }
            }
        }
    }

    /// Runs all computations and updates the store.
    ///
    /// This is called on market updates and lag recovery.
    async fn compute_all(&self) {
        // TODO: Consider passing `SharedMarketDataRef` and `SharedDerivedDataStore` directly
        // to computations and letting them acquire locks as needed. This would allow
        // computations to release locks earlier or use more granular locking strategies.

        // Acquire read locks
        let market_data = self.market_data.read().await;
        let store_read = self.store.read().await;

        let block = market_data.last_updated().map(|b| b.number);
        if block.is_none() {
            warn!("computing derived data without block info - market data may not be initialized");
        }

        // Run all computations
        let token_prices_result = self
            .token_price_computation
            .compute(&market_data, &store_read);
        let spot_prices_result = self
            .spot_price_computation
            .compute(&market_data, &store_read);
        let pool_depths_result = self
            .pool_depth_computation
            .compute(&market_data, &store_read);

        // Release read locks
        drop(store_read);
        drop(market_data);

        // Update store with results
        let mut store_write = self.store.write().await;

        match token_prices_result {
            Ok(prices) => {
                let count = prices.len();
                store_write.set_token_prices(prices, block);
                debug!(count, "updated token prices");
            }
            Err(e) => {
                warn!(error = ?e, "token price computation failed");
            }
        }

        match spot_prices_result {
            Ok(prices) => {
                let count = prices.len();
                store_write.set_spot_prices(prices, block);
                debug!(count, "updated spot prices");
            }
            Err(e) => {
                warn!(error = ?e, "spot price computation failed");
            }
        }

        match pool_depths_result {
            Ok(depths) => {
                let count = depths.len();
                store_write.set_pool_depths(depths, block);
                debug!(count, "updated pool depths");
            }
            Err(e) => {
                warn!(error = ?e, "pool depth computation failed");
            }
        }
    }
}

#[async_trait]
impl MarketEventHandler for ComputationManager {
    async fn handle_event(&mut self, event: &MarketEvent) -> Result<(), EventError> {
        match event {
            MarketEvent::MarketUpdated { added_components, removed_components, updated_components }
                if !added_components.is_empty() ||
                    !removed_components.is_empty() ||
                    !updated_components.is_empty() =>
            {
                trace!(
                    added = added_components.len(),
                    removed = removed_components.len(),
                    updated = updated_components.len(),
                    "market updated, running all computations"
                );

                self.compute_all().await;
            }
            MarketEvent::GasPriceUpdated { .. } => {
                trace!("gas price updated, recomputing token prices");

                // Acquire read locks
                let market_data = self.market_data.read().await;
                let store_read = self.store.read().await;

                let block = market_data.last_updated().map(|b| b.number);
                if block.is_none() {
                    warn!("recomputing token prices without block info");
                }

                // Run token price computation only
                let result = self
                    .token_price_computation
                    .compute(&market_data, &store_read);

                // Release read locks
                drop(store_read);
                drop(market_data);

                // Update store
                match result {
                    Ok(prices) => {
                        let mut store_write = self.store.write().await;
                        let count = prices.len();
                        store_write.set_token_prices(prices, block);
                        debug!(count, "updated token prices after gas price change");
                    }
                    Err(e) => {
                        warn!(error = ?e, "token price computation failed after gas price change");
                    }
                }
            }
            _ => {
                trace!("empty market update, skipping computations");
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tokio::sync::broadcast;

    use super::*;
    use crate::algorithm::test_utils::{setup_market, token, MockProtocolSim};

    fn wrap_market(market: crate::feed::market_data::SharedMarketData) -> SharedMarketDataRef {
        Arc::new(RwLock::new(market))
    }

    fn test_config() -> ComputationManagerConfig {
        let eth = token(1, "ETH");
        ComputationManagerConfig::new(eth.address)
    }

    #[test]
    fn new_creates_manager_with_empty_store() {
        let (market, _) = setup_market(vec![]);
        let market_ref = wrap_market(market);
        let config = test_config();

        let manager = ComputationManager::new(config, market_ref).unwrap();

        let store = manager.store();
        let guard = store.blocking_read();
        assert!(guard.token_prices().is_none());
        assert!(guard.spot_prices().is_none());
        assert!(guard.pool_depths().is_none());
    }

    #[test]
    fn invalid_slippage_threshold_returns_error() {
        let (market, _) = setup_market(vec![]);
        let market_ref = wrap_market(market);
        let config = test_config().with_depth_slippage_threshold(1.5);

        let result = ComputationManager::new(config, market_ref);
        assert!(matches!(result, Err(ComputationError::InvalidConfiguration(_))));
    }

    #[tokio::test]
    async fn handle_event_runs_computations_on_market_update() {
        let eth = token(1, "ETH");
        let usdc = token(2, "USDC");

        let (market, _) = setup_market(vec![(
            "eth_usdc",
            &eth,
            &usdc,
            MockProtocolSim::new(2000).with_gas(0),
        )]);
        let market_ref = wrap_market(market);

        let config = ComputationManagerConfig::new(eth.address.clone());
        let mut manager = ComputationManager::new(config, market_ref).unwrap();

        let event = MarketEvent::MarketUpdated {
            added_components: HashMap::from([(
                "eth_usdc".to_string(),
                vec![eth.address.clone(), usdc.address.clone()],
            )]),
            removed_components: vec![],
            updated_components: vec![],
        };

        manager.handle_event(&event).await.unwrap();

        let store = manager.store();
        let guard = store.read().await;
        assert!(guard.token_prices().is_some());
        assert!(guard.spot_prices().is_some());
    }

    #[tokio::test]
    async fn handle_event_skips_empty_update() {
        let (market, _) = setup_market(vec![]);
        let market_ref = wrap_market(market);
        let config = test_config();
        let mut manager = ComputationManager::new(config, market_ref).unwrap();

        let event = MarketEvent::MarketUpdated {
            added_components: HashMap::new(),
            removed_components: vec![],
            updated_components: vec![],
        };

        manager.handle_event(&event).await.unwrap();

        let store = manager.store();
        let guard = store.read().await;
        assert!(guard.token_prices().is_none());
    }

    #[tokio::test]
    async fn run_shuts_down_on_signal() {
        let (market, _) = setup_market(vec![]);
        let market_ref = wrap_market(market);
        let config = test_config();
        let manager = ComputationManager::new(config, market_ref).unwrap();

        let (_event_tx, event_rx) = broadcast::channel::<MarketEvent>(16);
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

        let handle = tokio::spawn(async move {
            manager.run(event_rx, shutdown_rx).await;
        });

        shutdown_tx.send(()).unwrap();

        tokio::time::timeout(tokio::time::Duration::from_secs(1), handle)
            .await
            .expect("manager should shutdown")
            .expect("task should complete successfully");
    }

    #[tokio::test]
    async fn run_shuts_down_on_channel_close() {
        let (market, _) = setup_market(vec![]);
        let market_ref = wrap_market(market);
        let config = test_config();
        let manager = ComputationManager::new(config, market_ref).unwrap();

        let (event_tx, event_rx) = broadcast::channel::<MarketEvent>(16);
        let (_shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);

        let handle = tokio::spawn(async move {
            manager.run(event_rx, shutdown_rx).await;
        });

        drop(event_tx);

        tokio::time::timeout(tokio::time::Duration::from_secs(1), handle)
            .await
            .expect("manager should shutdown on channel close")
            .expect("task should complete successfully");
    }

    #[test]
    fn store_returns_same_reference() {
        let (market, _) = setup_market(vec![]);
        let market_ref = wrap_market(market);
        let config = test_config();
        let manager = ComputationManager::new(config, market_ref).unwrap();

        let store1 = manager.store();
        let store2 = manager.store();

        assert!(Arc::ptr_eq(&store1, &store2));
    }
}
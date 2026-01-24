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
    store::DerivedData,
};
use crate::feed::{
    events::{EventError, MarketEvent, MarketEventHandler},
    market_data::SharedMarketDataRef,
};

/// Thread-safe handle to shared derived data store.
pub type SharedDerivedDataRef = Arc<RwLock<DerivedData>>;

/// Creates a new shared derived data store for async computation tests.
pub fn wrap_derived(store: DerivedData) -> SharedDerivedDataRef {
    Arc::new(RwLock::new(store))
}

/// Creates a new shared derived data instance wrapped in Arc<RwLock<>>.
#[allow(unused)] // TODO: remove when used, method added for parity with market data
pub fn new_shared_derived_data() -> SharedDerivedDataRef {
    wrap_derived(DerivedData::new())
}

/// Configuration for the ComputationManager.
///
/// TODO: Consider making this a registry of computation configs using `Box<dyn ComputationConfig>`
/// to support dynamic computation registration. This would allow adding new computation types
/// without modifying this struct. For now, we hardcode the three computation types.
#[derive(Debug, Clone)]
pub struct ComputationManagerConfig {
    /// Gas token address (e.g., WETH) for token price computation.
    pub gas_token: Address,
    /// Max hop count for token gas price computation.
    pub max_hop: usize,
    /// Slippage threshold for pool depth computation (0.0 < threshold < 1.0).
    pub depth_slippage_threshold: f64,
}

impl ComputationManagerConfig {
    /// Creates a new configuration with the given gas token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the slippage threshold for pool depth computation.
    pub fn with_depth_slippage_threshold(mut self, threshold: f64) -> Self {
        self.depth_slippage_threshold = threshold;
        self
    }

    /// Sets the max hop count for token gas price computation.
    pub fn with_max_hop(mut self, hop_count: usize) -> Self {
        self.max_hop = hop_count;
        self
    }

    /// Sets the gas token address.
    pub fn with_gas_token(mut self, gas_token: Address) -> Self {
        self.gas_token = gas_token;
        self
    }
}

impl Default for ComputationManagerConfig {
    fn default() -> Self {
        Self { gas_token: Address::zero(20), max_hop: 3, depth_slippage_threshold: 0.05 }
    }
}

/// Manages derived data computations triggered by market events.
pub struct ComputationManager {
    /// Reference to shared market data (read access).
    market_data: SharedMarketDataRef,
    /// Shared derived data store (write access).
    store: SharedDerivedDataRef,
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
            store: wrap_derived(DerivedData::new()),
            token_price_computation: TokenGasPriceComputation::default()
                .with_max_hops(config.max_hop)
                .with_gas_token(config.gas_token),
            spot_price_computation: SpotPriceComputation::new(),
            pool_depth_computation,
        })
    }

    /// Returns a reference to the shared derived data store.
    pub fn store(&self) -> SharedDerivedDataRef {
        Arc::clone(&self.store)
    }

    /// Runs the computation manager's main loop.
    ///
    /// Processes market events and updates the derived data store until
    /// shutdown is signaled or the event channel closes.
    #[allow(unused)]
    pub(crate) async fn run(
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
    ///
    /// **Dependency order**:
    /// 1. `SpotPriceComputation` - no dependencies
    /// 2. `TokenGasPriceComputation` - depends on spot_prices in store
    /// 3. `PoolDepthComputation` - no dependencies (runs in parallel with token prices)
    async fn compute_all(&self) {
        // Get block info for tracking
        let block = {
            let market_guard = self.market_data.read().await;
            let block = market_guard
                .last_updated()
                .map(|b| b.number);
            if block.is_none() {
                warn!("computing derived data without block info - market data may not be initialized");
            }
            block
        };

        // Phase 1: Compute spot prices first (no dependencies)
        let spot_prices_result = self
            .spot_price_computation
            .compute(&self.market_data, &self.store)
            .await;

        // Write spot prices to store before dependent computations
        match spot_prices_result {
            Ok(prices) => {
                let count = prices.len();
                self.store
                    .write()
                    .await
                    .set_spot_prices(prices, block);
                debug!(count, "updated spot prices");
            }
            Err(e) => {
                warn!(error = ?e, "spot price computation failed");
                // Cannot proceed with token prices if spot prices failed
                return;
            }
        }

        // Phase 2: Run dependent computations (token prices needs spot prices in store)
        // Pool depth has no store dependencies, so it can run in parallel
        let (token_prices_result, pool_depths_result) = tokio::join!(
            self.token_price_computation
                .compute(&self.market_data, &self.store),
            self.pool_depth_computation
                .compute(&self.market_data, &self.store)
        );

        // Update store with remaining results
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
            MarketEvent::MarketUpdated {
                added_components,
                removed_components,
                updated_components,
            } if !added_components.is_empty() ||
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

    #[test]
    fn invalid_slippage_threshold_returns_error() {
        let (market, _) = setup_market(vec![]);
        let config = ComputationManagerConfig::new().with_depth_slippage_threshold(1.5);

        let result = ComputationManager::new(config, market);
        assert!(matches!(result, Err(ComputationError::InvalidConfiguration(_))));
    }

    #[tokio::test]
    async fn handle_event_runs_computations_on_market_update() {
        let eth = token(1, "ETH");
        let usdc = token(2, "USDC");

        let (market, _) =
            setup_market(vec![("eth_usdc", &eth, &usdc, MockProtocolSim::new(2000).with_gas(0))]);

        let config = ComputationManagerConfig::new().with_gas_token(eth.address.clone());
        let mut manager = ComputationManager::new(config, market).unwrap();

        let event = MarketEvent::MarketUpdated {
            added_components: HashMap::from([(
                "eth_usdc".to_string(),
                vec![eth.address.clone(), usdc.address.clone()],
            )]),
            removed_components: vec![],
            updated_components: vec![],
        };

        manager
            .handle_event(&event)
            .await
            .unwrap();

        let store = manager.store();
        let guard = store.read().await;
        assert!(guard.token_prices().is_some());
        assert!(guard.spot_prices().is_some());
    }

    #[tokio::test]
    async fn handle_event_skips_empty_update() {
        let (market, _) = setup_market(vec![]);
        let config = ComputationManagerConfig::new();
        let mut manager = ComputationManager::new(config, market).unwrap();

        let event = MarketEvent::MarketUpdated {
            added_components: HashMap::new(),
            removed_components: vec![],
            updated_components: vec![],
        };

        manager
            .handle_event(&event)
            .await
            .unwrap();

        let store = manager.store();
        let guard = store.read().await;
        assert!(guard.token_prices().is_none());
    }

    #[tokio::test]
    async fn run_shuts_down_on_signal() {
        let (market, _) = setup_market(vec![]);
        let config = ComputationManagerConfig::new();
        let manager = ComputationManager::new(config, market).unwrap();

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
        let config = ComputationManagerConfig::new();
        let manager = ComputationManager::new(config, market).unwrap();

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
}

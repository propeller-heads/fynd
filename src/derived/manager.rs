//! Computation manager for derived data.
//!
//! The ComputationManager:
//! - Subscribes to MarketEvents from TychoFeed
//! - Runs derived computations (token prices, spot prices, pool depths)
//! - Updates DerivedDataStore (exclusive write access)
//! - Provides read access to workers via shared store reference

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};

use async_trait::async_trait;
use tokio::sync::{broadcast, RwLock};
use tracing::{info, trace, warn};
use tycho_simulation::tycho_common::models::Address;

use crate::{feed::market_data::SharedMarketData, types::ComponentId};

/// Information about which components changed in a market update.
///
/// Used to enable incremental computation - only recomputing derived data
/// for components that actually changed.
#[derive(Debug, Clone, Default)]
pub struct ChangedComponents {
    /// Newly added components with their token addresses.
    pub added: HashMap<ComponentId, Vec<Address>>,
    /// Components that were removed.
    pub removed: Vec<ComponentId>,
    /// Components whose state was updated (but not added/removed).
    pub updated: Vec<ComponentId>,
    /// If true, this represents a full recompute (startup/lag recovery).
    pub is_full_recompute: bool,
}

impl ChangedComponents {
    /// Creates a marker for full recompute where all components are considered changed.
    ///
    /// Used for startup and lag recovery scenarios.
    pub fn all(market: &SharedMarketData) -> Self {
        Self {
            added: market.component_topology().clone(),
            removed: vec![],
            updated: vec![],
            is_full_recompute: true,
        }
    }

    /// Returns true if this update changes the graph topology (adds or removes components).
    pub fn is_topology_change(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty()
    }

    /// Returns a HashSet of all changed component IDs.
    pub fn all_changed_ids(&self) -> HashSet<ComponentId> {
        let mut all = HashSet::new();
        all.extend(self.added.keys().cloned());
        all.extend(self.removed.iter().cloned());
        all.extend(self.updated.iter().cloned());
        all
    }
}

use super::{
    computation::DerivedComputation,
    computations::{PoolDepthComputation, SpotPriceComputation, TokenGasPriceComputation},
    error::ComputationError,
    events::DerivedDataEvent,
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
        Self { gas_token: Address::zero(20), max_hop: 2, depth_slippage_threshold: 0.01 }
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
    /// Event broadcaster for derived data updates.
    event_tx: broadcast::Sender<DerivedDataEvent>,
}

impl ComputationManager {
    /// Creates a new ComputationManager.
    ///
    /// Returns the manager and a receiver for derived data events.
    /// Workers can subscribe to the event sender via `event_sender()` to track
    /// computation readiness.
    pub fn new(
        config: ComputationManagerConfig,
        market_data: SharedMarketDataRef,
    ) -> Result<(Self, broadcast::Receiver<DerivedDataEvent>), ComputationError> {
        let pool_depth_computation = PoolDepthComputation::new(config.depth_slippage_threshold)?;
        let (event_tx, event_rx) = broadcast::channel(64);

        Ok((
            Self {
                market_data,
                store: wrap_derived(DerivedData::new()),
                token_price_computation: TokenGasPriceComputation::default()
                    .with_max_hops(config.max_hop)
                    .with_gas_token(config.gas_token),
                spot_price_computation: SpotPriceComputation::new(),
                pool_depth_computation,
                event_tx,
            },
            event_rx,
        ))
    }

    /// Returns a reference to the shared derived data store.
    pub fn store(&self) -> SharedDerivedDataRef {
        Arc::clone(&self.store)
    }

    /// Returns the event sender for workers to subscribe.
    pub fn event_sender(&self) -> broadcast::Sender<DerivedDataEvent> {
        self.event_tx.clone()
    }

    /// Runs the main loop until shutdown or channel close.
    ///
    /// **Note:** Consumes `self`. Call [`store()`](Self::store) before `run()` to retain access.
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
                            let market = self.market_data.read().await;
                            let changed = ChangedComponents::all(&market);
                            drop(market);
                            self.compute_all(&changed).await;
                        }
                    }
                }
            }
        }
    }

    /// Runs all computations and updates the store.
    ///
    /// This is called on market updates and lag recovery.
    /// Broadcasts `DerivedDataEvent` for each computation that completes.
    ///
    /// **Dependency order**:
    /// 1. `SpotPriceComputation` - no dependencies
    /// 2. `TokenGasPriceComputation` - depends on spot_prices in store
    /// 3. `PoolDepthComputation` - no dependencies (runs in parallel with token prices)
    async fn compute_all(&self, changed: &ChangedComponents) {
        let total_start = Instant::now();

        // Get block info for tracking
        let Some(block) = self
            .market_data
            .read()
            .await
            .last_updated()
            .map(|b| b.number)
        else {
            warn!("market data has no last updated block, skipping computations");
            return;
        };

        // Broadcast new block event
        let _ = self.event_tx.send(DerivedDataEvent::NewBlock { block });

        // Phase 1: Compute spot prices first (no dependencies)
        let spot_start = Instant::now();
        let spot_prices_result = self
            .spot_price_computation
            .compute(&self.market_data, &self.store, changed)
            .await;
        let spot_elapsed = spot_start.elapsed();

        // Write spot prices to store before dependent computations
        match spot_prices_result {
            Ok(prices) => {
                let count = prices.len();
                self.store
                    .write()
                    .await
                    .set_spot_prices(prices, block);
                info!(count, elapsed_ms = spot_elapsed.as_millis(), "spot prices computed");
                let _ = self.event_tx.send(DerivedDataEvent::ComputationComplete {
                    computation_id: SpotPriceComputation::ID,
                    block,
                });
            }
            Err(e) => {
                warn!(error = ?e, elapsed_ms = spot_elapsed.as_millis(), "spot price computation failed");
                // Cannot proceed with token prices if spot prices failed
                return;
            }
        }

        // Phase 2: Run dependent computations (token gas prices and pool depths need spot prices)
        let phase2_start = Instant::now();
        let (token_prices_result, pool_depths_result) = tokio::join!(
            self.token_price_computation
                .compute(&self.market_data, &self.store, changed),
            self.pool_depth_computation
                .compute(&self.market_data, &self.store, changed)
        );
        let phase2_elapsed = phase2_start.elapsed();

        // Update store with remaining results
        let mut store_write = self.store.write().await;

        match token_prices_result {
            Ok(prices) => {
                let count = prices.len();
                store_write.set_token_prices(prices, block);
                info!(count, elapsed_ms = phase2_elapsed.as_millis(), "token prices computed");
                let _ = self.event_tx.send(DerivedDataEvent::ComputationComplete {
                    computation_id: TokenGasPriceComputation::ID,
                    block,
                });
            }
            Err(e) => {
                warn!(error = ?e, "token price computation failed");
            }
        }

        match pool_depths_result {
            Ok(depths) => {
                let count = depths.len();
                store_write.set_pool_depths(depths, block);
                info!(count, elapsed_ms = phase2_elapsed.as_millis(), "pool depths computed");
                let _ = self.event_tx.send(DerivedDataEvent::ComputationComplete {
                    computation_id: PoolDepthComputation::ID,
                    block,
                });
            }
            Err(e) => {
                warn!(error = ?e, "pool depth computation failed");
            }
        }

        // Broadcast all complete event
        let _ = self.event_tx.send(DerivedDataEvent::AllComplete { block });

        let total_elapsed = total_start.elapsed();
        info!(
            block,
            total_ms = total_elapsed.as_millis(),
            "all derived computations complete"
        );
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
                    "market updated, running incremental computations"
                );

                let changed = ChangedComponents {
                    added: added_components.clone(),
                    removed: removed_components.clone(),
                    updated: updated_components.clone(),
                    is_full_recompute: false,
                };
                self.compute_all(&changed).await;
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
        let (mut manager, _event_rx) = ComputationManager::new(config, market).unwrap();

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
        let (mut manager, _event_rx) = ComputationManager::new(config, market).unwrap();

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
        let (manager, _event_rx) = ComputationManager::new(config, market).unwrap();

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
        let (manager, _event_rx) = ComputationManager::new(config, market).unwrap();

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

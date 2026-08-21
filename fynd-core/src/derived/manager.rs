//! Computation manager for derived data.
//!
//! The ComputationManager:
//! - Subscribes to MarketEvents from TychoFeed
//! - Runs derived computations and updates the DerivedData store
//! - Provides read access to workers via shared store reference
//!
//! Two instances share one store and one event channel: the per-block manager
//! ([`ComputationManager::new`] + [`run`](ComputationManager::run)) recomputes spot prices and
//! component depths every block, and the token-pricing manager
//! ([`token_pricing`](ComputationManager::token_pricing) +
//! [`run_throttled`](ComputationManager::run_throttled)) solves token prices on its own
//! interval, so its per-token sell legs never delay the per-block data.

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures::future::join_all;
use metrics::{counter, gauge, histogram};
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::{broadcast, RwLock};
use tracing::{error, info, trace, warn};
use tycho_simulation::tycho_common::models::Address;

use crate::types::ComponentId;

/// Information about which components changed in a market update.
///
/// Used to enable incremental computation - only recomputing derived data
/// for components that actually changed.
#[derive(Debug, Clone, Default)]
pub struct ChangedComponents {
    /// Newly added components with their token addresses.
    pub added: FxHashMap<ComponentId, Vec<Address>>,
    /// Components that were removed.
    pub removed: Vec<ComponentId>,
    /// Components whose state was updated (but not added/removed).
    pub updated: Vec<ComponentId>,
    /// If true, this represents a full recompute (startup/lag recovery).
    pub is_full_recompute: bool,
}

impl ChangedComponents {
    /// Returns true if this update changes the graph topology (adds or removes components).
    pub fn is_topology_change(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty()
    }

    /// Returns a HashSet of all changed component IDs.
    pub fn all_changed_ids(&self) -> FxHashSet<ComponentId> {
        let mut all = FxHashSet::default();
        all.extend(self.added.keys().cloned());
        all.extend(self.removed.iter().cloned());
        all.extend(self.updated.iter().cloned());
        all
    }
}

/// Coalesces a drained batch of [`MarketEvent`]s into a single incremental
/// [`ChangedComponents`], applying net semantics: a component that is added
/// then removed within the batch nets to removed; an add supersedes a prior
/// update; a remove supersedes a prior add/update.
///
/// Returns `None` when the batch carries no net changes. The result always has
/// `is_full_recompute: false` — this is the bounded lag-recovery path, never a
/// whole-topology recompute.
fn coalesce_market_events(events: &[MarketEvent]) -> Option<ChangedComponents> {
    let mut added: FxHashMap<ComponentId, Vec<Address>> = FxHashMap::default();
    let mut removed: FxHashSet<ComponentId> = FxHashSet::default();
    let mut updated: FxHashSet<ComponentId> = FxHashSet::default();

    for event in events {
        match event {
            MarketEvent::MarketUpdated {
                added_components,
                removed_components,
                updated_components,
            } => {
                for (id, tokens) in added_components {
                    removed.remove(id);
                    updated.remove(id);
                    added.insert(id.clone(), tokens.clone());
                }
                for id in removed_components {
                    added.remove(id);
                    updated.remove(id);
                    removed.insert(id.clone());
                }
                for id in updated_components {
                    if !added.contains_key(id) && !removed.contains(id) {
                        updated.insert(id.clone());
                    }
                }
            }
        }
    }

    if added.is_empty() && removed.is_empty() && updated.is_empty() {
        return None;
    }
    Some(ChangedComponents {
        added,
        removed: removed.into_iter().collect(),
        updated: updated.into_iter().collect(),
        is_full_recompute: false,
    })
}

use super::{
    computation::{ComputationId, ComputationRequirements, DerivedComputation},
    computations::{ComponentDepthComputation, SpotPriceComputation, TokenGasPriceComputation},
    error::ComputationError,
    events::DerivedDataEvent,
    registry::ErasedComputation,
    store::DerivedData,
};
use crate::feed::{
    events::{EventError, MarketEvent, MarketEventHandler},
    market_data::MarketData,
};

/// Thread-safe handle to shared derived data store.
pub type SharedDerivedDataRef = Arc<RwLock<DerivedData>>;

/// Configuration for the default computation set built by [`ComputationManager::new`].
#[derive(Debug, Clone)]
pub struct ComputationManagerConfig {
    /// Gas token address (e.g., WETH) for token price computation.
    gas_token: Address,
    /// Max hop count for token gas price computation.
    max_hop: usize,
    /// Slippage threshold for component depth computation (0.0 < threshold < 1.0).
    depth_slippage_threshold: f64,
}

impl ComputationManagerConfig {
    /// Creates a new configuration with the given gas token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the slippage threshold for component depth computation.
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

    /// Returns the gas token address.
    pub fn gas_token(&self) -> &Address {
        &self.gas_token
    }

    /// Returns the max hop count.
    pub fn max_hop(&self) -> usize {
        self.max_hop
    }

    /// Returns the depth slippage threshold.
    pub fn depth_slippage_threshold(&self) -> f64 {
        self.depth_slippage_threshold
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
    market_data: MarketData,
    /// Shared derived data store (write access).
    store: SharedDerivedDataRef,
    /// Registered computations, driven in dependency-stage order each block.
    computations: Vec<Box<dyn ErasedComputation>>,
    /// Event broadcaster for derived data updates.
    event_tx: broadcast::Sender<DerivedDataEvent>,
}

/// A dependency-ordered execution plan for the registered computations.
struct ComputationSchedule {
    /// Indices into `ComputationManager::computations`, grouped into stages run in order.
    stages: Vec<Vec<usize>>,
    /// Indices that could not be ordered because of a requirement cycle.
    unscheduled: Vec<usize>,
}

impl ComputationManager {
    /// Creates the per-block manager: spot prices and component depths, run for every block
    /// by [`run`](Self::run).
    ///
    /// Returns the manager and a receiver for derived data events.
    /// Workers can subscribe to the event sender via `event_sender()` to track
    /// computation readiness.
    pub fn new(
        config: ComputationManagerConfig,
        market_data: MarketData,
    ) -> Result<(Self, broadcast::Receiver<DerivedDataEvent>), ComputationError> {
        let (mut manager, event_rx) = Self::empty(market_data);
        manager.register(SpotPriceComputation::new())?;
        manager.register(ComponentDepthComputation::new(config.depth_slippage_threshold)?)?;
        Ok((manager, event_rx))
    }

    /// Creates the token-pricing manager: only [`TokenGasPriceComputation`], writing to the
    /// same store and event channel as `per_block`, so consumers see one coherent set of
    /// derived data and one event stream.
    pub fn token_pricing(
        config: &ComputationManagerConfig,
        per_block: &ComputationManager,
    ) -> Result<Self, ComputationError> {
        let mut manager = Self {
            market_data: per_block.market_data.clone(),
            store: per_block.store(),
            computations: Vec::new(),
            event_tx: per_block.event_sender(),
        };
        manager.register(
            TokenGasPriceComputation::default()
                .with_max_hops(config.max_hop)
                .with_gas_token(config.gas_token.clone()),
        )?;
        Ok(manager)
    }

    /// Creates a manager with no computations registered.
    ///
    /// [`new`](Self::new) builds on this to assemble the default computation set, and
    /// tests drive a custom set through [`register`](Self::register).
    pub(crate) fn empty(market_data: MarketData) -> (Self, broadcast::Receiver<DerivedDataEvent>) {
        let (event_tx, event_rx) = broadcast::channel(64);
        (
            Self {
                market_data,
                store: DerivedData::new_shared(),
                computations: Vec::new(),
                event_tx,
            },
            event_rx,
        )
    }

    /// Registers a computation to be driven each block.
    ///
    /// Registration order is preserved within a dependency stage; cross-stage order is
    /// derived from each computation's
    /// [`requirements`](crate::derived::computation::DerivedComputation::requirements).
    ///
    /// # Errors
    ///
    /// Returns [`ComputationError::DuplicateComputationId`] if a computation with the same
    /// [`ID`](DerivedComputation::ID) is already registered.
    pub(crate) fn register<C: DerivedComputation>(
        &mut self,
        computation: C,
    ) -> Result<(), ComputationError> {
        if self
            .computations
            .iter()
            .any(|existing| existing.id() == C::ID)
        {
            return Err(ComputationError::DuplicateComputationId(C::ID));
        }
        self.computations
            .push(Box::new(computation));
        Ok(())
    }

    /// Returns a reference to the shared derived data store.
    pub fn store(&self) -> SharedDerivedDataRef {
        Arc::clone(&self.store)
    }

    /// Returns the event sender for workers to subscribe.
    pub fn event_sender(&self) -> broadcast::Sender<DerivedDataEvent> {
        self.event_tx.clone()
    }

    /// Runs the loop of a [`token_pricing`](Self::token_pricing) manager: solve at most once
    /// per `min_interval`, coalescing the market events that arrive in between into one
    /// incremental recomputation, so no change is lost to the throttle. The first batch of
    /// events solves immediately.
    ///
    /// **Note:** Consumes `self`. Call [`store()`](Self::store) before this to retain access.
    pub async fn run_throttled(
        self,
        mut event_rx: broadcast::Receiver<MarketEvent>,
        mut shutdown_rx: broadcast::Receiver<()>,
        min_interval: Duration,
    ) {
        info!(min_interval_ms = min_interval.as_millis() as u64, "throttled manager started");

        let mut pending_events: Vec<MarketEvent> = Vec::new();
        let mut last_solve: Option<Instant> = None;

        loop {
            let next_solve_at = last_solve.map_or_else(Instant::now, |at| at + min_interval);
            tokio::select! {
                biased;

                _ = shutdown_rx.recv() => {
                    info!("throttled manager shutting down");
                    break;
                }

                event_result = event_rx.recv() => {
                    match event_result {
                        Ok(event) => pending_events.push(event),
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("event channel closed, throttled manager shutting down");
                            break;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            // Dropped changes self-correct on the affected components'
                            // next update, the same trade-off `recover_from_lag` makes.
                            warn!(skipped, "throttled manager lagged; continuing with buffered events");
                            counter!("derived_manager_lagged_events_total").increment(skipped);
                        }
                    }
                }

                _ = tokio::time::sleep_until(next_solve_at.into()), if !pending_events.is_empty() => {
                    if let Some(changed) = coalesce_market_events(&pending_events) {
                        self.compute_all(&changed).await;
                        last_solve = Some(Instant::now());
                    }
                    pending_events.clear();
                }
            }
        }
    }

    /// Runs the main loop until shutdown or channel close.
    ///
    /// **Note:** Consumes `self`. Call [`store()`](Self::store) before `run()` to retain access.
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
                                "computation manager lagged; draining buffered events and \
                                 recomputing changed components incrementally"
                            );
                            counter!("derived_manager_lag_recoveries_total").increment(1);
                            counter!("derived_manager_lagged_events_total")
                                .increment(skipped);
                            self.recover_from_lag(&mut event_rx).await;
                        }
                    }
                }
            }
        }
    }

    /// Runs all registered computations for the current block and updates the store.
    ///
    /// Computations run in dependency stages derived from their
    /// [`requirements`](crate::derived::computation::DerivedComputation::requirements):
    /// a stage runs concurrently and is written before the next stage starts, and a
    /// computation whose requirement did not succeed this block is skipped and reported
    /// as failed. Broadcasts a `DerivedDataEvent` per computation.
    async fn compute_all(&self, changed: &ChangedComponents) {
        let total_start = Instant::now();

        // Get block info for tracking
        let Some(block) = self
            .market_data
            .read()
            .await
            .last_updated()
            .map(|b| b.number())
        else {
            warn!("market data has no last updated block, skipping computations");
            return;
        };

        // Broadcast new block event
        let _ = self
            .event_tx
            .send(DerivedDataEvent::NewBlock { block });

        let nodes: Vec<(ComputationId, ComputationRequirements)> = self
            .computations
            .iter()
            .map(|computation| (computation.id(), computation.requirements()))
            .collect();
        let schedule = build_schedule(&nodes);
        for &idx in &schedule.unscheduled {
            let computation_id = nodes[idx].0;
            error!(computation = computation_id, "computation skipped: requirement cycle");
            counter!(
                "derived_computation_failures_total",
                "computation" => computation_id,
                "reason" => "cycle"
            )
            .increment(1);
            let _ = self
                .event_tx
                .send(DerivedDataEvent::ComputationFailed { computation_id, block });
        }

        let mut succeeded: FxHashSet<ComputationId> = FxHashSet::default();
        for stage in &schedule.stages {
            // Split the stage into runnable computations and ones whose requirements did
            // not hold this block; the latter are skipped and reported as failed.
            let mut runnable = Vec::new();
            {
                let store = self.store.read().await;
                for &idx in stage {
                    let reqs = &nodes[idx].1;
                    let fresh_ready = reqs
                        .fresh_requirements()
                        .iter()
                        .all(|id| succeeded.contains(id));
                    let stale_ready = reqs
                        .stale_requirements()
                        .iter()
                        .all(|id| succeeded.contains(id) || store.output_block(id).is_some());
                    if fresh_ready && stale_ready {
                        runnable.push(idx);
                    } else {
                        let computation_id = nodes[idx].0;
                        counter!(
                            "derived_computation_failures_total",
                            "computation" => computation_id,
                            "reason" => "upstream_failed"
                        )
                        .increment(1);
                        let _ = self
                            .event_tx
                            .send(DerivedDataEvent::ComputationFailed { computation_id, block });
                    }
                }
            }

            if runnable.is_empty() {
                continue;
            }

            // Run this stage's computations concurrently; they read the store as needed.
            let results = join_all(runnable.iter().map(|&idx| async move {
                let start = Instant::now();
                let result = self.computations[idx]
                    .compute_erased(&self.market_data, &self.store, changed, block)
                    .await;
                (idx, result, start.elapsed())
            }))
            .await;

            // Persist and report in stage order, taking the write lock once for the stage.
            let mut store = self.store.write().await;
            for (idx, result, elapsed) in results {
                let computation_id = nodes[idx].0;
                match result {
                    Ok(write) => {
                        (write.persist)(&mut store);
                        histogram!(
                            "derived_computation_duration_seconds",
                            "computation" => computation_id
                        )
                        .record(elapsed.as_secs_f64());
                        gauge!(
                            "derived_last_success_timestamp_seconds",
                            "computation" => computation_id
                        )
                        .set(unix_now_seconds());
                        info!(
                            computation = computation_id,
                            failed = write.failed_items.len(),
                            elapsed_ms = elapsed.as_millis(),
                            "computation complete"
                        );
                        let _ = self
                            .event_tx
                            .send(DerivedDataEvent::ComputationComplete {
                                computation_id,
                                block,
                                failed_items: write.failed_items,
                            });
                        succeeded.insert(computation_id);
                    }
                    Err(e) => {
                        counter!(
                            "derived_computation_failures_total",
                            "computation" => computation_id,
                            "reason" => "error"
                        )
                        .increment(1);
                        warn!(
                            error = ?e,
                            computation = computation_id,
                            elapsed_ms = elapsed.as_millis(),
                            "computation failed"
                        );
                        let _ = self
                            .event_tx
                            .send(DerivedDataEvent::ComputationFailed { computation_id, block });
                    }
                }
            }
        }

        info!(
            block,
            total_ms = total_start.elapsed().as_millis(),
            "all derived computations complete"
        );
    }

    ////// Recovers from a broadcast lag without a full-topology recompute.
    ///
    /// Drains the events still buffered in `event_rx` (returning the receiver to the live tail so
    /// it cannot immediately re-lag), coalesces them into one incremental `ChangedComponents`,
    /// and recomputes just that union.
    ///
    /// Components lost in the dropped window are not recomputed. Added and updated ones
    /// self-correct on their next `MarketUpdated`; removed ones never reappear, leaving stale
    /// `spot_prices`/`pool_depths` entries for the life of the process. Routing is unaffected:
    /// derived data is only read per graph edge, and a removed component has no edges.
    async fn recover_from_lag(&self, event_rx: &mut broadcast::Receiver<MarketEvent>) {
        let mut drained = Vec::new();
        loop {
            match event_rx.try_recv() {
                Ok(event) => drained.push(event),
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    counter!("derived_manager_lagged_events_total").increment(n);
                    continue;
                }
                Err(broadcast::error::TryRecvError::Closed) => break,
            }
        }
        if let Some(changed) = coalesce_market_events(&drained) {
            self.compute_all(&changed).await;
        }
    }
}

/// Seconds since the Unix epoch, for freshness gauges consumed as `time() - <gauge>`.
fn unix_now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

/// Computes the dependency-ordered execution plan for `nodes` (id paired with its
/// requirements).
///
/// Each node lands in a later stage than the `nodes` it requires; input order is
/// preserved within a stage. Nodes caught in a requirement cycle cannot be ordered and
/// are returned as `unscheduled`. A requirement naming an id absent from `nodes` does
/// not affect ordering (it is left to the runtime readiness check).
fn build_schedule(nodes: &[(ComputationId, ComputationRequirements)]) -> ComputationSchedule {
    let ids: Vec<ComputationId> = nodes
        .iter()
        .map(|(id, _)| *id)
        .collect();
    let mut stage_of: Vec<Option<usize>> = vec![None; nodes.len()];

    loop {
        let mut progressed = false;
        for (idx, (_, reqs)) in nodes.iter().enumerate() {
            if stage_of[idx].is_some() {
                continue;
            }
            let mut stage = 0;
            let mut ready = true;
            for dep in reqs
                .fresh_requirements()
                .iter()
                .chain(reqs.stale_requirements().iter())
            {
                let Some(dep_idx) = ids.iter().position(|id| id == dep) else {
                    continue;
                };
                match stage_of[dep_idx] {
                    Some(dep_stage) => stage = stage.max(dep_stage + 1),
                    None => {
                        ready = false;
                        break;
                    }
                }
            }
            if ready {
                stage_of[idx] = Some(stage);
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }

    let stage_count = stage_of
        .iter()
        .filter_map(|stage| *stage)
        .max()
        .map_or(0, |max| max + 1);
    let mut stages = vec![Vec::new(); stage_count];
    let mut unscheduled = Vec::new();
    for (idx, stage) in stage_of.iter().enumerate() {
        match stage {
            Some(stage) => stages[*stage].push(idx),
            None => unscheduled.push(idx),
        }
    }
    ComputationSchedule { stages, unscheduled }
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
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    use tokio::sync::broadcast;

    use super::*;
    use crate::{
        algorithm::test_utils::{component, setup_market_weighted, token, MockProtocolSim},
        derived::computation::{ComputationOutput, FailedItem, FailedItemError},
        feed::market_data::{MarketData, MarketState},
        types::BlockInfo,
    };

    /// Drains all currently-pending events from a broadcast receiver into a Vec.
    fn drain_events(rx: &mut broadcast::Receiver<DerivedDataEvent>) -> Vec<DerivedDataEvent> {
        let mut events = vec![];
        loop {
            match rx.try_recv() {
                Ok(e) => events.push(e),
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(broadcast::error::TryRecvError::Closed) => break,
            }
        }
        events
    }

    // --- coalesce_market_events: net semantics over a drained batch (pure) ---------

    #[test]
    fn coalesce_empty_batch_returns_none() {
        assert!(coalesce_market_events(&[]).is_none());
    }

    #[test]
    fn coalesce_unions_added_and_updated_across_events() {
        let eth = token(1, "ETH");
        let usdc = token(2, "USDC");
        let e1 = MarketEvent::MarketUpdated {
            added_components: FxHashMap::from_iter([(
                "eth_usdc".to_string(),
                vec![eth.address.clone(), usdc.address.clone()],
            )]),
            removed_components: vec![],
            updated_components: vec![],
        };
        let e2 = MarketEvent::MarketUpdated {
            added_components: FxHashMap::default(),
            removed_components: vec![],
            updated_components: vec!["eth_usdc".to_string(), "dai_usdc".to_string()],
        };
        let c = coalesce_market_events(&[e1, e2]).expect("net changes present");
        assert!(!c.is_full_recompute);
        // eth_usdc was added, so it stays in `added` (not double-counted in `updated`)
        assert!(c.added.contains_key("eth_usdc"));
        assert!(!c
            .updated
            .contains(&"eth_usdc".to_string()));
        // dai_usdc only ever appeared as updated
        assert!(c
            .updated
            .contains(&"dai_usdc".to_string()));
    }

    #[test]
    fn coalesce_add_then_remove_nets_to_removed() {
        let eth = token(1, "ETH");
        let usdc = token(2, "USDC");
        let add = MarketEvent::MarketUpdated {
            added_components: FxHashMap::from_iter([(
                "eth_usdc".to_string(),
                vec![eth.address.clone(), usdc.address.clone()],
            )]),
            removed_components: vec![],
            updated_components: vec![],
        };
        let remove = MarketEvent::MarketUpdated {
            added_components: FxHashMap::default(),
            removed_components: vec!["eth_usdc".to_string()],
            updated_components: vec![],
        };
        let c = coalesce_market_events(&[add, remove]).expect("net removal present");
        assert!(!c.added.contains_key("eth_usdc"));
        assert!(c
            .removed
            .contains(&"eth_usdc".to_string()));
        assert!(!c
            .updated
            .contains(&"eth_usdc".to_string()));
    }

    #[tokio::test]
    async fn lag_recovery_recomputes_incrementally_and_drains_to_tail() {
        let eth = token(1, "ETH");
        let usdc = token(2, "USDC");
        let (market, _) = setup_market_weighted(vec![(
            "eth_usdc",
            &eth,
            &usdc,
            MockProtocolSim::new(2000.0).with_gas(0),
        )]);
        let config = ComputationManagerConfig::new().with_gas_token(eth.address.clone());
        let (manager, _out_rx) = ComputationManager::new(config, market).unwrap();

        // Capacity-2 input channel; send 5 without reading to force Lagged on recv.
        let (tx, mut rx) = broadcast::channel::<MarketEvent>(2);
        for _ in 0..5 {
            tx.send(MarketEvent::MarketUpdated {
                added_components: FxHashMap::from_iter([(
                    "eth_usdc".to_string(),
                    vec![eth.address.clone(), usdc.address.clone()],
                )]),
                removed_components: vec![],
                updated_components: vec![],
            })
            .unwrap();
        }
        let err = rx
            .recv()
            .await
            .expect_err("receiver must have lagged");
        assert!(matches!(err, broadcast::error::RecvError::Lagged(_)));

        manager.recover_from_lag(&mut rx).await;

        // Recovery recomputed the coalesced change incrementally...
        let store = manager.store();
        let guard = store.read().await;
        assert!(guard.spot_prices().is_some());
        assert!(guard.component_depths().is_some());
        drop(guard);
        // ...and the receiver is back at the live tail (buffer drained).
        assert!(matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
    }

    // --- build_schedule: dependency staging (pure) --------------------------------

    #[test]
    fn schedule_empty_has_no_stages() {
        let schedule = build_schedule(&[]);
        assert!(schedule.stages.is_empty());
        assert!(schedule.unscheduled.is_empty());
    }

    #[test]
    fn schedule_single_root_is_one_stage() {
        let schedule = build_schedule(&[("a", ComputationRequirements::none())]);
        assert_eq!(schedule.stages, vec![vec![0]]);
        assert!(schedule.unscheduled.is_empty());
    }

    #[test]
    fn schedule_independent_roots_share_one_stage() {
        let schedule = build_schedule(&[
            ("a", ComputationRequirements::none()),
            ("b", ComputationRequirements::none()),
        ]);
        assert_eq!(schedule.stages, vec![vec![0, 1]]);
        assert!(schedule.unscheduled.is_empty());
    }

    #[test]
    fn schedule_chain_orders_into_successive_stages() {
        // a <- b <- c
        let schedule = build_schedule(&[
            ("a", ComputationRequirements::none()),
            ("b", ComputationRequirements::fresh(["a"])),
            ("c", ComputationRequirements::fresh(["b"])),
        ]);
        assert_eq!(schedule.stages, vec![vec![0], vec![1], vec![2]]);
        assert!(schedule.unscheduled.is_empty());
    }

    #[test]
    fn schedule_diamond_places_join_after_both_parents() {
        // a <- {b, c} <- d; mirrors fynd's spot -> {token, component} fan-out.
        let schedule = build_schedule(&[
            ("a", ComputationRequirements::none()),
            ("b", ComputationRequirements::fresh(["a"])),
            ("c", ComputationRequirements::fresh(["a"])),
            ("d", ComputationRequirements::fresh(["b", "c"])),
        ]);
        assert_eq!(schedule.stages, vec![vec![0], vec![1, 2], vec![3]]);
        assert!(schedule.unscheduled.is_empty());
    }

    #[test]
    fn schedule_preserves_input_order_within_a_stage() {
        let schedule = build_schedule(&[
            ("a", ComputationRequirements::none()),
            ("b", ComputationRequirements::fresh(["a"])),
            ("c", ComputationRequirements::fresh(["a"])),
        ]);
        // b registered before c, so it comes first in the shared stage.
        assert_eq!(schedule.stages, vec![vec![0], vec![1, 2]]);
    }

    #[test]
    fn schedule_stale_requirement_orders_after_its_producer() {
        let schedule = build_schedule(&[
            ("a", ComputationRequirements::none()),
            ("b", ComputationRequirements::stale(["a"])),
        ]);
        assert_eq!(schedule.stages, vec![vec![0], vec![1]]);
    }

    #[test]
    fn schedule_requirement_on_unregistered_id_does_not_affect_ordering() {
        // "ghost" is not registered, so "a" is treated as a root.
        let schedule = build_schedule(&[("a", ComputationRequirements::fresh(["ghost"]))]);
        assert_eq!(schedule.stages, vec![vec![0]]);
        assert!(schedule.unscheduled.is_empty());
    }

    #[test]
    fn schedule_two_node_cycle_is_unscheduled() {
        let schedule = build_schedule(&[
            ("a", ComputationRequirements::fresh(["b"])),
            ("b", ComputationRequirements::fresh(["a"])),
        ]);
        assert!(schedule.stages.is_empty());
        assert_eq!(schedule.unscheduled, vec![0, 1]);
    }

    #[test]
    fn schedule_isolates_cycle_from_schedulable_nodes() {
        // "root" schedules normally; "x" and "y" form a cycle and are unscheduled.
        let schedule = build_schedule(&[
            ("root", ComputationRequirements::none()),
            ("x", ComputationRequirements::fresh(["y"])),
            ("y", ComputationRequirements::fresh(["x"])),
        ]);
        assert_eq!(schedule.stages, vec![vec![0]]);
        assert_eq!(schedule.unscheduled, vec![1, 2]);
    }

    #[test]
    fn invalid_slippage_threshold_returns_error() {
        let (market, _) = setup_market_weighted(vec![]);
        let config = ComputationManagerConfig::new().with_depth_slippage_threshold(1.5);

        let result = ComputationManager::new(config, market);
        assert!(matches!(result, Err(ComputationError::InvalidConfiguration(_))));
    }

    #[tokio::test]
    async fn handle_event_runs_computations_on_market_update() {
        let eth = token(1, "ETH");
        let usdc = token(2, "USDC");

        let (market, _) = setup_market_weighted(vec![(
            "eth_usdc",
            &eth,
            &usdc,
            MockProtocolSim::new(2000.0).with_gas(0),
        )]);

        let config = ComputationManagerConfig::new().with_gas_token(eth.address.clone());
        let (mut manager, _event_rx) = ComputationManager::new(config, market).unwrap();

        let event = MarketEvent::MarketUpdated {
            added_components: FxHashMap::from_iter([(
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
        assert!(guard.spot_prices().is_some());
        assert!(guard.component_depths().is_some());
        // Token prices live in their own throttled manager, not the per-block set.
        assert!(guard.token_prices().is_none());
    }

    #[tokio::test]
    async fn handle_event_skips_empty_update() {
        let (market, _) = setup_market_weighted(vec![]);
        let config = ComputationManagerConfig::new();
        let (mut manager, _event_rx) = ComputationManager::new(config, market).unwrap();

        let event = MarketEvent::MarketUpdated {
            added_components: FxHashMap::default(),
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
        let (market, _) = setup_market_weighted(vec![]);
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

    // --- registry seam: custom computations driven through the manager -------------

    #[derive(Clone, Debug, PartialEq)]
    struct CounterOutput(u32);

    /// A minimal computation that ignores market data and uses the default `persist`
    /// (the path a downstream computation takes: store into the generic slot).
    struct CounterComputation;

    #[async_trait::async_trait]
    impl DerivedComputation for CounterComputation {
        type Output = CounterOutput;
        const ID: ComputationId = "counter";

        async fn compute(
            &self,
            _market: &MarketData,
            _store: &SharedDerivedDataRef,
            _changed: &ChangedComponents,
        ) -> Result<ComputationOutput<Self::Output>, ComputationError> {
            Ok(ComputationOutput::success(CounterOutput(7)))
        }
    }

    /// Builds a market carrying a `last_updated` block so `compute_all` runs.
    fn market_with_block() -> MarketData {
        let eth = token(1, "ETH");
        let usdc = token(2, "USDC");
        let (market, _) = setup_market_weighted(vec![(
            "eth_usdc",
            &eth,
            &usdc,
            MockProtocolSim::new(2000.0).with_gas(0),
        )]);
        market
    }

    #[tokio::test]
    async fn registered_custom_computation_runs_and_persists_via_default_slot() {
        let (mut manager, mut event_rx) = ComputationManager::empty(market_with_block());
        manager
            .register(CounterComputation)
            .unwrap();

        manager
            .compute_all(&ChangedComponents { is_full_recompute: true, ..Default::default() })
            .await;

        let store = manager.store();
        let guard = store.read().await;
        assert_eq!(
            guard.output::<CounterOutput>(CounterComputation::ID),
            Some(&CounterOutput(7)),
            "default persist should write the output into the generic slot"
        );
        assert!(guard
            .output_block(CounterComputation::ID)
            .is_some());

        let events = drain_events(&mut event_rx);
        assert!(
            events.iter().any(|e| matches!(
                e,
                DerivedDataEvent::ComputationComplete { computation_id: "counter", .. }
            )),
            "expected ComputationComplete(counter), got: {events:?}"
        );
    }

    #[test]
    fn registering_duplicate_id_is_rejected() {
        let (mut manager, _event_rx) = ComputationManager::empty(market_with_block());
        manager
            .register(CounterComputation)
            .unwrap();

        let result = manager.register(CounterComputation);

        assert!(matches!(result, Err(ComputationError::DuplicateComputationId("counter"))));
    }

    // --- exact event sequences (characterization) ---------------------------------

    /// Reduces an event stream to `(kind, computation_id)` pairs for exact comparison.
    fn event_summary(events: &[DerivedDataEvent]) -> Vec<(&'static str, &'static str)> {
        events
            .iter()
            .map(|event| match event {
                DerivedDataEvent::NewBlock { .. } => ("new_block", ""),
                DerivedDataEvent::ComputationComplete { computation_id, .. } => {
                    ("complete", *computation_id)
                }
                DerivedDataEvent::ComputationFailed { computation_id, .. } => {
                    ("failed", *computation_id)
                }
            })
            .collect()
    }

    /// Subscribes, runs one full-recompute pass, and returns the events it emitted.
    async fn run_full_recompute(manager: &ComputationManager) -> Vec<DerivedDataEvent> {
        let mut event_rx = manager.event_sender().subscribe();
        manager
            .compute_all(&ChangedComponents { is_full_recompute: true, ..Default::default() })
            .await;
        drain_events(&mut event_rx)
    }

    /// Defines a market-independent test computation with a fixed id, requirements, result.
    macro_rules! test_computation {
        ($name:ident, $id:literal, $reqs:expr, $result:expr) => {
            struct $name;

            #[async_trait::async_trait]
            impl DerivedComputation for $name {
                type Output = ();
                const ID: ComputationId = $id;

                fn requirements(&self) -> ComputationRequirements {
                    $reqs
                }

                async fn compute(
                    &self,
                    _market: &MarketData,
                    _store: &SharedDerivedDataRef,
                    _changed: &ChangedComponents,
                ) -> Result<ComputationOutput<Self::Output>, ComputationError> {
                    $result
                }
            }
        };
    }

    test_computation!(
        RootOk,
        "root",
        ComputationRequirements::none(),
        Ok(ComputationOutput::success(()))
    );
    test_computation!(
        DepOnRoot,
        "dep",
        ComputationRequirements::fresh(["root"]),
        Ok(ComputationOutput::success(()))
    );
    test_computation!(
        SecondDepOnRoot,
        "dep2",
        ComputationRequirements::fresh(["root"]),
        Ok(ComputationOutput::success(()))
    );
    test_computation!(
        RootErr,
        "boom",
        ComputationRequirements::none(),
        Err(ComputationError::InvalidConfiguration("boom".to_string()))
    );
    test_computation!(
        DepOnBoom,
        "dep_boom",
        ComputationRequirements::fresh(["boom"]),
        Ok(ComputationOutput::success(()))
    );
    test_computation!(
        ThirdOnBoom,
        "third",
        ComputationRequirements::fresh(["dep_boom"]),
        Ok(ComputationOutput::success(()))
    );
    test_computation!(
        StaleDepOnFlaky,
        "stale_dep",
        ComputationRequirements::stale(["flaky"]),
        Ok(ComputationOutput::success(()))
    );
    test_computation!(
        GhostDependent,
        "needs_ghost",
        ComputationRequirements::fresh(["ghost"]),
        Ok(ComputationOutput::success(()))
    );
    test_computation!(
        PartialProducer,
        "partial",
        ComputationRequirements::none(),
        Ok(ComputationOutput::with_failures(
            (),
            vec![FailedItem { key: "x".to_string(), error: FailedItemError::MissingSpotPrice }]
        ))
    );
    test_computation!(
        DepOnPartial,
        "dep_partial",
        ComputationRequirements::fresh(["partial"]),
        Ok(ComputationOutput::success(()))
    );

    /// A producer that succeeds while its flag is set and fails once it is cleared, so a
    /// later block can exercise the stale-dependency path (producer failed this block, but
    /// a prior-block value is still in the store).
    struct FlakyProducer {
        succeed: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl DerivedComputation for FlakyProducer {
        type Output = ();
        const ID: ComputationId = "flaky";

        async fn compute(
            &self,
            _market: &MarketData,
            _store: &SharedDerivedDataRef,
            _changed: &ChangedComponents,
        ) -> Result<ComputationOutput<Self::Output>, ComputationError> {
            if self.succeed.load(Ordering::SeqCst) {
                Ok(ComputationOutput::success(()))
            } else {
                Err(ComputationError::InvalidConfiguration("flaky".to_string()))
            }
        }
    }

    #[tokio::test]
    async fn events_follow_dependency_order_across_stages() {
        let (mut manager, _event_rx) = ComputationManager::empty(market_with_block());
        manager.register(RootOk).unwrap();
        manager.register(DepOnRoot).unwrap();

        let events = run_full_recompute(&manager).await;

        assert_eq!(
            event_summary(&events),
            vec![("new_block", ""), ("complete", "root"), ("complete", "dep")]
        );
    }

    #[tokio::test]
    async fn events_preserve_registration_order_within_a_stage() {
        let (mut manager, _event_rx) = ComputationManager::empty(market_with_block());
        manager.register(RootOk).unwrap();
        manager.register(DepOnRoot).unwrap();
        manager
            .register(SecondDepOnRoot)
            .unwrap();

        let events = run_full_recompute(&manager).await;

        // root runs in stage 0; dep then dep2 share stage 1 in registration order.
        assert_eq!(
            event_summary(&events),
            vec![
                ("new_block", ""),
                ("complete", "root"),
                ("complete", "dep"),
                ("complete", "dep2"),
            ]
        );
    }

    #[tokio::test]
    async fn failed_dependency_cascades_to_dependents() {
        let (mut manager, _event_rx) = ComputationManager::empty(market_with_block());
        manager.register(RootErr).unwrap();
        manager.register(DepOnBoom).unwrap();

        let events = run_full_recompute(&manager).await;

        // boom fails in stage 0; its dependent is skipped and reported failed.
        assert_eq!(
            event_summary(&events),
            vec![("new_block", ""), ("failed", "boom"), ("failed", "dep_boom")]
        );
    }

    #[tokio::test]
    async fn computation_with_unregistered_requirement_is_skipped() {
        let (mut manager, _event_rx) = ComputationManager::empty(market_with_block());
        manager
            .register(GhostDependent)
            .unwrap();

        let events = run_full_recompute(&manager).await;

        // "ghost" is never registered, so its fresh dependent never runs.
        assert_eq!(event_summary(&events), vec![("new_block", ""), ("failed", "needs_ghost")]);
    }

    #[tokio::test]
    async fn fresh_dependent_runs_when_producer_succeeds_partially() {
        let (mut manager, _event_rx) = ComputationManager::empty(market_with_block());
        manager
            .register(PartialProducer)
            .unwrap();
        manager.register(DepOnPartial).unwrap();

        let events = run_full_recompute(&manager).await;

        // A partial success (Ok with failed_items) still counts as succeeded, so the fresh
        // dependent runs -- the compatibility invariant with the old hardcoded flow.
        assert_eq!(
            event_summary(&events),
            vec![("new_block", ""), ("complete", "partial"), ("complete", "dep_partial"),]
        );
    }

    #[tokio::test]
    async fn failure_cascade_propagates_through_three_levels() {
        let (mut manager, _event_rx) = ComputationManager::empty(market_with_block());
        manager.register(RootErr).unwrap();
        manager.register(DepOnBoom).unwrap();
        manager.register(ThirdOnBoom).unwrap();

        let events = run_full_recompute(&manager).await;

        // boom fails; dep_boom is skipped; third (needs dep_boom) is skipped transitively.
        assert_eq!(
            event_summary(&events),
            vec![
                ("new_block", ""),
                ("failed", "boom"),
                ("failed", "dep_boom"),
                ("failed", "third"),
            ]
        );
    }

    #[tokio::test]
    async fn stale_dependency_runs_on_prior_value_after_producer_fails() {
        let succeed = Arc::new(AtomicBool::new(true));
        let (mut manager, _event_rx) = ComputationManager::empty(market_with_block());
        manager
            .register(FlakyProducer { succeed: Arc::clone(&succeed) })
            .unwrap();
        manager
            .register(StaleDepOnFlaky)
            .unwrap();

        // Block 1: producer succeeds and its value is stored.
        let first = run_full_recompute(&manager).await;
        assert_eq!(
            event_summary(&first),
            vec![("new_block", ""), ("complete", "flaky"), ("complete", "stale_dep")]
        );

        // Block 2: producer fails, but its prior-block value remains, so the stale
        // dependent still runs.
        succeed.store(false, Ordering::SeqCst);
        let second = run_full_recompute(&manager).await;
        assert_eq!(
            event_summary(&second),
            vec![("new_block", ""), ("failed", "flaky"), ("complete", "stale_dep")]
        );
    }

    #[tokio::test]
    async fn spot_price_failure_cascades_to_depths() {
        // Real fynd flow: a full recompute with no sim state makes spot prices fail outright.
        // Pool depths depend on them and fail with them. Token prices are unaffected by
        // construction — they run in their own manager and read no derived data.
        let (manager, _event_rx) = ComputationManager::new(
            ComputationManagerConfig::new(),
            market_with_component_no_sim_state(),
        )
        .unwrap();

        let events = run_full_recompute(&manager).await;

        assert_eq!(
            event_summary(&events),
            vec![("new_block", ""), ("failed", "spot_prices"), ("failed", "pool_depths")]
        );
    }

    #[tokio::test]
    async fn token_pricing_manager_writes_to_the_shared_store() {
        let eth = token(1, "ETH");
        let usdc = token(2, "USDC");
        let (market, _) = setup_market_weighted(vec![(
            "eth_usdc",
            &eth,
            &usdc,
            MockProtocolSim::new(2000.0).with_gas(0),
        )]);

        let config = ComputationManagerConfig::new().with_gas_token(eth.address.clone());
        let (per_block, _event_rx) = ComputationManager::new(config.clone(), market).unwrap();
        let mut pricing = ComputationManager::token_pricing(&config, &per_block).unwrap();

        let event = MarketEvent::MarketUpdated {
            added_components: HashMap::from([(
                "eth_usdc".to_string(),
                vec![eth.address.clone(), usdc.address.clone()],
            )]),
            removed_components: vec![],
            updated_components: vec![],
        };
        pricing
            .handle_event(&event)
            .await
            .unwrap();

        // Prices land in the per-block manager's store: it is shared.
        let store = per_block.store();
        let guard = store.read().await;
        assert!(guard.token_prices().is_some());
        assert!(guard.spot_prices().is_none(), "the pricing manager runs no other computation");
    }

    #[tokio::test]
    async fn run_throttled_solves_a_buffered_batch_once() {
        let eth = token(1, "ETH");
        let usdc = token(2, "USDC");
        let (market, _) = setup_market_weighted(vec![(
            "eth_usdc",
            &eth,
            &usdc,
            MockProtocolSim::new(2000.0).with_gas(0),
        )]);

        let config = ComputationManagerConfig::new().with_gas_token(eth.address.clone());
        let (per_block, mut derived_rx) = ComputationManager::new(config.clone(), market).unwrap();
        let pricing = ComputationManager::token_pricing(&config, &per_block).unwrap();
        let store = per_block.store();

        // Buffer several events before the loop starts: they must coalesce into one solve.
        let (event_tx, event_rx) = broadcast::channel::<MarketEvent>(16);
        for _ in 0..3 {
            event_tx
                .send(MarketEvent::MarketUpdated {
                    added_components: HashMap::from([(
                        "eth_usdc".to_string(),
                        vec![eth.address.clone(), usdc.address.clone()],
                    )]),
                    removed_components: vec![],
                    updated_components: vec![],
                })
                .unwrap();
        }
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(1);
        let handle = tokio::spawn(async move {
            pricing
                .run_throttled(event_rx, shutdown_rx, Duration::from_secs(3600))
                .await;
        });

        // Wait for the solve to land, then stop the loop.
        while store
            .read()
            .await
            .token_prices()
            .is_none()
        {
            tokio::task::yield_now().await;
        }
        shutdown_tx.send(()).unwrap();
        handle.await.unwrap();

        let mut completions = 0;
        while let Ok(event) = derived_rx.try_recv() {
            if let DerivedDataEvent::ComputationComplete {
                computation_id: "token_prices", ..
            } = event
            {
                completions += 1;
            }
        }
        assert_eq!(completions, 1, "three buffered events must coalesce into one solve");
    }

    /// Creates a market with a component in topology but WITHOUT simulation state.
    ///
    /// Used to trigger `TotalFailure` in spot_price computation (full recompute with
    /// all components missing sim_state → succeeded == 0 → failure).
    fn market_with_component_no_sim_state() -> MarketData {
        let eth = token(1, "ETH");
        let usdc = token(2, "USDC");
        let component = component("component", &[eth.clone(), usdc.clone()]);

        let mut market = MarketState::new();
        market.update_last_updated(BlockInfo::new(10, "0xhash".into(), 0));
        market.upsert_components(std::iter::once(component));
        // Note: no update_states() — simulation state is intentionally absent
        market.upsert_tokens([eth, usdc]);
        MarketData::new(std::sync::Arc::new(tokio::sync::RwLock::new(market)))
    }

    /// Creates a market with two components: one with sim state (component succeeds) and one
    /// without (component fails). Used to trigger partial spot price failure.
    fn market_with_mixed_sim_states() -> MarketData {
        let eth = token(1, "ETH");
        let usdc = token(2, "USDC");
        let dai = token(3, "DAI");

        let component1 = component("eth_usdc", &[eth.clone(), usdc.clone()]);
        let component2 = component("eth_dai", &[eth.clone(), dai.clone()]);

        let mut market = MarketState::new();
        market.update_last_updated(BlockInfo::new(10, "0xhash".into(), 0));
        market.upsert_components([component1, component2]);
        // Only component1 has simulation state; component2 intentionally has none
        market
            .update_states([("eth_usdc".to_string(), Box::new(MockProtocolSim::new(2000.0)) as _)]);
        market.upsert_tokens([eth, usdc, dai]);
        MarketData::new(std::sync::Arc::new(tokio::sync::RwLock::new(market)))
    }

    #[tokio::test]
    async fn test_spot_price_failure_broadcasts_computation_failed() {
        let market = market_with_component_no_sim_state();
        let config = ComputationManagerConfig::new();
        let (manager, mut event_rx) = ComputationManager::new(config, market).unwrap();

        // Full recompute with components that have no sim_state → TotalFailure
        let changed = ChangedComponents { is_full_recompute: true, ..Default::default() };
        manager.compute_all(&changed).await;

        let events = drain_events(&mut event_rx);

        assert!(
            events.iter().any(|e| matches!(
                e,
                DerivedDataEvent::ComputationFailed { computation_id: "spot_prices", .. }
            )),
            "expected ComputationFailed(spot_prices) in events: {events:?}"
        );
    }

    #[tokio::test]
    async fn run_shuts_down_on_channel_close() {
        let (market, _) = setup_market_weighted(vec![]);
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

    #[tokio::test]
    async fn partial_spot_price_failure_broadcasts_computation_complete() {
        // market_with_mixed_sim_states has component1 (with sim state) and component2 (without)
        // → spot price computation partially succeeds → ComputationComplete with failed_items
        let market = market_with_mixed_sim_states();
        let config = ComputationManagerConfig::new();
        let (manager, mut event_rx) = ComputationManager::new(config, market).unwrap();

        let changed = ChangedComponents { is_full_recompute: true, ..Default::default() };
        manager.compute_all(&changed).await;

        let events = drain_events(&mut event_rx);

        // Should broadcast ComputationComplete (not ComputationFailed) because component1 succeeds
        assert!(
            events.iter().any(|e| matches!(
                e,
                DerivedDataEvent::ComputationComplete { computation_id: "spot_prices", .. }
            )),
            "expected ComputationComplete(spot_prices), got: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(
                e,
                DerivedDataEvent::ComputationFailed { computation_id: "spot_prices", .. }
            )),
            "should not broadcast ComputationFailed for partial failure"
        );

        // The ComputationComplete event should carry the failed item for component2
        let complete = events.iter().find(|e| {
            matches!(e, DerivedDataEvent::ComputationComplete { computation_id: "spot_prices", .. })
        });
        if let Some(DerivedDataEvent::ComputationComplete { failed_items, .. }) = complete {
            assert!(
                !failed_items.is_empty(),
                "ComputationComplete should carry failed_items for component2"
            );
        }

        // The store should persist the failure reason for the failed component.
        // market_with_mixed_sim_states uses token(1, "ETH") and token(3, "DAI") for component2.
        let eth = token(1, "ETH");
        let dai = token(3, "DAI");
        let store = manager.store();
        let guard = store.read().await;
        let key_eth_dai = ("eth_dai".to_string(), eth.address.clone(), dai.address.clone());
        let key_dai_eth = ("eth_dai".to_string(), dai.address.clone(), eth.address.clone());
        assert!(
            guard
                .spot_price_failure(&key_eth_dai)
                .is_some() ||
                guard
                    .spot_price_failure(&key_dai_eth)
                    .is_some(),
            "store should persist failure reason for eth_dai (missing sim state)"
        );
    }

    // --- metrics ---------------------------------------------------------------------

    /// Mirrors `CounterComputation`, but always fails, to exercise the failure-counter path.
    struct FailingComputation;

    #[async_trait::async_trait]
    impl DerivedComputation for FailingComputation {
        type Output = ();
        const ID: ComputationId = "failing";

        async fn compute(
            &self,
            _market: &MarketData,
            _store: &SharedDerivedDataRef,
            _changed: &ChangedComponents,
        ) -> Result<ComputationOutput<Self::Output>, ComputationError> {
            Err(ComputationError::InvalidConfiguration("always fails".to_string()))
        }
    }

    /// Finds the debug value recorded for `name` carrying every label in `labels`.
    fn find_metric<'a>(
        recorded: &'a [(
            metrics_util::CompositeKey,
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            metrics_util::debugging::DebugValue,
        )],
        name: &str,
        labels: &[(&str, &str)],
    ) -> &'a metrics_util::debugging::DebugValue {
        recorded
            .iter()
            .find(|(key, _, _, _)| {
                key.key().name() == name &&
                    labels
                        .iter()
                        .all(|(label_key, label_value)| {
                            key.key()
                                .labels()
                                .any(|l| l.key() == *label_key && l.value() == *label_value)
                        })
            })
            .map(|(_, _, _, value)| value)
            .unwrap_or_else(|| panic!("missing {name}{labels:?}, got {recorded:?}"))
    }

    #[test]
    fn compute_all_records_derived_metrics() {
        use metrics_util::debugging::DebugValue;

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let (mut manager, _event_rx) = ComputationManager::empty(market_with_block());
                manager
                    .register(CounterComputation)
                    .unwrap();
                manager
                    .compute_all(&ChangedComponents {
                        is_full_recompute: true,
                        ..Default::default()
                    })
                    .await;
            })
        });

        let recorded = snapshotter.snapshot().into_vec();
        let recorded_names: Vec<(String, Vec<String>)> = recorded
            .iter()
            .map(|(key, _, _, _)| {
                (
                    key.key().name().to_string(),
                    key.key()
                        .labels()
                        .map(|l| format!("{}={}", l.key(), l.value()))
                        .collect(),
                )
            })
            .collect();
        for expected in
            ["derived_computation_duration_seconds", "derived_last_success_timestamp_seconds"]
        {
            assert!(
                recorded_names
                    .iter()
                    .any(|(name, labels)| name == expected &&
                        labels.contains(&"computation=counter".to_string())),
                "missing {expected}{{computation=counter}}, got {recorded_names:?}"
            );
        }

        match find_metric(
            &recorded,
            "derived_last_success_timestamp_seconds",
            &[("computation", "counter")],
        ) {
            DebugValue::Gauge(value) => {
                assert!(value.0 > 1.7e9, "gauge value {} not a sane unix timestamp", value.0);
            }
            other => panic!("derived_last_success_timestamp_seconds is not a gauge: {other:?}"),
        }

        match find_metric(
            &recorded,
            "derived_computation_duration_seconds",
            &[("computation", "counter")],
        ) {
            DebugValue::Histogram(samples) => {
                assert!(!samples.is_empty(), "expected at least one recorded duration sample");
            }
            other => panic!("derived_computation_duration_seconds is not a histogram: {other:?}"),
        }
    }

    #[test]
    fn compute_all_records_failure_metric() {
        use metrics_util::debugging::DebugValue;

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime builds");

        metrics::with_local_recorder(&recorder, || {
            rt.block_on(async {
                let (mut manager, _event_rx) = ComputationManager::empty(market_with_block());
                manager
                    .register(FailingComputation)
                    .unwrap();
                manager
                    .compute_all(&ChangedComponents {
                        is_full_recompute: true,
                        ..Default::default()
                    })
                    .await;
            })
        });

        let recorded = snapshotter.snapshot().into_vec();
        match find_metric(
            &recorded,
            "derived_computation_failures_total",
            &[("computation", "failing"), ("reason", "error")],
        ) {
            DebugValue::Counter(value) => {
                assert!(*value >= 1, "expected failure counter >= 1, got {value}");
            }
            other => panic!("derived_computation_failures_total is not a counter: {other:?}"),
        }
    }
}

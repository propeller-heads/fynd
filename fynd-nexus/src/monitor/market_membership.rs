//! Keeps the tracked components in step with the components of the market.

use std::{collections::HashSet, sync::Arc};

use fynd_core::{
    feed::{events::MarketEvent, market_data::MarketData},
    ComponentId,
};
use tokio::sync::broadcast::{error::RecvError, Receiver};
use tracing::{info, warn};

use crate::monitor::{health_store::HealthStore, monitor_clock::MonitorClock};

/// Tracks every component of `market`, then follows `events`: components the market adds join
/// the front of the probe queue, and components it removes leave the store.
///
/// Subscribe to the market events before calling it, so that no change falls between the first
/// read of the market and the first event. It returns when the event channel closes.
pub async fn keep_tracking(
    market: MarketData,
    mut events: Receiver<MarketEvent>,
    store: Arc<HealthStore>,
    clock: MonitorClock,
) {
    track_all(&market, &store, &clock).await;
    loop {
        match events.recv().await {
            Ok(MarketEvent::MarketUpdated { added_components, removed_components, .. }) => {
                for component_id in &removed_components {
                    store.untrack(component_id);
                }
                let added: Vec<ComponentId> = added_components.into_keys().collect();
                track_added(&market, &store, &clock, &added).await;
            }
            Err(RecvError::Lagged(missed)) => {
                warn!(missed, "missed market events; tracking the whole market again");
                track_all(&market, &store, &clock).await;
            }
            Err(RecvError::Closed) => {
                info!("the market events ended; the components are no longer followed");
                return;
            }
        }
    }
}

/// Makes the store track exactly the components the market holds.
async fn track_all(market: &MarketData, store: &HealthStore, clock: &MonitorClock) {
    let components: Vec<(ComponentId, String)> = {
        let view = market.read().await;
        view.component_topology()
            .into_keys()
            .filter_map(|id| {
                let protocol = view
                    .get_component(&id)?
                    .protocol_system
                    .clone();
                Some((id, protocol))
            })
            .collect()
    };
    let now = clock.now();
    let present: HashSet<&str> = components
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    store.retain(|id| present.contains(id));
    for (id, protocol) in &components {
        store.track(id, protocol, now);
    }
    info!(count = components.len(), "tracking the components of the market");
}

/// Makes the store track `added`, which the market holds.
async fn track_added(
    market: &MarketData,
    store: &HealthStore,
    clock: &MonitorClock,
    added: &[ComponentId],
) {
    let view = market.read().await;
    let now = clock.now();
    for id in added {
        if let Some(component) = view.get_component(id) {
            store.track(id, &component.protocol_system, now);
        }
    }
}

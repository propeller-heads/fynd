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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;
    use fynd_core::{
        algorithm::test_utils::{
            component_with_protocol, setup_market_unweighted, token, MockProtocolSim,
        },
        rustc_hash::FxHashMap,
        types::ComponentTopology,
    };
    use tokio::sync::broadcast;

    use super::*;
    use crate::monitor::component_health::HealthThresholds;

    const THRESHOLDS: HealthThresholds =
        HealthThresholds { broken_after: 2, inaccurate_after: 3, inaccuracy_bps: 100.0 };

    fn store() -> Arc<HealthStore> {
        Arc::new(HealthStore::new(THRESHOLDS, Default::default()))
    }

    fn ids(store: &HealthStore) -> Vec<String> {
        store
            .report(None, None)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    fn market_of(component_ids: &[&str]) -> MarketData {
        let (usdc, weth) = (token(1, "USDC"), token(2, "WETH"));
        let (market, _) = setup_market_unweighted(
            component_ids
                .iter()
                .map(|id| (*id, &usdc, &weth, Box::new(MockProtocolSim::new(1.0)) as _))
                .collect(),
        );
        market
    }

    #[tokio::test(start_paused = true)]
    async fn test_tracks_the_market_and_drops_components_it_no_longer_holds() {
        let market = market_of(&["0xa", "0xb"]);
        let store = store();
        store.track("0xgone", "uniswap_v2", Utc::now());
        let (sender, receiver) = broadcast::channel(8);
        tokio::spawn(keep_tracking(market, receiver, Arc::clone(&store), MonitorClock::new()));

        settle().await;
        drop(sender);

        assert_eq!(ids(&store), ["0xa", "0xb"]);
        assert_eq!(
            store
                .get("0xa")
                .unwrap()
                .protocol_system,
            "uniswap_v2"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_follows_added_and_removed_components() {
        let market = market_of(&["0xa", "0xb"]);
        let store = store();
        let (sender, receiver) = broadcast::channel(8);
        tokio::spawn(keep_tracking(
            market.clone(),
            receiver,
            Arc::clone(&store),
            MonitorClock::new(),
        ));
        settle().await;
        let tokens = [token(1, "USDC"), token(2, "WETH")];
        market
            .write()
            .await
            .upsert_components([component_with_protocol("0xc", "uniswap_v3", &tokens)]);

        sender
            .send(MarketEvent::MarketUpdated {
                added_components: FxHashMap::from_iter([(
                    "0xc".to_string(),
                    ComponentTopology::from(vec![
                        tokens[0].address.clone(),
                        tokens[1].address.clone(),
                    ]),
                )]),
                removed_components: vec!["0xa".to_string()],
                updated_components: vec![],
            })
            .unwrap();
        settle().await;

        assert_eq!(ids(&store), ["0xb", "0xc"]);
        assert_eq!(
            store
                .get("0xc")
                .unwrap()
                .protocol_system,
            "uniswap_v3"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_reads_the_whole_market_again_after_missed_events() {
        let market = market_of(&["0xa"]);
        let store = store();
        let (sender, receiver) = broadcast::channel(1);
        let tokens = [token(1, "USDC"), token(2, "WETH")];
        market
            .write()
            .await
            .upsert_components([component_with_protocol("0xb", "uniswap_v2", &tokens)]);
        for _ in 0..3 {
            sender
                .send(MarketEvent::MarketUpdated {
                    added_components: FxHashMap::default(),
                    removed_components: vec![],
                    updated_components: vec![],
                })
                .unwrap();
        }
        tokio::spawn(keep_tracking(market, receiver, Arc::clone(&store), MonitorClock::new()));

        settle().await;

        assert_eq!(ids(&store), ["0xa", "0xb"]);
    }
}

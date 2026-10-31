//! The poller that keeps a pod's excluded pools in step with a `fynd-nexus` service.

use std::time::Duration;

use chrono::{DateTime, Utc};
use metrics::gauge;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::time::Instant;
use tracing::{info, warn};

use super::{excluded_path, ExcludedComponent, ExcludedComponents, ExclusionCause};
use crate::{
    feed::market_data::MarketData,
    nexus::poller::{PollTimings, Polled, StreamApplier, StreamPoller},
    ComponentId,
};

/// How long a pod keeps excluding pools without hearing from the service, by default.
///
/// The service answers each poll, also when nothing changed, so a pod that has heard nothing for
/// this long no longer knows whether the pools it excludes are still unhealthy.
pub const DEFAULT_MAX_UNCONFIRMED_AGE: Duration = Duration::from_secs(600);

/// Name of the stream in the poll metrics and logs.
const STREAM: &str = "excluded_components";

/// Reads the excluded components of the service. A withdrawal excludes nothing.
struct ExcludedApplier;

impl StreamApplier for ExcludedApplier {
    type Update = ExcludedComponents;

    fn stream(&self) -> &'static str {
        STREAM
    }

    fn snapshot(
        &mut self,
        data: serde_json::Value,
        _as_of: DateTime<Utc>,
    ) -> Result<Self::Update, serde_json::Error> {
        serde_json::from_value(data)
    }

    fn withdrawal(&mut self) -> Self::Update {
        ExcludedComponents::default()
    }
}

/// Polls the components a `fynd-nexus` service found unhealthy, and sets them as the excluded
/// pools of the market.
///
/// It fails open. The market excludes nothing until the service reports a set, and again when the
/// service withdraws it, answers that it has none, or has not answered a poll for the maximum
/// unconfirmed age. A failed poll in between keeps the last set.
///
/// The snapshot age limit of the poller is off: a snapshot is as old as the last change of the
/// set, which can be days. The service withdraws the stream itself when its probing stops.
pub struct UnhealthyPoolsPoller {
    poller: StreamPoller<ExcludedApplier>,
    market: MarketData,
    max_unconfirmed_age: Duration,
    /// The components the market currently excludes because of this poller.
    excluded: FxHashMap<ComponentId, ExcludedComponent>,
}

impl UnhealthyPoolsPoller {
    /// A poller of the service at `base_url`, which must be a valid HTTP URL.
    pub(crate) fn new(
        base_url: &str,
        http: reqwest::Client,
        market: MarketData,
        max_unconfirmed_age: Duration,
    ) -> Self {
        Self::with_timings(base_url, http, market, max_unconfirmed_age, PollTimings::new(None))
    }

    fn with_timings(
        base_url: &str,
        http: reqwest::Client,
        market: MarketData,
        max_unconfirmed_age: Duration,
        timings: PollTimings,
    ) -> Self {
        let url = format!("{base_url}{}", excluded_path());
        Self {
            poller: StreamPoller::new(url, http, ExcludedApplier, timings),
            market,
            max_unconfirmed_age,
            excluded: FxHashMap::default(),
        }
    }

    /// Polls the service for as long as the task runs.
    pub async fn run(mut self) {
        let mut last_heard = Instant::now();
        loop {
            self.poll_and_apply(&mut last_heard)
                .await;
        }
    }

    /// Makes one poll and applies what it came to. `last_heard` is when the service last
    /// answered.
    async fn poll_and_apply(&mut self, last_heard: &mut Instant) {
        match self.poller.poll_once().await {
            Polled::Update(update) => {
                *last_heard = Instant::now();
                self.apply(update).await;
            }
            Polled::Unchanged => *last_heard = Instant::now(),
            Polled::Empty => {
                *last_heard = Instant::now();
                self.apply(ExcludedComponents::default())
                    .await;
            }
            Polled::Failed => {
                if !self.excluded.is_empty() && last_heard.elapsed() > self.max_unconfirmed_age {
                    warn!(
                        unheard_for = ?last_heard.elapsed(),
                        "Fynd Nexus has not answered for too long; routing through its excluded \
                         pools again"
                    );
                    self.apply(ExcludedComponents::default())
                        .await;
                }
            }
        }
    }

    /// Makes `update` the excluded pools of the market, and logs which pools it adds and frees.
    async fn apply(&mut self, update: ExcludedComponents) {
        let mut next = FxHashMap::default();
        for mut component in update.components {
            component.id = component.id.to_lowercase();
            next.insert(component.id.clone(), component);
        }
        let ids: FxHashSet<ComponentId> = next.keys().cloned().collect();
        let changed = self
            .market
            .write()
            .await
            .set_unhealthy_pools(ids);
        if changed {
            log_changes(&self.excluded, &next);
        }
        self.excluded = next;
        for cause in [ExclusionCause::Broken, ExclusionCause::Inaccurate] {
            let count = self
                .excluded
                .values()
                .filter(|component| component.cause == cause)
                .count();
            gauge!("nexus_excluded_pools", "cause" => cause.name()).set(count as f64);
        }
    }
}

/// Logs each pool that `next` excludes and `current` does not, and each pool it frees.
fn log_changes(
    current: &FxHashMap<ComponentId, ExcludedComponent>,
    next: &FxHashMap<ComponentId, ExcludedComponent>,
) {
    for (id, component) in next {
        if !current.contains_key(id) {
            info!(
                "excluding pool {id} ({}, {}): {}",
                component.protocol_system,
                component.cause.name(),
                component.reason
            );
        }
    }
    for id in current.keys() {
        if !next.contains_key(id) {
            info!("routing through pool {id} again");
        }
    }
}

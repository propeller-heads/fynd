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

#[cfg(test)]
mod tests {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::nexus::{MessageBody, NexusMessage};

    const EPOCH: u64 = 1_761_550_000_000;

    fn timings() -> PollTimings {
        PollTimings {
            poll_interval: Duration::from_millis(2),
            request_timeout: Duration::from_secs(5),
            max_backoff: Duration::from_millis(4),
            max_snapshot_age: None,
        }
    }

    fn component(id: &str, cause: ExclusionCause) -> ExcludedComponent {
        ExcludedComponent {
            id: id.to_string(),
            protocol_system: "uniswap_v2".to_string(),
            cause,
            since: "2026-10-27T09:00:00Z".parse().unwrap(),
            reason: "reverted".to_string(),
        }
    }

    fn snapshot(seq: u64, components: Vec<ExcludedComponent>) -> ResponseTemplate {
        // `as_of` is a day old: the age of a snapshot must not matter to the poller.
        let as_of = Utc::now() - chrono::Duration::days(1);
        let data = serde_json::to_value(ExcludedComponents { components }).unwrap();
        let message = NexusMessage::new(EPOCH, seq, MessageBody::Snapshot { as_of, data });
        ResponseTemplate::new(200).set_body_string(message.encode().unwrap())
    }

    fn withdrawn(seq: u64) -> ResponseTemplate {
        let message = NexusMessage::<serde_json::Value>::new(EPOCH, seq, MessageBody::Withdrawn);
        ResponseTemplate::new(200).set_body_string(message.encode().unwrap())
    }

    async fn serve(response: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        respond(&server, response).await;
        server
    }

    async fn respond(server: &MockServer, response: ResponseTemplate) {
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/v1/components/excluded"))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn poller_of(server: &MockServer, max_unconfirmed_age: Duration) -> UnhealthyPoolsPoller {
        UnhealthyPoolsPoller::with_timings(
            &server.uri(),
            reqwest::Client::new(),
            MarketData::new_shared(),
            max_unconfirmed_age,
            timings(),
        )
    }

    async fn excluded_in_market(poller: &UnhealthyPoolsPoller) -> Vec<String> {
        let mut ids: Vec<_> = poller
            .market
            .read()
            .await
            .base_market_state()
            .unhealthy_pools()
            .iter()
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    #[tokio::test]
    async fn test_snapshot_sets_the_unhealthy_pools_in_lowercase() {
        let server = serve(snapshot(
            1,
            vec![
                component("0xABCDEF", ExclusionCause::Broken),
                component("0x123456", ExclusionCause::Inaccurate),
            ],
        ))
        .await;
        let mut poller = poller_of(&server, DEFAULT_MAX_UNCONFIRMED_AGE);

        poller
            .poll_and_apply(&mut Instant::now())
            .await;

        assert_eq!(excluded_in_market(&poller).await, ["0x123456", "0xabcdef"]);
    }

    #[tokio::test]
    async fn test_a_pool_that_leaves_the_set_is_routable_again() {
        let server = serve(snapshot(1, vec![component("0xaa", ExclusionCause::Broken)])).await;
        let mut poller = poller_of(&server, DEFAULT_MAX_UNCONFIRMED_AGE);
        poller
            .poll_and_apply(&mut Instant::now())
            .await;
        respond(&server, snapshot(2, vec![component("0xbb", ExclusionCause::Broken)])).await;

        poller
            .poll_and_apply(&mut Instant::now())
            .await;

        assert_eq!(excluded_in_market(&poller).await, ["0xbb"]);
    }

    #[tokio::test]
    async fn test_a_withdrawal_frees_every_pool() {
        let server = serve(snapshot(1, vec![component("0xaa", ExclusionCause::Broken)])).await;
        let mut poller = poller_of(&server, DEFAULT_MAX_UNCONFIRMED_AGE);
        poller
            .poll_and_apply(&mut Instant::now())
            .await;
        respond(&server, withdrawn(2)).await;

        poller
            .poll_and_apply(&mut Instant::now())
            .await;

        assert!(excluded_in_market(&poller)
            .await
            .is_empty());
    }

    /// The service has no state after a restart, until its first probes finish.
    #[tokio::test]
    async fn test_an_empty_answer_frees_every_pool() {
        let server = serve(snapshot(1, vec![component("0xaa", ExclusionCause::Broken)])).await;
        let mut poller = poller_of(&server, DEFAULT_MAX_UNCONFIRMED_AGE);
        poller
            .poll_and_apply(&mut Instant::now())
            .await;
        respond(&server, ResponseTemplate::new(204)).await;

        poller
            .poll_and_apply(&mut Instant::now())
            .await;

        assert!(excluded_in_market(&poller)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn test_failed_polls_keep_the_last_set_until_the_service_is_unheard_for_too_long() {
        let server = serve(snapshot(1, vec![component("0xaa", ExclusionCause::Broken)])).await;
        let mut poller = poller_of(&server, Duration::from_millis(50));
        let mut last_heard = Instant::now();
        poller
            .poll_and_apply(&mut last_heard)
            .await;
        respond(&server, ResponseTemplate::new(500)).await;

        poller
            .poll_and_apply(&mut last_heard)
            .await;
        assert_eq!(excluded_in_market(&poller).await, ["0xaa"], "a short silence keeps the set");

        tokio::time::sleep(Duration::from_millis(80)).await;
        poller
            .poll_and_apply(&mut last_heard)
            .await;
        assert!(
            excluded_in_market(&poller)
                .await
                .is_empty(),
            "a long silence frees the pools"
        );
    }

    #[tokio::test]
    async fn test_an_unchanged_answer_keeps_the_set_however_old_the_snapshot() {
        let server = serve(snapshot(1, vec![component("0xaa", ExclusionCause::Broken)])).await;
        let mut poller = poller_of(&server, Duration::from_millis(30));
        let mut last_heard = Instant::now();
        poller
            .poll_and_apply(&mut last_heard)
            .await;
        respond(&server, ResponseTemplate::new(304)).await;

        tokio::time::sleep(Duration::from_millis(50)).await;
        poller
            .poll_and_apply(&mut last_heard)
            .await;

        assert_eq!(excluded_in_market(&poller).await, ["0xaa"]);
    }

    #[tokio::test]
    async fn test_gauge_counts_pools_per_cause() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let server = serve(snapshot(
            1,
            vec![
                component("0xaa", ExclusionCause::Broken),
                component("0xbb", ExclusionCause::Broken),
                component("0xcc", ExclusionCause::Inaccurate),
            ],
        ))
        .await;
        let mut poller = poller_of(&server, DEFAULT_MAX_UNCONFIRMED_AGE);

        poller
            .poll_and_apply(&mut Instant::now())
            .await;

        let mut counts = Vec::new();
        for (key, _, _, value) in snapshotter.snapshot().into_vec() {
            if let (true, DebugValue::Gauge(count)) =
                (key.key().name() == "nexus_excluded_pools", value)
            {
                let cause = key
                    .key()
                    .labels()
                    .find(|label| label.key() == "cause")
                    .map(|label| label.value().to_string());
                counts.push((cause.unwrap(), count.into_inner()));
            }
        }
        counts.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(counts, [("broken".to_string(), 2.0), ("inaccurate".to_string(), 1.0)]);
    }
}

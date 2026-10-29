//! The post-processors of one worker, run in config order.

use std::time::{Duration, Instant};

use num_bigint::BigInt;
use num_traits::ToPrimitive;
use tracing::warn;

use crate::{
    feed::{events::MarketEvent, market_data::MarketData},
    post_process::{PostProcessRequest, RoutePostProcessor},
    types::RouteResult,
};

/// The final replacement route and the names of the post-processors that replaced it.
pub(crate) struct ProcessedRoute {
    /// The route that the last replacing post-processor returned.
    pub(crate) route: RouteResult,
    /// The names of the post-processors that returned a route, in config order.
    pub(crate) post_processed_by: Vec<String>,
}

/// What happened when a post-processor ran on a route.
#[derive(Clone, Copy)]
enum PostProcessOutcome {
    /// The post-processor returned a replacement route.
    Replaced,
    /// The post-processor returned no replacement route.
    Kept,
    /// The post-processor did not finish before the post-process deadline.
    TimedOut,
    /// The post-process deadline had passed before the post-processor could start.
    Skipped,
}

impl PostProcessOutcome {
    fn label(self) -> &'static str {
        match self {
            Self::Replaced => "replaced",
            Self::Kept => "kept",
            Self::TimedOut => "timed_out",
            Self::Skipped => "skipped",
        }
    }
}

/// The named post-processor instances of one worker, in config order.
#[derive(Default)]
pub(crate) struct PostProcessors {
    worker_pool: String,
    instances: Vec<(String, Box<dyn RoutePostProcessor>)>,
}

impl PostProcessors {
    /// The instances of the worker pool `worker_pool`, each with its registered name.
    pub(crate) fn new(
        worker_pool: String,
        instances: Vec<(String, Box<dyn RoutePostProcessor>)>,
    ) -> Self {
        Self { worker_pool, instances }
    }

    /// True when the worker pool sets no post-processors.
    pub(crate) fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    /// Calls `initialize` on each post-processor.
    pub(crate) async fn initialize(&mut self, market: &MarketData) {
        for (_, post_processor) in &mut self.instances {
            post_processor.initialize(market).await;
        }
    }

    /// Sends `event` to each post-processor. An error is logged and the next one still gets the
    /// event.
    pub(crate) async fn handle_event(&mut self, event: &MarketEvent) {
        for (post_processor_name, post_processor) in &mut self.instances {
            if let Err(e) = post_processor.handle_event(event).await {
                warn!(post_processor = post_processor_name, "Error handling market event: {:?}", e);
            }
        }
    }

    /// Runs the post-processors on `route` in config order. Each one gets the last route a
    /// post-processor before it returned, or `route` when none did. Returns the final replacement
    /// route and the names of all post-processors that replaced a route, or `None` if none did.
    ///
    /// At [`PostProcessRequest::deadline`], the worker stops waiting for the post-processor that
    /// is running and keeps the route it had before that call. The post-processors after it do
    /// not run. Each post-processor records one [`PostProcessOutcome`].
    pub(crate) async fn run(
        &self,
        request: &PostProcessRequest<'_>,
        route: &RouteResult,
    ) -> Option<ProcessedRoute> {
        let deadline = request.deadline();
        let mut latest: Option<RouteResult> = None;
        let mut post_processed_by: Vec<String> = Vec::new();
        for (index, (post_processor_name, post_processor)) in self.instances.iter().enumerate() {
            let started = Instant::now();
            if started >= deadline {
                warn!(
                    worker_pool = %self.worker_pool,
                    post_processor = post_processor_name,
                    "post-process deadline passed; skipping the remaining post-processors"
                );
                for (skipped_name, _) in &self.instances[index..] {
                    record_post_processor_outcome(
                        &self.worker_pool,
                        skipped_name,
                        PostProcessOutcome::Skipped,
                    );
                }
                break;
            }
            let current = latest.as_ref().unwrap_or(route);
            let processing = post_processor.process(request, current);
            let outcome = match tokio::time::timeout_at(deadline.into(), processing).await {
                Ok(Some(replacement)) => {
                    record_post_processor_gain(
                        &self.worker_pool,
                        post_processor_name,
                        current,
                        &replacement,
                    );
                    post_processed_by.push(post_processor_name.clone());
                    latest = Some(replacement);
                    PostProcessOutcome::Replaced
                }
                Ok(None) => PostProcessOutcome::Kept,
                Err(_) => {
                    warn!(
                        worker_pool = %self.worker_pool,
                        post_processor = post_processor_name,
                        "post-processor passed the post-process deadline; keeping its input route"
                    );
                    PostProcessOutcome::TimedOut
                }
            };
            record_post_processor_outcome(&self.worker_pool, post_processor_name, outcome);
            record_post_processor_duration(
                &self.worker_pool,
                post_processor_name,
                started.elapsed(),
            );
        }
        latest.map(|route| ProcessedRoute { route, post_processed_by })
    }
}

/// Counts one outcome of a post-processor on a route.
fn record_post_processor_outcome(
    worker_pool: &str,
    post_processor_name: &str,
    outcome: PostProcessOutcome,
) {
    metrics::counter!(
        "worker_pool_post_processor_outcomes_total",
        "pool" => worker_pool.to_string(),
        "post_processor" => post_processor_name.to_string(),
        "outcome" => outcome.label()
    )
    .increment(1);
}

/// Records how much `replacement` gains over `input`, in basis points of the input net amount out.
///
/// Records negative gains unchanged. Nothing is recorded when the input net amount out is not
/// positive. A replacement that later fails the worker checks still counts here;
/// `worker_pool_post_processed_route_rejected_total` counts those.
fn record_post_processor_gain(
    worker_pool: &str,
    post_processor_name: &str,
    input: &RouteResult,
    replacement: &RouteResult,
) {
    let input_net = input.net_amount_out();
    if *input_net <= BigInt::ZERO {
        return;
    }
    let gain = replacement.net_amount_out() - input_net;
    let (Some(gain), Some(input_net)) = (gain.to_f64(), input_net.to_f64()) else {
        return;
    };
    let gain_bps = gain * 10_000.0 / input_net;
    metrics::histogram!(
        "worker_pool_post_processor_gain_bps",
        "pool" => worker_pool.to_string(),
        "post_processor" => post_processor_name.to_string()
    )
    .record(gain_bps);
}

/// Records the wall time of one post-processor run on a route.
fn record_post_processor_duration(
    worker_pool: &str,
    post_processor_name: &str,
    duration: Duration,
) {
    metrics::histogram!(
        "worker_pool_post_processor_duration_seconds",
        "pool" => worker_pool.to_string(),
        "post_processor" => post_processor_name.to_string()
    )
    .record(duration.as_secs_f64());
}

#[cfg(test)]
mod tests {
    use std::sync::{atomic::Ordering, Arc};

    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use num_bigint::BigInt;
    use rstest::rstest;
    use rustc_hash::FxHashMap;

    use super::*;
    use crate::{
        algorithm::test_utils::{order, setup_market_weighted, token},
        derived::{DerivedData, SharedDerivedDataRef},
        post_process::test_utils::{route_result, with_net_amount_out, MockPostProcessor},
        types::{Order, OrderSide, ResolvedRouteConstraints},
    };

    const POOL: &str = "test_pool";

    /// What a request borrows.
    struct Fixture {
        order: Order,
        market: MarketData,
        derived: SharedDerivedDataRef,
        exclusions: ResolvedRouteConstraints,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                order: order(&token(0x01, "A"), &token(0x02, "B"), 100, OrderSide::Sell),
                market: setup_market_weighted(Vec::new()).0,
                derived: DerivedData::new_shared(),
                exclusions: ResolvedRouteConstraints::default(),
            }
        }

        /// A request with a deadline that no test reaches.
        fn request(&self) -> PostProcessRequest<'_> {
            self.request_until(Instant::now() + Duration::from_secs(60))
        }

        fn request_until(&self, deadline: Instant) -> PostProcessRequest<'_> {
            PostProcessRequest::new(
                &self.order,
                &self.market,
                &self.derived,
                &self.exclusions,
                deadline,
            )
        }
    }

    fn build_post_processors(post_processors: Vec<(&str, MockPostProcessor)>) -> PostProcessors {
        let instances = post_processors
            .into_iter()
            .map(|(post_processor_name, post_processor)| {
                (
                    post_processor_name.to_string(),
                    Box::new(post_processor) as Box<dyn RoutePostProcessor>,
                )
            })
            .collect();
        PostProcessors::new(POOL.to_string(), instances)
    }

    fn market_event() -> MarketEvent {
        MarketEvent::MarketUpdated {
            added_components: FxHashMap::default(),
            removed_components: Vec::new(),
            updated_components: Vec::new(),
        }
    }

    /// The second post-processor adds one to what the first set, so only config order gives 101.
    #[tokio::test]
    async fn test_run_config_order() {
        let set = MockPostProcessor::new(|route| with_net_amount_out(route, 100));
        let add_one = MockPostProcessor::new(|route| {
            let net_amount_out = route.net_amount_out() + 1;
            RouteResult::new(route.route().clone(), net_amount_out, route.gas_price().clone())
        });
        let post_processors = build_post_processors(vec![("set", set), ("add_one", add_one)]);
        let fixture = Fixture::new();

        let route = post_processors
            .run(&fixture.request(), &route_result(7))
            .await
            .expect("both post-processors replace the route");

        assert_eq!(route.route.net_amount_out(), &BigInt::from(101));
    }

    #[tokio::test]
    async fn test_run_none_keeps_previous_route() {
        let set = MockPostProcessor::new(|route| with_net_amount_out(route, 100));
        let post_processors =
            build_post_processors(vec![("set", set), ("keep", MockPostProcessor::unchanged())]);
        let fixture = Fixture::new();

        let route = post_processors
            .run(&fixture.request(), &route_result(7))
            .await
            .expect("the first post-processor replaces the route");

        assert_eq!(route.route.net_amount_out(), &BigInt::from(100));
    }

    #[tokio::test]
    async fn test_run_unchanged_post_processor() {
        let post_processors = build_post_processors(vec![("keep", MockPostProcessor::unchanged())]);
        let fixture = Fixture::new();

        let route = post_processors
            .run(&fixture.request(), &route_result(7))
            .await;

        assert!(route.is_none());
    }

    #[tokio::test]
    async fn test_run_slow_post_processor_keeps_previous_route() {
        let slow = MockPostProcessor::new(|route| with_net_amount_out(route, 1000))
            .with_delay(Duration::from_secs(30));
        let post_processors = build_post_processors(vec![("slow", slow)]);
        let fixture = Fixture::new();
        let request = fixture.request_until(Instant::now() + Duration::from_millis(50));

        let route = post_processors
            .run(&request, &route_result(7))
            .await;

        assert!(route.is_none());
    }

    #[tokio::test]
    async fn test_run_timeout_keeps_earlier_replacement() {
        let set = MockPostProcessor::new(|route| with_net_amount_out(route, 100));
        let slow = MockPostProcessor::new(|route| with_net_amount_out(route, 1000))
            .with_delay(Duration::from_secs(30));
        let post_processors = build_post_processors(vec![("set", set), ("slow", slow)]);
        let fixture = Fixture::new();
        let request = fixture.request_until(Instant::now() + Duration::from_millis(50));

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let processed = post_processors
            .run(&request, &route_result(7))
            .await
            .expect("the first post-processor replaces the route");
        let recorded = crate::tests::metrics::recorded_metrics(&snapshotter);

        assert_eq!(processed.route.net_amount_out(), &BigInt::from(100));
        assert_eq!(processed.post_processed_by, ["set".to_string()]);
        assert_eq!(
            outcomes(&recorded),
            vec![outcome("set", "replaced"), outcome("slow", "timed_out")]
        );
    }

    #[tokio::test]
    async fn test_run_passed_deadline_skips_post_processors() {
        let set = MockPostProcessor::new(|route| with_net_amount_out(route, 1000));
        let post_processors = build_post_processors(vec![("set", set)]);
        let fixture = Fixture::new();
        let request = fixture.request_until(Instant::now() - Duration::from_millis(1));

        let route = post_processors
            .run(&request, &route_result(7))
            .await;

        assert!(route.is_none());
    }

    #[tokio::test]
    async fn test_run_records_duration_per_post_processor() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let post_processors =
            build_post_processors(vec![("first", MockPostProcessor::unchanged())]);
        let fixture = Fixture::new();

        post_processors
            .run(&fixture.request(), &route_result(7))
            .await;

        let recorded = crate::tests::metrics::recorded_metrics(&snapshotter);
        let labels: Vec<_> = recorded
            .iter()
            .filter(|(name, _, _)| name == "worker_pool_post_processor_duration_seconds")
            .map(|(_, labels, _)| labels.clone())
            .collect();
        assert_eq!(
            labels,
            vec![vec!["pool=test_pool".to_string(), "post_processor=first".to_string()]]
        );
    }

    /// Runs `post_processors` on `input` and returns the recorded metrics.
    async fn recorded_run(
        post_processors: &PostProcessors,
        request: &PostProcessRequest<'_>,
        input: RouteResult,
    ) -> Vec<crate::tests::metrics::Recorded> {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        post_processors
            .run(request, &input)
            .await;
        crate::tests::metrics::recorded_metrics(&snapshotter)
    }

    fn outcomes(recorded: &[crate::tests::metrics::Recorded]) -> Vec<(String, String, u64)> {
        let mut outcomes = Vec::new();
        for (name, labels, value) in recorded {
            if name != "worker_pool_post_processor_outcomes_total" {
                continue;
            }
            let DebugValue::Counter(count) = value else {
                panic!("expected counter, got {value:?}");
            };
            outcomes.push((
                find_label(labels, "post_processor="),
                find_label(labels, "outcome="),
                *count,
            ));
        }
        outcomes.sort();
        outcomes
    }

    fn find_label(labels: &[String], prefix: &str) -> String {
        labels
            .iter()
            .find(|label| label.starts_with(prefix))
            .unwrap_or_else(|| panic!("no label starting with {prefix} in {labels:?}"))
            .clone()
    }

    fn outcome(post_processor: &str, outcome: &str) -> (String, String, u64) {
        (format!("post_processor={post_processor}"), format!("outcome={outcome}"), 1)
    }

    #[tokio::test]
    async fn test_run_outcomes_changing_and_unchanged_processors() {
        let set = MockPostProcessor::new(|route| with_net_amount_out(route, 100));
        let post_processors =
            build_post_processors(vec![("set", set), ("keep", MockPostProcessor::unchanged())]);
        let fixture = Fixture::new();

        let recorded = recorded_run(&post_processors, &fixture.request(), route_result(7)).await;

        assert_eq!(outcomes(&recorded), vec![outcome("keep", "kept"), outcome("set", "replaced")]);
    }

    #[tokio::test]
    async fn test_run_outcome_slow_post_processor() {
        let slow = MockPostProcessor::new(|route| with_net_amount_out(route, 1000))
            .with_delay(Duration::from_secs(30));
        let post_processors = build_post_processors(vec![("slow", slow)]);
        let fixture = Fixture::new();
        let request = fixture.request_until(Instant::now() + Duration::from_millis(50));

        let recorded = recorded_run(&post_processors, &request, route_result(7)).await;

        assert_eq!(outcomes(&recorded), vec![outcome("slow", "timed_out")]);
    }

    #[tokio::test]
    async fn test_run_outcomes_expired_deadline() {
        let post_processors = build_post_processors(vec![
            ("first", MockPostProcessor::unchanged()),
            ("second", MockPostProcessor::unchanged()),
        ]);
        let fixture = Fixture::new();
        let request = fixture.request_until(Instant::now() - Duration::from_millis(1));

        let recorded = recorded_run(&post_processors, &request, route_result(7)).await;

        assert_eq!(
            outcomes(&recorded),
            vec![outcome("first", "skipped"), outcome("second", "skipped")]
        );
    }

    #[tokio::test]
    async fn test_run_post_processed_by_config_order() {
        let post_processors = build_post_processors(vec![
            ("first", MockPostProcessor::new(|route| with_net_amount_out(route, 10))),
            ("keep", MockPostProcessor::unchanged()),
            ("last", MockPostProcessor::new(|route| with_net_amount_out(route, 20))),
        ]);
        let fixture = Fixture::new();

        let processed = post_processors
            .run(&fixture.request(), &route_result(7))
            .await
            .expect("two post-processors replace the route");

        assert_eq!(processed.post_processed_by, ["first".to_string(), "last".to_string()]);
    }

    #[rstest]
    #[case::gain(100, 125, Some(2_500.0))]
    #[case::sub_bps_gain(200_000, 200_010, Some(0.5))]
    #[case::loss(100, 90, Some(-1_000.0))]
    #[case::zero_input(0, 100, None)]
    #[case::negative_input(-5, 100, None)]
    #[tokio::test]
    async fn test_run_gain_bps(
        #[case] input: i64,
        #[case] output: i64,
        #[case] expected: Option<f64>,
    ) {
        let set = MockPostProcessor::new(move |route| with_net_amount_out(route, output));
        let post_processors = build_post_processors(vec![("set", set)]);
        let fixture = Fixture::new();

        let recorded =
            recorded_run(&post_processors, &fixture.request(), route_result(input)).await;

        let gains: Vec<_> = recorded
            .iter()
            .filter(|(name, _, _)| name == "worker_pool_post_processor_gain_bps")
            .collect();
        match expected {
            Some(bps) => {
                assert_eq!(gains.len(), 1);
                assert_eq!(
                    gains[0].1,
                    vec!["pool=test_pool".to_string(), "post_processor=set".to_string()]
                );
                let DebugValue::Histogram(values) = &gains[0].2 else {
                    panic!("expected histogram, got {:?}", gains[0].2);
                };
                let recorded_values: Vec<f64> = values
                    .iter()
                    .map(|v| v.into_inner())
                    .collect();
                assert_eq!(recorded_values, vec![bps]);
            }
            None => assert!(gains.is_empty()),
        }
    }

    #[tokio::test]
    async fn test_handle_event_reaches_every_post_processor() {
        let first = MockPostProcessor::unchanged();
        let second = MockPostProcessor::unchanged();
        let (first_events, second_events) =
            (Arc::clone(&first.event_count), Arc::clone(&second.event_count));
        let mut post_processors = build_post_processors(vec![("first", first), ("second", second)]);

        post_processors
            .handle_event(&market_event())
            .await;

        assert_eq!(first_events.load(Ordering::SeqCst), 1);
        assert_eq!(second_events.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_handle_event_error_does_not_stop_next() {
        let failing = MockPostProcessor::unchanged().failing_events();
        let next = MockPostProcessor::unchanged();
        let next_events = Arc::clone(&next.event_count);
        let mut post_processors = build_post_processors(vec![("failing", failing), ("next", next)]);

        post_processors
            .handle_event(&market_event())
            .await;

        assert_eq!(next_events.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_initialize_reaches_every_post_processor() {
        let first = MockPostProcessor::unchanged();
        let second = MockPostProcessor::unchanged();
        let (first_calls, second_calls) =
            (Arc::clone(&first.initialize_calls), Arc::clone(&second.initialize_calls));
        let mut post_processors = build_post_processors(vec![("first", first), ("second", second)]);
        let fixture = Fixture::new();

        post_processors
            .initialize(&fixture.market)
            .await;

        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    }
}

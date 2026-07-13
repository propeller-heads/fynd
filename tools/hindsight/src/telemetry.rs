//! Prometheus metrics for re-solve comparisons, plus the `/metrics` HTTP exporter.
//!
//! Mirrors the exporter pattern used by the `fynd` binary: install a global Prometheus recorder
//! and serve its rendered output on a dedicated port via Actix.

use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use metrics::{counter, describe_counter, describe_histogram, histogram, Unit};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tracing::{error, info, warn};

use crate::{
    decoder::Registry,
    resolve::{Outcome, RangeComparison, StateResult, Verdict},
    usd,
};

const TRADES_TOTAL: &str = "hindsight_trades_total";
const SAVINGS_BPS: &str = "hindsight_savings_bps";
const SAVINGS_USD: &str = "hindsight_savings_usd";
const IMPROVEMENT_USD: &str = "hindsight_improvement_usd";
const VOLUME_USD: &str = "hindsight_volume_usd";
const BLOCK_SECONDS: &str = "hindsight_block_processing_seconds";
const SKIPPED_BLOCKS: &str = "hindsight_skipped_blocks_total";
const FEED_REBUILDS: &str = "hindsight_feed_rebuilds_total";

/// Absolute USD savings beyond which a comparison is logged with full per-trade context, so large
/// outliers can be traced and classified (a genuinely large trade vs a token-mispricing artifact
/// from the ETH-anchored valuation).
const USD_OUTLIER_THRESHOLD: f64 = 1_000.0;

/// Whether a USD savings value is large enough to log for inspection.
fn is_usd_outlier(usd: f64) -> bool {
    usd.abs() >= USD_OUTLIER_THRESHOLD
}

/// Metric label for a range's venue: registered venues (the registry's `[venues.*]` sections
/// — the integrator front-ends the comparison is pitched at) keep their name; everything else —
/// direct router entries, bots, unregistered addresses — collapses to "other". Keeps the
/// dashboard's venue filter to the registered names plus "other"; full venue detail stays in
/// the JSONL records.
fn venue_label<'a>(venue: &'a str, registry: &Registry) -> &'a str {
    if registry.venue(venue).is_some() {
        venue
    } else {
        "other"
    }
}

/// Metric label for a range's settling solver: registered solver names pass through, everything
/// else collapses to "unknown". Attribution can also produce raw addresses (largest-call guess),
/// venue names (fallback tier), and calldata-declared names from a venue's own vocabulary
/// (`MetaMask` aggregator ids like "pancakeSwapRouterFeeDynamic") — none belong in the bounded
/// metric vocabulary. The original label stays in the JSONL records, where it serves as the
/// registry-expansion worklist.
fn solver_label<'a>(solver: &'a str, registry: &Registry) -> &'a str {
    if registry.is_solver_name(solver) {
        solver
    } else {
        "unknown"
    }
}

/// The bounded label set shared by every per-trade metric, computed once per range.
struct MetricLabels<'a> {
    venue: &'a str,
    solver: &'a str,
    chain: &'a str,
}

/// Metric label for a trade's headline verdict.
pub(crate) fn outcome_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Win => "win",
        Verdict::Loss => "loss",
        Verdict::CoverageMiss => "coverage_miss",
        Verdict::Unsolvable => "unsolvable",
        Verdict::Sandwiched => "sandwiched",
    }
}

/// Register metric descriptions with the active recorder.
pub(crate) fn describe() {
    describe_counter!(
        TRADES_TOTAL,
        "Re-solved trades, labeled by venue / solver / chain / outcome / state (top|back). \
         Per-pair detail lives in the JSONL comparison output; a token-pair label here is unbounded \
         on mainnet and would explode Prometheus series cardinality over a long run."
    );
    describe_histogram!(
        SAVINGS_BPS,
        Unit::Count,
        "Gross bps delta of Fynd vs settled (positive = Fynd better), labeled by venue / solver / \
         chain / outcome / state (top|back)"
    );
    describe_histogram!(
        SAVINGS_USD,
        "Signed gross USD savings of Fynd vs settled (positive = Fynd better), for Fynd-priced \
         trades"
    );
    describe_histogram!(
        IMPROVEMENT_USD,
        "Gross USD uplift on trades Fynd would improve (losses excluded — a venue routes \
         elsewhere when Fynd is worse). Sum = value of adding Fynd; count = improving trades"
    );
    describe_histogram!(
        VOLUME_USD,
        "Observed settled trade volume in USD, labeled by venue / solver / outcome (headline \
         verdict). Valued from the output leg, falling back to the input leg when the output \
         token is unpriced"
    );
    describe_histogram!(BLOCK_SECONDS, Unit::Seconds, "Wall-clock time to process one block");
    describe_counter!(
        SKIPPED_BLOCKS,
        "Blocks skipped because the RPC could not provide receipts (e.g. it lagged the tycho \
         stream past the retry budget, or the block genuinely failed to decode)"
    );
    describe_counter!(
        FEED_REBUILDS,
        "Times the monitor declared its session unhealthy (feed died, or the monitor fell too \
         far behind chain head) and rebuilt the solver to resubscribe"
    );
}

/// Record a two-state range: the top-of-block (N-1) and back-of-block (N) outcomes, each tagged
/// with a `state` label ("top"/"back"). `prices_top`/`prices_back` are the solver's token-price
/// snapshots at each state, used to value savings in USD; an empty map disables USD for that state.
pub(crate) fn record_range(
    range: &RangeComparison,
    chain: &str,
    prices_top: &usd::PriceMap,
    prices_back: &usd::PriceMap,
    registry: &Registry,
) {
    let labels = MetricLabels {
        venue: venue_label(&range.venue, registry),
        solver: solver_label(&range.solver, registry),
        chain,
    };

    // Observed volume is a per-trade quantity (the settled amount), not per-state, so record it
    // once — valued at the top-of-block snapshot to match the headline. When the output token is
    // unpriced, fall back to the input leg: for a swap the two legs are worth roughly the same,
    // and the output leg is unpriced exactly when the trade is unsolvable (a long-tail token
    // outside the solver's graph) — without the fallback, unsolvable volume would be
    // systematically undercounted.
    let volume = usd::value_usd(range.token_out, range.settled_amount_out, prices_top)
        .or_else(|| usd::value_usd(range.token_in, range.amount_in, prices_top));
    if let Some(volume) = volume {
        histogram!(
            VOLUME_USD,
            "venue" => labels.venue.to_string(),
            "solver" => labels.solver.to_string(),
            "chain" => labels.chain.to_string(),
            "outcome" => outcome_label(range.verdict).to_string(),
        )
        .record(volume);
    }

    let savings_top = record_state(range, &range.top, "top", &labels, prices_top);
    record_state(range, &range.back, "back", &labels, prices_back);

    // One structured line per priced comparison, on the headline basis (top-of-block, gross).
    // Loki ingests pod stdout, so this line feeds the dashboard's top-trades table; keep the
    // message and field names stable — the LogQL query extracts them by regexp. Zero means
    // "unpriced" (or, for quoted_usd, "the solver declared no quote").
    if let (Some(savings_usd), Outcome::Solved(solved)) = (savings_top, &range.top.outcome) {
        let priced = |amount| usd::value_usd(range.token_out, amount, prices_top).unwrap_or(0.0);
        info!(
            tx = %range.tx_hash,
            block = range.block_number,
            venue = %range.venue,
            solver = %range.solver,
            token_in = %range.token_in,
            token_out = %range.token_out,
            verdict = %outcome_label(range.verdict),
            volume_usd = volume.unwrap_or(0.0),
            settled_usd = priced(range.settled_amount_out),
            fynd_usd = priced(solved.amount_out),
            quoted_usd = range.quote.as_ref().map_or(0.0, |quote| priced(quote.amount_out)),
            savings_usd,
            "trade comparison"
        );
    }
}

/// Record one block-state of a range under a `state` label. Emits the trade counter, and — for a
/// solved state — the gross bps delta, the signed USD savings, and the USD uplift (only when
/// Fynd beats the settled trade; a venue routes elsewhere when Fynd is worse). All highlighted
/// metrics compare gross vs gross, matching the headline verdict.
///
/// A sandwiched state's output was moved by MEV, not by Fynd's own routing, so it skips the
/// `SAVINGS_BPS`/`SAVINGS_USD`/`IMPROVEMENT_USD` histograms — the USD pair carry no outcome
/// label, so skipping is the only way to keep the "value of adding Fynd" aggregates clean. The
/// USD value is still computed and returned so the per-trade Loki line (in [`record_range`])
/// keeps logging.
///
/// Returns the signed USD savings it computed, `None` when the state is unsolved or unpriced.
fn record_state(
    range: &RangeComparison,
    state: &StateResult,
    state_label: &'static str,
    labels: &MetricLabels<'_>,
    prices: &usd::PriceMap,
) -> Option<f64> {
    counter!(
        TRADES_TOTAL,
        "venue" => labels.venue.to_string(),
        "solver" => labels.solver.to_string(),
        "chain" => labels.chain.to_string(),
        "outcome" => outcome_label(state.verdict).to_string(),
        "state" => state_label,
    )
    .increment(1);

    let sandwiched = state.verdict == Verdict::Sandwiched;

    if !sandwiched {
        if let Some(bps) = state.deltas.raw_bps {
            histogram!(
                SAVINGS_BPS,
                "venue" => labels.venue.to_string(),
                "solver" => labels.solver.to_string(),
                "chain" => labels.chain.to_string(),
                "outcome" => outcome_label(state.verdict).to_string(),
                "state" => state_label,
            )
            .record(bps);
        }
    }

    let Outcome::Solved(solved) = &state.outcome else {
        return None;
    };
    let usd =
        usd::savings_usd(range.token_out, solved.amount_out, range.settled_amount_out, prices)?;
    if sandwiched {
        return Some(usd);
    }

    if is_usd_outlier(usd) {
        warn!(
            tx = %range.tx_hash,
            block = range.block_number,
            state = state_label,
            venue = %range.venue,
            solver = %range.solver,
            token_in = %range.token_in,
            token_out = %range.token_out,
            amount_in = %range.amount_in,
            settled_out = %range.settled_amount_out,
            fynd_out = %solved.amount_out,
            token_out_price = ?prices.get(&range.token_out),
            usd,
            "USD outlier — inspect for token mispricing vs genuinely large trade"
        );
    }
    histogram!(
        SAVINGS_USD,
        "venue" => labels.venue.to_string(),
        "solver" => labels.solver.to_string(),
        "chain" => labels.chain.to_string(),
        "state" => state_label,
    )
    .record(usd);

    if usd > 0.0 {
        histogram!(
            IMPROVEMENT_USD,
            "venue" => labels.venue.to_string(),
            "solver" => labels.solver.to_string(),
            "chain" => labels.chain.to_string(),
            "state" => state_label,
        )
        .record(usd);
    }
    Some(usd)
}

pub(crate) fn record_block_seconds(seconds: f64) {
    histogram!(BLOCK_SECONDS).record(seconds);
}

pub(crate) fn record_skipped_block() {
    counter!(SKIPPED_BLOCKS).increment(1);
}

pub(crate) fn record_feed_rebuild() {
    counter!(FEED_REBUILDS).increment(1);
}

#[expect(clippy::unused_async)]
async fn metrics_handler(handle: PrometheusHandle) -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4; charset=utf-8")
        .body(handle.render())
}

/// Histogram bucket upper bounds per metric. Without explicit buckets the exporter renders
/// `histogram!` metrics as Prometheus summaries (quantile-labeled series), which
/// `histogram_quantile(..., *_bucket)` dashboard queries cannot read and which cannot be
/// re-aggregated across labels — the same trap documented for
/// `worker_router_solve_duration_seconds` in the fynd deploy values.
///
/// Savings are signed (negative = Fynd worse), so their edges are symmetric around zero.
const SAVINGS_BPS_BUCKETS: &[f64] =
    &[-1000.0, -300.0, -100.0, -30.0, -10.0, -3.0, 0.0, 3.0, 10.0, 30.0, 100.0, 300.0, 1000.0];
const SAVINGS_USD_BUCKETS: &[f64] = &[-3000.0, -300.0, -30.0, -3.0, 0.0, 3.0, 30.0, 300.0, 3000.0];
const VOLUME_USD_BUCKETS: &[f64] = &[10.0, 100.0, 1_000.0, 10_000.0, 100_000.0, 1_000_000.0];
const BLOCK_SECONDS_BUCKETS: &[f64] = &[0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0];

/// Install the global Prometheus recorder and serve `/metrics` on `port`.
///
/// The Actix server runs on its own thread with a dedicated Actix runtime; its server future is
/// `!Send`, so it can't be `tokio::spawn`ed onto the main multi-threaded runtime.
pub(crate) fn install_exporter(port: u16) -> anyhow::Result<()> {
    let handle = configure_buckets(PrometheusBuilder::new())
        .map_err(|e| anyhow::anyhow!("failed to configure histogram buckets: {e}"))?
        .install_recorder()
        .map_err(|e| anyhow::anyhow!("failed to install Prometheus recorder: {e}"))?;
    describe();

    std::thread::Builder::new()
        .name("hindsight-metrics".to_string())
        .spawn(move || {
            let result = actix_web::rt::System::new().block_on(async move {
                HttpServer::new(move || {
                    App::new().route(
                        "/metrics",
                        web::get().to({
                            let handle = handle.clone();
                            move || metrics_handler(handle.clone())
                        }),
                    )
                })
                .bind(("0.0.0.0", port))?
                .run()
                .await
            });
            if let Err(e) = result {
                error!("metrics server failed: {e}");
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to spawn metrics thread: {e}"))?;
    Ok(())
}

/// Register explicit buckets for every histogram metric, so the exporter renders true
/// Prometheus histograms (`*_bucket` series) instead of summaries.
fn configure_buckets(
    builder: PrometheusBuilder,
) -> Result<PrometheusBuilder, metrics_exporter_prometheus::BuildError> {
    use metrics_exporter_prometheus::Matcher;
    builder
        .set_buckets_for_metric(Matcher::Full(SAVINGS_BPS.into()), SAVINGS_BPS_BUCKETS)?
        .set_buckets_for_metric(Matcher::Full(SAVINGS_USD.into()), SAVINGS_USD_BUCKETS)?
        .set_buckets_for_metric(Matcher::Full(IMPROVEMENT_USD.into()), SAVINGS_USD_BUCKETS)?
        .set_buckets_for_metric(Matcher::Full(VOLUME_USD.into()), VOLUME_USD_BUCKETS)?
        .set_buckets_for_metric(Matcher::Full(BLOCK_SECONDS.into()), BLOCK_SECONDS_BUCKETS)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, Address, TxHash, U256};
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;
    use crate::{
        decoder::{AttributionSource, DecodedTrade, SandwichEvidence},
        resolve::{build_range, SolvedAmount},
    };

    fn trade(token_out: Address, settled: u64) -> DecodedTrade {
        DecodedTrade {
            tx_hash: TxHash::default(),
            block_number: 21_000_000,
            tx_index: 0,
            venue: "relay".into(),
            solver: "tycho".into(),
            solver_source: AttributionSource::TraceMatch,
            sender: Address::ZERO,
            token_in: Address::repeat_byte(0x11),
            token_out,
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(settled),
            venue_fee: None,
            venue_fee_out: None,
            settled_gas: None,
            quote: None,
            sandwich: None,
        }
    }

    fn solved(amount_out: u64, net: u64) -> Outcome {
        Outcome::Solved(SolvedAmount {
            amount_out: U256::from(amount_out),
            amount_out_net_gas: U256::from(net),
            gas_estimate: U256::from(21_000),
            quote_json: None,
        })
    }

    #[test]
    fn outcome_labels() {
        assert_eq!(outcome_label(Verdict::Win), "win");
        assert_eq!(outcome_label(Verdict::Loss), "loss");
        assert_eq!(outcome_label(Verdict::CoverageMiss), "coverage_miss");
        assert_eq!(outcome_label(Verdict::Unsolvable), "unsolvable");
    }

    #[test]
    fn usd_outlier_threshold() {
        assert!(!is_usd_outlier(0.0));
        assert!(!is_usd_outlier(USD_OUTLIER_THRESHOLD - 1.0));
        assert!(!is_usd_outlier(-(USD_OUTLIER_THRESHOLD - 1.0)));
        assert!(is_usd_outlier(USD_OUTLIER_THRESHOLD));
        assert!(is_usd_outlier(-(USD_OUTLIER_THRESHOLD * 100.0)));
    }

    #[test]
    fn record_range_labels_both_states() {
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        // Top wins (net 1005 USDC vs 1000 settled); back loses (net 995).
        let range = build_range(
            &trade(usdc, 1_000_000_000),
            &usd::PriceMap::new(),
            solved(1_010_000_000, 1_005_000_000),
            solved(998_000_000, 995_000_000),
        );
        // USDC priced at 2e-9 native-units per ETH-wei (ETH = $2000) anchors ETH→USD.
        let prices = usd::PriceMap::from([(usdc, 2e-9)]);

        let recorder = configure_buckets(PrometheusBuilder::new())
            .unwrap()
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(&range, "ethereum", &prices, &prices, &Registry::ethereum());
        });
        let rendered = handle.render();

        // Both states are counted, each under its own `state` label and verdict.
        assert!(rendered.contains("state=\"top\""), "rendered: {rendered}");
        assert!(rendered.contains("state=\"back\""), "rendered: {rendered}");
        assert!(rendered.contains("outcome=\"win\""));
        assert!(rendered.contains("outcome=\"loss\""));
        assert!(rendered.contains("venue=\"relay\""));
        // Top beats settled → uplift recorded; volume recorded once, tagged with the headline
        // verdict so the dashboard can split volume by outcome.
        assert!(rendered.contains("hindsight_improvement_usd"));
        let volume_line = rendered
            .lines()
            .find(|line| line.starts_with("hindsight_volume_usd_bucket"))
            .expect("volume histogram rendered");
        assert!(volume_line.contains("outcome=\"win\""), "volume line: {volume_line}");
        // Histograms must render as true histograms (le-labeled _bucket series), not summaries:
        // the dashboard reads them with histogram_quantile(..., *_bucket).
        // outcome label on savings_bps lets the dashboard show median per win/loss.
        assert!(
            rendered.contains("hindsight_savings_bps_bucket"),
            "savings_bps rendered as a summary, not a histogram: {rendered}"
        );
        assert!(
            rendered
                .lines()
                .any(|l| l.contains("hindsight_savings_bps_bucket") && l.contains("outcome=")),
            "savings_bps missing outcome label: {rendered}"
        );
        assert!(rendered.contains("hindsight_savings_usd_bucket"));
        assert!(rendered.contains("le=\"3\""));
    }

    #[test]
    fn label_vocabulary_is_registry_bounded() {
        // Every metric label value must come from the registry vocabulary: registered venue and
        // solver names pass through; raw addresses, unregistered names, and calldata-declared
        // aggregator ids collapse — venues to "other", solvers to "unknown".
        let registry = Registry::ethereum();
        assert_eq!(venue_label("relay", &registry), "relay");
        assert_eq!(venue_label("metamask", &registry), "metamask");
        assert_eq!(venue_label("okx", &registry), "other");
        assert_eq!(venue_label("0xD720183DdA64a8CDb424B5c13aF73baf713521f8", &registry), "other");
        assert_eq!(solver_label("kyberswap", &registry), "kyberswap");
        assert_eq!(solver_label("relay", &registry), "unknown");
        assert_eq!(solver_label("pancakeSwapRouterFeeDynamic", &registry), "unknown");
        assert_eq!(
            solver_label("0xB6F54cAed61C318027c022c47B94BAF139a99Dab", &registry),
            "unknown"
        );
    }

    #[test]
    fn raw_address_labels_collapse() {
        // Unknown venues/solvers carry raw 0x… addresses; every distinct address would mint a
        // fresh series set, unbounded over a long run. Venues outside the registry's
        // [venues.*] sections collapse to "other", solvers outside [solvers] to "unknown".
        let mut t = trade(address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"), 1_000);
        t.venue = "0xD720183DdA64a8CDb424B5c13aF73baf713521f8".to_string();
        t.solver = "0xB6F54cAed61C318027c022c47B94BAF139a99Dab".to_string();
        let range =
            build_range(&t, &usd::PriceMap::new(), solved(1_100, 1_050), solved(1_100, 1_050));

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(
                &range,
                "ethereum",
                &usd::PriceMap::new(),
                &usd::PriceMap::new(),
                &Registry::ethereum(),
            );
        });
        let rendered = handle.render();
        assert!(rendered.contains("venue=\"other\""), "rendered: {rendered}");
        assert!(rendered.contains("solver=\"unknown\""));
        assert!(!rendered.contains("0xD720183DdA64a8CDb424B5c13aF73baf713521f8"));
    }

    #[test]
    fn fallback_solver_label_collapses_to_unknown() {
        // The Fallback tier labels the solver with the entry point — a venue name like "relay",
        // not a solver — which would pollute the dashboard's solver dropdown. The metric label
        // must collapse to "unknown"; the JSONL keeps the entry-point detail.
        let mut t = trade(address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"), 1_000);
        t.solver = "relay".to_string();
        t.solver_source = AttributionSource::Fallback;
        let range =
            build_range(&t, &usd::PriceMap::new(), solved(1_100, 1_050), solved(1_100, 1_050));

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(
                &range,
                "ethereum",
                &usd::PriceMap::new(),
                &usd::PriceMap::new(),
                &Registry::ethereum(),
            );
        });
        let rendered = handle.render();
        assert!(rendered.contains("solver=\"unknown\""), "rendered: {rendered}");
        assert!(!rendered.contains("solver=\"relay\""));
    }

    #[test]
    fn volume_falls_back_to_input_leg_when_output_unpriced() {
        // Swap into a long-tail token: the output leg is unpriced — which is exactly why the
        // trade is unsolvable — so volume must be valued from the input leg instead, or
        // unsolvable volume would be systematically undercounted.
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let mut t = trade(Address::repeat_byte(0x42), 1_000);
        t.token_in = usdc;
        t.amount_in = U256::from(1_000_000_000u64); // 1000 USDC
        let range = build_range(
            &t,
            &usd::PriceMap::new(),
            Outcome::Unsolvable("no route".into()),
            Outcome::Unsolvable("no route".into()),
        );
        let prices = usd::PriceMap::from([(usdc, 2e-9)]);

        let recorder = configure_buckets(PrometheusBuilder::new())
            .unwrap()
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(&range, "ethereum", &prices, &prices, &Registry::ethereum());
        });
        let rendered = handle.render();
        let volume_line = rendered
            .lines()
            .find(|line| line.starts_with("hindsight_volume_usd_bucket"))
            .expect("volume recorded from the input leg");
        assert!(volume_line.contains("outcome=\"unsolvable\""), "volume line: {volume_line}");
    }

    #[test]
    fn record_range_skips_savings_when_unsolvable() {
        let range = build_range(
            &trade(Address::repeat_byte(0x22), 1_000),
            &usd::PriceMap::new(),
            Outcome::Unsolvable("x".into()),
            Outcome::Unsolvable("x".into()),
        );
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(
                &range,
                "ethereum",
                &usd::PriceMap::new(),
                &usd::PriceMap::new(),
                &Registry::ethereum(),
            );
        });
        let rendered = handle.render();
        assert!(rendered.contains("outcome=\"unsolvable\""));
        // No solved state and no prices → no savings/improvement samples.
        assert!(!rendered.contains("hindsight_savings_bps"));
        assert!(!rendered.contains("hindsight_improvement_usd"));
    }

    #[test]
    fn record_range_skips_savings_when_sandwiched() {
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let mut sandwiched = trade(usdc, 1_000_000_000);
        sandwiched.sandwich = Some(SandwichEvidence {
            front_tx: TxHash::repeat_byte(0xaa),
            back_tx: TxHash::repeat_byte(0xbb),
            attacker: Address::repeat_byte(0xcc),
            pools: vec![Address::repeat_byte(0xdd)],
        });
        let prices = usd::PriceMap::from([(usdc, 2e-9)]);
        let range = build_range(
            &sandwiched,
            &prices,
            solved(1_100_000_000, 1_090_000_000),
            solved(1_100_000_000, 1_090_000_000),
        );
        assert_eq!(range.verdict, Verdict::Sandwiched);

        let recorder = configure_buckets(PrometheusBuilder::new())
            .unwrap()
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(&range, "ethereum", &prices, &prices, &Registry::ethereum());
        });
        let rendered = handle.render();

        assert!(rendered.contains("outcome=\"sandwiched\""), "rendered: {rendered}");
        // Volume is still tracked, tagged with the sandwiched outcome — only the "value of adding
        // Fynd" histograms are excluded.
        assert!(rendered.contains("hindsight_volume_usd"));
        assert!(!rendered.contains("hindsight_savings_bps"), "rendered: {rendered}");
        assert!(!rendered.contains("hindsight_savings_usd"), "rendered: {rendered}");
        assert!(!rendered.contains("hindsight_improvement_usd"), "rendered: {rendered}");
    }
}

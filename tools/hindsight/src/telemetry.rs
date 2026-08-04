//! Prometheus metrics for re-solve comparisons, plus the `/metrics` HTTP exporter.
//!
//! Mirrors the exporter pattern used by the `fynd` binary: install a global Prometheus recorder
//! and serve its rendered output on a dedicated port via Actix.

use std::time::Duration;

use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use metrics::{counter, describe_counter, describe_histogram, histogram, Unit};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tracing::{error, info, warn};

use crate::{
    decoder::Registry,
    resolve::{render_route, Outcome, RangeComparison, StateResult, Verdict},
    usd::Prices,
};

const TRADES_TOTAL: &str = "hindsight_trades_total";
const SAVINGS_BPS: &str = "hindsight_savings_bps";
const SAVINGS_USD: &str = "hindsight_savings_usd";
const IMPROVEMENT_USD: &str = "hindsight_improvement_usd";
const SLIPPAGE_BPS: &str = "hindsight_slippage_bps";
const SLIPPAGE_USD: &str = "hindsight_slippage_usd";
const POSITIVE_SLIPPAGE_USD: &str = "hindsight_positive_slippage_usd";
const VOLUME_USD: &str = "hindsight_volume_usd";
const BLOCK_SECONDS: &str = "hindsight_block_processing_seconds";
const HEAD_LAG_BLOCKS: &str = "hindsight_chain_head_lag_blocks";
const RPC_INDEX_WAIT: &str = "hindsight_rpc_index_wait_seconds";
const SKIPPED_BLOCKS: &str = "hindsight_skipped_blocks_total";
const FEED_REBUILDS: &str = "hindsight_feed_rebuilds_total";
const UNTRACED_TRANSACTIONS: &str = "hindsight_untraced_transactions_total";

/// Absolute USD savings beyond which a comparison is logged with full per-trade context, so large
/// outliers can be traced and classified (a genuinely large trade vs a token-mispricing artifact
/// from the gas-token-anchored valuation).
const USD_OUTLIER_THRESHOLD: f64 = 1_000.0;

/// Settled USD notional below which a trade is excluded from the win-rate (`TRADES_TOTAL`) and bps
/// (`SAVINGS_BPS`) metrics. On a dust trade a few wei of rounding is tens to thousands of bps, so
/// dust dominates those unweighted quantiles and win counts while contributing nothing real. The
/// USD-weighted histograms (`VOLUME_USD`/`SAVINGS_USD`/`IMPROVEMENT_USD`) keep every trade — a $5
/// win adds $5 — so total savings and volume stay complete. A trade whose notional cannot be
/// priced is kept: we do not drop what we cannot measure.
const MIN_NOTIONAL_USD: f64 = 100.0;

/// Whether a USD savings value is large enough to log for inspection.
fn is_usd_outlier(usd: f64) -> bool {
    usd.abs() >= USD_OUTLIER_THRESHOLD
}

/// Metric label for a range's venue. Venues registered in the address book — the integrator
/// front-ends the comparison is pitched at — keep their name; everything else (direct router
/// entries, bots, unregistered addresses) collapses to "other". This keeps the dashboard's
/// venue filter bounded; full venue detail stays in the JSONL records.
fn venue_label<'a>(venue: &'a str, registry: &Registry) -> &'a str {
    if registry.venue(venue).is_some() {
        venue
    } else {
        "other"
    }
}

/// Metric label for a range's settling solver. Registered solver names pass through; everything
/// else collapses to "unknown". Attribution can also produce raw addresses, venue names, and
/// venue-declared ids like "pancakeSwapRouterFeeDynamic" — none of which belong in a bounded
/// metric label set. The original label stays in the JSONL records, where it serves as the
/// worklist for expanding the address book.
fn solver_label<'a>(solver: &'a str, registry: &Registry) -> &'a str {
    if registry.is_solver_name(solver) {
        solver
    } else {
        "unknown"
    }
}

/// Label value for a state with no winning route to attribute — Fynd did not solve it, so no
/// algorithm competed for it. Every series of a metric must carry the same label set, so an
/// unsolved state cannot simply omit the label.
const ALGORITHM_NONE: &str = "none";

/// The bounded label set shared by every per-trade metric, computed once per range.
struct MetricLabels<'a> {
    venue: &'a str,
    solver: &'a str,
    chain: &'a str,
}

/// Metric label for the algorithm whose route won a state's quote. Unlike venue and solver, this
/// needs no bounding against the registry: the value comes from Fynd's own worker pools, whose
/// `algorithm` names must resolve in the algorithm registry for the pool to spawn at all.
fn algorithm_label(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Solved(solved) if !solved.algorithm.is_empty() => &solved.algorithm,
        Outcome::Solved(_) | Outcome::Partial(_) | Outcome::Unsolvable(_) => ALGORITHM_NONE,
    }
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
        "Re-solved trades above the dust floor, labeled by venue / solver / chain / outcome / \
         algorithm / state (top|back). `algorithm` is the worker pool whose route won the quote, \
         so a per-venue split answers which algorithm serves that venue's flow best; it is \
         \"none\" when Fynd did not solve. Sub-$100-notional trades are excluded so the win-rate \
         reflects trades that matter; total volume stays complete in VOLUME_USD. Per-pair detail \
         lives in the JSONL comparison output; a token-pair label here is unbounded on mainnet and \
         would explode Prometheus series cardinality over a long run."
    );
    describe_histogram!(
        SAVINGS_BPS,
        Unit::Count,
        "Gross bps delta of Fynd vs settled (positive = Fynd better), labeled by venue / solver / \
         chain / outcome / algorithm / state (top|back). Sub-$100-notional trades are excluded — a \
         few wei of rounding is thousands of bps on dust and would swamp the quantiles."
    );
    describe_histogram!(
        SAVINGS_USD,
        "Signed gross USD savings of Fynd vs settled (positive = Fynd better), for Fynd-priced \
         trades, labeled by the algorithm whose route won"
    );
    describe_histogram!(
        IMPROVEMENT_USD,
        "Gross USD uplift on trades Fynd would improve (losses excluded — a venue routes \
         elsewhere when Fynd is worse), labeled by the algorithm whose route won. Sum = value of \
         adding Fynd; count = improving trades"
    );
    describe_histogram!(
        SLIPPAGE_BPS,
        Unit::Count,
        "Signed bps move of the top-of-block route re-executed at back-of-block (positive = the \
         route produced more than quoted), labeled by venue / solver / chain / outcome (headline \
         verdict)"
    );
    describe_histogram!(
        SLIPPAGE_USD,
        "Signed per-trade slippage USD (positive = the route produced more than quoted), labeled \
         by venue / solver / chain / outcome (headline verdict). The positive-only revenue \
         aggregate stays in POSITIVE_SLIPPAGE_USD"
    );
    describe_histogram!(
        POSITIVE_SLIPPAGE_USD,
        "USD surplus of the re-executed route over its top-of-block quote, recorded only when \
         positive. Sum = hypothetical revenue if positive slippage were charged; count = trades \
         with a surplus. Signed per-trade values stay in the JSONL records"
    );
    describe_histogram!(
        VOLUME_USD,
        "Observed settled trade volume in USD, labeled by venue / solver / outcome (headline \
         verdict). Valued from the output leg, falling back to the input leg when the output \
         token is unpriced"
    );
    describe_histogram!(
        BLOCK_SECONDS,
        Unit::Seconds,
        "Wall-clock time to re-solve one block, measured from the decoded trades. Excludes fetching \
         and decoding the block itself, so it does not measure how far behind chain head the \
         monitor is — HEAD_LAG_BLOCKS does"
    );
    describe_histogram!(
        HEAD_LAG_BLOCKS,
        Unit::Count,
        "How far the block being re-solved trails chain head, in blocks. The monitor is one block \
         behind by construction (it solves block N against state N-1); beyond that this is the \
         cost of fetching and decoding the block. Sampled once per block from the head check that \
         guards --max-lag-blocks"
    );
    describe_histogram!(
        RPC_INDEX_WAIT,
        Unit::Seconds,
        "Time spent waiting for the receipts RPC to index the target block before decoding it, \
         including the head check itself. The receipts RPC trails the tycho stream that picks the \
         target, so this is a floor on the lag that no amount of solving speed removes"
    );
    describe_counter!(
        SKIPPED_BLOCKS,
        "Blocks skipped because the RPC could not provide receipts (e.g. it lagged the tycho \
         stream past its lag budget, or the block genuinely failed to decode)"
    );
    describe_counter!(
        FEED_REBUILDS,
        "Times the monitor declared its session unhealthy (feed died, or the monitor fell too \
         far behind chain head) and rebuilt the solver to resubscribe"
    );
    describe_counter!(
        UNTRACED_TRANSACTIONS,
        "Matched solver transactions dropped because the RPC could not trace them. Their block \
         still contributes its other trades, so this counts trades missing from the aggregates \
         rather than blocks"
    );
}

/// Record a two-state range: the top-of-block (N-1) and back-of-block (N) outcomes, each tagged
/// with a `state` label ("top"/"back"). `prices_top`/`prices_back` are the solver's token-price
/// snapshots at each state, used to value savings in USD; an empty map disables USD for that state.
pub(crate) fn record_range(
    range: &RangeComparison,
    chain: &str,
    prices_top: &Prices,
    prices_back: &Prices,
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
    let volume = prices_top
        .value_usd(range.token_out, range.settled_amount_out)
        .or_else(|| prices_top.value_usd(range.token_in, range.amount_in));
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

    // Dust guard: sub-`MIN_NOTIONAL_USD` trades distort the unweighted win-rate and bps quantiles,
    // so they are kept out of `TRADES_TOTAL` and `SAVINGS_BPS`. An unpriced trade has no known
    // notional and is kept. The notional is a per-trade quantity, so both states share this gate.
    let above_floor = volume.is_none_or(|usd| usd >= MIN_NOTIONAL_USD);

    let savings_top = record_state(range, &range.top, "top", &labels, prices_top, above_floor);
    record_state(range, &range.back, "back", &labels, prices_back, above_floor);
    record_slippage(range, &labels, prices_back);

    // One structured line per priced comparison, on the headline basis (top-of-block, gross).
    // Loki ingests pod stdout, so this line feeds the dashboard's top-trades table; keep the
    // message and field names stable — the LogQL query extracts them by regexp. Zero means
    // "unpriced" (or, for quoted_usd, "the solver declared no quote"). Per-hop pools and amounts
    // stay in the JSONL records.
    //
    // `route` is last on purpose: it is the only field whose value contains spaces, so a LogQL
    // regexp can only bound it by end-of-line. Keep it last, or the dashboard's route column
    // silently swallows every field after it.
    if let (Some(savings_usd), Outcome::Solved(solved)) = (savings_top, &range.top.outcome) {
        let priced = |amount| {
            prices_top
                .value_usd(range.token_out, amount)
                .unwrap_or(0.0)
        };
        info!(
            tx = %range.tx_hash,
            block = range.block_number,
            venue = %range.venue,
            solver = %range.solver,
            token_in = %range.token_in,
            token_out = %range.token_out,
            verdict = %outcome_label(range.verdict),
            algorithm = %algorithm_label(&range.top.outcome),
            volume_usd = volume.unwrap_or(0.0),
            settled_usd = priced(range.settled_amount_out),
            fynd_usd = priced(solved.amount_out),
            quoted_usd = range.quote.as_ref().map_or(0.0, |quote| priced(quote.amount_out)),
            savings_usd,
            route = %solved.solved_route.as_deref().map(render_route).unwrap_or_default(),
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
/// `SAVINGS_BPS`/`SAVINGS_USD`/`IMPROVEMENT_USD` histograms — the USD histograms carry no
/// outcome label, so skipping is the only way to keep the "value of adding Fynd" aggregates
/// clean. The USD value is still computed and returned so the per-trade Loki line (in
/// `record_range`) keeps logging.
///
/// Returns the signed USD savings it computed, `None` when the state is unsolved or unpriced.
fn record_state(
    range: &RangeComparison,
    state: &StateResult,
    state_label: &'static str,
    labels: &MetricLabels<'_>,
    prices: &Prices,
    above_floor: bool,
) -> Option<f64> {
    let algorithm = algorithm_label(&state.outcome);

    if above_floor {
        counter!(
            TRADES_TOTAL,
            "venue" => labels.venue.to_string(),
            "solver" => labels.solver.to_string(),
            "chain" => labels.chain.to_string(),
            "outcome" => outcome_label(state.verdict).to_string(),
            "algorithm" => algorithm.to_string(),
            "state" => state_label,
        )
        .increment(1);
    }

    let sandwiched = state.verdict == Verdict::Sandwiched;

    if above_floor && !sandwiched {
        if let Some(bps) = state.deltas.raw_bps {
            histogram!(
                SAVINGS_BPS,
                "venue" => labels.venue.to_string(),
                "solver" => labels.solver.to_string(),
                "chain" => labels.chain.to_string(),
                "outcome" => outcome_label(state.verdict).to_string(),
                "algorithm" => algorithm.to_string(),
                "state" => state_label,
            )
            .record(bps);
        }
    }

    let Outcome::Solved(solved) = &state.outcome else {
        return None;
    };
    let usd = prices.savings_usd(range.token_out, solved.amount_out, range.settled_amount_out)?;
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
            token_out_price = ?prices.get(range.token_out),
            usd,
            "USD outlier — inspect for token mispricing vs genuinely large trade"
        );
    }
    histogram!(
        SAVINGS_USD,
        "venue" => labels.venue.to_string(),
        "solver" => labels.solver.to_string(),
        "chain" => labels.chain.to_string(),
        "algorithm" => algorithm.to_string(),
        "state" => state_label,
    )
    .record(usd);

    if usd > 0.0 {
        histogram!(
            IMPROVEMENT_USD,
            "venue" => labels.venue.to_string(),
            "solver" => labels.solver.to_string(),
            "chain" => labels.chain.to_string(),
            "algorithm" => algorithm.to_string(),
            "state" => state_label,
        )
        .record(usd);
    }
    Some(usd)
}

/// Record the top route's slippage between quote time (N-1) and re-execution (N): the signed bps
/// move and the signed USD value always (both labeled with the range's headline verdict), and
/// additionally the USD surplus alone when positive — its histogram sum is the running "revenue
/// if we charged positive slippage" aggregate, mirroring how [`IMPROVEMENT_USD`] sums uplift.
/// Valued at `prices_back`, the state the surplus is realized at. Sandwiched trades are not
/// skipped: the comparison is Fynd-quote vs Fynd-re-execution, so the settled trade's MEV does
/// not enter it, and block N's pool moves are real either way.
fn record_slippage(range: &RangeComparison, labels: &MetricLabels<'_>, prices: &Prices) {
    let Some(slippage) = range.slippage else {
        return;
    };
    let outcome = outcome_label(range.verdict).to_string();
    histogram!(
        SLIPPAGE_BPS,
        "venue" => labels.venue.to_string(),
        "solver" => labels.solver.to_string(),
        "chain" => labels.chain.to_string(),
        "outcome" => outcome.clone(),
    )
    .record(slippage.bps);

    let Some(usd) = prices.savings_usd(
        range.token_out,
        slippage.reexecuted_amount_out,
        slippage.quoted_amount_out,
    ) else {
        return;
    };
    histogram!(
        SLIPPAGE_USD,
        "venue" => labels.venue.to_string(),
        "solver" => labels.solver.to_string(),
        "chain" => labels.chain.to_string(),
        "outcome" => outcome,
    )
    .record(usd);

    if usd <= 0.0 {
        return;
    }
    if is_usd_outlier(usd) {
        warn!(
            tx = %range.tx_hash,
            block = range.block_number,
            venue = %range.venue,
            solver = %range.solver,
            token_out = %range.token_out,
            quoted_out = %slippage.quoted_amount_out,
            reexecuted_out = %slippage.reexecuted_amount_out,
            token_out_price = ?prices.get(range.token_out),
            usd,
            "positive slippage USD outlier — inspect for token mispricing vs a genuine pool move"
        );
    }
    histogram!(
        POSITIVE_SLIPPAGE_USD,
        "venue" => labels.venue.to_string(),
        "solver" => labels.solver.to_string(),
        "chain" => labels.chain.to_string(),
    )
    .record(usd);
}

pub(crate) fn record_block_seconds(seconds: f64) {
    histogram!(BLOCK_SECONDS).record(seconds);
}

pub(crate) fn record_head_lag_blocks(blocks: u64) {
    // Via u32 to keep the conversion lossless. The clamp cannot bind in practice: a lag past
    // `--max-lag-blocks` (hundreds) ends the session long before it could approach u32.
    let blocks = u32::try_from(blocks).unwrap_or(u32::MAX);
    histogram!(HEAD_LAG_BLOCKS).record(f64::from(blocks));
}

pub(crate) fn record_rpc_index_wait(wait: Duration) {
    histogram!(RPC_INDEX_WAIT).record(wait.as_secs_f64());
}

pub(crate) fn record_skipped_block() {
    counter!(SKIPPED_BLOCKS).increment(1);
}

pub(crate) fn record_untraced_transaction() {
    counter!(UNTRACED_TRANSACTIONS).increment(1);
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
/// Positive-only by construction, so the edges start near zero; most per-trade surpluses are
/// well under a dollar.
const POSITIVE_SLIPPAGE_USD_BUCKETS: &[f64] =
    &[0.01, 0.1, 0.3, 1.0, 3.0, 10.0, 30.0, 100.0, 300.0, 3000.0];
const VOLUME_USD_BUCKETS: &[f64] = &[10.0, 100.0, 1_000.0, 10_000.0, 100_000.0, 1_000_000.0];
const BLOCK_SECONDS_BUCKETS: &[f64] = &[0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 16.0];
/// One block is the floor (the monitor solves block N against state N-1), so the low edges are
/// spaced tightly enough to tell "at the floor" from "a block or two of fetch cost".
const HEAD_LAG_BLOCKS_BUCKETS: &[f64] = &[1.0, 2.0, 3.0, 5.0, 10.0, 30.0, 100.0, 600.0];
/// Sub-block edges: on a 2-second chain the interesting question is what fraction of a block time
/// goes on waiting for the RPC.
const RPC_INDEX_WAIT_BUCKETS: &[f64] = &[0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 4.0, 8.0];

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
        .set_buckets_for_metric(Matcher::Full(SLIPPAGE_BPS.into()), SAVINGS_BPS_BUCKETS)?
        .set_buckets_for_metric(Matcher::Full(SLIPPAGE_USD.into()), SAVINGS_USD_BUCKETS)?
        .set_buckets_for_metric(
            Matcher::Full(POSITIVE_SLIPPAGE_USD.into()),
            POSITIVE_SLIPPAGE_USD_BUCKETS,
        )?
        .set_buckets_for_metric(Matcher::Full(VOLUME_USD.into()), VOLUME_USD_BUCKETS)?
        .set_buckets_for_metric(Matcher::Full(BLOCK_SECONDS.into()), BLOCK_SECONDS_BUCKETS)?
        .set_buckets_for_metric(Matcher::Full(HEAD_LAG_BLOCKS.into()), HEAD_LAG_BLOCKS_BUCKETS)?
        .set_buckets_for_metric(Matcher::Full(RPC_INDEX_WAIT.into()), RPC_INDEX_WAIT_BUCKETS)
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

    fn empty_prices() -> Prices {
        Prices::new(&Registry::ethereum())
    }

    fn trade(token_out: Address, settled: u64) -> DecodedTrade {
        DecodedTrade {
            tx_hash: TxHash::default(),
            block_number: 21_000_000,
            tx_index: 0,
            venue: "relay".into(),
            solver: "tycho".into(),
            solver_source: AttributionSource::TraceMatch,
            decoder: "sender-netting",
            sender: Address::ZERO,
            token_in: Address::repeat_byte(0x11),
            token_out,
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(settled),
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            quote: None,
            min_amount_out: None,
            sandwich: None,
        }
    }

    fn solved(amount_out: u64, net: u64) -> Outcome {
        solved_by("bellman_ford", amount_out, net)
    }

    /// A solved outcome attributed to `algorithm`, for the cases that assert on the label.
    fn solved_by(algorithm: &str, amount_out: u64, net: u64) -> Outcome {
        Outcome::Solved(SolvedAmount {
            amount_out: U256::from(amount_out),
            amount_out_net_gas: U256::from(net),
            gas_estimate: U256::from(21_000),
            algorithm: algorithm.to_string(),
            quote_json: None,
            solved_route: None,
        })
    }

    #[test]
    fn test_outcome_labels() {
        assert_eq!(outcome_label(Verdict::Win), "win");
        assert_eq!(outcome_label(Verdict::Loss), "loss");
        assert_eq!(outcome_label(Verdict::CoverageMiss), "coverage_miss");
        assert_eq!(outcome_label(Verdict::Unsolvable), "unsolvable");
        assert_eq!(outcome_label(Verdict::Sandwiched), "sandwiched");
    }

    #[test]
    fn test_algorithm_label() {
        assert_eq!(algorithm_label(&solved_by("water_fill", 1_000, 1_000)), "water_fill");
        // Nothing solved, nothing to attribute — and a quote that declared no algorithm must not
        // mint an empty label value.
        assert_eq!(algorithm_label(&solved_by("", 1_000, 1_000)), ALGORITHM_NONE);
        assert_eq!(algorithm_label(&Outcome::Unsolvable("x".into())), ALGORITHM_NONE);
        assert_eq!(algorithm_label(&Outcome::Partial("x".into())), ALGORITHM_NONE);
    }

    #[test]
    fn test_algorithm_label_on_the_quality_metrics() {
        // The competition question — which algorithm serves a venue's flow best — is answered by
        // splitting the win-rate, bps, and USD-uplift metrics on `algorithm`, so all four carry it.
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let range = build_range(
            &trade(usdc, 1_000_000_000),
            &empty_prices(),
            solved_by("path_frank_wolfe", 1_010_000_000, 1_005_000_000),
            solved_by("path_frank_wolfe", 1_010_000_000, 1_005_000_000),
            &Outcome::Unsolvable("x".into()),
        );
        let mut prices = empty_prices();
        prices.insert(usdc, 2e-9);

        let recorder = configure_buckets(PrometheusBuilder::new())
            .unwrap()
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(&range, "ethereum", &prices, &prices, &Registry::ethereum());
        });
        let rendered = handle.render();

        for metric in [TRADES_TOTAL, SAVINGS_BPS, SAVINGS_USD, IMPROVEMENT_USD] {
            assert!(
                rendered
                    .lines()
                    .any(|line| line.starts_with(metric) &&
                        line.contains("algorithm=\"path_frank_wolfe\"")),
                "{metric} is missing the algorithm label: {rendered}"
            );
        }
        // Volume is the settled trade's notional, not a Fynd routing property, so it stays
        // unlabeled — attributing a competitor's trade size to one of our algorithms is wrong.
        let volume_line = rendered
            .lines()
            .find(|line| line.starts_with(VOLUME_USD))
            .expect("volume histogram rendered");
        assert!(!volume_line.contains("algorithm="), "volume line: {volume_line}");
    }

    #[test]
    fn test_unsolved_state_is_labeled_none() {
        // An unsolvable state still increments the win-rate counter (it is a coverage miss, not a
        // missing sample), so it needs a label value — and it must not be blank.
        let range = build_range(
            &trade(Address::repeat_byte(0x22), 1_000),
            &empty_prices(),
            Outcome::Unsolvable("missing token in Tycho".into()),
            Outcome::Unsolvable("missing token in Tycho".into()),
            &Outcome::Unsolvable("no top-of-block route to re-execute".into()),
        );
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(
                &range,
                "ethereum",
                &empty_prices(),
                &empty_prices(),
                &Registry::ethereum(),
            );
        });
        let rendered = handle.render();
        assert!(rendered.contains("algorithm=\"none\""), "rendered: {rendered}");
        assert!(!rendered.contains("algorithm=\"\""), "rendered: {rendered}");
    }

    #[test]
    fn test_usd_outlier_threshold() {
        assert!(!is_usd_outlier(0.0));
        assert!(!is_usd_outlier(USD_OUTLIER_THRESHOLD - 1.0));
        assert!(!is_usd_outlier(-(USD_OUTLIER_THRESHOLD - 1.0)));
        assert!(is_usd_outlier(USD_OUTLIER_THRESHOLD));
        assert!(is_usd_outlier(-(USD_OUTLIER_THRESHOLD * 100.0)));
    }

    #[test]
    fn test_lag_metrics_render_as_histograms() {
        let recorder = configure_buckets(PrometheusBuilder::new())
            .unwrap()
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_head_lag_blocks(3);
            record_rpc_index_wait(Duration::from_millis(1_500));
        });
        let rendered = handle.render();

        // True histograms (le-labeled _bucket series), not summaries: the lag panels read them
        // with histogram_quantile(..., *_bucket), which cannot see summary quantiles.
        assert!(rendered.contains("hindsight_chain_head_lag_blocks_bucket"), "{rendered}");
        assert!(rendered.contains("hindsight_rpc_index_wait_seconds_bucket"), "{rendered}");
        // A 1.5s wait falls above the 1s edge and within the 2s one.
        assert!(rendered.contains("hindsight_rpc_index_wait_seconds_bucket{le=\"1\"} 0"));
        assert!(rendered.contains("hindsight_rpc_index_wait_seconds_bucket{le=\"2\"} 1"));
    }

    #[test]
    fn test_record_range_both_states() {
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        // Top wins (net 1005 USDC vs 1000 settled); back loses (net 995).
        let range = build_range(
            &trade(usdc, 1_000_000_000),
            &empty_prices(),
            solved(1_010_000_000, 1_005_000_000),
            solved(998_000_000, 995_000_000),
            &solved(998_000_000, 995_000_000),
        );
        // USDC priced at 2e-9 native-units per ETH-wei (ETH = $2000) anchors ETH→USD.
        let mut prices = empty_prices();
        prices.insert(usdc, 2e-9);

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
    fn record_range_emits_slippage_metrics() {
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        // Quoted 1000 USDC at top, re-executed to 1005 USDC at back → +50 bps, +$5 surplus.
        let range = build_range(
            &trade(usdc, 1_000_000_000),
            &empty_prices(),
            solved(1_000_000_000, 995_000_000),
            solved(1_005_000_000, 1_000_000_000),
            &solved(1_005_000_000, 1_000_000_000),
        );
        let mut prices = empty_prices();
        prices.insert(usdc, 2e-9);

        let recorder = configure_buckets(PrometheusBuilder::new())
            .unwrap()
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(&range, "ethereum", &prices, &prices, &Registry::ethereum());
        });
        let rendered = handle.render();

        let slippage_bps_line = rendered
            .lines()
            .find(|line| line.starts_with("hindsight_slippage_bps_bucket"))
            .expect("slippage bps histogram rendered");
        assert!(slippage_bps_line.contains("outcome="), "rendered: {slippage_bps_line}");
        assert!(rendered.contains("hindsight_slippage_usd_bucket"), "rendered: {rendered}");
        let surplus_sum = rendered
            .lines()
            .find(|line| line.starts_with("hindsight_positive_slippage_usd_sum"))
            .expect("positive slippage surplus recorded");
        let value: f64 = surplus_sum
            .rsplit(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!((value - 5.0).abs() < 1e-3, "expected ~$5 surplus, got {surplus_sum}");
    }

    #[test]
    fn negative_slippage_records_bps_but_no_positive_usd() {
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        // Re-execution underperforms the quote: the signed bps histogram still records, the
        // positive-only USD surplus does not.
        let range = build_range(
            &trade(usdc, 1_000_000_000),
            &empty_prices(),
            solved(1_000_000_000, 995_000_000),
            solved(995_000_000, 990_000_000),
            &solved(995_000_000, 990_000_000),
        );
        let mut prices = empty_prices();
        prices.insert(usdc, 2e-9);

        let recorder = configure_buckets(PrometheusBuilder::new())
            .unwrap()
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(&range, "ethereum", &prices, &prices, &Registry::ethereum());
        });
        let rendered = handle.render();

        assert!(rendered.contains("hindsight_slippage_bps_bucket"), "rendered: {rendered}");
        let slippage_usd_sum = rendered
            .lines()
            .find(|line| line.starts_with("hindsight_slippage_usd_sum"))
            .expect("signed slippage USD recorded regardless of sign");
        let value: f64 = slippage_usd_sum
            .rsplit(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(value < 0.0, "expected a negative signed slippage USD, got {slippage_usd_sum}");
        assert!(
            !rendered.contains("hindsight_positive_slippage_usd"),
            "negative slippage must not count as chargeable surplus: {rendered}"
        );
    }

    #[test]
    fn failed_reexecution_emits_no_slippage_metrics() {
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        // The fresh back solve succeeded — slippage must come from the re-execution alone.
        let range = build_range(
            &trade(usdc, 1_000_000_000),
            &empty_prices(),
            solved(1_000_000_000, 995_000_000),
            solved(1_002_000_000, 997_000_000),
            &Outcome::Unsolvable("re-execution failed: no simulation state".into()),
        );
        let mut prices = empty_prices();
        prices.insert(usdc, 2e-9);

        let recorder = configure_buckets(PrometheusBuilder::new())
            .unwrap()
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(&range, "ethereum", &prices, &prices, &Registry::ethereum());
        });
        let rendered = handle.render();

        assert!(!rendered.contains("hindsight_slippage_bps"), "rendered: {rendered}");
        assert!(!rendered.contains("hindsight_slippage_usd"), "rendered: {rendered}");
        assert!(!rendered.contains("hindsight_positive_slippage_usd"));
    }

    #[test]
    fn test_label_values_against_the_registry() {
        // Every metric label value must come from the registry: registered venue and
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
    fn test_raw_address_labels() {
        // Unknown venues/solvers carry raw 0x… addresses; every distinct address would mint a
        // fresh series set, unbounded over a long run. Venues outside the registry's
        // [venues.*] sections collapse to "other", solvers outside [solvers] to "unknown".
        let mut t = trade(address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"), 1_000);
        t.venue = "0xD720183DdA64a8CDb424B5c13aF73baf713521f8".to_string();
        t.solver = "0xB6F54cAed61C318027c022c47B94BAF139a99Dab".to_string();
        let range = build_range(
            &t,
            &empty_prices(),
            solved(1_100, 1_050),
            solved(1_100, 1_050),
            &solved(1_100, 1_050),
        );

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(
                &range,
                "ethereum",
                &empty_prices(),
                &empty_prices(),
                &Registry::ethereum(),
            );
        });
        let rendered = handle.render();
        assert!(rendered.contains("venue=\"other\""), "rendered: {rendered}");
        assert!(rendered.contains("solver=\"unknown\""));
        assert!(!rendered.contains("0xD720183DdA64a8CDb424B5c13aF73baf713521f8"));
    }

    #[test]
    fn test_fallback_solver_label() {
        // The Fallback tier labels the solver with the entry point — a venue name like "relay",
        // not a solver — which would pollute the dashboard's solver dropdown. The metric label
        // must collapse to "unknown"; the JSONL keeps the entry-point detail.
        let mut t = trade(address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"), 1_000);
        t.solver = "relay".to_string();
        t.solver_source = AttributionSource::Fallback;
        let range = build_range(
            &t,
            &empty_prices(),
            solved(1_100, 1_050),
            solved(1_100, 1_050),
            &solved(1_100, 1_050),
        );

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(
                &range,
                "ethereum",
                &empty_prices(),
                &empty_prices(),
                &Registry::ethereum(),
            );
        });
        let rendered = handle.render();
        assert!(rendered.contains("solver=\"unknown\""), "rendered: {rendered}");
        assert!(!rendered.contains("solver=\"relay\""));
    }

    #[test]
    fn test_volume_with_unpriced_output() {
        // Swap into a long-tail token: the output leg is unpriced — which is exactly why the
        // trade is unsolvable — so volume must be valued from the input leg instead, or
        // unsolvable volume would be systematically undercounted.
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let mut t = trade(Address::repeat_byte(0x42), 1_000);
        t.token_in = usdc;
        t.amount_in = U256::from(1_000_000_000u64); // 1000 USDC
        let range = build_range(
            &t,
            &empty_prices(),
            Outcome::Unsolvable("no route".into()),
            Outcome::Unsolvable("no route".into()),
            &Outcome::Unsolvable("no top-of-block route to re-execute".into()),
        );
        let mut prices = empty_prices();
        prices.insert(usdc, 2e-9);

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
    fn test_record_range_unsolvable() {
        let range = build_range(
            &trade(Address::repeat_byte(0x22), 1_000),
            &empty_prices(),
            Outcome::Unsolvable("x".into()),
            Outcome::Unsolvable("x".into()),
            &Outcome::Unsolvable("x".into()),
        );
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(
                &range,
                "ethereum",
                &empty_prices(),
                &empty_prices(),
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
    fn test_record_range_sandwiched() {
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let mut sandwiched = trade(usdc, 1_000_000_000);
        sandwiched.sandwich = Some(SandwichEvidence {
            front_tx: TxHash::repeat_byte(0xaa),
            back_tx: TxHash::repeat_byte(0xbb),
            attacker: Address::repeat_byte(0xcc),
            pools: vec![Address::repeat_byte(0xdd)],
        });
        let mut prices = empty_prices();
        prices.insert(usdc, 2e-9);
        let range = build_range(
            &sandwiched,
            &prices,
            solved(1_100_000_000, 1_090_000_000),
            solved(1_100_000_000, 1_090_000_000),
            &solved(1_100_000_000, 1_090_000_000),
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

    #[test]
    fn test_below_notional_floor_excluded_from_quality_metrics() {
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        // A $1 trade (1e6 native units at the anchor below) that Fynd would win by ~50 bps: dust,
        // whose bps and win count would swamp the unweighted metrics if it were recorded.
        let range = build_range(
            &trade(usdc, 1_000_000),
            &empty_prices(),
            solved(1_005_000, 1_005_000),
            solved(1_005_000, 1_005_000),
            &solved(1_005_000, 1_005_000),
        );
        let mut prices = empty_prices();
        prices.insert(usdc, 2e-9);

        let recorder = configure_buckets(PrometheusBuilder::new())
            .unwrap()
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(&range, "ethereum", &prices, &prices, &Registry::ethereum());
        });
        let rendered = handle.render();

        // Kept out of the unweighted win-rate and bps quantiles...
        assert!(!rendered.contains("hindsight_trades_total"), "rendered: {rendered}");
        assert!(!rendered.contains("hindsight_savings_bps"), "rendered: {rendered}");
        // ...but its USD contribution is still counted in the weighted histograms.
        assert!(rendered.contains("hindsight_volume_usd"), "rendered: {rendered}");
        assert!(rendered.contains("hindsight_savings_usd"), "rendered: {rendered}");
    }

    #[test]
    fn test_unpriced_trade_passes_the_floor() {
        // No prices → the notional is unknown, so the trade is kept rather than silently dropped;
        // its count and (solved) bps still land in the metrics.
        let range = build_range(
            &trade(Address::repeat_byte(0x42), 1_000),
            &empty_prices(),
            solved(1_100, 1_050),
            solved(1_100, 1_050),
            &solved(1_100, 1_050),
        );
        let recorder = configure_buckets(PrometheusBuilder::new())
            .unwrap()
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record_range(
                &range,
                "ethereum",
                &empty_prices(),
                &empty_prices(),
                &Registry::ethereum(),
            );
        });
        let rendered = handle.render();
        assert!(rendered.contains("hindsight_trades_total"), "rendered: {rendered}");
        assert!(rendered.contains("hindsight_savings_bps"), "rendered: {rendered}");
    }
}

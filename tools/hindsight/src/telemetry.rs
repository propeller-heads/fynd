//! Prometheus metrics for re-solve comparisons, plus the `/metrics` HTTP exporter.
//!
//! Mirrors the exporter pattern used by the `fynd` binary: install a global Prometheus recorder
//! and serve its rendered output on a dedicated port via Actix.

use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use metrics::{
    counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram, Unit,
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tracing::{error, warn};

use crate::{
    resolve::{Outcome, RangeComparison, Verdict},
    usd,
};

const TRADES_TOTAL: &str = "hindsight_trades_total";
const SAVINGS_BPS: &str = "hindsight_savings_bps";
const SAVINGS_USD: &str = "hindsight_savings_usd";
const IMPROVEMENT_USD: &str = "hindsight_improvement_usd";
const VOLUME_USD: &str = "hindsight_volume_usd";
const COVERAGE_RATIO: &str = "hindsight_coverage_ratio";
const BLOCK_SECONDS: &str = "hindsight_block_processing_seconds";

/// Absolute USD savings beyond which a comparison is logged with full per-trade context, so large
/// outliers can be traced and classified (a genuinely large trade vs a token-mispricing artifact
/// from the ETH-anchored valuation).
const USD_OUTLIER_THRESHOLD: f64 = 1_000.0;

/// Whether a USD savings value is large enough to log for inspection.
fn is_usd_outlier(usd: f64) -> bool {
    usd.abs() >= USD_OUTLIER_THRESHOLD
}

/// Metric label for a trade's headline verdict.
pub(crate) fn outcome_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Win => "win",
        Verdict::Loss => "loss",
        Verdict::CoverageMiss => "coverage_miss",
        Verdict::Unsolvable => "unsolvable",
    }
}

/// Fraction of trades that were comparable (solvable) out of the total seen.
pub(crate) fn coverage_ratio(total: usize, comparable: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        comparable as f64 / total as f64
    }
}

/// Register metric descriptions with the active recorder.
pub(crate) fn describe() {
    describe_counter!(
        TRADES_TOTAL,
        "Re-solved trades, labeled by client / aggregator / chain / outcome. Per-pair detail lives \
         in the JSONL comparison output; a token-pair label here is unbounded on mainnet and would \
         explode Prometheus series cardinality over a long run."
    );
    describe_histogram!(
        SAVINGS_BPS,
        Unit::Count,
        "Net-of-gas bps delta of Fynd vs settled (positive = Fynd better)"
    );
    describe_histogram!(
        SAVINGS_USD,
        "Signed USD savings of Fynd vs settled (positive = Fynd better), for Fynd-priced trades"
    );
    describe_histogram!(
        IMPROVEMENT_USD,
        "Net-of-gas USD uplift on trades Fynd would improve (losses excluded — a client routes \
         elsewhere when Fynd is worse). Sum = value of adding Fynd; count = improving trades"
    );
    describe_histogram!(
        VOLUME_USD,
        "Observed settled trade volume in USD, labeled by client / aggregator (Fynd-priced trades)"
    );
    describe_gauge!(COVERAGE_RATIO, "Fraction of trades Fynd could re-solve");
    describe_histogram!(BLOCK_SECONDS, Unit::Seconds, "Wall-clock time to process one block");
}

/// Record a two-state range, anchored on the headline (top-of-block) state. `prices` is the
/// solver's token-price snapshot used to value savings in USD; an empty map disables USD recording.
pub(crate) fn record_range(range: &RangeComparison, chain: &str, prices: &usd::PriceMap) {
    counter!(
        TRADES_TOTAL,
        "client" => range.client.clone(),
        "aggregator" => range.aggregator.clone(),
        "chain" => chain.to_string(),
        "outcome" => outcome_label(range.verdict).to_string(),
    )
    .increment(1);

    // Observed volume covers every trade (including unsolvable) so per-client coverage is accurate.
    if let Some(volume) = usd::value_usd(range.token_out, range.settled_amount_out, prices) {
        histogram!(
            VOLUME_USD,
            "client" => range.client.clone(),
            "aggregator" => range.aggregator.clone(),
            "chain" => chain.to_string(),
        )
        .record(volume);
    }

    if let Some(bps) = range.top.deltas.net_bps {
        histogram!(
            SAVINGS_BPS,
            "client" => range.client.clone(),
            "aggregator" => range.aggregator.clone(),
            "chain" => chain.to_string(),
        )
        .record(bps);
    }

    if let Outcome::Solved(solved) = &range.top.outcome {
        if let Some(usd) =
            usd::savings_usd(range.token_out, solved.amount_out, range.settled_amount_out, prices)
        {
            if is_usd_outlier(usd) {
                warn!(
                    tx = %range.tx_hash,
                    block = range.block_number,
                    client = %range.client,
                    aggregator = %range.aggregator,
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
                "client" => range.client.clone(),
                "aggregator" => range.aggregator.clone(),
                "chain" => chain.to_string(),
            )
            .record(usd);
        }

        // Client-benefit uplift: net-of-gas improvement, recorded only when Fynd beats the settled
        // trade. A client routes elsewhere when Fynd is worse, so losses contribute nothing.
        if let Some(uplift) = usd::savings_usd(
            range.token_out,
            solved.amount_out_net_gas,
            range.settled_amount_out,
            prices,
        ) {
            if uplift > 0.0 {
                histogram!(
                    IMPROVEMENT_USD,
                    "client" => range.client.clone(),
                    "aggregator" => range.aggregator.clone(),
                    "chain" => chain.to_string(),
                )
                .record(uplift);
            }
        }
    }
}

pub(crate) fn record_coverage(total: usize, comparable: usize) {
    gauge!(COVERAGE_RATIO).set(coverage_ratio(total, comparable));
}

pub(crate) fn record_block_seconds(seconds: f64) {
    histogram!(BLOCK_SECONDS).record(seconds);
}

async fn metrics_handler(handle: PrometheusHandle) -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4; charset=utf-8")
        .body(handle.render())
}

/// Install the global Prometheus recorder and serve `/metrics` on `port`.
///
/// The Actix server runs on its own thread with a dedicated Actix runtime; its server future is
/// `!Send`, so it can't be `tokio::spawn`ed onto the main multi-threaded runtime.
pub(crate) fn install_exporter(port: u16) -> anyhow::Result<()> {
    let handle = PrometheusBuilder::new()
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn coverage_ratio_math() {
        assert_eq!(coverage_ratio(0, 0), 0.0);
        assert_eq!(coverage_ratio(10, 4), 0.4);
        assert_eq!(coverage_ratio(5, 5), 1.0);
    }
}

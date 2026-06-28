//! Prometheus metrics for re-solve comparisons, plus the `/metrics` HTTP exporter.
//!
//! Mirrors the exporter pattern used by the `fynd` binary: install a global Prometheus recorder
//! and serve its rendered output on a dedicated port via Actix.

use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use alloy::primitives::Address;
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram, Unit};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tracing::error;

use crate::{
    resolve::{Comparison, Outcome, RangeComparison, Verdict},
    usd,
};

const TRADES_TOTAL: &str = "hindsight_trades_total";
const SAVINGS_BPS: &str = "hindsight_savings_bps";
const SAVINGS_USD: &str = "hindsight_savings_usd";
const COVERAGE_RATIO: &str = "hindsight_coverage_ratio";
const BLOCK_SECONDS: &str = "hindsight_block_processing_seconds";

/// Metric label for a trade's headline verdict.
pub(crate) fn outcome_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Win => "win",
        Verdict::Loss => "loss",
        Verdict::Unsolvable => "unsolvable",
    }
}

/// Pair label as lowercase `token_in->token_out` addresses. Symbol resolution is a future
/// enhancement; raw addresses keep label cardinality deterministic.
pub(crate) fn pair_label(token_in: Address, token_out: Address) -> String {
    format!("{token_in:#x}->{token_out:#x}")
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
        "Re-solved trades, labeled by client / aggregator / pair / chain / outcome"
    );
    describe_histogram!(
        SAVINGS_BPS,
        Unit::Count,
        "Net-of-gas bps delta of Fynd vs settled (positive = Fynd better)"
    );
    describe_histogram!(
        SAVINGS_USD,
        "Signed USD savings of Fynd vs settled, for stablecoin-out trades (positive = Fynd better)"
    );
    describe_gauge!(COVERAGE_RATIO, "Fraction of trades Fynd could re-solve");
    describe_histogram!(BLOCK_SECONDS, Unit::Seconds, "Wall-clock time to process one block");
}

/// Record a single re-solve comparison.
pub(crate) fn record(cmp: &Comparison, chain: &str) {
    counter!(
        TRADES_TOTAL,
        "client" => cmp.client.clone(),
        "aggregator" => cmp.aggregator.clone(),
        "pair" => pair_label(cmp.token_in, cmp.token_out),
        "chain" => chain.to_string(),
        "outcome" => outcome_label(cmp.verdict).to_string(),
    )
    .increment(1);

    if let Some(bps) = cmp.deltas.net_bps {
        histogram!(
            SAVINGS_BPS,
            "client" => cmp.client.clone(),
            "aggregator" => cmp.aggregator.clone(),
            "chain" => chain.to_string(),
        )
        .record(bps);
    }

    if let Outcome::Solved(solved) = &cmp.outcome {
        if let Some(usd) = usd::savings_usd(cmp.token_out, solved.amount_out, cmp.settled_amount_out)
        {
            histogram!(
                SAVINGS_USD,
                "client" => cmp.client.clone(),
                "aggregator" => cmp.aggregator.clone(),
                "chain" => chain.to_string(),
            )
            .record(usd);
        }
    }
}

/// Record a two-state range, anchored on the headline (top-of-block) state.
pub(crate) fn record_range(range: &RangeComparison, chain: &str) {
    counter!(
        TRADES_TOTAL,
        "client" => range.client.clone(),
        "aggregator" => range.aggregator.clone(),
        "pair" => pair_label(range.token_in, range.token_out),
        "chain" => chain.to_string(),
        "outcome" => outcome_label(range.verdict).to_string(),
    )
    .increment(1);

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
            usd::savings_usd(range.token_out, solved.amount_out, range.settled_amount_out)
        {
            histogram!(
                SAVINGS_USD,
                "client" => range.client.clone(),
                "aggregator" => range.aggregator.clone(),
                "chain" => chain.to_string(),
            )
            .record(usd);
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
    use alloy::primitives::U256;
    use metrics_exporter_prometheus::PrometheusBuilder;

    use alloy::primitives::address;

    use super::*;
    use crate::resolve::{Deltas, SolvedAmount};

    fn comparison(verdict: Verdict, net_bps: Option<f64>) -> Comparison {
        Comparison {
            tx_hash: "0xabc".into(),
            block_number: 21_000_000,
            client: "relay".into(),
            aggregator: "tycho".into(),
            token_in: Address::repeat_byte(0x11),
            token_out: Address::repeat_byte(0x22),
            amount_in: U256::ZERO,
            settled_amount_out: U256::ZERO,
            outcome: Outcome::Unsolvable("x".into()),
            deltas: Deltas { raw_bps: None, net_bps },
            verdict,
        }
    }

    #[test]
    fn outcome_labels() {
        assert_eq!(outcome_label(Verdict::Win), "win");
        assert_eq!(outcome_label(Verdict::Loss), "loss");
        assert_eq!(outcome_label(Verdict::Unsolvable), "unsolvable");
    }

    #[test]
    fn pair_label_is_directional_hex() {
        let label = pair_label(Address::repeat_byte(0x11), Address::repeat_byte(0x22));
        assert!(label.starts_with("0x"));
        assert!(label.contains("->"));
        // Direction matters: in->out differs from out->in.
        assert_ne!(label, pair_label(Address::repeat_byte(0x22), Address::repeat_byte(0x11)));
    }

    #[test]
    fn coverage_ratio_math() {
        assert_eq!(coverage_ratio(0, 0), 0.0);
        assert_eq!(coverage_ratio(10, 4), 0.4);
        assert_eq!(coverage_ratio(5, 5), 1.0);
    }

    #[test]
    fn record_emits_labeled_metrics() {
        // Isolated recorder (not global) so the test is deterministic and re-runnable.
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record(&comparison(Verdict::Win, Some(42.0)), "ethereum");
            record_coverage(2, 1);
        });
        let rendered = handle.render();

        assert!(rendered.contains("hindsight_trades_total"), "rendered: {rendered}");
        assert!(rendered.contains("outcome=\"win\""));
        assert!(rendered.contains("client=\"relay\""));
        assert!(rendered.contains("hindsight_savings_bps"));
        assert!(rendered.contains("hindsight_coverage_ratio"));
    }

    #[test]
    fn record_emits_usd_for_stablecoin_out() {
        let usdc = address!("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let mut cmp = comparison(Verdict::Win, Some(10.0));
        cmp.token_out = usdc;
        cmp.settled_amount_out = U256::from(1_000_000_000u64); // 1000 USDC
        cmp.outcome = Outcome::Solved(SolvedAmount {
            amount_out: U256::from(1_010_000_000u64), // 1010 USDC
            amount_out_net_gas: U256::from(1_005_000_000u64),
            gas_estimate: U256::from(120_000u64),
        });

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || record(&cmp, "ethereum"));
        let rendered = handle.render();
        assert!(rendered.contains("hindsight_savings_usd"), "rendered: {rendered}");
    }

    #[test]
    fn record_skips_savings_when_unsolvable() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            record(&comparison(Verdict::Unsolvable, None), "ethereum");
        });
        let rendered = handle.render();
        assert!(rendered.contains("outcome=\"unsolvable\""));
        // No net bps → no savings sample recorded.
        assert!(!rendered.contains("hindsight_savings_bps"));
    }
}

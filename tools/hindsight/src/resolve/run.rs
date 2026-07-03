//! Live `resolve` driver: decode a block's aggregator trades, re-solve each through a running
//! Fynd, and report how Fynd compares to what settled on-chain.
//!
//! This compares at the chain's current state. Re-solving at top-of-block (N-1) and back-of-block
//! (N) as a range is a follow-up gated on `BlockStepController` support in `fynd-core`.

use std::{sync::Arc, time::Duration};

use alloy::{
    primitives::{Address, U256},
    providers::Provider,
};
use async_trait::async_trait;
use fynd_client::{FyndClientBuilder, RetryConfig};
use fynd_tools_common::{
    aggregator::{AggregatorClient, AggregatorQuote},
    fynd::FyndAggregator,
};
use serde::Serialize;
use tracing::{info, warn};

use crate::{
    decoder::decode_block,
    resolve::{compare_trade, Comparison, Outcome, SolvedAmount, Verdict},
};

/// Re-solves through a running Fynd instance over HTTP via the shared `FyndAggregator`.
struct FyndReSolver {
    aggregator: FyndAggregator,
}

#[async_trait]
impl super::ReSolver for FyndReSolver {
    async fn solve(&self, token_in: Address, token_out: Address, amount_in: U256) -> Outcome {
        match self
            .aggregator
            .quote(
                &format!("{token_in:#x}"),
                &format!("{token_out:#x}"),
                &amount_in.to_string(),
                None,
            )
            .await
        {
            Ok(quote) => quote_to_outcome(quote),
            Err(e) => Outcome::Unsolvable(format!("request failed: {e}")),
        }
    }
}

/// Map a Fynd [`AggregatorQuote`] onto a re-solve [`Outcome`].
fn quote_to_outcome(quote: AggregatorQuote) -> Outcome {
    if !quote.is_success() {
        return Outcome::Unsolvable(quote.status.to_string());
    }
    let Some(amount_out) = quote
        .amount_out
        .as_deref()
        .and_then(parse_u256)
    else {
        return Outcome::Unsolvable("missing amount_out".to_string());
    };
    let amount_out_net_gas = match quote
        .amount_out_net_gas
        .as_deref()
        .and_then(parse_u256)
    {
        Some(net) => net,
        None => {
            warn!(
                %amount_out,
                "quote missing amount_out_net_gas; using raw amount_out (net-of-gas delta is \
                 biased in Fynd's favour for this trade)"
            );
            amount_out
        }
    };
    let gas_estimate = quote
        .gas_units
        .map(U256::from)
        .unwrap_or(U256::ZERO);
    Outcome::Solved(SolvedAmount { amount_out, amount_out_net_gas, gas_estimate })
}

fn parse_u256(s: &str) -> Option<U256> {
    s.parse().ok()
}

/// Blocks farther behind chain head than this make a re-solve misleading: Fynd solves against the
/// chain's *current* state, so comparing that quote to a much older settled amount mixes present
/// liquidity with a historical baseline. ~100 blocks is roughly 20 minutes on Ethereum.
const STALE_BLOCK_THRESHOLD: u64 = 100;

/// Whether the newest requested block trails chain head by more than `threshold` blocks.
fn is_stale(head: u64, newest_requested: u64, threshold: u64) -> bool {
    head.saturating_sub(newest_requested) > threshold
}

/// Warn if `blocks` are far enough behind chain head that a current-state re-solve is misleading.
///
/// This is the guardrail against accidentally re-solving a historical block against the live Fynd
/// state, which yields nonsensical deltas. A fetch failure is itself only a warning — it must not
/// abort the run.
async fn warn_if_stale<P: Provider>(provider: &P, blocks: &[u64]) {
    let Some(&newest) = blocks.iter().max() else {
        return;
    };
    let head = match provider.get_block_number().await {
        Ok(head) => head,
        Err(error) => {
            warn!(%error, "could not fetch chain head to check block staleness");
            return;
        }
    };
    if is_stale(head, newest, STALE_BLOCK_THRESHOLD) {
        warn!(
            head,
            newest_requested = newest,
            blocks_behind = head.saturating_sub(newest),
            "re-solving blocks far behind chain head; Fynd solves at current state, so these \
             comparisons pit present liquidity against a historical settled amount"
        );
    }
}

/// Aggregate win/loss statistics over a set of comparisons.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct Summary {
    pub total: usize,
    pub wins: usize,
    pub losses: usize,
    /// Trades where Fynd only returned a partial route for the settled size (a coverage gap,
    /// tracked apart from `unsolvable`).
    pub coverage_miss: usize,
    pub unsolvable: usize,
    /// Median raw bps delta over solvable trades (positive = Fynd better).
    pub median_raw_bps: Option<f64>,
    /// Median net-of-gas bps delta over solvable trades.
    pub median_net_bps: Option<f64>,
}

pub(crate) fn summarize(comparisons: &[Comparison]) -> Summary {
    let mut raw: Vec<f64> = Vec::new();
    let mut net: Vec<f64> = Vec::new();
    let mut summary = Summary { total: comparisons.len(), ..Default::default() };

    for cmp in comparisons {
        match cmp.verdict {
            Verdict::Win => summary.wins += 1,
            Verdict::Loss => summary.losses += 1,
            Verdict::CoverageMiss => summary.coverage_miss += 1,
            Verdict::Unsolvable => summary.unsolvable += 1,
        }
        if let Some(d) = cmp.deltas.raw_bps {
            raw.push(d);
        }
        if let Some(d) = cmp.deltas.net_bps {
            net.push(d);
        }
    }

    summary.median_raw_bps = median(&mut raw);
    summary.median_net_bps = median(&mut net);
    summary
}

fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| {
        a.partial_cmp(b)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Some(values[values.len() / 2])
}

fn print_summary(summary: &Summary) {
    println!("\n{}", "=".repeat(60));
    println!("  HINDSIGHT RE-SOLVE  ({} trades)", summary.total);
    println!("{}", "=".repeat(60));
    let comparable = summary.wins + summary.losses;
    let win_pct =
        if comparable > 0 { summary.wins as f64 / comparable as f64 * 100.0 } else { 0.0 };
    println!("  Fynd wins:  {}/{} ({win_pct:.1}%)", summary.wins, comparable);
    println!("  losses:        {}", summary.losses);
    println!("  coverage miss: {}", summary.coverage_miss);
    println!("  unsolvable:    {}", summary.unsolvable);
    match summary.median_raw_bps {
        Some(d) => println!("  median raw:     {d:+.2} bps"),
        None => println!("  median raw:     n/a"),
    }
    match summary.median_net_bps {
        Some(d) => println!("  median net-gas: {d:+.2} bps"),
        None => println!("  median net-gas: n/a"),
    }
    println!("{}", "=".repeat(60));
}

/// Decode `blocks`, re-solve every trade through the Fynd instance at `fynd_url`, and report.
pub(crate) async fn run(
    rpc_url: &str,
    fynd_url: &str,
    block: Option<u64>,
    range: Option<&str>,
    timeout_ms: u64,
    json: bool,
) -> anyhow::Result<()> {
    let provider = crate::provider_from(rpc_url)?;
    let blocks = crate::resolve_blocks(&provider, block, range).await?;
    warn_if_stale(&provider, &blocks).await;

    let client = FyndClientBuilder::new(fynd_url)
        .with_timeout(Duration::from_millis(timeout_ms))
        .with_retry(RetryConfig::new(1, Duration::from_millis(0), Duration::from_millis(0)))
        .build_quote_only()
        .map_err(|e| anyhow::anyhow!("failed to build Fynd client: {e}"))?;
    let resolver =
        FyndReSolver { aggregator: FyndAggregator::new(Arc::new(client), timeout_ms, 0.0) };

    let mut comparisons = Vec::new();
    for block_number in &blocks {
        let trades = match decode_block(&provider, *block_number).await {
            Ok(trades) => trades,
            Err(error) => {
                warn!(block = block_number, %error, "failed to decode block; skipping");
                continue;
            }
        };
        info!(block = block_number, count = trades.len(), "re-solving decoded trades");
        for trade in &trades {
            comparisons.push(compare_trade(&resolver, trade).await);
        }
    }

    let summary = summarize(&comparisons);
    if json {
        #[derive(Serialize)]
        struct Report<'a> {
            summary: &'a Summary,
            comparisons: &'a [Comparison],
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&Report { summary: &summary, comparisons: &comparisons })?
        );
    } else {
        print_summary(&summary);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::Deltas;

    fn quote(amount_out: Option<&str>, net: Option<&str>, success: bool) -> AggregatorQuote {
        use fynd_tools_common::aggregator::AggregatorStatus;
        AggregatorQuote {
            status: if success { AggregatorStatus::Success } else { AggregatorStatus::NoRoute },
            amount_out: amount_out.map(str::to_string),
            amount_out_net_gas: net.map(str::to_string),
            gas_units: Some(120_000),
            protocols: vec![],
            num_splits: None,
            response_time_ms: 5,
            calldata: None,
            route: None,
        }
    }

    #[test]
    fn quote_to_outcome_solved() {
        let Outcome::Solved(s) = quote_to_outcome(quote(Some("10000"), Some("9900"), true)) else {
            panic!("expected solved");
        };
        assert_eq!(s.amount_out, U256::from(10_000u64));
        assert_eq!(s.amount_out_net_gas, U256::from(9_900u64));
        assert_eq!(s.gas_estimate, U256::from(120_000u64));
    }

    #[test]
    fn quote_to_outcome_net_falls_back_to_raw() {
        let Outcome::Solved(s) = quote_to_outcome(quote(Some("10000"), None, true)) else {
            panic!("expected solved");
        };
        assert_eq!(s.amount_out_net_gas, U256::from(10_000u64));
    }

    #[test]
    fn quote_to_outcome_unsuccessful_is_unsolvable() {
        assert!(matches!(quote_to_outcome(quote(None, None, false)), Outcome::Unsolvable(_)));
    }

    #[test]
    fn quote_to_outcome_missing_amount_is_unsolvable() {
        assert!(matches!(quote_to_outcome(quote(None, None, true)), Outcome::Unsolvable(_)));
    }

    fn comparison(verdict: Verdict, raw: Option<f64>, net: Option<f64>) -> Comparison {
        Comparison {
            tx_hash: "0x".into(),
            block_number: 1,
            client: "c".into(),
            aggregator: "a".into(),
            token_in: Address::ZERO,
            token_out: Address::ZERO,
            amount_in: U256::ZERO,
            settled_amount_out: U256::ZERO,
            outcome: Outcome::Unsolvable("x".into()),
            deltas: Deltas { raw_bps: raw, net_bps: net },
            verdict,
        }
    }

    #[test]
    fn summarize_counts_and_medians() {
        let comparisons = vec![
            comparison(Verdict::Win, Some(100.0), Some(80.0)),
            comparison(Verdict::Win, Some(50.0), Some(40.0)),
            comparison(Verdict::Loss, Some(-30.0), Some(-50.0)),
            comparison(Verdict::CoverageMiss, None, None),
            comparison(Verdict::Unsolvable, None, None),
        ];
        let s = summarize(&comparisons);
        assert_eq!(s.total, 5);
        assert_eq!(s.wins, 2);
        assert_eq!(s.losses, 1);
        assert_eq!(s.coverage_miss, 1);
        assert_eq!(s.unsolvable, 1);
        // Median of [-30, 50, 100] = 50.
        assert_eq!(s.median_raw_bps, Some(50.0));
    }

    #[test]
    fn summarize_empty() {
        assert_eq!(summarize(&[]), Summary::default());
    }

    #[test]
    fn is_stale_flags_only_far_behind_blocks() {
        assert!(is_stale(1_000, 800, 100));
        assert!(!is_stale(1_000, 950, 100));
        assert!(!is_stale(1_000, 1_000, 100));
        // Requested block ahead of head (e.g. head moved back on a reorg) is not stale.
        assert!(!is_stale(1_000, 1_050, 100));
    }
}

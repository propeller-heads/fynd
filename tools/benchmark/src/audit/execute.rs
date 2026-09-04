//! Per-trade execution: fire every participant's quote, validate via `eth_call`, and compute
//! the bps diffs that go into a [`TradeResult`].

use std::{collections::HashMap, sync::Arc, time::Duration};

use alloy::{
    hex,
    primitives::{Address, B256},
};
use anyhow::Result;
use fynd_tools_common::{
    aggregator::{AggregatorCalldata, AggregatorClient, AggregatorQuote, AggregatorStatus},
    bps::{eth_call_bps_diff, gas_adjusted_bps_diff, raw_bps_diff},
    constants::ETH_SENTINEL_ADDRESS,
    swap_simulation::EthCallRunner,
};
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use num_bigint::BigUint;
use tokio::time::sleep;
use tracing::warn;

use crate::{
    audit::output::{ParticipantResult, TradeResult},
    pair_selector::PairSpec,
};

pub(crate) struct QuoteTask {
    pub(crate) block_sample: usize,
    pub(crate) pair: PairSpec,
    pub(crate) amount: String,
    pub(crate) amount_percentile_idx: usize,
}

pub(crate) struct TradeConfig {
    pub(crate) rate_limiters: Arc<HashMap<String, Arc<DefaultDirectRateLimiter>>>,
    pub(crate) retry: RetryCfg,
    pub(crate) eth_call_runner: Option<Arc<EthCallRunner>>,
    pub(crate) quote_block: Option<B256>,
    pub(crate) baseline_fee_bps: u32,
}

/// Retry policy for aggregator requests that hit a rate-limit (429) or 5xx response.
#[derive(Clone, Copy)]
pub(crate) struct RetryCfg {
    pub(crate) max_retries: u32,
    pub(crate) base_ms: u64,
}

/// Build a per-aggregator token-bucket rate limiter at `rps` requests/sec, or `None` to
/// disable pacing when `rps <= 0`.
pub(crate) fn make_rate_limiter(rps: f64) -> Result<Option<Arc<DefaultDirectRateLimiter>>> {
    if rps <= 0.0 {
        return Ok(None);
    }
    let period = Duration::from_secs_f64(1.0 / rps);
    let quota = Quota::with_period(period)
        .ok_or_else(|| anyhow::anyhow!("invalid aggregator rps {rps}: non-positive period"))?;
    Ok(Some(Arc::new(RateLimiter::direct(quota))))
}

/// Whether an aggregator status warrants a retry (transient: rate-limit or server error).
fn is_retryable(status: &AggregatorStatus) -> bool {
    match status {
        AggregatorStatus::HttpError { code, .. } => *code == 429 || *code >= 500,
        AggregatorStatus::Success |
        AggregatorStatus::NoAmount |
        AggregatorStatus::NoRoute |
        AggregatorStatus::Unavailable => false,
    }
}

/// Quote an aggregator behind its rate limiter, retrying transient failures with
/// exponential backoff + jitter so rate-limited samples are recovered, not dropped.
async fn quote_governed(
    client: &dyn AggregatorClient,
    limiter: Option<&DefaultDirectRateLimiter>,
    retry: RetryCfg,
    token_in: &str,
    token_out: &str,
    amount: &str,
    wallet: Option<&str>,
) -> Result<AggregatorQuote> {
    let mut attempt = 0u32;
    loop {
        if let Some(limiter) = limiter {
            limiter.until_ready().await;
        }
        let quote = client
            .quote(token_in, token_out, amount, wallet)
            .await?;
        if !is_retryable(&quote.status) || attempt >= retry.max_retries {
            return Ok(quote);
        }
        let backoff = retry
            .base_ms
            .saturating_mul(1u64 << attempt.min(16));
        let jitter = fastrand::u64(0..=retry.base_ms);
        sleep(Duration::from_millis(backoff.saturating_add(jitter))).await;
        attempt += 1;
    }
}

/// One participant's raw quote outcome, keyed by participant name.
type RawResult = (String, anyhow::Result<AggregatorQuote>);
/// `(eth_call_amount_out, eth_call_diff_bps, eth_call_gas_used)`.
type EthCallResult = (Option<String>, Option<f64>, Option<u64>);

/// Baseline (Fynd) figures used to compute every other participant's diffs.
struct Baseline {
    amount: BigUint,
    net_gas: Option<BigUint>,
    gas_units: u64,
    eth_call_gas: Option<u64>,
    /// `eth_call` amount scaled up by `baseline_fee_bps` (zero-fee simulation). `None` when there
    /// is no `eth_call` result or it could not be parsed.
    ec_fee_adj: Option<BigUint>,
    ok: bool,
}

pub(crate) async fn execute_trade(
    participants: Vec<Arc<dyn AggregatorClient>>,
    task: QuoteTask,
    cfg: TradeConfig,
) -> Result<TradeResult> {
    let TradeConfig { rate_limiters, retry, eth_call_runner, quote_block, baseline_fee_bps } = cfg;

    // When validation is enabled all calldata must route to the runner's sender so the
    // balance-delta check reads the right account.
    let wallet = eth_call_runner
        .as_ref()
        .map(|r| r.sender_hex());

    let raw_results =
        fire_quotes(&participants, &task, &rate_limiters, retry, wallet.as_deref()).await;
    let eth_calls = run_eth_calls(
        &raw_results,
        eth_call_runner.as_deref(),
        &task.pair.token_in,
        &task.pair.token_out,
        quote_block,
    )
    .await;

    let baseline = extract_baseline(&raw_results, &eth_calls, baseline_fee_bps);

    let participants_out: Vec<ParticipantResult> = raw_results
        .into_iter()
        .zip(eth_calls)
        .enumerate()
        .map(|(idx, ((name, result), ec))| {
            build_participant_result(idx, name, result, ec, &baseline, baseline_fee_bps)
        })
        .collect();

    Ok(TradeResult {
        block_sample: task.block_sample,
        block_hash: quote_block.map(|h| format!("0x{}", hex::encode(h.as_slice()))),
        pair: task.pair.label.clone(),
        token_in: task.pair.token_in,
        token_out: task.pair.token_out,
        amount_in: task.amount,
        amount_percentile_idx: task.amount_percentile_idx,
        participants: participants_out,
    })
}

/// Fire every participant concurrently. Each aggregator is paced by its own rate limiter
/// and retries rate-limited responses, so a slow API doesn't drop samples; Fynd (which has
/// no limiter entry) runs unthrottled.
async fn fire_quotes(
    participants: &[Arc<dyn AggregatorClient>],
    task: &QuoteTask,
    rate_limiters: &Arc<HashMap<String, Arc<DefaultDirectRateLimiter>>>,
    retry: RetryCfg,
    wallet: Option<&str>,
) -> Vec<RawResult> {
    let futs: Vec<_> = participants
        .iter()
        .map(|p| {
            let p = p.clone();
            let token_in = task.pair.token_in.clone();
            let token_out = task.pair.token_out.clone();
            let amount = task.amount.clone();
            let wallet = wallet.map(str::to_string);
            let rate_limiters = rate_limiters.clone();
            async move {
                let name = p.name().to_string();
                let limiter = rate_limiters
                    .get(&name)
                    .map(|l| l.as_ref());
                let result = quote_governed(
                    p.as_ref(),
                    limiter,
                    retry,
                    &token_in,
                    &token_out,
                    &amount,
                    wallet.as_deref(),
                )
                .await;
                (name, result)
            }
        })
        .collect();
    futures::future::join_all(futs).await
}

/// Run `eth_call` validation for every participant that carries encoded calldata.
async fn run_eth_calls(
    raw_results: &[RawResult],
    runner: Option<&EthCallRunner>,
    token_in: &str,
    token_out: &str,
    block: Option<B256>,
) -> Vec<EthCallResult> {
    futures::future::join_all(raw_results.iter().map(|(_, r)| {
        let (calldata, quoted) = match r.as_ref().ok() {
            Some(q) => (q.calldata.as_ref(), q.amount_out.as_deref()),
            None => (None, None),
        };
        run_eth_call_for_calldata(calldata, quoted, runner, token_in, token_out, block)
    }))
    .await
}

/// Derive the Fynd baseline (`participants[0]`) used for all subsequent diffs.
fn extract_baseline(
    raw_results: &[RawResult],
    eth_calls: &[EthCallResult],
    baseline_fee_bps: u32,
) -> Baseline {
    let (amount, net_gas, gas_units, eth_call_gas) = match raw_results.first() {
        Some((_, Ok(q))) if q.is_success() => (
            q.amount_out
                .as_deref()
                .and_then(parse_amount),
            q.amount_out_net_gas
                .as_deref()
                .and_then(parse_amount),
            q.gas_units.unwrap_or(0),
            eth_calls
                .first()
                .and_then(|(_, _, g)| *g),
        ),
        _ => (None, None, 0, None),
    };
    let ec_fee_adj = eth_calls
        .first()
        .and_then(|(amt, _, _)| amt.as_deref())
        .and_then(parse_amount)
        .map(|amount| apply_fee_bps(&amount, baseline_fee_bps));
    let ok = amount
        .as_ref()
        .is_some_and(|a| *a > BigUint::ZERO);
    Baseline {
        amount: amount.unwrap_or_default(),
        net_gas,
        gas_units,
        eth_call_gas,
        ec_fee_adj,
        ok,
    }
}

/// Parse an aggregator's decimal token amount into a `BigUint`, warning and returning `None`
/// when it is unparseable so a bad value never silently becomes `0`.
fn parse_amount(amount: &str) -> Option<BigUint> {
    match amount.parse::<BigUint>() {
        Ok(v) => Some(v),
        Err(_) => {
            warn!("cannot parse amount '{amount}' as an integer — skipping");
            None
        }
    }
}

/// Scale a token amount up by `fee_bps` basis points. Returns the input unchanged when
/// `fee_bps == 0`.
fn apply_fee_bps(amount: &BigUint, fee_bps: u32) -> BigUint {
    if fee_bps == 0 {
        return amount.clone();
    }
    amount * (10_000u32 + fee_bps) / 10_000u32
}

fn build_participant_result(
    idx: usize,
    name: String,
    result: anyhow::Result<AggregatorQuote>,
    eth_call: EthCallResult,
    baseline: &Baseline,
    baseline_fee_bps: u32,
) -> ParticipantResult {
    let (ec_amount, ec_diff, ec_gas) = eth_call;
    let (ec_amount, ec_diff) =
        adjust_baseline_eth_call(idx, baseline_fee_bps, ec_amount, ec_diff, result.as_ref().ok());
    let (raw_diff, gas_reported, gas_onchain) =
        compute_diffs(idx, baseline, &result, ec_amount.as_deref(), ec_gas);

    match result {
        Ok(q) => ParticipantResult {
            name,
            status: q.status.to_string(),
            amount_out: q.amount_out,
            amount_out_net_gas: q.amount_out_net_gas,
            gas_units: q.gas_units,
            protocols: q.protocols,
            route: q.route,
            num_splits: q.num_splits,
            response_time_ms: Some(q.response_time_ms),
            eth_call_amount_out: ec_amount,
            eth_call_diff_bps: ec_diff,
            eth_call_gas_used: ec_gas,
            raw_diff_bps: raw_diff,
            gas_adjusted_diff_bps_reported: gas_reported,
            gas_adjusted_diff_bps_onchain: gas_onchain,
        },
        Err(e) => {
            warn!("participant {name} request failed: {e:#}");
            ParticipantResult {
                name,
                status: format!("error: {e}"),
                amount_out: None,
                amount_out_net_gas: None,
                gas_units: None,
                protocols: vec![],
                route: None,
                num_splits: None,
                response_time_ms: None,
                eth_call_amount_out: None,
                eth_call_diff_bps: None,
                eth_call_gas_used: None,
                raw_diff_bps: None,
                gas_adjusted_diff_bps_reported: None,
                gas_adjusted_diff_bps_onchain: None,
            }
        }
    }
}

/// For the baseline (Fynd, `idx == 0`), optionally scale up its `eth_call` result to simulate
/// zero-fee output, recomputing the diff against the (unchanged) quoted `amount_out`.
fn adjust_baseline_eth_call(
    idx: usize,
    baseline_fee_bps: u32,
    ec_amount: Option<String>,
    ec_diff: Option<f64>,
    quote: Option<&AggregatorQuote>,
) -> (Option<String>, Option<f64>) {
    if idx != 0 || baseline_fee_bps == 0 {
        return (ec_amount, ec_diff);
    }
    let Some(actual) = ec_amount
        .as_deref()
        .and_then(|s| s.parse::<BigUint>().ok())
    else {
        return (ec_amount, ec_diff);
    };
    let scaled = &actual * (10_000u32 + baseline_fee_bps) / 10_000u32;
    let new_diff = quote
        .and_then(|q| q.amount_out.as_deref())
        .and_then(|s| s.parse::<BigUint>().ok())
        .filter(|q| *q > BigUint::ZERO)
        .and_then(|q| eth_call_bps_diff(&scaled, &q));
    (Some(scaled.to_string()), new_diff)
}

/// Compute the three diffs (raw, gas-adjusted reported, gas-adjusted on-chain) for a
/// non-baseline participant. All `None` for the baseline or when the baseline is missing.
fn compute_diffs(
    idx: usize,
    baseline: &Baseline,
    result: &anyhow::Result<AggregatorQuote>,
    ec_amount: Option<&str>,
    ec_gas: Option<u64>,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    if idx == 0 || !baseline.ok {
        return (None, None, None);
    }
    let Ok(q) = result else {
        return (None, None, None);
    };
    if !q.is_success() {
        return (None, None, None);
    }
    let Some(other_raw) = q
        .amount_out
        .as_deref()
        .and_then(parse_amount)
    else {
        return (None, None, None);
    };
    let net_gas = baseline.net_gas.as_ref();

    let raw = raw_bps_diff(&baseline.amount, &other_raw);
    let gas_reported = gas_adjusted_bps_diff(
        &baseline.amount,
        net_gas,
        baseline.gas_units,
        &other_raw,
        q.gas_units.unwrap_or(0),
    );
    let gas_onchain = match (baseline.ec_fee_adj.as_ref(), ec_amount.and_then(parse_amount)) {
        (Some(ec_fee_adj), Some(ec_amount)) => gas_adjusted_bps_diff(
            ec_fee_adj,
            net_gas,
            baseline.eth_call_gas.unwrap_or(0),
            &ec_amount,
            ec_gas.unwrap_or(0),
        ),
        _ => None,
    };
    (raw, gas_reported, gas_onchain)
}

/// Decode a 0x-prefixed hex string into a 20-byte `Address`, returning `None` on any error.
fn parse_address_hex(addr_hex: &str) -> Option<Address> {
    let bytes = hex::decode(addr_hex.trim_start_matches("0x")).ok()?;
    (bytes.len() == 20).then(|| Address::from_slice(&bytes))
}

/// Run `eth_call` validation for a participant that carries encoded calldata.
///
/// Returns `(eth_call_amount_out, eth_call_diff_bps, eth_call_gas_used)`. All `None` when
/// the runner is absent, calldata is absent, addresses are invalid, or the call reverts.
async fn run_eth_call_for_calldata(
    calldata: Option<&AggregatorCalldata>,
    quoted_amount: Option<&str>,
    runner: Option<&EthCallRunner>,
    token_in_hex: &str,
    token_out_hex: &str,
    block: Option<B256>,
) -> EthCallResult {
    let Some(runner) = runner else {
        return (None, None, None);
    };
    let Some(block) = block else {
        return (None, None, None);
    };
    let Some(cd) = calldata else {
        return (None, None, None);
    };

    let Some(token_in) = parse_address_hex(token_in_hex) else {
        warn!("eth_call: invalid token_in address {token_in_hex}");
        return (None, None, None);
    };
    let Some(token_out) = parse_address_hex(token_out_hex) else {
        warn!("eth_call: invalid token_out address {token_out_hex}");
        return (None, None, None);
    };

    // Aggregators use 0xeeee…eeee as a sentinel for native ETH; normalise to zero address
    // so the ERC-20 slot probe is skipped and the ETH balance path is used.
    let token_in = if token_in == ETH_SENTINEL_ADDRESS { Address::ZERO } else { token_in };
    let token_out = if token_out == ETH_SENTINEL_ADDRESS { Address::ZERO } else { token_out };

    let Some(router) = parse_address_hex(&cd.to) else {
        warn!("eth_call: invalid router address '{}'", cd.to);
        return (None, None, None);
    };

    let calldata_bytes = hex::decode(cd.data.trim_start_matches("0x")).unwrap_or_default();
    let value: BigUint = cd.value.parse().unwrap_or_default();

    match runner
        .execute(token_in, token_out, router, &calldata_bytes, &value, block)
        .await
    {
        Ok(Some((actual, gas_used))) => {
            let diff_bps = quoted_amount
                .and_then(|q| q.parse::<BigUint>().ok())
                .filter(|q| *q > BigUint::ZERO)
                .and_then(|q| eth_call_bps_diff(&actual, &q));
            (Some(actual.to_string()), diff_bps, gas_used)
        }
        Ok(None) => (None, None, None),
        Err(e) => {
            warn!("eth_call validation failed: {e}");
            (None, None, None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;

    /// Aggregator stub that returns `HttpError{429}` for its first `fails` calls, then
    /// `Success`, while counting how many times `quote` was invoked.
    struct FlakyAggregator {
        remaining_fails: AtomicUsize,
        calls: AtomicUsize,
    }

    fn quote_with_status(status: AggregatorStatus) -> AggregatorQuote {
        AggregatorQuote {
            status,
            amount_out: None,
            amount_out_net_gas: None,
            gas_units: None,
            protocols: Vec::new(),
            num_splits: None,
            response_time_ms: 0,
            calldata: None,
            route: None,
        }
    }

    #[async_trait]
    impl AggregatorClient for FlakyAggregator {
        fn name(&self) -> &str {
            "flaky"
        }

        async fn quote(
            &self,
            _token_in: &str,
            _token_out: &str,
            _amount: &str,
            _wallet: Option<&str>,
        ) -> Result<AggregatorQuote> {
            self.calls
                .fetch_add(1, Ordering::SeqCst);
            if self
                .remaining_fails
                .load(Ordering::SeqCst) >
                0
            {
                self.remaining_fails
                    .fetch_sub(1, Ordering::SeqCst);
                return Ok(quote_with_status(AggregatorStatus::HttpError {
                    code: 429,
                    snippet: "rate limited".to_string(),
                }));
            }
            Ok(quote_with_status(AggregatorStatus::Success))
        }
    }

    fn fast_retry() -> RetryCfg {
        RetryCfg { max_retries: 3, base_ms: 0 }
    }

    #[tokio::test]
    async fn quote_governed_retries_then_succeeds() {
        let client =
            FlakyAggregator { remaining_fails: AtomicUsize::new(2), calls: AtomicUsize::new(0) };
        let quote = quote_governed(&client, None, fast_retry(), "a", "b", "1", None)
            .await
            .expect("quote");
        assert_eq!(quote.status, AggregatorStatus::Success);
        assert_eq!(client.calls.load(Ordering::SeqCst), 3); // 2 failures + 1 success
    }

    #[tokio::test]
    async fn quote_governed_gives_up_after_max_retries() {
        let client =
            FlakyAggregator { remaining_fails: AtomicUsize::new(10), calls: AtomicUsize::new(0) };
        let quote = quote_governed(&client, None, fast_retry(), "a", "b", "1", None)
            .await
            .expect("quote");
        match quote.status {
            AggregatorStatus::HttpError { code, .. } => assert_eq!(code, 429),
            other => panic!("expected HttpError, got {other:?}"),
        }
        assert_eq!(client.calls.load(Ordering::SeqCst), 4); // initial + 3 retries
    }

    #[test]
    fn make_rate_limiter_disabled_for_non_positive_rps() {
        assert!(make_rate_limiter(0.0)
            .expect("ok")
            .is_none());
        assert!(make_rate_limiter(-1.0)
            .expect("ok")
            .is_none());
        assert!(make_rate_limiter(5.0)
            .expect("ok")
            .is_some());
    }

    #[test]
    fn is_retryable_classifies_transient_statuses() {
        assert!(is_retryable(&AggregatorStatus::HttpError { code: 429, snippet: String::new() }));
        assert!(is_retryable(&AggregatorStatus::HttpError { code: 503, snippet: String::new() }));
        assert!(!is_retryable(&AggregatorStatus::HttpError { code: 400, snippet: String::new() }));
        assert!(!is_retryable(&AggregatorStatus::Success));
        assert!(!is_retryable(&AggregatorStatus::NoRoute));
        assert!(!is_retryable(&AggregatorStatus::Unavailable));
    }
}

use std::time::Duration;

use alloy::{
    primitives::{Address, Bytes},
    rpc::{
        client::RpcClient,
        json_rpc::ErrorPayload,
        types::simulate::{SimCallResult, SimulatedBlock},
    },
    transports::mock::Asserter,
};
use num_bigint::BigUint;

use super::*;
use crate::simulation::erc20_slots::MappingPosition;

#[test]
fn test_success_reports_amount_out_and_gas_used() {
    let result = SimulationAttempt::Success { amount_out: BigUint::from(42_u8), gas_used: 123_456 }
        .into_result();
    assert!(
        matches!(result, SimulationResult::Success { amount_out, gas_used } if amount_out == BigUint::from(42_u8) && gas_used == 123_456)
    );
}

#[test]
fn test_revert_reports_no_gas() {
    let result = SimulationAttempt::Reverted { reason: "no liquidity".to_string() }.into_result();
    assert!(matches!(result, SimulationResult::Failure { reason } if reason == "no liquidity"));
}

#[test]
fn test_decode_revert_reason_decodes_error_string() {
    let mut data = Error::SELECTOR.to_vec();
    Error("no liquidity".to_string()).abi_encode_raw(&mut data);
    assert_eq!(decode_revert_reason(Some(&data), "node error"), "reverted: no liquidity");
}

#[test]
fn test_decode_revert_reason_keeps_unknown_data() {
    assert_eq!(decode_revert_reason(Some(&[1, 2, 3]), "node error"), "reverted with data 0x010203");
}

#[test]
fn test_decode_revert_reason_keeps_node_message_without_data() {
    assert_eq!(decode_revert_reason(None, "node error"), "node error");
}

#[test]
fn test_token_overrides_fund_both_holders_and_both_spenders() {
    let sender = Address::repeat_byte(1);
    let router = Address::repeat_byte(2);
    let token = Address::repeat_byte(3);
    let permit2 = Address::repeat_byte(4);
    let positions =
        Erc20SlotPositions::new(MappingPosition::Standard(5), MappingPosition::Standard(6));
    let overrides = token_overrides(sender, token, router, permit2, positions);
    let state_diff = overrides
        .get(&token)
        .and_then(|override_| override_.state_diff.as_ref())
        .expect("token state diff");

    // A `transfer_from` route pulls from the sender and a `use_vaults_funds` one from the router,
    // so both hold a balance and the sender approves both spenders a route can name.
    assert!(state_diff.contains_key(&positions.balance_slot(sender)));
    assert!(state_diff.contains_key(&positions.balance_slot(router)));
    assert!(state_diff.contains_key(&positions.allowance_slot(sender, router)));
    assert!(state_diff.contains_key(&positions.allowance_slot(sender, permit2)));
    assert_eq!(state_diff.len(), 4);
}

/// An envelope for the tests that drive the call directly, without a quote to derive one from.
fn test_envelope() -> SimulationEnvelope {
    SimulationEnvelope { gas_limit: SIMULATION_MIN_GAS_LIMIT, gas_price: 1 }
}

#[test]
fn test_envelope_doubles_the_estimated_gas() {
    let envelope = SimulationEnvelope::new(Some(1_000_000), Some(7));
    assert_eq!(envelope.gas_limit, 2_000_000);
    assert_eq!(envelope.gas_price, 7);
}

#[test]
fn test_envelope_raises_a_small_estimate_to_the_floor() {
    let envelope = SimulationEnvelope::new(Some(1_000), Some(7));
    assert_eq!(envelope.gas_limit, SIMULATION_MIN_GAS_LIMIT);
}

#[test]
fn test_envelope_caps_a_large_estimate() {
    let envelope = SimulationEnvelope::new(Some(u64::MAX), Some(7));
    assert_eq!(envelope.gas_limit, SIMULATION_MAX_GAS_LIMIT);
}

#[test]
fn test_envelope_prices_a_quote_that_carries_no_gas_price() {
    let envelope = SimulationEnvelope::new(None, None);
    assert_eq!(envelope.gas_limit, SIMULATION_MIN_GAS_LIMIT);
    assert_eq!(envelope.gas_price, SIMULATION_FALLBACK_GAS_PRICE);
}

#[test]
fn test_block_overrides_leave_nothing_a_pool_reads_at_zero() {
    let overrides = block_overrides();
    assert_eq!(overrides.coinbase, Some(SIMULATION_COINBASE));
    assert_eq!(overrides.gas_limit, Some(SIMULATION_BLOCK_GAS_LIMIT));
    assert_ne!(overrides.random, Some(B256::ZERO));
    assert!(overrides.random.is_some());
}

#[test]
fn test_sender_override_funds_a_used_account() {
    let sender = sender_override();
    assert_eq!(sender.balance, Some(SIMULATION_FUNDING_VALUE));
    assert_eq!(sender.nonce, Some(SIMULATION_SENDER_NONCE));
}

fn mocked_simulator(asserter: &Asserter, timeout: Duration) -> QuoteSimulator {
    QuoteSimulator::with_provider(
        RootProvider::new(RpcClient::mocked(asserter.clone())),
        Address::repeat_byte(9),
        timeout,
    )
}

fn simulated_response(return_data: Vec<u8>, status: bool, gas_used: u64) -> Vec<SimulatedBlock> {
    vec![SimulatedBlock {
        inner: Default::default(),
        calls: vec![SimCallResult {
            return_data: Bytes::from(return_data),
            gas_used,
            status,
            ..Default::default()
        }],
    }]
}

#[tokio::test]
async fn test_simulate_call_against_mocked_provider() {
    let asserter = Asserter::new();
    asserter.push_success(&simulated_response(
        U256::from(123_u64)
            .to_be_bytes::<32>()
            .to_vec(),
        true,
        87_654,
    ));
    let result = simulate_with_overrides(
        &RootProvider::new(RpcClient::mocked(asserter)),
        Address::repeat_byte(1),
        Address::repeat_byte(2),
        U256::ZERO,
        &[0x12],
        native_balance_override(Address::repeat_byte(1)),
        test_envelope(),
    )
    .await
    .into_result();
    assert!(
        matches!(result, SimulationResult::Success { amount_out, gas_used } if amount_out == BigUint::from(123_u64) && gas_used == 87_654)
    );
}

#[tokio::test]
async fn test_simulated_call_rejects_non_uint256_return_data() {
    let asserter = Asserter::new();
    asserter.push_success(&simulated_response(vec![0; 31], true, 1));
    let result = simulate_with_overrides(
        &RootProvider::new(RpcClient::mocked(asserter)),
        Address::repeat_byte(1),
        Address::repeat_byte(2),
        U256::ZERO,
        &[],
        native_balance_override(Address::repeat_byte(1)),
        test_envelope(),
    )
    .await
    .into_result();
    assert!(matches!(result, SimulationResult::Failure { reason } if reason.contains("31 bytes")));
}

#[tokio::test]
async fn test_simulated_call_decodes_revert_data_from_mocked_rpc_error() {
    let asserter = Asserter::new();
    let mut revert_data = Error::SELECTOR.to_vec();
    Error("insufficient output".to_string()).abi_encode_raw(&mut revert_data);
    asserter.push_failure(ErrorPayload::internal_error_with_message_and_obj(
        "execution reverted".into(),
        serde_json::value::to_raw_value(&format!("0x{}", alloy::hex::encode(&revert_data)))
            .expect("revert data serializes"),
    ));
    let result = simulate_with_overrides(
        &RootProvider::new(RpcClient::mocked(asserter)),
        Address::repeat_byte(1),
        Address::repeat_byte(2),
        U256::ZERO,
        &[],
        native_balance_override(Address::repeat_byte(1)),
        test_envelope(),
    )
    .await
    .into_result();
    assert!(
        matches!(result, SimulationResult::Failure { reason } if reason.contains("reverted: insufficient output"))
    );
}

#[tokio::test]
async fn test_slot_cache_reuses_resolved_positions_without_more_probes() {
    let asserter = Asserter::new();
    for _ in 0..44 {
        asserter.push_success(&Bytes::from(
            B256::from(U256::from_limbs([0xdead_beef, 0, 0, 0]))
                .as_slice()
                .to_vec(),
        ));
    }
    let simulator = mocked_simulator(&asserter, Duration::from_secs(1));
    let token = Address::repeat_byte(3);
    simulator
        .cached_positions(token, Address::repeat_byte(1), Address::repeat_byte(2))
        .await
        .expect("probes resolve slots");
    assert!(asserter.read_q().is_empty());
    simulator
        .cached_positions(token, Address::repeat_byte(1), Address::repeat_byte(2))
        .await
        .expect("cache resolves slots");
    assert!(asserter.read_q().is_empty());
}

/// A server that accepts the connection and never answers, so the request outlives the timeout.
async fn unresponsive_rpc_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a local listener");
    let address = listener
        .local_addr()
        .expect("read the listener address");
    tokio::spawn(async move {
        let mut accepted = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            accepted.push(stream);
        }
    });
    format!("http://{address}")
}

#[tokio::test]
async fn test_simulation_times_out_when_the_node_does_not_answer() {
    let simulator = QuoteSimulator::new(
        &unresponsive_rpc_url().await,
        Chain::Ethereum,
        Duration::from_millis(50),
    )
    .expect("build a simulator against the local listener");

    let attempt = simulator
        .simulate_within_timeout(
            Address::repeat_byte(1),
            Address::repeat_byte(2),
            U256::ZERO,
            &[0x12],
            native_balance_override(Address::repeat_byte(1)),
            test_envelope(),
        )
        .await;

    assert!(
        matches!(attempt.into_result(), SimulationResult::Failure { reason } if reason.contains("timed out"))
    );
}

/// Router fee the fixture charges, in output-token units.
const FIXTURE_ROUTER_FEE: u64 = 7_000;

/// Client fee the fixture charges, in output-token units.
const FIXTURE_CLIENT_FEE: u64 = 3_000;

/// A quote whose raw output is `RAW_AMOUNT_OUT` and whose fees leave `after_fees` receivable.
///
/// `max_slippage` is non-zero, so a baseline that used `min_amount_received` on its own would fail
/// these tests rather than pass them.
fn quote_with_fees(after_fees: u64) -> OrderQuote {
    let slippage = after_fees / 100;
    let mut quote = quote_without_fees();
    quote.set_amount_out(BigUint::from(after_fees + FIXTURE_ROUTER_FEE + FIXTURE_CLIENT_FEE));
    quote.set_fee_breakdown(crate::FeeBreakdown::new(
        BigUint::from(FIXTURE_ROUTER_FEE),
        BigUint::from(FIXTURE_CLIENT_FEE),
        BigUint::from(slippage),
        BigUint::from(after_fees - slippage),
    ));
    quote
}

/// The same quote before encoding computes its fees.
fn quote_without_fees() -> OrderQuote {
    OrderQuote::new(
        "test-order".to_string(),
        crate::QuoteStatus::Success,
        BigUint::from(1_000u64),
        BigUint::from(1_000_000u64),
        BigUint::from(50_000u64),
        BigUint::from(1_000_000u64),
        crate::BlockInfo::new(1, "0x1".to_string(), 1),
        "test_algorithm".to_string(),
        tycho_simulation::tycho_common::Bytes::from(vec![0xAA; 20]),
        tycho_simulation::tycho_common::Bytes::from(vec![0xAA; 20]),
        "1".to_string(),
    )
}

#[test]
fn test_deviation_bps_simulated_below_quote() {
    let deviation = deviation_bps(&quote_with_fees(1_000_000), &BigUint::from(999_000u64))
        .expect("a quote carrying fees has a baseline");

    assert!((deviation - -10.0).abs() < 1e-9, "got {deviation}");
}

#[test]
fn test_deviation_bps_simulated_above_quote() {
    let deviation = deviation_bps(&quote_with_fees(1_000_000), &BigUint::from(1_001_000u64))
        .expect("a quote carrying fees has a baseline");

    assert!((deviation - 10.0).abs() < 1e-9, "got {deviation}");
}

/// The router returns the output after it takes its fees, so the baseline must be the quoted
/// amount less those fees. Comparing against the raw `amount_out` would read this as a shortfall
/// the size of the fee.
#[test]
fn test_deviation_bps_excludes_the_router_fee_from_the_baseline() {
    let quote = quote_with_fees(1_000_000);
    let after_fees = quote
        .fee_breakdown()
        .expect("the quote carries fees")
        .min_amount_received() +
        quote
            .fee_breakdown()
            .expect("the quote carries fees")
            .max_slippage();

    let deviation =
        deviation_bps(&quote, &after_fees).expect("a quote carrying fees has a baseline");

    assert!(deviation.abs() < 1e-9, "a simulation matching the post-fee quote is not a deviation");
    assert!(after_fees < *quote.amount_out(), "the baseline sits below the raw swap output");
}

#[test]
fn test_deviation_bps_without_a_fee_breakdown() {
    assert_eq!(deviation_bps(&quote_without_fees(), &BigUint::from(1_000u64)), None);
}

#[test]
fn test_deviation_bps_zero_quoted_amount() {
    let mut quote = quote_with_fees(1_000_000);
    quote.set_fee_breakdown(crate::FeeBreakdown::new(
        BigUint::ZERO,
        BigUint::ZERO,
        BigUint::ZERO,
        BigUint::ZERO,
    ));

    assert_eq!(deviation_bps(&quote, &BigUint::from(1_000u64)), None);
}

/// An amount past `f64` saturates to infinity rather than failing to convert, so the ratio is
/// rejected on being non-finite. Recording it would put `inf` in the histogram.
#[test]
fn test_deviation_bps_amount_beyond_f64() {
    let huge = BigUint::from(1u8) << 2_000;

    assert_eq!(deviation_bps(&quote_with_fees(1_000_000), &huge), None);
}

/// Names every metric a run recorded, with its labels and value.
fn recorded_metrics(
    snapshotter: &metrics_util::debugging::Snapshotter,
) -> Vec<(String, Vec<String>, metrics_util::debugging::DebugValue)> {
    snapshotter
        .snapshot()
        .into_vec()
        .into_iter()
        .map(|(key, _, _, value)| {
            (
                key.key().name().to_string(),
                key.key()
                    .labels()
                    .map(|label| format!("{}={}", label.key(), label.value()))
                    .collect(),
                value,
            )
        })
        .collect()
}

#[test]
fn test_record_outcome_success() {
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();

    metrics::with_local_recorder(&recorder, || {
        record_outcome(
            &quote_with_fees(1_000_000),
            &SimulationAttempt::Success {
                amount_out: BigUint::from(999_000u64),
                gas_used: 120_000,
            },
        );
    });

    let recorded = recorded_metrics(&snapshotter);
    let counted = recorded
        .iter()
        .find(|(name, ..)| name == "quote_simulations_total")
        .expect("the outcome is counted");
    assert!(
        counted
            .1
            .contains(&"outcome=success".to_string()),
        "{:?}",
        counted.1
    );
    assert!(
        counted
            .1
            .contains(&"algorithm=test_algorithm".to_string()),
        "{:?}",
        counted.1
    );

    let (.., deviation) = recorded
        .iter()
        .find(|(name, ..)| name == "quote_simulation_deviation_bps")
        .expect("a successful simulation records its deviation");
    assert!(
        matches!(deviation, metrics_util::debugging::DebugValue::Histogram(values)
            if values.len() == 1 && (values[0].into_inner() - -10.0).abs() < 1e-9),
        "{deviation:?}"
    );
}

#[test]
fn test_record_outcome_reverted() {
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();

    metrics::with_local_recorder(&recorder, || {
        record_outcome(
            &quote_with_fees(1_000_000),
            &SimulationAttempt::Reverted { reason: "reverted".to_string() },
        );
    });

    let recorded = recorded_metrics(&snapshotter);
    assert!(recorded
        .iter()
        .any(|(name, labels, _)| name == "quote_simulations_total" &&
            labels.contains(&"outcome=reverted".to_string())));
    assert!(
        !recorded
            .iter()
            .any(|(name, ..)| name == "quote_simulation_deviation_bps"),
        "a call that returned no amount has no deviation to record"
    );
}

#[test]
fn test_record_outcome_failed() {
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();

    metrics::with_local_recorder(&recorder, || {
        record_outcome(
            &quote_with_fees(1_000_000),
            &SimulationAttempt::Failure { reason: "timed out".to_string() },
        );
    });

    assert!(recorded_metrics(&snapshotter)
        .iter()
        .any(|(name, labels, _)| name == "quote_simulations_total" &&
            labels.contains(&"outcome=failed".to_string())));
}

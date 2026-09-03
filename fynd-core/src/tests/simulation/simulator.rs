use std::time::Duration;

use alloy::{
    primitives::{Address, Bytes},
    rpc::{
        client::RpcClient,
        json_rpc::ErrorPayload,
        types::simulate::{SimCallResult, SimulatedBlock},
    },
    sol_types::SolError,
    transports::mock::Asserter,
};
use num_bigint::BigUint;

use super::*;
use crate::simulation::{
    deviation::fixtures::quote_with_fees,
    token_layout::{KeyOrder, MappingPosition, TokenLayout},
};

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
fn test_token_overrides_fund_both_holders_and_both_spenders() {
    let sender = Address::repeat_byte(1);
    let router = Address::repeat_byte(2);
    let token = Address::repeat_byte(3);
    let permit2 = Address::repeat_byte(4);
    let balance = MappingPosition::Direct { base: 5, key_order: KeyOrder::Solidity };
    let allowance = MappingPosition::Direct { base: 6, key_order: KeyOrder::Solidity };
    let layout = TokenLayout::new(token, balance, allowance);
    let overrides = token_overrides(sender, router, permit2, layout);
    let state_diff = overrides
        .get(&token)
        .and_then(|override_| override_.state_diff.as_ref())
        .expect("token state diff");

    // A `transfer_from` route pulls from the sender and a `use_vaults_funds` one from the router,
    // so both hold a balance and the sender approves both spenders a route can name.
    assert!(state_diff.contains_key(&layout.balance_slot(sender)));
    assert!(state_diff.contains_key(&layout.balance_slot(router)));
    assert!(state_diff.contains_key(&layout.allowance_slot(sender, router)));
    assert!(state_diff.contains_key(&layout.allowance_slot(sender, permit2)));
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

/// The funding value is what makes a simulated sender solvent, and it is bounded on both sides:
/// too small starves a large trade, too large overflows a rebasing token's balance arithmetic.
#[test]
fn test_funding_value_is_ten_to_the_thirty_sixth() {
    let ten = U256::from(10_u8);
    assert_eq!(SIMULATION_FUNDING_VALUE, ten.pow(U256::from(36_u8)));
    // Room left above the value, so a token that packs flags into the balance word still reads
    // it back unchanged.
    assert!(SIMULATION_FUNDING_VALUE < U256::MAX >> 128);
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
    .await;
    assert!(
        matches!(result, CallOutcome::Success { amount_out, gas_used } if amount_out == BigUint::from(123_u64) && gas_used == 87_654)
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
    .await;
    assert!(matches!(result, CallOutcome::Failure(reason) if reason.contains("31 bytes")));
}

#[tokio::test]
async fn test_simulated_call_decodes_revert_data_from_mocked_rpc_error() {
    let asserter = Asserter::new();
    let revert_data = crate::simulation::revert::SolidityErrors::Error {
        reason: "insufficient output".to_string(),
    }
    .abi_encode();
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
    .await;
    assert!(
        matches!(result, CallOutcome::Failure(reason) if reason.contains("reverted: insufficient output"))
    );
}

#[tokio::test]
async fn test_layout_cache_reuses_a_resolved_layout() {
    let asserter = Asserter::new();
    let simulator = mocked_simulator(&asserter, Duration::from_secs(1));
    let token = Address::repeat_byte(3);
    let layout = TokenLayout::new(
        token,
        MappingPosition::Direct { base: 0, key_order: KeyOrder::Solidity },
        MappingPosition::Direct { base: 1, key_order: KeyOrder::Solidity },
    );
    simulator
        .layout_cache
        .lock()
        .expect("layout cache lock")
        .insert(token, Ok(layout));

    let resolved = simulator
        .cached_layout(token, Address::repeat_byte(1), Address::repeat_byte(2))
        .await
        .expect("cache resolves layout");

    assert_eq!(resolved, layout);
    // The mock queues no response, so a discovery attempt would fail rather than pass silently.
    assert!(asserter.read_q().is_empty(), "a cached layout makes no RPC call");
}

/// A token this build cannot resolve is remembered as such. Rediscovering it would spend a trace
/// and up to 64 `eth_call`s on every quote that touches it.
#[tokio::test]
async fn test_layout_cache_reuses_an_unresolvable_verdict() {
    let asserter = Asserter::new();
    let simulator = mocked_simulator(&asserter, Duration::from_secs(1));
    let token = Address::repeat_byte(4);
    simulator
        .layout_cache
        .lock()
        .expect("layout cache lock")
        .insert(token, Err("no supported balance mapping".to_string()));

    let error = simulator
        .cached_layout(token, Address::repeat_byte(1), Address::repeat_byte(2))
        .await
        .expect_err("the token stays unresolvable");

    assert!(error.contains("no supported balance mapping"), "{error}");
    assert!(asserter.read_q().is_empty(), "a remembered verdict makes no RPC call");
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

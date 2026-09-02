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

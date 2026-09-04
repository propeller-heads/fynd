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
use crate::{
    simulation::{
        deviation::fixtures::quote_with_fees,
        token_layout::{KeyOrder, MappingPosition, TokenLayout, PROBE_SENTINEL},
    },
    tests::metrics::recorded_metrics,
};

/// A budget long enough that a mocked provider, which answers at once, never meets it.
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

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

/// `eth_simulateV1` numbers its own block, and `debug_traceCall` at latest runs one below it, so
/// the trace has to be pinned to the block the simulation reported. Predicting that number instead
/// makes the node refuse the call outright once a block lands mid-quote.
#[test]
fn test_executed_in_pins_the_block_without_touching_the_rest() {
    let base = block_overrides();
    let pinned = executed_in(base.clone(), 25_903_761, 1_788_531_000);

    assert_eq!(pinned.number, Some(U256::from(25_903_761_u64)));
    assert_eq!(pinned.time, Some(1_788_531_000));
    assert_eq!(pinned.coinbase, base.coinbase);
    assert_eq!(pinned.gas_limit, base.gas_limit);
    assert_eq!(pinned.random, base.random);
}

/// The simulation itself must leave the number unset, or the node rejects a block that does not
/// sit above the head.
#[test]
fn test_block_overrides_leave_the_block_for_the_node_to_number() {
    let overrides = block_overrides();

    assert_eq!(overrides.number, None);
    assert_eq!(overrides.time, None);
}

/// The funding value is what makes a simulated sender solvent, and it is bounded on both sides:
/// too small starves a large trade, too large overflows a rebasing token's balance arithmetic.
#[test]
fn test_funding_value_bounds() {
    // Above any practical input at 18 decimals.
    assert!(SIMULATION_FUNDING_VALUE > U256::from(10_u8).pow(U256::from(30_u8)));
    // Room left above the value, so a token that packs flags into the balance word still reads it
    // back unchanged, and a rebasing token's multiplication does not overflow.
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
        SimulatedCall {
            sender: Address::repeat_byte(1),
            router: Address::repeat_byte(2),
            value: U256::ZERO,
            data: &[0x12],
        },
        native_balance_override(Address::repeat_byte(1)),
        test_envelope(),
        TEST_TIMEOUT,
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
        SimulatedCall {
            sender: Address::repeat_byte(1),
            router: Address::repeat_byte(2),
            value: U256::ZERO,
            data: &[],
        },
        native_balance_override(Address::repeat_byte(1)),
        test_envelope(),
        TEST_TIMEOUT,
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
        SimulatedCall {
            sender: Address::repeat_byte(1),
            router: Address::repeat_byte(2),
            value: U256::ZERO,
            data: &[],
        },
        native_balance_override(Address::repeat_byte(1)),
        test_envelope(),
        TEST_TIMEOUT,
    )
    .await;
    assert!(
        matches!(result, CallOutcome::Failure(reason) if reason.contains("reverted: insufficient output"))
    );
}

/// A prestate trace naming one account and the slots its read touched.
fn prestate(contract: Address, slots: &[B256]) -> serde_json::Value {
    let storage: serde_json::Map<String, serde_json::Value> = slots
        .iter()
        .map(|slot| (format!("{slot:#x}"), serde_json::json!(format!("{:#x}", B256::ZERO))))
        .collect();
    serde_json::json!({ format!("{contract:#x}"): { "storage": storage } })
}

/// Queues one full discovery: a balance trace and probe, then an allowance trace and probe.
fn push_successful_discovery(
    asserter: &Asserter,
    token: Address,
    holder: Address,
    spender: Address,
) {
    let layout = TokenLayout::new(
        token,
        MappingPosition::Direct { base: 0, key_order: KeyOrder::Solidity },
        MappingPosition::Direct { base: 0, key_order: KeyOrder::Solidity },
    );
    let sentinel = Bytes::from(B256::from(PROBE_SENTINEL).to_vec());
    asserter.push_success(&prestate(token, &[layout.balance_slot(holder)]));
    asserter.push_success(&sentinel);
    asserter.push_success(&prestate(token, &[layout.allowance_slot(holder, spender)]));
    asserter.push_success(&sentinel);
}

#[tokio::test]
async fn test_layout_cache_reuses_a_resolved_layout() {
    let asserter = Asserter::new();
    let simulator = mocked_simulator(&asserter, TEST_TIMEOUT);
    let token = Address::repeat_byte(3);
    let holder = Address::repeat_byte(1);
    let spender = Address::repeat_byte(2);
    push_successful_discovery(&asserter, token, holder, spender);

    let first = simulator
        .cached_layout(token, holder, spender)
        .await
        .expect("discovery resolves the layout");
    let second = simulator
        .cached_layout(token, holder, spender)
        .await
        .expect("the second call reads the cache");

    assert_eq!(first, second);
    // The mock has nothing left queued, so a second discovery would have failed rather than
    // passed silently.
    assert!(asserter.read_q().is_empty(), "a cached layout makes no RPC call");
}

/// A token this build cannot resolve is remembered as such. Rediscovering it would spend a trace
/// and its probes on every quote that touches it.
#[tokio::test]
async fn test_layout_cache_remembers_an_unsupported_token() {
    let asserter = Asserter::new();
    let simulator = mocked_simulator(&asserter, TEST_TIMEOUT);
    // Both balance views name a slot no convention produces, so recovery fails on the token
    // itself rather than on the node.
    for _ in 0..2 {
        asserter.push_success(&prestate(Address::repeat_byte(3), &[B256::repeat_byte(0x99)]));
        asserter.push_success(&Bytes::from(B256::from(PROBE_SENTINEL).to_vec()));
    }

    let first = simulator
        .cached_layout(Address::repeat_byte(3), Address::repeat_byte(1), Address::repeat_byte(2))
        .await
        .expect_err("the token's layout is not one this build recovers");
    let second = simulator
        .cached_layout(Address::repeat_byte(3), Address::repeat_byte(1), Address::repeat_byte(2))
        .await
        .expect_err("the verdict is remembered");

    assert!(first.contains("could not recover"), "{first}");
    assert_eq!(first, second);
    assert!(asserter.read_q().is_empty(), "a remembered verdict makes no RPC call");
}

/// A node that failed to answer says nothing about the token, so the next quote discovers again.
/// Remembering it would disable simulation for that token until the process restarts.
#[tokio::test]
async fn test_layout_cache_retries_after_a_node_failure() {
    let asserter = Asserter::new();
    let simulator = mocked_simulator(&asserter, TEST_TIMEOUT);
    let token = Address::repeat_byte(3);
    let holder = Address::repeat_byte(1);
    let spender = Address::repeat_byte(2);
    asserter.push_failure(ErrorPayload {
        code: -32005,
        message: "limit exceeded".into(),
        data: None,
    });
    push_successful_discovery(&asserter, token, holder, spender);

    let refused = simulator
        .cached_layout(token, holder, spender)
        .await
        .expect_err("the node refused the trace");
    let resolved = simulator
        .cached_layout(token, holder, spender)
        .await;

    assert!(refused.contains("discovery failed"), "{refused}");
    assert!(resolved.is_ok(), "the retry resolves: {resolved:?}");
}

/// A reverting call frame carrying revert data, as the call tracer reports it.
fn reverting_frame(output: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "from": format!("{:#x}", Address::repeat_byte(1)),
        "to": format!("{:#x}", Address::repeat_byte(2)),
        "gas": "0x0",
        "gasUsed": "0x0",
        "input": "0x",
        "output": format!("0x{}", alloy::hex::encode(output)),
        "error": "execution reverted",
        "type": "CALL",
    })
}

async fn simulate_reverting_call(asserter: Asserter) -> SimulationAttempt {
    mocked_simulator(&asserter, TEST_TIMEOUT)
        .simulate_within_timeout(
            SimulatedCall {
                sender: Address::repeat_byte(1),
                router: Address::repeat_byte(2),
                value: U256::ZERO,
                data: &[0x12],
            },
            native_balance_override(Address::repeat_byte(1)),
            test_envelope(),
        )
        .await
}

/// `eth_simulateV1` drops the revert payload, so a reverted call arrives saying only that it
/// reverted. Replaying it under the tracer is what turns that into a named error, and this is the
/// path the whole trace exists for.
#[tokio::test]
async fn test_simulate_names_a_revert_the_node_reported_without_a_payload() {
    let asserter = Asserter::new();
    asserter.push_success(&simulated_response(Vec::new(), false, 0));
    asserter.push_success(&reverting_frame(
        &crate::simulation::revert::RouterErrors::TychoRouter__EmptySwaps {}.abi_encode(),
    ));

    let attempt = simulate_reverting_call(asserter).await;

    assert!(
        matches!(attempt.into_result(), SimulationResult::Failure { reason }
            if reason.contains("TychoRouter__EmptySwaps")),
        "the traced error names the revert"
    );
}

/// A trace the node cannot serve leaves the message it already gave, so a revert is still
/// reported as a revert rather than swallowed.
#[tokio::test]
async fn test_simulate_keeps_the_node_message_when_the_trace_fails() {
    let asserter = Asserter::new();
    asserter.push_success(&simulated_response(Vec::new(), false, 0));
    asserter.push_failure(ErrorPayload {
        code: -32601,
        message: "the method debug_traceCall does not exist".into(),
        data: None,
    });

    let attempt = simulate_reverting_call(asserter).await;

    assert!(
        matches!(attempt.into_result(), SimulationResult::Failure { reason }
            if reason == "simulation reverted: execution reverted"),
        "the node's own message survives"
    );
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
            SimulatedCall {
                sender: Address::repeat_byte(1),
                router: Address::repeat_byte(2),
                value: U256::ZERO,
                data: &[0x12],
            },
            native_balance_override(Address::repeat_byte(1)),
            test_envelope(),
        )
        .await;

    assert!(
        matches!(attempt.into_result(), SimulationResult::Failure { reason } if reason.contains("timed out"))
    );
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

/// Drives the real call path against a live node: the simulation must be accepted (the node
/// numbers its own block) and a reverting call must come back named by the trace.
#[tokio::test]
#[ignore = "requires RPC_URL"]
async fn test_live_simulate_and_trace() {
    let rpc_url = std::env::var("RPC_URL").expect("set RPC_URL");
    let provider = alloy::providers::ProviderBuilder::default()
        .connect_http(rpc_url.parse().expect("valid URL"));
    let sender = Address::repeat_byte(0x11);
    let usdt = "0xdAC17F958D2ee523a2206206994597C13D831ec7"
        .parse::<Address>()
        .expect("valid address");

    for (name, data) in [
        // A selector USDT does not implement: reverts with an empty payload, which is the path
        // that has to reach the tracer.
        ("unknown selector", alloy::hex::decode("deadbeef").expect("hex")),
        // transferFrom with no allowance: reverts through SafeMath.
        ("transferFrom without allowance", {
            let mut data = alloy::hex::decode("23b872dd").expect("hex");
            data.extend_from_slice(&sender.into_word().0);
            data.extend_from_slice(&Address::repeat_byte(0x22).into_word().0);
            data.extend_from_slice(&U256::from(1_000_000_u64).to_be_bytes::<32>());
            data
        }),
    ] {
        let outcome = simulate_with_overrides(
            &provider,
            SimulatedCall { sender, router: usdt, value: U256::ZERO, data: &data },
            native_balance_override(sender),
            test_envelope(),
            Duration::from_secs(10),
        )
        .await;

        let described = match &outcome {
            CallOutcome::Reverted { reason } => format!("reverted: {reason}"),
            CallOutcome::Success { amount_out, .. } => format!("success: {amount_out}"),
            CallOutcome::Failure(reason) => format!("failed: {reason}"),
        };
        println!("  {name} -> {described}");
        assert!(
            !described.contains("block numbers must be in order"),
            "{name}: the node refused the block the simulation asked for"
        );
    }
}

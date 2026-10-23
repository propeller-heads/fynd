use std::time::Duration;

use alloy::{
    primitives::{Address, Bytes},
    rpc::{
        client::RpcClient,
        json_rpc::ErrorPayload,
        types::simulate::{SimCallResult, SimulatedBlock},
    },
    sol_types::{SolCall, SolError},
    transports::mock::Asserter,
};
use num_bigint::BigUint;
use tycho_simulation::tycho_common::Bytes as TychoBytes;

use super::*;
use crate::{
    algorithm::test_utils::{component, token, MockProtocolSim},
    simulation::{
        deviation::fixtures::quote_with_fees,
        pamm_overrides::fixtures as pamm_fixtures,
        token_layout::{KeyOrder, MappingPosition, TokenLayout, PROBE_SENTINEL},
    },
    tests::metrics::recorded_metrics,
    Route, Swap, Transaction,
};

/// A budget long enough that a mocked provider, which answers at once, never meets it.
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn test_success_reports_amount_out_and_gas_used() {
    let result = SimulationAttempt::Success {
        amount_out: BigUint::from(42_u8),
        gas_used: 123_456,
        logs: Vec::new(),
    }
    .into_result();
    assert!(
        matches!(result, SimulationResult::Success { amount_out, gas_used, .. } if amount_out == BigUint::from(42_u8) && gas_used == 123_456)
    );
}

#[test]
fn test_revert_reports_no_gas() {
    let result =
        SimulationAttempt::Reverted { reason: "no liquidity".to_string(), replay: empty_replay() }
            .into_result();
    assert!(matches!(result, SimulationResult::Reverted { reason } if reason == "no liquidity"));
}

#[test]
fn test_failure_stays_apart_from_a_revert() {
    let result = SimulationAttempt::Failure { reason: "timed out".to_string() }.into_result();
    assert!(matches!(result, SimulationResult::Failure { reason } if reason == "timed out"));
}

#[tokio::test]
async fn test_simulated_call_returns_its_logs_undecoded() {
    let emitter = Address::repeat_byte(0xCC);
    let topics = vec![B256::repeat_byte(0x11), B256::repeat_byte(0x22)];
    let data = Bytes::from(vec![0x33; 64]);
    let mut response = simulated_response(
        U256::from(123_u64)
            .to_be_bytes::<32>()
            .to_vec(),
        true,
        87_654,
    );
    response[0].calls[0].logs = vec![alloy::rpc::types::Log {
        inner: alloy::primitives::Log::new_unchecked(emitter, topics.clone(), data.clone()),
        ..Default::default()
    }];
    let asserter = Asserter::new();
    asserter.push_success(&response);

    let result = simulate_with_overrides(
        &RootProvider::new(RpcClient::mocked(asserter)),
        SimulatedCall {
            sender: Address::repeat_byte(1),
            router: Address::repeat_byte(2),
            value: U256::ZERO,
            data: &[0x12],
            block: BlockNumberOrTag::Latest,
            quoted_block_number: None,
            quoted_block_timestamp: None,
        },
        native_balance_override(Address::repeat_byte(1)),
        test_envelope(),
        TEST_TIMEOUT,
    )
    .await;

    let CallOutcome::Success { logs, .. } = result else {
        panic!("the call succeeded");
    };
    assert_eq!(
        logs,
        vec![EventLog {
            address: emitter.to_vec().into(),
            topics: topics
                .iter()
                .map(|topic| topic.to_vec().into())
                .collect(),
            data: data.to_vec().into(),
        }]
    );
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

/// The signature code merges into the sender's existing override. Replacing that override would
/// drop the sender's funding, and the simulation would revert on an empty balance instead of in
/// the swap.
#[test]
fn test_client_fee_signature_override_on_the_sender() {
    let sender = Address::repeat_byte(1);
    let mut overrides = native_balance_override(sender);

    add_client_fee_receiver_override(&mut overrides, sender);

    let account = &overrides[&sender];
    assert_eq!(account.balance, Some(SIMULATION_FUNDING_VALUE));
    assert_eq!(account.nonce, Some(SIMULATION_SENDER_NONCE));
    assert_eq!(account.code, Some(Bytes::from_static(&ACCEPT_ANY_ERC1271_SIGNATURE_CODE)));
}

/// The byte the mocked simulator's native-token address repeats. A quote selling the native token
/// skips token layout discovery, so the simulation is the only RPC call.
const MOCKED_NATIVE_TOKEN_BYTE: u8 = 9;

/// The router the encoded quote calls.
const ENCODED_QUOTE_ROUTER: Address = Address::repeat_byte(0x02);

/// An encoded quote selling the mocked simulator's native token.
fn encoded_quote() -> OrderQuote {
    encoded_quote_on("uniswap_v2", Box::new(MockProtocolSim::new(1.0)))
}

/// An encoded quote selling the mocked simulator's native token through one swap on `protocol`,
/// priced on `state`.
fn encoded_quote_on(
    protocol: &str,
    state: Box<dyn tycho_simulation::tycho_common::simulation::protocol_sim::ProtocolSim>,
) -> OrderQuote {
    let token_in = token(MOCKED_NATIVE_TOKEN_BYTE, "NATIVE");
    let token_out = token(0x0B, "OUT");
    let swap = Swap::new(
        "pool".to_string(),
        protocol.to_string(),
        token_in.address.clone(),
        token_out.address.clone(),
        BigUint::from(1_000_u64),
        BigUint::from(990_u64),
        BigUint::from(50_000_u64),
        component("pool", &[token_in.clone(), token_out.clone()]),
        state,
    );
    let route = Route::new(
        vec![swap],
        [(token_in.address.clone(), token_in), (token_out.address.clone(), token_out)],
    )
    .expect("a one-swap route");
    let mut quote = quote_with_fees(1_000_000).with_route(route);
    quote.set_transaction(Transaction::new(
        TychoBytes::from(ENCODED_QUOTE_ROUTER.as_slice()),
        BigUint::ZERO,
        vec![0x12],
    ));
    quote
}

/// Client fee params naming `receiver`, with the zero placeholder signature the TS client sends.
fn client_fee_params(receiver: &[u8]) -> ClientFeeParams {
    ClientFeeParams::new(
        100,
        TychoBytes::from(receiver),
        BigUint::ZERO,
        u64::MAX,
        TychoBytes::from(vec![0; 65]),
    )
}

/// With client fee params on the quote, the simulation gives the fee receiver code that accepts
/// the placeholder signature. Without them, or for a zero receiver the router never checks, no
/// address gets that code.
#[rstest::rstest]
#[case::with_client_fee(Some(Address::repeat_byte(0x0C)), true)]
#[case::without_client_fee(None, false)]
#[case::zero_receiver(Some(Address::ZERO), false)]
#[tokio::test]
async fn test_simulate_attempt_client_fee_receiver(
    #[case] receiver: Option<Address>,
    #[case] receiver_gets_code: bool,
) {
    let client_fee = receiver.map(|receiver| client_fee_params(receiver.as_slice()));
    let asserter = Asserter::new();
    // Only a reverted attempt carries the replay payload, the one place the overrides the
    // simulation sent can be read back.
    asserter.push_success(&simulated_response(
        crate::simulation::revert::RouterErrors::TychoRouter__EmptySwaps {}.abi_encode(),
        false,
        0,
    ));

    let attempt = mocked_simulator(&asserter, TEST_TIMEOUT)
        .simulate_attempt(&encoded_quote(), client_fee.as_ref(), SimulationPurpose::QUOTE)
        .await;

    let SimulationAttempt::Reverted { replay, .. } = attempt else {
        panic!("the mocked call reverts");
    };
    let overrides = replay.payload.block_state_calls[0]
        .state_overrides
        .as_ref()
        .expect("state overrides");
    let code = Bytes::from_static(&ACCEPT_ANY_ERC1271_SIGNATURE_CODE);
    let accounts_with_code: Vec<Address> = overrides
        .iter()
        .filter(|(_, account)| account.code.as_ref() == Some(&code))
        .map(|(address, _)| *address)
        .collect();
    let expected: Vec<Address> = receiver
        .filter(|_| receiver_gets_code)
        .into_iter()
        .collect();
    assert_eq!(accounts_with_code, expected);
}

/// A receiver the simulation cannot give code to fails setup before any RPC call: one that is not
/// an address, and the router, whose own code the override would replace.
#[rstest::rstest]
#[case::not_20_bytes(vec![0x0C; 19], "is not 20 bytes")]
#[case::router(ENCODED_QUOTE_ROUTER.to_vec(), "is the router")]
#[tokio::test]
async fn test_simulate_attempt_unusable_client_fee_receiver(
    #[case] receiver: Vec<u8>,
    #[case] expected_reason: &str,
) {
    let asserter = Asserter::new();

    let attempt = mocked_simulator(&asserter, TEST_TIMEOUT)
        .simulate_attempt(
            &encoded_quote(),
            Some(&client_fee_params(&receiver)),
            SimulationPurpose::QUOTE,
        )
        .await;

    let SimulationAttempt::Failure { reason } = attempt else {
        panic!("setup fails");
    };
    assert!(reason.starts_with("simulation setup failed: client fee receiver"), "{reason}");
    assert!(reason.contains(expected_reason), "{reason}");
}

/// An envelope for the tests that drive the call directly, without a quote to derive one from.
fn empty_replay() -> Box<Replay> {
    Box::new(Replay { parent_block: 0, payload: SimulatePayload::default() })
}

fn test_envelope() -> SimulationEnvelope {
    SimulationEnvelope { gas_limit: SIMULATION_MIN_GAS_LIMIT, gas_price: 1 }
}

#[test]
fn test_envelope_multiplies_the_estimated_gas() {
    let envelope = SimulationEnvelope::new(Some(1_000_000), Some(7));
    assert_eq!(envelope.gas_limit, 1_000_000 * SIMULATION_GAS_LIMIT_MULTIPLIER);
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
    let overrides = block_overrides(None, None);
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
    let base = block_overrides(None, None);
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
    let overrides = block_overrides(None, None);

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
        Address::repeat_byte(MOCKED_NATIVE_TOKEN_BYTE),
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

/// Answers `eth_simulateV1` with a successful call, echoing the request id, and only when the
/// request names `block` as the state to run on.
struct SimulatesOn {
    block: &'static str,
    response: serde_json::Value,
}

impl wiremock::Match for SimulatesOn {
    fn matches(&self, request: &wiremock::Request) -> bool {
        let Ok(body) = serde_json::from_slice::<serde_json::Value>(&request.body) else {
            return false;
        };
        body["method"] == "eth_simulateV1" && body["params"][1] == self.block
    }
}

impl wiremock::Respond for SimulatesOn {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let id = serde_json::from_slice::<serde_json::Value>(&request.body)
            .map(|body| body["id"].clone())
            .unwrap_or_default();
        wiremock::ResponseTemplate::new(200)
            .set_body_json(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": self.response}))
    }
}

/// Answers every request with the error geth gives for a block it does not have.
struct HeaderNotFound;

impl wiremock::Respond for HeaderNotFound {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let id = serde_json::from_slice::<serde_json::Value>(&request.body)
            .map(|body| body["id"].clone())
            .unwrap_or_default();
        wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32000, "message": "header not found"},
        }))
    }
}

fn http_simulator(
    server: &wiremock::MockServer,
    block_time: Duration,
    request_timeout: Duration,
) -> QuoteSimulator {
    QuoteSimulator::with_provider(
        RootProvider::new_http(server.uri().parse().unwrap()),
        Address::repeat_byte(9),
        request_timeout,
    )
    .with_block_time(block_time)
}

fn call_on_block(number: u64) -> SimulatedCall<'static> {
    SimulatedCall {
        sender: Address::repeat_byte(1),
        router: Address::repeat_byte(2),
        value: U256::ZERO,
        data: &[0x12],
        block: BlockNumberOrTag::Number(number),
        quoted_block_number: None,
        quoted_block_timestamp: None,
    }
}

#[tokio::test]
async fn test_simulation_waits_for_a_node_that_does_not_have_the_block_yet() {
    let server = wiremock::MockServer::start().await;
    let response = serde_json::to_value(simulated_response(
        U256::from(123_u64)
            .to_be_bytes::<32>()
            .to_vec(),
        true,
        87_654,
    ))
    .unwrap();
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(HeaderNotFound)
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    wiremock::Mock::given(SimulatesOn { block: "0x4d2", response: response.clone() })
        .respond_with(SimulatesOn { block: "0x4d2", response })
        .mount(&server)
        .await;
    let simulator = http_simulator(&server, Duration::from_secs(2), TEST_TIMEOUT);
    let started = std::time::Instant::now();

    let attempt = simulator
        .simulate_within_timeout(
            call_on_block(1_234),
            native_balance_override(Address::repeat_byte(1)),
            test_envelope(),
        )
        .await;

    assert!(matches!(attempt, SimulationAttempt::Success { .. }), "found on the third try");
    assert!(started.elapsed() >= Duration::from_millis(1_500), "waited 500 ms, then 1 s");
}

#[tokio::test]
async fn test_simulation_gives_up_on_the_block_after_two_block_times() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(HeaderNotFound)
        .mount(&server)
        .await;
    let simulator = http_simulator(&server, Duration::from_millis(400), TEST_TIMEOUT);
    let started = std::time::Instant::now();

    let attempt = simulator
        .simulate_within_timeout(
            call_on_block(1_234),
            native_balance_override(Address::repeat_byte(1)),
            test_envelope(),
        )
        .await;

    let elapsed = started.elapsed();
    assert!(
        matches!(&attempt, SimulationAttempt::BlockUnavailable { reason } if reason.contains("header not found")),
        "a node failure, not a failed or reverted call"
    );
    assert!(elapsed >= Duration::from_millis(800), "waited two block times: {elapsed:?}");
    assert!(elapsed < Duration::from_millis(1_500), "and no longer: {elapsed:?}");
    let requests = server
        .received_requests()
        .await
        .unwrap_or_default()
        .len();
    assert_eq!(requests, 3, "asked at 0, 500 ms and 800 ms");
}

#[tokio::test]
async fn test_waiting_for_the_block_stays_within_the_request_timeout() {
    // Two Ethereum block times would be 24 s; the request allows 1 s.
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(HeaderNotFound)
        .mount(&server)
        .await;
    let simulator = http_simulator(&server, Duration::from_secs(12), Duration::from_millis(1_000));
    let started = std::time::Instant::now();

    let attempt = simulator
        .simulate_within_timeout(
            call_on_block(1_234),
            native_balance_override(Address::repeat_byte(1)),
            test_envelope(),
        )
        .await;

    let elapsed = started.elapsed();
    assert!(matches!(attempt, SimulationAttempt::BlockUnavailable { .. }));
    assert!(elapsed < Duration::from_millis(1_000), "within the request timeout: {elapsed:?}");
    let requests = server
        .received_requests()
        .await
        .unwrap_or_default()
        .len();
    assert_eq!(requests, 2, "asked at 0 and 500 ms; a 1 s wait would outlast the request");
}

#[tokio::test]
async fn test_other_node_errors_are_not_retried() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jsonrpc": "2.0", "id": 0,
            "error": {"code": -32601, "message": "the method eth_simulateV1 does not exist"},
        })))
        .mount(&server)
        .await;
    let simulator = http_simulator(&server, Duration::from_secs(2), TEST_TIMEOUT);

    let attempt = simulator
        .simulate_within_timeout(
            call_on_block(1_234),
            native_balance_override(Address::repeat_byte(1)),
            test_envelope(),
        )
        .await;

    assert!(matches!(attempt, SimulationAttempt::Failure { .. }));
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .len(),
        1
    );
}

#[test]
fn test_simulator_runs_on_the_latest_block_by_default() {
    let simulator = mocked_simulator(&Asserter::new(), TEST_TIMEOUT);
    let quote = encoded_quote();

    let setup = SimulationSetup::for_quote(&quote, None, simulator.block).expect("a valid quote");

    assert_eq!(setup.call.block, BlockNumberOrTag::Latest);
}

#[test]
fn test_setup_runs_on_the_quote_block_when_asked() {
    let quote = encoded_quote();

    let setup =
        SimulationSetup::for_quote(&quote, None, SimulationBlock::Quote).expect("a valid quote");

    assert_eq!(setup.call.block, BlockNumberOrTag::Number(quote.block().number()));
}

#[tokio::test]
async fn test_simulation_runs_on_the_block_it_is_given() {
    let server = wiremock::MockServer::start().await;
    let response = serde_json::to_value(simulated_response(
        U256::from(123_u64)
            .to_be_bytes::<32>()
            .to_vec(),
        true,
        87_654,
    ))
    .unwrap();
    wiremock::Mock::given(SimulatesOn { block: "0x4d2", response: response.clone() })
        .respond_with(SimulatesOn { block: "0x4d2", response })
        .expect(1)
        .mount(&server)
        .await;
    let provider = RootProvider::new_http(server.uri().parse().unwrap());

    let result = simulate_with_overrides(
        &provider,
        SimulatedCall {
            sender: Address::repeat_byte(1),
            router: Address::repeat_byte(2),
            value: U256::ZERO,
            data: &[0x12],
            block: BlockNumberOrTag::Number(1_234),
            quoted_block_number: None,
            quoted_block_timestamp: None,
        },
        native_balance_override(Address::repeat_byte(1)),
        test_envelope(),
        TEST_TIMEOUT,
    )
    .await;

    assert!(matches!(result, CallOutcome::Success { .. }), "the node was asked for block 1234");
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
            block: BlockNumberOrTag::Latest,
            quoted_block_number: None,
            quoted_block_timestamp: None,
        },
        native_balance_override(Address::repeat_byte(1)),
        test_envelope(),
        TEST_TIMEOUT,
    )
    .await;
    assert!(
        matches!(result, CallOutcome::Success { amount_out, gas_used, .. } if amount_out == BigUint::from(123_u64) && gas_used == 87_654)
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
            block: BlockNumberOrTag::Latest,
            quoted_block_number: None,
            quoted_block_timestamp: None,
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
            block: BlockNumberOrTag::Latest,
            quoted_block_number: None,
            quoted_block_timestamp: None,
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

/// A prestate trace naming one contract and the slots its read touched.
fn prestate(contract: Address, slots: &[B256]) -> serde_json::Value {
    let storage: serde_json::Map<String, serde_json::Value> = slots
        .iter()
        .map(|slot| (format!("{slot:#x}"), serde_json::json!(format!("{:#x}", B256::ZERO))))
        .collect();
    serde_json::json!({ format!("{contract:#x}"): { "code": "0x6000", "storage": storage } })
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
    // itself rather than on the node. The scaled probe traces once more and finds no mapping
    // slot to write.
    for _ in 0..2 {
        asserter.push_success(&prestate(Address::repeat_byte(3), &[B256::repeat_byte(0x99)]));
        asserter.push_success(&Bytes::from(B256::from(PROBE_SENTINEL).to_vec()));
    }
    asserter.push_success(&prestate(Address::repeat_byte(3), &[B256::repeat_byte(0x99)]));

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
                block: BlockNumberOrTag::Latest,
                quoted_block_number: None,
                quoted_block_timestamp: None,
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
        matches!(attempt.into_result(), SimulationResult::Reverted { reason }
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
        matches!(attempt.into_result(), SimulationResult::Reverted { reason }
            if reason == "simulation reverted: execution reverted"),
        "the node's own message survives"
    );
}

/// A reverted call is only reproducible against the block it ran in: a signed RFQ quote expires,
/// and `prevrandao` is drawn per call. The replay has to carry that block, not the head's.
#[tokio::test]
async fn test_reverted_simulation_carries_a_replay_pinned_to_its_block() {
    let asserter = Asserter::new();
    let mut response = simulated_response(
        crate::simulation::revert::RouterErrors::TychoRouter__EmptySwaps {}.abi_encode(),
        false,
        0,
    );
    response[0].inner.header.inner.number = 101;
    response[0].inner.header.inner.timestamp = 1_700_000_000;
    asserter.push_success(&response);
    let sender = Address::repeat_byte(1);

    let outcome = simulate_with_overrides(
        &RootProvider::new(RpcClient::mocked(asserter)),
        SimulatedCall {
            sender,
            router: Address::repeat_byte(2),
            value: U256::ZERO,
            data: &[0x12],
            block: BlockNumberOrTag::Latest,
            quoted_block_number: None,
            quoted_block_timestamp: None,
        },
        native_balance_override(sender),
        test_envelope(),
        TEST_TIMEOUT,
    )
    .await;

    let CallOutcome::Reverted { replay, .. } = outcome else {
        panic!("the call reverted");
    };
    assert_eq!(replay.parent_block, 100);
    let block = &replay.payload.block_state_calls[0];
    let environment = block
        .block_overrides
        .as_ref()
        .expect("block overrides");
    assert_eq!(environment.number, Some(U256::from(101)));
    assert_eq!(environment.time, Some(1_700_000_000));
    assert!(environment.random.is_some(), "prevrandao is pinned");
    assert!(block
        .state_overrides
        .as_ref()
        .is_some_and(|overrides| overrides.contains_key(&sender)));
    assert_eq!(block.calls[0].input.input(), Some(&Bytes::from_static(&[0x12])));
    assert!(replay
        .payload_json()
        .contains("\"blockStateCalls\""));
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
        SimulationBlock::Latest,
    )
    .expect("build a simulator against the local listener");

    let attempt = simulator
        .simulate_within_timeout(
            SimulatedCall {
                sender: Address::repeat_byte(1),
                router: Address::repeat_byte(2),
                value: U256::ZERO,
                data: &[0x12],
                block: BlockNumberOrTag::Latest,
                quoted_block_number: None,
                quoted_block_timestamp: None,
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
                logs: Vec::new(),
            },
            SimulationPurpose::QUOTE,
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
    for (name, labels, _) in &recorded {
        assert!(labels.contains(&"purpose=quote".to_string()), "{name}: {labels:?}");
    }

    let (.., deviation) = recorded
        .iter()
        .find(|(name, ..)| name == "quote_simulation_deviation_bps")
        .expect("a successful simulation records its deviation");
    assert!(
        matches!(deviation, metrics_util::debugging::DebugValue::Histogram(values)
            if values.len() == 1 && (values[0].into_inner() - -10.0).abs() < 1e-9),
        "{deviation:?}"
    );

    for (metric, expected) in
        [("quote_simulation_gas_estimate", 50_000.0), ("quote_simulation_gas_used", 120_000.0)]
    {
        let (_, labels, gas) = recorded
            .iter()
            .find(|(name, ..)| name == metric)
            .unwrap_or_else(|| panic!("a successful simulation records {metric}"));
        assert!(labels.contains(&"algorithm=test_algorithm".to_string()), "{labels:?}");
        assert!(
            matches!(gas, metrics_util::debugging::DebugValue::Histogram(values)
                if values.len() == 1 && values[0].into_inner() == expected),
            "{metric}: {gas:?}"
        );
    }
}

#[test]
fn test_record_outcome_reverted() {
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();

    metrics::with_local_recorder(&recorder, || {
        record_outcome(
            &quote_with_fees(1_000_000),
            &SimulationAttempt::Reverted { reason: "reverted".to_string(), replay: empty_replay() },
            SimulationPurpose::QUOTE,
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
    assert!(
        !recorded
            .iter()
            .any(|(name, ..)| name.starts_with("quote_simulation_gas_")),
        "a reverted call has no gas to compare against the estimate"
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
            SimulationPurpose::QUOTE,
        );
    });

    assert!(recorded_metrics(&snapshotter)
        .iter()
        .any(|(name, labels, _)| name == "quote_simulations_total" &&
            labels.contains(&"outcome=failed".to_string())));
}

/// Runs the receiver code on a live node: an `isValidSignature` call with a zero placeholder
/// signature and a 1000-wei transfer with no calldata must both return the ERC-1271 magic value.
#[tokio::test]
#[ignore = "requires RPC_URL"]
async fn test_live_accept_any_erc1271_signature_code() {
    alloy::sol! {
        function isValidSignature(bytes32 hash, bytes signature) returns (bytes4 magicValue);
    }

    let rpc_url = std::env::var("RPC_URL").expect("set RPC_URL");
    let provider = alloy::providers::ProviderBuilder::default()
        .connect_http(rpc_url.parse().expect("valid URL"));
    let sender = Address::repeat_byte(0x11);
    let receiver = Address::repeat_byte(0x22);
    let mut overrides = native_balance_override(sender);
    add_client_fee_receiver_override(&mut overrides, receiver);

    let signature_check =
        isValidSignatureCall { hash: B256::repeat_byte(0x11), signature: Bytes::from(vec![0; 65]) }
            .abi_encode();
    let mut magic_value = [0_u8; 32];
    magic_value[..4].copy_from_slice(&isValidSignatureCall::SELECTOR);

    for (name, value, data) in [
        ("isValidSignature", U256::ZERO, signature_check),
        ("value transfer", U256::from(1_000_u64), Vec::new()),
    ] {
        let outcome = simulate_with_overrides(
            &provider,
            SimulatedCall {
                sender,
                router: receiver,
                value,
                data: &data,
                block: BlockNumberOrTag::Latest,
                quoted_block_number: None,
                quoted_block_timestamp: None,
            },
            overrides.clone(),
            test_envelope(),
            Duration::from_secs(10),
        )
        .await;

        let CallOutcome::Success { amount_out, .. } = outcome else {
            panic!("{name}: the call to the receiver code did not succeed");
        };
        assert_eq!(amount_out, BigUint::from_bytes_be(&magic_value), "{name}");
    }
}

#[test]
fn test_record_outcome_labels_a_caller_purpose() {
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();

    metrics::with_local_recorder(&recorder, || {
        record_outcome(
            &quote_with_fees(1_000_000),
            &SimulationAttempt::Success {
                amount_out: BigUint::from(999_000u64),
                gas_used: 120_000,
                logs: Vec::new(),
            },
            SimulationPurpose::new("sample"),
        );
    });

    let recorded = recorded_metrics(&snapshotter);
    let names: Vec<&str> = recorded
        .iter()
        .map(|(name, ..)| name.as_str())
        .collect();
    for metric in [
        "quote_simulations_total",
        "quote_simulation_deviation_bps",
        "quote_simulation_gas_estimate",
        "quote_simulation_gas_used",
    ] {
        assert!(names.contains(&metric), "{metric} is recorded: {names:?}");
    }
    for (name, labels, _) in &recorded {
        assert!(labels.contains(&"purpose=sample".to_string()), "{name}: {labels:?}");
    }
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
            SimulatedCall {
                sender,
                router: usdt,
                value: U256::ZERO,
                data: &data,
                block: BlockNumberOrTag::Latest,
                quoted_block_number: None,
                quoted_block_timestamp: None,
            },
            native_balance_override(sender),
            test_envelope(),
            Duration::from_secs(10),
        )
        .await;

        let described = match &outcome {
            CallOutcome::Reverted { reason, .. } => format!("reverted: {reason}"),
            CallOutcome::Success { amount_out, .. } => format!("success: {amount_out}"),
            CallOutcome::Failure(reason) => format!("failed: {reason}"),
            CallOutcome::BlockUnavailable(reason) => format!("block unavailable: {reason}"),
        };
        println!("  {name} -> {described}");
        assert!(
            !described.contains("block numbers must be in order"),
            "{name}: the node refused the block the simulation asked for"
        );
    }
}

/// The block the pAMM fixtures quote for.
const QUOTED_BLOCK: u64 = 26_201_954;

/// An encoded quote through a pAMM swap whose state carries `overrides`, when given.
fn pamm_quote(
    overrides: Option<tycho_simulation::price_level_stream::overrides::PammSimulationOverrides>,
) -> OrderQuote {
    encoded_quote_on("fallback:fermiswap", Box::new(pamm_fixtures::pamm_state(overrides)))
}

/// Answers every `eth_simulateV1` with a successful call that returns 990.
async fn simulation_server() -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    let response = serde_json::to_value(simulated_response(
        U256::from(990_u64)
            .to_be_bytes::<32>()
            .to_vec(),
        true,
        87_654,
    ))
    .unwrap();
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(move |request: &wiremock::Request| {
            let id = serde_json::from_slice::<serde_json::Value>(&request.body)
                .map(|body| body["id"].clone())
                .unwrap_or_default();
            wiremock::ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": response}))
        })
        .mount(&server)
        .await;
    server
}

/// The one `eth_simulateV1` request `server` received.
async fn sent_simulation(server: &wiremock::MockServer) -> serde_json::Value {
    let requests = server
        .received_requests()
        .await
        .expect("request recording is on");
    let [request] = requests.as_slice() else {
        panic!("expected one request, got {}", requests.len());
    };
    serde_json::from_slice(&request.body).expect("a JSON-RPC request")
}

/// The overrides `request` sends for the account whose address repeats `byte`.
fn account_in(request: &serde_json::Value, byte: u8) -> Option<&serde_json::Value> {
    let address = format!("{:#x}", Address::repeat_byte(byte));
    request["params"][0]["blockStateCalls"][0]["stateOverrides"]
        .as_object()?
        .iter()
        .find(|(key, _)| key.to_lowercase() == address)
        .map(|(_, account)| account)
}

#[test]
fn test_block_overrides_set_the_quoted_block() {
    let overrides = block_overrides(Some(QUOTED_BLOCK), Some(1_792_115_924));

    assert_eq!(overrides.number, Some(U256::from(QUOTED_BLOCK)));
    assert_eq!(overrides.time, Some(1_792_115_924));
    assert_eq!(overrides.coinbase, Some(SIMULATION_COINBASE));
    assert_eq!(overrides.gas_limit, Some(SIMULATION_BLOCK_GAS_LIMIT));
    assert!(overrides.random.is_some());
}

/// A route whose pAMM swap carries overrides runs on the quoted block's parent, as the quoted
/// block, with the venue's state next to the sender's funding, whichever block the simulator is
/// set to.
#[rstest::rstest]
#[case::latest(SimulationBlock::Latest)]
#[case::quote(SimulationBlock::Quote)]
#[tokio::test]
async fn test_simulate_attempt_pamm_route_runs_as_the_quoted_block(#[case] block: SimulationBlock) {
    let server = simulation_server().await;
    let quote = pamm_quote(Some(pamm_fixtures::overrides(
        Some(QUOTED_BLOCK),
        [(0xa1, pamm_fixtures::storage(&[(0, 7)]))],
    )));

    let attempt = http_simulator(&server, Duration::from_secs(12), TEST_TIMEOUT)
        .with_block(block)
        .simulate_attempt(&quote, None, SimulationPurpose::QUOTE)
        .await;

    assert!(matches!(attempt, SimulationAttempt::Success { .. }));
    let request = sent_simulation(&server).await;
    assert_eq!(request["params"][1], format!("{:#x}", QUOTED_BLOCK - 1));
    let environment = &request["params"][0]["blockStateCalls"][0]["blockOverrides"];
    assert_eq!(environment["number"], format!("{:#x}", QUOTED_BLOCK));
    assert_eq!(environment["time"], format!("{:#x}", pamm_fixtures::timestamp_of(QUOTED_BLOCK)));
    assert_eq!(
        environment["coinbase"]
            .as_str()
            .map(str::to_lowercase),
        Some(format!("{SIMULATION_COINBASE:#x}"))
    );
    let venue = account_in(&request, 0xa1).expect("the venue's account is overridden");
    assert_eq!(
        venue["stateDiff"][format!("{:#x}", B256::ZERO)],
        format!("{:#x}", B256::from(U256::from(7)))
    );
    assert!(venue.get("state").is_none(), "never a full state replacement");
    let sender = account_in(&request, quote_sender_byte()).expect("the sender is funded");
    assert!(sender.get("balance").is_some());
}

/// A route whose pAMM swap carries no overrides runs exactly as any other route.
#[tokio::test]
async fn test_simulate_attempt_pamm_route_without_overrides() {
    let server = simulation_server().await;

    let attempt = http_simulator(&server, Duration::from_secs(12), TEST_TIMEOUT)
        .simulate_attempt(&pamm_quote(None), None, SimulationPurpose::QUOTE)
        .await;

    assert!(matches!(attempt, SimulationAttempt::Success { .. }));
    let request = sent_simulation(&server).await;
    assert_eq!(request["params"][1], "latest");
    let environment = &request["params"][0]["blockStateCalls"][0]["blockOverrides"];
    assert!(environment.get("number").is_none());
    assert!(environment.get("time").is_none());
}

/// A route whose pAMM overrides name no block runs on the usual block, with the venue's state.
#[tokio::test]
async fn test_simulate_attempt_pamm_overrides_without_a_block() {
    let server = simulation_server().await;
    let quote = pamm_quote(Some(pamm_fixtures::overrides(
        None,
        [(0xa1, pamm_fixtures::storage(&[(0, 7)]))],
    )));

    let attempt = http_simulator(&server, Duration::from_secs(12), TEST_TIMEOUT)
        .simulate_attempt(&quote, None, SimulationPurpose::QUOTE)
        .await;

    assert!(matches!(attempt, SimulationAttempt::Success { .. }));
    let request = sent_simulation(&server).await;
    assert_eq!(request["params"][1], "latest");
    assert!(request["params"][0]["blockStateCalls"][0]["blockOverrides"]
        .get("number")
        .is_none());
    assert!(account_in(&request, 0xa1).is_some());
}

/// The byte the fixture quote's sender address repeats.
fn quote_sender_byte() -> u8 {
    let sender = encoded_quote().sender().clone();
    assert!(
        sender
            .iter()
            .all(|byte| *byte == sender[0]),
        "the fixture sender repeats one byte"
    );
    sender[0]
}

#[rstest::rstest]
#[case::with_overrides(true, "overrides=applied")]
#[case::without_overrides(false, "overrides=none")]
fn test_record_pamm_outcome(#[case] applied: bool, #[case] label: &str) {
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();

    metrics::with_local_recorder(&recorder, || {
        record_pamm_outcome(
            &quote_with_fees(1_000_000),
            &SimulationAttempt::Success {
                amount_out: BigUint::from(999_000u64),
                gas_used: 120_000,
                logs: Vec::new(),
            },
            "success",
            PammSimulation { applied, funding_collisions: 2 },
            SimulationPurpose::QUOTE,
        );
    });

    let recorded = recorded_metrics(&snapshotter);
    let find = |name: &str| {
        recorded
            .iter()
            .find(|(recorded_name, ..)| recorded_name == name)
            .unwrap_or_else(|| panic!("{name} is recorded"))
    };
    let simulations = find(pamm_overrides::SIMULATIONS_TOTAL);
    assert!(
        simulations
            .1
            .contains(&label.to_string()),
        "{:?}",
        simulations.1
    );
    assert!(
        simulations
            .1
            .contains(&"outcome=success".to_string()),
        "{:?}",
        simulations.1
    );
    assert!(find(pamm_overrides::SIMULATION_DEVIATION_BPS)
        .1
        .contains(&label.to_string()));
    assert_eq!(
        find(pamm_overrides::FUNDING_COLLISIONS_TOTAL).2,
        metrics_util::debugging::DebugValue::Counter(2)
    );
}

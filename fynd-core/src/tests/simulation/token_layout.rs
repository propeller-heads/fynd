use alloy::{
    hex,
    primitives::{address, Address, U256},
    providers::ProviderBuilder,
    rpc::{client::RpcClient, json_rpc::ErrorPayload},
    transports::mock::Asserter,
};
use rstest::rstest;

use super::*;

/// Highest base the migrated sentinel-probe path searched. The vectors below walk it to show the
/// namespaced layout collides with none of the small bases.
const MAX_STANDARD_BASE: u16 = 20;

fn usdc() -> Address {
    address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")
}

fn weth() -> Address {
    address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
}

fn solidity(base: u16) -> MappingPosition {
    MappingPosition::Direct { base, key_order: KeyOrder::Solidity }
}

/// Known-good hashes, computed outside this crate. They pin the mapping arithmetic itself, so a
/// change to `solidity_mapping` fails here rather than only failing against a live token.
#[rstest]
#[case(Address::ZERO, 0, hex!("ad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5"))]
#[case(usdc(), 0, hex!("c6521c8ea4247e8beb499344e591b9401fb2807ff9997dd598fd9e56c73a264d"))]
#[case(usdc(), 1, hex!("84893e0f271e5f8233d24aa85ba38e0d2ed8f0fc8f608c286ccee51e6c35dd6e"))]
fn test_balance_slot_vectors(
    #[case] holder: Address,
    #[case] base: u16,
    #[case] expected: [u8; 32],
) {
    assert_eq!(balance_slot(holder, solidity(base)).0, expected);
}

#[test]
fn test_allowance_slot_vector() {
    assert_eq!(
        allowance_slot(usdc(), weth(), solidity(0)).0,
        hex!("7b7d28f4178b11583278450af3b85d49a04fd0597c53f7ed3fbfac3750fde37d")
    );
}

/// An allowance is a nested mapping, so it must not collide with the balance of either key, and
/// swapping owner and spender must move it.
#[test]
fn test_allowance_slot_is_distinct_and_ordered() {
    assert_ne!(allowance_slot(usdc(), weth(), solidity(0)), balance_slot(usdc(), solidity(0)));
    assert_ne!(
        allowance_slot(usdc(), weth(), solidity(0)),
        allowance_slot(weth(), usdc(), solidity(0))
    );
}

#[test]
fn test_openzeppelin_v5_slots_collide_with_no_standard_base() {
    let balance = balance_slot(usdc(), MappingPosition::OpenZeppelinV5);
    let allowance = allowance_slot(usdc(), weth(), MappingPosition::OpenZeppelinV5);
    for base in 0..=MAX_STANDARD_BASE {
        assert_ne!(balance, balance_slot(usdc(), solidity(base)));
        assert_ne!(allowance, allowance_slot(usdc(), weth(), solidity(base)));
    }
}

/// The namespace is a constant here but a derivation in OpenZeppelin, so it is re-derived rather
/// than restated.
#[test]
fn test_openzeppelin_v5_namespaces_match_erc7201() {
    let name_hash = keccak256(b"openzeppelin.storage.ERC20");
    let encoded = (U256::from_be_bytes(*name_hash) - U256::from(1_u8)).to_be_bytes::<32>();
    let mut derived = *keccak256(encoded);
    derived[31] = 0;

    assert_eq!(B256::new(derived), OZ_V5_BALANCES_NS);
    // Allowances are the next field of the same struct.
    assert_eq!(
        U256::from_be_bytes(*OZ_V5_ALLOWANCES_NS),
        U256::from_be_bytes(*OZ_V5_BALANCES_NS) + U256::from(1_u8)
    );
}

#[rstest]
#[case::deep_solidity(MappingPosition::Direct { base: 516, key_order: KeyOrder::Solidity })]
#[case::shallow_solidity(solidity(0))]
#[case::vyper(MappingPosition::Direct { base: 17, key_order: KeyOrder::Vyper })]
#[case::openzeppelin_v5(MappingPosition::OpenZeppelinV5)]
fn test_recover_position_round_trip(#[case] position: MappingPosition) {
    let owner = Address::repeat_byte(0x11);
    let spender = Address::repeat_byte(0x22);

    assert_eq!(
        recover_position(balance_slot(owner, position), |candidate| balance_slot(owner, candidate)),
        Some(position)
    );
    assert_eq!(
        recover_position(allowance_slot(owner, spender, position), |candidate| allowance_slot(
            owner, spender, candidate
        )),
        Some(position)
    );
}

#[test]
fn test_recover_position_of_a_slot_no_convention_produces() {
    let owner = Address::repeat_byte(0x11);
    assert_eq!(
        recover_position(B256::repeat_byte(0x99), |candidate| balance_slot(owner, candidate)),
        None
    );
}

fn mocked_provider(asserter: &Asserter) -> RootProvider<Ethereum> {
    RootProvider::new(RpcClient::mocked(asserter.clone()))
}

/// A prestate trace naming one contract and the slots its read touched.
fn prestate(contract: Address, slots: &[B256]) -> serde_json::Value {
    prestate_of(&[(contract, true, slots)])
}

/// A prestate trace naming several accounts, each with or without code.
fn prestate_of(accounts: &[(Address, bool, &[B256])]) -> serde_json::Value {
    let mut trace = serde_json::Map::new();
    for (account, has_code, slots) in accounts {
        let storage: serde_json::Map<String, serde_json::Value> = slots
            .iter()
            .map(|slot| (format!("{slot:#x}"), serde_json::json!(format!("{:#x}", B256::ZERO))))
            .collect();
        let mut entry = serde_json::json!({ "storage": storage });
        if *has_code {
            entry["code"] = serde_json::json!("0x6080604052");
        }
        trace.insert(format!("{account:#x}"), entry);
    }
    serde_json::Value::Object(trace)
}

fn word(value: U256) -> Bytes {
    Bytes::from(B256::from(value).to_vec())
}

fn out_of_gas_payload() -> ErrorPayload {
    ErrorPayload { code: -32000, message: "out of gas".into(), data: None }
}

fn sentinel_word() -> Vec<u8> {
    B256::from(PROBE_SENTINEL).to_vec()
}

fn revert_payload() -> ErrorPayload {
    ErrorPayload { code: 3, message: "execution reverted".into(), data: None }
}

/// A rate limit: an error response that says nothing about the slot.
fn throttled_payload() -> ErrorPayload {
    ErrorPayload { code: -32005, message: "limit exceeded".into(), data: None }
}

/// The first candidate is a decoy the token reads but does not key on, so the probe has to go on
/// to the second rather than stop at the first slot the trace named.
#[tokio::test]
async fn test_find_accessed_slot_takes_the_candidate_that_moves_the_answer() {
    let holder = Address::repeat_byte(1);
    let contract = Address::repeat_byte(3);
    let mapping = balance_slot(holder, solidity(2));
    // Sorted descending, so the decoy is probed first.
    let decoy = B256::repeat_byte(0xff);
    let asserter = Asserter::new();
    asserter.push_success(&prestate(contract, &[decoy, mapping]));
    asserter.push_success(&Bytes::from(vec![0_u8; 32]));
    asserter.push_success(&Bytes::from(sentinel_word()));

    let found = find_accessed_slot(&mocked_provider(&asserter), Address::repeat_byte(9), &[0x70])
        .await
        .expect("the second candidate answers");

    assert_eq!(found, (contract, mapping));
}

/// Every probe reverting means every candidate is genuinely wrong, which is a property of the
/// token and is remembered.
#[tokio::test]
async fn test_find_accessed_slot_reports_unsupported_when_every_probe_reverts() {
    let asserter = Asserter::new();
    asserter.push_success(&prestate(
        Address::repeat_byte(3),
        &[B256::repeat_byte(0xff), B256::repeat_byte(0xee)],
    ));
    asserter.push_failure(revert_payload());
    asserter.push_failure(revert_payload());

    let error = find_accessed_slot(&mocked_provider(&asserter), Address::repeat_byte(9), &[0x70])
        .await
        .expect_err("no candidate matched");

    assert!(matches!(error, DiscoveryError::Unsupported(_)), "{error:?}");
}

/// A node that declined to run the probe proves nothing, so the token stays undecided and the
/// next quote tries again. Counting it as a miss would cache "unsupported" for the process.
#[tokio::test]
async fn test_find_accessed_slot_reports_rpc_when_a_probe_is_refused() {
    let asserter = Asserter::new();
    asserter.push_success(&prestate(Address::repeat_byte(3), &[B256::repeat_byte(0xff)]));
    asserter.push_failure(throttled_payload());

    let error = find_accessed_slot(&mocked_provider(&asserter), Address::repeat_byte(9), &[0x70])
        .await
        .expect_err("the probe was refused");

    assert!(matches!(error, DiscoveryError::Rpc(_)), "{error:?}");
}

#[tokio::test]
async fn test_find_accessed_slot_reports_rpc_when_the_trace_fails() {
    let asserter = Asserter::new();
    asserter.push_failure(throttled_payload());

    let error = find_accessed_slot(&mocked_provider(&asserter), Address::repeat_byte(9), &[0x70])
        .await
        .expect_err("the trace failed");

    assert!(matches!(error, DiscoveryError::Rpc(_)), "{error:?}");
}

/// Funding a token whose allowance lives in another contract would write the balance to one
/// address and the approval to another, so the layout is refused rather than half-applied.
#[tokio::test]
async fn test_discover_layout_refuses_split_storage() {
    let holder = Address::repeat_byte(1);
    let spender = Address::repeat_byte(2);
    let token = Address::repeat_byte(9);
    let asserter = Asserter::new();
    asserter.push_success(&prestate(Address::repeat_byte(3), &[balance_slot(holder, solidity(0))]));
    asserter.push_success(&Bytes::from(sentinel_word()));
    asserter.push_success(&prestate(
        Address::repeat_byte(4),
        &[allowance_slot(holder, spender, solidity(1))],
    ));
    asserter.push_success(&Bytes::from(sentinel_word()));

    let error = discover_layout(&mocked_provider(&asserter), token, holder, spender)
        .await
        .expect_err("split storage is not fundable");

    assert!(
        matches!(&error, DiscoveryError::Unsupported(reason) if reason.contains("different contracts")),
        "{error:?}"
    );
}

/// A rebasing token computes `balanceOf` from shares, so the balance trace finds arithmetic and
/// the shares trace finds the mapping. The fallback is what keeps the module off an address list.
#[tokio::test]
async fn test_discover_balance_falls_back_to_the_shares_view() {
    let holder = Address::repeat_byte(1);
    let contract = Address::repeat_byte(3);
    let shares = balance_slot(holder, solidity(0));
    let asserter = Asserter::new();
    // The balance trace names a slot that does not key the answer.
    asserter.push_success(&prestate(contract, &[B256::repeat_byte(0xff)]));
    asserter.push_failure(revert_payload());
    // The shares trace names the mapping itself.
    asserter.push_success(&prestate(contract, &[shares]));
    asserter.push_success(&Bytes::from(sentinel_word()));

    let (storage_contract, position, encoding) =
        discover_balance(&mocked_provider(&asserter), Address::repeat_byte(9), holder)
            .await
            .expect("the shares view places the mapping");

    assert_eq!(storage_contract, contract);
    assert_eq!(position, solidity(0));
    assert_eq!(encoding, BalanceEncoding::Plain);
}

/// On an Arbitrum chain every call reads ArbOS state, an account without code that the node
/// refuses to override. It is never probed, so the token's own slot is reached.
#[tokio::test]
async fn test_find_accessed_slot_skips_accounts_without_code() {
    let holder = Address::repeat_byte(1);
    // Sorts after ArbOS, so without the filter the ArbOS slots are probed first.
    let token = Address::repeat_byte(0xfe);
    let arbos = address!("0xA4b05FffffFffFFFFfFFfffFfffFFfffFfFfFFFf");
    let mapping = balance_slot(holder, solidity(0));
    let asserter = Asserter::new();
    asserter.push_success(&prestate_of(&[
        (arbos, false, &[B256::repeat_byte(0xff), B256::repeat_byte(0xee)]),
        (token, true, &[mapping]),
    ]));
    // One probe only: the token's slot. A probe of ArbOS would take this response instead.
    asserter.push_success(&Bytes::from(sentinel_word()));

    let found = find_accessed_slot(&mocked_provider(&asserter), token, &[0x70])
        .await
        .expect("the token's slot answers");

    assert_eq!(found, (token, mapping));
    assert!(asserter.read_q().is_empty(), "no response left over");
}

/// A reflection token loops over a list in `balanceOf`; the sentinel written into that list's
/// length slot runs the call out of gas. That says the slot is not the mapping, and the probe
/// goes on.
#[tokio::test]
async fn test_find_accessed_slot_takes_out_of_gas_as_a_miss() {
    let holder = Address::repeat_byte(1);
    let contract = Address::repeat_byte(3);
    let mapping = balance_slot(holder, solidity(2));
    let asserter = Asserter::new();
    asserter.push_success(&prestate(contract, &[B256::repeat_byte(0xff), mapping]));
    asserter.push_failure(out_of_gas_payload());
    asserter.push_success(&Bytes::from(sentinel_word()));

    let found = find_accessed_slot(&mocked_provider(&asserter), Address::repeat_byte(9), &[0x70])
        .await
        .expect("the second candidate answers");

    assert_eq!(found, (contract, mapping));
}

/// A reflection token divides its stored word by a rate, so the sentinel reads back as zero and
/// neither view places the mapping. A large write reads back scaled down, and twice that write
/// reads back twice as much: that places the mapping and gives the rate.
#[tokio::test]
async fn test_discover_balance_places_a_scaled_mapping() {
    let holder = Address::repeat_byte(1);
    let token = Address::repeat_byte(9);
    let mapping = balance_slot(holder, solidity(1));
    let length_slot = B256::with_last_byte(5);
    let rate = U256::from(10_u8).pow(U256::from(51_u8));
    let asserter = Asserter::new();
    // `balanceOf`: the length slot runs out of gas, the mapping reads the sentinel back as zero.
    asserter.push_success(&prestate(token, &[mapping, length_slot]));
    asserter.push_success(&word(U256::ZERO));
    asserter.push_failure(out_of_gas_payload());
    // `sharesOf` does not exist: its trace reads no storage.
    asserter.push_success(&prestate(token, &[]));
    // The scaled probe traces `balanceOf` again and writes to the mapping only.
    asserter.push_success(&prestate(token, &[mapping, length_slot]));
    asserter.push_success(&word(SCALE_PROBE / rate));
    asserter.push_success(&word(SCALE_PROBE * U256::from(2_u8) / rate));

    let (storage_contract, position, encoding) =
        discover_balance(&mocked_provider(&asserter), token, holder)
            .await
            .expect("the scaled probe places the mapping");

    assert_eq!(storage_contract, token);
    assert_eq!(position, solidity(1));
    let BalanceEncoding::Scaled { rate: found } = encoding else {
        panic!("a scaled mapping, got {encoding:?}");
    };
    let relative_error = found.abs_diff(rate) * U256::from(1_000_000_u32) / rate;
    assert!(relative_error < U256::from(1_u8), "rate {found} for {rate}");
}

/// A token that loops over its holders reads more slots than the cap, with its mapping below
/// them in key order. The scaled probe keeps only slots that are the holder's mapping, so the cap
/// cannot push the mapping out.
#[tokio::test]
async fn test_discover_balance_reaches_a_scaled_mapping_behind_many_slots() {
    let holder = Address::repeat_byte(1);
    let token = Address::repeat_byte(9);
    let mapping = balance_slot(holder, solidity(2));
    // Decoys that sort above any hash, more of them than the cap.
    let mut slots: Vec<B256> = Vec::new();
    for index in 0..=MAX_SLOTS_TO_VERIFY {
        let mut decoy = [0xff_u8; 32];
        decoy[31] = u8::try_from(index).expect("the cap fits a byte");
        slots.push(B256::new(decoy));
    }
    slots.push(mapping);
    let rate = U256::from(1_000_u32);
    let asserter = Asserter::new();
    asserter.push_success(&prestate(token, &slots));
    for _ in 0..MAX_SLOTS_TO_VERIFY {
        asserter.push_success(&word(U256::ZERO));
    }
    asserter.push_success(&prestate(token, &[]));
    asserter.push_success(&prestate(token, &slots));
    asserter.push_success(&word(SCALE_PROBE / rate));
    asserter.push_success(&word(SCALE_PROBE * U256::from(2_u8) / rate));

    let (_, position, encoding) = discover_balance(&mocked_provider(&asserter), token, holder)
        .await
        .expect("the mapping is probed despite the cap");

    assert_eq!(position, solidity(2));
    assert_eq!(encoding, BalanceEncoding::Scaled { rate });
}

/// A slot whose write does not scale the answer in proportion feeds something other than the
/// balance, so it is not taken as one.
#[tokio::test]
async fn test_discover_balance_refuses_a_slot_that_does_not_scale_in_proportion() {
    let holder = Address::repeat_byte(1);
    let token = Address::repeat_byte(9);
    let mapping = balance_slot(holder, solidity(1));
    let asserter = Asserter::new();
    asserter.push_success(&prestate(token, &[mapping]));
    asserter.push_success(&word(U256::ZERO));
    asserter.push_success(&prestate(token, &[]));
    asserter.push_success(&prestate(token, &[mapping]));
    asserter.push_success(&word(U256::from(1_000_u32)));
    asserter.push_success(&word(U256::from(1_500_u32)));

    let error = discover_balance(&mocked_provider(&asserter), token, holder)
        .await
        .expect_err("no proportional mapping");

    assert!(matches!(error, DiscoveryError::Unsupported(_)), "{error:?}");
}

#[test]
fn test_balance_word_scales_and_caps() {
    let layout = TokenLayout::new(Address::ZERO, solidity(0), solidity(1));
    let funding = U256::from(10_u8).pow(U256::from(36_u8));

    assert_eq!(layout.balance_word(funding), funding, "a plain slot holds the balance");
    let scaled = layout.with_balance_encoding(BalanceEncoding::Scaled { rate: U256::from(7_u8) });
    assert_eq!(scaled.balance_word(funding), funding * U256::from(7_u8));
    let huge = layout.with_balance_encoding(BalanceEncoding::Scaled { rate: U256::MAX >> 8 });
    assert_eq!(huge.balance_word(funding), MAX_SCALED_WORD, "capped instead of overflowing");
}

/// Exercises the exact layouts that motivated the trace-guided path: USDT, whose storage the
/// sentinel probe could not place, and stETH, whose balance is derived from shares.
///
/// Asserts the property the discovery exists for -- writing the discovered slot changes what the
/// token reports -- rather than that two slots differ, which two keccak hashes always do.
/// Requires an endpoint serving `debug_traceCall`, so it stays opt-in.
#[tokio::test]
#[ignore = "requires RPC_URL with debug_traceCall support"]
async fn test_discovers_mainnet_usdt_and_steth_layouts() {
    let rpc_url = std::env::var("RPC_URL").expect("set RPC_URL for the live layout test");
    let provider = ProviderBuilder::default().connect_http(
        rpc_url
            .parse()
            .expect("RPC_URL must be a valid HTTP URL"),
    );
    let holder = address!("0x0000000000000000000000000000000000000001");
    let spender = address!("0x0000000000000000000000000000000000000002");
    let usdt = address!("0xdAC17F958D2ee523a2206206994597C13D831ec7");
    let steth = address!("0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84");

    for token in [usdt, steth] {
        let layout = discover_layout(&provider, token, holder, spender)
            .await
            .unwrap_or_else(|error| panic!("{token:#x} layout discovery failed: {error}"));
        let storage = layout.storage_contract();

        // A rebasing token keys its mapping on shares, so the slot answers `sharesOf` rather than
        // `balanceOf`; either view proving the write is what the funding override needs.
        let balance_views = [
            IERC20LayoutProbe::balanceOfCall { account: holder }.abi_encode(),
            ISharesToken::sharesOfCall { account: holder }.abi_encode(),
        ];
        let mut funds = false;
        for calldata in balance_views {
            funds |=
                slot_matches(&provider, token, storage, &calldata, layout.balance_slot(holder))
                    .await
                    .unwrap_or_else(|error| panic!("{token:#x} balance probe failed: {error}"));
        }
        assert!(funds, "{token:#x}: the discovered balance slot does not set the balance");

        let allowance_calldata =
            IERC20LayoutProbe::allowanceCall { owner: holder, spender }.abi_encode();
        let approves = slot_matches(
            &provider,
            token,
            storage,
            &allowance_calldata,
            layout.allowance_slot(holder, spender),
        )
        .await
        .unwrap_or_else(|error| panic!("{token:#x} allowance probe failed: {error}"));
        assert!(approves, "{token:#x}: the discovered allowance slot does not set the allowance");
    }
}

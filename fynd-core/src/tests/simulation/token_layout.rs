use alloy::{
    hex,
    primitives::{address, Address, U256},
    providers::ProviderBuilder,
    rpc::{client::RpcClient, json_rpc::ErrorPayload},
    transports::mock::Asserter,
};
use rstest::rstest;

use super::{fixtures::*, *};

/// OpenZeppelin v5's ERC-20 balances namespace, as ERC-7201 derives it from
/// "openzeppelin.storage.ERC20".
const OZ_V5_BALANCES_NS: [u8; 32] =
    hex!("52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace00");
/// Solady's ERC-20 seeds: the last four bytes of the word its slot is hashed from.
const SOLADY_BALANCE_SEED: [u8; 4] = hex!("87a211a2");
const SOLADY_ALLOWANCE_SEED: [u8; 4] = hex!("7f5e9f20");

fn usdc() -> Address {
    address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")
}

fn weth() -> Address {
    address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2")
}

/// The keccak256 inputs a Vyper mapping hashes: the base or the outer hash first, then the key.
fn vyper_preimages(base: [u8; 32], keys: &[Address]) -> Vec<Vec<u8>> {
    let mut preimages = Vec::new();
    let mut outer = base;
    for &key in keys {
        let preimage = [outer, padded(key)].concat();
        outer = keccak256(&preimage).0;
        preimages.push(preimage);
    }
    preimages
}

/// Solady packs the holder, the seed and the spender into one input rather than nesting hashes.
fn solady_preimages(keys: &[Address]) -> Vec<Vec<u8>> {
    let seed = if keys.len() == 1 { SOLADY_BALANCE_SEED } else { SOLADY_ALLOWANCE_SEED };
    let mut preimage = keys[0].to_vec();
    preimage.extend([0_u8; 8]);
    preimage.extend(seed);
    if let Some(spender) = keys.get(1) {
        preimage.extend(spender.as_slice());
    }
    vec![preimage]
}

/// A token that derives its namespace at runtime and reads an unrelated flag first: the chain
/// has a link with no key, and the trace holds a hash the chain does not use.
fn runtime_namespace_preimages(keys: &[Address]) -> Vec<Vec<u8>> {
    let namespace = b"token.storage.balances".to_vec();
    let mut preimages = vec![[padded(keys[0]), base_word(9)].concat(), namespace.clone()];
    preimages.extend(solidity_preimages(keccak256(&namespace).0, keys));
    preimages
}

/// Records the template whose slot is the last traced hash.
fn record<const KEYS: usize>(
    preimages: &[Vec<u8>],
    keys: &[Address; KEYS],
) -> Option<SlotTemplate<KEYS>> {
    let slot = keccak256(
        preimages
            .last()
            .expect("at least one hash"),
    );
    SlotTemplate::record(preimages, slot, keys)
}

fn assert_replays<const KEYS: usize>(
    convention: fn(&[Address]) -> Vec<Vec<u8>>,
    recorded: [Address; KEYS],
    asked: [Address; KEYS],
) {
    let template = record(&convention(&recorded), &recorded)
        .unwrap_or_else(|| panic!("the {KEYS}-key mapping records"));
    let expected = keccak256(
        convention(&asked)
            .last()
            .expect("at least one hash"),
    );
    assert_eq!(template.slot(&asked), expected, "{KEYS} keys");
}

/// Known-good hashes, computed outside this crate. They pin the replay arithmetic, so a slip in
/// where a key is written fails here rather than only against a live token.
#[rstest]
#[case(Address::ZERO, 0, hex!("ad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5"))]
#[case(usdc(), 0, hex!("c6521c8ea4247e8beb499344e591b9401fb2807ff9997dd598fd9e56c73a264d"))]
#[case(usdc(), 1, hex!("84893e0f271e5f8233d24aa85ba38e0d2ed8f0fc8f608c286ccee51e6c35dd6e"))]
fn test_balance_slot_vectors(
    #[case] holder: Address,
    #[case] base: u16,
    #[case] expected: [u8; 32],
) {
    let layout = solidity_layout(Address::ZERO, base, 0);
    assert_eq!(layout.balance_slot(holder).0, expected);
}

#[test]
fn test_allowance_slot_vector() {
    let layout = solidity_layout(Address::ZERO, 0, 0);
    assert_eq!(
        layout.allowance_slot(usdc(), weth()).0,
        hex!("7b7d28f4178b11583278450af3b85d49a04fd0597c53f7ed3fbfac3750fde37d")
    );
}

/// A template recorded with one set of keys gives the slot the token itself hashes for another.
#[rstest]
#[case::deep_solidity(|keys: &[Address]| solidity_preimages(base_word(516), keys))]
#[case::vyper(|keys: &[Address]| vyper_preimages(base_word(17), keys))]
#[case::openzeppelin_v5(|keys: &[Address]| solidity_preimages(OZ_V5_BALANCES_NS, keys))]
#[case::solady(solady_preimages)]
#[case::runtime_namespace(runtime_namespace_preimages)]
fn test_record_replays_with_other_keys(#[case] convention: fn(&[Address]) -> Vec<Vec<u8>>) {
    let recorded = [Address::repeat_byte(0x11), Address::repeat_byte(0x22)];
    assert_replays(convention, [recorded[0]], [usdc()]);
    assert_replays(convention, recorded, [usdc(), weth()]);
}

#[test]
fn test_record_of_a_slot_no_traced_hash_yields() {
    let holder = Address::repeat_byte(0x11);
    let preimages = solidity_preimages(base_word(0), &[holder]);
    assert_eq!(SlotTemplate::record(&preimages, B256::repeat_byte(0x99), &[holder]), None);
}

/// A slot that does not depend on the holder would fund one address for every holder.
#[test]
fn test_record_of_a_hash_without_the_key() {
    let preimages = vec![b"token.storage.supply".to_vec()];
    assert_eq!(record(&preimages, &[Address::repeat_byte(0x11)]), None);
}

/// An allowance chain that never hashes the spender would approve the probe spender's slot for
/// every real one.
#[test]
fn test_record_of_a_chain_without_the_spender() {
    let owner = Address::repeat_byte(0x11);
    let preimages = solidity_preimages(base_word(1), &[owner]);
    assert_eq!(record(&preimages, &[owner, Address::repeat_byte(0x22)]), None);
}

/// Two earlier hashes in one input leave no single chain to replay.
#[test]
fn test_record_of_an_input_holding_two_earlier_hashes() {
    let holder = Address::repeat_byte(0x11);
    let first = b"first".to_vec();
    let second = b"second".to_vec();
    let combined = [padded(holder), keccak256(&first).0, keccak256(&second).0].concat();
    assert_eq!(record(&[first, second, combined], &[holder]), None);
}

#[test]
fn test_template_writes_every_occurrence_of_a_key() {
    let holder = Address::repeat_byte(0x11);
    let preimages = vec![[padded(holder), padded(holder)].concat()];
    let template = record(&preimages, &[holder]).expect("the mapping records");
    assert_eq!(template.slot(&[usdc()]), keccak256([padded(usdc()), padded(usdc())].concat()));
}

#[test]
fn test_template_writes_every_occurrence_of_the_previous_hash() {
    let convention = |key: Address| {
        let inner = [padded(key), base_word(0)].concat();
        let hash = keccak256(&inner).0;
        vec![inner, [hash, hash].concat()]
    };
    let holder = Address::repeat_byte(0x11);
    let template = record(&convention(holder), &[holder]).expect("the mapping records");
    let expected = convention(usdc());
    assert_eq!(template.slot(&[usdc()]), keccak256(&expected[1]));
}

fn mocked_provider(asserter: &Asserter) -> RootProvider<Ethereum> {
    RootProvider::new(RpcClient::mocked(asserter.clone()))
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
    let contract = Address::repeat_byte(3);
    let mapping = B256::repeat_byte(0x99);
    // Sorted descending, so the decoy is probed first.
    let decoy = B256::repeat_byte(0xff);
    let asserter = Asserter::new();
    asserter.push_success(&prestate(contract, &[decoy, mapping]));
    asserter.push_success(&Bytes::from(vec![0_u8; 32]));
    asserter.push_success(&sentinel_response());

    let found = find_accessed_slot(&mocked_provider(&asserter), Address::repeat_byte(9), &[0x70])
        .await
        .expect("the second candidate answers");

    assert_eq!(found, AccessedSlot { storage_contract: contract, slot: mapping, value_shift: 0 });
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

/// A token that keeps flags in the low byte reports the stored word shifted right, and the probe
/// still has to recognise the slot.
#[tokio::test]
async fn test_find_accessed_slot_measures_a_shifted_value() {
    let contract = Address::repeat_byte(3);
    let mapping = B256::repeat_byte(0x99);
    let asserter = Asserter::new();
    asserter.push_success(&prestate(contract, &[mapping]));
    asserter.push_success(&Bytes::from(B256::from(PROBE_SENTINEL >> 8).to_vec()));

    let found = find_accessed_slot(&mocked_provider(&asserter), Address::repeat_byte(9), &[0x70])
        .await
        .expect("the shifted sentinel identifies the slot");

    assert_eq!(found, AccessedSlot { storage_contract: contract, slot: mapping, value_shift: 8 });
}

#[rstest]
#[case::exact(B256::from(PROBE_SENTINEL).to_vec(), Some(0))]
#[case::low_byte_dropped(B256::from(PROBE_SENTINEL >> 8).to_vec(), Some(8))]
#[case::other_value(B256::from(PROBE_SENTINEL * U256::from(3_u8)).to_vec(), None)]
#[case::zero(vec![0_u8; 32], None)]
#[case::short(vec![0xde, 0xad], None)]
fn test_read_value_shift(#[case] response: Vec<u8>, #[case] expected: Option<u8>) {
    assert_eq!(read_value_shift(&response), expected);
}

#[test]
fn test_encode_shifts_the_amount_into_place() {
    let mapping = Mapping::<1> { slot: SlotTemplate { steps: Vec::new() }, value_shift: 8 };
    let amount = U256::from(1_000_u32);
    assert_eq!(mapping.encode(amount), B256::from(amount << 8));
}

/// An amount too large for the shifted field is capped, and the cap keeps bit 255 clear.
#[rstest]
#[case::unshifted(0)]
#[case::shifted(8)]
fn test_encode_caps_below_the_top_bit(#[case] value_shift: u8) {
    let mapping = Mapping::<1> { slot: SlotTemplate { steps: Vec::new() }, value_shift };
    let stored = U256::from_be_bytes(mapping.encode(U256::MAX).0);
    assert!(!stored.bit(255), "{stored:#x}");
    assert_eq!(stored >> usize::from(value_shift), U256::MAX >> usize::from(value_shift + 1));
}

/// The input sits past the start of memory and between other steps, as a real trace has it.
#[tokio::test]
async fn test_trace_hash_preimages_reads_the_hashed_range() {
    let words =
        [hex::encode([0xaa_u8; 32]), hex::encode([0xbb_u8; 32]), hex::encode([0xcc_u8; 32])];
    let trace = serde_json::json!({
        "failed": false, "gas": 0, "returnValue": "0x",
        "structLogs": [
            { "pc": 0, "op": "MSTORE", "gas": 0, "gasCost": 0, "depth": 1, "stack": [] },
            { "pc": 1, "op": "KECCAK256", "gas": 0, "gasCost": 0, "depth": 1,
              "stack": ["0x40", "0x10"], "memory": words },
        ],
    });
    let asserter = Asserter::new();
    asserter.push_success(&trace);

    let preimages =
        trace_hash_preimages(&mocked_provider(&asserter), Address::repeat_byte(9), &[0x70])
            .await
            .expect("the trace is readable");

    let expected = [[0xaa_u8; 16].as_slice(), &[0xbb_u8; 32], &[0xcc_u8; 16]].concat();
    assert_eq!(preimages, vec![expected]);
}

/// Memory past the last word the trace shows reads as zeros, as the EVM expands it.
#[tokio::test]
async fn test_trace_hash_preimages_pads_past_the_end_of_memory() {
    let trace = serde_json::json!({
        "failed": false, "gas": 0, "returnValue": "0x",
        "structLogs": [
            { "pc": 1, "op": "KECCAK256", "gas": 0, "gasCost": 0, "depth": 1,
              "stack": ["0x40", "0x0"], "memory": [hex::encode([0xaa_u8; 32])] },
        ],
    });
    let asserter = Asserter::new();
    asserter.push_success(&trace);

    let preimages =
        trace_hash_preimages(&mocked_provider(&asserter), Address::repeat_byte(9), &[0x70])
            .await
            .expect("the trace is readable");

    assert_eq!(preimages, vec![[[0xaa_u8; 32], [0_u8; 32]].concat()]);
}

/// A node that ignores `enableMemory` says nothing about the token, so the token stays
/// undecided rather than being remembered as unsupported.
#[tokio::test]
async fn test_trace_hash_preimages_reports_rpc_without_memory() {
    let trace = serde_json::json!({
        "failed": false, "gas": 0, "returnValue": "0x",
        "structLogs": [
            { "pc": 1, "op": "KECCAK256", "gas": 0, "gasCost": 0, "depth": 1,
              "stack": ["0x40", "0x0"] },
        ],
    });
    let asserter = Asserter::new();
    asserter.push_success(&trace);

    let error = trace_hash_preimages(&mocked_provider(&asserter), Address::repeat_byte(9), &[0x70])
        .await
        .expect_err("the trace has no memory");

    assert!(matches!(error, DiscoveryError::Rpc(_)), "{error:?}");
}

/// Funding a token whose allowance lives in another contract would write the balance to one
/// address and the approval to another, so the layout is refused rather than half-applied.
#[tokio::test]
async fn test_discover_layout_refuses_split_storage() {
    let balance = solidity_preimages(base_word(0), &[PROBE_OWNER]);
    let allowance = solidity_preimages(base_word(1), &[PROBE_OWNER, PROBE_SPENDER]);
    let asserter = Asserter::new();
    for (contract, preimages) in [(3, balance), (4, allowance)] {
        let slot = keccak256(
            preimages
                .last()
                .expect("a mapping hashes"),
        );
        push_mapping(&asserter, Address::repeat_byte(contract), slot, &preimages);
    }

    let error = discover_layout(&mocked_provider(&asserter), Address::repeat_byte(9))
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
    let contract = Address::repeat_byte(3);
    let shares = solidity_preimages(base_word(0), &[PROBE_OWNER]);
    let asserter = Asserter::new();
    // The balance trace names a slot that does not key the answer.
    asserter.push_success(&hash_trace(&[]));
    asserter.push_success(&prestate(contract, &[B256::repeat_byte(0xff)]));
    asserter.push_failure(revert_payload());
    // The shares trace names the mapping itself.
    let slot = keccak256(shares.last().expect("a mapping hashes"));
    push_mapping(&asserter, contract, slot, &shares);

    let (storage_contract, mapping) =
        discover_balance(&mocked_provider(&asserter), Address::repeat_byte(9))
            .await
            .expect("the shares view places the mapping");

    assert_eq!(storage_contract, contract);
    assert_eq!(
        mapping.slot.slot(&[usdc()]),
        solidity_layout(Address::ZERO, 0, 0).balance_slot(usdc())
    );
}

/// Most tokens have no `sharesOf`, so its failure must not replace why `balanceOf` failed.
#[tokio::test]
async fn test_discover_balance_reports_every_view() {
    let contract = Address::repeat_byte(3);
    let asserter = Asserter::new();
    // `balanceOf` reads a slot that moves the answer but that no traced hash yields.
    push_mapping(&asserter, contract, B256::repeat_byte(0x99), &[]);
    // `sharesOf` reads nothing that moves the answer.
    asserter.push_success(&hash_trace(&[]));
    asserter.push_success(&prestate(contract, &[B256::repeat_byte(0xff)]));
    asserter.push_failure(revert_payload());

    let error = discover_balance(&mocked_provider(&asserter), Address::repeat_byte(9))
        .await
        .expect_err("neither view places the mapping");

    let DiscoveryError::Unsupported(reason) = error else {
        panic!("expected an unsupported layout, got {error:?}");
    };
    assert!(reason.contains("balanceOf: could not recover"), "{reason}");
    assert!(reason.contains("sharesOf: could not identify"), "{reason}");
}

/// What the token reports for `calldata` with `word` stored at `slot`, or `None` when the call
/// reverts, as a view the token does not have does.
async fn reported_with(
    provider: &RootProvider<Ethereum>,
    token: Address,
    storage: Address,
    calldata: &[u8],
    (slot, word): (B256, B256),
) -> Option<U256> {
    match provider
        .call(token_call(token, calldata))
        .overrides(state_override_single(storage, slot, word))
        .await
    {
        Ok(response) => Some(U256::from_be_slice(&response[..32])),
        Err(error)
            if error
                .as_error_resp()
                .is_some_and(is_revert) =>
        {
            None
        }
        Err(error) => panic!("{token:#x} read-back call failed: {error}"),
    }
}

/// Exercises layouts that motivated the trace-guided path: USDT, whose storage the sentinel
/// probe could not place; stETH, whose balance is derived from shares; mUSD, whose mappings sit
/// under its own ERC-7201 namespace; CULT, a Solady token; and AUSD, which keeps flags in the
/// low byte of each balance word.
///
/// Asserts the property the discovery exists for -- storing the encoded amount makes the token
/// report that amount -- for holders other than the probe keys discovery traced, which is what
/// the replayed template has to get right. Requires an endpoint serving `debug_traceCall`, so it
/// stays opt-in.
#[tokio::test]
#[ignore = "requires RPC_URL with debug_traceCall support"]
async fn test_discovers_mainnet_layouts() {
    let rpc_url = std::env::var("RPC_URL").expect("set RPC_URL for the live layout test");
    let provider = ProviderBuilder::default().connect_http(
        rpc_url
            .parse()
            .expect("RPC_URL must be a valid HTTP URL"),
    );
    // Short addresses, which the probe keys are chosen to avoid, so the replay is what places them.
    let holder = address!("0x0000000000000000000000000000000000000001");
    let spender = address!("0x0000000000000000000000000000000000000002");
    let amount = U256::from(123_456_789_000_u64);
    let tokens = [
        address!("0xdAC17F958D2ee523a2206206994597C13D831ec7"),
        address!("0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84"),
        address!("0xacA92E438df0B2401fF60dA7E4337B687a2435DA"),
        address!("0x0000000000c5dc95539589fbD24BE07c6C14eCa4"),
        address!("0x00000000eFE302BEAA2b3e6e1b18d08D69a9012a"),
    ];

    for token in tokens {
        let layout = discover_layout(&provider, token)
            .await
            .unwrap_or_else(|error| panic!("{token:#x} layout discovery failed: {error}"));
        let storage = layout.storage_contract();

        // A rebasing token keys its mapping on shares, so the slot answers `sharesOf` rather than
        // `balanceOf`; either view reporting the amount is what the funding override needs.
        let balance_views = [
            IERC20LayoutProbe::balanceOfCall { account: holder }.abi_encode(),
            ISharesToken::sharesOfCall { account: holder }.abi_encode(),
        ];
        let write = layout.encode_balance(holder, amount);
        let mut funds = false;
        for calldata in balance_views {
            funds |=
                reported_with(&provider, token, storage, &calldata, write).await == Some(amount);
        }
        assert!(funds, "{token:#x}: the discovered balance slot does not set the balance");

        let allowance_calldata =
            IERC20LayoutProbe::allowanceCall { owner: holder, spender }.abi_encode();
        let write = layout.encode_allowance(holder, spender, amount);
        let approved = reported_with(&provider, token, storage, &allowance_calldata, write).await;
        assert_eq!(
            approved,
            Some(amount),
            "{token:#x}: the discovered slot does not set the allowance"
        );
    }
}

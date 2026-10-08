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

fn namespaced(base: B256) -> MappingPosition {
    MappingPosition::Namespaced { base }
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
    let balance = balance_slot(usdc(), namespaced(OZ_V5_BALANCES_NS));
    let allowance = allowance_slot(usdc(), weth(), namespaced(OZ_V5_ALLOWANCES_NS));
    for base in 0..=MAX_STANDARD_BASE {
        assert_ne!(balance, balance_slot(usdc(), solidity(base)));
        assert_ne!(allowance, allowance_slot(usdc(), weth(), solidity(base)));
    }
}

/// ERC-7201: the hash of the name, minus one, hashed again, with the low byte cleared.
fn erc7201_root(name: &str) -> U256 {
    let name_hash = keccak256(name.as_bytes());
    let encoded = (U256::from_be_bytes(*name_hash) - U256::from(1_u8)).to_be_bytes::<32>();
    let mut root = *keccak256(encoded);
    root[31] = 0;
    U256::from_be_bytes(root)
}

/// The namespace is a constant here but a derivation in OpenZeppelin, so it is re-derived rather
/// than restated. Balances and allowances are fields 0 and 1 of `ERC20Storage`.
#[test]
fn test_openzeppelin_v5_namespaces_match_erc7201() {
    let root = erc7201_root("openzeppelin.storage.ERC20");
    assert_eq!(U256::from_be_bytes(*OZ_V5_BALANCES_NS), root);
    assert_eq!(U256::from_be_bytes(*OZ_V5_ALLOWANCES_NS), root + U256::from(1_u8));
}

/// Balances and allowances are fields 4 and 5 of `B20CoreStorage`.
#[test]
fn test_b20_namespace_derivation() {
    let root = erc7201_root("base.b20");
    assert_eq!(U256::from_be_bytes(*B20_BALANCES_NS), root + U256::from(4_u8));
    assert_eq!(U256::from_be_bytes(*B20_ALLOWANCES_NS), root + U256::from(5_u8));
}

/// Animoca predates ERC-7201: its base is the name's hash minus one, with no second hash.
#[test]
fn test_animoca_namespace_derivation() {
    let base = U256::from_be_bytes(*keccak256(b"animoca.core.token.ERC20.ERC20.storage")) -
        U256::from(1_u8);
    assert_eq!(U256::from_be_bytes(*ANIMOCA_BALANCES_NS), base);
    assert_eq!(U256::from_be_bytes(*ANIMOCA_ALLOWANCES_NS), base + U256::from(1_u8));
}

/// Slots read from live Base traces of `balanceOf(0xc0ffee…4979)` and
/// `allowance(0xc0ffee…4979, 0x…beef)`: OpenUSD (B20), OFC (Animoca) and LFI (Solady).
#[rstest]
#[case::b20(
    namespaced(B20_BALANCES_NS),
    namespaced(B20_ALLOWANCES_NS),
    hex!("d4666ef49e7c6c6a37b3873bce5266884781ced670ce5ed2dcfbc78e2cd20706"),
    hex!("d524dc3d8422ac9498448108b07f9c43110f9a5385981596483772e407b4a087"),
)]
#[case::animoca(
    namespaced(ANIMOCA_BALANCES_NS),
    namespaced(ANIMOCA_ALLOWANCES_NS),
    hex!("abad80d1fe36c6fbade5c6bbcdfac77698a145d0b3ba32d1107f89dc1128fe27"),
    hex!("6a583ab0f4452d4d791a09f89017f8ad7cddbcd8baf297533afd4f198a30e1b7"),
)]
#[case::solady(
    MappingPosition::Solady,
    MappingPosition::Solady,
    hex!("8b6da50b2a928d7a8d7a1fee25fede2ee334b79973fb83f03fdf45abc1d31a12"),
    hex!("eff26bf61dd4e6cecc5d59debad89becaf6d85bdde35b8e89e2672b7564a3638"),
)]
fn test_traced_slot_vectors(
    #[case] balance: MappingPosition,
    #[case] allowance: MappingPosition,
    #[case] balance_vector: [u8; 32],
    #[case] allowance_vector: [u8; 32],
) {
    let holder = address!("0xc0ffee254729296a45a3885639ac7e10f9d54979");
    let spender = address!("0x000000000000000000000000000000000000beef");
    assert_eq!(balance_slot(holder, balance).0, balance_vector);
    assert_eq!(allowance_slot(holder, spender, allowance).0, allowance_vector);
}

#[rstest]
#[case::deep_solidity(
    MappingPosition::Direct { base: 516, key_order: KeyOrder::Solidity },
    MappingPosition::Direct { base: 517, key_order: KeyOrder::Solidity },
)]
#[case::shallow_solidity(solidity(0), solidity(1))]
#[case::vyper(
    MappingPosition::Direct { base: 17, key_order: KeyOrder::Vyper },
    MappingPosition::Direct { base: 18, key_order: KeyOrder::Vyper },
)]
#[case::openzeppelin_v5(namespaced(OZ_V5_BALANCES_NS), namespaced(OZ_V5_ALLOWANCES_NS))]
#[case::b20(namespaced(B20_BALANCES_NS), namespaced(B20_ALLOWANCES_NS))]
#[case::animoca(namespaced(ANIMOCA_BALANCES_NS), namespaced(ANIMOCA_ALLOWANCES_NS))]
#[case::solady(MappingPosition::Solady, MappingPosition::Solady)]
fn test_recover_position_round_trip(
    #[case] balance: MappingPosition,
    #[case] allowance: MappingPosition,
) {
    let owner = Address::repeat_byte(0x11);
    let spender = Address::repeat_byte(0x22);

    assert_eq!(
        recover_position(balance_slot(owner, balance), |candidate| balance_slot(owner, candidate)),
        Some(balance)
    );
    assert_eq!(
        recover_position(allowance_slot(owner, spender, allowance), |candidate| allowance_slot(
            owner, spender, candidate
        )),
        Some(allowance)
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

/// A prestate trace holding one contract and the storage slots the call read.
fn prestate(contract: Address, slots: &[B256]) -> serde_json::Value {
    serde_json::json!({ format!("{contract:#x}"): traced_account(Some("0x6000"), slots) })
}

fn traced_account(code: Option<&str>, slots: &[B256]) -> serde_json::Value {
    let storage: serde_json::Map<String, serde_json::Value> = slots
        .iter()
        .map(|slot| (format!("{slot:#x}"), serde_json::json!(format!("{:#x}", B256::ZERO))))
        .collect();
    match code {
        Some(code) => serde_json::json!({ "code": code, "storage": storage }),
        None => serde_json::json!({ "storage": storage }),
    }
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

/// ArbOS sorts before most tokens and has no code. The asserter holds one probe response, so a
/// probe of the ArbOS slot would take it, and the lookup would return ArbOS instead of the token.
/// A node either leaves the code out or reports it empty.
#[rstest]
#[case::code_left_out(None)]
#[case::code_empty(Some("0x"))]
#[tokio::test]
async fn test_find_accessed_slot_skips_accounts_without_code(#[case] arbos_code: Option<&str>) {
    let holder = Address::repeat_byte(1);
    let arbos = address!("0xA4b05FffffFffFFFFfFFfffFfffFFfffFfFfFFFf");
    let token = Address::repeat_byte(0xe9);
    let mapping = balance_slot(holder, solidity(0));
    let asserter = Asserter::new();
    asserter.push_success(&serde_json::json!({
        format!("{arbos:#x}"): traced_account(arbos_code, &[B256::repeat_byte(0xef)]),
        format!("{token:#x}"): traced_account(Some("0x6000"), &[mapping]),
    }));
    asserter.push_success(&Bytes::from(sentinel_word()));

    let found = find_accessed_slot(&mocked_provider(&asserter), token, &[0x70])
        .await
        .expect("the token's balance slot is found");

    assert_eq!(found, (token, mapping));
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

    let (storage_contract, position) =
        discover_balance(&mocked_provider(&asserter), Address::repeat_byte(9), holder)
            .await
            .expect("the shares view places the mapping");

    assert_eq!(storage_contract, contract);
    assert_eq!(position, solidity(0));
}

/// Live discovery against one chain, checked by writing the discovered slots.
///
/// Requires the case's RPC URL with `debug_traceCall` support, so it stays opt-in.
#[rstest]
// USDT, whose storage the sentinel probe could not place, and stETH, whose balance is derived
// from shares.
#[case::mainnet("RPC_URL", &[
    address!("0xdAC17F958D2ee523a2206206994597C13D831ec7"), // USDT
    address!("0xae7ab96520DE3A18E5e111B5EaAb095312D7fE84"), // stETH
])]
// Every call on an Arbitrum chain reads ArbOS state. The node refuses to override ArbOS state, so
// these tokens resolve only when discovery skips the ArbOS account.
#[case::arbitrum("ARBITRUM_RPC_URL", &[
    address!("0xaf88d065e77c8cC2239327C5EDb3A432268e5831"), // USDC
    address!("0xFd086bC7CD5C481DCC9C85ebE478A1C0b69FCbb9"), // USDT
    address!("0xFF970A61A04b1cA14834A43f5dE4533eBDDB5CC8"), // USDC.e
])]
#[case::robinhood("ROBINHOOD_RPC_URL", &[
    address!("0xe934e36a439c94017b64a3fece66af12099abf50"), // STONKBROKER, Solidity base 0
    address!("0x63ee32ac3077d1fbd8a77ebba2a6ed4b8e9c1e18"), // INU, Solady
])]
// Two tokens on B20 (Base's native standard), one on Animoca's ERC-20 library and one on Solady's.
#[case::base("BASE_RPC_URL", &[
    address!("0xb2000000000000000000002feb517dfec7415344"), // OpenUSD, B20
    address!("0xb200000000000000000000cfbdf64a8706a94a01"), // B20
    address!("0x752c5a95d202972e124390f30a50154409d3c858"), // OFC, Animoca
    address!("0x3722264ab15a1dfce5a5af89e6547f7949a8aba3"), // LFI, Solady
])]
#[tokio::test]
#[ignore = "requires the case's RPC URL with debug_traceCall support"]
async fn test_discover_layout_live(#[case] rpc_env: &str, #[case] tokens: &[Address]) {
    assert_discovered_layouts_fund_and_approve(rpc_env, tokens).await;
}

/// Checks that writing the discovered slots changes what the token reports, rather than comparing
/// two keccak hashes, which always differ.
async fn assert_discovered_layouts_fund_and_approve(rpc_env: &str, tokens: &[Address]) {
    let rpc_url = std::env::var(rpc_env).unwrap_or_else(|_| {
        panic!("set {rpc_env} to the chain's HTTP RPC URL for the live layout test")
    });
    let provider = ProviderBuilder::default().connect_http(
        rpc_url
            .parse()
            .unwrap_or_else(|_| panic!("{rpc_env} must be a valid HTTP URL")),
    );
    let holder = address!("0x0000000000000000000000000000000000000001");
    let spender = address!("0x0000000000000000000000000000000000000002");

    for &token in tokens {
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

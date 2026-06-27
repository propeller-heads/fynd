//! Fork integration test for [`EthCallRunner`].
//!
//! Exercises the `eth_simulateV1` path end-to-end against a real Ethereum RPC by simulating a
//! WETH `deposit()`: send 1 ETH to the WETH contract and read back the sender's WETH balance.
//! The output is deterministic (1 ETH in → 1 WETH out), so the test asserts an exact amount.
//!
//! `#[ignore]`d so it never runs in the default `cargo nextest run` (CI has no node). Run with:
//! `RPC_URL=<https-eth-rpc> cargo test -p fynd-tools-common --test eth_call_integration -- --ignored`.

use alloy::{primitives::Address, providers::ProviderBuilder};
use fynd_tools_common::eth_call::EthCallRunner;
use num_bigint::BigUint;

/// Canonical WETH9 contract on Ethereum mainnet.
const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
/// `deposit()` selector.
const DEPOSIT_CALLDATA: [u8; 4] = [0xd0, 0xe3, 0x0d, 0xb0];
/// 1 ETH in wei.
const ONE_ETH_WEI: &str = "1000000000000000000";

#[tokio::test]
#[ignore = "requires a live Ethereum RPC; run with --ignored and RPC_URL set"]
async fn eth_simulate_weth_deposit_returns_exact_amount() {
    let Ok(rpc_url) = std::env::var("RPC_URL") else {
        eprintln!("skipping eth_call fork test: RPC_URL not set");
        return;
    };

    let url = rpc_url
        .parse::<reqwest::Url>()
        .expect("RPC_URL must be a valid URL");
    let provider = ProviderBuilder::default().connect_http(url);
    let runner = EthCallRunner::new(provider);

    let block = runner
        .latest_block_hash()
        .await
        .expect("fetch latest block hash");

    let weth: Address = WETH.parse().unwrap();
    let value: BigUint = ONE_ETH_WEI.parse().unwrap();

    // token_in = native ETH (zero), token_out = WETH, router = WETH, calldata = deposit().
    let (amount, gas) = runner
        .execute(Address::ZERO, weth, weth, &DEPOSIT_CALLDATA, &value, block)
        .await
        .expect("execute should not error")
        .expect("deposit should produce a balance");

    assert_eq!(amount, value, "1 ETH deposited must yield exactly 1 WETH");
    assert!(gas.unwrap_or(0) > 21_000, "deposit must consume more than base gas");
}

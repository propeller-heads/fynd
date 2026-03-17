//! ERC-20 helpers: balance/allowance slot detection via eth_call + state overrides.
//!
//! Uses brute-force probing (slots 0..=20) which works on any node without the
//! debug namespace, unlike `EVMBalanceSlotDetector` which requires `debug_traceCall`.

use alloy::{
    network::Ethereum,
    primitives::{keccak256, map::B256HashMap, Address, Bytes as AlloyBytes, TxKind, B256, U256},
    providers::{Provider, RootProvider},
    rpc::types::{
        state::{AccountOverride, StateOverride},
        TransactionRequest,
    },
    sol,
    sol_types::SolCall,
};
use anyhow::bail;
use num_bigint::BigUint;

sol! {
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

pub const MAX_PROBE_SLOT: u64 = 20;

pub fn balance_slot_at(holder: Address, position: u64) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(holder.as_slice());
    buf[56..64].copy_from_slice(&position.to_be_bytes());
    keccak256(buf)
}

pub fn allowance_slot_at(owner: Address, spender: Address, position: u64) -> B256 {
    let inner = balance_slot_at(owner, position);
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(spender.as_slice());
    buf[32..64].copy_from_slice(inner.as_slice());
    keccak256(buf)
}

pub fn state_override_single(contract: Address, slot: B256, value: B256) -> StateOverride {
    let mut state_diff = B256HashMap::default();
    state_diff.insert(slot, value);
    let mut overrides = StateOverride::default();
    overrides
        .insert(contract, AccountOverride { state_diff: Some(state_diff), ..Default::default() });
    overrides
}

pub async fn find_balance_slot(
    provider: &RootProvider<Ethereum>,
    token: Address,
    holder: Address,
) -> anyhow::Result<u64> {
    let calldata = IERC20::balanceOfCall { account: holder }.abi_encode();
    let target = B256::from(U256::MAX);
    for position in 0..=MAX_PROBE_SLOT {
        let slot = balance_slot_at(holder, position);
        let result = provider
            .call(TransactionRequest {
                to: Some(TxKind::Call(token)),
                input: AlloyBytes::from(calldata.clone()).into(),
                ..Default::default()
            })
            .overrides(state_override_single(token, slot, target))
            .await?;
        if result.len() >= 32 && result[..32] == *target.as_slice() {
            return Ok(position);
        }
    }
    bail!(
        "could not detect balance slot for {token:#x} (tried 0..={MAX_PROBE_SLOT}); \
         the token may use a non-standard storage layout"
    )
}

pub async fn find_allowance_slot(
    provider: &RootProvider<Ethereum>,
    token: Address,
    owner: Address,
    spender: Address,
) -> anyhow::Result<u64> {
    let calldata = IERC20::allowanceCall { owner, spender }.abi_encode();
    let target = B256::from(U256::MAX);
    for position in 0..=MAX_PROBE_SLOT {
        let slot = allowance_slot_at(owner, spender, position);
        let result = provider
            .call(TransactionRequest {
                to: Some(TxKind::Call(token)),
                input: AlloyBytes::from(calldata.clone()).into(),
                ..Default::default()
            })
            .overrides(state_override_single(token, slot, target))
            .await?;
        if result.len() >= 32 && result[..32] == *target.as_slice() {
            return Ok(position);
        }
    }
    bail!(
        "could not detect allowance slot for {token:#x} (tried 0..={MAX_PROBE_SLOT}); \
         the token may use a non-standard storage layout"
    )
}

pub async fn read_erc20_allowance(
    provider: &RootProvider<Ethereum>,
    token: Address,
    owner: Address,
    spender: Address,
) -> anyhow::Result<BigUint> {
    let calldata = IERC20::allowanceCall { owner, spender }.abi_encode();
    let result = provider
        .call(TransactionRequest {
            to: Some(TxKind::Call(token)),
            input: AlloyBytes::from(calldata).into(),
            ..Default::default()
        })
        .await?;
    if result.len() < 32 {
        bail!("allowance() returned {} bytes, expected 32", result.len());
    }
    Ok(BigUint::from_bytes_be(&result[..32]))
}

#[cfg(test)]
mod tests {
    use alloy::hex;

    use super::*;

    #[test]
    fn balance_slot_zero_address_position_zero() {
        let slot = balance_slot_at(Address::ZERO, 0);
        // keccak256(0x00…00 ++ 0x00…00) — well-known value used in Solidity mapping proofs
        let expected = hex!("ad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5");
        assert_eq!(slot.0, expected);
    }

    #[test]
    fn balance_slot_usdc_position_zero() {
        let usdc: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        let slot = balance_slot_at(usdc, 0);
        let expected = hex!("c6521c8ea4247e8beb499344e591b9401fb2807ff9997dd598fd9e56c73a264d");
        assert_eq!(slot.0, expected);
    }

    #[test]
    fn balance_slot_position_changes_output() {
        let usdc: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        let slot0 = balance_slot_at(usdc, 0);
        let slot1 = balance_slot_at(usdc, 1);
        let expected1 = hex!("84893e0f271e5f8233d24aa85ba38e0d2ed8f0fc8f608c286ccee51e6c35dd6e");
        assert_ne!(slot0, slot1, "different positions must yield different slots");
        assert_eq!(slot1.0, expected1);
    }

    #[test]
    fn allowance_slot_usdc_weth_position_zero() {
        let usdc: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        let weth: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
            .parse()
            .unwrap();
        let slot = allowance_slot_at(usdc, weth, 0);
        let expected = hex!("7b7d28f4178b11583278450af3b85d49a04fd0597c53f7ed3fbfac3750fde37d");
        assert_eq!(slot.0, expected);
    }

    #[test]
    fn allowance_slot_differs_from_balance_slot() {
        let usdc: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        let weth: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
            .parse()
            .unwrap();
        assert_ne!(balance_slot_at(usdc, 0), allowance_slot_at(usdc, weth, 0));
    }

    #[test]
    fn allowance_slot_is_not_symmetric() {
        let usdc: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        let weth: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
            .parse()
            .unwrap();
        // allowance(owner, spender) != allowance(spender, owner)
        assert_ne!(allowance_slot_at(usdc, weth, 0), allowance_slot_at(weth, usdc, 0));
    }
}

//! ERC-20 balance/allowance storage-slot detection for `eth_call`/`eth_simulateV1` overrides.
//!
//! Brute-force probes standard Solidity mapping slots `0..=MAX_PROBE_SLOT` by writing a
//! sentinel value into a candidate slot and reading it back via `balanceOf` / `allowance`,
//! then falls through to the OpenZeppelin v5 namespaced storage layout used by upgradeable
//! ERC-20 proxies. Works on any node — unlike `EVMBalanceSlotDetector`, which needs the
//! `debug` namespace.
//!
//! The `find_*_slot` functions return the full computed slot (`B256`) so callers can write
//! directly to it via `state_diff`.

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

sol! {
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

const MAX_PROBE_SLOT: u64 = 20;

/// Sentinel written to a candidate storage slot during balance/allowance probing.
///
/// Deliberately avoids high bits so tokens that mask upper bits (e.g. USDC packs a
/// blacklist flag in bit 255 of its balance slot) still return this value exactly from
/// `balanceOf`/`allowance`, allowing an exact-match check.
const PROBE_SENTINEL: U256 = U256::from_limbs([0xdead_beef, 0, 0, 0]);

/// OZ v5 ERC-20 namespaced storage location for `_balances` (field 0 of `ERC20Storage`).
///
/// Computed as `keccak256(abi.encode(uint256(keccak256("openzeppelin.storage.ERC20")) - 1)) &
/// ~bytes32(uint256(0xff))`. Tokens using OpenZeppelin Upgradeable 5.x store their `_balances`
/// mapping here instead of slot 0.
const OZ_V5_BALANCES_NS: B256 =
    B256::new(alloy::hex!("52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace00"));

/// OZ v5 `_allowances` is field 1 in `ERC20Storage`, so namespace + 1.
const OZ_V5_ALLOWANCES_NS: B256 =
    B256::new(alloy::hex!("52c63247e1f47db19d5ce0460030c497f067ca4cebf71ba98eeadabe20bace01"));

/// Compute `keccak256(abi.encode(holder, position))` for a standard Solidity `mapping(address =>
/// ...)` at `position`.
pub fn balance_slot_at(holder: Address, position: u64) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(holder.as_slice());
    buf[56..64].copy_from_slice(&position.to_be_bytes());
    keccak256(buf)
}

/// Compute the balance slot using a full 32-byte mapping base slot (e.g. the OZ v5 namespace).
pub fn balance_slot_at_b256(holder: Address, base: B256) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(holder.as_slice());
    buf[32..64].copy_from_slice(base.as_slice());
    keccak256(buf)
}

/// Compute `keccak256(abi.encode(spender, keccak256(abi.encode(owner, position))))` for
/// a standard Solidity `mapping(address => mapping(address => ...))` at `position`.
pub fn allowance_slot_at(owner: Address, spender: Address, position: u64) -> B256 {
    let inner = balance_slot_at(owner, position);
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(spender.as_slice());
    buf[32..64].copy_from_slice(inner.as_slice());
    keccak256(buf)
}

/// Compute the allowance slot using a full 32-byte mapping base slot (e.g. the OZ v5 namespace).
pub fn allowance_slot_at_b256(owner: Address, spender: Address, base: B256) -> B256 {
    let inner = balance_slot_at_b256(owner, base);
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(spender.as_slice());
    buf[32..64].copy_from_slice(inner.as_slice());
    keccak256(buf)
}

/// Build a `StateOverride` that writes `value` to a single storage `slot` of `contract`.
pub fn state_override_single(contract: Address, slot: B256, value: B256) -> StateOverride {
    let mut state_diff = B256HashMap::default();
    state_diff.insert(slot, value);
    let mut overrides = StateOverride::default();
    overrides
        .insert(contract, AccountOverride { state_diff: Some(state_diff), ..Default::default() });
    overrides
}

/// Probe `token` for the storage slot that holds `holder`'s value under `calldata`, comparing
/// the read-back against `PROBE_SENTINEL`. Tries standard mapping positions `0..=MAX_PROBE_SLOT`
/// (via `standard_slot`) then the OZ v5 namespaced layout (`oz_slot`).
async fn probe_slot(
    provider: &RootProvider<Ethereum>,
    token: Address,
    calldata: &[u8],
    standard_slot: impl Fn(u64) -> B256,
    oz_slot: B256,
) -> anyhow::Result<Option<B256>> {
    let sentinel = B256::from(PROBE_SENTINEL);
    for position in 0..=MAX_PROBE_SLOT {
        let slot = standard_slot(position);
        if slot_matches(provider, token, calldata, slot, sentinel).await? {
            return Ok(Some(slot));
        }
    }
    if slot_matches(provider, token, calldata, oz_slot, sentinel).await? {
        return Ok(Some(oz_slot));
    }
    Ok(None)
}

/// Override `slot` of `token` with `sentinel`, then check whether `calldata` reads it back.
async fn slot_matches(
    provider: &RootProvider<Ethereum>,
    token: Address,
    calldata: &[u8],
    slot: B256,
    sentinel: B256,
) -> anyhow::Result<bool> {
    let result = provider
        .call(TransactionRequest {
            to: Some(TxKind::Call(token)),
            input: AlloyBytes::from(calldata.to_vec()).into(),
            ..Default::default()
        })
        .overrides(state_override_single(token, slot, sentinel))
        .await?;
    Ok(result.len() >= 32 && result[..32] == *sentinel.as_slice())
}

/// Find the storage slot that holds the ERC-20 balance of `holder` in `token`.
///
/// Returns the full computed slot (`B256`). Probes standard Solidity mapping positions
/// `0..=MAX_PROBE_SLOT`, then falls through to the OpenZeppelin v5 namespaced storage layout.
pub async fn find_balance_slot(
    provider: &RootProvider<Ethereum>,
    token: Address,
    holder: Address,
) -> anyhow::Result<B256> {
    let calldata = IERC20::balanceOfCall { account: holder }.abi_encode();
    let oz_slot = balance_slot_at_b256(holder, OZ_V5_BALANCES_NS);
    match probe_slot(provider, token, &calldata, |p| balance_slot_at(holder, p), oz_slot).await? {
        Some(slot) => Ok(slot),
        None => bail!(
            "could not detect balance slot for {token:#x} (tried standard 0..={MAX_PROBE_SLOT} \
             and OZ v5 namespace); the token may use a non-standard storage layout"
        ),
    }
}

/// Find the storage slot that holds the ERC-20 allowance of `owner` for `spender` in `token`.
///
/// Returns the full computed slot (`B256`). Probes standard Solidity nested-mapping positions
/// `0..=MAX_PROBE_SLOT`, then the OpenZeppelin v5 namespaced layout.
pub async fn find_allowance_slot(
    provider: &RootProvider<Ethereum>,
    token: Address,
    owner: Address,
    spender: Address,
) -> anyhow::Result<B256> {
    let calldata = IERC20::allowanceCall { owner, spender }.abi_encode();
    let oz_slot = allowance_slot_at_b256(owner, spender, OZ_V5_ALLOWANCES_NS);
    match probe_slot(provider, token, &calldata, |p| allowance_slot_at(owner, spender, p), oz_slot)
        .await?
    {
        Some(slot) => Ok(slot),
        None => bail!(
            "could not detect allowance slot for {token:#x} (tried standard 0..={MAX_PROBE_SLOT} \
             and OZ v5 namespace); the token may use a non-standard storage layout"
        ),
    }
}

#[cfg(test)]
mod tests {
    use alloy::hex;

    use super::*;

    #[test]
    fn balance_slot_zero_address_position_zero() {
        let slot = balance_slot_at(Address::ZERO, 0);
        // keccak256(0x00…00 ++ 0x00…00) — well-known value used in Solidity mapping proofs.
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

    #[test]
    fn oz_v5_balance_slot_differs_from_standard() {
        let usdc: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        // The OZ v5 namespaced layout must not collide with any standard position.
        let oz = balance_slot_at_b256(usdc, OZ_V5_BALANCES_NS);
        for position in 0..=MAX_PROBE_SLOT {
            assert_ne!(oz, balance_slot_at(usdc, position));
        }
    }

    #[test]
    fn oz_v5_allowance_slot_differs_from_standard() {
        let usdc: Address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .parse()
            .unwrap();
        let weth: Address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
            .parse()
            .unwrap();
        let oz = allowance_slot_at_b256(usdc, weth, OZ_V5_ALLOWANCES_NS);
        for position in 0..=MAX_PROBE_SLOT {
            assert_ne!(oz, allowance_slot_at(usdc, weth, position));
        }
    }
}

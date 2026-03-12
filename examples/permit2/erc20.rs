//! ERC-20 helpers for the permit2 example: on-chain reads and storage slot detection.

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

sol! {
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

/// Maximum storage slot position to probe when detecting balance/allowance slots.
const MAX_PROBE_SLOT: u64 = 20;

/// Find the storage slot position used by an ERC-20 token's `balances` mapping.
///
/// Probes slot positions 0..=[`MAX_PROBE_SLOT`] by applying a state override and calling
/// `balanceOf`. Returns the first position whose override is reflected in the return value.
pub async fn find_balance_slot(
    provider: &RootProvider<Ethereum>,
    token: Address,
    holder: Address,
) -> Result<u64, Box<dyn std::error::Error>> {
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
    Err(format!(
        "could not detect balance slot for {token:#x} (tried 0..={MAX_PROBE_SLOT}); \
         the token may use a non-standard storage layout"
    )
    .into())
}

/// Find the storage slot position used by an ERC-20 token's `allowances` mapping.
///
/// Probes slot positions 0..=[`MAX_PROBE_SLOT`] by applying a state override and calling
/// `allowance`. Returns the first position whose override is reflected in the return value.
pub async fn find_allowance_slot(
    provider: &RootProvider<Ethereum>,
    token: Address,
    owner: Address,
    spender: Address,
) -> Result<u64, Box<dyn std::error::Error>> {
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
    Err(format!(
        "could not detect allowance slot for {token:#x} (tried 0..={MAX_PROBE_SLOT}); \
         the token may use a non-standard storage layout"
    )
    .into())
}

/// Compute `keccak256(address_padded32 || uint256(position))` — the Solidity mapping slot
/// for a single-key `mapping(address => ...)` at storage base `position`.
pub fn balance_slot_at(holder: Address, position: u64) -> B256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(holder.as_slice());
    buf[56..64].copy_from_slice(&position.to_be_bytes());
    keccak256(buf)
}

/// Compute the Solidity slot for `allowances[owner][spender]` at storage base `position`.
///
/// Inner = `keccak256(owner_padded32 || uint256(position))`
/// Outer = `keccak256(spender_padded32 || inner)`
pub fn allowance_slot_at(owner: Address, spender: Address, position: u64) -> B256 {
    let inner = balance_slot_at(owner, position);
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(spender.as_slice());
    buf[32..64].copy_from_slice(inner.as_slice());
    keccak256(buf)
}

/// Build a `StateOverride` that sets a single storage slot on one contract.
fn state_override_single(contract: Address, slot: B256, value: B256) -> StateOverride {
    let mut state_diff = B256HashMap::default();
    state_diff.insert(slot, value);
    let mut overrides = StateOverride::default();
    overrides
        .insert(contract, AccountOverride { state_diff: Some(state_diff), ..Default::default() });
    overrides
}

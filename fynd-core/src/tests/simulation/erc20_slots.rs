use alloy::{hex, primitives::U256};

use super::*;

fn usdc() -> Address {
    "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
        .parse()
        .expect("valid address")
}

fn weth() -> Address {
    "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
        .parse()
        .expect("valid address")
}

#[test]
fn test_balance_slot_zero_address_position_zero() {
    assert_eq!(
        balance_slot_at(Address::ZERO, 0).0,
        hex!("ad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5")
    );
}

#[test]
fn test_balance_slot_usdc_position_zero() {
    assert_eq!(
        balance_slot_at(usdc(), 0).0,
        hex!("c6521c8ea4247e8beb499344e591b9401fb2807ff9997dd598fd9e56c73a264d")
    );
}

#[test]
fn test_balance_slot_position_changes_output() {
    assert_ne!(balance_slot_at(usdc(), 0), balance_slot_at(usdc(), 1));
    assert_eq!(
        balance_slot_at(usdc(), 1).0,
        hex!("84893e0f271e5f8233d24aa85ba38e0d2ed8f0fc8f608c286ccee51e6c35dd6e")
    );
}

#[test]
fn test_allowance_slot_usdc_weth_position_zero() {
    assert_eq!(
        allowance_slot_at(usdc(), weth(), 0).0,
        hex!("7b7d28f4178b11583278450af3b85d49a04fd0597c53f7ed3fbfac3750fde37d")
    );
}

#[test]
fn test_allowance_slot_differs_from_balance_slot() {
    assert_ne!(balance_slot_at(usdc(), 0), allowance_slot_at(usdc(), weth(), 0));
}

#[test]
fn test_allowance_slot_is_not_symmetric() {
    assert_ne!(allowance_slot_at(usdc(), weth(), 0), allowance_slot_at(weth(), usdc(), 0));
}

#[test]
fn test_oz_v5_balance_slot_differs_from_standard() {
    let slot = balance_slot_at_b256(usdc(), OZ_V5_BALANCES_NS);
    for position in 0..=MAX_PROBE_SLOT {
        assert_ne!(slot, balance_slot_at(usdc(), position));
    }
}

#[test]
fn test_oz_v5_allowance_slot_differs_from_standard() {
    let slot = allowance_slot_at_b256(usdc(), weth(), OZ_V5_ALLOWANCES_NS);
    for position in 0..=MAX_PROBE_SLOT {
        assert_ne!(slot, allowance_slot_at(usdc(), weth(), position));
    }
}

#[test]
fn test_oz_v5_balances_ns_matches_derivation() {
    let name_hash = keccak256(b"openzeppelin.storage.ERC20");
    let encoded = (U256::from_be_bytes(*name_hash) - U256::from(1_u8)).to_be_bytes::<32>();
    let mut derived = *keccak256(encoded);
    derived[31] = 0;
    assert_eq!(B256::new(derived), OZ_V5_BALANCES_NS);
}

#[test]
fn test_oz_v5_allowances_ns_is_balances_ns_plus_one() {
    assert_eq!(
        U256::from_be_bytes(*OZ_V5_ALLOWANCES_NS),
        U256::from_be_bytes(*OZ_V5_BALANCES_NS) + U256::from(1_u8)
    );
}

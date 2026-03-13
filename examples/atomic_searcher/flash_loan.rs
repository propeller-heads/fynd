//! Flash loan integration for capital-free arbitrage execution.
//!
//! Supports two tiers:
//! - Tier 1: UniV2/SushiV2 flash swap (near-zero extra gas, first pool in cycle)
//! - Tier 2: Balancer V2 flash loan (fallback, ~80-100k extra gas overhead)

use alloy::{
    primitives::{Address, Bytes, U256},
    sol,
    sol_types::SolCall,
};

// Balancer V2 Vault on Ethereum mainnet (used in Tier 1 select_flash_tier)
#[allow(dead_code)]
pub const BALANCER_VAULT: Address = Address::new([
    0xBA, 0x12, 0x22, 0x22, 0x22, 0x22, 0x8d, 0x8B, 0xa4, 0x45,
    0x95, 0x8a, 0x75, 0xa0, 0x70, 0x4d, 0x56, 0x6B, 0xF2, 0xC8,
]);

/// Extra gas overhead for Balancer flash loan (Tier 2).
/// Tier 1 (UniV2 flash swap) adds near-zero extra gas, offset by
/// eliminating the approval tx (~46k savings).
#[allow(dead_code)]
pub const BALANCER_FLASH_GAS_OVERHEAD: u64 = 100_000;

// ABI for the FlashArbExecutor contract
sol! {
    #[sol(rpc)]
    contract FlashArbExecutor {
        function executeFlashSwapV2(
            address pair,
            uint256 amountOut,
            bool zeroForOne,
            uint256 repayAmount,
            address router,
            bytes calldata routerCalldata
        ) external;

        function executeFlashLoan(
            address token,
            uint256 amount,
            address router,
            bytes calldata routerCalldata
        ) external;

        function rescueTokens(address token) external;
        function rescueETH() external;

        event FlashArbExecuted(
            uint8 tier,
            address token,
            uint256 borrowed,
            uint256 repaid,
            uint256 profit
        );
    }
}

/// Which flash tier to use for a given cycle.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum FlashTier {
    /// Tier 1: Flash swap from a UniV2/SushiV2 pair (first pool in cycle).
    V2FlashSwap {
        pair_address: Address,
        /// True if selling token0 for token1 (borrow token1).
        zero_for_one: bool,
        /// Amount of output token to borrow (output of first hop).
        amount_out: U256,
        /// Exact repay amount in input token (computed from reserves).
        repay_amount: U256,
    },
    /// Tier 2: Balancer V2 flash loan (works for any route).
    BalancerLoan,
}

/// Encode calldata for `executeFlashSwapV2` (Tier 1).
#[allow(dead_code)]
pub fn encode_flash_swap_v2_call(
    pair: Address,
    amount_out: U256,
    zero_for_one: bool,
    repay_amount: U256,
    router: Address,
    router_calldata: Bytes,
) -> Bytes {
    let call = FlashArbExecutor::executeFlashSwapV2Call {
        pair,
        amountOut: amount_out,
        zeroForOne: zero_for_one,
        repayAmount: repay_amount,
        router,
        routerCalldata: router_calldata,
    };
    Bytes::from(call.abi_encode())
}

/// Encode calldata for `executeFlashLoan` (Tier 2).
pub fn encode_flash_loan_call(
    token: Address,
    amount: U256,
    router: Address,
    router_calldata: Bytes,
) -> Bytes {
    let call = FlashArbExecutor::executeFlashLoanCall {
        token,
        amount,
        router,
        routerCalldata: router_calldata,
    };
    Bytes::from(call.abi_encode())
}

/// Compiled bytecode for deploying FlashArbExecutor.
/// Constructor takes one argument: address balancerVault.
#[allow(dead_code)]
pub const FLASH_ARB_BYTECODE: &str = include_str!(
    "contracts/FlashArbExecutor.bytecode"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_flash_loan_call_produces_valid_calldata() {
        let token = Address::ZERO;
        let amount = U256::from(1_000_000u64);
        let router = Address::ZERO;
        let router_calldata = Bytes::from(vec![0xab, 0xcd]);

        let calldata = encode_flash_loan_call(
            token, amount, router, router_calldata,
        );

        // Function selector for executeFlashLoan(address,uint256,address,bytes)
        assert!(calldata.len() > 4, "calldata should have selector + args");
        // First 4 bytes are the selector
        let selector = &calldata[..4];
        let expected_selector =
            &alloy::primitives::keccak256(
                b"executeFlashLoan(address,uint256,address,bytes)"
            )[..4];
        assert_eq!(selector, expected_selector);
    }

    #[test]
    fn encode_flash_swap_v2_call_produces_valid_calldata() {
        let pair = Address::ZERO;
        let amount_out = U256::from(500_000u64);
        let repay_amount = U256::from(600_000u64);
        let router = Address::ZERO;
        let router_calldata = Bytes::from(vec![0x12, 0x34]);

        let calldata = encode_flash_swap_v2_call(
            pair, amount_out, true, repay_amount, router, router_calldata,
        );

        assert!(calldata.len() > 4, "calldata should have selector + args");
        let selector = &calldata[..4];
        let expected_selector = &alloy::primitives::keccak256(
            b"executeFlashSwapV2(address,uint256,bool,uint256,address,bytes)"
        )[..4];
        assert_eq!(selector, expected_selector);
    }
}

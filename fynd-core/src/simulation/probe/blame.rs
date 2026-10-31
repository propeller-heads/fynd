//! Who a reverting probe swap points at: the pool, or a token it moves.

/// Errors that say a token refused to move, as `simulation/revert.rs` names them.
///
/// These are the token-side errors of the contracts the swap calls through: OpenZeppelin's ERC-20
/// and `SafeERC20` errors, Solady's `SafeTransferLib` errors, Permit2's, and the `require`
/// strings that Uniswap's transfer helpers raise.
const TOKEN_ERRORS: [&str; 17] = [
    "ApproveFailed",
    "ERC20InsufficientAllowance",
    "ERC20InsufficientBalance",
    "ERC20InvalidApprover",
    "ERC20InvalidReceiver",
    "ERC20InvalidSender",
    "ERC20InvalidSpender",
    "ETHTransferFailed",
    "FluidSafeTransferError",
    "InsufficientAllowance",
    "InsufficientBalance",
    "Permit2AmountOverflow",
    "Permit2Failed",
    "SafeERC20FailedDecreaseAllowance",
    "SafeERC20FailedOperation",
    "TransferFailed",
    "TransferFromFailed",
];

/// `require` strings of the same kind. Compared whole, so `STF` does not match inside a longer
/// word.
const TOKEN_REQUIRE_STRINGS: [&str; 3] = ["STF", "TRANSFER_FAILED", "TRANSFER_FROM_FAILED"];

/// Whom a reverting probe swap points at.
///
/// A token that fails to transfer, or taxes or blocks the transfer, makes every pool that holds
/// it revert, so the revert says nothing about the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RevertBlame {
    /// The pool or its venue reverted, or the revert has no recognised token cause.
    Pool,
    /// A token refused to move.
    Token,
}

impl RevertBlame {
    /// Reads the blame off a revert reason, as `QuoteSimulator` reports it.
    ///
    /// The reason names the error that reverted. It points at a token when it names one of:
    /// `ApproveFailed`, `ERC20InsufficientAllowance`, `ERC20InsufficientBalance`,
    /// `ERC20InvalidApprover`, `ERC20InvalidReceiver`, `ERC20InvalidSender`,
    /// `ERC20InvalidSpender`, `ETHTransferFailed`, `FluidSafeTransferError`,
    /// `InsufficientAllowance`, `InsufficientBalance`, `Permit2AmountOverflow`, `Permit2Failed`,
    /// `SafeERC20FailedDecreaseAllowance`, `SafeERC20FailedOperation`, `TransferFailed` and
    /// `TransferFromFailed`, or the `require` strings `STF`, `TRANSFER_FAILED` and
    /// `TRANSFER_FROM_FAILED`. Every other reason, such as a router or executor error, a pool's
    /// own `require`, a panic, an unknown selector and `FailedCall` (which names an erased
    /// cause), points at the pool.
    pub fn of_reason(reason: &str) -> Self {
        let names_token_error = reason
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|word| TOKEN_ERRORS.contains(&word) || TOKEN_REQUIRE_STRINGS.contains(&word));
        if names_token_error {
            Self::Token
        } else {
            Self::Pool
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::transfer_from(
        "simulation reverted: TransferFromFailed(TransferFromFailed)",
        RevertBlame::Token
    )]
    #[case::transfer("simulation reverted: TransferFailed(TransferFailed)", RevertBlame::Token)]
    #[case::erc20_with_arguments(
        "simulation reverted: ERC20InsufficientBalance(ERC20InsufficientBalance { sender: 0x42, \
         balance: 7, needed: 9 })",
        RevertBlame::Token
    )]
    #[case::safe_erc20(
        "simulation reverted: SafeERC20FailedOperation(SafeERC20FailedOperation { token: 0x1 })",
        RevertBlame::Token
    )]
    #[case::uniswap_transfer_string("simulation reverted: reverted: STF", RevertBlame::Token)]
    #[case::router_error(
        "simulation reverted: TychoRouter__NegativeSlippage(TychoRouter__NegativeSlippage { \
         amount: 99, minAmount: 100 })",
        RevertBlame::Pool
    )]
    #[case::executor_error(
        "simulation reverted: Dispatcher__SwapReverted(Dispatcher__SwapReverted { executor: 0x1 })",
        RevertBlame::Pool
    )]
    #[case::pool_require("simulation reverted: reverted: ds-math-sub-underflow", RevertBlame::Pool)]
    #[case::panic("simulation reverted: panic: code 0x11", RevertBlame::Pool)]
    #[case::unknown_selector(
        "simulation reverted: unknown error, selector 0xdeadbeef",
        RevertBlame::Pool
    )]
    #[case::erased_cause("simulation reverted: FailedCall(FailedCall)", RevertBlame::Pool)]
    #[case::vault_balance(
        "simulation reverted: Vault__InsufficientBalance(Vault__InsufficientBalance { user: 0x1 })",
        RevertBlame::Pool
    )]
    #[case::name_inside_a_longer_word("reverted: NOSTFOUND", RevertBlame::Pool)]
    #[case::empty("", RevertBlame::Pool)]
    fn test_of_reason(#[case] reason: &str, #[case] expected: RevertBlame) {
        assert_eq!(RevertBlame::of_reason(reason), expected);
    }
}

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

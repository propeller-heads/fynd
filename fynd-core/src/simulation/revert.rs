//! Decoding of the revert a simulated router call produces.
//!
//! `eth_simulateV1` reports a revert as a status flag and a message: the payload that names the
//! error does not survive it, so a reverting quote arrives with nothing to explain it. The reason
//! is recovered here by tracing the same call and reading the frame that actually reverted.

use alloy::{rpc::types::trace::geth::CallFrame, sol, sol_types::SolInterface};

sol! {
    /// The custom errors of the `tycho-execution` version this crate pins.
    ///
    /// Copied from that version's Solidity sources rather than generated from an ABI it publishes,
    /// so the set goes stale when the contracts gain an executor or rename an error, and nothing
    /// here fails when it does. `test_router_error_selectors` pins a sample against selectors
    /// computed outside this declaration, which catches a wrong argument type; a newly added error
    /// is caught only by regenerating the list on a tycho version bump. Until then it reports its
    /// selector, so a route that reverts on a newer contract stays diagnosable.
    ///
    /// Uniswap V4's `Currency` is a value type over `address` and is written as one here: the
    /// selector is taken from the ABI signature, which carries the underlying type.
    #[derive(Debug)]
    interface RouterErrors {
        error AerodromeV1Executor__InvalidDataLength();
        error BalancerV2Executor__InvalidDataLength();
        error BalancerV3Executor__InvalidDataLength();
        error BalancerV3Executor__SenderIsNotVault(address sender);
        error BebopExecutor__InvalidDataLength();
        error BebopExecutor__InvalidSelector();
        error BebopExecutor__InvalidTarget();
        error BebopExecutor__ZeroAddress();
        error BopAMMExecutor__InvalidDataLength();
        error BopAMMExecutor__ZeroSettlementAddress();
        error CurveExecutor__AddressZero();
        error CurveExecutor__InvalidDataLength();
        error CurveExecutor__TokenAddressZero();
        error Dispatcher__AddressZero();
        error Dispatcher__CallbackReverted(address executor);
        error Dispatcher__ExecutorAlreadyExists(address executor);
        error Dispatcher__ExecutorIsTimelocked(address executor);
        error Dispatcher__InvalidDataLength();
        error Dispatcher__NonContractExecutor();
        error Dispatcher__SwapReverted(address executor);
        error Dispatcher__UnapprovedExecutor(address executor);
        error Dispatcher__UnsupportedSingleHopCycle(address token);
        error ERC4626Executor__InvalidDataLength();
        error ERC4626Executor__InvalidTarget();
        error EkuboExecutor__AddressZero();
        error EkuboExecutor__CoreOnly();
        error EkuboExecutor__InvalidDataLength();
        error EkuboExecutor__UnknownCallback();
        error EkuboV3Executor__CoreOnly();
        error EkuboV3Executor__InvalidDataLength();
        error EkuboV3Executor__UnknownCallback();
        error EtherfiExecutor__InvalidDataLength();
        error EtherfiExecutor__InvalidDirection();
        error EtherfiExecutor__NotAContract();
        error EtherfiExecutor__ZeroAddress();
        error FeeCalculator__AddressZero();
        error FeeCalculator__FeeTooHigh();
        error FermiSwapExecutor__AmountTooLarge();
        error FermiSwapExecutor__InvalidDataLength();
        error FermiSwapExecutor__ZeroSwapperAddress();
        error FluidV1Executor__InvalidCallback();
        error FluidV1Executor__InvalidDataLength();
        error FluidV1Executor__ZeroLiquidityAddress();
        error HashflowExecutor__InvalidDataLength();
        error HashflowExecutor__InvalidHashflowRouter();
        error LiquoriceExecutor__AmountBelowMinimum();
        error LiquoriceExecutor__InvalidDataLength();
        error LiquoriceExecutor__InvalidSelector();
        error LiquoriceExecutor__NotAContract();
        error LiquoriceExecutor__ZeroAddress();
        error LunarBaseExecutor__InvalidDataLength();
        error MaverickV2Executor__InvalidDataLength();
        error MetricExecutor__AmountInTooLarge();
        error MetricExecutor__InvalidCallback();
        error MetricExecutor__InvalidDataLength();
        error MetricExecutor__InvalidOracle();
        error MetricExecutor__InvalidOracleUpdateFlag();
        error NativeWrapExecutor__InvalidDataLength();
        error NativeWrapExecutor__ZeroAddress();
        error PropAMMExecutor__InvalidDataLength();
        error PropAMMFallbackExecutor__InvalidDataLength();
        error RingSwapV2Executor__InsufficientLiquidity();
        error RingSwapV2Executor__InvalidDataLength();
        error RingSwapV2Executor__InvalidFewToken(address token, address fwToken);
        error RingSwapV2Executor__InvalidPair(address pair, address fwTokenIn, address fwTokenOut);
        error RingSwapV2Executor__ZeroFewFactory();
        error RingSwapV2Executor__ZeroRingSwapFactory();
        error RocketpoolExecutor__InvalidDataLength();
        error SlipstreamsExecutor__InvalidDataLength();
        error TransferManager__AddressZero();
        error TransferManager__DifferentTokenIn(address tokenIn, address tokenInStorage);
        error TransferManager__ExceededTransferFromAllowance(uint256 allowedAmount, uint256 amountAttempted);
        error TransferManager__NotAContract(address addr);
        error TransferManager__UnknownTransferType();
        error TychoRouter__AddressZero();
        error TychoRouter__AmountOutZero();
        error TychoRouter__EmptySwaps();
        error TychoRouter__ExpiredClientSignature(uint256 deadline, uint256 blockTimestamp);
        error TychoRouter__FeesExceedOutput(uint256 totalFees, uint256 actualAmountOut);
        error TychoRouter__InvalidClientSignature();
        error TychoRouter__InvalidDataLength();
        error TychoRouter__InvalidMinAmountOut(uint256 minAmountOut, uint256 expectedAmountOut);
        error TychoRouter__MsgValueDoesNotMatchAmountIn(uint256 msgValue, uint256 amountIn);
        error TychoRouter__NegativeOutputDelta(int256 amount);
        error TychoRouter__NegativeSlippage(uint256 amount, uint256 minAmount);
        error TychoRouter__NoPendingFeeCalculator();
        error TychoRouter__NotAContract(address addr);
        error TychoRouter__TimelockNotExpired(uint256 activationTimestamp, uint256 blockTimestamp);
        error TychoRouter__ZeroInput();
        error UniswapV2Executor__InvalidDataLength();
        error UniswapV2Executor__InvalidFee();
        error UniswapV3Executor__InvalidDataLength();
        error UniswapV4Executor__DeltaNotNegative(address currency);
        error UniswapV4Executor__DeltaNotPositive(address currency);
        error UniswapV4Executor__InvalidAngstromAttestationDataLength(uint256 length);
        error UniswapV4Executor__InvalidDataLength();
        error UniswapV4Executor__NotPoolManager();
        error UniswapV4Executor__UnknownCallback(bytes4 selector);
        error UniswapV4Executor__V4TooMuchRequested(uint256 maxAmountInRequested, uint256 amountRequested);
        error UniswapV4Executor__ZeroAddressAngstromHook();
        error UniswapXFiller__AddressZero();
        error UniswapXFiller__BatchExecutionNotSupported();
        error Vault__AddressZero();
        error Vault__AmountZero();
        error Vault__InsufficientBalance(address user, address token, uint256 requested, uint256 available);
        error Vault__InvalidInputDelta(address token, int256 expected, int256 actual);
        error Vault__UnexpectedInputDelta(int256 inputDelta);
        error Vault__UnexpectedNonZeroCount(uint256 nonZeroCount);
    }
}

sol! {
    /// Errors a swap reverts with that the Tycho router does not define: the token's, the venue's,
    /// and the ones the libraries the router calls through raise on its behalf.
    ///
    /// Held apart from [`RouterErrors`] because the list is not generated from a source this crate
    /// builds against. It holds the errors dev has seen, plus the rest of the two standard sets
    /// those belong to -- OpenZeppelin's `Address`, `SafeERC20` and ERC-6093, and Solady's
    /// `SafeTransferLib` -- since a swap that hits one of a set will hit its siblings.
    ///
    /// `FailedCall` deserves a word: OpenZeppelin's `Address.functionCall` raises it only when the
    /// call it made reverted with no data at all, so it names an erased cause rather than a cause.
    #[derive(Debug)]
    interface ExternalErrors {
        error ApproveFailed();
        error CannotSwapWhileLocked();
        error ERC20InsufficientAllowance(address spender, uint256 allowance, uint256 needed);
        error ERC20InsufficientBalance(address sender, uint256 balance, uint256 needed);
        error ERC20InvalidApprover(address approver);
        error ERC20InvalidReceiver(address receiver);
        error ERC20InvalidSender(address sender);
        error ERC20InvalidSpender(address spender);
        error ETHTransferFailed();
        error FailedCall();
        error FluidSafeTransferError(uint256 code);
        error InsufficientAllowance(uint256 allowance);
        error InsufficientBalance(uint256 balance, uint256 needed);
        error Permit2AmountOverflow();
        error Permit2Failed();
        error SafeERC20FailedDecreaseAllowance(address spender, uint256 currentAllowance, uint256 requestedDecrease);
        error SafeERC20FailedOperation(address token);
        error StaleUpdate();
        error TransferFailed();
        error TransferFromFailed();
    }
}

sol! {
    /// The two errors Solidity itself defines: `revert("...")` and an assertion failure.
    #[derive(Debug)]
    interface SolidityErrors {
        error Error(string reason);
        error Panic(uint256 code);
    }
}

/// Reads the reason out of a reverting call frame, descending to the frame that produced it.
///
/// A revert bubbles up the call tree, so the top frame carries only a generic message while the
/// error that names the cause sits at the bottom.
pub(crate) fn reason_from_frame(frame: &CallFrame) -> Option<String> {
    if !reverted(frame) {
        return None;
    }
    let deepest = reporting_frame(frame);
    if let Some(reason) = deepest
        .output
        .as_ref()
        .and_then(|output| decode_error(output))
    {
        return Some(reason);
    }
    if let Some(reason) = deepest.revert_reason.as_deref() {
        return Some(decode_hex_reason(reason));
    }
    if let Some(output) = deepest
        .output
        .as_ref()
        .filter(|output| !output.is_empty())
    {
        return Some(format!("0x{}", alloy::hex::encode(output)));
    }
    deepest.error.clone()
}

/// Whether a frame reverted, either with revert data or with a plain execution error.
fn reverted(frame: &CallFrame) -> bool {
    frame.revert_reason.is_some() || frame.error.is_some()
}

/// The frame whose revert is worth reporting.
///
/// A revert bubbles up the call tree, so the deepest reverting frame is where it started and the
/// ones above carry a generic message. The deepest frame that carries revert data wins, because
/// only data names the error. Descending past it would trade a named error for "execution
/// reverted": a frame can revert with nothing while the frame above it reverted with a selector.
fn reporting_frame(frame: &CallFrame) -> &CallFrame {
    deepest_frame_with_data(frame).unwrap_or_else(|| deepest_reverting_frame(frame))
}

/// The deepest reverting frame carrying at least a selector, if any frame does.
fn deepest_frame_with_data(frame: &CallFrame) -> Option<&CallFrame> {
    if !reverted(frame) {
        return None;
    }
    frame
        .calls
        .iter()
        .find_map(deepest_frame_with_data)
        .or_else(|| carries_data(frame).then_some(frame))
}

/// The deepest reverting frame, used when no frame carried revert data.
fn deepest_reverting_frame(frame: &CallFrame) -> &CallFrame {
    let mut current = frame;
    while let Some(next) = current
        .calls
        .iter()
        .find(|child| reverted(child))
    {
        current = next;
    }
    current
}

/// Whether a frame's revert output is long enough to hold a selector.
fn carries_data(frame: &CallFrame) -> bool {
    frame
        .output
        .as_ref()
        .is_some_and(|output| output.len() >= 4)
}

/// Names the error a revert payload carries, or reports its selector when the error is unknown.
///
/// Returns `None` for a payload too short to carry a selector, which leaves the caller its other
/// sources rather than reporting an empty revert as a named one.
pub(crate) fn decode_error(data: &[u8]) -> Option<String> {
    if data.len() < 4 {
        return None;
    }
    if let Ok(error) = SolidityErrors::SolidityErrorsErrors::abi_decode(data) {
        return Some(match error {
            SolidityErrors::SolidityErrorsErrors::Error(inner) => {
                format!("reverted: {}", inner.reason)
            }
            SolidityErrors::SolidityErrorsErrors::Panic(inner) => {
                format!("panic: code {:#x}", inner.code)
            }
        });
    }
    if let Ok(error) = RouterErrors::RouterErrorsErrors::abi_decode(data) {
        return Some(format!("{error:?}"));
    }
    if let Ok(error) = ExternalErrors::ExternalErrorsErrors::abi_decode(data) {
        return Some(format!("{error:?}"));
    }
    // An error this build does not know still reports its selector and its arguments, so it can be
    // looked up and read without another simulation.
    let arguments = &data[4..];
    if arguments.is_empty() {
        return Some(format!("unknown error, selector 0x{}", alloy::hex::encode(&data[..4])));
    }
    Some(format!(
        "unknown error, selector 0x{} with data 0x{}",
        alloy::hex::encode(&data[..4]),
        alloy::hex::encode(arguments)
    ))
}

/// Decodes a revert reason a node already rendered as a hex string, leaving other text as it is.
fn decode_hex_reason(raw: &str) -> String {
    if let Some(hex) = raw.strip_prefix("0x") {
        if let Ok(bytes) = alloy::hex::decode(hex) {
            if let Some(decoded) = decode_error(&bytes) {
                return decoded;
            }
        }
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use alloy::{
        primitives::{address, Address, Bytes, U256},
        sol_types::SolError,
    };
    use rstest::rstest;

    use super::*;

    fn frame(output: Option<Bytes>, error: Option<&str>, calls: Vec<CallFrame>) -> CallFrame {
        CallFrame { output, error: error.map(str::to_string), calls, ..Default::default() }
    }

    /// The error a route hits when the router pays less than the quote's floor. Naming it is the
    /// whole point of the tracer, so it has to survive the round trip.
    #[test]
    fn test_decode_error_names_a_router_error() {
        let encoded = RouterErrors::TychoRouter__NegativeSlippage {
            amount: U256::from(99u64),
            minAmount: U256::from(100u64),
        }
        .abi_encode();

        let decoded = decode_error(&encoded).expect("a router error decodes");

        assert!(decoded.contains("TychoRouter__NegativeSlippage"), "{decoded}");
        assert!(decoded.contains("99") && decoded.contains("100"), "{decoded}");
    }

    #[test]
    fn test_decode_error_reads_a_require_string() {
        let encoded =
            SolidityErrors::Error { reason: "ds-math-sub-underflow".to_string() }.abi_encode();

        assert_eq!(decode_error(&encoded).as_deref(), Some("reverted: ds-math-sub-underflow"));
    }

    #[test]
    fn test_decode_error_reads_a_panic() {
        let encoded = SolidityErrors::Panic { code: U256::from(0x11u64) }.abi_encode();

        assert_eq!(decode_error(&encoded).as_deref(), Some("panic: code 0x11"));
    }

    /// An error the contracts gained after this crate was generated. The selector is what makes it
    /// searchable, so it has to reach the caller rather than be dropped.
    /// Selectors taken from the signatures in the `tycho-execution` Solidity sources, not from
    /// this declaration. A test that encodes and decodes through the same declaration passes even
    /// when an argument type here has drifted from the contract's; this one does not.
    #[test]
    fn test_router_error_selectors_match_the_contracts() {
        let cases: [(Vec<u8>, [u8; 4]); 4] = [
            // error TychoRouter__NegativeSlippage(uint256 amount, uint256 minAmount);
            (
                RouterErrors::TychoRouter__NegativeSlippage {
                    amount: U256::ZERO,
                    minAmount: U256::ZERO,
                }
                .abi_encode(),
                alloy::hex!("fd56708c"),
            ),
            // error TychoRouter__EmptySwaps();
            (RouterErrors::TychoRouter__EmptySwaps {}.abi_encode(), alloy::hex!("bc0c932b")),
            // error TychoRouter__AmountOutZero();
            (RouterErrors::TychoRouter__AmountOutZero {}.abi_encode(), alloy::hex!("6fa417d5")),
            // error Dispatcher__SwapReverted(address executor);
            (
                RouterErrors::Dispatcher__SwapReverted { executor: Address::ZERO }.abi_encode(),
                alloy::hex!("2680f8fe"),
            ),
        ];

        for (encoded, selector) in cases {
            assert_eq!(encoded[..4], selector, "0x{}", alloy::hex::encode(&encoded[..4]));
        }
    }

    /// The selectors dev logged as unknown, so a regression puts them back to bare hex.
    #[test]
    fn test_decode_error_names_an_external_error() {
        let cases = [
            ("0xd6bda275", "FailedCall"),
            ("0x666a2814", "StaleUpdate"),
            ("0x90b8ec18", "TransferFailed"),
            ("0x7939f424", "TransferFromFailed"),
            ("0x1e8107a0", "CannotSwapWhileLocked"),
        ];

        for (selector, name) in cases {
            let data = alloy::hex::decode(selector).expect("a selector is hex");

            let decoded = decode_error(&data).expect("a selector decodes");

            assert!(decoded.contains(name), "{selector}: {decoded}");
        }
    }

    /// The token error a route hits when the router pulls more than the sender holds. It carries
    /// arguments, so it also proves the payload is read and not just the selector matched.
    #[test]
    fn test_decode_error_names_an_external_error_with_arguments() {
        let encoded = ExternalErrors::ERC20InsufficientBalance {
            sender: address!("0x0000000000000000000000000000000000000042"),
            balance: U256::from(7u64),
            needed: U256::from(9u64),
        }
        .abi_encode();

        let decoded = decode_error(&encoded).expect("a token error decodes");

        assert!(decoded.contains("ERC20InsufficientBalance"), "{decoded}");
        assert!(decoded.contains("7") && decoded.contains("9"), "{decoded}");
    }

    #[test]
    fn test_decode_error_reports_an_unknown_selector() {
        let decoded = decode_error(&[0xde, 0xad, 0xbe, 0xef]).expect("4 bytes is a selector");

        assert_eq!(decoded, "unknown error, selector 0xdeadbeef");
    }

    /// An unknown error's arguments are the only thing that says which pool or amount tripped it,
    /// so they travel with the selector rather than being cut off.
    #[test]
    fn test_decode_error_keeps_an_unknown_error_arguments() {
        let decoded = decode_error(&[0xde, 0xad, 0xbe, 0xef, 0x01, 0x02]).expect("a selector");

        assert_eq!(decoded, "unknown error, selector 0xdeadbeef with data 0x0102");
    }

    #[test]
    fn test_decode_error_without_a_selector() {
        assert_eq!(decode_error(&[]), None);
        assert_eq!(decode_error(&[0x01, 0x02, 0x03]), None);
    }

    fn reverting(output: Option<Vec<u8>>, calls: Vec<CallFrame>) -> CallFrame {
        frame(output.map(Bytes::from), Some("execution reverted"), calls)
    }

    /// Which frame of a reverting tree names the cause.
    ///
    /// A revert bubbles up, so the top frame usually carries only a generic message. The frame
    /// worth reporting is the deepest one that carries data: descending past it would trade a
    /// named error for "execution reverted", and stopping short of it would report the wrapper.
    #[rstest]
    // The error sits one level down, under a top frame that says only that it reverted.
    #[case::descends_to_the_error(
        reverting(None, vec![reverting(Some(RouterErrors::Dispatcher__SwapReverted {
            executor: address!("0x000000000000000000000000000000000000dEaD"),
        }.abi_encode()), vec![])]),
        "Dispatcher__SwapReverted"
    )]
    // A bare sibling comes first: taking the first reverting child would lose the name.
    #[case::prefers_the_child_carrying_data(
        reverting(None, vec![
            reverting(None, vec![]),
            reverting(Some(RouterErrors::TychoRouter__AmountOutZero {}.abi_encode()), vec![]),
        ]),
        "TychoRouter__AmountOutZero"
    )]
    // The child reverted with nothing: taking the deepest frame would throw the name away.
    #[case::keeps_an_ancestor_over_a_bare_child(
        reverting(
            Some(RouterErrors::TychoRouter__NegativeSlippage {
                amount: U256::from(99u64),
                minAmount: U256::from(100u64),
            }.abi_encode()),
            vec![reverting(None, vec![])],
        ),
        "TychoRouter__NegativeSlippage"
    )]
    // Two levels down, so the descent does not stop at the first frame carrying data.
    #[case::takes_the_deepest_error(
        reverting(
            Some(RouterErrors::TychoRouter__AmountOutZero {}.abi_encode()),
            vec![reverting(None, vec![reverting(
                Some(RouterErrors::TychoRouter__EmptySwaps {}.abi_encode()), vec![],
            )])],
        ),
        "TychoRouter__EmptySwaps"
    )]
    fn test_reason_from_frame_reports(#[case] top: CallFrame, #[case] expected: &str) {
        let reason = reason_from_frame(&top).expect("the tree reverted");

        assert!(reason.contains(expected), "{reason}");
    }

    #[test]
    fn test_reason_from_frame_of_a_call_that_did_not_revert() {
        assert_eq!(reason_from_frame(&frame(None, None, vec![])), None);
    }

    #[test]
    fn test_reason_from_frame_falls_back_to_the_frame_error() {
        let top = frame(None, Some("out of gas"), vec![]);

        assert_eq!(reason_from_frame(&top).as_deref(), Some("out of gas"));
    }

    /// geth reports a revert reason as a hex string on some paths; it names the same error.
    #[test]
    fn test_reason_from_frame_decodes_a_hex_revert_reason() {
        let encoded = RouterErrors::TychoRouter__EmptySwaps {}.abi_encode();
        let mut top = frame(None, Some("execution reverted"), vec![]);
        top.revert_reason = Some(format!("0x{}", alloy::hex::encode(&encoded)));

        let reason = reason_from_frame(&top).expect("the frame reverted");

        assert!(reason.contains("TychoRouter__EmptySwaps"), "{reason}");
    }

    #[test]
    fn test_reason_from_frame_keeps_a_plain_revert_reason() {
        let mut top = frame(None, Some("execution reverted"), vec![]);
        top.revert_reason = Some("execution reverted".to_string());

        assert_eq!(reason_from_frame(&top).as_deref(), Some("execution reverted"));
    }
}

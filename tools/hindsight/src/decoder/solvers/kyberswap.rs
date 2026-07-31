//! KyberSwap-specific calldata extraction.
//!
//! Kyber's solver API asks integrators to pass a `clientData` blob, which the router embeds
//! verbatim in the swap calldata: a flat JSON object carrying the integrator's name and — the
//! valuable part — the off-chain quoted output (`AmountOut`) the route was chosen on. The settled
//! amount tells us what the user got; the quote tells us what the solver promised at decision
//! time, which is the number a venue like Relay actually compared against ours.
//!
//! The same calldata names the integrator's fee recipients, which is how a frontend's cut out of
//! the swap is recovered even though the frontend itself is not in the address book.

use alloy::{
    primitives::{Address, U256},
    sol,
    sol_types::SolCall,
};

use crate::decoder::solvers::{SolverKnowledge, SwapIntent};

/// `KyberSwap` represents native ETH with this sentinel address rather than the zero address —
/// hindsight's convention — so it is normalized on the way out.
const KYBERSWAP_NATIVE: Address =
    alloy::primitives::address!("0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");

sol! {
    /// `MetaAggregationRouterV2.swap`'s parameter shape, verified against a live reverted trade
    /// (tx 0xd3b7ffae…, Base): decoding recovered `srcToken`/`dstToken`/`amount`/`minReturnAmount`
    /// matching the on-chain values exactly. Only the fields this decoder reads are commented;
    /// every field must still be declared for the tuple to decode at the right offsets.
    struct SwapDescriptionV2 {
        address srcToken;
        address dstToken;
        address[] srcReceivers;
        uint256[] srcAmounts;
        address[] feeReceivers;
        uint256[] feeAmounts;
        address dstReceiver;
        uint256 amount;
        uint256 minReturnAmount;
        uint256 flags;
        bytes permit;
    }

    struct SwapExecutionParams {
        address callTarget;
        address approveTarget;
        bytes targetData;
        SwapDescriptionV2 desc;
        bytes clientData;
    }

    function swap(SwapExecutionParams execution) external payable returns (uint256, uint256);

    function swapSimpleMode(
        address caller,
        SwapDescriptionV2 desc,
        bytes targetData,
        bytes clientData
    );
}

/// Native ETH is `Address::ZERO` in hindsight's convention.
fn normalize_native(token: Address) -> Address {
    if token == KYBERSWAP_NATIVE {
        Address::ZERO
    } else {
        token
    }
}

/// `KyberSwap`'s declared quote, scanned out of a swap frame's calldata: `AmountOut` and, when
/// present, `Timestamp`. `Source` is not read — nothing downstream consumes it.
///
/// The blob is plain ASCII JSON inside ABI-encoded bytes, so it is located by its `{"Source"`
/// marker rather than by decoding the router call — which also finds it when Kyber's calldata is
/// nested inside a wrapper's (Relay, `MetaMask`). The JSON is flat, so the object ends at the
/// first closing brace. Anything malformed or missing returns `None` — a quote is decoration, not
/// a hard fact, so its absence must never fail the swap intent it decorates.
fn declared_quote(input: &[u8]) -> Option<(U256, Option<u64>)> {
    const MARKER: &[u8] = b"{\"Source\"";
    let start = input
        .windows(MARKER.len())
        .position(|window| window == MARKER)?;
    let rest = &input[start..];
    let end = rest
        .iter()
        .position(|&byte| byte == b'}')?;
    let json: serde_json::Value = serde_json::from_slice(&rest[..=end]).ok()?;
    let amount_out = json
        .get("AmountOut")?
        .as_str()?
        .parse::<U256>()
        .ok()?;
    let timestamp = json
        .get("Timestamp")
        .and_then(serde_json::Value::as_u64);
    Some((amount_out, timestamp))
}

/// The `KyberSwap` solver.
pub(crate) struct Kyberswap;

impl SolverKnowledge for Kyberswap {
    /// Extract the trader's swap terms from a `swap` call's `SwapDescriptionV2`:
    /// `srcToken`/`dstToken` (native ETH normalized to `Address::ZERO`), `amount`, and the
    /// enforced floor `minReturnAmount` (the revert reads "Return amount is not enough" below
    /// it) — a proper ABI decode, not a byte scan, since the shape is a nested tuple, not
    /// word-aligned data. The hint is unused: `KyberSwap`'s fields are decoded by ABI position, not
    /// located by value. When the calldata also carries a `clientData` quote, it is attached; a
    /// missing or malformed one does not fail the intent.
    fn swap_intent(&self, input: &[u8], _amount_in_hint: Option<U256>) -> Option<SwapIntent> {
        let call = swapCall::abi_decode(input).ok()?;
        let desc = call.execution.desc;
        if desc.amount.is_zero() || desc.minReturnAmount.is_zero() {
            return None;
        }
        let intent = SwapIntent::new(
            normalize_native(desc.srcToken),
            normalize_native(desc.dstToken),
            desc.amount,
            desc.minReturnAmount,
        );
        Some(match declared_quote(input) {
            Some((amount_out, timestamp)) => intent.with_quote(amount_out, timestamp),
            None => intent,
        })
    }

    /// The integrator fee recipients Kyber's router is told to pay out of the swap.
    ///
    /// Read from the root call only. A wrapper that nests Kyber's calldata (Relay, `MetaMask`)
    /// owns the flow and accounts its own fee, so a nested fee is that venue's to report.
    fn fee_recipients(&self, input: &[u8]) -> Vec<Address> {
        if let Ok(call) = swapCall::abi_decode(input) {
            return call.execution.desc.feeReceivers;
        }
        if let Ok(call) = swapSimpleModeCall::abi_decode(input) {
            return call.desc.feeReceivers;
        }
        Vec::new()
    }
}

/// A `MetaAggregationRouterV2.swap` call paying `fee_receivers`, for tests here and in
/// `solvers::tests`. Only the fee tier is meaningful; the rest is the minimum a decode needs.
///
/// The layout is the one that decoded Base tx
/// 0x78c70ca665a6e5d15e2af5a5b497cb3c1eb1214000308f1f0e2eb8e7e8c63e69, whose declared receiver
/// 0x41ec04c3… is the address the trace shows taking 10% of the output.
#[cfg(test)]
pub(crate) fn swap_calldata(fee_receivers: Vec<alloy::primitives::Address>) -> Vec<u8> {
    use alloy::primitives::{Address, Bytes};
    swapCall {
        execution: SwapExecutionParams {
            callTarget: Address::ZERO,
            approveTarget: Address::ZERO,
            targetData: Bytes::default(),
            desc: SwapDescriptionV2 {
                srcToken: Address::ZERO,
                dstToken: Address::ZERO,
                srcReceivers: Vec::new(),
                srcAmounts: Vec::new(),
                feeReceivers: fee_receivers,
                feeAmounts: vec![U256::from(1000)],
                dstReceiver: Address::ZERO,
                amount: U256::ZERO,
                minReturnAmount: U256::ZERO,
                flags: U256::from(704),
                permit: Bytes::default(),
            },
            clientData: Bytes::default(),
        },
    }
    .abi_encode()
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;

    use super::*;

    /// The real clientData blob of tx 0xf25ceafd… (the audited Relay+KyberSwap trade).
    const BLOB: &str = "{\"Source\":\"relay\",\"AmountInUSD\":\"70329.579441\",\
        \"AmountOutUSD\":\"70313.631096\",\"AmountOut\":\"70400409935\",\
        \"RouteID\":\"64a9cae8zRtEfLCS:8eba9537dcRHuLNs\",\"Timestamp\":1783421726}";

    /// The blob as it appears live: raw ASCII surrounded by ABI-encoded calldata bytes.
    fn calldata_with(blob: &str) -> Vec<u8> {
        let mut input = vec![0xe2u8, 0x1f, 0xd0, 0xe9]; // selector + padding around the blob
        input.extend_from_slice(&[0u8; 96]);
        input.extend_from_slice(blob.as_bytes());
        input.extend_from_slice(&[0u8; 17]);
        input
    }

    #[test]
    fn test_declared_quote_real_relay_blob() {
        let (amount_out, timestamp) = declared_quote(&calldata_with(BLOB)).unwrap();
        assert_eq!(amount_out, U256::from(70_400_409_935u64));
        assert_eq!(timestamp, Some(1_783_421_726));
    }

    #[test]
    fn test_declared_quote_without_blob() {
        assert!(declared_quote(&calldata_with("")).is_none());
        assert!(declared_quote(&[]).is_none());
    }

    #[test]
    fn test_fee_recipients_from_swap_calldata() {
        let collector = address!("0x41ec04c311d54f787f9e6c83d3fc7036f572fea0");
        assert_eq!(Kyberswap.fee_recipients(&swap_calldata(vec![collector])), vec![collector]);
        // A swap with no integrator fee names nobody.
        assert!(Kyberswap
            .fee_recipients(&swap_calldata(Vec::new()))
            .is_empty());
    }

    #[test]
    fn test_fee_recipients_from_foreign_calldata() {
        // Kyber's blob nested in a wrapper's calldata is not a root Kyber call: that venue owns
        // the fee. Neither is an empty input.
        assert!(Kyberswap
            .fee_recipients(&calldata_with(BLOB))
            .is_empty());
        assert!(Kyberswap.fee_recipients(&[]).is_empty());
    }

    #[test]
    fn test_declared_quote_truncated_or_fieldless_blob() {
        // Truncated before the closing brace: no valid JSON object to parse.
        let truncated = &BLOB[..BLOB.len() - 20];
        assert!(declared_quote(&calldata_with(truncated)).is_none());
        // Well-formed but missing AmountOut.
        assert!(declared_quote(&calldata_with("{\"Source\":\"relay\"}")).is_none());
    }

    /// Build a `swap` call's calldata via the `sol!` types, mirroring how a real trade encodes.
    /// `client_data` is embedded verbatim as raw bytes, mirroring `KyberSwap`'s plain-ASCII-JSON
    /// blob inside the ABI-encoded field. Named apart from the module's `swap_calldata`, which
    /// varies the fee receivers instead of the swap terms.
    fn swap_calldata_with_terms(
        src: Address,
        dst: Address,
        amount: u64,
        min_return: u64,
        client_data: &str,
    ) -> Vec<u8> {
        let desc = SwapDescriptionV2 {
            srcToken: src,
            dstToken: dst,
            srcReceivers: vec![],
            srcAmounts: vec![],
            feeReceivers: vec![],
            feeAmounts: vec![],
            dstReceiver: Address::repeat_byte(0x77),
            amount: U256::from(amount),
            minReturnAmount: U256::from(min_return),
            flags: U256::ZERO,
            permit: vec![].into(),
        };
        let execution = SwapExecutionParams {
            callTarget: Address::repeat_byte(0x88),
            approveTarget: Address::ZERO,
            targetData: vec![].into(),
            desc,
            clientData: client_data.as_bytes().to_vec().into(),
        };
        swapCall { execution }.abi_encode()
    }

    #[test]
    fn test_swap_intent_round_trip() {
        let src = Address::repeat_byte(0x11);
        let dst = Address::repeat_byte(0x22);
        let intent = Kyberswap
            .swap_intent(&swap_calldata_with_terms(src, dst, 1_000_000, 990_000, ""), None)
            .unwrap();
        assert_eq!(intent.token_in, src);
        assert_eq!(intent.token_out, dst);
        assert_eq!(intent.amount_in, U256::from(1_000_000u64));
        assert_eq!(intent.min_amount_out, U256::from(990_000u64));
        // No clientData quote declared: the accessor falls back to the floor.
        assert_eq!(intent.quoted_amount_out(), U256::from(990_000u64));
        assert_eq!(intent.timestamp, None);
    }

    #[test]
    fn test_swap_intent_with_declared_quote() {
        let src = Address::repeat_byte(0x11);
        let dst = Address::repeat_byte(0x22);
        let intent = Kyberswap
            .swap_intent(&swap_calldata_with_terms(src, dst, 1_000_000, 990_000, BLOB), None)
            .unwrap();
        assert_eq!(intent.min_amount_out, U256::from(990_000u64));
        assert_eq!(intent.quoted_amount_out(), U256::from(70_400_409_935u64));
        assert_eq!(intent.timestamp, Some(1_783_421_726));
    }

    #[test]
    fn test_swap_intent_malformed_quote_does_not_fail_the_intent() {
        // clientData present but missing AmountOut: the ABI-decoded terms are still recovered,
        // the quote is just absent.
        let src = Address::repeat_byte(0x11);
        let dst = Address::repeat_byte(0x22);
        let intent = Kyberswap
            .swap_intent(
                &swap_calldata_with_terms(src, dst, 1_000_000, 990_000, "{\"Source\":\"relay\"}"),
                None,
            )
            .unwrap();
        assert_eq!(intent.quoted_amount_out(), U256::from(990_000u64));
        assert_eq!(intent.timestamp, None);
    }

    #[test]
    fn test_swap_intent_normalizes_native_eth() {
        let intent = Kyberswap
            .swap_intent(
                &swap_calldata_with_terms(
                    KYBERSWAP_NATIVE,
                    Address::repeat_byte(0x22),
                    1_000,
                    900,
                    "",
                ),
                None,
            )
            .unwrap();
        assert_eq!(intent.token_in, Address::ZERO);
        assert_eq!(intent.token_out, Address::repeat_byte(0x22));
    }

    #[test]
    fn test_swap_intent_zero_amounts_rejected() {
        let a = Address::repeat_byte(0x11);
        let b = Address::repeat_byte(0x22);
        assert!(Kyberswap
            .swap_intent(&swap_calldata_with_terms(a, b, 0, 900, ""), None)
            .is_none());
        assert!(Kyberswap
            .swap_intent(&swap_calldata_with_terms(a, b, 1_000, 0, ""), None)
            .is_none());
    }

    #[test]
    fn test_swap_intent_garbage_input() {
        assert!(Kyberswap
            .swap_intent(&[], None)
            .is_none());
        assert!(Kyberswap
            .swap_intent(&[0xde, 0xad, 0xbe, 0xef], None)
            .is_none());
        // A well-formed but unrelated call (KyberSwap's own clientData blob calldata) must not
        // decode as a `swap` execution.
        assert!(Kyberswap
            .swap_intent(&calldata_with(BLOB), None)
            .is_none());
    }
}

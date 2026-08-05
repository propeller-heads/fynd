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

use crate::decoder::solvers::{SolverKnowledge, SolverQuote};

sol! {
    /// `MetaAggregationRouterV2`'s swap description. Only `feeReceivers` is read; the rest are
    /// named to match the ABI so the decode lines up.
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

    function swap(SwapExecutionParams execution);

    function swapSimpleMode(
        address caller,
        SwapDescriptionV2 desc,
        bytes targetData,
        bytes clientData
    );
}

/// The `KyberSwap` solver.
pub(crate) struct Kyberswap;

impl SolverKnowledge for Kyberswap {
    /// Extract `KyberSwap`'s `clientData` quote from transaction calldata.
    ///
    /// The blob is plain ASCII JSON inside ABI-encoded bytes, so it is located by its `{"Source"`
    /// marker rather than by decoding the router call — which also finds it when Kyber's calldata
    /// is nested inside a wrapper's (Relay, `MetaMask`). The JSON is flat, so the object ends at
    /// the first closing brace. Anything malformed or missing returns `None`.
    fn embedded_quote(&self, input: &[u8], _amount_in: U256) -> Option<SolverQuote> {
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
        let source = json
            .get("Source")?
            .as_str()?
            .to_string();
        let timestamp = json
            .get("Timestamp")
            .and_then(serde_json::Value::as_u64);
        Some(SolverQuote { amount_out, source: Some(source), timestamp })
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
    fn test_real_relay_blob() {
        let quote = Kyberswap
            .embedded_quote(&calldata_with(BLOB), U256::ZERO)
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(70_400_409_935u64));
        assert_eq!(quote.source.as_deref(), Some("relay"));
        assert_eq!(quote.timestamp, Some(1_783_421_726));
    }

    #[test]
    fn test_calldata_without_blob() {
        assert!(Kyberswap
            .embedded_quote(&calldata_with(""), U256::ZERO)
            .is_none());
        assert!(Kyberswap
            .embedded_quote(&[], U256::ZERO)
            .is_none());
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
    fn test_truncated_or_fieldless_blob() {
        // Truncated before the closing brace: no valid JSON object to parse.
        let truncated = &BLOB[..BLOB.len() - 20];
        assert!(Kyberswap
            .embedded_quote(&calldata_with(truncated), U256::ZERO)
            .is_none());
        // Well-formed but missing AmountOut.
        assert!(Kyberswap
            .embedded_quote(&calldata_with("{\"Source\":\"relay\"}"), U256::ZERO)
            .is_none());
    }
}

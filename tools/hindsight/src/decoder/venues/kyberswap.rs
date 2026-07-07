//! KyberSwap-specific calldata extraction.
//!
//! Kyber's aggregator API asks integrators to pass a `clientData` blob, which the router embeds
//! verbatim in the swap calldata: a flat JSON object carrying the integrator's name and — the
//! valuable part — the off-chain quoted output (`AmountOut`) the route was chosen on. The settled
//! amount tells us what the user got; the quote tells us what the solver promised at decision
//! time, which is the number a client like Relay actually compared against ours.

use alloy::primitives::U256;

use crate::decoder::venues::SolverQuote;

/// Extract KyberSwap's `clientData` quote from transaction calldata.
///
/// The blob is plain ASCII JSON inside ABI-encoded bytes, so it is located by its `{"Source"`
/// marker rather than by decoding the router call — which also finds it when Kyber's calldata is
/// nested inside a wrapper's (Relay, MetaMask). The JSON is flat, so the object ends at the
/// first closing brace. Anything malformed or missing returns `None`.
pub(crate) fn embedded_quote(input: &[u8]) -> Option<SolverQuote> {
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

#[cfg(test)]
mod tests {
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
    fn extracts_real_relay_blob() {
        let quote = embedded_quote(&calldata_with(BLOB)).unwrap();
        assert_eq!(quote.amount_out, U256::from(70_400_409_935u64));
        assert_eq!(quote.source.as_deref(), Some("relay"));
        assert_eq!(quote.timestamp, Some(1_783_421_726));
    }

    #[test]
    fn declines_calldata_without_blob() {
        assert!(embedded_quote(&calldata_with("")).is_none());
        assert!(embedded_quote(&[]).is_none());
    }

    #[test]
    fn declines_truncated_or_fieldless_blob() {
        // Truncated before the closing brace: no valid JSON object to parse.
        let truncated = &BLOB[..BLOB.len() - 20];
        assert!(embedded_quote(&calldata_with(truncated)).is_none());
        // Well-formed but missing AmountOut.
        assert!(embedded_quote(&calldata_with("{\"Source\":\"relay\"}")).is_none());
    }
}

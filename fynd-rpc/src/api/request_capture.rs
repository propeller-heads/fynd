//! Builds the re-issuable, signature-free representation of a quote request for
//! the replay-capture log emitted by the `/v1/quote` handler.

use fynd_core::QuoteStatus;
use fynd_rpc_types::QuoteRequest;
use serde_json::Value;

/// Serializes `request` to a re-issuable JSON string with all encoding data removed.
///
/// `encoding_options` is the only place a request carries user signatures
/// (Permit2 / client-fee), and none of it is needed to replay routing against
/// live state. The request is serialized, then the `options.encoding_options`
/// key is dropped so signatures never reach the logs. The shared wire DTO is
/// left untouched (it has no `skip_serializing_if` on that field), and the
/// result still deserializes back into a [`QuoteRequest`] because serde treats
/// a missing `Option` field as `None`.
pub(crate) fn replay_json(request: &QuoteRequest) -> String {
    let mut value = match serde_json::to_value(request) {
        Ok(value) => value,
        Err(_) => return "<unserializable>".to_string(),
    };
    if let Some(options) = value
        .get_mut("options")
        .and_then(Value::as_object_mut)
    {
        options.remove("encoding_options");
    }
    value.to_string()
}

/// Stable snake_case code for a per-order [`QuoteStatus`], matching the wire
/// serialization. The wildcard guards the `#[non_exhaustive]` enum against new
/// variants added upstream.
pub(crate) fn quote_status_code(status: QuoteStatus) -> &'static str {
    match status {
        QuoteStatus::Success => "success",
        QuoteStatus::NoRouteFound => "no_route_found",
        QuoteStatus::InsufficientLiquidity => "insufficient_liquidity",
        QuoteStatus::Timeout => "timeout",
        QuoteStatus::NotReady => "not_ready",
        QuoteStatus::PriceCheckFailed => "price_check_failed",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use fynd_rpc_types::{
        Bytes, ClientFeeParams, EncodingOptions, Order, OrderSide, PermitDetails, PermitSingle,
        QuoteOptions, QuoteRequest,
    };
    use num_bigint::BigUint;

    use super::*;

    fn order() -> Order {
        Order::new(
            Bytes::from([0xAAu8; 20]),
            Bytes::from([0xBBu8; 20]),
            BigUint::from(1_000_000_000_000_000_000u64),
            OrderSide::Sell,
            Bytes::from([0xCCu8; 20]),
        )
    }

    fn request_with_signatures() -> QuoteRequest {
        let permit = PermitSingle::new(
            PermitDetails::new(
                Bytes::from([0xAAu8; 20]),
                BigUint::from(1u64),
                BigUint::from(2u64),
                BigUint::from(3u64),
            ),
            Bytes::from([0xDDu8; 20]),
            BigUint::from(9u64),
        );
        let fee = ClientFeeParams::new(
            100,
            Bytes::from([0xEEu8; 20]),
            BigUint::from(0u64),
            1_893_456_000,
            Bytes::from([0x11u8; 65]),
        );
        let encoding = EncodingOptions::new(0.005)
            .with_permit2(permit, Bytes::from([0x22u8; 65]))
            .with_client_fee_params(fee);
        let options = QuoteOptions::default()
            .with_timeout_ms(2000)
            .with_min_responses(1)
            .with_max_gas(BigUint::from(500_000u64))
            .with_encoding_options(encoding);
        QuoteRequest::new(vec![order()]).with_options(options)
    }

    #[test]
    fn replay_json_drops_encoding_and_signatures() {
        let json = replay_json(&request_with_signatures());
        assert!(!json.contains("encoding_options"), "json was: {json}");
        assert!(!json.contains("signature"), "json was: {json}");
        assert!(!json.contains("permit"), "json was: {json}");
        assert!(!json.contains("client_fee"), "json was: {json}");
        // Routing inputs preserved.
        assert!(json.contains("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), "json was: {json}");
        assert!(json.contains("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"), "json was: {json}");
    }

    #[test]
    fn replay_json_preserves_solve_options_and_round_trips() {
        let json = replay_json(&request_with_signatures());
        let reparsed: QuoteRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(reparsed.orders().len(), 1);
        assert_eq!(reparsed.orders()[0].token_in().as_ref(), [0xAAu8; 20]);
        assert_eq!(reparsed.orders()[0].token_out().as_ref(), [0xBBu8; 20]);
        assert_eq!(reparsed.options().timeout_ms(), Some(2000));
        assert_eq!(reparsed.options().min_responses(), Some(1));
        assert_eq!(reparsed.options().max_gas(), Some(&BigUint::from(500_000u64)));
        assert!(reparsed.options().encoding_options().is_none());
    }

    #[test]
    fn status_code_maps_known_variants() {
        use fynd_core::QuoteStatus;
        assert_eq!(quote_status_code(QuoteStatus::Success), "success");
        assert_eq!(quote_status_code(QuoteStatus::NoRouteFound), "no_route_found");
        assert_eq!(quote_status_code(QuoteStatus::InsufficientLiquidity), "insufficient_liquidity");
        assert_eq!(quote_status_code(QuoteStatus::Timeout), "timeout");
        assert_eq!(quote_status_code(QuoteStatus::NotReady), "not_ready");
        assert_eq!(quote_status_code(QuoteStatus::PriceCheckFailed), "price_check_failed");
    }
}

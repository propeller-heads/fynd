//! State that pods share through a `fynd-nexus` service, and the messages that carry it.
//!
//! The service holds one connection per source and keeps the latest state of each in memory. Pods
//! poll it over HTTP. Each response carries a [`NexusMessage`] as JSON.

pub(crate) mod client;
pub mod rfq;

use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// The protocol version this build writes and reads.
pub const PROTOCOL_VERSION: u32 = 1;

/// The current message of a nexus stream, for state of type `D`.
///
/// `epoch` changes each time the service starts and `seq` grows per stream, so a reader can tell a
/// replayed or reordered message from a new one. In JSON the version is the field `v` and the
/// body's fields sit beside `epoch` and `seq`, with `type` naming the kind of body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NexusMessage<D> {
    /// The protocol version the message was written in.
    #[serde(rename = "v")]
    pub version: u32,
    /// Identifies the service run that sent the message.
    pub epoch: u64,
    /// The position of the message in its stream.
    pub seq: u64,
    /// The snapshot or withdrawal carried by the message.
    #[serde(flatten)]
    pub body: MessageBody<D>,
}

/// A snapshot or withdrawal carried by a [`NexusMessage`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessageBody<D> {
    /// The current state.
    Snapshot {
        /// When the service got the state from its source.
        as_of: DateTime<Utc>,
        /// The state.
        data: D,
    },
    /// The service has no usable state: what the source sent went stale, or its feed ended.
    Withdrawn,
}

/// Errors when decoding text as a [`NexusMessage`].
#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    /// The text is not a message of this protocol.
    #[error("malformed nexus message: {0}")]
    Malformed(#[from] serde_json::Error),
    /// The message uses a protocol version this build does not support.
    #[error("unsupported nexus protocol version {found}; this build reads {supported}")]
    UnsupportedVersion {
        /// The version in the message.
        found: u32,
        /// The version this build reads.
        supported: u32,
    },
}

/// Reads only the protocol version so decoding can report an unsupported version even when the
/// message body cannot be parsed.
#[derive(Deserialize)]
struct VersionField {
    v: u32,
}

impl<D> NexusMessage<D> {
    /// A message in this build's protocol version.
    pub fn new(epoch: u64, seq: u64, body: MessageBody<D>) -> Self {
        Self { version: PROTOCOL_VERSION, epoch, seq, body }
    }
}

impl<D: Serialize> NexusMessage<D> {
    /// The JSON text of this message.
    pub fn encode(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

impl<D: DeserializeOwned> NexusMessage<D> {
    /// Reads a message written by [`encode`](Self::encode).
    ///
    /// # Errors
    ///
    /// [`MessageError::UnsupportedVersion`] for an unsupported protocol version, and
    /// [`MessageError::Malformed`] when the text does not match this version's message format.
    pub fn decode(text: &str) -> Result<Self, MessageError> {
        let unsupported =
            |found| MessageError::UnsupportedVersion { found, supported: PROTOCOL_VERSION };
        match serde_json::from_str::<Self>(text) {
            Ok(message) if message.version == PROTOCOL_VERSION => Ok(message),
            Ok(message) => Err(unsupported(message.version)),
            Err(error) => {
                let found = serde_json::from_str::<VersionField>(text)
                    .ok()
                    .map(|field| field.v)
                    .filter(|version| *version != PROTOCOL_VERSION);
                Err(match found {
                    Some(version) => unsupported(version),
                    None => MessageError::Malformed(error),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use num_bigint::BigUint;
    use rstest::rstest;
    use serde_json::json;
    use tycho_simulation::{
        book::{Levels, PriceLevel},
        pamm::protocols::metric::{
            MetricBidAskResponse, MetricMetadata, MetricPoolPrice, MetricPrices,
        },
        rfq::protocols::{
            bebop::{BebopBook, BebopPrices},
            hashflow::{HashflowMarketMakerLevels, HashflowPair, HashflowPrices},
        },
        tycho_core::Bytes,
    };

    use super::*;

    const EPOCH: u64 = 1_761_550_000_000;

    fn levels(price: f64, quantity: f64) -> Levels {
        Levels::new(vec![PriceLevel { price, quantity }]).unwrap()
    }

    fn bebop_prices(bid: f64, ask: f64) -> BebopPrices {
        BebopPrices {
            pairs: vec![BebopBook {
                base: Bytes::from([0xc0; 20]),
                quote: Bytes::from([0xa0; 20]),
                last_update_ts: 1_761_550_000_123,
                bids: levels(bid, 0.325717),
                asks: levels(ask, 0.325717),
            }],
        }
    }

    fn hashflow_prices() -> HashflowPrices {
        let pair = HashflowPair {
            base_token: Bytes::from([0xc0; 20]),
            quote_token: Bytes::from([0xa0; 20]),
        };
        HashflowPrices {
            levels_by_maker: HashMap::from([(
                "mm1".to_string(),
                vec![HashflowMarketMakerLevels { pair, levels: levels(2999.1, 1.5) }],
            )]),
        }
    }

    fn metric_prices() -> MetricPrices {
        MetricPrices {
            pools: vec![MetricPoolPrice {
                metadata: MetricMetadata {
                    pool_address: Bytes::from([0x11; 20]),
                    token0: Bytes::from([0xc0; 20]),
                    token1: Bytes::from([0xa0; 20]),
                    tvl_fiat: Some(1_234_567.89),
                    pause_level: Some(0),
                },
                bid_ask: MetricBidAskResponse {
                    bid_adj: BigUint::from(u128::MAX),
                    ask_adj: BigUint::from(1u8),
                    total_token0_available: Some(BigUint::from(7u8)),
                    total_token1_available: None,
                    server_ts: 1_761_550_000,
                    price_provider_status: Some("healthy".to_string()),
                    depth: Default::default(),
                },
            }],
        }
    }

    fn snapshot<D>(data: D) -> NexusMessage<D> {
        let as_of = "2026-10-27T09:00:00.123456Z"
            .parse()
            .unwrap();
        NexusMessage::new(EPOCH, 42, MessageBody::Snapshot { as_of, data })
    }

    fn round_trip<D>(message: &NexusMessage<D>) -> NexusMessage<D>
    where
        D: Serialize + DeserializeOwned,
    {
        NexusMessage::decode(&message.encode().unwrap()).unwrap()
    }

    #[rstest]
    #[case::snapshot(snapshot(json!({"x": [1, 2.5, null]})))]
    #[case::withdrawn(NexusMessage::new(EPOCH, 43, MessageBody::Withdrawn))]
    fn test_round_trip_each_message_type(#[case] message: NexusMessage<serde_json::Value>) {
        assert_eq!(round_trip(&message), message);
    }

    #[test]
    fn test_round_trip_typed_snapshots() {
        let bebop = snapshot(bebop_prices(3000.5, 3001.5));
        let hashflow = snapshot(hashflow_prices());
        let metric = snapshot(metric_prices());

        assert_eq!(round_trip(&bebop), bebop);
        assert_eq!(round_trip(&hashflow), hashflow);
        assert_eq!(round_trip(&metric), metric);
    }

    #[rstest]
    #[case::widened_f32(f64::from(3000.1f32))]
    #[case::sum_of_decimals(0.1 + 0.2)]
    #[case::smallest_subnormal(f64::from_bits(1))]
    #[case::next_after_round(f64::from_bits(3000.0f64.to_bits() + 1))]
    fn test_round_trip_price_precision(#[case] price: f64) {
        let message = snapshot(bebop_prices(price, price));

        let MessageBody::Snapshot { data, .. } = round_trip(&message).body else {
            panic!("a snapshot must decode as a snapshot");
        };

        assert_eq!(data.pairs[0].bids[0].price.to_bits(), price.to_bits());
        assert_eq!(data.pairs[0].asks[0].price.to_bits(), price.to_bits());
    }

    #[rstest]
    #[case::snapshot(
        snapshot(json!({"x": 1})),
        json!({
            "v": 1,
            "type": "snapshot",
            "epoch": EPOCH,
            "seq": 42,
            "as_of": "2026-10-27T09:00:00.123456Z",
            "data": {"x": 1}
        })
    )]
    #[case::withdrawn(
        NexusMessage::new(7, 3, MessageBody::Withdrawn),
        json!({"v": 1, "type": "withdrawn", "epoch": 7, "seq": 3})
    )]
    fn test_encoded_json_shape(
        #[case] message: NexusMessage<serde_json::Value>,
        #[case] expected: serde_json::Value,
    ) {
        let text = message.encode().unwrap();

        let value: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(value, expected);
    }

    #[rstest]
    #[case::newer(r#"{"v":2,"type":"withdrawn","epoch":1,"seq":1}"#, 2)]
    #[case::zero(r#"{"v":0,"type":"withdrawn","epoch":1,"seq":1}"#, 0)]
    #[case::unreadable_body(r#"{"v":9,"type":"something_new","payload":[1]}"#, 9)]
    fn test_decode_unknown_version(#[case] text: &str, #[case] version: u32) {
        let error = NexusMessage::<serde_json::Value>::decode(text).unwrap_err();

        let MessageError::UnsupportedVersion { found, supported } = error else {
            panic!("expected an unsupported version error, got {error}");
        };
        assert_eq!((found, supported), (version, PROTOCOL_VERSION));
    }

    #[rstest]
    #[case::not_json("not json")]
    #[case::no_version(r#"{"type":"withdrawn","epoch":1,"seq":1}"#)]
    #[case::unknown_type(r#"{"v":1,"type":"goodbye","epoch":1,"seq":1}"#)]
    #[case::no_epoch(r#"{"v":1,"type":"withdrawn","seq":1}"#)]
    #[case::snapshot_without_data(
        r#"{"v":1,"type":"snapshot","epoch":1,"seq":1,"as_of":"2026-10-27T09:00:00Z"}"#
    )]
    #[case::snapshot_with_bad_timestamp(
        r#"{"v":1,"type":"snapshot","epoch":1,"seq":1,"as_of":"yesterday","data":1}"#
    )]
    fn test_decode_malformed_message(#[case] text: &str) {
        assert!(matches!(
            NexusMessage::<serde_json::Value>::decode(text),
            Err(MessageError::Malformed(_))
        ));
    }

    #[test]
    fn test_decode_snapshot_with_wrong_data_shape() {
        let text = snapshot(json!({"not": "prices"}))
            .encode()
            .unwrap();

        assert!(matches!(
            NexusMessage::<BebopPrices>::decode(&text),
            Err(MessageError::Malformed(_))
        ));
    }
}

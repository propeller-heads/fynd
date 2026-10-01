//! Measures how far each Hashflow leg's signed quote lands from the price levels it was solved on.
//!
//! The encoder requests the signed quote itself and keeps only the calldata, so the signed amounts
//! never reach the quote. Hashflow's executor data carries them, packed as
//! `… | base_token | quote_token | base_token_amount | quote_token_amount | …` (addresses 20 bytes,
//! amounts 32-byte big-endian, then `quote_expiry`), so they are read back from the encoded
//! transaction. A leg is found by its token pair followed by that shape; two Hashflow legs on one
//! pair take the pair's occurrences in route order.
//!
//! The gap compares rates rather than amounts, so a maker that signs for a different input than
//! the leg asked for still yields the price difference. A leg whose data cannot be found counts in
//! `rfq_signed_quote_unread_total`, which is what shows the packing has changed.

use std::collections::HashMap;

use metrics::{counter, histogram};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tracing::debug;

use crate::{OrderQuote, QuoteStatus, Swap};

const HASHFLOW: &str = "rfq:hashflow";

/// Log target for one line per measured leg, off unless enabled (`fynd::rfq_signed_quote=debug`).
const SIGNED_QUOTE_TARGET: &str = "fynd::rfq_signed_quote";

const AMOUNT_LEN: usize = 32;

/// Records the signed-quote gap of every Hashflow leg in the encoded quotes.
pub(crate) fn record_signed_quote_gaps(quotes: &[OrderQuote]) {
    for quote in quotes {
        if quote.status() == QuoteStatus::Success {
            record_quote(quote);
        }
    }
}

fn record_quote(quote: &OrderQuote) {
    let (Some(route), Some(transaction)) = (quote.route(), quote.transaction()) else { return };
    let calldata = transaction.data();
    // Where the next search for each pair starts, so a second leg on a pair reads the next
    // occurrence rather than the first leg's again.
    let mut next_search: HashMap<Vec<u8>, usize> = HashMap::new();
    for swap in route.swaps() {
        if swap.protocol() != HASHFLOW {
            continue;
        }
        let pair = [swap.token_in().as_ref(), swap.token_out().as_ref()].concat();
        let start = next_search
            .get(&pair)
            .copied()
            .unwrap_or(0);
        let Some((offset, signed)) = read_signed_amounts(calldata, &pair, start) else {
            counter!("rfq_signed_quote_unread_total", "protocol" => HASHFLOW).increment(1);
            continue;
        };
        next_search.insert(pair, offset + 1);
        record_leg(quote, swap, &signed);
    }
}

/// The amounts a maker signed for one leg.
struct SignedAmounts {
    amount_in: BigUint,
    amount_out: BigUint,
}

/// Finds `pair` (base token then quote token) in `calldata` at or after `start`, followed by the
/// shape of Hashflow's quote, and reads the two signed amounts. Returns the pair's offset with
/// them.
///
/// Another leg's data can hold the same pair back to back too: a Uniswap V3 swap packs
/// `token_in | token_out | fee | …`. So a match counts only when the three words after it read as
/// Hashflow's two amounts and its `quote_expiry`; otherwise the search moves on.
fn read_signed_amounts(
    calldata: &[u8],
    pair: &[u8],
    start: usize,
) -> Option<(usize, SignedAmounts)> {
    let mut from = start;
    loop {
        let offset = calldata
            .get(from..)?
            .windows(pair.len())
            .position(|window| window == pair)? +
            from;
        let words = calldata.get(offset + pair.len()..offset + pair.len() + 3 * AMOUNT_LEN)?;
        let (amount_in, rest) = words.split_at(AMOUNT_LEN);
        let (amount_out, expiry) = rest.split_at(AMOUNT_LEN);
        if is_amount(amount_in) && is_amount(amount_out) && is_unix_time(expiry) {
            return Some((
                offset,
                SignedAmounts {
                    amount_in: BigUint::from_bytes_be(amount_in),
                    amount_out: BigUint::from_bytes_be(amount_out),
                },
            ));
        }
        from = offset + 1;
    }
}

/// A non-zero token amount below 2^128: any real amount fits, while the address and fee bytes of
/// another protocol's data fill the high half of the word.
fn is_amount(word: &[u8]) -> bool {
    word[..AMOUNT_LEN / 2]
        .iter()
        .all(|byte| *byte == 0) &&
        word.iter().any(|byte| *byte != 0)
}

/// A unix time in seconds between 2001 and 2286, as Hashflow's `quote_expiry` is.
fn is_unix_time(word: &[u8]) -> bool {
    let value = BigUint::from_bytes_be(word);
    value >= BigUint::from(1_000_000_000u64) && value < BigUint::from(10_000_000_000u64)
}

fn record_leg(quote: &OrderQuote, swap: &Swap, signed: &SignedAmounts) {
    let Some(deviation_bps) = rate_deviation_bps(swap, signed) else { return };
    histogram!("rfq_signed_quote_deviation_bps", "protocol" => HASHFLOW).record(deviation_bps);
    debug!(
        target: SIGNED_QUOTE_TARGET,
        order_id = quote.order_id(),
        protocol = swap.protocol(),
        component_id = swap.component_id(),
        token_in = %swap.token_in(),
        token_out = %swap.token_out(),
        level_amount_in = %swap.amount_in(),
        level_amount_out = %swap.amount_out(),
        signed_amount_in = %signed.amount_in,
        signed_amount_out = %signed.amount_out,
        deviation_bps,
        "signed RFQ quote against its price levels"
    );
}

/// How far the signed rate lands from the price-level rate, in basis points. Negative means the
/// maker signed for less than its levels advertised.
fn rate_deviation_bps(swap: &Swap, signed: &SignedAmounts) -> Option<f64> {
    let level_rate = swap.amount_out().to_f64()? / swap.amount_in().to_f64()?;
    let signed_rate = signed.amount_out.to_f64()? / signed.amount_in.to_f64()?;
    let deviation = (signed_rate / level_rate - 1.0) * 10_000.0;
    deviation
        .is_finite()
        .then_some(deviation)
}

#[cfg(test)]
mod tests {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use rustc_hash::FxHashMap;
    use tycho_simulation::tycho_common::Bytes;

    use super::*;
    use crate::{
        algorithm::test_utils::{component, token, MockProtocolSim},
        tests::metrics::recorded_metrics,
        types::{BlockInfo, Route, Transaction},
    };

    fn amount(value: u64) -> Vec<u8> {
        let mut word = vec![0u8; AMOUNT_LEN];
        word[AMOUNT_LEN - 8..].copy_from_slice(&value.to_be_bytes());
        word
    }

    /// Hashflow's packed quote for one leg, padded on both sides as in a router call.
    fn hashflow_block(
        token_in: &Bytes,
        token_out: &Bytes,
        signed_in: u64,
        signed_out: u64,
    ) -> Vec<u8> {
        [
            vec![0x11; 80],
            token_in.to_vec(),
            token_out.to_vec(),
            amount(signed_in),
            amount(signed_out),
            amount(1_755_610_328),
            vec![0x22; 64],
        ]
        .concat()
    }

    fn hashflow_swap(amount_in: u64, level_out: u64) -> Swap {
        let (a, b) = (token(0x0A, "A"), token(0x0B, "B"));
        Swap::new(
            "hashflow".to_string(),
            HASHFLOW.to_string(),
            a.address.clone(),
            b.address.clone(),
            BigUint::from(amount_in),
            BigUint::from(level_out),
            BigUint::ZERO,
            component("hashflow", &[a, b]),
            Box::new(MockProtocolSim::new(1.0)),
        )
    }

    fn encoded_quote(swaps: Vec<Swap>, calldata: Vec<u8>, status: QuoteStatus) -> OrderQuote {
        let mut quote = OrderQuote::new(
            "order".to_string(),
            status,
            BigUint::from(1_000u64),
            BigUint::from(1_000u64),
            BigUint::ZERO,
            BigUint::from(1_000u64),
            BlockInfo::new(1, "0x1".to_string(), 1),
            "algo".to_string(),
            Bytes::from(vec![0xAA; 20]),
            Bytes::from(vec![0xBB; 20]),
            "1".to_string(),
        )
        .with_route(Route::new(swaps, FxHashMap::default()).expect("non-empty route"));
        quote.set_transaction(Transaction::new(Bytes::from(vec![0; 20]), BigUint::ZERO, calldata));
        quote
    }

    fn record(quote: &OrderQuote) -> Vec<(String, Vec<String>, DebugValue)> {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            record_signed_quote_gaps(std::slice::from_ref(quote))
        });
        recorded_metrics(&snapshotter)
    }

    fn deviations(recorded: &[(String, Vec<String>, DebugValue)]) -> Vec<f64> {
        recorded
            .iter()
            .filter(|(name, ..)| name == "rfq_signed_quote_deviation_bps")
            .flat_map(|(.., value)| match value {
                DebugValue::Histogram(values) => values
                    .iter()
                    .map(|v| v.into_inner())
                    .collect(),
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn test_rate_deviation_bps() {
        let swap = hashflow_swap(1_000, 2_000);
        let signed = |amount_in: u64, amount_out: u64| SignedAmounts {
            amount_in: BigUint::from(amount_in),
            amount_out: BigUint::from(amount_out),
        };

        assert_eq!(rate_deviation_bps(&swap, &signed(1_000, 2_000)), Some(0.0));
        let below = rate_deviation_bps(&swap, &signed(1_000, 1_980)).unwrap();
        assert!((below - -100.0).abs() < 1e-9, "{below}");
        let partial = rate_deviation_bps(&swap, &signed(500, 990)).unwrap();
        assert!((partial - -100.0).abs() < 1e-9, "a smaller fill at the same rate: {partial}");
        assert_eq!(rate_deviation_bps(&swap, &signed(0, 0)), None, "no rate to compare");
    }

    #[test]
    fn test_record_signed_quote_gaps_reads_leg() {
        let swap = hashflow_swap(1_000, 2_000);
        let calldata = hashflow_block(swap.token_in(), swap.token_out(), 1_000, 1_980);

        let recorded = record(&encoded_quote(vec![swap], calldata, QuoteStatus::Success));

        let gaps = deviations(&recorded);
        assert_eq!(gaps.len(), 1);
        assert!((gaps[0] - -100.0).abs() < 1e-9, "{gaps:?}");
    }

    /// Two legs on one pair, e.g. a split across two makers, each read their own block.
    #[test]
    fn test_record_signed_quote_gaps_same_pair_twice() {
        let (first, second) = (hashflow_swap(1_000, 2_000), hashflow_swap(1_000, 2_000));
        let calldata = [
            hashflow_block(first.token_in(), first.token_out(), 1_000, 1_980),
            hashflow_block(second.token_in(), second.token_out(), 1_000, 2_000),
        ]
        .concat();

        let recorded = record(&encoded_quote(vec![first, second], calldata, QuoteStatus::Success));

        let gaps = deviations(&recorded);
        assert_eq!(gaps.len(), 2, "{gaps:?}");
        assert!((gaps[0] - -100.0).abs() < 1e-9 && gaps[1].abs() < 1e-9, "{gaps:?}");
    }

    /// A leg whose pair is missing, or whose amounts are cut off, is counted as unread.
    #[test]
    fn test_record_signed_quote_gaps_unread() {
        let swap = hashflow_swap(1_000, 2_000);
        let mut truncated = hashflow_block(swap.token_in(), swap.token_out(), 1_000, 1_980);
        truncated.truncate(80 + 40 + 2 * AMOUNT_LEN);

        for calldata in [vec![0x33; 256], truncated] {
            let recorded = record(&encoded_quote(
                vec![hashflow_swap(1_000, 2_000)],
                calldata,
                QuoteStatus::Success,
            ));

            assert!(deviations(&recorded).is_empty());
            assert!(
                recorded
                    .iter()
                    .any(|(name, ..)| name == "rfq_signed_quote_unread_total"),
                "{recorded:?}"
            );
        }
    }

    /// An AMM leg of the same pair packs `token_in | token_out | fee | receiver …` ahead of the
    /// Hashflow block. Its bytes are not read as amounts.
    #[test]
    fn test_record_signed_quote_gaps_skips_amm_leg_on_same_pair() {
        let swap = hashflow_swap(1_000, 2_000);
        let uniswap_v3_leg = [
            swap.token_in().to_vec(),
            swap.token_out().to_vec(),
            vec![0x00, 0x01, 0xf4],
            vec![0x44; 40],
        ]
        .concat();
        let calldata =
            [uniswap_v3_leg, hashflow_block(swap.token_in(), swap.token_out(), 1_000, 1_980)]
                .concat();

        let recorded = record(&encoded_quote(vec![swap], calldata, QuoteStatus::Success));

        let gaps = deviations(&recorded);
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert!((gaps[0] - -100.0).abs() < 1e-9, "{gaps:?}");
    }

    #[test]
    fn test_record_signed_quote_gaps_skips_unencoded_quote() {
        let swap = hashflow_swap(1_000, 2_000);
        let calldata = hashflow_block(swap.token_in(), swap.token_out(), 1_000, 1_980);

        let recorded = record(&encoded_quote(vec![swap], calldata, QuoteStatus::EncodingFailed));

        assert!(recorded.is_empty(), "{recorded:?}");
    }
}

//! JSON-lines output for the live monitor.
//!
//! Projects each re-solved trade to one JSON record carrying both block states (verdict, bps, USD
//! deltas, and a slim route/calldata or the unsolvable reason), and projects a Fynd [`OrderQuote`]
//! to a slim route + calldata that omits each hop's bulky, sometimes-unserializable
//! `protocol_state`.

use fynd_core::types::{OrderQuote, Swap, Transaction};
use tracing::warn;

use crate::{
    resolve::{Outcome, RangeComparison, StateResult},
    usd,
};

/// Append one JSON line per re-solved trade to `writer` — every comparison, not just wins. Each
/// record carries both block states with their verdict (win/loss/unsolvable), so downstream can
/// filter to wins for the improvement view or to unsolvables for the coverage worklist (where Fynd
/// needs to improve). Losses keep their route (what path Fynd took and lost on); unsolvables keep
/// the reason.
pub(super) fn write_comparisons<W: std::io::Write>(
    writer: &mut W,
    ranges: &[RangeComparison],
    prices_top: &usd::PriceMap,
    prices_back: &usd::PriceMap,
) {
    for range in ranges {
        let Ok(line) = serde_json::to_string(&comparison_record(range, prices_top, prices_back))
        else {
            continue;
        };
        if let Err(e) = writeln!(writer, "{line}") {
            warn!(error = %e, "failed to write comparison record");
            return;
        }
    }
    if let Err(e) = writer.flush() {
        warn!(error = %e, "failed to flush comparisons writer");
    }
}

/// Build the JSON record for one re-solved trade: block, settled tx, decoded amounts, and a `top`
/// and `back` state (each with its verdict, bps, USD delta, and slim route/calldata or unsolvable
/// reason). Top is valued at N-1 prices, back at N prices, matching the state each was solved at.
fn comparison_record(
    range: &RangeComparison,
    prices_top: &usd::PriceMap,
    prices_back: &usd::PriceMap,
) -> serde_json::Value {
    serde_json::json!({
        "block": range.block_number,
        "settled_tx": range.tx_hash,
        "client": range.client,
        "aggregator": range.aggregator,
        "token_in": format!("{:#x}", range.token_in),
        "token_out": format!("{:#x}", range.token_out),
        "amount_in": range.amount_in.to_string(),
        "settled_amount_out": range.settled_amount_out.to_string(),
        "settled_amount_out_net_gas": range.settled_amount_out_net_gas.to_string(),
        "settled_gas_cost": range.settled_gas.map(|gas| gas.to_string()),
        "quoted_amount_out": range.quote.as_ref().map(|q| q.amount_out.to_string()),
        "quote_source": range.quote.as_ref().and_then(|q| q.source.clone()),
        "quote_timestamp": range.quote.as_ref().and_then(|q| q.timestamp),
        "top": state_record(&range.top, range, prices_top),
        "back": state_record(&range.back, range, prices_back),
    })
}

/// JSON for one block-state of an improvement: verdict, bps, Fynd amounts, the USD improvement
/// (net-of-gas Fynd output minus the gas-adjusted settled output, valued at `prices`), and the
/// slim quote. `settled_value_usd` stays gross — it is the trade's notional, not a comparison.
fn state_record(
    state: &StateResult,
    range: &RangeComparison,
    prices: &usd::PriceMap,
) -> serde_json::Value {
    let token_out = range.token_out;
    let solved = match &state.outcome {
        Outcome::Solved(solved) => Some(solved),
        Outcome::Partial(_) | Outcome::Unsolvable(_) => None,
    };
    // The reason Fynd could not serve the trade — the coverage-gap signal (missing token,
    // insufficient liquidity, timeout, partial-fill coverage miss).
    let unsolvable_reason = match &state.outcome {
        Outcome::Unsolvable(reason) | Outcome::Partial(reason) => Some(reason.as_str()),
        Outcome::Solved(_) => None,
    };
    let improvement_usd = solved.and_then(|s| {
        usd::savings_usd(token_out, s.amount_out_net_gas, range.settled_amount_out_net_gas, prices)
    });
    let fynd_value_usd = solved.and_then(|s| usd::value_usd(token_out, s.amount_out, prices));
    serde_json::json!({
        "verdict": state.verdict,
        "net_bps": state.deltas.net_bps,
        "raw_bps": state.deltas.raw_bps,
        "fynd_amount_out": solved.map(|s| s.amount_out.to_string()),
        "fynd_amount_out_net_gas": solved.map(|s| s.amount_out_net_gas.to_string()),
        "gas_estimate": solved.map(|s| s.gas_estimate.to_string()),
        "improvement_usd": improvement_usd,
        "fynd_value_usd": fynd_value_usd,
        "settled_value_usd": usd::value_usd(token_out, range.settled_amount_out, prices),
        "unsolvable_reason": unsolvable_reason,
        "quote": solved
            .and_then(|s| s.quote_json.as_deref())
            .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok()),
    })
}

/// Project an `OrderQuote` down to what an investigation needs: order id, status, the encoded
/// transaction (calldata), and a per-hop route (protocol, pool, tokens, amounts, gas). Built from
/// the quote object's accessors so it never touches each hop's `protocol_state` — which is both
/// the bulk of the size and unserializable for vm pools (Curve etc.).
pub(super) fn slim_quote(quote: &OrderQuote) -> serde_json::Value {
    let route: Vec<serde_json::Value> = quote
        .route()
        .map(|route| {
            route
                .swaps()
                .iter()
                .map(slim_swap)
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "order_id": quote.order_id(),
        "status": serde_json::to_value(quote.status()).ok(),
        "transaction": quote.transaction().map(slim_transaction),
        "route": route,
    })
}

/// One route hop: protocol, pool (the component id is the pool address), tokens, amounts, gas.
fn slim_swap(swap: &Swap) -> serde_json::Value {
    serde_json::json!({
        "protocol": swap.protocol(),
        "pool": swap.component_id(),
        "token_in": serde_json::to_value(swap.token_in()).ok(),
        "token_out": serde_json::to_value(swap.token_out()).ok(),
        "amount_in": swap.amount_in().to_string(),
        "amount_out": swap.amount_out().to_string(),
        "gas_estimate": swap.gas_estimate().to_string(),
        "split": swap.split(),
    })
}

/// The encoded on-chain transaction: target, native value, and hex calldata.
fn slim_transaction(transaction: &Transaction) -> serde_json::Value {
    serde_json::json!({
        "to": serde_json::to_value(transaction.to()).ok(),
        "value": transaction.value().to_string(),
        "data": format!("0x{}", alloy::hex::encode(transaction.data())),
    })
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, U256};
    use num_bigint::BigUint;

    use super::*;
    use crate::{
        decoder::{DecodedTrade, SolverQuote},
        resolve::{build_range, SolvedAmount},
    };

    #[test]
    fn comparison_record_carries_solver_quote() {
        let trade = DecodedTrade {
            tx_hash: Default::default(),
            block_number: 25_480_207,
            client: "relay".into(),
            aggregator: "kyberswap".into(),
            sender: Address::ZERO,
            token_in: Address::ZERO,
            token_out: Address::repeat_byte(0x22),
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(69_996_280_564u64),
            client_fee: None,
            client_fee_out: None,
            settled_gas: None,
            quote: Some(SolverQuote {
                amount_out: U256::from(70_400_409_935u64),
                source: Some("relay".to_string()),
                timestamp: Some(1_783_421_726),
            }),
        };
        let range = build_range(
            &trade,
            &usd::PriceMap::new(),
            Outcome::Unsolvable("x".into()),
            Outcome::Unsolvable("x".into()),
        );
        let rec = comparison_record(&range, &usd::PriceMap::new(), &usd::PriceMap::new());
        assert_eq!(
            rec.pointer("/quoted_amount_out")
                .unwrap(),
            "70400409935"
        );
        assert_eq!(rec.pointer("/quote_source").unwrap(), "relay");
        assert_eq!(
            rec.pointer("/quote_timestamp")
                .unwrap()
                .as_u64(),
            Some(1_783_421_726)
        );
    }

    #[test]
    fn slim_transaction_emits_hex_calldata_and_address() {
        use tycho_simulation::tycho_common::Bytes;
        let tx = Transaction::new(
            Bytes::from(vec![0x11u8; 20]),
            BigUint::from(5u8),
            vec![0xde, 0xad, 0xbe, 0xef],
        );
        let slim = slim_transaction(&tx);
        assert_eq!(slim.get("data").unwrap(), "0xdeadbeef");
        assert_eq!(slim.get("value").unwrap(), "5");
        assert!(slim
            .get("to")
            .unwrap()
            .as_str()
            .unwrap()
            .starts_with("0x"));
    }

    #[test]
    fn improvement_record_carries_top_and_back_with_usd_and_slim_route() {
        let usdc: Address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            .parse()
            .unwrap();
        let weth: Address = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
            .parse()
            .unwrap();
        // ETH=$2000: USDC (6dp) = 2e-9 native units/wei, WETH (18dp) = 1.0.
        let prices = usd::PriceMap::from([(usdc, 2e-9), (weth, 1.0)]);

        let trade = DecodedTrade {
            tx_hash: Default::default(),
            block_number: 25_000_000,
            client: "relay".into(),
            aggregator: "1inch".into(),
            sender: Address::ZERO,
            token_in: weth,
            token_out: usdc,
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(1_000_000_000u64), // settled 1000 USDC
            client_fee: None,
            client_fee_out: None,
            settled_gas: None,
            quote: None,
        };
        // quote_json is already the slim projection (what order_quote_to_outcome stores).
        let quote = Some(
            r#"{"order_id":"o","status":"success","transaction":{"to":"0xrouter","value":"0",
                "data":"0x01"},"route":[{"protocol":"uniswap_v3","pool":"0xpool",
                "token_in":"0xaaa","token_out":"0xbbb","amount_in":"1","amount_out":"2",
                "gas_estimate":"0","split":1.0}]}"#
                .to_string(),
        );
        // Top: net 1005 USDC → +$5. Back: net 1001 USDC → +$1. Both win.
        let top = Outcome::Solved(SolvedAmount {
            amount_out: U256::from(1_010_000_000u64),
            amount_out_net_gas: U256::from(1_005_000_000u64),
            gas_estimate: U256::from(21_000u64),
            quote_json: quote.clone(),
        });
        let back = Outcome::Solved(SolvedAmount {
            amount_out: U256::from(1_002_000_000u64),
            amount_out_net_gas: U256::from(1_001_000_000u64),
            gas_estimate: U256::from(21_000u64),
            quote_json: quote,
        });
        let range = build_range(&trade, &prices, top, back);

        let rec = comparison_record(&range, &prices, &prices);
        let top_usd = rec
            .pointer("/top/improvement_usd")
            .unwrap()
            .as_f64()
            .unwrap();
        let back_usd = rec
            .pointer("/back/improvement_usd")
            .unwrap()
            .as_f64()
            .unwrap();
        assert!((top_usd - 5.0).abs() < 1e-3, "top_usd={top_usd}");
        assert!((back_usd - 1.0).abs() < 1e-3, "back_usd={back_usd}");
        assert!(
            rec.pointer("/back/net_bps")
                .unwrap()
                .as_f64()
                .unwrap() >
                0.0
        );
        // Both states embed the slim quote: calldata and route/pool are present.
        assert_eq!(
            rec.pointer("/top/quote/transaction/data")
                .unwrap(),
            "0x01"
        );
        assert_eq!(
            rec.pointer("/top/quote/route/0/pool")
                .unwrap(),
            "0xpool"
        );
        assert_eq!(
            rec.pointer("/back/quote/route/0/protocol")
                .unwrap(),
            "uniswap_v3"
        );
    }

    #[test]
    fn comparison_record_captures_unsolvable_reason_and_null_quote() {
        let trade = DecodedTrade {
            tx_hash: Default::default(),
            block_number: 25_000_000,
            client: "relay".into(),
            aggregator: "1inch".into(),
            sender: Address::ZERO,
            token_in: Address::repeat_byte(0x11),
            token_out: Address::repeat_byte(0x22),
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(1_000u64),
            client_fee: None,
            client_fee_out: None,
            settled_gas: None,
            quote: None,
        };
        // A coverage gap: Fynd could not solve at either state.
        let range = build_range(
            &trade,
            &usd::PriceMap::new(),
            Outcome::Unsolvable("missing token in Tycho".into()),
            Outcome::Unsolvable("missing token in Tycho".into()),
        );
        let rec = comparison_record(&range, &usd::PriceMap::new(), &usd::PriceMap::new());
        assert_eq!(rec.pointer("/top/verdict").unwrap(), "unsolvable");
        assert_eq!(
            rec.pointer("/top/unsolvable_reason")
                .unwrap(),
            "missing token in Tycho"
        );
        assert!(rec
            .pointer("/top/quote")
            .unwrap()
            .is_null());
    }
}

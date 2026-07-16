//! JSON-lines output for the live monitor.
//!
//! Projects each re-solved trade to one JSON record carrying both block states (verdict, bps, USD
//! deltas, and a slim route/calldata or the unsolvable reason), and projects a Fynd [`OrderQuote`]
//! to a slim route + calldata that omits each hop's bulky, sometimes-unserializable
//! `protocol_state`.

use std::{
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use fynd_core::types::{OrderQuote, Swap, Transaction};
use tracing::{info, warn};

use crate::{
    resolve::{Outcome, RangeComparison, StateResult},
    usd,
};

/// Append-only comparisons writer that rotates to a new file at each UTC day boundary —
/// `comparisons-YYYY-MM-DD.jsonl` inside its directory — so an external sync job (e.g. an S3
/// upload `CronJob`) ships closed daily files instead of re-shipping one ever-growing one.
pub(crate) struct RotatingWriter {
    dir: PathBuf,
    date: String,
    writer: BufWriter<std::fs::File>,
}

impl RotatingWriter {
    /// Open today's file inside `dir` for appending, creating the directory if needed.
    pub(crate) fn open(dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create comparisons directory {}", dir.display()))?;
        let date = utc_date();
        let writer = open_dated(&dir, &date)?;
        Ok(Self { dir, date, writer })
    }

    /// The path of the file currently being written.
    pub(crate) fn current_path(&self) -> PathBuf {
        dated_path(&self.dir, &self.date)
    }

    /// The current file's writer, rotated first when the UTC day has changed.
    pub(crate) fn writer(&mut self) -> &mut BufWriter<std::fs::File> {
        self.rotate_to(utc_date());
        &mut self.writer
    }

    /// Switch to `date`'s file when it differs from the current one. A failed rotation keeps the
    /// previous day's file: for a long unattended run, appending to yesterday's file beats dying
    /// on a transient filesystem error.
    fn rotate_to(&mut self, date: String) {
        if date == self.date {
            return;
        }
        if let Err(e) = self.writer.flush() {
            warn!(error = %e, "failed to flush comparisons file before rotation");
        }
        match open_dated(&self.dir, &date) {
            Ok(writer) => {
                info!(path = %dated_path(&self.dir, &date).display(), "rotated comparisons file");
                self.writer = writer;
                self.date = date;
            }
            Err(e) => {
                warn!(error = %e, "failed to rotate comparisons file; keeping the previous day's");
            }
        }
    }
}

fn dated_path(dir: &Path, date: &str) -> PathBuf {
    dir.join(format!("comparisons-{date}.jsonl"))
}

fn open_dated(dir: &Path, date: &str) -> anyhow::Result<BufWriter<std::fs::File>> {
    let path = dated_path(dir, date);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open comparisons jsonl {}", path.display()))?;
    Ok(BufWriter::new(file))
}

/// Today's UTC date as `YYYY-MM-DD` from the system clock.
fn utc_date() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    date_from_unix(secs)
}

/// Civil date for a unix timestamp (UTC), via the days-to-civil-calendar algorithm — exact for
/// the whole unix era, so no calendar dependency is needed for a filename.
fn date_from_unix(secs: u64) -> String {
    let z = (secs / 86_400).cast_signed() + 719_468_i64;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Append one JSON line per re-solved trade to `writer` — every comparison, not just wins. Each
/// record carries both block states with their verdict (win/loss/unsolvable), so downstream can
/// filter to wins for the improvement view or to unsolvables for the coverage worklist (where Fynd
/// needs to improve). Losses keep their route (what path Fynd took and lost on); unsolvables keep
/// the reason.
pub(super) fn write_comparisons<W: std::io::Write>(
    writer: &mut W,
    ranges: &[RangeComparison],
    prices_top: &usd::Prices,
    prices_back: &usd::Prices,
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
    prices_top: &usd::Prices,
    prices_back: &usd::Prices,
) -> serde_json::Value {
    serde_json::json!({
        "block": range.block_number,
        "tx_index": range.tx_index,
        "settled_tx": range.tx_hash,
        "venue": range.venue,
        "solver": range.solver,
        "solver_source": range.solver_source,
        "decode_strategy": range.decode_strategy,
        "token_in": format!("{:#x}", range.token_in),
        "token_out": format!("{:#x}", range.token_out),
        "amount_in": range.amount_in.to_string(),
        "settled_amount_out": range.settled_amount_out.to_string(),
        "settled_amount_out_net_gas": range.settled_amount_out_net_gas.to_string(),
        "settled_gas_cost": range.settled_gas.map(|gas| gas.to_string()),
        "quoted_amount_out": range.quote.as_ref().map(|q| q.amount_out.to_string()),
        "quote_source": range.quote.as_ref().and_then(|q| q.source.clone()),
        "quote_timestamp": range.quote.as_ref().and_then(|q| q.timestamp),
        "sandwich": range.sandwich,
        "top": state_record(&range.top, range, prices_top),
        "back": state_record(&range.back, range, prices_back),
    })
}

/// JSON for one block-state of an improvement: verdict, bps, Fynd amounts, the USD improvement
/// (gross Fynd output minus the gross settled output, valued at `prices` — the same basis as the
/// headline verdict), and the slim quote. `settled_value_usd` stays gross — it is the trade's
/// notional, not a comparison.
fn state_record(
    state: &StateResult,
    range: &RangeComparison,
    prices: &usd::Prices,
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
    let improvement_usd =
        solved.and_then(|s| prices.savings_usd(token_out, s.amount_out, range.settled_amount_out));
    let fynd_value_usd = solved.and_then(|s| prices.value_usd(token_out, s.amount_out));
    serde_json::json!({
        "verdict": state.verdict,
        "net_bps": state.deltas.net_bps,
        "raw_bps": state.deltas.raw_bps,
        "fynd_amount_out": solved.map(|s| s.amount_out.to_string()),
        "fynd_amount_out_net_gas": solved.map(|s| s.amount_out_net_gas.to_string()),
        "gas_estimate": solved.map(|s| s.gas_estimate.to_string()),
        "improvement_usd": improvement_usd,
        "fynd_value_usd": fynd_value_usd,
        "settled_value_usd": prices.value_usd(token_out, range.settled_amount_out),
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
    use alloy::primitives::{Address, TxHash, U256};
    use num_bigint::BigUint;

    use super::*;
    use crate::{
        decoder::{AttributionSource, DecodedTrade, Registry, SandwichEvidence, SolverQuote},
        resolve::{build_range, SolvedAmount},
    };

    fn empty_prices() -> usd::Prices {
        usd::Prices::new(&Registry::ethereum())
    }

    #[test]
    fn date_from_unix_matches_utc_calendar() {
        assert_eq!(date_from_unix(0), "1970-01-01");
        assert_eq!(date_from_unix(86_399), "1970-01-01"); // last second of the first day
        assert_eq!(date_from_unix(86_400), "1970-01-02"); // day boundary
        assert_eq!(date_from_unix(1_783_477_604), "2026-07-08");
        assert_eq!(date_from_unix(1_709_164_800), "2024-02-29"); // leap day
        assert_eq!(date_from_unix(951_782_400), "2000-02-29"); // 400-year-rule leap day
    }

    #[test]
    fn rotating_writer_switches_files_at_a_new_date() {
        let dir = std::env::temp_dir().join(format!("hindsight-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let mut rotating = RotatingWriter::open(&dir).unwrap();
        let today = rotating.current_path();
        writeln!(rotating.writer(), "{{\"day\":1}}").unwrap();

        // Same date: no rotation, appends to the same file.
        rotating.rotate_to(rotating.date.clone());
        writeln!(rotating.writer(), "{{\"day\":1,\"line\":2}}").unwrap();

        // New date: subsequent writes land in the new file. Write through the field, not
        // `writer()` — the accessor would immediately rotate back to the real system date.
        rotating.rotate_to("2099-01-01".to_string());
        writeln!(rotating.writer, "{{\"day\":2}}").unwrap();
        drop(rotating);

        let first = std::fs::read_to_string(&today).unwrap();
        assert_eq!(first.lines().count(), 2);
        let second = std::fs::read_to_string(dir.join("comparisons-2099-01-01.jsonl")).unwrap();
        assert_eq!(second.trim(), "{\"day\":2}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn comparison_record_carries_solver_quote() {
        let trade = DecodedTrade {
            tx_hash: TxHash::default(),
            block_number: 25_480_207,
            tx_index: 3,
            venue: "relay".into(),
            solver: "kyberswap".into(),
            solver_source: AttributionSource::TraceMatch,
            decode_strategy: "netting",
            sender: Address::ZERO,
            token_in: Address::ZERO,
            token_out: Address::repeat_byte(0x22),
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(69_996_280_564u64),
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            quote: Some(SolverQuote {
                amount_out: U256::from(70_400_409_935u64),
                source: Some("relay".to_string()),
                timestamp: Some(1_783_421_726),
            }),
            sandwich: None,
        };
        let range = build_range(
            &trade,
            &empty_prices(),
            Outcome::Unsolvable("x".into()),
            Outcome::Unsolvable("x".into()),
        );
        let rec = comparison_record(&range, &empty_prices(), &empty_prices());
        assert_eq!(rec.pointer("/tx_index").unwrap(), 3);
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
        let mut prices = empty_prices();
        prices.insert(usdc, 2e-9);
        prices.insert(weth, 1.0);

        let trade = DecodedTrade {
            tx_hash: TxHash::default(),
            block_number: 25_000_000,
            tx_index: 0,
            venue: "relay".into(),
            solver: "1inch".into(),
            solver_source: AttributionSource::TraceMatch,
            decode_strategy: "netting",
            sender: Address::ZERO,
            token_in: weth,
            token_out: usdc,
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(1_000_000_000u64), // settled 1000 USDC
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            quote: None,
            sandwich: None,
        };
        // quote_json is already the slim projection (what order_quote_to_outcome stores).
        let quote = Some(
            r#"{"order_id":"o","status":"success","transaction":{"to":"0xrouter","value":"0",
                "data":"0x01"},"route":[{"protocol":"uniswap_v3","pool":"0xpool",
                "token_in":"0xaaa","token_out":"0xbbb","amount_in":"1","amount_out":"2",
                "gas_estimate":"0","split":1.0}]}"#
                .to_string(),
        );
        // Top: gross 1010 USDC → +$10. Back: gross 1002 USDC → +$2. Both win.
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
        assert!((top_usd - 10.0).abs() < 1e-3, "top_usd={top_usd}");
        assert!((back_usd - 2.0).abs() < 1e-3, "back_usd={back_usd}");
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
            tx_hash: TxHash::default(),
            block_number: 25_000_000,
            tx_index: 0,
            venue: "relay".into(),
            solver: "1inch".into(),
            solver_source: AttributionSource::TraceMatch,
            decode_strategy: "netting",
            sender: Address::ZERO,
            token_in: Address::repeat_byte(0x11),
            token_out: Address::repeat_byte(0x22),
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(1_000u64),
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            quote: None,
            sandwich: None,
        };
        // A coverage gap: Fynd could not solve at either state.
        let range = build_range(
            &trade,
            &empty_prices(),
            Outcome::Unsolvable("missing token in Tycho".into()),
            Outcome::Unsolvable("missing token in Tycho".into()),
        );
        let rec = comparison_record(&range, &empty_prices(), &empty_prices());
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

    #[test]
    fn comparison_record_carries_sandwich_evidence_and_becomes_sandwiched_verdict() {
        let mut trade = DecodedTrade {
            tx_hash: TxHash::default(),
            block_number: 25_000_000,
            tx_index: 42,
            venue: "relay".into(),
            solver: "1inch".into(),
            solver_source: AttributionSource::TraceMatch,
            decode_strategy: "netting",
            sender: Address::ZERO,
            token_in: Address::repeat_byte(0x11),
            token_out: Address::repeat_byte(0x22),
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(1_000u64),
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            quote: None,
            sandwich: None,
        };
        trade.sandwich = Some(SandwichEvidence {
            front_tx: TxHash::repeat_byte(0x11),
            back_tx: TxHash::repeat_byte(0x22),
            attacker: Address::repeat_byte(0x33),
            pools: vec![Address::repeat_byte(0x44)],
        });

        // Solved states: only those are reclassified as sandwiched (unsolved keep their verdict).
        let solved = |amount: u64| {
            Outcome::Solved(SolvedAmount {
                amount_out: U256::from(amount),
                amount_out_net_gas: U256::from(amount),
                gas_estimate: U256::from(21_000u64),
                quote_json: None,
            })
        };
        let range = build_range(&trade, &empty_prices(), solved(1_100), solved(1_050));
        let rec = comparison_record(&range, &empty_prices(), &empty_prices());

        assert_eq!(rec.pointer("/tx_index").unwrap(), 42);
        assert_eq!(rec.pointer("/top/verdict").unwrap(), "sandwiched");
        assert_eq!(rec.pointer("/back/verdict").unwrap(), "sandwiched");
        assert_eq!(
            rec.pointer("/sandwich/attacker")
                .unwrap(),
            &format!("{:#x}", Address::repeat_byte(0x33))
        );
        assert_eq!(
            rec.pointer("/sandwich/pools/0")
                .unwrap(),
            &format!("{:#x}", Address::repeat_byte(0x44))
        );
    }
}

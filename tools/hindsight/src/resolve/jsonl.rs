//! JSON-lines output for the live monitor.
//!
//! Projects each re-solved trade to one JSON record carrying both block states (verdict, bps, USD
//! deltas, and a slim route/calldata or the unsolvable reason), and projects a Fynd `OrderQuote`
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
    decoder::{RevertCause, TradeStatus},
    resolve::{render_route, Outcome, RangeComparison, StateResult},
    telemetry::revert_cause_label,
    usd::Prices,
};

/// The rotating file's name prefix — every trade, settled or reverted, writes here (see
/// `RangeComparison`'s `status` field); there is only one stream.
const PREFIX: &str = "comparisons";

/// Append-only writer that rotates to a new file at each UTC day boundary — `comparisons-YYYY-MM
/// -DD.jsonl` inside its directory — so an external sync job (e.g. an S3 upload `CronJob`) ships
/// closed daily files instead of re-shipping one ever-growing one. `monitor` opens one of these in
/// its `--comparisons-dir`.
pub(crate) struct RotatingWriter {
    dir: PathBuf,
    date: String,
    writer: BufWriter<std::fs::File>,
}

impl RotatingWriter {
    /// Open today's `comparisons-*.jsonl` file inside `dir` for appending, creating the directory
    /// if needed.
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
            warn!(error = %e, "failed to flush jsonl file before rotation");
        }
        match open_dated(&self.dir, &date) {
            Ok(writer) => {
                info!(path = %dated_path(&self.dir, &date).display(), "rotated jsonl file");
                self.writer = writer;
                self.date = date;
            }
            Err(e) => {
                warn!(error = %e, "failed to rotate jsonl file; keeping the previous day's");
            }
        }
    }
}

fn dated_path(dir: &Path, date: &str) -> PathBuf {
    dir.join(format!("{PREFIX}-{date}.jsonl"))
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
pub(crate) fn write_comparisons<W: std::io::Write>(
    writer: &mut W,
    ranges: &[RangeComparison],
    prices_top: &Prices,
    prices_back: &Prices,
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

/// A record's flat status fields: `status` ("settled"/"reverted"), and — for a revert — a bounded
/// `cause` label (matching `telemetry::revert_cause_label`) plus its free-text `cause_detail`.
/// Kept flat (not the nested `{"kind":...}` shape `RevertCause` itself serializes to) so a jq
/// pass can filter on `status`/`cause` directly, matching the old `reverts-*.jsonl` ergonomics
/// now that both streams are one.
fn status_fields(status: &TradeStatus) -> (&'static str, Option<&'static str>, Option<&str>) {
    match status {
        TradeStatus::Settled => ("settled", None, None),
        TradeStatus::Reverted { cause } => {
            let detail = match cause {
                RevertCause::Other(detail) => Some(detail.as_str()),
                RevertCause::SlippageFloor | RevertCause::OutOfGas => None,
            };
            ("reverted", Some(revert_cause_label(cause)), detail)
        }
    }
}

/// Build the JSON record for one re-solved trade — settled or reverted, told apart by `status`:
/// block, tx, decoded amounts, a `top` and `back` state (each with its verdict, bps, fillable/
/// margin judgment, and slim route/calldata or unsolvable reason), and the top route's slippage
/// between the two states. `top`/`back`/`slippage` are `null` when the trade's terms were
/// unknown — there was nothing to solve. Top is valued at N-1 prices, back (and the slippage) at
/// N prices, matching the state each was produced at.
fn comparison_record(
    range: &RangeComparison,
    prices_top: &Prices,
    prices_back: &Prices,
) -> serde_json::Value {
    let (status, cause, cause_detail) = status_fields(&range.status);
    // Signed in both directions; the positive records are the "revenue if we charged positive
    // slippage" view, filtered downstream.
    let slippage = range.slippage.map(|slippage| {
        let usd = range.token_out.and_then(|token_out| {
            prices_back.savings_usd(
                token_out,
                slippage.reexecuted_amount_out,
                slippage.quoted_amount_out,
            )
        });
        serde_json::json!({ "bps": slippage.bps, "usd": usd })
    });
    serde_json::json!({
        "block": range.block_number,
        "tx_index": range.tx_index,
        "tx_hash": range.tx_hash,
        "status": status,
        "cause": cause,
        "cause_detail": cause_detail,
        "venue": range.venue,
        "solver": range.solver,
        "solver_source": range.solver_source,
        "decoder": range.decoder,
        "sender": format!("{:#x}", range.sender),
        "token_in": range.token_in.map(|token| format!("{token:#x}")),
        "token_out": range.token_out.map(|token| format!("{token:#x}")),
        "amount_in": range.amount_in.map(|amount| amount.to_string()),
        "settled_amount_out": range.settled_amount_out.map(|amount| amount.to_string()),
        "settled_amount_out_net_gas": range.settled_amount_out_net_gas.map(|amount| amount.to_string()),
        "settled_gas_cost": range.settled_gas.map(|gas| gas.to_string()),
        "min_amount_out": range.min_amount_out.map(|amount| amount.to_string()),
        "quoted_amount_out": range.declared_quote.map(|amount| amount.to_string()),
        "quote_timestamp": range.quote_timestamp,
        "sandwich": range.sandwich,
        "slippage": slippage,
        "top": range.top.as_ref().map(|top| state_record(top, range, prices_top)),
        "back": range.back.as_ref().map(|back| state_record(back, range, prices_back)),
    })
}

/// JSON for one block-state of a trade: verdict, bps, Fynd amounts, the USD improvement (gross
/// Fynd output minus the gross settled output, valued at `prices` — the same basis as the
/// headline verdict), the winning route's algorithm and rendered path, the slim quote, and the
/// fillable/margin judgment against `min_amount_out` (present whenever a floor is known, settled
/// or reverted). `settled_value_usd`/`improvement_usd` are `null` for a reverted trade — nothing
/// settled to value or improve on.
fn state_record(
    state: &StateResult,
    range: &RangeComparison,
    prices: &Prices,
) -> serde_json::Value {
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
    let token_and_settled = range
        .token_out
        .zip(range.settled_amount_out);
    let improvement_usd = solved.and_then(|s| {
        token_and_settled
            .and_then(|(token_out, settled)| prices.savings_usd(token_out, s.amount_out, settled))
    });
    let fynd_value_usd = range
        .token_out
        .zip(solved)
        .and_then(|(token_out, s)| prices.value_usd(token_out, s.amount_out));
    let settled_value_usd =
        token_and_settled.and_then(|(token_out, settled)| prices.value_usd(token_out, settled));
    serde_json::json!({
        "verdict": state.verdict,
        "net_bps": state.deltas.net_bps,
        "raw_bps": state.deltas.raw_bps,
        "fillable": state.fillable,
        "margin_bps": state.margin_bps,
        "fynd_amount_out": solved.map(|s| s.amount_out.to_string()),
        "fynd_amount_out_net_gas": solved.map(|s| s.amount_out_net_gas.to_string()),
        "gas_estimate": solved.map(|s| s.gas_estimate.to_string()),
        // Flat route attribution, so a jq pass can group by algorithm or read the path at a glance
        // without walking the nested per-hop route below.
        "algorithm": solved.map(|s| s.algorithm.as_str()),
        "route": solved.map(|s| s.solved_route.as_deref().map(render_route).unwrap_or_default()),
        "improvement_usd": improvement_usd,
        "fynd_value_usd": fynd_value_usd,
        "settled_value_usd": settled_value_usd,
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
        decoder::{AttributionSource, DecodedTrade, Registry, SandwichEvidence},
        resolve::{build_range, test_support, SolvedAmount},
    };

    fn empty_prices() -> Prices {
        Prices::new(&Registry::ethereum())
    }

    #[test]
    fn test_date_from_unix_at_day_boundaries() {
        assert_eq!(date_from_unix(0), "1970-01-01");
        assert_eq!(date_from_unix(86_399), "1970-01-01"); // last second of the first day
        assert_eq!(date_from_unix(86_400), "1970-01-02"); // day boundary
        assert_eq!(date_from_unix(1_783_477_604), "2026-07-08");
        assert_eq!(date_from_unix(1_709_164_800), "2024-02-29"); // leap day
        assert_eq!(date_from_unix(951_782_400), "2000-02-29"); // 400-year-rule leap day
    }

    #[test]
    fn test_rotating_writer_at_a_new_date() {
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
    fn test_comparison_record_with_declared_quote() {
        let trade = DecodedTrade {
            tx_hash: TxHash::default(),
            block_number: 25_480_207,
            tx_index: 3,
            status: TradeStatus::Settled,
            venue: "relay".into(),
            solver: "kyberswap".into(),
            solver_source: AttributionSource::TraceMatch,
            decoder: "sender-netting",
            sender: Address::repeat_byte(0x77),
            token_in: Some(Address::ZERO),
            token_out: Some(Address::repeat_byte(0x22)),
            amount_in: Some(U256::from(1_000u64)),
            amount_out: Some(U256::from(69_996_280_564u64)),
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            min_amount_out: Some(U256::from(69_996_280_564u64)),
            declared_quote: Some(U256::from(70_400_409_935u64)),
            quote_timestamp: Some(1_783_421_726),
            sandwich: None,
        };
        let range = build_range(
            &trade,
            &empty_prices(),
            Some((
                Outcome::Unsolvable("x".into()),
                Outcome::Unsolvable("x".into()),
                Outcome::Unsolvable("x".into()),
            )),
        );
        let rec = comparison_record(&range, &empty_prices(), &empty_prices());
        assert_eq!(rec.pointer("/tx_index").unwrap(), 3);
        assert_eq!(rec.pointer("/sender").unwrap(), &format!("{:#x}", Address::repeat_byte(0x77)));
        assert_eq!(rec.pointer("/status").unwrap(), "settled");
        assert_eq!(
            rec.pointer("/quoted_amount_out")
                .unwrap(),
            "70400409935"
        );
        assert_eq!(rec.pointer("/min_amount_out").unwrap(), "69996280564");
        assert!(rec.pointer("/quote_source").is_none());
        assert_eq!(
            rec.pointer("/quote_timestamp")
                .unwrap()
                .as_u64(),
            Some(1_783_421_726)
        );
    }

    #[test]
    fn test_slim_transaction_encoding() {
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

    /// Shared fixture for the top/back improvement and route-attribution tests: a win at both
    /// states, with the top route re-executing to the same output the fresh back solve finds.
    fn improvement_record_top_and_back() -> serde_json::Value {
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
            status: TradeStatus::Settled,
            venue: "relay".into(),
            solver: "1inch".into(),
            solver_source: AttributionSource::TraceMatch,
            decoder: "sender-netting",
            sender: Address::ZERO,
            token_in: Some(weth),
            token_out: Some(usdc),
            amount_in: Some(U256::from(1_000u64)),
            amount_out: Some(U256::from(1_000_000_000u64)), // settled 1000 USDC
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            min_amount_out: None,
            declared_quote: None,
            quote_timestamp: None,
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
        let solved_route = Box::new(test_support::route(&[
            ("uniswap_v3", "WETH", "DAI"),
            ("vm:curve", "DAI", "USDC"),
        ]));
        // Top: gross 1010 USDC → +$10. Back: gross 1002 USDC → +$2. Both win. The top route's
        // re-execution matches the fresh back solve, so the slippage numbers read off `back`.
        let top = Outcome::Solved(SolvedAmount {
            amount_out: U256::from(1_010_000_000u64),
            amount_out_net_gas: U256::from(1_005_000_000u64),
            gas_estimate: U256::from(21_000u64),
            algorithm: "bellman_ford".to_string(),
            quote_json: quote.clone(),
            solved_route: Some(solved_route.clone()),
        });
        let back = Outcome::Solved(SolvedAmount {
            amount_out: U256::from(1_002_000_000u64),
            amount_out_net_gas: U256::from(1_001_000_000u64),
            gas_estimate: U256::from(21_000u64),
            algorithm: "bellman_ford".to_string(),
            quote_json: quote,
            solved_route: Some(solved_route),
        });
        let range = build_range(&trade, &prices, Some((top, back.clone(), back)));

        comparison_record(&range, &prices, &prices)
    }

    #[test]
    fn test_improvement_record_top_and_back() {
        let rec = improvement_record_top_and_back();
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
        // Slippage: the top route re-executed at back produced 1002 vs 1010 quoted → −8 USDC,
        // ≈ −79.2 bps and −$8 (signed; positive records are the chargeable-surplus view).
        let slippage_bps = rec
            .pointer("/slippage/bps")
            .unwrap()
            .as_f64()
            .unwrap();
        assert!((slippage_bps + 79.2).abs() < 0.1, "slippage_bps={slippage_bps}");
        let slippage_usd = rec
            .pointer("/slippage/usd")
            .unwrap()
            .as_f64()
            .unwrap();
        assert!((slippage_usd + 8.0).abs() < 1e-3, "slippage_usd={slippage_usd}");
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
    fn test_improvement_record_attributes_the_winning_route() {
        let rec = improvement_record_top_and_back();
        // Route attribution is flat on the state, so grouping by algorithm or protocol does not
        // have to walk the nested per-hop route.
        assert_eq!(rec.pointer("/top/algorithm").unwrap(), "bellman_ford");
        assert_eq!(
            rec.pointer("/top/route").unwrap(),
            "WETH -[uniswap_v3]-> DAI -[vm:curve]-> USDC"
        );
    }

    #[test]
    fn test_comparison_record_unsolvable() {
        let trade = DecodedTrade {
            tx_hash: TxHash::default(),
            block_number: 25_000_000,
            tx_index: 0,
            status: TradeStatus::Settled,
            venue: "relay".into(),
            solver: "1inch".into(),
            solver_source: AttributionSource::TraceMatch,
            decoder: "sender-netting",
            sender: Address::ZERO,
            token_in: Some(Address::repeat_byte(0x11)),
            token_out: Some(Address::repeat_byte(0x22)),
            amount_in: Some(U256::from(1_000u64)),
            amount_out: Some(U256::from(1_000u64)),
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            min_amount_out: None,
            declared_quote: None,
            quote_timestamp: None,
            sandwich: None,
        };
        // A coverage gap: Fynd could not solve at either state.
        let range = build_range(
            &trade,
            &empty_prices(),
            Some((
                Outcome::Unsolvable("missing token in Tycho".into()),
                Outcome::Unsolvable("missing token in Tycho".into()),
                Outcome::Unsolvable("no top-of-block route to re-execute".into()),
            )),
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
        // No route means nothing to attribute: the fields are null, not an empty algorithm or an
        // empty path string that would read as a real but unrendered route.
        for field in ["algorithm", "route"] {
            assert!(
                rec.pointer(&format!("/top/{field}"))
                    .unwrap()
                    .is_null(),
                "{field} should be null on an unsolvable state"
            );
        }
        // No top route means nothing was re-executed: slippage is null, not zero.
        assert!(rec
            .pointer("/slippage")
            .unwrap()
            .is_null());
    }

    #[test]
    fn test_comparison_record_sandwiched() {
        let mut trade = DecodedTrade {
            tx_hash: TxHash::default(),
            block_number: 25_000_000,
            tx_index: 42,
            status: TradeStatus::Settled,
            venue: "relay".into(),
            solver: "1inch".into(),
            solver_source: AttributionSource::TraceMatch,
            decoder: "sender-netting",
            sender: Address::ZERO,
            token_in: Some(Address::repeat_byte(0x11)),
            token_out: Some(Address::repeat_byte(0x22)),
            amount_in: Some(U256::from(1_000u64)),
            amount_out: Some(U256::from(1_000u64)),
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            min_amount_out: None,
            declared_quote: None,
            quote_timestamp: None,
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
                algorithm: String::new(),
                quote_json: None,
                solved_route: None,
            })
        };
        let range = build_range(
            &trade,
            &empty_prices(),
            Some((solved(1_100), solved(1_050), solved(1_050))),
        );
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

    fn revert_solved(amount_out: u64) -> Outcome {
        Outcome::Solved(SolvedAmount {
            amount_out: U256::from(amount_out),
            amount_out_net_gas: U256::from(amount_out),
            gas_estimate: U256::from(21_000),
            algorithm: String::new(),
            quote_json: None,
            solved_route: None,
        })
    }

    fn reverted_trade(cause: RevertCause) -> DecodedTrade {
        DecodedTrade {
            tx_hash: TxHash::repeat_byte(0x55),
            block_number: 25_000_000,
            tx_index: 9,
            status: TradeStatus::Reverted { cause },
            venue: "relay".into(),
            solver: "fly".into(),
            solver_source: AttributionSource::TraceMatch,
            decoder: "reverted",
            sender: Address::repeat_byte(0x99),
            token_in: Some(Address::repeat_byte(0x11)),
            token_out: Some(Address::repeat_byte(0x22)),
            amount_in: Some(U256::from(1_000u64)),
            amount_out: None,
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            min_amount_out: Some(U256::from(10_000u64)),
            declared_quote: None,
            quote_timestamp: None,
            sandwich: None,
        }
    }

    #[test]
    fn test_reverted_record_slippage_floor_fillable_at_back() {
        let trade = reverted_trade(RevertCause::SlippageFloor);
        let range = build_range(
            &trade,
            &empty_prices(),
            Some((revert_solved(9_800), revert_solved(10_200), revert_solved(10_200))),
        );
        let rec = comparison_record(&range, &empty_prices(), &empty_prices());

        assert_eq!(rec.pointer("/tx_index").unwrap(), 9);
        assert_eq!(rec.pointer("/tx_hash").unwrap(), &TxHash::repeat_byte(0x55).to_string());
        // A reverted trade still records who sent it — the tx sender, since there is no netted
        // flow to draw a different tracked party from.
        assert_eq!(rec.pointer("/sender").unwrap(), &format!("{:#x}", Address::repeat_byte(0x99)));
        assert_eq!(rec.pointer("/status").unwrap(), "reverted");
        assert_eq!(rec.pointer("/cause").unwrap(), "slippage_floor");
        assert!(rec
            .pointer("/cause_detail")
            .unwrap()
            .is_null());
        assert_eq!(rec.pointer("/min_amount_out").unwrap(), "10000");
        // Nothing settled: settled-only fields are absent.
        assert!(rec
            .pointer("/settled_amount_out")
            .unwrap()
            .is_null());
        assert_eq!(rec.pointer("/top/fillable").unwrap(), false);
        assert_eq!(rec.pointer("/back/fillable").unwrap(), true);
        assert!(
            rec.pointer("/back/margin_bps")
                .unwrap()
                .as_f64()
                .unwrap() >
                0.0
        );
    }

    #[test]
    fn test_reverted_record_other_cause_carries_detail() {
        let trade = reverted_trade(RevertCause::Other("execution reverted".to_string()));
        let range = build_range(
            &trade,
            &empty_prices(),
            Some((
                Outcome::Unsolvable("no route".into()),
                Outcome::Unsolvable("no route".into()),
                Outcome::Unsolvable("no route".into()),
            )),
        );
        let rec = comparison_record(&range, &empty_prices(), &empty_prices());

        assert_eq!(rec.pointer("/cause").unwrap(), "other");
        assert_eq!(rec.pointer("/cause_detail").unwrap(), "execution reverted");
        assert!(rec
            .pointer("/top/fillable")
            .unwrap()
            .is_null());
    }

    #[test]
    fn test_reverted_trade_with_unknown_terms_has_no_top_or_back() {
        let mut trade = reverted_trade(RevertCause::Other("unknown revert".to_string()));
        trade.token_in = None;
        trade.token_out = None;
        trade.amount_in = None;
        trade.min_amount_out = None;
        let range = build_range(&trade, &empty_prices(), None);
        let rec = comparison_record(&range, &empty_prices(), &empty_prices());

        assert_eq!(rec.pointer("/status").unwrap(), "reverted");
        assert!(rec.pointer("/top").unwrap().is_null());
        assert!(rec.pointer("/back").unwrap().is_null());
        assert!(rec
            .pointer("/token_in")
            .unwrap()
            .is_null());
    }

    #[test]
    fn test_write_comparisons_appends_lines_for_settled_and_reverted() {
        let settled_range = build_range(
            &reverted_trade(RevertCause::OutOfGas),
            &empty_prices(),
            Some((Outcome::Unsolvable("x".into()), revert_solved(10_000), revert_solved(10_000))),
        );
        let mut buf: Vec<u8> = Vec::new();
        write_comparisons(
            &mut buf,
            std::slice::from_ref(&settled_range),
            &empty_prices(),
            &empty_prices(),
        );
        write_comparisons(
            &mut buf,
            std::slice::from_ref(&settled_range),
            &empty_prices(),
            &empty_prices(),
        );

        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.lines().count(), 2);
        for line in text.lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["cause"], "out_of_gas");
        }
    }
}

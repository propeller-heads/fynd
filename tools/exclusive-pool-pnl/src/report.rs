//! Renders the per-swap table, the totals block, and the JSON artifact.

use anyhow::Result;
use chrono::DateTime;
use serde_json::{json, Value};

use crate::{
    pnl::{totals, SwapPnl, Totals},
    pool::REFERENCE_SYMBOL,
};

/// Formats a Unix second as `YYYY-MM-DD HH:MM:SS` UTC.
fn format_time(timestamp: u64) -> String {
    DateTime::from_timestamp(timestamp as i64, 0)
        .map(|t| {
            t.format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| timestamp.to_string())
}

/// Prints one row per swap, valued at `horizon`.
pub fn print_swaps(swaps: &[SwapPnl], horizon: u64) {
    println!(
        "{:<19} {:>9} {:>5} {:>10} {:>10} {:>9} {:>10} {:>10} {:>10}",
        "time (UTC)", "block", "pool", "size", "pool px", "ref px", "fee bps", "fee", "LP pnl",
    );
    for swap in swaps {
        let markout = swap
            .markouts
            .iter()
            .find(|m| m.horizon_secs == horizon);
        // Side is stated from the pool's point of view: what its LPs did with token0.
        let side = if swap.pool_bought_token0() { "buys" } else { "sells" };
        let fee_bps = swap
            .signed_fee_bps
            .map(|bps| format!("{bps:.1}"))
            .unwrap_or_else(|| "-".to_string());
        let pending = if swap.fee.pending { "*" } else { "" };
        match markout {
            Some(m) => println!(
                "{:<19} {:>9} {:>5} {:>10.2} {:>10.2} {:>9.2} {:>10} {:>9.3}{} {:>10.3}",
                format_time(swap.timestamp),
                swap.block,
                side,
                swap.size(),
                swap.pool_price,
                m.reference_price,
                fee_bps,
                m.fee_revenue,
                pending,
                m.lp_pnl,
            ),
            None => println!(
                "{:<19} {:>9} {:>5} {:>10.2} {:>10.2} {:>9} {:>10} {:>10} {:>10}",
                format_time(swap.timestamp),
                swap.block,
                side,
                swap.size(),
                swap.pool_price,
                "-",
                fee_bps,
                "-",
                "no ref px",
            ),
        }
    }
    if swaps
        .iter()
        .any(|swap| swap.fee.pending)
    {
        println!("\n* fee not yet flushed to LPs; Ekubo credits it on the next pool interaction");
    }
}

/// Prints the totals block for every requested horizon.
pub fn print_totals(swaps: &[SwapPnl], horizons: &[u64]) {
    let all: Vec<Totals> = horizons
        .iter()
        .map(|horizon| totals(swaps, *horizon))
        .collect();
    let volume: f64 = swaps.iter().map(SwapPnl::size).sum();
    println!("\nswaps: {}", swaps.len());
    println!("volume: {volume:.2} (token1 units)");
    println!("reference: binance {REFERENCE_SYMBOL} 1m closes\n");
    println!(
        "{:>10} {:>12} {:>14} {:>10} {:>16} {:>14} {:>10}",
        "horizon", "priced vol", "fee revenue", "fee bps", "adverse sel.", "net LP pnl", "LP bps",
    );
    for t in &all {
        println!(
            "{:>9}s {:>12.2} {:>14.3} {:>10.1} {:>16.3} {:>14.3} {:>10.1}",
            t.horizon_secs,
            t.volume,
            t.fee_revenue,
            t.fee_bps(),
            t.adverse_selection,
            t.lp_pnl,
            t.lp_bps(),
        );
    }
    if all
        .iter()
        .any(|t| t.volume + 1e-9 < volume)
    {
        println!(
            "\nA horizon prices less than the full volume when it has not elapsed yet for every \
             swap; those swaps are excluded from that row."
        );
    }
}

/// Builds the JSON artifact holding every swap and every horizon.
pub fn to_json(swaps: &[SwapPnl], horizons: &[u64]) -> Value {
    let rows: Vec<Value> = swaps
        .iter()
        .map(|swap| {
            json!({
                "block": swap.block,
                "timestamp": swap.timestamp,
                "time_utc": format_time(swap.timestamp),
                "tx": swap.tx,
                "delta0": swap.delta0,
                "delta1": swap.delta1,
                "size": swap.size(),
                "pool_price": swap.pool_price,
                "signed_fee_bps": swap.signed_fee_bps,
                "lp_fee0_raw": swap.fee.amount0.to_string(),
                "lp_fee1_raw": swap.fee.amount1.to_string(),
                "lp_fee_pending": swap.fee.pending,
                "markouts": swap.markouts.iter().map(|m| json!({
                    "horizon_secs": m.horizon_secs,
                    "reference_price": m.reference_price,
                    "adverse_selection": m.adverse_selection,
                    "fee_revenue": m.fee_revenue,
                    "lp_pnl": m.lp_pnl,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let summary: Vec<Value> = horizons
        .iter()
        .map(|horizon| {
            let t = totals(swaps, *horizon);
            json!({
                "horizon_secs": t.horizon_secs,
                "volume": t.volume,
                "fee_revenue": t.fee_revenue,
                "fee_bps": t.fee_bps(),
                "adverse_selection": t.adverse_selection,
                "lp_pnl": t.lp_pnl,
                "lp_bps": t.lp_bps(),
            })
        })
        .collect();
    json!({ "reference_symbol": REFERENCE_SYMBOL, "swaps": rows, "totals": summary })
}

/// Writes the JSON artifact to `path`.
pub fn write_json(swaps: &[SwapPnl], horizons: &[u64], path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, serde_json::to_string_pretty(&to_json(swaps, horizons))?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pnl::{AttributedFee, Markout};

    fn swap() -> SwapPnl {
        SwapPnl {
            block: 25_805_516,
            timestamp: 1_787_339_435,
            tx: "0xabc".to_string(),
            delta0: -0.651_354,
            delta1: 1_522.185,
            pool_price: 2_336.95,
            signed_fee_bps: Some(283.6),
            fee: AttributedFee { amount0: 1, amount1: 0, pending: false },
            markouts: vec![Markout {
                horizon_secs: 300,
                reference_price: 2_411.92,
                adverse_selection: -48.829,
                fee_revenue: 44.558,
                lp_pnl: -4.271,
            }],
        }
    }

    #[test]
    fn formats_a_timestamp_as_utc() {
        assert_eq!(format_time(1_787_339_435), "2026-08-21 19:10:35");
    }

    #[test]
    fn json_holds_one_row_per_swap_and_one_total_per_horizon() {
        let value = to_json(&[swap()], &[300, 3_600]);
        assert_eq!(
            value["swaps"]
                .as_array()
                .expect("array")
                .len(),
            1
        );
        assert_eq!(
            value["totals"]
                .as_array()
                .expect("array")
                .len(),
            2
        );
    }

    #[test]
    fn json_reports_raw_fees_as_strings_to_survive_u128() {
        let value = to_json(&[swap()], &[300]);
        assert_eq!(value["swaps"][0]["lp_fee0_raw"], "1");
    }

    #[test]
    fn totals_skip_a_horizon_the_swap_has_no_price_for() {
        let value = to_json(&[swap()], &[3_600]);
        assert_eq!(value["totals"][0]["volume"], 0.0);
    }
}

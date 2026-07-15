//! Decoder verification against Allium's `aggregator_trades` ground truth.
//!
//! The `verify` subcommand decodes a block locally, fetches the same block from Allium, and diffs
//! the two sets of trades — reporting matches, token/solver/amount mismatches, and gaps in either
//! direction. This is a development sanity check, not a production code path.

pub(crate) mod allium;

use std::collections::{HashMap, HashSet};

use alloy::{
    primitives::{Address, Bytes, TxHash, U256},
    providers::Provider,
    rpc::types::{TransactionInput, TransactionRequest},
};
use anyhow::Context;
use tracing::warn;

use crate::{
    decoder::{DecodedTrade, Decoder},
    verify::allium::{AlliumClient, AlliumRow},
};

const NATIVE_DECIMALS: u8 = 18;
/// `keccak256("decimals()")[..4]`.
const DECIMALS_SELECTOR: [u8; 4] = [0x31, 0x3c, 0xe5, 0x67];

/// Outcome of comparing one transaction's decode against Allium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Match,
    TokenMismatch,
    SolverMismatch,
    AmountMismatch,
    OursOnly,
    AlliumOnly,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Match => "match",
            Status::TokenMismatch => "token mismatch",
            Status::SolverMismatch => "solver mismatch",
            Status::AmountMismatch => "amount mismatch",
            Status::OursOnly => "ours only",
            Status::AlliumOnly => "allium only (decoder gap)",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct TxComparison {
    pub(crate) tx_hash: TxHash,
    pub(crate) status: Status,
    pub(crate) detail: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct VerifyReport {
    pub(crate) comparisons: Vec<TxComparison>,
}

impl VerifyReport {
    #[expect(clippy::print_stdout)]
    pub(crate) fn print(&self) {
        let count = |status: Status| {
            self.comparisons
                .iter()
                .filter(|c| c.status == status)
                .count()
        };
        println!("\nVerification vs Allium ground truth:");
        println!("  compared txs:          {}", self.comparisons.len());
        println!("  matches:               {}", count(Status::Match));
        println!("  token mismatches:      {}", count(Status::TokenMismatch));
        println!("  solver mismatches:     {}", count(Status::SolverMismatch));
        println!("  amount mismatches:     {}", count(Status::AmountMismatch));
        println!("  ours only:             {}", count(Status::OursOnly));
        println!("  allium only (gaps):    {}", count(Status::AlliumOnly));

        let problems: Vec<&TxComparison> = self
            .comparisons
            .iter()
            .filter(|c| c.status != Status::Match)
            .collect();
        if problems.is_empty() {
            return;
        }
        println!("\n  details:");
        for comparison in problems {
            println!("    [{}] {}", comparison.status.label(), comparison.tx_hash);
            if !comparison.detail.is_empty() {
                println!("        {}", comparison.detail);
            }
        }
    }
}

/// Decode each block locally and compare against Allium's `aggregator_trades`.
pub(crate) async fn run<P: Provider>(
    decoder: &mut Decoder<P>,
    allium: &AlliumClient,
    blocks: &[u64],
    tolerance_bps: f64,
) -> anyhow::Result<VerifyReport> {
    let mut comparisons = Vec::new();
    let mut decimals = HashMap::new();
    for &block in blocks {
        let ours = match decoder.decode_block(block).await {
            Ok(ours) => ours,
            Err(error) => {
                warn!(block, %error, "failed to decode block; skipping");
                continue;
            }
        };
        let theirs = allium
            .fetch_block(block)
            .await
            .with_context(|| format!("failed to fetch Allium trades for block {block}"))?;
        compare_block(
            decoder.provider(),
            &ours,
            &theirs,
            tolerance_bps,
            &mut decimals,
            &mut comparisons,
        )
        .await;
    }
    Ok(VerifyReport { comparisons })
}

async fn compare_block<P: Provider>(
    provider: &P,
    ours: &[DecodedTrade],
    theirs: &[AlliumRow],
    tolerance_bps: f64,
    decimals: &mut HashMap<Address, u8>,
    out: &mut Vec<TxComparison>,
) {
    let our_by_tx: HashMap<TxHash, &DecodedTrade> = ours
        .iter()
        .map(|t| (t.tx_hash, t))
        .collect();
    let mut their_by_tx: HashMap<TxHash, Vec<&AlliumRow>> = HashMap::new();
    for row in theirs {
        let Some(tx) = row
            .transaction_hash
            .as_deref()
            .and_then(|hash| hash.parse::<TxHash>().ok())
        else {
            warn!(hash = ?row.transaction_hash, "skipping Allium row with unusable tx hash");
            continue;
        };
        their_by_tx
            .entry(tx)
            .or_default()
            .push(row);
    }

    for (tx, trade) in &our_by_tx {
        match their_by_tx.get(tx) {
            Some(rows) => {
                out.push(compare_trade(provider, trade, rows, tolerance_bps, decimals).await);
            }
            None => out.push(TxComparison {
                tx_hash: *tx,
                status: Status::OursOnly,
                detail: format!("decoded a {} trade Allium does not list", trade.solver),
            }),
        }
    }
    for (tx, rows) in &their_by_tx {
        if our_by_tx.contains_key(tx) {
            continue;
        }
        let project = rows
            .iter()
            .find_map(|r| r.project.clone())
            .unwrap_or_else(|| "unknown".to_string());
        out.push(TxComparison {
            tx_hash: *tx,
            status: Status::AlliumOnly,
            detail: format!("Allium has a {project} trade we did not decode"),
        });
    }
}

async fn compare_trade<P: Provider>(
    provider: &P,
    ours: &DecodedTrade,
    rows: &[&AlliumRow],
    tolerance_bps: f64,
    decimals: &mut HashMap<Address, u8>,
) -> TxComparison {
    let mut detail = Vec::new();
    let token_ok = tokens_agree(ours, rows, &mut detail);
    let solver_ok = solver_agrees(ours, rows, &mut detail);
    let amount_ok = amounts_agree(provider, ours, rows, tolerance_bps, decimals, &mut detail).await;

    let status = if !token_ok {
        Status::TokenMismatch
    } else if !solver_ok {
        Status::SolverMismatch
    } else if !amount_ok {
        Status::AmountMismatch
    } else {
        Status::Match
    };

    TxComparison { tx_hash: ours.tx_hash, status, detail: detail.join("; ") }
}

/// Allium splits a multi-leg swap into per-leg rows, so check membership in sets rather than
/// exact one-to-one matching: our netted `token_in` must appear among the sold tokens and our
/// `token_out` among the bought tokens across all rows for this tx.
fn tokens_agree(ours: &DecodedTrade, rows: &[&AlliumRow], detail: &mut Vec<String>) -> bool {
    let sold: HashSet<Address> = rows
        .iter()
        .filter_map(|r| {
            r.token_sold_address
                .as_deref()
                .and_then(parse_addr)
        })
        .collect();
    let bought: HashSet<Address> = rows
        .iter()
        .filter_map(|r| {
            r.token_bought_address
                .as_deref()
                .and_then(parse_addr)
        })
        .collect();

    let in_ok = sold.contains(&ours.token_in);
    let out_ok = bought.contains(&ours.token_out);
    if !in_ok {
        detail.push(format!("token_in {} not among Allium sold tokens", ours.token_in));
    }
    if !out_ok {
        detail.push(format!("token_out {} not among Allium bought tokens", ours.token_out));
    }
    in_ok && out_ok
}

fn solver_agrees(ours: &DecodedTrade, rows: &[&AlliumRow], detail: &mut Vec<String>) -> bool {
    let ours_norm = normalize_name(&ours.solver);
    let agrees = rows
        .iter()
        .filter_map(|r| r.project.as_deref())
        .any(|project| names_match(&ours_norm, &normalize_name(project)));
    if !agrees {
        let projects: Vec<&str> = rows
            .iter()
            .filter_map(|r| r.project.as_deref())
            .collect();
        detail.push(format!("solver {} vs Allium {projects:?}", ours.solver));
    }
    agrees
}

async fn amounts_agree<P: Provider>(
    provider: &P,
    ours: &DecodedTrade,
    rows: &[&AlliumRow],
    tolerance_bps: f64,
    decimals: &mut HashMap<Address, u8>,
    detail: &mut Vec<String>,
) -> bool {
    // Allium records each leg of a multi-leg swap as its own row. We net the whole swap into one
    // in/out pair, which has no per-leg counterpart to compare against — only compare amounts when
    // Allium also reported a single leg.
    let [row] = rows else {
        detail.push(format!("amount not compared ({} Allium legs for this tx)", rows.len()));
        return true;
    };
    let (Some(sold), Some(bought)) = (row.token_sold_amount, row.token_bought_amount) else {
        detail.push("amount not compared (Allium amount is null)".to_string());
        return true;
    };

    let in_leg =
        AmountLeg { token: ours.token_in, ours: ours.amount_in, theirs: sold, field: "amount_in" };
    let out_leg = AmountLeg {
        token: ours.token_out,
        ours: ours.amount_out,
        theirs: bought,
        field: "amount_out",
    };

    let in_ok = amount_within(provider, &in_leg, tolerance_bps, decimals, detail).await;
    let out_ok = amount_within(provider, &out_leg, tolerance_bps, decimals, detail).await;
    in_ok && out_ok
}

/// One side of a decoded trade to compare: our on-chain amount (raw integer) vs Allium's
/// human-readable float, which must be normalized to the same decimal scale before comparison.
struct AmountLeg {
    token: Address,
    ours: U256,
    theirs: f64,
    field: &'static str,
}

async fn amount_within<P: Provider>(
    provider: &P,
    leg: &AmountLeg,
    tolerance_bps: f64,
    cache: &mut HashMap<Address, u8>,
    detail: &mut Vec<String>,
) -> bool {
    let Some(decimals) = token_decimals(provider, leg.token, cache).await else {
        detail.push(format!("{} not compared (decimals unavailable for {})", leg.field, leg.token));
        return true;
    };
    let normalized = normalize_amount(leg.ours, decimals);
    let diff = bps_diff(normalized, leg.theirs);
    if diff > tolerance_bps {
        detail.push(format!(
            "{} {normalized:.6} vs Allium {:.6} ({diff:.0} bps)",
            leg.field, leg.theirs
        ));
        return false;
    }
    true
}

async fn token_decimals<P: Provider>(
    provider: &P,
    token: Address,
    cache: &mut HashMap<Address, u8>,
) -> Option<u8> {
    if token == Address::ZERO {
        return Some(NATIVE_DECIMALS);
    }
    if let Some(decimals) = cache.get(&token) {
        return Some(*decimals);
    }
    match fetch_decimals(provider, token).await {
        Ok(decimals) => {
            cache.insert(token, decimals);
            Some(decimals)
        }
        Err(error) => {
            warn!(%token, %error, "failed to fetch token decimals");
            None
        }
    }
}

async fn fetch_decimals<P: Provider>(provider: &P, token: Address) -> anyhow::Result<u8> {
    let tx = TransactionRequest {
        to: Some(token.into()),
        input: TransactionInput::new(Bytes::from_static(&DECIMALS_SELECTOR)),
        ..Default::default()
    };
    let result = provider
        .call(tx)
        .await
        .with_context(|| format!("decimals() call failed for {token}"))?;
    // decimals() returns uint8 right-aligned in a 32-byte ABI word; the value is the last byte.
    result
        .last()
        .copied()
        .with_context(|| format!("empty decimals() return for {token}"))
}

fn parse_addr(value: &str) -> Option<Address> {
    value.parse().ok()
}

fn normalize_amount(amount: U256, decimals: u8) -> f64 {
    crate::usd::u256_to_f64(amount) / 10f64.powi(i32::from(decimals))
}

fn bps_diff(ours: f64, theirs: f64) -> f64 {
    if theirs == 0.0 {
        return f64::INFINITY;
    }
    ((ours - theirs).abs() / theirs) * 10_000.0
}

/// Lowercase and strip non-alphanumerics, then fold known aliases: `zeroex` → `0x`, so that
/// e.g. `uniswap_x`/`uniswap`, `1inch`/`1inch_ar_v6`, and `0x`/`zeroex` compare equal.
fn normalize_name(name: &str) -> String {
    let normalized: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect();
    match normalized.as_str() {
        "zeroex" => "0x".to_string(),
        _ => normalized,
    }
}

fn names_match(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a.starts_with(b) || b.starts_with(a))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;

    use super::*;
    use crate::decoder::AttributionSource;

    fn addr(n: u8) -> Address {
        let mut bytes = [0u8; 20];
        bytes[19] = n;
        Address::from(bytes)
    }

    fn trade(token_in: Address, token_out: Address, solver: &str) -> DecodedTrade {
        DecodedTrade {
            tx_hash: TxHash::ZERO,
            block_number: 1,
            tx_index: 0,
            venue: "relay".to_string(),
            solver: solver.to_string(),
            solver_source: AttributionSource::TraceMatch,
            sender: addr(1),
            token_in,
            token_out,
            amount_in: U256::from(1000),
            amount_out: U256::from(2000),
            venue_fee: None,
            venue_fee_out: None,
            settled_gas: None,
            quote: None,
            sandwich: None,
        }
    }

    fn row(sold: Address, bought: Address, project: &str) -> AlliumRow {
        AlliumRow {
            project: Some(project.to_string()),
            token_sold_address: Some(sold.to_string()),
            token_sold_amount: Some(1.0),
            token_bought_address: Some(bought.to_string()),
            token_bought_amount: Some(2.0),
            transaction_hash: Some("0xabc".to_string()),
        }
    }

    #[test]
    fn tokens_agree_when_present() {
        let ours = trade(addr(10), addr(11), "1inch");
        let allium = row(addr(10), addr(11), "1inch");
        let mut detail = Vec::new();
        assert!(tokens_agree(&ours, &[&allium], &mut detail));
        assert!(detail.is_empty());
    }

    #[test]
    fn tokens_disagree_flags_detail() {
        let ours = trade(addr(10), addr(99), "1inch");
        let allium = row(addr(10), addr(11), "1inch");
        let mut detail = Vec::new();
        assert!(!tokens_agree(&ours, &[&allium], &mut detail));
        assert_eq!(detail.len(), 1);
    }

    #[test]
    fn solver_matches_on_prefix() {
        let ours = trade(addr(10), addr(11), "uniswap");
        let allium = row(addr(10), addr(11), "uniswap_x");
        let mut detail = Vec::new();
        assert!(solver_agrees(&ours, &[&allium], &mut detail));
    }

    #[test]
    fn solver_matches_zeroex_alias() {
        let ours = trade(addr(10), addr(11), "0x");
        let allium = row(addr(10), addr(11), "zeroex");
        let mut detail = Vec::new();
        assert!(solver_agrees(&ours, &[&allium], &mut detail));
    }

    #[test]
    fn solver_disagrees_on_different_venue() {
        let ours = trade(addr(10), addr(11), "tycho");
        let allium = row(addr(10), addr(11), "1inch");
        let mut detail = Vec::new();
        assert!(!solver_agrees(&ours, &[&allium], &mut detail));
    }

    #[test]
    fn normalize_amount_applies_decimals() {
        assert!((normalize_amount(U256::from(1_000_000u64), 6) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn bps_diff_zero_for_equal() {
        assert!(bps_diff(1.0, 1.0).abs() < 1e-9);
    }

    #[test]
    fn bps_diff_scales() {
        assert!((bps_diff(1.01, 1.0) - 100.0).abs() < 1e-6);
    }
}

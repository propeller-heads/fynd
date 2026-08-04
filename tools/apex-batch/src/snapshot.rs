//! The subset of hindsight's block-batch capture JSONL this runner reads.
//!
//! `hindsight monitor --capture-dir` writes one JSON object per block (see
//! `tools/hindsight/src/capture.rs`). These structs deserialize the fields the batch runner needs
//! and serde ignores the rest, the same "own your subset" arrangement `hindsight`'s own
//! `report/record.rs` has with its comparisons writer.
//!
//! The two crates cannot share the type: `hindsight` is a binary with no library target, so this
//! mirror is maintained by hand. The round-trip test at the bottom pins the field names against a
//! literal copied from the writer's output, so a rename on the capture side fails here loudly
//! instead of silently deserializing to `None`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use alloy::primitives::{Address, U256};
use serde::Deserialize;

/// Where an order's `min_amount_out` came from. Mirrors hindsight's `capture::LimitSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    /// Decoded from the settled transaction's own calldata — a real on-chain commitment.
    Extracted,
    /// Derived from the executed output because the settling solver declared no floor. Results
    /// are split on this: a synthetic limit is an assumption, and a loose one flatters APEX.
    Synthetic,
}

/// Fynd's top-of-block counterfactual for one order — the baseline APEX's batch clearing is
/// scored against.
#[derive(Debug, Clone, Deserialize)]
pub struct FyndCounterfactual {
    /// `solved`, `partial`, or `unsolvable`.
    pub status: String,
    /// Gross output Fynd quoted, in `token_out` native units. Absent unless solved.
    #[serde(default, with = "u256_string_option")]
    pub amount_out: Option<U256>,
    /// Output after Fynd's own estimated gas cost. Absent unless solved.
    #[serde(default, with = "u256_string_option")]
    pub amount_out_net_gas: Option<U256>,
    /// The worker pool that won the quote, for per-algorithm splits of the baseline.
    #[serde(default)]
    pub algorithm: Option<String>,
    /// Why Fynd had no route — kept so an order missing from the batch can be attributed to
    /// Fynd's coverage rather than to APEX.
    #[serde(default)]
    pub unsolvable_reason: Option<String>,
}

impl FyndCounterfactual {
    /// Whether Fynd produced a full-size quote, i.e. whether there is a baseline to beat at all.
    pub fn is_solved(&self) -> bool {
        self.status == "solved"
    }
}

/// One settled trade as an APEX batch order.
#[derive(Debug, Clone, Deserialize)]
pub struct CapturedTrade {
    pub tx_hash: String,
    /// Position in the block, for the deferred original-positions batch variant.
    pub tx_index: u64,
    pub venue: String,
    pub solver: String,
    pub token_in: Address,
    pub token_out: Address,
    #[serde(with = "u256_string")]
    pub amount_in: U256,
    #[serde(with = "u256_string")]
    pub settled_amount_out: U256,
    /// Settled output after the gas the trader paid for the route.
    #[serde(with = "u256_string")]
    pub settled_amount_out_net_gas: U256,
    /// The order's output floor. Absent when neither an extracted nor a synthetic limit was
    /// derivable, which excludes the order from the batch.
    #[serde(default, with = "u256_string_option")]
    pub min_amount_out: Option<U256>,
    /// Provenance of `min_amount_out`; absent exactly when the limit is.
    #[serde(default)]
    pub limit_source: Option<LimitSource>,
    /// Whether MEV bracketed the settled trade, making its output an unfair baseline.
    #[serde(default)]
    pub sandwiched: bool,
    pub fynd_top: FyndCounterfactual,
}

/// One captured block: the orders, and the price view they were solved under.
#[derive(Debug, Clone, Deserialize)]
pub struct BlockBatchSnapshot {
    pub block: u64,
    /// Fynd's derived per-token price at N-1: the token's native units per wei of the gas token.
    /// APEX's tâtonnement start, and the same view the Fynd baseline was valued in.
    pub token_prices: HashMap<Address, f64>,
    pub trades: Vec<CapturedTrade>,
}

/// Read every `.jsonl` file in `dir` into block snapshots, sorted by block number.
///
/// Should: walk the directory's `batches-*.jsonl` files, parse one snapshot per line, skip and
/// count malformed lines rather than failing the run (a truncated tail is the normal shape of a
/// file a live monitor is still appending to), then sort by `block` so the runner's per-block
/// series is monotonic regardless of file order.
pub fn load_snapshots(_dir: &Path) -> anyhow::Result<Vec<BlockBatchSnapshot>> {
    todo!(
        "read every jsonl line in the capture directory, skipping malformed ones, sorted by block"
    )
}

/// The capture files inside `dir`, in name order.
///
/// Should: list `dir`, keep entries whose file name ends in `.jsonl`, and sort — the daily
/// rotation makes the name order the chronological one.
pub fn capture_files(_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    todo!("list and sort the directory's .jsonl capture files")
}

/// Decimal-string `U256` de/serialization, matching the capture writer's `to_string`.
mod u256_string {
    use alloy::primitives::U256;
    use serde::{Deserialize, Deserializer};

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<U256, D::Error> {
        let raw = String::deserialize(de)?;
        raw.parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Optional decimal-string `U256`, for fields the capture omits when absent.
mod u256_string_option {
    use alloy::primitives::U256;
    use serde::{Deserialize, Deserializer};

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Option<U256>, D::Error> {
        let Some(raw) = Option::<String>::deserialize(de)? else {
            return Ok(None);
        };
        raw.parse()
            .map(Some)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line in exactly the shape `hindsight::capture::write_snapshot` emits. Copied from the
    /// writer, not hand-designed: it is the contract, and it is what makes a field rename on the
    /// capture side fail here.
    const CAPTURE_LINE: &str = r#"{
        "block": 25000000,
        "token_prices": {
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48": 2e-9,
            "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2": 1.0
        },
        "trades": [
            {
                "tx_hash": "0x4242424242424242424242424242424242424242424242424242424242424242",
                "tx_index": 3,
                "venue": "relay",
                "solver": "kyberswap",
                "decoder": "sender-netting",
                "token_in": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
                "token_out": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
                "amount_in": "1000000000000000000",
                "settled_amount_out": "1000000000",
                "settled_amount_out_net_gas": "998500000",
                "min_amount_out": "990000000",
                "limit_source": "extracted",
                "sandwiched": false,
                "fynd_top": {
                    "status": "solved",
                    "amount_out": "1010000000",
                    "amount_out_net_gas": "1005000000",
                    "algorithm": "most_liquid",
                    "unsolvable_reason": null
                }
            }
        ]
    }"#;

    #[test]
    #[ignore = "scaffold: enable once the capture writer emits real trade records"]
    fn test_parses_a_line_from_the_capture_writer() {
        let snapshot: BlockBatchSnapshot =
            serde_json::from_str(CAPTURE_LINE).expect("the capture line must parse");

        assert_eq!(snapshot.block, 25_000_000);
        assert_eq!(snapshot.token_prices.len(), 2);
        assert_eq!(snapshot.trades.len(), 1);

        let trade = &snapshot.trades[0];
        assert_eq!(trade.venue, "relay");
        assert_eq!(trade.solver, "kyberswap");
        assert_eq!(trade.tx_index, 3);
        // Amounts are decimal strings on the wire: a JSON number would lose precision above 2^53,
        // which every 18-decimal amount in this file exceeds.
        assert_eq!(trade.amount_in, U256::from(1_000_000_000_000_000_000u64));
        assert_eq!(trade.settled_amount_out, U256::from(1_000_000_000u64));
        assert_eq!(trade.min_amount_out, Some(U256::from(990_000_000u64)));
        assert_eq!(trade.limit_source, Some(LimitSource::Extracted));
        assert!(!trade.sandwiched);
        assert!(trade.fynd_top.is_solved());
        assert_eq!(trade.fynd_top.amount_out, Some(U256::from(1_010_000_000u64)));
        assert_eq!(trade.fynd_top.algorithm.as_deref(), Some("most_liquid"));
    }

    #[test]
    #[ignore = "scaffold: enable once the capture writer emits real trade records"]
    fn test_absent_limit_parses_as_none() {
        // The capture omits `min_amount_out` and `limit_source` entirely when no floor was
        // derivable. That must read as "unknown", never as a zero floor every batch clears.
        let line = CAPTURE_LINE
            .replace("\"min_amount_out\": \"990000000\",", "")
            .replace("\"limit_source\": \"extracted\",", "");
        let snapshot: BlockBatchSnapshot =
            serde_json::from_str(&line).expect("a limitless trade must still parse");

        let trade = &snapshot.trades[0];
        assert_eq!(trade.min_amount_out, None);
        assert_eq!(trade.limit_source, None);
    }

    #[test]
    #[ignore = "scaffold: enable once the capture writer emits real trade records"]
    fn test_unsolved_counterfactual_is_not_a_zero_baseline() {
        let line = CAPTURE_LINE.replace(
            r#""status": "solved",
                    "amount_out": "1010000000",
                    "amount_out_net_gas": "1005000000",
                    "algorithm": "most_liquid",
                    "unsolvable_reason": null"#,
            r#""status": "unsolvable",
                    "amount_out": null,
                    "amount_out_net_gas": null,
                    "algorithm": null,
                    "unsolvable_reason": "missing token in Tycho""#,
        );
        let snapshot: BlockBatchSnapshot =
            serde_json::from_str(&line).expect("an unsolvable baseline must parse");

        let counterfactual = &snapshot.trades[0].fynd_top;
        assert!(!counterfactual.is_solved());
        assert_eq!(counterfactual.amount_out, None);
        assert_eq!(
            counterfactual
                .unsolvable_reason
                .as_deref(),
            Some("missing token in Tycho")
        );
    }
}

//! The subset of the monitor's JSONL comparison record the report reads.
//!
//! `monitor --comparisons-dir` writes one JSON object per re-solved trade (see
//! `resolve::jsonl`). The report only needs the headline fields, so these structs deserialize a
//! subset; serde ignores the rest (the slim quote, per-hop route, back-state amounts). The
//! round-trip test in this module writes a record through the monitor's own writer and parses it
//! back, so a rename on the writer side is caught here rather than silently reading `None`.

use serde::Deserialize;

/// One re-solved trade: the settled trade's identity plus Fynd's result at each block state.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Comparison {
    pub block: u64,
    pub settled_tx: String,
    pub venue: String,
    pub solver: String,
    pub token_in: String,
    pub token_out: String,
    /// Optimistic state (N-1); the report's headline, matching the monitor's headline verdict.
    pub top: State,
    /// The mock-`PropAMM` outcome, present only for runs the monitor drove with `--propamm-pair`.
    #[serde(default)]
    pub propamm: Option<PropAmm>,
}

/// One trade's mock-`PropAMM` outcome, as written by `monitor --propamm-pair`.
///
/// The mock pool quotes at a configured fee-free price and charges nothing, so `fee_headroom` is
/// the fee the signed extension could have charged on this trade and still beaten the public
/// market.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PropAmm {
    /// The mirrored pair as token symbols, e.g. `WETH/USDC`.
    #[serde(default)]
    pub pair: Option<String>,
    /// The offset the mock was priced at, in basis points relative to the public best route for
    /// this order. Absent for an order that was not calibrated, which is excluded from the
    /// group test.
    #[serde(default)]
    pub offset_bps: Option<i32>,
    /// Whether the winning route ran through the mock pool.
    pub won: bool,
    /// That headroom as a fraction of the committed output, in basis points.
    #[serde(default)]
    pub fee_headroom_bps: Option<f64>,
    /// The committed output valued in USD — the flow the pool captured.
    #[serde(default)]
    pub committed_usd: Option<f64>,
    /// The headroom valued in USD.
    #[serde(default)]
    pub fee_headroom_usd: Option<f64>,
    /// Whether Fynd beat the settled trade **without** the mock — public liquidity only.
    #[serde(default)]
    pub without_won: Option<bool>,
    /// Whether Fynd beat the settled trade **with** the mock available.
    #[serde(default)]
    pub with_won: Option<bool>,
    /// USD Fynd gained over the settled trade without the mock. Negative on a loss.
    #[serde(default)]
    pub without_improvement_usd: Option<f64>,
    /// USD Fynd gained over the settled trade with the mock available.
    #[serde(default)]
    pub with_improvement_usd: Option<f64>,
    /// Net-of-gas bps over the settled trade without the mock.
    #[serde(default)]
    pub without_net_bps: Option<f64>,
    /// Net-of-gas bps over the settled trade with the mock available.
    #[serde(default)]
    pub with_net_bps: Option<f64>,
}

/// Fynd's result at one block state.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct State {
    pub verdict: String,
    #[serde(default)]
    pub net_bps: Option<f64>,
    /// Signed USD delta of Fynd's output vs the settled output — negative on a loss. Present only
    /// for a solved state.
    #[serde(default)]
    pub improvement_usd: Option<f64>,
    /// The settled trade's gross notional in USD — present whenever the output (or input) token is
    /// priced, including for unsolvable trades, so coverage can be weighed by dollars.
    #[serde(default)]
    pub settled_value_usd: Option<f64>,
}

impl State {
    /// Fynd produced a full-size quote and it was scored against the settled trade (win or loss),
    /// or it was scored but discounted as sandwiched. These are the "served" trades — the ones
    /// where routing quality, not coverage, is the question.
    pub(crate) fn is_served(&self) -> bool {
        self.verdict == "win" || self.verdict == "loss" || self.verdict == "sandwiched"
    }

    /// A fair win/loss comparison: served and not discounted by MEV. Savings aggregates are taken
    /// over these, matching the dashboard (which excludes sandwiched trades from the value view).
    pub(crate) fn is_scored(&self) -> bool {
        self.verdict == "win" || self.verdict == "loss"
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, TxHash, U256};

    use super::*;
    use crate::{
        decoder::{AttributionSource, DecodedTrade, Registry},
        resolve::{build_range, jsonl::write_comparisons, Outcome, SolvedAmount},
        usd::Prices,
    };

    /// A record written by the monitor's own writer parses back into `Comparison` with the headline
    /// fields populated — guards against the writer and this reader drifting apart.
    #[test]
    fn test_parses_a_record_from_the_monitor_writer() {
        let usdc: Address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            .parse()
            .unwrap();
        let weth: Address = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"
            .parse()
            .unwrap();
        let mut prices = Prices::new(&Registry::ethereum());
        prices.insert(usdc, 2e-9);
        prices.insert(weth, 1.0);

        let trade = DecodedTrade {
            tx_hash: TxHash::repeat_byte(0x42),
            block_number: 25_000_000,
            tx_index: 0,
            venue: "relay".into(),
            solver: "1inch".into(),
            solver_source: AttributionSource::TraceMatch,
            decoder: "sender-netting",
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
        // Top solved above settled → a win with a positive USD improvement.
        let top = Outcome::Solved(SolvedAmount {
            amount_out: U256::from(1_010_000_000u64),
            amount_out_net_gas: U256::from(1_010_000_000u64),
            gas_estimate: U256::from(21_000u64),
            quote_json: None,
        });
        let range = build_range(&trade, &prices, top, Outcome::Unsolvable("x".into()));

        let mut buf = Vec::new();
        write_comparisons(&mut buf, std::slice::from_ref(&range), &prices, &prices, &[]);
        let line = String::from_utf8(buf).unwrap();

        let record: Comparison = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(record.block, 25_000_000);
        assert_eq!(record.venue, "relay");
        assert_eq!(record.solver, "1inch");
        assert_eq!(record.top.verdict, "win");
        assert!(record.top.is_scored());
        assert!(record.top.net_bps.unwrap() > 0.0);
        assert!((record.top.improvement_usd.unwrap() - 10.0).abs() < 1e-3);
        assert_eq!(record.token_out, format!("{usdc:#x}"));
    }

    /// The mock-`PropAMM` fields the monitor writes parse back too — the same writer/reader drift
    /// guard, for the fields the `PropAMM` section keys off.
    #[test]
    fn test_parses_the_propamm_fields_from_the_monitor_writer() {
        use crate::propamm::report::{Observation, Record};

        let usdc: Address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            .parse()
            .unwrap();
        let observed = Observation {
            token_out: usdc,
            solved: true,
            won: true,
            committed_amount_out: Some(1_000_000_000u64.into()),
            fee_headroom: Some(400_000u64.into()),
            offset_bps: Some(5),
            public_best_out: Some(1_000_000_000u64.into()),
            without: Some(crate::propamm::report::PublicOnly {
                amount_out: 1_000_000_000u64.into(),
                amount_out_net_gas: 999_000_000u64.into(),
            }),
        };
        let record = Record::new(
            &observed,
            Some("WETH/USDC".to_string()),
            Some(1_000.0),
            Some(0.4),
            crate::propamm::report::AbResult {
                without_won: Some(false),
                with_won: Some(true),
                without_improvement_usd: Some(-1.0),
                with_improvement_usd: Some(3.0),
                without_net_bps: Some(-8.0),
                with_net_bps: Some(24.0),
            },
        );

        let line = serde_json::to_string(&serde_json::json!({
            "block": 1,
            "settled_tx": "0xabc",
            "venue": "relay",
            "solver": "1inch",
            "token_in": "0xaaa",
            "token_out": "0xbbb",
            "top": { "verdict": "win" },
            "propamm": record,
        }))
        .unwrap();

        let parsed: Comparison = serde_json::from_str(&line).unwrap();
        let propamm = parsed
            .propamm
            .expect("the propamm field round-trips");
        assert_eq!(propamm.pair.as_deref(), Some("WETH/USDC"));
        assert_eq!(propamm.offset_bps, Some(5));
        assert!(propamm.won);
        // 400_000 / 1_000_000_000 = 4 bps.
        assert!((propamm.fee_headroom_bps.unwrap() - 4.0).abs() < 1e-9);
        assert!((propamm.committed_usd.unwrap() - 1_000.0).abs() < 1e-9);
        assert!((propamm.fee_headroom_usd.unwrap() - 0.4).abs() < 1e-9);
    }

    /// An ordinary monitor run writes no `propamm` field, and the reader must treat that as absent
    /// rather than failing to parse the whole record.
    #[test]
    fn test_propamm_is_absent_when_the_harness_is_off() {
        let line = serde_json::json!({
            "block": 1, "settled_tx": "0xabc", "venue": "relay", "solver": "1inch",
            "token_in": "0xaaa", "token_out": "0xbbb", "top": { "verdict": "win" },
            "propamm": serde_json::Value::Null,
        })
        .to_string();
        let parsed: Comparison = serde_json::from_str(&line).unwrap();
        assert!(parsed.propamm.is_none());
    }
}

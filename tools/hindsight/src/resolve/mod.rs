//! Re-solve engine: run Fynd on a decoded swap's inputs and compare its output against what
//! actually settled on-chain.
//!
//! The `SteppingSolver` trait abstracts the solver so the two-state comparison pipeline is
//! testable without a live Fynd instance. The production implementation (`monitor`) drives an
//! in-process `fynd-core` solver one block at a time: each trade is solved at top-of-block (N-1),
//! then measured twice at back-of-block (N) — the top route is re-executed to isolate the
//! slippage between quote time and execution time, and the trade is solved fresh to show what
//! routing at the block's end state would deliver.

mod compare;
pub(crate) mod jsonl;
pub(crate) mod monitor;

use alloy::primitives::{Address, TxHash, U256};
use async_trait::async_trait;
pub(crate) use compare::{Deltas, Slippage, Verdict};
use fynd_core::types::Route;
use serde::Serialize;

use crate::{
    decoder::{AttributionSource, DecodedTrade, SandwichEvidence, SolverQuote},
    usd::Prices,
};

/// What Fynd's winning route was: the worker-pool algorithm that produced it and the path it took.
/// Kept as typed fields (not dug back out of `quote_json`) so the metrics and the per-trade log
/// line can read them without parsing JSON.
///
/// `Default` means "no route detail" — both fields empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct RouteSummary {
    /// Name of the algorithm whose route won the quote: `bellman_ford`, `most_liquid`,
    /// `path_frank_wolfe`, `water_fill`. Empty when the quote declared none.
    pub algorithm: String,
    /// The route as a readable path, tokens and protocols interleaved:
    /// `USDT -[uniswap_v2]-> DAI -[vm:balancer]-> WETH`. Split legs carry their share and are
    /// joined with ` + `. Empty when the quote carried no route. Logged and serialized under the
    /// name `route`.
    pub path: String,
}

/// A Fynd quote for the re-solved order.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SolvedAmount {
    pub amount_out: U256,
    /// Output after Fynd's own estimated gas cost.
    pub amount_out_net_gas: U256,
    pub gas_estimate: U256,
    /// Which algorithm won and the path its route took.
    pub route: RouteSummary,
    /// The complete serialized Fynd quote (route, per-hop pools/amounts, encoded transaction) for
    /// dumping improvements. `None` when not captured (e.g. the HTTP resolve path).
    #[serde(default)]
    pub quote_json: Option<String>,
    /// The solved route, kept in memory so [`SteppingSolver::reexecute`] can replay it at
    /// back-of-block. `None` for re-executed results and mocks. Not serialized (the slim
    /// projection in `quote_json` covers the JSONL) and excluded from equality (a route carries
    /// unserializable, incomparable protocol states). Boxed so a route-carrying `SolvedAmount`
    /// doesn't blow up `Outcome`'s size relative to its other variants.
    #[serde(skip)]
    pub solved_route: Option<Box<Route>>,
}

impl PartialEq for SolvedAmount {
    fn eq(&self, other: &Self) -> bool {
        self.amount_out == other.amount_out &&
            self.amount_out_net_gas == other.amount_out_net_gas &&
            self.gas_estimate == other.gas_estimate &&
            self.quote_json == other.quote_json
    }
}

impl Eq for SolvedAmount {}

/// The outcome of re-solving a trade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub(crate) enum Outcome {
    /// Fynd produced a quote for the trade's full size.
    Solved(SolvedAmount),
    /// Fynd returned a route but for far less than the settled size — a liquidity-limited partial
    /// route. Tracked apart from `Outcome::Unsolvable` so a coverage gap is not read as a loss.
    Partial(String),
    /// Fynd could not solve at all (missing token in Tycho, insufficient liquidity, timeout).
    Unsolvable(String),
}

/// Fynd's result at a single block state.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct StateResult {
    pub outcome: Outcome,
    pub deltas: Deltas,
    pub verdict: Verdict,
}

impl StateResult {
    fn new(outcome: Outcome, settled_amount_out: U256, settled_net_gas: U256) -> Self {
        let outcome = compare::served(outcome, settled_amount_out);
        let deltas = compare::compare(&outcome, settled_amount_out, settled_net_gas);
        let verdict = compare::verdict(&outcome, &deltas);
        Self { outcome, deltas, verdict }
    }
}

/// A trade re-solved at both block states, presented as a range.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RangeComparison {
    pub tx_hash: TxHash,
    pub block_number: u64,
    pub tx_index: u64,
    pub venue: String,
    pub solver: String,
    /// The evidence tier the solver label came from (from the decoder).
    pub solver_source: AttributionSource,
    /// Which decoder recovered the settled trade.
    pub decoder: &'static str,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub settled_amount_out: U256,
    /// Settled output after the gas the trader paid for the route, in `token_out` units. Equals
    /// `settled_amount_out` when that gas is unknown, was paid by someone else, or the output
    /// token is unpriced.
    pub settled_amount_out_net_gas: U256,
    /// Wei cost of the settled route's gas, when the trader paid it (from the decoder).
    pub settled_gas: Option<U256>,
    /// The solver's own off-chain quote from its calldata, when declared (from the decoder).
    pub quote: Option<SolverQuote>,
    /// Evidence that a front-run and a back-run bracketed this trade (from the decoder). `None`
    /// when no bracket pair was found.
    pub sandwich: Option<SandwichEvidence>,
    /// Optimistic: solved at state N-1, before the block's swaps moved the pools.
    pub top: StateResult,
    /// Pessimistic: solved fresh at state N, after the block's swaps moved the pools — what
    /// routing at the block's end state would deliver.
    pub back: StateResult,
    /// Headline verdict — top-of-block (the optimistic default).
    pub verdict: Verdict,
    /// Slippage of the top route between quote time (N-1) and re-execution (N). `None` when the
    /// top was unsolved or the re-execution failed.
    pub slippage: Option<Slippage>,
}

/// Solves a sell order at the current block state and steps to the next block. The production
/// implementation (`monitor`) drives an in-process `fynd-core` solver via
/// `fynd_core::BlockStepController`; tests use a mock returning a top- then back-of-block
/// outcome.
#[async_trait]
pub(crate) trait SteppingSolver {
    /// Solve a sell order at the solver's current block state.
    async fn solve(&self, token_in: Address, token_out: Address, amount_in: U256) -> Outcome;
    /// Release the held block and settle the solver onto the next block's state.
    async fn advance(&self) -> anyhow::Result<()>;
    /// Re-execute `top`'s route at the solver's current block state — same pools, splits, and
    /// input amount against the pools as the block left them.
    async fn reexecute(&self, top: &SolvedAmount) -> Outcome;
}

/// Build a `RangeComparison` from a trade's three outcomes: the top-of-block solve, the fresh
/// back-of-block solve, and the top route's re-execution at back-of-block (which feeds only the
/// `slippage` field).
///
/// When the decoder isolated the gas the trader paid for the settled route, its cost is converted
/// into `token_out` units at the `prices` snapshot (top-of-block — a fine approximation for a gas
/// deduction) and subtracted from the settled output, so both sides of the net comparison carry
/// their own gas.
///
/// When the decoder flagged the trade as sandwiched, each *solved* state's verdict becomes
/// `Verdict::Sandwiched`: its win or loss measures the MEV that moved the settled output, not
/// routing quality. Unsolved states keep their verdicts — a sandwich explains the settled price,
/// not why Fynd had no route, so the coverage buckets (`Unsolvable`, `CoverageMiss`) stay
/// intact. The bps/USD deltas are left untouched either way, so the size of MEV-inflated deltas
/// stays studyable offline.
pub(crate) fn build_range(
    trade: &DecodedTrade,
    prices: &Prices,
    top: Outcome,
    back: Outcome,
    reexecuted: &Outcome,
) -> RangeComparison {
    let settled_net_gas = trade
        .settled_gas
        .and_then(|gas| prices.gas_in_token(gas, trade.token_out))
        .map_or(trade.amount_out, |gas_out| trade.amount_out.saturating_sub(gas_out));
    // Computed from the raw outcomes: the coverage-miss reclassification below discards the
    // solved amounts the slippage is measured from.
    let slippage = compare::slippage(&top, reexecuted);
    let mut top = StateResult::new(top, trade.amount_out, settled_net_gas);
    let mut back = StateResult::new(back, trade.amount_out, settled_net_gas);
    if trade.sandwich.is_some() {
        for state in [&mut top, &mut back] {
            if let Outcome::Solved(_) = state.outcome {
                state.verdict = Verdict::Sandwiched;
            }
        }
    }
    let verdict = top.verdict;
    RangeComparison {
        tx_hash: trade.tx_hash,
        block_number: trade.block_number,
        tx_index: trade.tx_index,
        venue: trade.venue.clone(),
        solver: trade.solver.clone(),
        solver_source: trade.solver_source,
        decoder: trade.decoder,
        token_in: trade.token_in,
        token_out: trade.token_out,
        amount_in: trade.amount_in,
        settled_amount_out: trade.amount_out,
        settled_amount_out_net_gas: settled_net_gas,
        settled_gas: trade.settled_gas,
        quote: trade.quote.clone(),
        sandwich: trade.sandwich.clone(),
        top,
        back,
        verdict,
        slippage,
    }
}

/// Re-solve every trade in a held block at top-of-block, advance to back-of-block, then measure
/// each trade twice at the new state: re-execute its top route against the pools as the block
/// left them (for the slippage), and solve it fresh (for the `back` comparison). Solving all
/// trades at one state before advancing keeps each state's reads consistent and steps the chain
/// only once per block.
pub(crate) async fn resolve_block_range<S: SteppingSolver + ?Sized>(
    solver: &S,
    trades: &[DecodedTrade],
    prices: &Prices,
) -> anyhow::Result<Vec<RangeComparison>> {
    let mut tops = Vec::with_capacity(trades.len());
    for trade in trades {
        tops.push(
            solver
                .solve(trade.token_in, trade.token_out, trade.amount_in)
                .await,
        );
    }

    solver.advance().await?;

    let mut ranges = Vec::with_capacity(trades.len());
    for (trade, top) in trades.iter().zip(tops) {
        let reexecuted = match &top {
            Outcome::Solved(solved) => solver.reexecute(solved).await,
            Outcome::Partial(_) | Outcome::Unsolvable(_) => {
                Outcome::Unsolvable("no top-of-block route to re-execute".to_string())
            }
        };
        let back = solver
            .solve(trade.token_in, trade.token_out, trade.amount_in)
            .await;
        ranges.push(build_range(trade, prices, top, back, &reexecuted));
    }
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use alloy::primitives::TxHash;

    use super::*;
    use crate::decoder::Registry;

    fn empty_prices() -> Prices {
        Prices::new(&Registry::ethereum())
    }

    fn trade(settled: u64) -> DecodedTrade {
        DecodedTrade {
            tx_hash: TxHash::default(),
            block_number: 21_000_000,
            tx_index: 0,
            venue: "relay".into(),
            solver: "tycho".into(),
            solver_source: AttributionSource::TraceMatch,
            decoder: "sender-netting",
            sender: Address::ZERO,
            token_in: Address::repeat_byte(0x11),
            token_out: Address::repeat_byte(0x22),
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(settled),
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            quote: None,
            sandwich: None,
        }
    }

    fn solved(amount_out: u64, net: u64) -> Outcome {
        Outcome::Solved(SolvedAmount {
            amount_out: U256::from(amount_out),
            amount_out_net_gas: U256::from(net),
            gas_estimate: U256::from(21_000),
            route: RouteSummary::default(),
            quote_json: None,
            solved_route: None,
        })
    }

    /// Stepping mock: `solve` returns `top` before `advance()` and `back` after; `reexecute`
    /// returns `reexecuted` (the top route replayed at the new state).
    struct MockStepping {
        advanced: std::sync::atomic::AtomicBool,
        top: Outcome,
        back: Outcome,
        reexecuted: Outcome,
    }

    #[async_trait]
    impl SteppingSolver for MockStepping {
        async fn solve(&self, _: Address, _: Address, _: U256) -> Outcome {
            if self
                .advanced
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                self.back.clone()
            } else {
                self.top.clone()
            }
        }

        async fn advance(&self) -> anyhow::Result<()> {
            self.advanced
                .store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }

        async fn reexecute(&self, _: &SolvedAmount) -> Outcome {
            assert!(
                self.advanced
                    .load(std::sync::atomic::Ordering::Relaxed),
                "reexecute must only run after advance()"
            );
            self.reexecuted.clone()
        }
    }

    #[test]
    fn test_build_range_headline() {
        let range = build_range(
            &trade(10_000),
            &empty_prices(),
            solved(10_200, 10_100),
            solved(10_010, 9_990),
            &solved(10_010, 9_990),
        );
        assert_eq!(range.verdict, Verdict::Win); // top is the headline
        assert!(range.top.deltas.raw_bps.unwrap() > range.back.deltas.raw_bps.unwrap());
    }

    #[test]
    fn test_build_range_partial_fill() {
        // Fynd fills only 10% of a 10_000 settled trade → reclassified as a coverage miss.
        let range = build_range(
            &trade(10_000),
            &empty_prices(),
            solved(1_000, 990),
            solved(1_000, 990),
            &solved(1_000, 990),
        );
        assert_eq!(range.verdict, Verdict::CoverageMiss);
        assert_eq!(range.top.deltas, Deltas { raw_bps: None, net_bps: None });
        assert!(matches!(range.top.outcome, Outcome::Partial(_)));
    }

    #[test]
    fn test_build_range_sandwiched_trade() {
        let mut sandwiched = trade(10_000);
        sandwiched.sandwich = Some(SandwichEvidence {
            front_tx: TxHash::repeat_byte(0xaa),
            back_tx: TxHash::repeat_byte(0xbb),
            attacker: Address::repeat_byte(0xcc),
            pools: vec![Address::repeat_byte(0xdd)],
        });
        let range = build_range(
            &sandwiched,
            &empty_prices(),
            solved(10_200, 10_100),
            solved(9_800, 9_700),
            &solved(9_800, 9_700),
        );

        assert_eq!(range.verdict, Verdict::Sandwiched);
        assert_eq!(range.top.verdict, Verdict::Sandwiched);
        assert_eq!(range.back.verdict, Verdict::Sandwiched);
        // Deltas are unaffected by the override: still computed for offline analysis.
        assert!(range.top.deltas.raw_bps.unwrap() > 0.0);
        assert!(range.back.deltas.raw_bps.unwrap() < 0.0);
    }

    #[test]
    fn test_build_range_sandwiched_with_unsolved_states() {
        // The sandwich explains the settled price, not why Fynd had no route: an unsolved state
        // keeps its verdict so the coverage buckets are unaffected by the reclassification.
        let mut sandwiched = trade(10_000);
        sandwiched.sandwich = Some(SandwichEvidence {
            front_tx: TxHash::repeat_byte(0xaa),
            back_tx: TxHash::repeat_byte(0xbb),
            attacker: Address::repeat_byte(0xcc),
            pools: vec![Address::repeat_byte(0xdd)],
        });
        let range = build_range(
            &sandwiched,
            &empty_prices(),
            solved(10_200, 10_100),
            Outcome::Unsolvable("missing token in Tycho".into()),
            &Outcome::Unsolvable("re-execution failed".into()),
        );

        assert_eq!(range.top.verdict, Verdict::Sandwiched);
        assert_eq!(range.back.verdict, Verdict::Unsolvable);
        assert_eq!(range.verdict, Verdict::Sandwiched); // headline follows top
    }

    #[test]
    fn test_build_range_priced_gas() {
        // The settled trader paid 200 token_out units of gas (100 wei at a price of 2 units/wei):
        // the secondary net column carries the deduction; the verdict stays gross vs gross.
        let mut with_gas = trade(10_000);
        with_gas.settled_gas = Some(U256::from(100u64));
        let mut prices = empty_prices();
        prices.insert(with_gas.token_out, 2.0);

        let range = build_range(
            &with_gas,
            &prices,
            solved(10_050, 9_990),
            solved(10_050, 9_990),
            &solved(10_050, 9_990),
        );
        assert_eq!(range.settled_amount_out_net_gas, U256::from(9_800u64));
        assert_eq!(range.settled_amount_out, U256::from(10_000u64));
        assert_eq!(range.verdict, Verdict::Win);
    }

    #[test]
    fn test_build_range_unpriced_gas() {
        // token_out is not in the price map → no deduction. The secondary net column stays
        // gross; the verdict is unaffected either way (gross 10_050 beats gross 10_000).
        let mut with_gas = trade(10_000);
        with_gas.settled_gas = Some(U256::from(100u64));

        let range = build_range(
            &with_gas,
            &empty_prices(),
            solved(10_050, 9_990),
            solved(10_050, 9_990),
            &solved(10_050, 9_990),
        );
        assert_eq!(range.settled_amount_out_net_gas, U256::from(10_000u64));
        assert_eq!(range.verdict, Verdict::Win);
    }

    #[tokio::test]
    async fn resolve_block_range_pairs_top_back_and_reexecution() {
        // Two trades. The top solve wins; the fresh back solve loses vs settled; the top route
        // re-executed at back-of-block produces less than quoted (negative slippage).
        let solver = MockStepping {
            advanced: std::sync::atomic::AtomicBool::new(false),
            top: solved(10_200, 10_100),
            back: solved(9_950, 9_850),
            reexecuted: solved(9_900, 9_800),
        };
        let trades = [trade(10_000), trade(10_000)];
        let ranges = resolve_block_range(&solver, &trades, &empty_prices())
            .await
            .unwrap();

        assert_eq!(ranges.len(), 2);
        for range in &ranges {
            assert_eq!(range.top.verdict, Verdict::Win);
            assert_eq!(range.back.verdict, Verdict::Loss);
            assert!(range.top.deltas.raw_bps.unwrap() > range.back.deltas.raw_bps.unwrap());
            let slippage = range.slippage.unwrap();
            assert!(slippage.bps < 0.0, "re-execution below quote must be negative slippage");
            assert_eq!(slippage.quoted_amount_out, U256::from(10_200u64));
            assert_eq!(slippage.reexecuted_amount_out, U256::from(9_900u64));
        }
    }

    #[tokio::test]
    async fn resolve_block_range_back_solve_without_top_route() {
        // An unsolved top has no route to re-execute, so there is no slippage — but the fresh
        // back-of-block solve does not need a top route: back still carries a real comparison.
        let solver = MockStepping {
            advanced: std::sync::atomic::AtomicBool::new(false),
            top: Outcome::Unsolvable("missing token in Tycho".into()),
            back: solved(10_100, 10_000),
            reexecuted: solved(10_100, 10_000),
        };
        let trades = [trade(10_000)];
        let ranges = resolve_block_range(&solver, &trades, &empty_prices())
            .await
            .unwrap();

        assert_eq!(ranges[0].top.verdict, Verdict::Unsolvable);
        assert_eq!(ranges[0].back.verdict, Verdict::Win);
        assert_eq!(ranges[0].slippage, None);
    }

    #[test]
    fn build_range_positive_slippage_from_raw_outcomes() {
        // The route re-executed to more than quoted: the surplus we could charge. The fresh
        // back solve is a different route and plays no part in the slippage.
        let range = build_range(
            &trade(10_000),
            &empty_prices(),
            solved(10_000, 9_900),
            solved(10_500, 10_400),
            &solved(10_050, 9_950),
        );
        let slippage = range.slippage.unwrap();
        assert!((slippage.bps - 50.0).abs() < 0.01, "expected +50 bps, got {}", slippage.bps);
    }

    #[test]
    fn build_range_slippage_survives_unsolved_back() {
        // The fresh back solve failed (e.g. the pair lost its route at state N), but the top
        // route still re-executed: the slippage must survive independently of `back`.
        let range = build_range(
            &trade(10_000),
            &empty_prices(),
            solved(10_000, 9_900),
            Outcome::Unsolvable("no route at back-of-block".into()),
            &solved(10_050, 9_950),
        );
        assert_eq!(range.back.verdict, Verdict::Unsolvable);
        let slippage = range.slippage.unwrap();
        assert!((slippage.bps - 50.0).abs() < 0.01, "expected +50 bps, got {}", slippage.bps);
    }

    #[test]
    fn build_range_slippage_survives_coverage_miss_reclassification() {
        // Both states cover only 10% of the settled size and are reclassified as coverage
        // misses (losing their solved amounts) — the slippage between the top quote and its
        // re-execution must still be measured from the raw outcomes.
        let range = build_range(
            &trade(10_000),
            &empty_prices(),
            solved(1_000, 990),
            solved(1_010, 1_000),
            &solved(1_010, 1_000),
        );
        assert_eq!(range.verdict, Verdict::CoverageMiss);
        let slippage = range.slippage.unwrap();
        assert!((slippage.bps - 100.0).abs() < 0.01, "expected +100 bps, got {}", slippage.bps);
    }
}

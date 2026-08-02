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

use std::collections::HashMap;

use alloy::primitives::{Address, TxHash, U256};
use async_trait::async_trait;
pub(crate) use compare::{Deltas, Slippage, Verdict};
use fynd_core::types::{Route, Swap};
use serde::Serialize;
use tycho_simulation::tycho_common::models::Address as CoreAddress;

use crate::{
    decoder::{AttributionSource, DecodedTrade, SandwichEvidence, TradeStatus},
    usd::Prices,
};

/// One route leg, reduced to what rendering needs. A `Swap` also carries a `ProtocolComponent`
/// and a boxed `ProtocolSim` that a route string has no use for and that cannot be built outside
/// `fynd-core`, so the rendering below works on this instead and stays unit-testable.
struct Leg<'a> {
    token_in: &'a CoreAddress,
    token_out: &'a CoreAddress,
    protocol: &'a str,
    /// The leg's declared share of `token_in`. `0.0` means "all the remaining balance".
    split: f64,
}

impl<'a> Leg<'a> {
    fn from_swap(swap: &'a Swap) -> Self {
        Self {
            token_in: swap.token_in(),
            token_out: swap.token_out(),
            protocol: swap.protocol(),
            split: *swap.split(),
        }
    }
}

/// Render a solved route as a readable path: `USDT -[uniswap_v2]-> DAI -[vm:balancer]-> WETH`.
/// Token symbols are resolved from the route's own token map (populated by the algorithm that
/// built it, via [`fynd_core::types::Route::token_symbol`]); a token missing from that map falls
/// back to a shortened address.
pub(crate) fn render_route(route: &Route) -> String {
    let legs: Vec<Leg<'_>> = Route::swaps(route)
        .iter()
        .map(Leg::from_swap)
        .collect();
    render_legs(&legs, &route_symbols(route))
}

/// Symbols for every token in `route`, resolved from the route's own token map.
fn route_symbols(route: &Route) -> HashMap<CoreAddress, String> {
    let mut symbols = HashMap::new();
    for swap in Route::swaps(route) {
        for token in [swap.token_in(), swap.token_out()] {
            if let Some(symbol) = route.token_symbol(token) {
                symbols.insert(token.clone(), symbol.to_string());
            }
        }
    }
    symbols
}

/// Render a route's legs as a readable path, given each token's symbol. Kept apart from
/// [`render_route`] so the algorithm is unit-testable against plain `Leg`s and a symbol map,
/// without building a real `Route`/`Swap`.
///
/// Swaps that connect — one's output token is the next's input — chain into a single arrow path.
/// A split fans several legs out of the same token, so its legs cannot share one chain: each
/// becomes its own path carrying its share of the input, and the paths are joined with ` + `.
/// Protocol ids are Tycho's own, so a newly integrated DEX reads correctly without a lookup table
/// here.
fn render_legs(legs: &[Leg<'_>], symbols: &HashMap<CoreAddress, String>) -> String {
    let mut paths: Vec<String> = Vec::new();
    let mut open = String::new();
    let mut tip: Option<&CoreAddress> = None;
    for (leg, share) in legs.iter().zip(split_shares(legs)) {
        if tip != Some(leg.token_in) {
            if !open.is_empty() {
                paths.push(std::mem::take(&mut open));
            }
            open = token_label(leg.token_in, symbols);
        }
        open.push_str(&arrow(leg.protocol, share));
        open.push_str(&token_label(leg.token_out, symbols));
        tip = Some(leg.token_out);
    }
    if !open.is_empty() {
        paths.push(open);
    }
    paths.join(" + ")
}

/// The arrow between two tokens: ` -[uniswap_v2]-> `, or ` -[vm:curve 40%]-> ` for one leg of a
/// split, where the share tells you how much of the input took this leg.
fn arrow(protocol: &str, share: Option<f64>) -> String {
    match share {
        Some(share) => format!(" -[{protocol} {:.0}%]-> ", share * 100.0),
        None => format!(" -[{protocol}]-> "),
    }
}

/// Each leg's share of its input token, or `None` when it is the only leg consuming that token.
///
/// A split's legs all consume the same token, and by `Route`'s split convention every leg but one
/// declares an explicit fraction while the last declares `0.0`, meaning "all the remaining
/// balance". Reconstruct that remainder so every leg of a split reads as a percentage.
fn split_shares(legs: &[Leg<'_>]) -> Vec<Option<f64>> {
    let mut consumers: HashMap<&CoreAddress, (usize, f64)> = HashMap::new();
    for leg in legs {
        let entry = consumers
            .entry(leg.token_in)
            .or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += leg.split;
    }
    legs.iter()
        .map(|leg| {
            let (count, declared) = consumers
                .get(leg.token_in)
                .copied()
                .unwrap_or((1, 0.0));
            if count < 2 {
                return None;
            }
            Some(if leg.split == 0.0 { 1.0 - declared } else { leg.split })
        })
        .collect()
}

/// A token's symbol, or a shortened address when the route has no entry for it — an unknown token
/// still has to read as a distinct hop rather than vanish from the path.
fn token_label(token: &CoreAddress, symbols: &HashMap<CoreAddress, String>) -> String {
    if let Some(symbol) = symbols.get(token) {
        return symbol.clone();
    }
    let hex = token.to_string();
    match hex.get(..8) {
        Some(prefix) => format!("{prefix}…"),
        None => hex,
    }
}

/// A Fynd quote for the re-solved order.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SolvedAmount {
    pub amount_out: U256,
    /// Output after Fynd's own estimated gas cost.
    pub amount_out_net_gas: U256,
    pub gas_estimate: U256,
    /// Name of the algorithm whose route won the quote: `bellman_ford`, `most_liquid`,
    /// `path_frank_wolfe`, `water_fill`. Empty when the quote declared none, or when this is a
    /// re-executed outcome (it only feeds the slippage numbers and does not re-declare a route).
    pub algorithm: String,
    /// The complete serialized Fynd quote (route, per-hop pools/amounts, encoded transaction) for
    /// dumping improvements. `None` when not captured (e.g. the HTTP resolve path).
    #[serde(default)]
    pub quote_json: Option<String>,
    /// The solved route, kept in memory so [`SteppingSolver::reexecute`] can replay it at
    /// back-of-block and so [`render_route`] can derive the readable path at serialization time
    /// (serialized under `route`, alongside `algorithm`). `None` for re-executed results and
    /// mocks. Not serialized directly (the derived path and the slim projection in `quote_json`
    /// cover the JSONL) and excluded from equality (a route carries unserializable, incomparable
    /// protocol states). Boxed so a route-carrying `SolvedAmount` doesn't blow up `Outcome`'s size
    /// relative to its other variants.
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
    /// Fynd vs the settled output. `Deltas::default()` (all `None`) when nothing settled to
    /// compare against — a reverted trade.
    pub deltas: Deltas,
    pub verdict: Verdict,
    /// Whether the quote cleared the trade's on-chain floor (`min_amount_out`), when one is
    /// known. Computed for both settled and reverted trades — a settled trade cleared its floor
    /// by construction, but the margin is still informative. `None` when the state was not
    /// solved or no floor is known.
    pub fillable: Option<bool>,
    pub margin_bps: Option<f64>,
}

impl StateResult {
    /// `settled` is `Some((gross, net_gas))` for a settled trade, `None` for a reverted one —
    /// there is nothing settled to compare gross output against, so `deltas`/`verdict` fall back
    /// to their unsolved-comparison values (`Deltas::default()`, `Verdict::Unsolvable`) even when
    /// `outcome` itself solved; `fillable`/`margin_bps` are unaffected, since they judge the
    /// outcome against `min_amount_out`, not against a settled amount.
    fn new(outcome: Outcome, settled: Option<(U256, U256)>, min_amount_out: Option<U256>) -> Self {
        let outcome = match settled {
            Some((gross, _)) => compare::served(outcome, gross),
            None => outcome,
        };
        let deltas = match settled {
            Some((gross, net)) => compare::compare(&outcome, gross, net),
            None => Deltas::default(),
        };
        let verdict = compare::verdict(&outcome, &deltas);
        let (fillable, margin_bps) = compare::floor_judgment(&outcome, min_amount_out);
        Self { outcome, deltas, verdict, fillable, margin_bps }
    }
}

/// A trade re-solved at both block states, presented as a range — settled or reverted, told
/// apart by `status`. A reverted trade's settled-only fields (`settled_amount_out` and friends)
/// are simply absent; everything else about the record has the same shape.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RangeComparison {
    pub tx_hash: TxHash,
    pub block_number: u64,
    pub tx_index: u64,
    #[serde(flatten)]
    pub status: TradeStatus,
    pub venue: String,
    pub solver: String,
    /// The evidence tier the solver label came from (from the decoder).
    pub solver_source: AttributionSource,
    /// Which decoder recovered the trade.
    pub decoder: &'static str,
    /// The trader, from the decoder: the netted flow's tracked party for a settled trade, or the
    /// transaction sender for a reverted one (there is no netted flow to draw a different party
    /// from). Lets a venue-filler fill (e.g. Relay's rotating filler) be segmented by its actual
    /// trader without an on-chain lookup.
    pub sender: Address,
    /// `None` only for a reverted trade whose solver frame's calldata did not parse.
    pub token_in: Option<Address>,
    pub token_out: Option<Address>,
    pub amount_in: Option<U256>,
    /// `None` for a reverted trade: nothing was delivered.
    pub settled_amount_out: Option<U256>,
    /// Settled output after the gas the trader paid for the route, in `token_out` units. Equals
    /// `settled_amount_out` when that gas is unknown, was paid by someone else, or the output
    /// token is unpriced. `None` for a reverted trade.
    pub settled_amount_out_net_gas: Option<U256>,
    /// Wei cost of the settled route's gas, when the trader paid it (from the decoder).
    pub settled_gas: Option<U256>,
    /// The on-chain enforced floor declared in the settling solver frame's own calldata (from
    /// the decoder).
    pub min_amount_out: Option<U256>,
    /// The solver's own off-chain quote, when its calldata declares one (from the decoder;
    /// unit-checked against the settled amount).
    pub declared_quote: Option<U256>,
    /// Unix timestamp of `declared_quote`, when the calldata carries one.
    pub quote_timestamp: Option<u64>,
    /// Evidence that a front-run and a back-run bracketed this trade (from the decoder). `None`
    /// when no bracket pair was found, or the trade reverted.
    pub sandwich: Option<SandwichEvidence>,
    /// Optimistic: solved at state N-1, before the block's swaps moved the pools. `None` when
    /// the trade's terms were unknown — there was nothing to solve.
    pub top: Option<StateResult>,
    /// Pessimistic: solved fresh at state N, after the block's swaps moved the pools — what
    /// routing at the block's end state would deliver. `None` alongside `top`.
    pub back: Option<StateResult>,
    /// Headline verdict — top-of-block (the optimistic default). `None` alongside `top`.
    pub verdict: Option<Verdict>,
    /// Slippage of the top route between quote time (N-1) and re-execution (N). `None` when the
    /// top was unsolved, the re-execution failed, or the trade's terms were unknown.
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

/// Build a `RangeComparison` from a trade and, when its terms were known, the three outcomes
/// solving it produced: the top-of-block solve, the fresh back-of-block solve, and the top
/// route's re-execution at back-of-block (which feeds only the `slippage` field). `None` when the
/// trade's terms were unknown — there was nothing to solve, so `top`/`back`/`verdict`/`slippage`
/// on the returned comparison are all `None` too, but the trade is still recorded.
///
/// When the decoder isolated the gas the trader paid for the settled route, its cost is converted
/// into `token_out` units at the `prices` snapshot (top-of-block — a fine approximation for a gas
/// deduction) and subtracted from the settled output, so both sides of the net comparison carry
/// their own gas. Reverted trades have no settled output to deduct from — `settled` is `None`.
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
    solved: Option<(Outcome, Outcome, Outcome)>,
) -> RangeComparison {
    let settled = trade.amount_out.map(|gross| {
        let net_gas = trade
            .settled_gas
            .and_then(|gas| {
                trade
                    .token_out
                    .and_then(|token| prices.gas_in_token(gas, token))
            })
            .map_or(gross, |gas_out| gross.saturating_sub(gas_out));
        (gross, net_gas)
    });

    let (top, back, verdict, slippage) = match solved {
        Some((top_outcome, back_outcome, reexecuted)) => {
            // Computed from the raw outcomes: the coverage-miss reclassification below discards
            // the solved amounts the slippage is measured from.
            let slippage = compare::slippage(&top_outcome, &reexecuted);
            let mut top = StateResult::new(top_outcome, settled, trade.min_amount_out);
            let mut back = StateResult::new(back_outcome, settled, trade.min_amount_out);
            if trade.sandwich.is_some() {
                for state in [&mut top, &mut back] {
                    if let Outcome::Solved(_) = state.outcome {
                        state.verdict = Verdict::Sandwiched;
                    }
                }
            }
            let verdict = top.verdict;
            (Some(top), Some(back), Some(verdict), slippage)
        }
        None => (None, None, None, None),
    };

    RangeComparison {
        tx_hash: trade.tx_hash,
        block_number: trade.block_number,
        tx_index: trade.tx_index,
        status: trade.status.clone(),
        venue: trade.venue.clone(),
        solver: trade.solver.clone(),
        solver_source: trade.solver_source,
        decoder: trade.decoder,
        sender: trade.sender,
        token_in: trade.token_in,
        token_out: trade.token_out,
        amount_in: trade.amount_in,
        settled_amount_out: settled.map(|(gross, _)| gross),
        settled_amount_out_net_gas: settled.map(|(_, net)| net),
        settled_gas: trade.settled_gas,
        min_amount_out: trade.min_amount_out,
        declared_quote: trade.declared_quote,
        quote_timestamp: trade.quote_timestamp,
        sandwich: trade.sandwich.clone(),
        top,
        back,
        verdict,
        slippage,
    }
}

/// Re-solve every trade in a held block at top-of-block, advance to back-of-block, then measure
/// each again at the new state: its top route is re-executed against the pools as the block left
/// them (for the slippage) and it is solved fresh (for the `back` comparison). Solving everything
/// at one state before advancing keeps each state's reads consistent and steps the chain only
/// once per block. Settled and reverted trades go through the same wave — a reverted trade whose
/// solver calldata parsed into a `SwapIntent` is solved exactly like a settled one, judged against
/// its `min_amount_out` floor instead of a settled amount.
///
/// A trade with unknown terms (a reverted trade whose solver calldata did not parse) is recorded
/// but not solved — there is nothing to feed the solver.
pub(crate) async fn resolve_block_range<S: SteppingSolver + ?Sized>(
    solver: &S,
    trades: &[DecodedTrade],
    prices: &Prices,
) -> anyhow::Result<Vec<RangeComparison>> {
    let mut tops = Vec::with_capacity(trades.len());
    for trade in trades {
        let top = match trade.terms() {
            Some((token_in, token_out, amount_in)) => Some(
                solver
                    .solve(token_in, token_out, amount_in)
                    .await,
            ),
            None => None,
        };
        tops.push(top);
    }

    solver.advance().await?;

    let mut ranges = Vec::with_capacity(trades.len());
    for (trade, top) in trades.iter().zip(tops) {
        let Some(top) = top else {
            ranges.push(build_range(trade, prices, None));
            continue;
        };
        let reexecuted = match &top {
            Outcome::Solved(solved) => solver.reexecute(solved).await,
            Outcome::Partial(_) | Outcome::Unsolvable(_) => {
                Outcome::Unsolvable("no top-of-block route to re-execute".to_string())
            }
        };
        // `top` was solved from `trade.terms()`, so the terms are known here too.
        let (token_in, token_out, amount_in) = trade
            .terms()
            .expect("a solved top implies known terms");
        let back = solver
            .solve(token_in, token_out, amount_in)
            .await;
        ranges.push(build_range(trade, prices, Some((top, back, reexecuted))));
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
            status: TradeStatus::Settled,
            venue: "relay".into(),
            solver: "tycho".into(),
            solver_source: AttributionSource::TraceMatch,
            decoder: "sender-netting",
            sender: Address::ZERO,
            token_in: Some(Address::repeat_byte(0x11)),
            token_out: Some(Address::repeat_byte(0x22)),
            amount_in: Some(U256::from(1_000u64)),
            amount_out: Some(U256::from(settled)),
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            min_amount_out: None,
            declared_quote: None,
            quote_timestamp: None,
            sandwich: None,
        }
    }

    /// A reverted trade whose solver calldata parsed into a `SwapIntent` — carries terms and a
    /// floor (`min_amount_out`) but no settled output.
    fn reverted_trade(cause: crate::decoder::RevertCause, min_amount_out: u64) -> DecodedTrade {
        DecodedTrade {
            tx_hash: TxHash::repeat_byte(0x02),
            block_number: 21_000_000,
            tx_index: 1,
            status: TradeStatus::Reverted { cause },
            venue: "relay".into(),
            solver: "fly".into(),
            solver_source: AttributionSource::TraceMatch,
            decoder: "reverted",
            sender: Address::ZERO,
            token_in: Some(Address::repeat_byte(0x11)),
            token_out: Some(Address::repeat_byte(0x22)),
            amount_in: Some(U256::from(1_000u64)),
            amount_out: None,
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            min_amount_out: Some(U256::from(min_amount_out)),
            declared_quote: None,
            quote_timestamp: None,
            sandwich: None,
        }
    }

    /// A reverted trade whose solver calldata did not parse — no terms, nothing to solve.
    fn reverted_trade_without_terms() -> DecodedTrade {
        DecodedTrade {
            tx_hash: TxHash::repeat_byte(0x03),
            block_number: 21_000_000,
            tx_index: 2,
            status: TradeStatus::Reverted {
                cause: crate::decoder::RevertCause::Other("unknown revert".to_string()),
            },
            venue: "relay".into(),
            solver: "relay".into(),
            solver_source: AttributionSource::Fallback,
            decoder: "reverted",
            sender: Address::ZERO,
            token_in: None,
            token_out: None,
            amount_in: None,
            amount_out: None,
            venue_fee_in: None,
            venue_fee_out: None,
            settled_gas: None,
            min_amount_out: None,
            declared_quote: None,
            quote_timestamp: None,
            sandwich: None,
        }
    }

    fn solved(amount_out: u64, net: u64) -> Outcome {
        Outcome::Solved(SolvedAmount {
            amount_out: U256::from(amount_out),
            amount_out_net_gas: U256::from(net),
            gas_estimate: U256::from(21_000),
            algorithm: String::new(),
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
            Some((solved(10_200, 10_100), solved(10_010, 9_990), solved(10_010, 9_990))),
        );
        assert_eq!(range.verdict, Some(Verdict::Win)); // top is the headline
        let top = range.top.unwrap();
        let back = range.back.unwrap();
        assert!(top.deltas.raw_bps.unwrap() > back.deltas.raw_bps.unwrap());
    }

    #[test]
    fn test_build_range_partial_fill() {
        // Fynd fills only 10% of a 10_000 settled trade → reclassified as a coverage miss.
        let range = build_range(
            &trade(10_000),
            &empty_prices(),
            Some((solved(1_000, 990), solved(1_000, 990), solved(1_000, 990))),
        );
        assert_eq!(range.verdict, Some(Verdict::CoverageMiss));
        let top = range.top.unwrap();
        assert_eq!(top.deltas, Deltas { raw_bps: None, net_bps: None });
        assert!(matches!(top.outcome, Outcome::Partial(_)));
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
            Some((solved(10_200, 10_100), solved(9_800, 9_700), solved(9_800, 9_700))),
        );

        assert_eq!(range.verdict, Some(Verdict::Sandwiched));
        let top = range.top.unwrap();
        let back = range.back.unwrap();
        assert_eq!(top.verdict, Verdict::Sandwiched);
        assert_eq!(back.verdict, Verdict::Sandwiched);
        // Deltas are unaffected by the override: still computed for offline analysis.
        assert!(top.deltas.raw_bps.unwrap() > 0.0);
        assert!(back.deltas.raw_bps.unwrap() < 0.0);
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
            Some((
                solved(10_200, 10_100),
                Outcome::Unsolvable("missing token in Tycho".into()),
                Outcome::Unsolvable("re-execution failed".into()),
            )),
        );

        assert_eq!(range.top.unwrap().verdict, Verdict::Sandwiched);
        assert_eq!(range.back.unwrap().verdict, Verdict::Unsolvable);
        assert_eq!(range.verdict, Some(Verdict::Sandwiched)); // headline follows top
    }

    #[test]
    fn test_build_range_priced_gas() {
        // The settled trader paid 200 token_out units of gas (100 wei at a price of 2 units/wei):
        // the secondary net column carries the deduction; the verdict stays gross vs gross.
        let mut with_gas = trade(10_000);
        with_gas.settled_gas = Some(U256::from(100u64));
        let mut prices = empty_prices();
        prices.insert(with_gas.token_out.unwrap(), 2.0);

        let range = build_range(
            &with_gas,
            &prices,
            Some((solved(10_050, 9_990), solved(10_050, 9_990), solved(10_050, 9_990))),
        );
        assert_eq!(range.settled_amount_out_net_gas, Some(U256::from(9_800u64)));
        assert_eq!(range.settled_amount_out, Some(U256::from(10_000u64)));
        assert_eq!(range.verdict, Some(Verdict::Win));
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
            Some((solved(10_050, 9_990), solved(10_050, 9_990), solved(10_050, 9_990))),
        );
        assert_eq!(range.settled_amount_out_net_gas, Some(U256::from(10_000u64)));
        assert_eq!(range.verdict, Some(Verdict::Win));
    }

    #[test]
    fn test_build_range_unknown_terms_records_without_solving() {
        // A reverted trade whose solver calldata did not parse: nothing to solve, but it is
        // still recorded, tagged by its cause.
        let range = build_range(&reverted_trade_without_terms(), &empty_prices(), None);
        assert!(matches!(range.status, TradeStatus::Reverted { .. }));
        assert!(range.token_in.is_none());
        assert!(range.top.is_none());
        assert!(range.back.is_none());
        assert_eq!(range.verdict, None);
        assert_eq!(range.slippage, None);
    }

    #[test]
    fn test_build_range_reverted_trade_judged_against_floor() {
        // A reverted trade with known terms and a floor: no settled amount to compare against
        // (deltas/verdict stay at their unsolved defaults), but fillable/margin_bps still judge
        // the quote against min_amount_out.
        let range = build_range(
            &reverted_trade(crate::decoder::RevertCause::SlippageFloor, 10_000),
            &empty_prices(),
            Some((solved(9_800, 9_700), solved(10_100, 10_000), solved(10_100, 10_000))),
        );
        assert!(range.settled_amount_out.is_none());
        let top = range.top.unwrap();
        let back = range.back.unwrap();
        assert_eq!(top.deltas, Deltas { raw_bps: None, net_bps: None });
        assert_eq!(top.fillable, Some(false));
        assert_eq!(back.fillable, Some(true));
        assert!(back.margin_bps.unwrap() > 0.0);
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
            let top = range.top.as_ref().unwrap();
            let back = range.back.as_ref().unwrap();
            assert_eq!(top.verdict, Verdict::Win);
            assert_eq!(back.verdict, Verdict::Loss);
            assert!(top.deltas.raw_bps.unwrap() > back.deltas.raw_bps.unwrap());
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

        assert_eq!(ranges[0].top.as_ref().unwrap().verdict, Verdict::Unsolvable);
        assert_eq!(ranges[0].back.as_ref().unwrap().verdict, Verdict::Win);
        assert_eq!(ranges[0].slippage, None);
    }

    #[tokio::test]
    async fn resolve_block_range_solves_reverts_and_skips_unknown_terms() {
        // The floor is 10_000: unfillable at top, fillable at back — the shape a sequencer that
        // fills against fresher state would avoid. The intent-less revert has no terms, so it is
        // recorded but never reaches the solver.
        let solver = MockStepping {
            advanced: std::sync::atomic::AtomicBool::new(false),
            top: solved(9_800, 9_700),
            back: solved(10_100, 10_000),
            reexecuted: solved(10_100, 10_000),
        };
        let trades = [
            reverted_trade(crate::decoder::RevertCause::SlippageFloor, 10_000),
            reverted_trade_without_terms(),
        ];
        let ranges = resolve_block_range(&solver, &trades, &empty_prices())
            .await
            .unwrap();

        assert_eq!(ranges.len(), 2);
        assert!(ranges[1].top.is_none(), "the terms-less revert must not be solved");

        let comparison = &ranges[0];
        assert_eq!(comparison.min_amount_out, Some(U256::from(10_000u64)));
        assert_eq!(
            comparison
                .top
                .as_ref()
                .unwrap()
                .fillable,
            Some(false)
        );
        let back = comparison.back.as_ref().unwrap();
        assert_eq!(back.fillable, Some(true));
        assert!(back.margin_bps.unwrap() > 0.0);
    }

    #[test]
    fn build_range_positive_slippage_from_raw_outcomes() {
        // The route re-executed to more than quoted: the surplus we could charge. The fresh
        // back solve is a different route and plays no part in the slippage.
        let range = build_range(
            &trade(10_000),
            &empty_prices(),
            Some((solved(10_000, 9_900), solved(10_500, 10_400), solved(10_050, 9_950))),
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
            Some((
                solved(10_000, 9_900),
                Outcome::Unsolvable("no route at back-of-block".into()),
                solved(10_050, 9_950),
            )),
        );
        assert_eq!(range.back.unwrap().verdict, Verdict::Unsolvable);
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
            Some((solved(1_000, 990), solved(1_010, 1_000), solved(1_010, 1_000))),
        );
        assert_eq!(range.verdict, Some(Verdict::CoverageMiss));
        let slippage = range.slippage.unwrap();
        assert!((slippage.bps - 100.0).abs() < 0.01, "expected +100 bps, got {}", slippage.bps);
    }

    /// A token address keyed by its symbol's last byte — distinct across the symbols used below,
    /// where the first byte is not (USDT and USDC share it) — so `symbols()` can name it back.
    fn token(symbol: &str) -> CoreAddress {
        let tag = symbol
            .as_bytes()
            .last()
            .copied()
            .unwrap_or(0);
        CoreAddress::from(vec![tag; 20])
    }

    /// A symbol table for the symbols used below.
    fn symbols() -> HashMap<CoreAddress, String> {
        ["USDT", "DAI", "WETH", "USDC"]
            .into_iter()
            .map(|symbol| (token(symbol), symbol.to_string()))
            .collect()
    }

    /// One route leg. `split` follows `Route`'s convention: an explicit fraction, or 0.0 for the
    /// leg that takes whatever is left.
    fn leg<'a>(
        token_in: &'a CoreAddress,
        token_out: &'a CoreAddress,
        protocol: &'a str,
        split: f64,
    ) -> Leg<'a> {
        Leg { token_in, token_out, protocol, split }
    }

    #[test]
    fn test_render_legs_multi_hop() {
        // Connecting legs chain into one arrow path; no percentages, since nothing is split.
        let (usdt, dai, weth) = (token("USDT"), token("DAI"), token("WETH"));
        let route = render_legs(
            &[leg(&usdt, &dai, "uniswap_v2", 0.0), leg(&dai, &weth, "vm:balancer", 0.0)],
            &symbols(),
        );
        assert_eq!(route, "USDT -[uniswap_v2]-> DAI -[vm:balancer]-> WETH");
    }

    #[test]
    fn test_render_legs_split() {
        // Two legs out of USDC: the 0.6 leg is explicit, the 0.0 leg takes the remainder. They fan
        // out of the same token so they cannot share one chain — each becomes its own path, joined
        // by " + ", and both carry their share.
        let (usdc, weth) = (token("USDC"), token("WETH"));
        let route = render_legs(
            &[leg(&usdc, &weth, "uniswap_v3", 0.6), leg(&usdc, &weth, "vm:curve", 0.0)],
            &symbols(),
        );
        assert_eq!(route, "USDC -[uniswap_v3 60%]-> WETH + USDC -[vm:curve 40%]-> WETH");
    }

    #[test]
    fn test_render_legs_split_then_common_hop() {
        // A split that reconverges: both legs land on WETH, then one leg carries on to USDC. The
        // continuation chains onto the path that ended at WETH rather than starting a third path.
        let (usdt, dai, weth, usdc) = (token("USDT"), token("DAI"), token("WETH"), token("USDC"));
        let route = render_legs(
            &[
                leg(&usdt, &weth, "uniswap_v3", 0.25),
                leg(&usdt, &dai, "vm:curve", 0.0),
                leg(&dai, &usdc, "uniswap_v2", 0.0),
            ],
            &symbols(),
        );
        assert_eq!(
            route,
            "USDT -[uniswap_v3 25%]-> WETH + USDT -[vm:curve 75%]-> DAI -[uniswap_v2]-> USDC"
        );
    }

    #[test]
    fn test_render_legs_unknown_token() {
        // A long-tail token with no entry in the symbol table still has to read as a distinct
        // hop, so it falls back to a shortened address rather than an empty string.
        let (usdt, unknown) = (token("USDT"), CoreAddress::from(vec![0xab; 20]));
        assert_eq!(
            render_legs(&[leg(&usdt, &unknown, "uniswap_v2", 0.0)], &symbols()),
            "USDT -[uniswap_v2]-> 0xababab…"
        );
    }

    #[test]
    fn test_render_legs_empty() {
        assert_eq!(render_legs(&[], &symbols()), "");
    }

    #[test]
    fn test_render_route_resolves_symbols_from_the_route_itself() {
        // Unlike `render_legs` above (fed a hand-built symbol table), this exercises the glue
        // that reads symbols directly off a real `Route`'s own token map.
        let route =
            test_support::route(&[("uniswap_v2", "USDT", "DAI"), ("vm:balancer", "DAI", "WETH")]);
        assert_eq!(render_route(&route), "USDT -[uniswap_v2]-> DAI -[vm:balancer]-> WETH");
    }
}

/// Test-only route construction, shared by tests here and in sibling modules (`jsonl`) that need
/// a real `Route` with resolvable token symbols — as opposed to the pure-rendering tests above,
/// which build `Leg`s directly to stay decoupled from `Swap`'s heavier constructor.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;

    use alloy::primitives::U256;
    use chrono::NaiveDateTime;
    use fynd_core::types::{Route, Swap};
    use num_bigint::BigUint;
    use tycho_simulation::{
        evm::protocol::uniswap_v2::state::UniswapV2State,
        tycho_common::{
            models::{protocol::ProtocolComponent, token::Token, Chain, ChangeType},
            Bytes,
        },
    };

    /// Build a route from `(protocol, token_in_symbol, token_out_symbol)` hops, chained in order.
    /// Each hop gets a throwaway `UniswapV2State` pool — real enough to satisfy `Swap::new`,
    /// irrelevant to what the route-summary code reads (protocol id, token addresses, and the
    /// route's own token map).
    pub(crate) fn route(hops: &[(&str, &str, &str)]) -> Route {
        let mut tokens: HashMap<Bytes, Token> = HashMap::new();
        let mut swaps = Vec::with_capacity(hops.len());
        for &(protocol, token_in_symbol, token_out_symbol) in hops {
            let token_in = symbol_token(token_in_symbol);
            let token_out = symbol_token(token_out_symbol);
            let component = ProtocolComponent::new(
                "test-pool",
                protocol,
                "swap",
                Chain::Ethereum,
                vec![token_in.address.clone(), token_out.address.clone()],
                vec![],
                HashMap::new(),
                ChangeType::default(),
                Bytes::default(),
                NaiveDateTime::default(),
            );
            swaps.push(Swap::new(
                "test-pool".to_string(),
                protocol.to_string(),
                token_in.address.clone(),
                token_out.address.clone(),
                BigUint::from(1_000u64),
                BigUint::from(990u64),
                BigUint::from(100_000u64),
                component,
                Box::new(UniswapV2State::new(U256::from(1_000_000u64), U256::from(1_000_000u64))),
            ));
            tokens.insert(token_in.address.clone(), token_in);
            tokens.insert(token_out.address.clone(), token_out);
        }
        Route::new(swaps, tokens).expect("test route must not be empty")
    }

    /// A deterministic token keyed by its symbol's last byte, matching the address scheme the
    /// pure-rendering tests use so both layers agree on the same token identity.
    fn symbol_token(symbol: &str) -> Token {
        let tag = symbol
            .as_bytes()
            .last()
            .copied()
            .unwrap_or(0);
        Token::new(&Bytes::from(vec![tag; 20]), symbol, 18, 0, &[], Chain::Ethereum, 100)
    }
}

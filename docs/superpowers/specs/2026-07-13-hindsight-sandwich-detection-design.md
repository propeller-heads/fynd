# Hindsight Sandwich Detection — Design

Date: 2026-07-13
Status: Approved

## Problem

Large "improvement" opportunities reported by hindsight are often sandwich victims: a frontrun
degraded the settled output, so Fynd's top-of-block re-solve looks like a big win. These trades
inflate the win and USD-uplift aggregates with value Fynd could not actually have captured (the
victim's execution was moved by MEV, not by inferior routing). Hindsight should recognize likely
sandwiches, record the evidence, and classify them apart from real wins and losses.

## Detection methodology

Bracket-pair with pool overlap — the zeromev/EigenPhi heuristic — plus a log-level direction
check on the attacker's token flow.

Detection runs inside `Decoder::decode_block`, which already fetches every receipt of the block in
one `eth_getBlockReceipts` call. All signals below come from those receipts; detection makes no
additional RPC calls.

For a victim trade at transaction index `i`, scan a window of `W = 2` transactions on each side for
a pair `(front, back)` with `front.index < i < back.index` satisfying all three conditions:

1. **Attacker link** — `front.from == back.from`, or `front.to == back.to` where that shared
   target is not a registry-known client or solver. The registry exclusion prevents two unrelated
   users entering the same popular router (Universal Router, 1inch) from tripping the same-`to`
   check; real sandwich bots settle through private contracts. Pairs where the linking address
   equals the victim's sender are excluded (self-trades are not sandwiches).
2. **Pool overlap** — both `front` and `back` emitted at least one log from a pool contract the
   victim's swap touched. Pool contracts are the addresses that emitted a log in the victim
   transaction, excluding everything known not to be a pool: ERC-20 `Transfer` and `Approval`
   emitters (token contracts), the wrapped-native token (its `Deposit`/`Withdrawal` logs appear
   on every wrapping transaction), and Permit2. Counting any of those would give two
   transactions that merely share a token or its plumbing a trivial overlap.
3. **Direction** — some linked entity accumulated the victim's output token in `front` and
   disposed of it in `back` (per that leg's ERC-20 `Transfer` logs), the shape of buying before
   the victim and selling after. Checked on the linking address and, for a shared-sender link,
   on the pair's shared target contract too — a bot's inventory usually sits in its private
   contract, not the signing EOA. A native-ETH output is checked as its wrapped form (native
   moves emit no log; pools settle in WETH). Without this condition, an arbitrage bot trading
   the same busy pool twice inside the window would flag every unrelated trade between its two
   transactions.

The first matching pair (closest bracket) wins; multi-victim grouping is out of scope.

### Known limitations

The heuristic is bounded in both directions; the effect on the aggregates differs:

**False negatives** — a real sandwich is not flagged, so its MEV-inflated "win" keeps polluting
the savings aggregates exactly as every sandwich did before this feature. Detection reduces that
pollution; it does not eliminate it:

- an attacker sandwiching an intermediate hop of a multi-hop victim (direction is checked on the
  victim's output token only);
- an attacker whose legs move only native ETH — invisible in receipts; rare, since pools settle
  in WETH;
- an attacker rotating both its sender and its contract between the two legs (no link holds);
- middle victims of a sandwich spanning more than `W = 2` transactions per side;
- Uniswap V4: the singleton PoolManager collapses all V4 pools into one log address, so overlap
  there is per-protocol rather than per-pool (coarser evidence, not a miss by itself — the link
  and direction must still hold).

**False positives** — a genuine comparison is reclassified to `sandwiched` and drops out of the
savings aggregates: a filler or batch-settlement solver EOA settling two opposite-direction
fills around an unrelated victim satisfies all three conditions. Label-known addresses cannot be
blanket-excluded, because several labeled operators are themselves sandwich bots. The evidence
keeps the attacker address, so the rate is measurable from the JSONL by joining attacker labels.

### Evidence

```rust
pub(crate) struct SandwichEvidence {
    pub front_tx: TxHash,
    pub back_tx: TxHash,
    /// The linking address: shared sender, or the shared non-router target contract.
    pub attacker: Address,
    /// The overlapping pool contracts.
    pub pools: Vec<Address>,
}
```

New module: `tools/hindsight/src/decoder/sandwich.rs`.

## Data plumbing

- `DecodedTrade` gains `tx_index: u64` (from the receipt) and
  `sandwich: Option<SandwichEvidence>` (skipped in serialization when `None`).
- `RangeComparison` carries both through; the JSONL comparison record gains `tx_index` and a
  `sandwich` object (`front_tx`, `back_tx`, `attacker`, `pools`). Fields are additive, so existing
  JSONL consumers are unaffected.
- The `decode` subcommand's human-readable and JSON output includes both.

## Verdict

New variant `Verdict::Sandwiched`, following the `CoverageMiss` pattern: in `build_range`, a
*solved* state of a trade with sandwich evidence gets `Sandwiched` as its verdict (the headline
follows top-of-block) — its win or loss measures the MEV that moved the settled output, not
routing quality. Unsolved states keep `Unsolvable`/`CoverageMiss`: the sandwich explains the
settled price, not why Fynd had no route, so the coverage shares the dashboard reports are
unaffected by reclassification. The bps and USD deltas are still computed and written to JSONL —
only the classification changes — so the size of MEV-inflated deltas remains studyable offline.

## Telemetry

- `TRADES_TOTAL` and `VOLUME_USD` gain the `outcome="sandwiched"` label value.
- Sandwiched trades skip the `SAVINGS_BPS`, `SAVINGS_USD`, and `IMPROVEMENT_USD` histograms:
  those carry no outcome label, so skipping is the only way to keep the "value of adding Fynd"
  aggregates clean.
- The per-trade Loki info line keeps logging, with `verdict=sandwiched`.

## Testing

Unit tests in `sandwich.rs` with synthetic receipts:

- same-`from` bracket pair detected;
- same-`to` pair detected only when the target is not a registry-known client/solver;
- no flag when the attacker link holds but pool overlap fails (and vice versa);
- window boundary: pairs beyond `W = 2` on either side are ignored;
- self-sandwich (linking address == victim sender) excluded;
- token-contract logs (Transfer/Approval emitters), the wrapped-native token, and Permit2 do not
  count as pools;
- direction: a linked pair with no token flow, or accumulating on both legs (arbitrage repeat),
  is not flagged; inventory held in the bot contract and native-output (WETH-mapped) flows are.

Plus: `build_range` verdict override test, JSONL serialization test for the new fields, and a
telemetry test that sandwiched states skip the savings histograms. The existing `verify`
subcommand remains a live check that detection does not disturb decoding.

## Out of scope

- Changes to the Python analysis package under `tools/hindsight/analysis` — the new fields are
  additive; segmentation on them can come later.
- Multi-victim sandwich grouping.
- Amount-aware direction checks (back-leg disposal sized against the front-leg accumulation).

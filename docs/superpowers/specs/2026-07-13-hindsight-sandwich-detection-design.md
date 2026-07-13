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

Bracket-pair with pool overlap — the zeromev/EigenPhi heuristic without direction decoding.

Detection runs inside `Decoder::decode_block`, which already fetches every receipt of the block in
one `eth_getBlockReceipts` call. All signals below come from those receipts; detection makes no
additional RPC calls.

For a victim trade at transaction index `i`, scan a window of `W = 2` transactions on each side for
a pair `(front, back)` with `front.index < i < back.index` satisfying both conditions:

1. **Attacker link** — `front.from == back.from`, or `front.to == back.to` where that shared
   target is not a registry-known client or solver. The registry exclusion prevents two unrelated
   users entering the same popular router (Universal Router, 1inch) from tripping the same-`to`
   check; real sandwich bots settle through private contracts. Pairs where the linking address
   equals the victim's sender are excluded (self-trades are not sandwiches).
2. **Pool overlap** — both `front` and `back` emitted at least one log from a pool contract the
   victim's swap touched. Pool contracts are the addresses that emitted a non-ERC20-Transfer log
   (`topic0 != Transfer`) in the victim transaction; filtering out Transfer-emitters keeps token
   contracts (WETH, USDC) from producing trivial overlaps.

Known coarseness: Uniswap V4's singleton PoolManager collapses all V4 pools into one log address,
so overlap there is per-protocol rather than per-pool. Acceptable — the attacker link must also
hold.

The first matching pair (closest bracket) wins; multi-victim grouping is out of scope.

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

New variant `Verdict::Sandwiched`, following the `CoverageMiss` pattern: in `build_range`, a trade
with sandwich evidence gets `Sandwiched` as its headline verdict and both per-state verdicts. The
bps and USD deltas are still computed and written to JSONL — only the classification changes — so
the size of MEV-inflated deltas remains studyable offline.

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
- token-contract logs (Transfer-only emitters) do not count as pools.

Plus: `build_range` verdict override test, JSONL serialization test for the new fields, and a
telemetry test that sandwiched states skip the savings histograms. The existing `verify`
subcommand remains a live check that detection does not disturb decoding.

## Out of scope

- Direction-aware decoding of attacker swaps (opposite-direction verification).
- Changes to the Python analysis package under `tools/hindsight/analysis` — the new fields are
  additive; segmentation on them can come later.
- Multi-victim sandwich grouping.

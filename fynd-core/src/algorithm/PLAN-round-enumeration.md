# Plan: round-based candidate enumeration for water-fill

A second way for water-fill to produce its candidate paths, living alongside the current one so the
two can be switched and compared. Nothing here removes existing code.

## The idea

Today water-fill enumerates every combination of pools for every token sequence, ranks the lot by a
spot×depth heuristic, keeps 5,000, and simulates all of them at the full order amount to rank them
properly. About a dozen of those ever get used.

Instead: ask most-liquid for the best route, take the pools it used off the table, ask again. Ten
rounds. Each round's answer is the best route that avoids everything already claimed, so the results
come out mutually component-disjoint and in descending order — which is what `select_disjoint`
currently has to hunt for.

Then fill in the combinations the rounds skip past. If round 1 is `A-(1a)-B-(2a)-C` and round 2 is
`A-(1b)-B-(2b)-C`, the mixes `(1a,2b)` and `(1b,2a)` are also candidates, and they are disjoint from
each other — a second valid pairing over the same four pools that greedy exclusion never offers.

The cache is what makes ten rounds affordable. Round 1 already simulated every pool on every leg it
touched; round 2 needs the pool that came second, which round 1 computed and threw away.

## Current code

`fynd-core/src/algorithm/most_liquid.rs`

| item | line | notes |
|---|---|---|
| `MostLiquidError` | 39 | private; `LegNotTradable { from, to }`, `TokenMissing(NodeIndex)` |
| `SettledRoute` | 53 | `legs: SmallVec<[SettledLeg; INLINE_LEGS]>`, `net_amount_out: BigInt` |
| `SettledLeg` | 65 | `pool: usize` (index into `pools_between`), `amount_out`, `gas` |
| `find_token_paths` | 308 | `pub(crate)`, returns `Vec<TokenPath>` |
| `score_token_path` | 377 | private, associated fn |
| `solve_token_path` | 431 | private; settles one token sequence against the cache |
| `build_route` | 503 | private; materialises the winner into a `Route` |
| `OrderSwapCache` | 1019 | `pairs: FxHashMap<(NodeIndex, NodeIndex), PairChoice>`, `enabled: bool` |
| `PairChoice` | 1025 | `pool: usize`, `outputs: FxHashMap<BigUint, (BigUint, BigUint)>` |
| `settle` | 1044 | picks a pool for one leg, simulating what it must |

`fynd-core/src/algorithm/water_fill.rs`

| item | line | notes |
|---|---|---|
| `WaterFillAlgorithm` | 121 | holds an `mla: MostLiquidAlgorithm` |
| `SetupResult` | 190 | `ordered`, `market`, `gas_price`, `best_single`, `token_prices` |
| `select_disjoint` | 298 | greedy, best-first, skips paths sharing a pool |
| `setup` | 327 | the pass being replaced |
| `find_candidate_paths` | 1220 | the bounded beam search; untouched by this work |

`fynd-core/src/graph/token_graph.rs`: `TokenPath` (70), `pools_between` (89), `paths_between` (148),
`expand_path` (193).

## Part 1 — rework the cache

Rename `OrderSwapCache` to `CachedPoolSwap` and make it `pub(crate)`. Two behaviour changes.

### Remember every pool, not just the winner

Today a pair remembers which pool won and what that pool paid per amount. The rounds need what the
*losers* paid, so replace `PairChoice` with a full outcome table per amount:

```rust
pub(crate) struct CachedPoolSwap<'g> {
    pairs: FxHashMap<(NodeIndex, NodeIndex), PairOutcomes>,
    excluded: FxHashSet<&'g ComponentId>,
    enabled: bool,
}

/// What every pool on one pair paid, per input amount seen.
struct PairOutcomes {
    /// Indexed by position in `pools_between`, so a pool costs an index rather than an id.
    by_amount: FxHashMap<BigUint, SmallVec<[PoolOutcome; INLINE_POOLS]>>,
}

enum PoolOutcome {
    /// Never simulated at this amount.
    Unasked,
    /// Simulated and could not trade the amount. Deterministic — do not ask again.
    Untradable,
    Paid { amount_out: BigUint, gas: BigUint },
}
```

`settle` then works from the table:

1. Look up `(pair, amount)`. Missing → all entries `Unasked`.
2. Simulate every `Unasked`, non-excluded pool and fill its entry in.
3. Return the best non-excluded `Paid` entry, ranked by net after that leg's own gas, exactly as
   today. `None` when there is no such entry.

After a round, that table is complete for the amounts it saw, so a later round at the same amount is
a lookup with no simulation at all. A round that reaches a leg at a *new* amount pays for the pools
it has to ask — see "what is not free" below.

Keep the existing `enabled` flag doing what it does now (off = never write, so every lookup misses).
The current "trust the previous winner, only re-simulate that one at a new amount" shortcut cannot
survive here — it is what leaves the table incomplete. Keep it only if it is put behind its own flag
that the round enumeration turns off.

### Exclusions

```rust
pub(crate) fn exclude(&mut self, component_id: &'g ComponentId)
pub(crate) fn is_excluded(&self, component_id: &ComponentId) -> bool
```

Global by `ComponentId`: a pool claimed by any round is off the table everywhere it appears. That is
what makes the rounds component-disjoint and matches how `select_disjoint` defines interference.

The set holds `&'g ComponentId` borrowed from the graph, which outlives the solve — no ids are
cloned. This puts a lifetime on `CachedPoolSwap`, so `MostLiquidAlgorithm::find_best_route` needs
`CachedPoolSwap::new(...)` bound to the graph borrow. That compiles as-is; the graph parameter is
already borrowed for the whole call.

Excluded pools are skipped inside `settle`, not filtered by the caller, so nothing above it has to
know about the set.

## Part 2 — the round loop

New file, `fynd-core/src/algorithm/round_enumeration.rs` (rename freely). It needs these to become
`pub(crate)` in `most_liquid.rs`: `score_token_path`, `solve_token_path`, `build_route`,
`SettledRoute`, `SettledLeg` (and their fields), `MostLiquidError`.

```
rank the token sequences once:
    sequences = mla.find_token_paths(graph, token_in, token_out)
    sort by score_token_path, descending      // same order most-liquid uses

for round in 0..MAX_ROUNDS (10):
    best = None
    for sequence in sequences:                // every sequence, every round
        match mla.solve_token_path(graph, sequence, market, prices, gas_price, cache, amount_in):
            Ok(settled) if settled.net_amount_out > best  => best = Some((sequence, settled))
            Ok(_)  => keep looking
            Err(_) => this sequence is used up for now, move on
    let Some((sequence, settled)) = best else { break }   // nothing left anywhere
    routes.push((sequence, settled))
    for leg in &settled.legs:
        cache.exclude(&pools_between(pair)[leg.pool].component_id)
```

Notes for whoever implements it:

- Every round scans every sequence. That is deliberate — round 2's best may be a different sequence
  entirely. The cost is bounded by cache hits, not by simulations.
- `MostLiquidError::LegNotTradable` means every pool on some leg is excluded (or none can trade the
  amount). Treat it as "this sequence is finished", skip it, do not count it as a failure.
- Stop early when a round produces nothing.
- The routes come out in descending net order by construction. Do not re-sort them against each
  other; do sort the final candidate list once the combinations below are added.

## Part 3 — cross-combinations

Only pools serving the same token pair may be swapped for one another — this is not a free product
over all pools.

```
group routes by their token sequence
for each sequence with two or more routes:
    per leg, collect the distinct pools those routes used
    emit every combination of one pool per leg
    drop combinations already produced as a round result
```

No cap for now. A sequence appearing in R rounds with L legs yields at most R^L combinations, so
watch this number in the logs — if it grows, a cap is the first thing to add.

Settle each combination through the same cache so its shared prefixes cost nothing, and rank it by
the net it produces. A combination's first leg is at the order amount and is already in the table;
downstream legs arrive at amounts the rounds never saw, so those cost one simulation each.

## Part 4 — water-fill integration

Output: `Vec<Path<'g, DepthAndPrice>>` sorted by net at the full order amount, descending — the same
shape and meaning as `SetupResult::ordered`, so everything downstream (`select_disjoint`,
`disjoint_waterfill`, `fillspill_alloc`) works unchanged.

Turning a `(sequence, SettledRoute)` into a `Path` borrows out of the graph: for each leg,
`&pools_between(from, to)[leg.pool]`, plus `&graph[node]` for the tokens. Use `Path::add_hop`. No
component ids, tokens or states are cloned.

`SetupResult::best_single` is the first round's route, built once with `build_route`.

**This replaces `setup`'s full-amount simulation loop entirely.** The rounds already settled every
route they returned, so re-simulating them to rank them would be doing the work twice.

The toggle goes on `WaterFillAlgorithm` as a field plus a consuming setter, matching
`MostLiquidAlgorithm::with_cache_pair_swaps`:

```rust
pub(crate) fn with_round_enumeration(mut self, enabled: bool) -> Self
```

Default to the existing enumeration so the new one is opt-in. Note the limitation: `AlgorithmConfig`
carries no field for this, so a benchmark comparison means flipping the default and rebuilding, one
arm per binary. Do not add the config plumbing as part of this work — that file needs its own
refactor first.

## Part 5 — reuse the cache in the split passes

Out of scope for the first pass, but the reason the cache is built this way. `simulate_step`
(`water_fill.rs:218`) re-simulates whole paths per chunk, and the chunk amounts repeat across paths.
Once the enumeration lands, look at whether the split passes can settle legs through the same cache.
The blocker is that the split passes simulate against per-path `committed` overlays, so a cached
entry keyed on `(pair, pool, amount)` alone is not valid there. Leave it until the enumeration is
measured.

## Tests

In `most_liquid.rs`, against `CachedPoolSwap` directly, with a closure that records which pools were
simulated (see `test_settle_scales_executed_price_to_spot_price_units` for the pattern):

- a second `settle` at the same amount simulates nothing
- excluding the winner returns the second-best pool and simulates nothing
- excluding every pool on the pair returns `None`
- a pool that failed once is not asked again at the same amount
- `enabled: false` still returns the right pool, simulating everything each time

In the new module, on a fixture with two pools per leg over one sequence:

- round 1 takes the best pool on each leg; round 2 takes the other two; both are disjoint
- the four-path set includes both mixes
- a sequence whose legs are exhausted is skipped and the loop continues on other sequences
- ten rounds against a graph with fewer than ten disjoint routes stops early rather than looping

In `water_fill.rs`:

- the existing water-fill tests pass with the toggle on, in particular
  `test_water_fill_output_near_two_component_optimum` and `test_split_fills_when_no_single_path_can`
- with the toggle on, the candidate list is sorted by net descending

## What is not free

Be straight about this in the final report rather than claiming ten free rounds:

- Only the first leg of a route sees a repeated amount. Once a round picks a different pool, every
  downstream leg arrives at a new amount and its pools have to be simulated. Reuse is strongest at
  depth 1 and thins out with depth.
- Every round walks every token sequence. With the cache warm those are hash lookups, but there are
  `rounds × sequences × legs` of them.
- The combinations in part 3 are unbounded by design for now.

## How to compare

`fynd-core/benches/configs/water_fill_d2.toml` and `water_fill_d3.toml`. Run each arm and compare
solve time and output amount per order. The numbers that matter: total solve time, how many
simulations each arm runs, and whether the split output is at least as good as before — a faster
enumeration that finds worse routes is not a win.

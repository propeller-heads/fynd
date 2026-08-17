# Plan: move decomposition onto the topology graph

Move `decomposition` from `StableDiGraph` (one edge per pool) to `TopologyGraph` (one edge per
token pair), and take two things from the algorithms already on it:

1. **Per-leg price filtering** — `most_liquid::score_token_path` (`most_liquid.rs:431`) scores a
   token sequence from the best pool on each leg. Decomposition's price filter can do the same and
   stop evaluating one product per pool combination.
2. **Score-based truncation** — the enumeration cap becomes a cap on *token sequences kept by
   score* instead of a prefix of a depth-first walk.

Not in this plan: the swap cache (`PoolRef::swap_cache` already memoises per pool and amount; the
gap is that branches hold private `PoolRef` copies, which is the `TODO.md` redesign item), the
`seen_pools` first-fit weakness, and water-fill's amount-aware discovery.

Everything below is inside `fynd-core/src/algorithm/decomposition/`. Nothing in `graph/`,
`derived/`, `feed/` or any shared crate changes.

## Why

`decomposition/token_graph.rs:151` enumerates one path per **pool combination**. Then
`graph_build.rs:524` hashes those paths back down by token sequence, and `build_branch`
(`graph_build.rs:609`) re-derives the per-leg pool set from the group. The pool-level granularity
is built only to be undone.

`TopologyGraph` hands over both halves directly: `paths_between_ix` returns the token sequence,
`pools_between` returns a leg's pools. The comment at `mod.rs:739-748` defends the current choice on
three grounds, none of which hold:

- *"one path per pool combination"* — created at `token_graph.rs:311`, discarded at
  `graph_build.rs:548`.
- *"shares the graph manager with `bellman_ford` and `path_frank_wolfe`"* — those are
  `StableDiGraph<()>`, decomposition is `StableDiGraph<DepthAndPrice>`, and `worker.rs:114` gives
  every worker its own manager. Nothing is shared.
- *"the weights on the edges are not read here"* — true, and it means decomposition wants the graph
  for topology alone, which is what `TopologyGraph` is.

Cost: enumeration goes from (sequences × ∏ pools per leg) to a meet-in-the-middle walk over pairs,
roughly `b^(n/2)`. On the recorded fixture `WETH→GNO` has 27 sell-side neighbours, so the 20,000-path
cap is doing real truncation today.

## Commit 1 — rename `token_graph.rs` to `search.rs`

Rename only, no content change, so the rewrite in commit 2 reads as a diff of code rather than a
diff of file names. The module no longer holds a graph once commit 2 lands, and `decomposition::
token_graph` sitting next to `graph::token_graph` is the confusing half of the current naming.

## Commit 2 — search over the topology graph

Rewrite `search.rs`. It stops being a graph wrapper and becomes the filter and the queries
decomposition runs against `TopologyGraph`.

**Deleted:** `DirectPath`, `SearchBounds`, `TokenGraph`, `PathSearch`, `DEADLINE_CHECK_INTERVAL`,
`path_component_ids`.

**Kept, reshaped:**

| Now | Becomes |
| --- | --- |
| `AllowedTokens` + `TokenGraph::new` | `fn search_filter(..) -> GraphQueryFilter` |
| `TokenGraph::paths_between` | `fn token_paths(graph, from, to, filter) -> Vec<TokenPath>` |
| `TokenGraph::contains_token` | `graph.get_token_ix(addr).is_some()` at the call sites |
| `TokenGraph::highest_degree_token` | same name, counting **pools** (see below) |
| `path_component_ids(paths)` | `fn sequence_component_ids(graph, sequences)` |

**`search_filter`** folds `AllowedTokens` into `GraphQueryFilter.connector_tokens`: the intersection
of the operator allowlist with the tokens the derived store prices, or the allowlist alone when
either endpoint is unpriced (`token_graph.rs:103`, which stops a thin market losing every route).
`GraphQueryFilter` owns its set, so this allocates once per solve where the current code borrows —
accepted.

**`token_paths`** returns empty when `from == to`, preserving today's behaviour
(`token_graph.rs:145`); `TopologyGraph` would otherwise return cycles, which decomposition has no
use for. It maps `GraphError::TokenNotFound` onto the endpoint checks `prepare_solve_input` does at
`mod.rs:471-477`.

**`highest_degree_token` must count pools, not edges.** On `StableDiGraph` `graph.edges(node).
count()` is a pool count; on `TopologyGraph` it is a *pair* count. That silently changes the derived
connector token, which the README and `DecompositionConfig::with_connector_token`
(`mod.rs:222`) both document as pool-edge degree. Sum `edge.weight().pools().len()` instead —
`water_fill::derive_anchor_tokens` (`water_fill.rs:1960`) already does exactly this, and its comment
is the warning: *"counting edges would rank a token on five thin pairs above one on three deep
ones."* This is behaviour preservation, not one of the two learnings.

**`sequence_component_ids`** dedupes `(from, to)` pairs across sequences, then unions
`pools_between`. Copies `most_liquid::snapshot_market_state` (`most_liquid.rs:572`).

**`reference_pools` stops searching.** It runs three path enumerations today (`mod.rs:567-585`) and
keeps only the component ids, discarding every `DirectPath` — each of which cloned a token vector and
a component vector to build (`token_graph.rs:311-322`). All three are `max_hops: 1` queries between
two fixed tokens, so on `TopologyGraph` the answer is `pools_between(from_ix, to_ix)`: a slice read,
no walk and no allocation. The same three pairs are then searched *again* by `reference.rs:215` when
the reference is actually built; that second search stays, since it needs the pools, but the first
one stops being a search at all.

**Call sites:**

- `mod.rs:749-750` — `GraphType = TopologyGraph<DepthAndPrice>`, `GraphManager =
  TopologyGraphManager<DepthAndPrice>`. Rewrite the comment above it; it currently argues the
  opposite.
- `SolveInput` (`mod.rs:285`) — `token_graph: TokenGraph<'a>` and `paths: Vec<DirectPath>` become
  `graph: &'a TopologyGraph<DepthAndPrice>`, `filter: GraphQueryFilter`, `sequences: Vec<TokenPath>`.
- `prepare_solve_input` (`mod.rs:407`), `get_connector_token` (`mod.rs:524`), `reference_pools`
  (`mod.rs:554`), `reference.rs:204` — follow the type changes.
- `reference.rs:70` `REFERENCE_MAX_PATHS` goes: a reference leg is a one-hop query between two fixed
  tokens, which is one token sequence.

**Deadline.** `GraphQueryFilter` carries no deadline and `bidirectional_search` has none, so the
truncation logic at `mod.rs:494-504` (which re-searches the reference legs when the candidate search
stopped early) loses its trigger. Drop it and always search the reference legs when `max_hops < 2`:
a one-hop query per leg is cheap and the search can no longer stop early. `water_fill::
top_scored_paths` runs the same unbounded search in production, so this is not a new exposure — but
it is the one place where the plan removes a bound rather than replacing it, and it should be
watched in the benchmark.

## Commit 3 — per-leg price filtering (learning 1)

Replace `group_by_token_sequence` (`graph_build.rs:524`) and `marginal_price`
(`graph_build.rs:581`). Today `marginal_price` runs `∏ spot_price · (1 - fee)` once per **enumerated
path**, so a pool on the first leg shared by 500 paths is asked for its spot price 500 times.

New shape, per token sequence:

```
struct LegPrices {
    /// `spot_price * (1 - fee)` per pool, in the leg's own direction. Same order as
    /// `TopologyGraph::pools_between`, so an index here is an index there.
    factors: Vec<f64>,
    /// The largest factor on the leg.
    best: f64,
}
```

Memoised by `(NodeIndex, NodeIndex)` for the length of one `build_routes_subgraph` call, because
sequences share legs. That memo is where the redundancy goes; no swap cache is involved, these are
spot prices.

Rules, in order:

1. Resolve every token against the registry. Any missing → drop the sequence. (Unchanged,
   `resolve_tokens`, `graph_build.rs:565`.)
2. Per leg, drop a pool whose simulation state is missing, whose `spot_price` errors, or whose
   factor is not finite or is `<= 0`. A leg left with no pools drops the sequence.
3. Sequence upper bound `U = ∏ best_i`. If `minimum_price > 0 && U < minimum_price`, drop the
   sequence — no combination through it can clear the floor.
4. Otherwise keep pool `p` on leg `i` when `f_i(p) · ∏_{j≠i} best_j >= minimum_price`.

**Why rule 4 is exactly what the current code computes.** Today a pool reaches `build_branch` iff at
least one enumerated path through it cleared the floor. The enumerated set for a sequence is the full
cartesian product of its legs' pools, so the best path through `p` on leg `i` takes the best pool on
every other leg — which is the left-hand side of rule 4. Same surviving set, without the product.

Compute `∏_{j≠i} best_j` by skipping leg `i`, not as `U / best_i`: legs are at most three, and the
division both differs in the last place and divides by zero when a leg's best factor is zero.

**Two deliberate differences:**

- A path today whose product is positive because *two* factors are negative would pass; rule 2 drops
  any non-positive factor. Spot prices are non-negative in practice, and a negative one is a broken
  simulation rather than a route.
- Pools now arrive at a hop in `pools_between` order (component-id sorted at graph build) rather than
  in enumeration order, before the existing `rank_desc` by `PoolRef::weight` and the `max_routes`
  truncation (`graph_build.rs:653`). Equal-weight pools can therefore be cut differently.

`build_branch` (`graph_build.rs:609`) then reads its pools from the filtered per-leg sets instead of
from `group.component_paths`. `seen_pools` and its first-fit behaviour are untouched.

## Commit 4 — score-based truncation (learning 3)

`max_enumerated_paths` (`mod.rs:140`, `mod.rs:241`) caps *pool paths* and cuts a **depth-first
prefix** — the first 20,000 in graph edge order, which no ranking has seen. Worse, a prefix cut can
leave a sequence with a partly-populated leg, so a group looks complete when it is not.

Replace with `max_token_paths`, a cap on **token sequences ranked by the upper bound `U` already
computed in commit 3**. Sequences are then either kept whole or not kept.

Order of operations in `build_routes_subgraph`:

1. Filter each sequence (commit 3), which yields `U` for free.
2. Sort by `U`, truncate to `max_token_paths`.
3. Build `SequentialRoute`s from the survivors — the expensive step, since every `PoolRef` clones a
   simulation state. The cap exists to bound *this*.
4. `rank_desc` by `SequentialRoute::weight` and group, unchanged (`graph_build.rs:151-153`).

`U` is a price bound with no depth in it, while `SequentialRoute::weight` is
`price · (1 - fee) · inertia` (`sequence.rs:221`). So the cut is coarser than the ranking that
follows it. That is the right trade at step 2: the cap is there to bound cost, the real ordering
still happens at step 4 over everything that survived. Set the default high enough that it rarely
binds — a sequence stands for all its pool combinations now, so the number is far below 20,000;
pick it from the benchmark's observed sequence counts rather than by converting the old value.

Public API: `with_max_enumerated_paths` is replaced by `with_max_token_paths`, not deprecated
alongside it. Update the doc comments at `mod.rs:128-131` and `mod.rs:183` that describe the old
quantity.

## Commit 5 — tests

- `tests/graph_build_tests.rs`, `tests/reference_tests.rs`, `tests/algorithm_tests.rs` build their
  markets with `setup_market_weighted_petgraph` (`test_utils.rs:561`). Switch to
  `setup_market_weighted` (`test_utils.rs:519`), which already returns a
  `TopologyGraphManager<DepthAndPrice>`. Check whether `setup_market_weighted_petgraph` has any
  caller left; if not, delete it.
- The helpers at `graph_build_tests.rs:61-109` (`bounds`, `TokenGraph::new`, `paths_between`) become
  the new `search_filter` / `token_paths` pair.

New tests worth having:

- Rule 4 equivalence: a two-leg fixture where one pool clears the floor only against the best pool
  on the other leg. It must survive, and it must not survive when its own factor is lowered.
- A leg whose pools all fail to price drops the sequence, and one whose pools partly fail keeps the
  rest.
- `highest_degree_token` picks the token with the most pools, not the most pairs — a fixture with one
  token on three thin pairs against one token on two deep pairs.
- Truncation keeps the highest-`U` sequences, and every kept sequence has every pool on every leg.

## Verifying

The output is expected to move, so "tests pass" is not the check.

1. Benchmark before and after on the same trade set (`benches/configs/decomposition_d2.toml`,
   `decomposition_d3.toml`) and compare per-pair BPS. Sequences that the prefix cut was dropping
   should come back; regressions point at the ordering change in commit 3.
2. Watch the `decomposition candidate subgraph built` debug line (`graph_build.rs:159`):
   `enumerated_paths` is replaced by a sequence count, and `token_paths`, `grouped_branches` and
   `kept_branches` should be recognisable against today's numbers on the same pairs.
3. Confirm the derived connector token is unchanged on the fixture — that is what the pool-count
   degree fix in commit 2 is protecting.
4. `./check.sh`.

## Open

- The default for `max_token_paths` needs a number from the benchmark, not a guess.
- Commit 2 removes the early-stop deadline from candidate discovery. If the benchmark shows a dense
  three-hop pair running long, the fix is bounds on `TopologyGraph`'s search — which is a change in
  `graph/`, outside this module, and would need to be raised before it is made.

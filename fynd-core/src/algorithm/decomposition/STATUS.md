# Where decomposition stands

Written at the end of the session that removed `Branch` and moved the search onto `TopologyGraph`.
Current names only — the module was renamed twice during that session.

## Shape today

```
DecompositionGraph = ParallelRoute          // a type alias, not a type
ParallelRoute.inner : Vec<SplitKind>        // alternatives + their splits
SplitKind           = Sequence | Direct     // a chain, or one pool
SequenceRoute.hops  : Vec<ParallelRoute>    // hops in series
Pool                                        // one component, one direction
```

Two kinds of split and nothing else: a `ParallelRoute` over pools (a hop), or over sequences (the
graph's branches, and a grouped branch's tails). A grouped branch is a two-hop `SequenceRoute` — the
shared hop plus a split over the tails — so `Branch` and `BranchSide` no longer exist.

`solve_parallel_route` is the only split solver; it solves the graph and a hop alike, taking the
optimizer for its own level plus the one for everything below. `solve_decomposition_graph` is the
full `_solve` pipeline on top of it.

## Landed this session

- `Branch` deleted (938 lines), `graph.rs` deleted (416), `reference.rs` folded into `mod.rs`.
- `Sellable` deleted — every split hands its amount to `SplitKind`, so no trait is needed.
- `solve_graph` collapsed into `solve_parallel_route`. The two copies had already drifted: one
  checked for a zero amount before the single-alternative case, defibot checks it after.
- Five `ParallelRoute` guards fixed to defibot's predicate (`not solved`, not "no splits"). Latent —
  it only bites when a level has splits set *and* an unsolved alternative, which the fixtures never
  produce.
- Search moved to `TopologyGraph<DepthAndPrice>`: `paths_between_ix` for token sequences, then
  `expand_path` for pool combinations. Downstream still receives `DirectPath`, deliberately.
- `assemble.rs` carries split factors unmultiplied and folds them once, because `f64` multiplication
  is not associative and the old grouping order has to be reproduced exactly.

Every one of these is at **zero snapshot movement**.

## Read this before touching anything

**The snapshot test flakes.** At least one scenario solves close enough to `TIMEOUT_MS = 5_000` that
CPU load flips it to `Timeout`, and the test then reports "route changed". It passes alone and fails
in the full suite, on a *different* scenario each time.

This cost most of a session. A flaky timeout was diagnosed as a regression, "fixed" by deleting three
defibot behaviours, and the passing run was read as confirmation. Restoring all three later showed
nothing had been broken.

**So: before calling anything a regression, run the snapshot alone.** Fixing the test — raising the
timeout, or dropping `status` from the comparison — is the highest-value small job on this list.

`test_candidate_beats_the_reference_on_a_path_the_reference_cannot_see` has failed all session and
predates it. 963/965 otherwise; `fmt` and `doc` clean; 7 clippy warnings, all pre-existing.

## Next steps, in order

1. **Fix the snapshot flake.** Everything below is verified against it.
2. **Real token-path scoring.** Score sequences by simulating the best pool per leg at the order
   amount instead of the `price × (1 - fee) × inertia` heuristic, then rank, then group by head/tail
   as now. Needs the cache below. Note it makes shared-pool overvaluation *worse* before grouping
   corrects it — that is the experiment.
3. **The order-lifetime swap cache.** Take water_fill's `SwapCache` (`water_fill.rs:291`), not
   most_liquid's `PoolSwapsCache`: it is keyed per pool-direction, which is what a solve that splits
   across a pair needs, and it already has amount interpolation and per-caller opt-in.
   - It needs one axis water_fill lacks: `may_cache()` separate from `may_interpolate()`. The
     coupled-path pass re-sells the same `(component, amount)` against state an earlier branch moved
     and must bypass *exact* hits, not just interpolation.
   - The bypass covers the whole pass, not its first sell: `sell_branches_in_sequence` propagates
     post-trade state into later branches mid-loop.
4. **Stop expanding to `DirectPath`.** Once scoring is on sequences, `group_by_token_sequence` and
   the expansion both delete and `build_token_sequence` reads legs from `pools_between`.
5. **Depth guard.** `solve_parallel_route` and `solve_sequence_route` are mutually recursive with
   nothing bounding nesting. Construction only rejects split-of-split.
6. **Pure quoting** (`TODO.md`) — `sell(&mut self)` still mutates. Step 3 is its prerequisite.

## Doc hygiene

- `README.md` — describes five types including `Branch`. Rewrite the structure section.
- `PLAN-route-refactor.md` — **delete**. It specifies a design we did not build, and its claim that
  `SplitOptimizer` needed object-safety work was already false. It misled twice.
- `PLAN-topology-graph.md` — the wins still apply and got bigger, but every name and line number is
  stale and the `reference_pools` section describes waste that no longer exists. Prefer
  `file:symbol` over `file:line` when regenerating.

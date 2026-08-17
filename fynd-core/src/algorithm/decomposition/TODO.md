THE ACTUAL TODOS (written by human)

- ~~optimize performance on BF~~
- ~~optimize performance on WF~~
- review decomposition head to toe
    - benchmark and profile
- test using ARCs on derived data store
- remove component ID as string and use a 32byte array which is copiable and hasahble
- Test BF's heuristics for sorting decomposition routes
- redesign caching, optimizer contract, state mutation, route components, the "sellable"
  trait (see more below)
- make bounded depth=3 decomposition with allowed tokens
- check WF sell limit bug
- see https://propellerswap.slack.com/archives/C0B6CCP9WJ0/p1786018811003529
- see https://propellerswap.slack.com/archives/C05LQALE9L1/p1786013988336139

## Redesign: pure quoting, owned route components, no `Sellable`

Four problems that turn out to be one problem. **Do not start without a snapshot test**
that pins
the route every benchmark pair returns — every step below can change output silently.

### What is wrong now

- `Sellable` is an 11-method trait with 5 impls (`optimizers/mod.rs`), and 9 of the
  methods are
  getters that report what the *last* sell left on the object. It lives under
  `optimizers/` but
  `solve.rs` uses it too.
- `optimize` is generic over the alternative, so `SplitOptimizer` is not object-safe.
  That is why
  `solve_graph` takes two optimizer type parameters and why `mod.rs` repeats the same
  call in a
  `match` arm per choice.
- Selling mutates. `PoolRef::sell` writes `sell_amount`, `buy_amount`, `gas`,
  `new_state` and the
  swap cache, so the optimizer's results are read back off the objects afterwards. That
  forces the
  route components to be passed as `&mut`.
- The search cannot see a pool shared by two branches. Each alternative owns private
  `PoolRef`
  copies and is scored against untouched state; `sell_with_coupled_paths` only corrects
  the totals
  afterwards.

### The shape to move to

1. **Split `types.rs` into `components/`**, one file per type — `pool.rs`, `hop.rs`,
   `sequence.rs`, `branch.rs`, `graph.rs`, `split.rs`. defibot has exactly this split
   (`routes/simple.py`, `parallel.py`, `sequential.py`); the port collapsed it into one
   3,800-line
   file.
2. **`enum RouteComponent`** in `components/mod.rs` with a variant per type, delegating
   each method
   to the type's own file. It replaces `Sellable`: the optimizer calls a method on the
   enum instead
   of being generic. `SplitOptimizer` then becomes object-safe, `solve_graph` loses both
   type
   parameters, and the `match` in `mod.rs` collapses to picking a
   `Box<dyn SplitOptimizer>`.
3. **Make quoting pure.** `ProtocolSim::get_amount_out` already takes `&self` and
   returns the new
   state, so nothing below us needs mutation. Replace `sell(&mut self, ..)` with
   `quote(&self, amount, ..) -> Quote { bought, gas, new_state }`.
4. **`optimize` returns the splits and mutates nothing.** The caller applies them. With
   no `&mut`,
   `RouteComponent` can own its variants instead of borrowing them.

### The two things that have to move somewhere

- **The swap cache.** It is on `PoolRef` today. Pure quoting needs it in the calling
  context, keyed
  by `(component, amount)`. It is not optional: `PairComparison` re-sells the same
  amounts across
  its five refinement passes, and the Frank-Wolfe line search hits repeats constantly.
- **Shared-pool state.** A sequence can fold an amount through its hops from base state,
  because
  `build_branch` already forbids one pool serving two legs of a sequence. But
  `sell_with_coupled_paths` re-sells branches against each other's post-trade state, so
  pure
  quoting has to carry an explicit state overlay — which is `build_post_swap_overrides`
  in
  `PathFrankWolfeAlgorithm`.

That overlay is the prize: the same mechanism purity needs is the one that would let the
split
search score shared pools correctly *during* the search, instead of overvaluing a split
and having
`sell_with_coupled_paths` walk it back.

### Order

Steps 1 and 2 are mechanical and safe to land on their own. Steps 3 and 4 change
behaviour and
should wait for the snapshot test.

## Fix bellman_ford's hop limit

Not decomposition, but it invalidates every benchmark comparison, so it blocks the work
above.

`BellmanFordAlgorithm::get_subgraph` bounds which **nodes expand**, not how long a path
may be. An
intermediate token reachable by a short path contributes its own outgoing edges, and the
relaxation
chains through them. At `max_hops = 2` it returns 3-hop routes; at `max_hops = 3`, 5-hop
ones. Its
own `test_respects_max_hops` passes only because the far node in that toy graph is not
also
reachable by a short path.

Until this is fixed, compare against `water_fill`, which honours the limit.


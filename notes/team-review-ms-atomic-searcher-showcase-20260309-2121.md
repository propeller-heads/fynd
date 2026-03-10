# Team Code Review: ms/atomic-searcher-showcase

**Reviewed:** 2026-03-09
**Agents:** Claude, Codex (gpt-5.2-codex), Gemini (gemini-2.5-pro)
**Test Results:** type-check PASS, tests 236/238 PASS (2 pre-existing failures)

## Executive Summary

The atomic searcher showcase is well-structured and working end-to-end against live Tycho. The algorithm is sound (layered BF + GSS). Three issues need attention before merging: f64 precision loss in the GSS, redundant cycle scanning, and missing state overrides for repeated pools. None are blockers for a showcase, but the GSS precision issue should be fixed.

## Synthesis

### High Confidence Issues (flagged by 2+ agents)

1. **GSS f64 precision loss** (all 3 agents)
   `amount_optimizer.rs:153-159,182` - Converting BigUint amounts (1e18 scale) to f64 loses precision above ~9e15. The GSS operates in f64 space but the inputs are wei-scale integers. This can shift the optimal point or collapse the search interval. The `as u128` cast can also overflow silently.
   **Severity:** Important. Fix for correctness.

2. **Redundant cycle detection** (Codex + Gemini)
   `cycle_detector.rs:127-215` - `find_cycles()` scans `active_nodes` for closing edges, then `find_closing_edges()` scans ALL layers for the same thing. The second pass subsumes the first. This doubles simulation calls for closing edges.
   **Severity:** Should fix. Remove the first pass.

3. **No state overrides / re-simulation** (Codex + Gemini)
   `amount_optimizer.rs:38-49` - Each hop in the GSS evaluation simulates against original pool state. If a pool appears twice in a cycle (or two candidates share a pool), the simulation is optimistic. The solver (bellman_ford.rs) handles this with `native_state_overrides` and `vm_state_override`.
   **Severity:** Consider. Acceptable for a showcase with a documented limitation, but real cycles with repeated pools would be mispriced.

### Worth Investigating (single-agent findings)

4. **O(V) Address-to-NodeIndex lookup per edge** (Codex)
   `amount_optimizer.rs:101-113` - `graph.node_indices().find()` is O(V) per edge per candidate. Should build a HashMap<Address, NodeIndex> once per block.

5. **GSS upper bound diverges on always-negative profit** (Codex)
   `amount_optimizer.rs:133-149` - If profit is `-inf`, `doubled_profit < hi_profit` is false (NaN comparison), so the loop doubles 20 times. Should short-circuit on non-finite profits.

6. **Memory: distance/predecessor sized by full graph** (Codex)
   `cycle_detector.rs:37-49` - Allocates by max node index in the entire graph, not the subgraph. For sparse graphs, this wastes memory. Could remap to dense indices.

7. **net_profit overflow/saturation** (Codex)
   `amount_optimizer.rs:204-209` - `BigUint::to_i128().unwrap_or(i128::MAX)` then `saturating_sub` can give wrong results for very large values. Should use BigInt.

### Contradictions

None. All three agents agreed on the core issues. Gemini was less specific on line numbers; Codex provided the most detailed analysis.

## Combined Issues

### Blockers

None. This is a showcase example, not production code.

### Important (should fix)

- [ ] **GSS precision**: Replace f64-space GSS with integer-space bisection, or at minimum guard against `to_f64()` returning infinity (`amount_optimizer.rs:153-182`) -- found by: all 3
- [ ] **Remove redundant cycle scan**: Delete the first closing-edge scan in `find_cycles()` lines 127-196; `find_closing_edges()` already covers it (`cycle_detector.rs:127-215`) -- found by: Codex, Gemini
- [ ] **Validate seed_eth > 0**: Add early check for negative/NaN/zero seed (`main.rs:127`) -- found by: Codex

### Consider (nice-to-have)

- [ ] Build `HashMap<Address, NodeIndex>` once per block instead of O(V) scan per edge (`amount_optimizer.rs:101-113`) -- found by: Codex
- [ ] Short-circuit GSS when profit is non-finite (`amount_optimizer.rs:140-149`) -- found by: Codex
- [ ] Add state overrides to GSS evaluation for repeated pools, or document the limitation (`amount_optimizer.rs:38-49`) -- found by: Codex, Gemini
- [ ] Use BigInt for net_profit instead of i128 (`amount_optimizer.rs:204-209`) -- found by: Codex
- [ ] Remap subgraph nodes to dense indices to reduce distance array size (`cycle_detector.rs:37-49`) -- found by: Codex

## Action Items

- [ ] Fix GSS precision (switch to integer bisection or add f64 overflow guards) (`amount_optimizer.rs:153-182`)
- [ ] Remove redundant cycle scan (`cycle_detector.rs:127-196`)
- [ ] Add seed_eth validation (`main.rs:127`)
- [ ] DECISION: Is state-override re-simulation needed for the showcase, or is a README note sufficient?

---

<details>
<summary>Claude Review (full)</summary>

The code is well-organized with clean separation between cycle detection, amount optimization, and the main loop. The BF adaptation from A-to-B to cycle detection is correct in principle. The SPFA optimization and subgraph extraction are proper reuses from the solver.

Key findings: (1) GSS precision: the f64 conversion is the weakest link. At 1 ETH = 1e18 wei, f64 mantissa (53 bits) can represent ~9e15 exactly. Amounts above ~9 ETH lose sub-wei precision in the GSS. This is acceptable for a showcase but would be wrong for production. (2) The redundant scan is wasteful but harmless. (3) The dedup key using only component IDs is weak; two cycles with the same pools but different token orderings would be collapsed. In practice, this is unlikely since the BF follows directed edges, but the key should include from/to addresses for correctness. (4) The library visibility changes are appropriate for an extensibility story.
</details>

<details>
<summary>Codex Review (full)</summary>

See above: 10 findings with specific line references covering GSS precision, upper bound divergence, unprofitable cycle reporting, net_profit overflow, redundant cycle detection, memory allocation, O(V) scans, state overrides, and CLI validation.
</details>

<details>
<summary>Gemini Review (full)</summary>

Three priority tiers: High (GSS precision, memory churn), Medium (redundant cycle detection, no state overrides), Low (GSS upper bound edge cases). Agrees on all core issues. Notes the algorithmic foundations are solid.
</details>

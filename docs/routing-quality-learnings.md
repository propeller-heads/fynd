# Routing Quality Learnings

Generated on 2026-06-27 after the routing-quality evolution loop, hidden-holdout validation, and a fresh 10k trade rerun.

## TLDR

The biggest routing-quality lesson is that the route search should not be thought of as "find one best path." For meaningful DEX aggregator trades, especially larger trades, the real problem is "find a portfolio of executable paths, allocate flow across them, account for gas, and model shared pool state honestly." The old `split` algorithm captured the first major improvement by splitting across pool-disjoint paths. The new `agent_candidate` keeps that safety baseline and adds shared-pool splitting, path-frontier expansion, allocation refinement, execution-order search, route compression, protocol-diverse candidate selection, and Bellman-Ford path injection.

The biggest benchmark-process lesson is that LLM-driven algorithm evolution works only if it is treated like an optimization experiment, not like a one-off coding session. The loop needs a frozen market snapshot, a visible exposed dataset, a hidden holdout, persistent per-iteration commits, notes carried forward to fresh sessions, and final reruns on a fresh sample. Without that, agents overfit, repeat failed ideas, or optimize charts that are not comparable.

On the final hidden holdout, `agent_candidate` beat `split` with 365 wins, 0 losses, and +1.3838 mean bps on the common-success set. On a later fresh 10k Dune sample, the final candidate beat Bellman-Ford with 1991 wins, 0 losses, and +373.5139 mean bps. Against the unmodified Fynd `path_frank_wolfe` baseline on that same fresh sample, the final candidate improved mean bps by +258.7343 and added +1659 wins, while keeping losses at 0.

## Artifacts

The main repo documents are:

| Path | Purpose |
|---|---|
| `docs/routing-quality-bench.md` | Original offline benchmark design, early split/PFW comparisons, and baseline learnings. |
| `docs/routing-quality-handover.md` | Handover for future agents improving routing quality. |
| `docs/routing-quality-evolution-loop.md` | How the LLM iteration loop works, including exposed/holdout split and agent invocation. |
| `docs/routing-quality-learnings.md` | This consolidated learning document. |

The most important campaign artifacts are:

| Path | Purpose |
|---|---|
| `.agents/routing-quality/runs/bold-final2-claude-opus-max-20260626-235511-61777/summary.md` | Final evolution-loop summary. |
| `.agents/routing-quality/runs/bold-final2-claude-opus-max-20260626-235511-61777/public/learnings.md` | Per-iteration agent notes and accumulated learnings. |
| `.agents/routing-quality/runs/bold-final2-claude-opus-max-20260626-235511-61777/private/final-holdout.json` | Hidden-holdout result versus `split`. |
| `/Users/markusschmitt/Documents/llm-output/2026-06-27-fresh-10k-trades/fresh10k_algorithm_report.md` | Fresh 10k rerun report versus Bellman-Ford. |
| `/Users/markusschmitt/Documents/llm-output/2026-06-27-fresh-10k-trades/fresh10k_algorithm_summary.csv` | Machine-readable fresh 10k summary. |
| `/Users/markusschmitt/Documents/llm-output/2026-06-27-fresh-10k-trades/fresh10k_final_charts_report.html` | Final HTML report with all fresh 10k charts, annotations, and runtime charts. |
| `/Users/markusschmitt/Documents/llm-output/2026-06-27-fresh-10k-trades/fresh10k_mean_bps_chart.html` | Fresh 10k mean-bps chart. |
| `/Users/markusschmitt/Documents/llm-output/2026-06-27-fresh-10k-trades/fresh10k_bps_distribution_summary.csv` | Per-algorithm distribution summary for trade-level bps deltas versus Bellman-Ford. |
| `/Users/markusschmitt/Documents/llm-output/2026-06-27-fresh-10k-trades/fresh10k_bps_distribution_points.csv` | Per-trade bps deltas versus Bellman-Ford, derived from the saved 10k result nets. |
| `/Users/markusschmitt/Documents/llm-output/2026-06-27-fresh-10k-trades/per_trade_improvement_csvs/` | Full 10,000-row per-iteration CSVs showing every trade's improvement versus Bellman-Ford. |
| `/Users/markusschmitt/Documents/llm-output/2026-06-27-fresh-10k-trades/fresh10k_per_trade_improvement_csvs.zip` | Zip archive containing the full per-iteration trade CSVs, combined long CSV, manifest, and README. |
| `/Users/markusschmitt/Documents/llm-output/2026-06-27-fresh-10k-trades/timing/fresh10k_timing_100_enriched.csv` | Solve-time measurement on a 100-request subset from the fresh 10k sample. |
| `/Users/markusschmitt/Documents/llm-output/2026-06-27-routing-quality-vs-bellman-ford-charts.html` | Earlier comparison charts versus Bellman-Ford. |

## Benchmark Lessons

The offline harness was the right architecture. It loads a frozen `MarketSnapshot`, computes derived data once, constructs in-process solvers, and replays every algorithm over the same trade list. This made algorithm differences attributable to code changes, not live market drift, websocket timing, RPC state, or API behavior.

The primary metric should remain `net_amount_out`, equivalent to production `amount_out_net_gas`. Gross output is misleading once routes add extra paths, because an extra path can improve gross output while losing after gas. The final route selection has to compare net output after gas conversion into output-token terms.

Coverage matters. A router can look good if it only solves easy trades. Every result needs coverage, wins, losses, mean bps, and median bps. The benchmark reports wins/losses on the common-success set, but coverage tells whether one algorithm is failing to route trades the others can solve.

Median bps is not very informative for this dataset. It is 0.00 across nearly every run because most common-success trades are small, single-path, and identical across algorithms. The signal lives in mean bps, win count, loss count, coverage, and per-trade deltas among the subset of trades where routing choice matters.

The frozen snapshot limits what can be concluded. The snapshot used for the main evolution work is native-protocol-heavy, with Uniswap v2/v3-like pools and no VM-backed Balancer/Curve-style state serialization. This was a pragmatic choice because native `ProtocolSim` states round-trip through JSON reliably, while VM-backed states are not yet good snapshot material. It means results are strong for the supported snapshot universe, but not a final claim about all possible Fynd production routes.

Benchmark comparisons must use the same snapshot and request set. Earlier comparisons used an original roughly 10k request file and a frozen snapshot. The final rerun used a fresh Dune 10k request file but deliberately reused the same frozen snapshot so every algorithm version remained comparable on market state. If the snapshot changes, all algorithms must be rerun.

The trade sample needs raw amounts. Dune `dex_aggregator.trades` has `token_bought_amount_raw` and `token_sold_amount_raw`, and those raw fields are required for exact benchmark requests. Human-readable decimal amounts are not enough because token decimals vary.

The fresh 10k sample pulled from Dune used Ethereum aggregator trades over the prior 168 hours, `amount_usd >= 100`, random order, and `LIMIT 10000`. It produced 10,000 CSV rows, 10,000 benchmark entries, and 0 skipped rows. Dune execution id: `01KW4Q18CGH80MZWWPABQ4TDET`.

## Dataset Lessons

The final evolution campaign used 9973 source trades, split into 7473 exposed trades and 2500 hidden holdout trades with seed 424242. Per-iteration benchmarking used a 1000-trade sample from the exposed set. The final holdout benchmark used the hidden holdout, and agents did not see it through the default sandbox/prompt setup.

The fresh 10k rerun was separate from the evolution holdout. It was useful because it answered a different question: not "did we overfit the held-out split from the original dataset," but "does the whole sequence still look good on a newly sampled set of real aggregator trades?" That rerun confirmed the broad ordering, but also showed that some later changes improve win count more than mean bps.

Only about 63 percent of the fresh 10k requests were solved by the main algorithms on the frozen snapshot. That is expected given the snapshot covers a subset of protocols/pools and a specific market state. It should not be interpreted as production coverage. It is benchmark coverage under this frozen experimental universe.

The common-success set can hide coverage differences. On the fresh 10k rerun, Bellman-Ford coverage was 6308, split and `agent_candidate` coverage were 6312, and original `path_frank_wolfe` coverage was 6298. Those differences matter even when the bps table is reported on common-success trades.

## Baseline Algorithm Lessons

`most_liquid` is a useful simple baseline but not competitive on quality. It tends to choose the obvious high-liquidity path, and many small trades tie, but it misses multi-hop and split opportunities.

Bellman-Ford is a real upgrade over `most_liquid` because it searches multi-hop routes and can reach paths that a heuristic ranking misses. In several iterations, the remaining losses were Bellman-Ford-only wins, which indicated a path-search/topology gap rather than an allocation-quality gap.

`split` was the first major quality breakthrough. It enumerates candidate paths, simulates them at the full amount, picks a best single path, selects pool-disjoint paths, water-fills chunks across them, and returns the better of the split route or the best single path. Pool-disjoint splitting captures most of the easy split-routing gain while staying simple and on-chain-valid.

The unmodified Fynd `path_frank_wolfe` implementation is a real improvement over Bellman-Ford, but it was not enough. On the fresh 10k rerun, unmodified PFW had 6298 coverage, 332 wins, 0 losses, and +114.7796 mean bps versus Bellman-Ford. The final `agent_candidate` had 6312 coverage, 1991 wins, 0 losses, and +373.5139 mean bps versus Bellman-Ford.

The reason `split` and PFW differ is structural. `split` is conservative and pool-disjoint, while PFW can optimize allocations more smoothly. But the new candidate's shared-pool sequential simulation keeps the important validity property while allowing more expressive route portfolios.

## Final Algorithm Architecture

The final `agent_candidate` should be understood as a route-family selector. It does not trust any single heuristic. For each order it builds candidate paths, evaluates several route families, simulates each candidate net of gas, optionally compresses routes, and returns the best valid net output.

The safety baseline is always present. The algorithm evaluates the best single path and a faithful pool-disjoint `split` replica. That means experimental routes are additive candidates. They can raise the chosen net, but they are not supposed to force a worse route than the incumbent split-style route.

The central validity trick is sequential shared-pool simulation. When a route uses overlapping pools, the algorithm does not independently simulate every leg from the original pool state. It simulates legs one after another and threads a shared map of updated pool states. Later legs see the depleted state left by earlier legs. This avoids the classic benchmark artifact where a router double-counts the same liquidity.

The main route families in the final candidate are:

| Route family | What it does | Main lesson |
|---|---|---|
| Best single path | Simulates each candidate path at full amount and keeps the best single route. | Re-simulation alone is a strong baseline and protects small trades. |
| Faithful `split` replica | Reproduces pool-disjoint split water-fill. | Keep a known-good fallback so new ideas cannot regress the core route. |
| Fill-and-spill | Allocates chunks across non-disjoint paths while updating pool state. | Biggest quality lever; shared-pool splitting captures routes `split` must reject. |
| Convex piecewise flow | Builds marginal-output curves and allocates fixed segments to best marginal path segments. | Useful as an alternate starting point, but only after exact sequential refinement. |
| Conditional-gradient/Frank-Wolfe refinement | Moves flow between paths along promising pairwise directions and line-searches the amount. | Helps refine allocations, but gains are small compared with fill-and-spill. |
| K-shortest frontier | Adds near-best and diverse paths that the normal spot-depth ranking may bury. | Candidate discovery matters after allocation improves. |
| Beam/A* frontier | Searches token-path prefixes using optimistic bounds and keeps promising complete paths. | Can add useful paths, but must be re-simulated because the bound is heuristic. |
| Execution-order search | Tries several execution orders for shared-pool paths. | Order matters when routes share pools; gains are real but usually small. |
| Portfolio local search | Mutates active path sets and split amounts, accepting only exact net improvements. | Substantially increases win count, but runtime becomes a concern. |
| Protocol-aware frontier | Reserves candidate slots for protocol-family diversity. | Prevents v3-like near-duplicates from crowding out v2 side-liquidity routes. |
| Gas compression | Merges duplicate same-pool same-direction swaps into one combined swap if net improves. | Removing redundant gas can turn gross wins into net wins. |
| Bellman-Ford injection | Adds Bellman-Ford-style paths into the candidate pool. | Solves the residual topology/path-search losses versus BF. |

## Iteration-by-Iteration Learning

The large improvement came early from shared-pool fill-and-spill. In the fresh 10k rerun, iteration 1 jumped from original PFW's +114.7796 mean bps versus Bellman-Ford to +373.0400 mean bps, with wins rising from 332 to 1519. This is the strongest evidence that shared-pool split routing is the main quality frontier.

Convex piecewise flow improved the exposed sample slightly, but the standalone approximation was dominated unless followed by exact sequential refinement. The useful part was not the approximate flow model by itself. The useful part was creating a different allocation starting point and then scoring it against the true executable objective.

Conditional-gradient/Frank-Wolfe refinement gave small but real gains. It improved allocations by finding larger pairwise moves than fixed-step hill-climbing. The right objective was always exact sequential net output, not the approximate separable path model.

K-shortest and beam/A* frontiers taught that candidate discovery still matters after allocation improves. The wins were not large, but they found paths that the normal completed-path ranking or spot-depth heuristic would not prioritize. The important constraint is that frontier scores are only prefilters; every selected path still needs full simulation.

Execution-order optimization improved exact net but did not necessarily create many new wins. This tells us that order matters, but only on the subset of shared-pool routes where the route was already active and near optimal. It is a polish layer, not the main engine.

Portfolio local search was a meaningful win-count layer. It improved the exposed 1000-trade benchmark from 104 to 127 wins versus `split`, with 0 losses, and improved 55 candidate nets with no worsened nets in the before/after comparison. The lesson is that deterministic local mutation over path sets can unlock missed path activations and split fractions, as long as every proposal is scored by exact sequential simulation.

Protocol-aware candidate selection improved cases where generic ranking spent too many slots on near-duplicate concentrated-liquidity paths. The agent diagnostics showed many wins used mixed v2/v3 route portfolios. Reserving slots for constant-product, concentrated-liquidity, and mixed profiles helped expose useful side liquidity.

Gas compression was the first layer that improved by removing swaps rather than adding paths. Shared-pool routes can contain duplicate same-pool same-direction legs, especially when paths converge or diverge. Merging those into one swap of the combined amount preserves the pool-state endpoint and can reduce gas materially. On the exposed sample, compression improved 66 candidate nets, worsened none, and created 7 new wins versus `split`.

Bellman-Ford injection removed the remaining losses versus Bellman-Ford in the fresh 10k rerun. Earlier iterations kept seeing a small set of Bellman-Ford-only wins that allocation and order improvements could not fix. That was the clue that the residual problem was path discovery/topology, not split allocation. Injecting BF-style paths into the candidate pool fixed that class.

## Results We Should Remember

Final hidden holdout versus `split`, from the evolution campaign:

| Algorithm | Coverage | Wins vs split | Losses vs split | Mean bps | Median bps |
|---|---:|---:|---:|---:|---:|
| most_liquid | 1495 | 0 | 202 | -36.4969 | 0.00 |
| bellman_ford | 1497 | 21 | 182 | -18.9038 | 0.00 |
| split | 1500 | 0 | 0 | 0.0000 | 0.00 |
| agent_candidate | 1500 | 365 | 0 | +1.3838 | 0.00 |

Fresh 10k rerun versus Bellman-Ford:

| Iteration | Candidate | Coverage | Wins | Losses | Mean bps vs BF | Median bps |
|---:|---|---:|---:|---:|---:|---:|
| 0 | Unmodified Fynd path_frank_wolfe | 6298 | 332 | 0 | 114.7796 | 0.00 |
| 1 | Penumbra / fill-and-spill | 6312 | 1519 | 126 | 373.0400 | 0.00 |
| 2 | Convex flow | 6312 | 1563 | 126 | 373.0450 | 0.00 |
| 3 | Frank-Wolfe refinement | 6312 | 1578 | 126 | 373.0470 | 0.00 |
| 4 | K-shortest frontier | 6312 | 1617 | 126 | 373.6376 | 0.00 |
| 5 | Beam/A* frontier | 6312 | 1619 | 126 | 373.6423 | 0.00 |
| 6 | Order optimization | 6312 | 1625 | 126 | 373.6433 | 0.00 |
| 7 | Portfolio local search | 6312 | 1751 | 124 | 373.3795 | 0.00 |
| 8 | Protocol-aware frontier | 6312 | 1809 | 124 | 373.4017 | 0.00 |
| 9 | Gas compression | 6312 | 1977 | 124 | 373.4317 | 0.00 |
| 10 | Bellman-Ford injection | 6312 | 1991 | 0 | 373.5139 | 0.00 |

The best mean bps on the fresh 10k rerun was iteration 6, order optimization, at +373.6433 bps versus Bellman-Ford. The final iteration had slightly lower mean bps than iteration 6 but had the highest win count and removed all measured losses versus Bellman-Ford. That is a useful tradeoff distinction: one can optimize mean bps, win count, or "never lose" behavior, and they are not always identical.

## Metric Interpretation

Mean bps can be dominated by a few large improvements. This is why iteration 6 can have the best mean bps while iteration 10 has more wins and fewer losses. For production routing, the right choice depends on risk appetite. If the route selector truly keeps the best of all candidates, the "0 losses" property is valuable. But if runtime forces pruning, we need to be explicit about which property we optimize.

Wins and losses are more robust than median bps for this dataset. Since the median is always 0, a strategy can improve hundreds of trades while median bps does not move. Any chart should show at least mean bps, win count, loss count, and coverage.

Comparing versus Bellman-Ford and comparing versus `split` answer different questions. Versus Bellman-Ford, improvements look large because `split` already beats BF substantially. Versus `split`, the final hidden holdout result of +1.3838 mean bps is the cleaner measure of the new candidate's incremental value over the existing stronger baseline.

The fresh 10k trade-level distributions are highly zero-inflated with a fat positive tail. For the final candidate versus Bellman-Ford on pairwise common-success trades, 4310 of 6308 trades were exactly unchanged, 1998 improved, and 0 lost. The pairwise per-trade mean was +441.0532 bps, while the median stayed 0 and p99 was about +1560 bps. This explains why median bps remains flat even when the algorithm materially improves many trades.

Histogram-style charts are useful here, but they need signed and log-sized bps buckets. A plain linear histogram is dominated by exact-zero trades and hides the long positive tail. The final HTML report now includes a single overlaid all-iteration histogram, a detailed final-candidate histogram, a histogram matrix across all preserved iterations, and a final-run baseline histogram matrix.

## Runtime Measurements

The original fresh 10k quality reruns did not measure per-solve runtime. The benchmark JSON and logs reported coverage, wins, losses, mean bps, median bps, and net amounts, but did not instrument solver latency.

Runtime was measured afterwards on a smaller fixed subset: the first 100 requests from the fresh 10k sample. For each algorithm version, the compiled release `fynd-benchmark quality` binary was run once with an empty request file and once with the 100-request file. Solve-only time was estimated as `sample_real_s - empty_real_s`, so process startup, snapshot loading, derived-data preparation, and graph construction are subtracted as a first-order approximation. This is not a production latency percentile measurement, but it is enough to compare the relative cost of the candidate families. All rows below solved 66 of the 100 timing requests.

Iteration runtime on the 100-request subset:

| Iteration | Candidate | Solve ms/request | Solve ms/solved |
|---:|---|---:|---:|
| 0 | Unmodified Fynd path_frank_wolfe | 4.8 | 7.3 |
| 1 | Penumbra / fill-and-spill | 35.4 | 53.6 |
| 2 | Convex flow | 73.8 | 111.8 |
| 3 | Frank-Wolfe refinement | 74.6 | 113.0 |
| 4 | K-shortest frontier | 119.8 | 181.5 |
| 5 | Beam/A* frontier | 146.2 | 221.5 |
| 6 | Order optimization | 155.6 | 235.8 |
| 7 | Portfolio local search | 294.9 | 446.8 |
| 8 | Protocol-aware frontier | 260.1 | 394.1 |
| 9 | Gas compression | 188.8 | 286.1 |
| 10 | Bellman-Ford injection | 267.7 | 405.6 |

Baseline runtime on the same subset:

| Algorithm | Solve ms/request | Solve ms/solved |
|---|---:|---:|
| MostLiquid | 12.2 | 18.5 |
| Bellman-Ford | 12.4 | 18.8 |
| Split | 8.4 | 12.7 |
| Final candidate | 260.9 | 395.3 |

The timing result changes the production interpretation. The final candidate is much better as a quality oracle than as an obvious low-latency drop-in replacement. It is roughly 21x slower than Bellman-Ford and 31x slower than `split` on this measurement. The likely production path is therefore not "ship every layer for every quote," but use the full candidate as an oracle for ablation, then gate expensive layers by trade size, price impact, near-tie conditions, and available latency budget.

## LLM Evolution Loop Lessons

Fresh sessions are useful when the loop writes durable context. Each iteration needs the prior progress, benchmark result, notes, and patch. Otherwise fresh agents rediscover the same ideas or repeat failed approaches. The `public/learnings.md` file was essential.

The holdout split was necessary. The agents saw at least a 1000-trade exposed sample while building, but the 2500-trade hidden holdout remained out of prompt/sandbox access. That gave a cleaner final score and reduced the risk that the agents simply overfit a visible benchmark.

The exposed sample size matters. Early tests with tiny samples were too noisy and too easy to overfit. The user correctly pushed for at least 1000 exposed trades during building and at least 2500 holdout trades. That made the loop more expensive but much more credible.

The loop should preserve every branch/commit. It is not enough to keep only the final file. The useful knowledge came from comparing iterations, rerunning old commits on a fresh sample, and seeing which changes moved mean bps, wins, losses, and coverage. Local commits and patches per iteration made that possible.

For bold exploration, the prompt matters. Asking agents to "improve incrementally" tends to produce small tuning changes. Asking for different approaches, such as Penumbra fill-and-spill, convex network flow, Frank-Wolfe refinement, K-shortest frontiers, and BF path injection, broadened the search space and produced the main wins.

Programmatic Codex/Claude CLI runs are feasible, but model and effort settings matter for these tasks. The loop exposed `--agent-command`, `--codex-model`, and `--codex-effort`, and Claude could be invoked with an Opus/max-style command. The orchestration script should keep model, effort, prompt, logs, and exact commit in the artifact directory so results remain auditable.

Parallel reruns help. The final fresh 10k rerun was slow, especially portfolio/local-search-heavy iterations. Running later commits in separate worktrees in parallel saved wall-clock time, but it required careful result-file validation so placeholders were not mistaken for real benchmark JSON.

## Engineering Gotchas

Do not report benchmark wins from routes that are not executable. The offline harness trusts algorithm-provided per-leg outputs in some places, so the algorithm must be disciplined. The final candidate avoids double-counting by sequentially re-simulating shared-pool routes and computing net from the assembled swaps.

Gas must be in the objective. A route with more gross output can be worse after gas. Every candidate route needs to be evaluated by `gross - gas_cost_in_output_token`. Route compression existed because some shared-pool routes were gross-improving but gas-worse until redundant same-pool swaps were merged.

Candidate path ranking must not drop unscored paths too aggressively. Earlier notes showed that pools with missing derived spot/depth can still simulate correctly. Dropping them can hand wins to Bellman-Ford and reduce coverage.

Shared-pool route assembly is order-sensitive. A path portfolio is not fully specified by allocations alone. If two legs reuse a pool, the route's output can depend on which leg is executed first. The final algorithm treats order as part of the optimization for relevant cases.

Runtime is now a real product concern. The final candidate is excellent as a research algorithm and benchmark oracle, but several layers add simulations: piecewise curves, K-shortest/beam frontiers, local search, best-order simulation, protocol-specialized routes, and compression. Productionizing this likely requires ablation, caching, gating, and latency budgets.

`cargo fmt` may churn unrelated code in this repo because installed nightly rustfmt differs from the committed formatting style. The campaign notes explicitly warned not to blindly run `cargo fmt` in that worktree. This should be normalized before upstreaming large algorithm changes.

The current hidden holdout split manifest stores a long list of indices. That is useful for reproducibility but noisy in direct CLI output. Summary commands should print only `{source_total, exposed_size, holdout_size, seed}` unless the exact indices are needed.

## What We Learned About Routing

The routing objective is closer to a constrained portfolio optimization problem than a shortest-path problem. A route can be a tree or portfolio of paths, not just a path. The hard parts are path discovery, split allocation, gas, shared state, and execution order.

Most of the easy quality gain comes from splitting. The transition from single-path or PFW-style routing to shared-pool fill-and-spill produced the biggest jump. Later optimizers mostly polished edge cases.

Candidate diversity is valuable. Pure "best full-output route" selection can over-index on near-duplicates. Useful liquidity can sit behind lower-ranked paths, different protocols, or paths that are only good for the first small slice of flow.

Approximate optimization should be treated as candidate generation, not truth. Convex piecewise flow, beam bounds, K-shortest scores, and protocol heuristics are all useful ways to suggest candidates. The final truth must be exact simulation of an executable route.

Never-lose fallbacks are powerful. Keeping best single path, split replica, and BF-injected path candidates lets experimental techniques be aggressive without forcing risk onto every trade. The final selector can be bold because the final route is chosen by net output.

The residual gap after allocation improvements was path discovery. The repeated "7 Bellman-Ford-only wins" pattern was the clue. Local search, order search, and gas compression did not solve it. BF path injection did.

## What We Learned About Agentic Optimization

Agents are good at exploring multiple algorithmic families when the harness is deterministic and feedback is fast enough. The loop produced useful ideas across fill-and-spill, convex flow, Frank-Wolfe, K-shortest, beam search, local search, protocol-aware selection, gas compression, and BF injection.

Agents need explicit permission to explore bold alternatives. Without that, they tend to tune constants or add small refinements. The strongest gains came after prompting for different approaches rather than local incremental improvement.

Agents need an honest scoreboard. A single exposed benchmark is not enough. The process became much more credible after adding a hidden holdout and then rerunning all preserved versions on a fresh 10k sample.

Agent notes are not optional. The persistent `learnings.md` prevented the loop from repeatedly trying dominated standalone convex flow, and it preserved the diagnosis that remaining BF wins were path-search misses rather than allocation misses.

## Productionization Recommendations

Treat the current final `agent_candidate` as a research oracle and ablation source, not as a production-ready low-latency router without further work. Its quality is good, but the cost of evaluating every layer on every request is high.

Run ablations on the fresh 10k sample and the hidden holdout. Measure each layer's marginal contribution to mean bps, wins, losses, coverage, and wall-clock latency. The likely production stack is a gated subset, not the full research stack.

Promote the guaranteed-safe pieces first: best single path, faithful split replica, shared-pool sequential simulation, fill-and-spill, BF path injection, and gas compression. These have clear conceptual value and safety stories.

Gate expensive layers. Beam/K-shortest frontiers, protocol-aware routes, order search, and portfolio local search should probably run only on larger trades, near-ties, high-price-impact trades, or when cheap heuristics indicate that split routing might matter.

Add per-route diagnostics to the benchmark output. For each win, record which route family won, whether compression was used, number of paths, shared-pool count, gas saved, protocols used, and execution-order strategy. This would make future ablation and productization much easier.

Add latency reporting to the offline harness. Quality alone is not enough. The ad hoc 100-request timing subset showed that the final candidate is expensive, but the benchmark itself should emit per-solve latency, warm/cold breakdowns, and percentiles so every future quality result also carries a viability signal for Fynd's 10 to 50 ms positioning.

Refresh the snapshot story. To test production relevance, capture multiple snapshots across time and broader protocol sets. VM-backed pools need a serialization strategy or separate live-sim benchmark lane.

Keep the holdout discipline. Future LLM runs should continue to use exposed samples for iteration and hidden holdouts for scoring. The holdout should not be in the agent-accessible worktree or prompt context.

## Open Questions

Which subset of the final candidate gives the best quality-per-millisecond? The fresh rerun shows quality, but the production decision needs latency and CPU cost.

How much of the final mean-bps improvement comes from a small number of very large trades? We need bucketed analysis by USD size, hop count, protocol family, and input/output pair.

Does gas compression remain valid across all protocol families we want to support? It is safe for same-pool same-direction native AMM interactions under the current assumptions, but broader VM-like pools should be treated carefully.

Can shared contiguous path segments be compressed, not just duplicate single-pool edges? The notes suggest trie-style prefix/suffix merging as a next step.

Can Bellman-Ford injection be made cheaper? It solved residual losses, but production needs a cheap path-rescue lane rather than another expensive full search.

Can the convex network-flow idea be made graph-native instead of path-native? The current convex-flow approximation works over candidate paths. A true graph-level convex network-flow or dual-decomposition approach could be stronger but is more complex.

## Bottom Line

The main technical learning is that high-quality DEX aggregation needs executable split-route portfolio optimization with honest shared-pool state simulation. The main process learning is that LLMs can help discover and stack these ideas if the benchmark is deterministic, the holdout is hidden, every iteration is preserved, and every result is rerun on a fresh sample before drawing conclusions.

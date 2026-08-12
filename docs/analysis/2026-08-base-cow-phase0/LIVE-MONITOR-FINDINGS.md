# Live APEX/Fynd monitor — investigation log

Findings from moving the live batching monitor to Hetzner and chasing its search-deadline problem
to a root cause. Companion to [`deploy/README.md`](deploy/README.md) (how to run it) and
[`RESULTS.md`](RESULTS.md) (what the data says). Session branch: `mp/feat/apex-live-monitor`.

## Timeline summary

| Date | Event |
|---|---|
| 2026-08-07 | Enriched recording (APEX clearing prices, full Fynd quote detail); git-dep swap for `apex-solver` (rev `c1e049d`, `fix(algorithm): clear at best-so-far prices when the deadline fires`) |
| 2026-08-07 | Deployed to Hetzner (`agent@100.116.92.13`) as systemd `apex-monitor`, with a watchdog timer and a daily disk-guard/compaction timer |
| 2026-08-07 | Diagnosed and fixed the search-deadline problem (below) — root cause: `aerodrome_slipstreams`/`lunarbase` |
| 2026-08-07 20:36:27 UTC | Clean-slate restart on the final config; everything before is archived at `/home/agent/apex-archive/20260807T203433Z-pre-final-config/` on the box |
| 2026-08-09 08:26 UTC | Watchdog caught one genuine production stall (feed wedged, process alive) — see "Failure resistance" below |
| 2026-08-10/11 | Found and helped verify a real correctness bug in `tycho-simulation`'s `uniswap_v3`/`aerodrome_slipstreams` target-price math via a peer session (below) |
| 2026-08-12 07:59:21 UTC | Monitor stopped by request. `apex-monitor.service`, `apex-monitor-health.timer`, `apex-monitor-compact.timer` all stopped and disabled on the box. **4.4742 days total run, 383,309 comparisons joined** — final numbers in [`RESULTS.md`](RESULTS.md) |

## The search-deadline problem

At deploy time, ~80% of APEX components hit their 1.5s search deadline before converging —
`cluster_cut`/unfilled orders dominated, and clearing prices for the deadline-fired majority were
best-so-far (unconverged) rather than final.

### What did NOT fix it (all measured, in order tried)

| Lever | Result |
|---|---|
| Budget 1.5s → 3s | Deadline rate 80% → 79%. No effect — cost is per-iteration, not per-search-window. |
| Budget → 20s | Deadline rate down to ~22%, but `w30` median solve reached 32s — not viable for a 2s-block chain. |
| APEX worker parallelism (`max_workers` 1→4, stage workers 6→4) | `effective_parallelism_avg` 1.0→1.87–2.15 (not 4× — capped by a single global `Mutex<SupplyCache>` in `apex-solver/src/market/router.rs:89`). Iterations 39→51–69. Real but a small constant factor. |
| Turbine's mixed step-strategy price search (`[AllTokens, Top(2), Top(1)]`, `max_iterations=3000`, `max_it_at_min_step=10`) | Deadline rate 80%→49% at a 3s budget. Real, adopted, but still not enough on its own. |
| `--min-tvl 50` (245→201 pools/component) | Deadline rate 65%→48-51%. Real (pool count is genuinely superlinear in solve cost), but treating the symptom, not the cause. |
| `tolerance: 0.002` instead of `0.0` in the Brent-solver early exit | **Did not fix it** — 91% still deadline-fired at `min-tvl 10` with slipstreams present. Ruled out as *the* fix (see root cause). |

### Root cause: `aerodrome_slipstreams` and `lunarbase` have no analytic `query_pool_swap`

In `tycho-simulation`, `uniswap_v2`/`v3`/`v4` and `aerodrome_v1` implement `ProtocolSim::query_pool_swap`
analytically (closed-form sqrt-price-target math, one call). `aerodrome_slipstreams` and `lunarbase`
have no override and fall through to the generic trait-default implementation — a history-based
Brent's-method search that calls `get_amount_out` (a full swap simulation + state clone) up to
`MAX_ITERATIONS = 30` times per query. `tools/apex-batch/src/adapter.rs` additionally passed
`tolerance: 0.0`, making the search's early exit mathematically unreachable (`is_within_tolerance`
requires exact f64 equality at `tolerance=0.0`), so every query ran the full 30 iterations.

**A/B proof** (live feed, identical config except `--protocols`):

| | with slipstreams (389 pools) | without (302 pools) |
|---|---|---|
| components hitting deadline | 48–51% | **2%** |
| converged (`NoImprovement`) exits | 8% | **76%** |
| ms per supply call | 1.84 | **0.16** |

Supply calls went *up* while wall time went *down* 6.7× — proof this is a per-call cost effect,
not a pool-count effect. `lunarbase` is a single pool at our TVL floor and contributes nothing
measurable today; excluded anyway so a future TVL change can't silently reintroduce a
generic-Brent pool.

### The fix, deployed

`--protocols uniswap_v2,uniswap_v3,uniswap_v4,pancakeswap_v3,aerodrome_v1,native_wrapper`
(explicit list, replacing `native_onchain`'s auto-expansion) + `--min-tvl 10` restored (the TVL
floor was only needed to route around the slipstreams cost; the real market is available again)
+ `--apex-workers 4` (parallelism, still worth the constant factor) + `--apex-budget-ms 3000`.

Measured after deploy: deadline rate settled at **3–5%**, 67–80% of exits are `NoImprovement`
(converged), ms/supply-call ~0.19–0.22.

### Recording fixes this investigation required

- **Per-component `deadline_fired`, `orders_in`, `pools_in_scope`** (`tools/apex-batch/src/live.rs`)
  — the bracket-level counter couldn't say *which* component in a multi-component job hit the
  deadline, which blocked exactly the split-by-convergence analysis needed to diagnose this.
- **`SolveMetrics` recorded per component** (`supply_calls`, `supply_wall_ms`,
  `effective_parallelism_avg`, `pool_builds`, `workers`, cache hit/miss) — APEX returns these
  unconditionally; we were discarding them.
- **Taxonomy fix: `ClusterCut` → `UnfilledAtBestSoFar` / `NotPriced`.** Every non-filling order in
  a deadline-fired solve used to be labelled `ClusterCut` ("never evaluated"). Wrong: all observed
  APEX calls produced exactly one trading cluster, and the between-cluster skip path (the only way
  a cluster can genuinely go unevaluated) never fired once. A deadline firing mid-cluster still
  clears at best-so-far prices and returns priced tokens — those orders were evaluated and simply
  didn't cross. Split into three honest states:
  - `UnfilledAtLimit` — priced, converged, didn't cross.
  - `UnfilledAtBestSoFar` — priced, deadline fired mid-solve, didn't cross (provisional rejection).
  - `NotPriced` — never priced at all (dropped by APEX's own `TokenClusterSolver` when its tokens
    land in no cluster — `token_cluster.rs:217` — independent of the deadline).
  This mislabelling had inflated the "APEX considered and rejected this order" denominator by
  conflating it with "APEX never looked at this order." Not backported to the offline `stage2`/
  `stage3` runners (`tools/apex-batch/src/bin/`) — deliberately, since their results are already
  published and changing the taxonomy would break comparison against them.
- **The internalization artifact this explains.** Before the fix, pooled internalization read
  ~0.94 — misleadingly high. Split by convergence (main run, window-1, matched pool-count ~280):
  converged components internalize **0.70**, deadline-fired ones **0.999–1.000** (in one probe,
  `pool_cleared_wei` was *exactly* zero across 40 deadline-fired components). An unconverged
  search cannot price pool routes properly, so what it clears looks like pure order-against-order
  crossing. Mostly resolved now that the deadline rate is 3–5% instead of 50–80%, but **always
  report internalization split by `deadline_fired`, never pooled**, when working with older data.

## `live_join.py` fixes (the offline analysis join)

- **`.zst` glob bug.** The disk guard's daily compaction rotates closed days to
  `<prefix>-YYYY-MM-DD.jsonl.zst`; the join only globbed `*.jsonl` and read plain files. Failed
  silently — no error, just fewer records. Measured impact: 25,158 of 213,052 comparisons (12%)
  on a 3.5-day collection. Fixed by decompressing `.zst` files transparently via the `zstd` CLI.
- **Dust floor was $100, should be $1.** `bps()` divides by the raw baseline amount with only a
  `<= 0` guard; a fill settling for a few raw token units (one observed case: $0.000002 notional)
  makes the ratio explode, and `mean_bps` in particular was reading in the millions. Copied
  `hindsight::telemetry::MIN_NOTIONAL_USD` (=$100) as the fix initially — wrong for this order
  population, whose **median fill is ~$15**; $100 discarded the median trade and 74% of all fills
  as "dust." Recalibrated to **$1**, which removes true near-zero/rounding outliers (~15% of
  fills) without cutting the small-but-real long-tail flow this study measures. The unweighted
  stats (median/mean/win-rate) are gated by this floor; the USD-weighted sums (`net_surplus_usd`,
  `gross_surplus_usd`) are not — every fill still counts toward total surplus. `dust_excluded`
  reports the count per bucket so the exclusion isn't silent.

## Cross-session collaboration

### `fynd-api-breaking-perf` — fynd-core solve-time perf, PR #389

Asked (by a teammate, relayed through this session) whether any performance idea for fynd's
public API had required a breaking change. Found via git archaeology:

- **PR #389** (`mp/perf/arc-components-tokens`, open/unmerged): Arc-shares `MarketState`'s
  `simulation_states` and `ProtocolComponent`s in `extract_subset` instead of deep-copying per
  solve. **No API break** — `get_component`/`get_token`/`token_registry_ref` keep their exact
  signatures. Measured: offline solve time mean **-15.5%** (p50 -12.5%, p90/p95 -18.2%), per
  algorithm bellman_ford -12.7%/path_frank_wolfe -11.8%/water_fill -2.4%/most_liquid -2.9%;
  byte-identical output on all 4 algorithms; live concurrent 60-block paired A/B mean delta -5.1%
  (95% CI [-9.8, -0.4], p=0.0013).
  - **The API-breaking variant, tried and rejected with data.** Two throwaway branches
    (`bench-tokens-plain`, `tokens-plain-variant`, never merged) also Arc-share `Token`, which
    would change `token_registry_ref`'s return type. Measured at **0-4% (noise)** per algorithm —
    `Token` is only two heap allocations and candidate sets hold few unique tokens, so sharing it
    saved nothing measurable while forcing every caller naming the map's value type to change.
    Documented in PR #389's own "considered and rejected" section. Do not re-attempt without new
    data.

### `tycho-aerodrome` — a real correctness bug in production `tycho-simulation`

Investigating whether `aerodrome_slipstreams` could get an analytic `query_pool_swap` (the
long-term fix for the root cause above), found a peer session (`tycho-aerodrome`) already on
exactly this: branch `mp/feat/slipstreams-swap-to-price` → `mp/feat/slipstreams-verify-tests`.

Their own added test (`test_pool_target_price_closed_form_matches_generic_search`) **self-documents
as failing**, diagnosing a **double fee-markup**: `get_sqrt_price_limit` marks the target price up
by `1/(1-fee)` converting it to a raw sqrt-price limit, and `spot_price()` marks the *result* up by
`1/(1-fee)` again when reporting it — the closed form lands on `target/(1-fee)²`, not `target`.
Measured for `aerodrome_slipstreams`/`velodrome_slipstreams`: amount_in off by ~4000×/~2×
respectively. Their docstring claimed this is pre-existing in shared `clmm.rs`, "also present in
`uniswap_v3`/`uniswap_v4`, not introduced by this branch" — **we did not pin fynd to this WIP
branch.**

**Independently verified against fynd's actual pinned `tycho-simulation` (0.345.1, not the WIP
branch)**: `get_sqrt_price_limit` (`sqrt_price_math.rs:232-262`) and `uniswap_v3::spot_price`
(`state.rs:264-271`, `add_fee_markup(price, self.fee())`) compose the same way in the released
version. Built a throwaway test reusing their exact WBTC/WETH multi-tick fixture as a
`UniswapV3State`:

```
WBTC->WETH x0.999: closed_form_in=131823  generic_in=524734497  relative_diff=3979.60  landed_error_bps=10.01
```

**Identical numbers to their `aerodrome_slipstreams` measurement** — same pool geometry, same fee
tier, same shared code path, so a byte-identical outcome is expected, and it's real-world
confirmation on a released version, not just a structural pattern match on unreleased code.

**The non-obvious part worth remembering:** the *landed price* is only ~10bps off target (small,
since `(1-fee)²` vs `(1-fee)` is a tiny delta at low fee tiers) while `amount_in` is ~4000× wrong —
composed with thin local liquidity at the crossed tick. A price-only consumer of this API would
never notice; **APEX's `register_supply` reads `amount_in`/`amount_out` directly**, so this
directly corrupted supply-curve construction for every `uniswap_v3` pool (472 of our ~940-pool
universe) for as long as we've been running on 0.345.1 — this is *separate* from, and larger in
scope than, the slipstreams exclusion above.

Fix in progress upstream: `mp/fix/clmm-swap-to-price-fee-markup` (not yet a PR as of this
writing). Findings shared with and confirmed useful by the `tycho-aerodrome` session, cited with
attribution in their upcoming PR description.

**Action item, not yet done:** once that fix lands and releases, bump fynd's `tycho-simulation`
pin (`Cargo.toml:146`, currently `>=0.345.1`), rebuild, redeploy. At that point re-including
`aerodrome_slipstreams`/`lunarbase` in `--protocols` also becomes viable again (the analytic
`query_pool_swap` removes the original performance reason they were excluded) — worth
reconsidering once both fixes are in.

## Failure resistance, validated live

- **Watchdog caught a real stall**: 2026-08-09 08:26:23 UTC, "apex-monitor applied no block for
  630s (blocks=64035); restarting." The process had not crashed — the feed wedged and it sat idle,
  exactly the failure mode (websocket closes, process stays alive) that `Restart=always` cannot
  see. It needed `SIGKILL` (ignored `SIGTERM` for ~90s before the watchdog's kill), which is why
  the apex-stream writer was changed to flush every line rather than rely on the 8KB `BufWriter`
  default — a `SIGKILL`'d process loses whatever sat in that buffer.
- Zero jobs shed since the final config deployed. Head lag consistently ~0.01 blocks mean.
- Two deliberate restart tests (kill -9, simulated stall) both recovered within their expected
  windows during initial verification.
- **Final shutdown confirmed the same behavior.** `systemctl stop` sent `SIGTERM`; the process did
  not exit within systemd's timeout and was `SIGKILL`'d — the identical pattern seen during the
  2026-08-09 stall. No data was lost: both JSONL streams' final lines parse cleanly, which is the
  direct payoff of the per-line-flush fix made earlier (a `SIGKILL`'d process loses whatever sat
  in an unflushed buffer, and the default `BufWriter` would have lost up to 8KB here).

## Known open items

- `batch_vs_singles`'s `mean_bps` is still statistically unstable — different root cause from the
  dust-floor fix above: the singles-control's own fill amount (the denominator) can legitimately
  be near-zero on a real-sized order (that's what "this order finds almost no liquidity alone"
  looks like), so the $1 floor (which gates on the *order's* USD value) doesn't constrain it. Use
  `median_bps`/`apex_wins_share` for this comparison; don't quote its mean.
- `NotPriced` taxonomy fix not backported to `stage2.rs`/`stage3.rs` (see above) — intentional, but
  means offline and live results use different label semantics for the same underlying event.
- Throwaway Allium API key (from an earlier phase of this study) still needs rotation.
- `tycho-simulation` fee-markup fix: see action item above.

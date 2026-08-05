# Stage 3 — RESUME (written 2026-08-05, pre-restart)

State at handoff: **no stage-3 results exist yet.** Two live captures died with the session
before their sweeps ran (their snapshots survived); the first offline sweep was killed stuck
(singleton-component blowup, since fixed in f4242f05); the relaunched lean sweep had finished
its quote pass only when the session restart cut it down. All code fixes are committed and
pushed; the sweep just needs to be run again.

## Relaunch sequence (all via the harness background runner — never detached, never piped)

1. **Sweep #1 — old snapshot, block 49,562,643** (legacy provenance caveats below):

   ```bash
   cargo build --release -p apex-batch
   ./target/release/stage3 \
     --snapshot /Users/pistomat/Projects/propeller-heads/fynd/data/apex-snapshots/base-native-49562643.json.zst \
     --days 2026-08-03 --days 2026-08-02 \
     --data-dir /Users/pistomat/Projects/propeller-heads/fynd/data/hindsight/base-comparisons
   ```

   Writes `stage3_results-49562643.json` into this directory. The lean sweep's per-cell pace
   was unmeasured at cutoff — arm a Monitor on `^w=` lines; if the first pools cell exceeds
   ~10 min, cut `--limit-bps 100` (drops to 20 cells).

2. **Fresh capture with the fixed binary** (records real clone/subset provenance + full token
   identity; adaptive fynd-quote coverage — probes 100 quotes, then densest even stride within
   a 20-min budget, full coverage if it fits). Needs `TYCHO_API_KEY` (+ optional `RPC_URL`) in
   the env — if unavailable, the binary fails fast with the exact user command to run via `!`:

   ```bash
   ./target/release/stage3 \
     --days 2026-08-03 --days 2026-08-02 \
     --data-dir /Users/pistomat/Projects/propeller-heads/fynd/data/hindsight/base-comparisons \
     --snapshot-out /Users/pistomat/Projects/propeller-heads/fynd/data/apex-snapshots
   ```

   This runs capture AND sweep #2 in one invocation (results file named by the new block
   label). Solver build takes minutes and is silent — that is normal. Report the
   clone_box_ms / arc_from_ms / subset_dropped_by_cap numbers prominently: they are the
   shadow-run sizing measurements (build-order item 1).

3. **README** (this directory): both blocks' cell tables side by side, labeled by block —
   stage3-vs-stage2 matched/surplus, internalization_share, drift cost = Original − Current
   per window, apex-vs-fynd on both anchors, solve-time p50/p90/max — plus the caveats below.
   Commit + push results JSON + README.

## Caveats to carry into the README

- **Old snapshot (49562643) is legacy-format**: clone/subset provenance unrecorded (zeros),
  token tax/gas/quality reload as neutral defaults (explicit compat path, f5c9bf59); its fynd
  quotes are a 6.7% even-stride sample (2,436 of 36,538) — cell-b fynd comparisons on it are
  sample-scoped.
- **Serializable-only scope**: from-disk sweeps see 122 native pools; the 108 uniswap_v4
  components are manifest-listed as dropped. A live run does the full-vs-serializable delta
  cells itself.
- **Singleton-skip pacing fix (f4242f05)**: singleton components skip the APEX solve in BOTH
  cells; their pool routing is measured by the pool-implied quote pass (19,972/36,538 orders
  quoted at 49562643, ~7 s). Batch-vs-singles attribution stays with the stage-2 control.
- **Offline deadline is 3 s** (was 10 s) — quality-ceiling framing no longer applies to these
  cells; they are budget-realistic.
- **Stage 3 is drift-contaminated by design** (orders up to ~2 days old vs snapshot state) —
  integration + mechanism milestone; clean surplus numbers are stage 4.

## Ownership note

`tools/apex-batch/src/bin/stage3.rs` and `docs/analysis/**` were this agent's files; the
apex-wire agent concurrently owns `tools/hindsight/**` and apex-batch lib files — check
`git status` before committing and never commit their in-flight changes.

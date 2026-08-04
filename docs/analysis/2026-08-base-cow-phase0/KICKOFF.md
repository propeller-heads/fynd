# Phase 1 Kickoff — Live APEX Monitor on Base

Entry point for the implementation session. Read this first, then PLAN.md.

## Where everything lives

- **PLAN.md** (this directory) — full plan + decisions log. **The "PHASE 1 v2" section plus the
  v2.1, v2.2, and v2.3 amendments are authoritative**; everything above them is history kept for
  context. Do not implement from the "PHASE 1 REPLAN" (v1) section.
- **grill-round1/** — adversarial review trail (critic_raw.md, grilling_log.md,
  scaffolding-deferred.md). Explains *why* v2 looks the way it does: 22 findings, 3 critical,
  all criticals rooted in APEX API facts inherited from turbine instead of verified against
  apex-solver.
- **Scaffold map**:
  - `tools/apex-batch/` — offline batch-replay crate (input builder is the shared lib the live
    stage must call — no duplicate path).
  - `tools/hindsight/src/capture.rs` — block-batch capture scaffolding.
  - min_amount_out / order-limit plumbing threaded through hindsight (commit b3fcb3a8).

## Build order

0. **Grill round 2 on the v2 plan** (blocking, per grilling_log verdict). Verify:
   - LimitOrder construction — limit-price direction + Fraction 18-dec scaling,
   - pre-advance `clone_box()` cost claim,
   - recording-v2 replay math,
   - pacing / degradation ladder (now backstopped by async v2.3 — check the thread-cap
     guardrail reasoning too).
1. **Adapter property tests** — direct-vs-adapter ProtocolSim agreement; de-`#[ignore]` the
   existing ignored tests.
2. **≥2-connected-trades/day pre-check** from `~/Projects/propeller-heads/fynd/data/hindsight/base-comparisons/`
   (10-minute script). Go/no-go gate for live single-block value claims.
3. **Shadow run** — apex solve time, clone cost, recording bytes/day, `is_partial` check on the
   Base stream (hard pre-capture gate).
4. **0x Settler floor extractor** — lands BEFORE the live stage (dominant Base flow).
5. **Live single-block stage** — brackets both states (N−1 headline, N biased bottom), fixed
   1 s budget per solve, async off the critical path (v2.3).
6. **Capture format v2** — hourly append-only segments + periodic checkpoints.
7. **Offline window sweeps** — windows {1, 6, 30, 150} blocks, solved at window-start and
   window-end states; w1 offline shares the v4 handicap with the other cells.

## Environment

- Tycho: `TYCHO_API_KEY` + endpoint `tycho-base-beta.propellerheads.xyz` (the default fynd
  endpoint hides VM protocols; beta serves them with the same key).
- `BASE_RPC_URL` for on-chain reads.
- The secrets-guard hook blocks env-var probing from the agent — ask the user to run checks
  via the `!` prefix (e.g. `! echo ${TYCHO_API_KEY:+set}`).
- **apex-solver is a machine-local path dependency.** Swap to a git dep with `[patch]` for
  local dev before anything merges toward deploy (tracked plan item v2 #15).
- Local 10-day dataset: `~/Projects/propeller-heads/fynd/data/hindsight/base-comparisons/`.

## Known traps

- **Verify every APEX API claim against apex-solver source, not turbine.** All three critical
  grill findings came from turbine-inherited assumptions.
- APEX orders are **LimitOrders, not MarketOrders** (MarketOrders are supply-side; the
  limit-order map forms solve clusters — apex algorithm/mod.rs:241).
- Pool states are `Box<dyn ProtocolSim>` replaced wholesale on `advance()` — **clone the
  filtered subset before advancing**; no borrow survives the step.
- `UniswapV4State` is non-serializable only because of `hook: Option<Box<dyn HookHandler>>`
  (verified in tycho-simulation 0.345.1). Hookless pools are plain CLMM; the upstream
  serialization ask is small — frame it that way.
- Sandbox E2BIG: the Bash sandbox profile grows with registered git worktrees; if sandboxed
  commands fail at spawn with E2BIG, prune stale worktrees (`git worktree remove` /
  `git worktree prune`, then restart Claude Code) or bypass the sandbox for the session.

# Slippage Features — Handover

Status of the slippage decay collection + analysis system as of the 213h
exploration (ENG-5986 / 5993 and related). For system design see
[ARCHITECTURE.md](ARCHITECTURE.md); this doc covers **current state, how to
re-run the pipeline, findings, and gotchas**.

> Note: [NEXT_STEPS.md](NEXT_STEPS.md) is the original plan. Its `assemble`
> command is **superseded** — the Rust `assemble` binary OOMs at >1M files;
> use `notebooks/assemble_polars.py` instead (see below).

## What this delivers

A prospective system that captures live Fynd quotes and resimulates each route
at blocks +1..+10 to measure **route decay** (how much the quoted output
degrades before execution), decomposed into **market movement** (unavoidable)
vs **execution slippage** (reducible by Fynd). The goal is to predict and
reduce transaction reverts.

Output of the latest run: an EDA over 213h / 1.74M quotes
(`notebooks/04_213h_eda.html`) with findings below.

## Pipeline — how to re-run

All commands from repo root. Python steps use `uv run python3` (deps: polars,
pyarrow, matplotlib, seaborn, scipy, jupytext, nbconvert).

### 1. Collect (live)

```bash
TYCHO_API_KEY=... RPC_URL=... ./tools/slippage-features/run_collection.sh
```

Runs dual-Fynd (primary on :3000 with `slippage-features`, re-quote instance on
:3001) + the quote driver (randomized 1k trades every 5 min from
`trades_10k.json`). Writes per-quote parquets to `slippage-data/`
(`quote_log_*`, `hop_decay/`, `hop_static/`, `tycho_route_decay/`). Auto-restarts
on crash; data on disk is safe across restarts. Stop it by killing the script +
the two `fynd serve` processes.

### 2. Consolidate hop files

```bash
uv run python3 tools/slippage-features/notebooks/consolidate_hops.py
```

Folds the ~1.7M tiny per-quote hop parquets into
`slippage-data/hop_consolidated/{hop_decay,hop_static}.parquet` so downstream
steps load in seconds instead of ~50 min. One-time per dataset.

### 3. Assemble the unified dataset

```bash
# With CoinGecko enrichment (fetches uncached tokens, ~2.5s/token, cached to disk):
COINGECKO_API_KEY=... uv run python3 tools/slippage-features/notebooks/assemble_polars.py
# Or cache-only (fast, uncached tokens -> long_tail):
uv run python3 tools/slippage-features/notebooks/assemble_polars.py --skip-coingecko
```

Produces `slippage-data/unified/chain_id=1/unified.parquet` (one row per
quote×offset, 30 feature columns). Computes per-quote features before the
~17M-row offset expansion and streams the hop-max aggregation, so peak memory
stays ~8-15 GB at 1.7M-file scale.

### 4. Run the EDA

```bash
# Quick (figures to notebooks/figures/, dynamic conclusions to stdout):
uv run python3 tools/slippage-features/notebooks/04_213h_eda.py
# Full HTML with inline figures:
uv run jupytext --to ipynb tools/slippage-features/notebooks/04_213h_eda.py -o tools/slippage-features/notebooks/04_213h_eda.ipynb
uv run jupyter nbconvert --to notebook --execute --ExecutePreprocessor.timeout=3600 tools/slippage-features/notebooks/04_213h_eda.ipynb --output 04_213h_eda.ipynb
uv run jupyter nbconvert --to html tools/slippage-features/notebooks/04_213h_eda.ipynb
```

The notebook computes its executive summary and takeaways **from the data**, so
re-running on a new dataset updates the conclusions automatically. It prefers
`hop_consolidated/` and falls back to globbing raw files.

### 5. (Optional) Ground-truth resim

The Rust `node-resim` binary replays each route via `eth_call` at historical
blocks for on-chain ground truth. **Not yet run on the 213h data.** Needs an
archive RPC node. Without it, all decay numbers are Tycho-sim estimates.

## Latest findings (213h, 1.74M quotes, ~259h block span)

- **Mean route decay: −2.08 bps raw / +1.34 winsorized** — heavy-tailed;
  38.7% of routes degrade, 22.3% improve, 39% unchanged.
- **Execution slippage now dominates: 41% market movement / 59% execution
  slippage** (reversed vs the earlier 44h run) — strengthens the case that Fynd
  can meaningfully reduce decay.
- **Tail risk: 5.9%** of routes have >20 bps decay at offset 10.
- **Decay is concave (front-loaded)** — most damage in the first 1-2 blocks;
  fast execution matters more than a wide slippage tolerance.
- **Offset-1 (next block): P95 = 7.1 bps, P99 = 21.8 bps.** A single global
  tolerance of ~22 bps caps reverts at ~1%, but per-pair tolerance is much
  better (large-meme needs ~2.6 bps, stable-large ~32 bps for the same 1%).
- Fee tier / protocol / pair type / hop count are the strongest structural
  predictors. 15 CoinGecko pair buckets.

## Gotchas / known issues

- **Memory at scale**: the old Rust `assemble` binary and naive full-file loads
  OOM at 1.7M files. Use `assemble_polars.py` + `consolidate_hops.py`; the EDA
  trims columns and downcasts to Float32 to stay lean (~2.6 GB peak).
- **CoinGecko coverage**: cached to `slippage-data/coingecko_cache.json`.
  Cache-only runs leave uncached tokens as `long_tail` (17.3% mcap-null on the
  213h set). Re-run assembly with `--coingecko-api-key` to fetch new tokens.
- **V4 fee_tier is null** (tycho-simulation doesn't expose V4 dynamic fees), so
  V4 (~55% of hops) is excluded from the fee-tier analysis.
- **`gap_to_second_best_bps` is always null** — BellmanFord uses SPFA
  relaxation, not candidate enumeration.
- **BellmanFord `max_hops` = SPFA exploration rounds, not max output hops.**
  Routes can legitimately have more swaps than `max_hops`; this is by design,
  not a bug.
- **node-resim not run** on this dataset — decay is Tycho-sim, not on-chain.

## Data location

Raw data is **not in git** (gitignored). The 213h dataset is archived to S3 as
`slippage-data-213h-complete.tar.gz` (4.6 GB, 5.25M files: raw parquets +
`unified/` + `hop_consolidated/` + quote logs + CoinGecko cache). To reproduce
the analysis, restore the archive and run steps 2-4 (or just keep `unified/` +
`hop_consolidated/`, ~1 GB, which the EDA reads directly).

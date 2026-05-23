# %% [markdown]
# # Slippage Feature EDA — Route Decay Analysis
#
# Reproducible analysis of the slippage feature collection data.
# Run the assembler first to produce the unified dataset:
#
# ```bash
# cargo run -p slippage-features --release --bin assemble -- \
#   --quote-log-dir ./slippage-data \
#   --hop-decay-dir ./slippage-data/hop_decay \
#   --tycho-route-decay-dir ./slippage-data/tycho_route_decay \
#   --route-decay-dir ./slippage-data/route_decay \
#   --output-dir ./slippage-data/unified
# ```

# %% [markdown]
# ## 0. Setup

# %%
from pathlib import Path

import polars as pl
import numpy as np
from scipy import stats

def find_workspace_root() -> Path:
    p = Path(__file__).resolve().parent if "__file__" in dir() else Path.cwd()
    while p != p.parent:
        cargo = p / "Cargo.toml"
        if cargo.exists() and "[workspace]" in cargo.read_text():
            return p
        p = p.parent
    raise FileNotFoundError("workspace root not found")

PROJECT_ROOT = find_workspace_root()
DATA_DIR = PROJECT_ROOT / "slippage-data"
UNIFIED_PATH = DATA_DIR / "unified" / "chain_id=1" / "unified.parquet"
HOP_DECAY_DIR = DATA_DIR / "hop_decay"
HOP_STATIC_DIR = DATA_DIR / "hop_static"
TYCHO_ROUTE_DIR = DATA_DIR / "tycho_route_decay"

# %% [markdown]
# ## 1. Load Data

# %%
df = pl.read_parquet(UNIFIED_PATH)
print(f"Unified: {df.shape[0]:,} rows, {df['quote_id'].n_unique():,} quotes, {df.shape[1]} columns")

blocks = df["block_number"]
span_blocks = blocks.max() - blocks.min()
span_hours = span_blocks * 12 / 3600
print(f"Block range: {blocks.min()} → {blocks.max()} ({span_blocks:,} blocks, ~{span_hours:.1f}h)")
print(f"Block offsets: {sorted(df['block_offset'].unique().to_list())}")

# %%
hd = pl.concat([pl.read_parquet(f) for f in sorted(HOP_DECAY_DIR.glob("*.parquet"))])
hs = pl.concat([pl.read_parquet(f) for f in sorted(HOP_STATIC_DIR.glob("*.parquet"))])
trd = pl.concat([pl.read_parquet(f) for f in sorted(TYCHO_ROUTE_DIR.glob("*.parquet"))])
print(f"Hop decay:  {hd.shape[0]:,} rows")
print(f"Hop static: {hs.shape[0]:,} rows")
print(f"Tycho route decay: {trd.shape[0]:,} rows")

hd_full = hd.join(hs, on=["quote_id", "solver_id", "hop_index"], how="left")

# %% [markdown]
# ## 2. Route Decay Distribution

# %%
decay = df["route_decay_bps"]
pcts = [1, 5, 10, 25, 50, 75, 90, 95, 99]

print("Route decay (bps):")
print(f"  mean={decay.mean():.2f}, median={decay.median():.2f}, std={decay.std():.2f}")
print(f"  min={decay.min():.2f}, max={decay.max():.2f}")
print("\nPercentiles:")
for p in pcts:
    print(f"  P{p:2d}: {decay.quantile(p/100):8.2f}")

neg = (decay < 0).sum()
zero = (decay == 0).sum()
pos = (decay > 0).sum()
print(f"\nImproved: {neg} ({100*neg/decay.len():.1f}%), "
      f"Unchanged: {zero} ({100*zero/decay.len():.1f}%), "
      f"Degraded: {pos} ({100*pos/decay.len():.1f}%)")

# %% [markdown]
# ## 3. Decay by Block Offset

# %%
by_offset = (df.group_by("block_offset").agg([
    pl.col("route_decay_bps").mean().alias("mean"),
    pl.col("route_decay_bps").median().alias("median"),
    pl.col("route_decay_bps").std().alias("std"),
    pl.col("route_decay_bps").quantile(0.05).alias("p05"),
    pl.col("route_decay_bps").quantile(0.95).alias("p95"),
    pl.len().alias("n"),
]).sort("block_offset"))

print(f"{'offset':>6} {'mean':>8} {'median':>8} {'std':>8} {'P5':>8} {'P95':>8} {'n':>6}")
for row in by_offset.iter_rows(named=True):
    print(f"{row['block_offset']:6d} {row['mean']:8.2f} {row['median']:8.2f} "
          f"{row['std']:8.2f} {row['p05']:8.2f} {row['p95']:8.2f} {row['n']:6d}")

# %% [markdown]
# ## 4. Market Movement vs Execution Slippage

# %%
decomp = df.filter(pl.col("market_movement_bps").is_not_null())
fill_rate = decomp.height / df.height
print(f"Decomposition fill rate: {decomp.height}/{df.height} ({100*fill_rate:.1f}%)")

if decomp.height > 0:
    mm = decomp["market_movement_bps"]
    es = decomp["execution_slippage_bps"]
    print(f"\nMarket movement:    mean={mm.mean():+.2f}, median={mm.median():+.2f}, std={mm.std():.2f}")
    print(f"Execution slippage: mean={es.mean():+.2f}, median={es.median():+.2f}, std={es.std():.2f}")

    mm_abs = mm.abs().sum()
    es_abs = es.abs().sum()
    total_abs = mm_abs + es_abs
    if total_abs > 0:
        print(f"\nFraction of total |decay|:")
        print(f"  Market movement:    {100*mm_abs/total_abs:.1f}%")
        print(f"  Execution slippage: {100*es_abs/total_abs:.1f}%")

    # Decomposition by block offset
    print(f"\n{'offset':>6} {'mm_mean':>8} {'es_mean':>8} {'mm_frac':>8}")
    decomp_by_off = (decomp.group_by("block_offset").agg([
        pl.col("market_movement_bps").mean().alias("mm_mean"),
        pl.col("execution_slippage_bps").mean().alias("es_mean"),
        pl.col("market_movement_bps").abs().sum().alias("mm_abs"),
        pl.col("execution_slippage_bps").abs().sum().alias("es_abs"),
    ]).sort("block_offset"))
    for row in decomp_by_off.iter_rows(named=True):
        total = row["mm_abs"] + row["es_abs"]
        frac = row["mm_abs"] / total if total > 0 else 0
        print(f"{row['block_offset']:6} {row['mm_mean']:+8.2f} {row['es_mean']:+8.2f} {100*frac:7.1f}%")

# %% [markdown]
# ## 5. Decay by Pair Type

# %%
by_pair = (df.group_by("pair_bucket").agg([
    pl.col("route_decay_bps").mean().alias("mean"),
    pl.col("route_decay_bps").median().alias("median"),
    pl.col("route_decay_bps").std().alias("std"),
    pl.col("route_decay_bps").quantile(0.95).alias("p95"),
    pl.col("quote_id").n_unique().alias("quotes"),
]).sort("mean", descending=True))

print(f"{'pair_bucket':>18} {'mean':>8} {'median':>8} {'std':>8} {'P95':>8} {'quotes':>7}")
for row in by_pair.iter_rows(named=True):
    print(f"{row['pair_bucket']:>18} {row['mean']:8.2f} {row['median']:8.2f} "
          f"{row['std']:8.2f} {row['p95']:8.2f} {row['quotes']:7d}")

# %% [markdown]
# ## 6. Decay by Hop Count

# %%
by_hops = (df.group_by("hop_count").agg([
    pl.col("route_decay_bps").mean().alias("mean"),
    pl.col("route_decay_bps").median().alias("median"),
    pl.col("route_decay_bps").std().alias("std"),
    pl.col("route_decay_bps").quantile(0.95).alias("p95"),
    pl.col("quote_id").n_unique().alias("quotes"),
]).sort("hop_count"))

print(f"{'hops':>6} {'mean':>8} {'median':>8} {'std':>8} {'P95':>8} {'quotes':>7}")
for row in by_hops.iter_rows(named=True):
    print(f"{row['hop_count']:6d} {row['mean']:8.2f} {row['median']:8.2f} "
          f"{row['std']:8.2f} {row['p95']:8.2f} {row['quotes']:7d}")

# %% [markdown]
# ## 7. Hop-Level: Decay by Protocol

# %%
by_proto = (hd_full.group_by("protocol").agg([
    pl.col("hop_decay_bps").mean().alias("mean"),
    pl.col("hop_decay_bps").median().alias("median"),
    pl.col("hop_decay_bps").std().alias("std"),
    pl.col("hop_decay_bps").quantile(0.95).alias("p95"),
    pl.len().alias("n"),
]).sort("mean", descending=True))

print(f"{'protocol':>15} {'mean':>8} {'median':>8} {'std':>8} {'P95':>8} {'n':>7}")
for row in by_proto.iter_rows(named=True):
    proto = row["protocol"] or "null"
    print(f"{proto:>15} {row['mean']:8.2f} {row['median']:8.2f} "
          f"{row['std']:8.2f} {row['p95']:8.2f} {row['n']:7d}")

# %% [markdown]
# ## 8. Hop-Level: Decay by Fee Tier

# %%
by_fee = (hd_full
    .filter(pl.col("fee_tier").is_not_null())
    .with_columns((pl.col("fee_tier") * 10000).round(0).cast(pl.Int32).alias("fee_bps"))
    .group_by("fee_bps").agg([
        pl.col("hop_decay_bps").mean().alias("mean"),
        pl.col("hop_decay_bps").median().alias("median"),
        pl.col("hop_decay_bps").std().alias("std"),
        pl.col("hop_decay_bps").quantile(0.95).alias("p95"),
        pl.len().alias("n"),
    ]).sort("fee_bps"))

print(f"{'fee_bps':>8} {'mean':>8} {'median':>8} {'std':>8} {'P95':>8} {'n':>7}")
for row in by_fee.iter_rows(named=True):
    print(f"{row['fee_bps']:8d} {row['mean']:8.2f} {row['median']:8.2f} "
          f"{row['std']:8.2f} {row['p95']:8.2f} {row['n']:7d}")

# %% [markdown]
# ## 9. Hop-Level: Pool Depth vs Decay

# %%
hd5 = hd_full.filter(pl.col("block_offset") == 5)
target = hd5["hop_decay_bps"].to_numpy()

print("Spearman correlations with hop_decay_bps (offset=5):\n")
print(f"{'feature':>25} {'rho':>8} {'p-value':>12} {'direction':>15}")
for col in ["depth_at_1pct", "depth_at_5pct", "spot_price",
            "token_price_in_native", "fee_tier"]:
    vals = hd5[col]
    if vals.dtype == pl.Utf8:
        numeric = vals.cast(pl.Float64, strict=False)
    else:
        numeric = vals.cast(pl.Float64, strict=False)

    mask = numeric.is_not_null() & numeric.is_not_nan()
    valid = mask.to_numpy()
    if valid.sum() < 100:
        print(f"{col:>25}   (insufficient data: {valid.sum()} valid)")
        continue

    x = numeric.to_numpy()[valid]
    y = target[valid]
    rho, pval = stats.spearmanr(x, y)
    direction = "↑ more decay" if rho > 0 else "↓ less decay"
    sig = "***" if pval < 0.001 else "**" if pval < 0.01 else "*" if pval < 0.05 else ""
    print(f"{col:>25} {rho:+8.4f} {pval:12.2e} {direction:>15} {sig}")

# %% [markdown]
# ## 10. Depth Quartile Analysis

# %%
depth_vals = (hd5
    .filter(pl.col("depth_at_1pct").is_not_null())
    .with_columns(pl.col("depth_at_1pct").cast(pl.Float64, strict=False).alias("depth_f64"))
    .filter(pl.col("depth_f64").is_not_null()))

if depth_vals.height > 200:
    q25, q50, q75 = [depth_vals["depth_f64"].quantile(q) for q in [0.25, 0.5, 0.75]]
    print(f"depth_at_1pct quartiles: Q1<{q25:.0f}, Q2<{q50:.0f}, Q3<{q75:.0f}")
    print(f"\n{'quartile':>20} {'mean':>8} {'median':>8} {'P95':>8} {'n':>6}")
    for label, lo, hi in [
        ("Q1 (shallowest)", 0, q25),
        ("Q2", q25, q50),
        ("Q3", q50, q75),
        ("Q4 (deepest)", q75, float("inf")),
    ]:
        subset = depth_vals.filter(
            (pl.col("depth_f64") >= lo) & (pl.col("depth_f64") < hi))
        if subset.height > 0:
            m = subset["hop_decay_bps"].mean()
            med = subset["hop_decay_bps"].median()
            p95 = subset["hop_decay_bps"].quantile(0.95)
            print(f"{label:>20} {m:8.2f} {med:8.2f} {p95:8.2f} {subset.height:6d}")

# %% [markdown]
# ## 11. Worst Pools

# %%
by_pool = (hd5
    .join(hs, on=["quote_id", "solver_id", "hop_index"], how="left")
    .group_by(["component_id", "protocol"]).agg([
        pl.col("hop_decay_bps").mean().alias("mean"),
        pl.col("hop_decay_bps").std().alias("std"),
        pl.col("hop_decay_bps").quantile(0.95).alias("p95"),
        pl.len().alias("n"),
    ])
    .filter(pl.col("n") >= 20)
    .sort("mean", descending=True))

print(f"Top 15 pools by mean decay (offset=5, n≥20):\n")
print(f"{'pool':>44} {'proto':>12} {'mean':>8} {'std':>8} {'P95':>8} {'n':>5}")
for row in by_pool.head(15).iter_rows(named=True):
    pool = (row["component_id"] or "?")[:42]
    proto = row["protocol"] or "?"
    print(f"{pool:>44} {proto:>12} {row['mean']:8.2f} {row['std']:8.2f} "
          f"{row['p95']:8.2f} {row['n']:5d}")

# %% [markdown]
# ## 12. Highest Decay Quotes

# %%
df10 = df.filter(pl.col("block_offset") == 10)
worst = (df10
    .sort("route_decay_bps", descending=True)
    .select(["quote_id", "route_decay_bps", "max_hop_decay_bps",
             "hop_count", "pair_bucket", "token_in_category",
             "token_out_category", "gas_estimate"])
    .head(15))

print("Top 15 quotes by decay at offset=10:\n")
print(f"{'decay':>8} {'max_hop':>8} {'hops':>5} {'pair':>18} {'in':>10} {'out':>10}")
for row in worst.iter_rows(named=True):
    print(f"{row['route_decay_bps']:8.1f} {row['max_hop_decay_bps']:8.1f} "
          f"{row['hop_count']:5d} {row['pair_bucket']:>18} "
          f"{row['token_in_category']:>10} {row['token_out_category']:>10}")

# %% [markdown]
# ## 13. Data Quality

# %%
print("Null rates per column:\n")
print(f"{'column':>30} {'nulls':>8} {'total':>8} {'pct':>6}")
for col in df.columns:
    nulls = df[col].null_count()
    if nulls > 0:
        print(f"{col:>30} {nulls:8d} {df.height:8d} {100*nulls/df.height:5.1f}%")

# %% [markdown]
# ## 14. Summary
#
# Key findings (update as more data accumulates):
#
# 1. **Decay is symmetric**: ~33% improve, ~33% unchanged, ~33% degrade.
#    Mean is near zero — most routes hold over 10 blocks.
#
# 2. **Market movement dominates**: ~83% of |decay| is unavoidable market
#    movement. Only ~17% is route-specific execution slippage.
#
# 3. **Fee tier is a strong predictor**: 5bps pools (hot pairs like ETH/USDC)
#    have the highest decay. Lower-fee pools serve more volatile pairs.
#
# 4. **Deeper pools have MORE decay** (counterintuitive): rho=+0.22.
#    Because deep pools serve high-volume volatile pairs, not because
#    depth causes decay.
#
# 5. **Pair type matters**: stable-mid pairs have the highest decay and
#    variance. stable-stable pairs are near zero.
#
# 6. **Tail risk is real**: P99 > 15 bps, with outliers > 80 bps. A small
#    number of routes account for most revert risk.

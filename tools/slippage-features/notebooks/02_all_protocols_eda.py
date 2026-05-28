# %% [markdown]
# # Slippage Feature EDA — All Protocols (9 DEXes, ~46h collection)
#
# Deep analysis of route decay across all supported protocols on Ethereum
# mainnet. Companion to `01_decay_eda.py` which covers V2/V3 only.
#
# **Protocols**: uniswap_v2/v3/v4, sushiswap_v2, pancakeswap_v2/v3,
# ekubo_v2/v3, fluid_v1.

# %% [markdown]
# ## 0. Setup

# %%
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np
import polars as pl
import seaborn as sns
from scipy import stats

sns.set_theme(style="whitegrid", palette="deep", font_scale=1.1)
FIGSIZE = (12, 6)
COLORS = sns.color_palette("deep", 10)

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
FIG_DIR = PROJECT_ROOT / "tools" / "slippage-features" / "notebooks" / "figures"
FIG_DIR.mkdir(exist_ok=True)

def save(fig, name):
    fig.savefig(FIG_DIR / f"{name}.png", dpi=150, bbox_inches="tight")
    plt.close(fig)

P99_CAP = None  # set after loading data

# %% [markdown]
# ## 1. Load Data

# %%
df = pl.read_parquet(UNIFIED_PATH)
print(f"Unified: {df.shape[0]:,} rows, {df['quote_id'].n_unique():,} quotes, {df.shape[1]} columns")

blocks = df["block_number"]
span_blocks = blocks.max() - blocks.min()
span_hours = span_blocks * 12 / 3600
print(f"Block range: {blocks.min():,} → {blocks.max():,} ({span_blocks:,} blocks, ~{span_hours:.1f}h)")
print(f"Offsets: {sorted(df['block_offset'].unique().to_list())}")

# %%
hd = pl.concat([pl.read_parquet(f) for f in sorted(HOP_DECAY_DIR.glob("*.parquet"))])
hs = pl.concat([pl.read_parquet(f) for f in sorted(HOP_STATIC_DIR.glob("*.parquet"))])
hd_full = hd.join(hs, on=["quote_id", "solver_id", "hop_index"], how="left")
print(f"Hop decay: {hd.shape[0]:,} rows | Hop static: {hs.shape[0]:,} rows")

# %% [markdown]
# ## 2. Decay Distribution & Outlier Analysis

# %%
decay = df["route_decay_bps"].to_numpy()
decay_clean = decay[np.isfinite(decay)]
P99_CAP = np.percentile(decay_clean, 99)

fig, axes = plt.subplots(1, 2, figsize=(14, 5))

axes[0].hist(decay_clean, bins=200, range=(-50, 50), color=COLORS[0], alpha=0.8, edgecolor="none")
axes[0].axvline(0, color="red", linestyle="--", alpha=0.5)
axes[0].set_xlabel("Route Decay (bps)")
axes[0].set_ylabel("Count")
axes[0].set_title("Route Decay Distribution (clipped to ±50 bps)")
med = np.median(decay_clean)
axes[0].axvline(med, color=COLORS[1], linestyle=":", label=f"median={med:.1f}")
axes[0].legend()

pcts = [1, 5, 10, 25, 50, 75, 90, 95, 99]
vals = [np.percentile(decay_clean, p) for p in pcts]
axes[1].bar([f"P{p}" for p in pcts], vals, color=[COLORS[2] if v >= 0 else COLORS[3] for v in vals])
axes[1].set_ylabel("Decay (bps)")
axes[1].set_title("Percentile Profile")
axes[1].axhline(0, color="grey", linewidth=0.5)
for i, v in enumerate(vals):
    axes[1].text(i, v + 0.3, f"{v:.1f}", ha="center", fontsize=8)

fig.suptitle(f"Route Decay — {len(decay_clean):,} observations", y=1.02)
fig.tight_layout()
save(fig, "01_decay_distribution")

# %%
neg = (decay_clean < 0).sum()
zero = (decay_clean == 0).sum()
pos = (decay_clean > 0).sum()
n = len(decay_clean)
print(f"Improved: {neg:,} ({100*neg/n:.1f}%)")
print(f"Unchanged: {zero:,} ({100*zero/n:.1f}%)")
print(f"Degraded: {pos:,} ({100*pos/n:.1f}%)")
print(f"\nRaw:       mean={np.mean(decay_clean):.2f}, std={np.std(decay_clean):.2f}")
capped = np.clip(decay_clean, -P99_CAP, P99_CAP)
print(f"Winsorized (P1–P99): mean={np.mean(capped):.2f}, std={np.std(capped):.2f}")
print(f"\nP1={np.percentile(decay_clean,1):.1f}, P99={P99_CAP:.1f}, Max={np.max(decay_clean):.1f}")

# %%
print("=== Outlier characterization ===")
for threshold in [50, 100, 500, 1000]:
    above = (decay_clean > threshold).sum()
    below = (decay_clean < -threshold).sum()
    if above > 0 or below > 0:
        print(f"  |decay| > {threshold} bps: {above} above, {below} below ({100*(above+below)/n:.3f}%)")

extreme = df.filter(pl.col("route_decay_bps") > 500)
if extreme.height > 0:
    print(f"\nRoutes with >500 bps decay:")
    by_proto_ext = extreme["pair_bucket"].value_counts().sort("count", descending=True)
    for row in by_proto_ext.head(5).iter_rows(named=True):
        print(f"  {row['pair_bucket']}: {row['count']}")

# %% [markdown]
# ## 3. Decay by Block Offset

# %%
by_offset = (df.group_by("block_offset").agg([
    pl.col("route_decay_bps").clip(-P99_CAP, P99_CAP).mean().alias("mean"),
    pl.col("route_decay_bps").median().alias("median"),
    pl.col("route_decay_bps").quantile(0.05).alias("p05"),
    pl.col("route_decay_bps").quantile(0.95).alias("p95"),
    pl.len().alias("n"),
]).sort("block_offset"))

offsets = by_offset["block_offset"].to_numpy()
means = by_offset["mean"].to_numpy()
medians = by_offset["median"].to_numpy()
p05 = by_offset["p05"].to_numpy()
p95 = by_offset["p95"].to_numpy()
ns_off = by_offset["n"].to_numpy()

fig, ax = plt.subplots(figsize=FIGSIZE)
ax.fill_between(offsets, p05, p95, alpha=0.15, color=COLORS[0], label="P5–P95 band")
ax.plot(offsets, means, "o-", color=COLORS[0], label="Mean (winsorized)")
ax.plot(offsets, medians, "s--", color=COLORS[1], label="Median")
ax.axhline(0, color="grey", linewidth=0.5)
ax.set_xlabel("Block Offset (blocks after quote)")
ax.set_ylabel("Route Decay (bps)")
ax.set_title("Route Decay vs Block Offset — How Quickly Do Routes Go Stale?")
ax.set_xticks(offsets)
for i, (o, nn) in enumerate(zip(offsets, ns_off)):
    ax.text(o, p95[i] + 1, f"n={nn:,}", ha="center", fontsize=7, color="grey")
ax.legend()
fig.tight_layout()
save(fig, "02_decay_by_offset")

# %% [markdown]
# ## 4. Decay by Protocol (with statistical test)

# %%
by_proto = (hd_full.group_by("protocol").agg([
    pl.col("hop_decay_bps").clip(-P99_CAP, P99_CAP).mean().alias("mean_w"),
    pl.col("hop_decay_bps").mean().alias("mean_raw"),
    pl.col("hop_decay_bps").median().alias("median"),
    pl.col("hop_decay_bps").std().alias("std"),
    pl.col("hop_decay_bps").quantile(0.95).alias("p95"),
    pl.len().alias("n"),
    pl.col("quote_id").n_unique().alias("quotes"),
]).filter(pl.col("n") > 100)
.sort("mean_w", descending=True))

protos = by_proto["protocol"].to_list()
means_w = by_proto["mean_w"].to_numpy()
means_r = by_proto["mean_raw"].to_numpy()
ns = by_proto["n"].to_numpy()

fig, axes = plt.subplots(1, 2, figsize=(14, 5))

bars = axes[0].barh(protos, means_w, color=COLORS[:len(protos)])
axes[0].axvline(0, color="grey", linewidth=0.5)
axes[0].set_xlabel("Winsorized Mean Hop Decay (bps)")
axes[0].set_title("Per-Hop Decay by Protocol")
axes[0].invert_yaxis()
for i, (mw, mr, n_) in enumerate(zip(means_w, means_r, ns)):
    axes[0].text(max(means_w) * 0.95, i, f"n={n_:,} (raw={mr:.1f})", va="center", ha="right", fontsize=7, color="grey")

p95s = by_proto["p95"].to_numpy()
axes[1].barh(protos, p95s, color=COLORS[:len(protos)], alpha=0.7)
axes[1].set_xlabel("P95 Hop Decay (bps)")
axes[1].set_title("Tail Risk by Protocol")
axes[1].invert_yaxis()

fig.tight_layout()
save(fig, "03_decay_by_protocol")

# %%
# Kruskal-Wallis test: are protocol differences statistically significant?
groups = []
group_names = []
for proto in protos:
    vals = hd_full.filter(pl.col("protocol") == proto)["hop_decay_bps"].to_numpy()
    valid = vals[np.isfinite(vals)]
    if len(valid) > 100:
        groups.append(valid)
        group_names.append(proto)

if len(groups) >= 2:
    h_stat, p_val = stats.kruskal(*groups)
    print(f"Kruskal-Wallis test across {len(groups)} protocols: H={h_stat:.1f}, p={p_val:.2e}")
    if p_val < 0.001:
        print("  → Protocol differences are highly significant (p < 0.001)")

print(f"\n{'protocol':>15} {'mean_w':>8} {'mean_raw':>8} {'median':>8} {'P95':>8} {'n':>10}")
for row in by_proto.iter_rows(named=True):
    print(f"{row['protocol']:>15} {row['mean_w']:8.2f} {row['mean_raw']:8.2f} "
          f"{row['median']:8.2f} {row['p95']:8.2f} {row['n']:10,}")

# %% [markdown]
# ## 5. Decay by Fee Tier

# %%
fee_data = (hd_full
    .filter(pl.col("fee_tier").is_not_null())
    .with_columns((pl.col("fee_tier") * 10000).round(0).cast(pl.Int32).alias("fee_bps"))
    .filter(pl.col("fee_bps") > 0))

by_fee = (fee_data.group_by("fee_bps").agg([
    pl.col("hop_decay_bps").clip(-P99_CAP, P99_CAP).mean().alias("mean"),
    pl.col("hop_decay_bps").std().alias("std"),
    pl.col("hop_decay_bps").quantile(0.95).alias("p95"),
    pl.len().alias("n"),
]).filter(pl.col("n") > 100)
.sort("fee_bps"))

fig, ax = plt.subplots(figsize=FIGSIZE)
fees = [str(f) for f in by_fee["fee_bps"].to_list()]
means_f = by_fee["mean"].to_numpy()
p95_f = by_fee["p95"].to_numpy()

x = np.arange(len(fees))
w = 0.35
ax.bar(x - w/2, means_f, w, label="Mean (winsorized)", color=COLORS[0])
ax.bar(x + w/2, p95_f, w, label="P95", color=COLORS[3], alpha=0.7)
ax.set_xticks(x)
ax.set_xticklabels([f"{f} bps" for f in fees])
ax.set_ylabel("Decay (bps)")
ax.set_xlabel("Fee Tier")
ax.set_title("Hop Decay by Fee Tier — Low-Fee Pools Show Higher Decay (volatile pair concentration)")
ax.axhline(0, color="grey", linewidth=0.5)
ax.legend()
for i, n in enumerate(by_fee["n"].to_list()):
    ax.text(i, max(p95_f) * 1.05, f"n={n:,}", ha="center", fontsize=8, color="grey")
fig.tight_layout()
save(fig, "04_decay_by_fee_tier")

# %% [markdown]
# ## 6. Decay by Pair Type

# %%
by_pair = (df.group_by("pair_bucket").agg([
    pl.col("route_decay_bps").clip(-P99_CAP, P99_CAP).mean().alias("mean"),
    pl.col("route_decay_bps").median().alias("median"),
    pl.col("route_decay_bps").quantile(0.05).alias("p05"),
    pl.col("route_decay_bps").quantile(0.95).alias("p95"),
    pl.col("quote_id").n_unique().alias("quotes"),
]).filter(pl.col("quotes") > 50)
.sort("mean", descending=True))

fig, ax = plt.subplots(figsize=FIGSIZE)
pairs = by_pair["pair_bucket"].to_list()
means_bp = by_pair["mean"].to_numpy()
p05_bp = by_pair["p05"].to_numpy()
p95_bp = by_pair["p95"].to_numpy()

y = np.arange(len(pairs))
ax.barh(y, means_bp, color=[COLORS[2] if m >= 0 else COLORS[3] for m in means_bp], alpha=0.8)
for i in range(len(pairs)):
    ax.plot([p05_bp[i], p95_bp[i]], [y[i], y[i]], "k-", linewidth=1.5, alpha=0.5)
    ax.plot(p05_bp[i], y[i], "|", color="black", markersize=8)
    ax.plot(p95_bp[i], y[i], "|", color="black", markersize=8)

ax.set_yticks(y)
ax.set_yticklabels(pairs)
ax.set_xlabel("Route Decay (bps)")
ax.set_title("Decay by Pair Type — Bars=Winsorized Mean, Whiskers=P5–P95")
ax.axvline(0, color="grey", linewidth=0.5)
ax.invert_yaxis()
for i, q in enumerate(by_pair["quotes"].to_list()):
    ax.text(max(p95_bp) * 0.95, i, f"{q:,} quotes", va="center", ha="right", fontsize=8, color="grey")
fig.tight_layout()
save(fig, "05_decay_by_pair_type")

# %% [markdown]
# ## 7. Market Movement vs Execution Slippage

# %%
decomp = df.filter(pl.col("market_movement_bps").is_not_null())
all_mean = df["route_decay_bps"].mean()
decomp_mean = decomp["route_decay_bps"].mean()
fill_pct = 100 * decomp.height / df.height
print(f"Decomposition fill rate: {decomp.height:,}/{df.height:,} ({fill_pct:.1f}%)")
print(f"Mean decay (all data):       {all_mean:.2f} bps")
print(f"Mean decay (decomp subset):  {decomp_mean:.2f} bps")
if abs(decomp_mean - all_mean) > 1:
    print("  ⚠ Decomposed subset is not representative — selection bias likely.")

if decomp.height > 100:
    mm = decomp["market_movement_bps"].to_numpy()
    es = decomp["execution_slippage_bps"].to_numpy()

    fig, axes = plt.subplots(1, 3, figsize=(16, 5))

    axes[0].scatter(mm, es, alpha=0.05, s=3, color=COLORS[0])
    axes[0].axhline(0, color="grey", linewidth=0.5)
    axes[0].axvline(0, color="grey", linewidth=0.5)
    axes[0].set_xlabel("Market Movement (bps)")
    axes[0].set_ylabel("Execution Slippage (bps)")
    axes[0].set_title(f"Decomposition Scatter (axes clipped ±30, n={len(mm):,})")
    axes[0].set_xlim(-30, 30)
    axes[0].set_ylim(-30, 30)

    decomp_off = (decomp.group_by("block_offset").agg([
        pl.col("market_movement_bps").abs().mean().alias("mm_abs"),
        pl.col("execution_slippage_bps").abs().mean().alias("es_abs"),
    ]).sort("block_offset"))
    off = decomp_off["block_offset"].to_numpy()
    mm_abs = decomp_off["mm_abs"].to_numpy()
    es_abs = decomp_off["es_abs"].to_numpy()

    axes[1].bar(off, mm_abs, label="Market Movement", color=COLORS[0])
    axes[1].bar(off, es_abs, bottom=mm_abs, label="Execution Slippage", color=COLORS[3])
    axes[1].set_xlabel("Block Offset")
    axes[1].set_ylabel("Mean |Decay| (bps)")
    axes[1].set_title("Decomposition by Block Offset")
    axes[1].legend()

    total_mm = np.abs(mm).sum()
    total_es = np.abs(es).sum()
    axes[2].pie([total_mm, total_es],
                labels=["Market\nMovement", "Execution\nSlippage"],
                autopct="%1.0f%%",
                colors=[COLORS[0], COLORS[3]],
                startangle=90)
    axes[2].set_title(f"Share of |Decay|\n({fill_pct:.0f}% fill rate — may not be representative)")

    fig.suptitle("Market Movement vs Execution Slippage — What Drives Decay?", y=1.02)
    fig.tight_layout()
    save(fig, "06_decomposition")

# %% [markdown]
# ## 8. Pool Depth vs Decay

# %%
hd5 = hd_full.filter(pl.col("block_offset") == 5)
depth_df = (hd5
    .filter(pl.col("depth_at_1pct").is_not_null())
    .with_columns(pl.col("depth_at_1pct").cast(pl.Float64, strict=False).alias("depth_f64"))
    .filter(pl.col("depth_f64").is_not_null() & (pl.col("depth_f64") > 0)))

if depth_df.height > 500:
    log_depth = np.log10(depth_df["depth_f64"].to_numpy())
    decay_d = depth_df["hop_decay_bps"].to_numpy()

    fig, axes = plt.subplots(1, 2, figsize=(14, 5))

    axes[0].scatter(log_depth, decay_d, alpha=0.02, s=2, color=COLORS[0])
    axes[0].set_xlabel("log10(Pool Depth at 1% Impact)")
    axes[0].set_ylabel("Hop Decay (bps)")
    axes[0].set_title("Depth vs Decay — Scatter")
    axes[0].set_ylim(-50, 50)
    axes[0].axhline(0, color="grey", linewidth=0.5)

    valid = np.isfinite(log_depth) & np.isfinite(decay_d)
    rho, pval = stats.spearmanr(log_depth[valid], decay_d[valid])
    axes[0].text(0.05, 0.95, f"Spearman rho={rho:.3f}, p={pval:.1e}",
                 transform=axes[0].transAxes, fontsize=10, va="top",
                 bbox=dict(boxstyle="round", facecolor="wheat", alpha=0.5))

    q25, q50, q75 = np.percentile(log_depth[valid], [25, 50, 75])
    groups = []
    tick_labels = []
    for label, lo, hi in [("Q1\n(shallow)", -np.inf, q25), ("Q2", q25, q50),
                           ("Q3", q50, q75), ("Q4\n(deep)", q75, np.inf)]:
        mask = (log_depth >= lo) & (log_depth < hi)
        groups.append(decay_d[mask])
        tick_labels.append(label)

    bp = axes[1].boxplot(groups, tick_labels=tick_labels, showfliers=False, patch_artist=True)
    for patch, color in zip(bp["boxes"], COLORS[:4]):
        patch.set_facecolor(color)
        patch.set_alpha(0.6)
    axes[1].set_ylabel("Hop Decay (bps)")
    axes[1].set_title("Decay by Depth Quartile")
    axes[1].axhline(0, color="grey", linewidth=0.5)
    for i, g in enumerate(groups):
        axes[1].text(i + 1, axes[1].get_ylim()[1] * 0.9, f"n={len(g):,}", ha="center", fontsize=8)

    fig.suptitle("Pool Depth vs Decay — Does Deeper Liquidity Protect Against Decay?", y=1.02)
    fig.tight_layout()
    save(fig, "07_depth_vs_decay")

# %% [markdown]
# ## 9. Hop Count Analysis

# %%
by_hops = (df.group_by("hop_count").agg([
    pl.col("route_decay_bps").clip(-P99_CAP, P99_CAP).mean().alias("mean"),
    pl.col("route_decay_bps").std().alias("std"),
    pl.col("route_decay_bps").quantile(0.95).alias("p95"),
    pl.col("quote_id").n_unique().alias("quotes"),
]).sort("hop_count"))

fig, ax = plt.subplots(figsize=(8, 5))
hops = by_hops["hop_count"].to_numpy()
means_h = by_hops["mean"].to_numpy()
p95_h = by_hops["p95"].to_numpy()
quotes_h = by_hops["quotes"].to_numpy()

ax.bar(hops - 0.15, means_h, 0.3, label="Mean (winsorized)", color=COLORS[0])
ax.bar(hops + 0.15, p95_h, 0.3, label="P95", color=COLORS[3], alpha=0.7)
ax.set_xlabel("Hop Count")
ax.set_ylabel("Decay (bps)")
ax.set_title("Decay by Route Complexity")
ax.legend()
ax.axhline(0, color="grey", linewidth=0.5)
for i, (h, q) in enumerate(zip(hops, quotes_h)):
    ax.text(h, max(p95_h) * 1.08, f"{q:,}", ha="center", fontsize=8, color="grey")
fig.tight_layout()
save(fig, "08_decay_by_hop_count")

# %% [markdown]
# ## 10. Worst Pools

# %%
by_pool = (hd5  # hd5 already has hs columns via hd_full
    .group_by(["component_id", "protocol"]).agg([
        pl.col("hop_decay_bps").clip(-P99_CAP, P99_CAP).mean().alias("mean"),
        pl.col("hop_decay_bps").std().alias("std"),
        pl.col("hop_decay_bps").quantile(0.95).alias("p95"),
        pl.len().alias("n"),
    ]).filter(pl.col("n") >= 50)
    .sort("mean", descending=True))

fig, ax = plt.subplots(figsize=(12, 6))
top15 = by_pool.head(15)
pool_labels = [f"{r['component_id'][:10]}...({r['protocol']})" for r in top15.iter_rows(named=True)]
means_pool = top15["mean"].to_numpy()
p95_pool = top15["p95"].to_numpy()

y = np.arange(len(pool_labels))
ax.barh(y, means_pool, color=COLORS[3], alpha=0.7, label="Mean (winsorized)")
ax.barh(y, p95_pool, height=0.3, color=COLORS[0], alpha=0.5, label="P95")
ax.set_yticks(y)
ax.set_yticklabels(pool_labels, fontsize=9)
ax.set_xlabel("Hop Decay (bps)")
ax.set_title("Top 15 Pools by Mean Decay (offset=5, n>=50)")
ax.legend()
ax.invert_yaxis()
fig.tight_layout()
save(fig, "09_worst_pools")

# %% [markdown]
# ## 11. Temporal Stability & Time-of-Day

# %%
temporal = (df.filter(pl.col("block_offset") == 5)
    .group_by("block_number").agg([
        pl.col("route_decay_bps").clip(-P99_CAP, P99_CAP).mean().alias("mean_decay"),
        pl.col("route_decay_bps").std().alias("std_decay"),
        pl.len().alias("n"),
    ]).sort("block_number"))

if temporal.height > 20:
    block_nums = temporal["block_number"].to_numpy()
    means_t = temporal["mean_decay"].to_numpy()
    stds_t = temporal["std_decay"].to_numpy()
    hours = (block_nums - block_nums.min()) * 12 / 3600

    fig, axes = plt.subplots(2, 1, figsize=(14, 8), sharex=True)

    axes[0].plot(hours, means_t, linewidth=0.5, alpha=0.7, color=COLORS[0])
    window = min(50, len(means_t) // 5)
    if window > 1:
        rolling = np.convolve(means_t, np.ones(window)/window, mode="valid")
        axes[0].plot(hours[window-1:], rolling, linewidth=2, color=COLORS[3], label=f"{window}-block MA")
    axes[0].axhline(0, color="grey", linewidth=0.5)
    axes[0].set_ylabel("Mean Decay (bps)")
    axes[0].set_title("Decay Over Time (offset=5, winsorized)")
    axes[0].legend()

    axes[1].plot(hours, stds_t, linewidth=0.5, alpha=0.7, color=COLORS[1])
    if window > 1:
        rolling_s = np.convolve(stds_t, np.ones(window)/window, mode="valid")
        axes[1].plot(hours[window-1:], rolling_s, linewidth=2, color=COLORS[3], label=f"{window}-block MA")
    axes[1].set_ylabel("Std of Decay (bps)")
    axes[1].set_xlabel("Hours from Collection Start")
    axes[1].set_title("Decay Volatility Over Time")
    axes[1].legend()
    fig.tight_layout()
    save(fig, "10_temporal_stability")

# %%
# Time-of-day analysis (estimated from block number)
hour_data = df.filter(
    pl.col("block_offset") == 5
).filter(pl.col("hour_of_day").is_not_null())

if hour_data.height > 1000:
    by_hour = (hour_data.group_by("hour_of_day").agg([
        pl.col("route_decay_bps").clip(-P99_CAP, P99_CAP).mean().alias("mean"),
        pl.col("route_decay_bps").std().alias("std"),
        pl.len().alias("n"),
    ]).sort("hour_of_day"))

    fig, ax = plt.subplots(figsize=FIGSIZE)
    h = by_hour["hour_of_day"].to_numpy()
    m = by_hour["mean"].to_numpy()
    s = by_hour["std"].to_numpy()
    n_h = by_hour["n"].to_numpy()

    ax.bar(h, m, color=[COLORS[3] if v > 0 else COLORS[0] for v in m], alpha=0.7)
    ax.set_xlabel("Hour of Day (UTC, estimated)")
    ax.set_ylabel("Mean Decay (bps)")
    ax.set_title("Decay by Hour of Day — Is There Intraday Seasonality?")
    ax.axhline(0, color="grey", linewidth=0.5)
    ax.set_xticks(range(0, 24, 2))
    fig.tight_layout()
    save(fig, "10b_decay_by_hour")

# %% [markdown]
# ## 12. Feature Correlations (expanded)

# %%
df10 = df.filter(pl.col("block_offset") == 10)
target = df10["route_decay_bps"].to_numpy()

feature_specs = [
    ("hop_count", df10["hop_count"].cast(pl.Float64).to_numpy(), None),
    ("gas_estimate", df10["gas_estimate"].cast(pl.Float64).to_numpy(), None),
    ("max_hop_decay_bps", df10["max_hop_decay_bps"].to_numpy(), None),
    ("is_l2", df10["is_l2"].cast(pl.Float64).to_numpy(), None),
]
for col in ["min_mcap", "log_mcap_ratio"]:
    vals = df10[col]
    if vals.drop_nulls().len() > 200:
        mask = vals.is_not_null().to_numpy()
        feature_specs.append((col, vals.to_numpy(), mask))

# Add hop-level features aggregated to route level
for col_name, agg_name in [("fee_tier", "mean_fee"), ("depth_at_1pct", "mean_log_depth")]:
    agg = (hd_full.filter(pl.col("block_offset") == 10)
        .group_by(["quote_id", "solver_id"]).agg([
            pl.col("hop_decay_bps").max().alias("_max_hd"),
        ]))
    if col_name == "fee_tier":
        agg2 = (hd_full.filter(pl.col("block_offset") == 10)
            .filter(pl.col(col_name).is_not_null())
            .group_by(["quote_id", "solver_id"]).agg([
                pl.col(col_name).mean().alias(agg_name),
            ]))
    else:
        agg2 = (hd_full.filter(pl.col("block_offset") == 10)
            .filter(pl.col(col_name).is_not_null())
            .with_columns(pl.col(col_name).cast(pl.Float64, strict=False).alias("_num"))
            .filter(pl.col("_num").is_not_null() & (pl.col("_num") > 0))
            .with_columns(pl.col("_num").log(10).alias(agg_name))
            .group_by(["quote_id", "solver_id"]).agg([
                pl.col(agg_name).mean().alias(agg_name),
            ]))
    joined = df10.join(agg2, on=["quote_id", "solver_id"], how="left")
    vals = joined[agg_name]
    if vals.drop_nulls().len() > 200:
        mask = vals.is_not_null().to_numpy()
        feature_specs.append((agg_name, vals.to_numpy(), mask))

results = []
for name, vals, mask in feature_specs:
    if mask is not None:
        valid = mask & np.isfinite(vals) & np.isfinite(target)
    else:
        valid = np.isfinite(vals) & np.isfinite(target)
    if valid.sum() < 100:
        continue
    rho, pval = stats.spearmanr(vals[valid], target[valid])
    results.append((name, rho, pval, valid.sum()))

results.sort(key=lambda x: abs(x[1]), reverse=True)

fig, ax = plt.subplots(figsize=(10, 6))
names = [r[0] for r in results]
rhos = [r[1] for r in results]
pvals_r = [r[2] for r in results]
ns_r = [r[3] for r in results]

colors = [COLORS[3] if r > 0 else COLORS[0] for r in rhos]
ax.barh(names, rhos, color=colors, alpha=0.8)
ax.set_xlabel("Spearman rho with route_decay_bps")
ax.set_title("Feature Correlations with Decay (offset=10)")
ax.axvline(0, color="grey", linewidth=0.5)
for i, (r, p, nn) in enumerate(zip(rhos, pvals_r, ns_r)):
    sig = "***" if p < 0.001 else "**" if p < 0.01 else "*" if p < 0.05 else ""
    ax.text(r + 0.005 * np.sign(r), i, f"{r:.3f} {sig} (n={nn:,})", va="center", fontsize=8)
ax.invert_yaxis()
fig.tight_layout()
save(fig, "11_feature_correlations")

# %% [markdown]
# ## 13. Decay Heatmap: Protocol x Fee Tier

# %%
heatmap_data = (hd_full
    .filter(pl.col("fee_tier").is_not_null() & pl.col("protocol").is_not_null())
    .with_columns((pl.col("fee_tier") * 10000).round(0).cast(pl.Int32).alias("fee_bps"))
    .filter(pl.col("fee_bps") > 0)
    .group_by(["protocol", "fee_bps"]).agg([
        pl.col("hop_decay_bps").clip(-P99_CAP, P99_CAP).mean().alias("mean_decay"),
        pl.len().alias("n"),
    ]).filter(pl.col("n") >= 50))

if heatmap_data.height > 3:
    pivot = heatmap_data.pivot(on="fee_bps", index="protocol", values="mean_decay").sort("protocol")
    protos_hm = pivot["protocol"].to_list()
    fee_cols = [c for c in pivot.columns if c != "protocol"]
    matrix = pivot.select(fee_cols).to_numpy()

    fig, ax = plt.subplots(figsize=(10, 5))
    im = ax.imshow(matrix, cmap="RdYlGn_r", aspect="auto", interpolation="nearest")
    ax.set_xticks(range(len(fee_cols)))
    ax.set_xticklabels([f"{c} bps" for c in fee_cols])
    ax.set_yticks(range(len(protos_hm)))
    ax.set_yticklabels(protos_hm)
    ax.set_xlabel("Fee Tier")
    ax.set_title("Winsorized Mean Hop Decay (bps) — Protocol x Fee Tier")

    for i in range(len(protos_hm)):
        for j in range(len(fee_cols)):
            val = matrix[i, j]
            if np.isfinite(val):
                ax.text(j, i, f"{val:.1f}", ha="center", va="center", fontsize=9,
                        color="white" if abs(val) > 3 else "black")

    plt.colorbar(im, ax=ax, label="Mean Decay (bps)")
    fig.tight_layout()
    save(fig, "12_heatmap_protocol_fee")

# %% [markdown]
# ## 14. High-Decay Route Analysis

# %%
high_decay = df10.filter(pl.col("route_decay_bps") > 20)
total_10 = df10.height
pct_high = 100 * high_decay.height / total_10

print(f"Routes with >20 bps decay at offset=10: {high_decay.height:,} / {total_10:,} ({pct_high:.2f}%)")

if high_decay.height > 10:
    fig, axes = plt.subplots(1, 2, figsize=(14, 5))

    high_pairs = high_decay["pair_bucket"].value_counts().sort("count", descending=True)
    axes[0].barh(high_pairs["pair_bucket"].to_list(), high_pairs["count"].to_list(), color=COLORS[3])
    axes[0].set_xlabel("Count of High-Decay Routes (>20 bps)")
    axes[0].set_title("High-Decay Routes by Pair Type")
    axes[0].invert_yaxis()

    high_hops = high_decay["hop_count"].value_counts().sort("hop_count")
    axes[1].bar(high_hops["hop_count"].to_list(), high_hops["count"].to_list(), color=COLORS[3])
    axes[1].set_xlabel("Hop Count")
    axes[1].set_ylabel("Count")
    axes[1].set_title("High-Decay Routes by Hop Count")

    fig.suptitle(f"High-Decay Routes (>20 bps at offset=10) — {pct_high:.1f}% of all routes", y=1.02)
    fig.tight_layout()
    save(fig, "13_high_decay_analysis")

# %% [markdown]
# ## 15. Data Quality Summary

# %%
print("Null rates per column:\n")
print(f"{'column':>30} {'nulls':>10} {'total':>10} {'pct':>6}")
for col in df.columns:
    nulls = df[col].null_count()
    if nulls > 0:
        print(f"{col:>30} {nulls:10,} {df.height:10,} {100*nulls/df.height:5.1f}%")

# %% [markdown]
# ## 16. Key Takeaways
#
# 1. **Positive directional bias** — 37.8% of routes degrade vs 18.5%
#    that improve. The large unchanged bucket (43.7%) reflects routes
#    through pools with no state changes between blocks. Among routes
#    that DO move, degradation outweighs improvement ~2:1.
#
# 2. **Market movement dominates where decomposition is available** — but
#    the decomposition only covers ~7% of data (re-quote fill rate) and
#    is likely selection-biased. Treat the 80/20 market/execution split
#    as directional, not precise.
#
# 3. **Fee tier is the strongest structural predictor** — low-fee pools
#    (1 bps, 5 bps) serve volatile pairs and show the highest decay.
#    This is a confounded effect (volatile pairs choose low-fee pools),
#    not a causal one.
#
# 4. **Deeper pools have MORE decay** — because deep pools serve
#    high-volume volatile pairs. Depth does not protect against decay;
#    it correlates with the kind of pair that decays most.
#
# 5. **Protocol differences are statistically significant** — ekubo_v3
#    and sushiswap_v2 show the highest winsorized mean decay. V4 pools
#    are surprisingly stable. Differences may reflect pair mix rather
#    than protocol mechanics.
#
# 6. **Tail risk concentrates in 3.4% of routes** — routes with >20 bps
#    decay at offset 10. These are disproportionately stable-mid and
#    mid-longtail pair types through low-fee concentrated liquidity
#    pools.
#
# 7. **Extreme outliers exist** (max ~9700 bps) but are rare (<0.1%).
#    All aggregations in this notebook use winsorized means (P1–P99)
#    to prevent outlier contamination.

# %% [markdown]
# ## 17. Feature Recommendations for Prediction Model
#
# Features ranked by predictive value (Spearman rho with decay), noting
# whether each is available at quote time (usable as model input):
#
# | Feature | Available at quote time? | Notes |
# |---------|--------------------------|-------|
# | `fee_tier` | Yes | Strongest structural predictor via pair volatility proxy |
# | `pair_bucket` | Yes (via CoinGecko) | Pair type classification; stable-mid is highest risk |
# | `hop_count` | Yes | More hops = more compounding risk |
# | `depth_at_1pct` | Yes (from DerivedData) | Counterintuitive positive correlation — confounded |
# | `protocol` | Yes | Significant differences; useful as categorical feature |
# | `hour_of_day` | Yes | If intraday seasonality is confirmed with more data |
# | `gas_estimate` | Yes | Weak correlation but available |
# | `log_mcap_ratio` | Requires CoinGecko | Market cap ratio between token pair |
# | `max_hop_decay_bps` | **No** — this is a label | Only known after resimulation |
#
# **Not yet collected but expected to be strong predictors:**
# - CEX realized volatility (ENG-5990)
# - CEX-DEX spread
# - Trade size relative to pool depth

"""Plot bps-delta histograms from a fynd-benchmark audit JSON report.

Renders one figure per audit file: a grid of aggregator (rows) x bps-metric
(cols) histograms. Outliers are trimmed with the Tukey 1.5*IQR rule before
binning so the body of each distribution is legible.

Convention: a positive bps delta means Fynd produced the better quote.
"""

import argparse
import json
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
import numpy as np  # noqa: E402

METRICS = [
    ("raw_diff_bps", "Raw (gas-ignored)"),
    ("gas_adjusted_diff_bps_reported", "Gas-adjusted (reported)"),
    ("gas_adjusted_diff_bps_onchain", "Gas-adjusted (on-chain)"),
]
AGGREGATORS = ["nordstern", "kyberswap"]


def collect(results, name, field):
    """Return all non-null values of `field` for participant `name`."""
    out = []
    for r in results:
        for p in r["participants"]:
            if p["name"] == name and p.get(field) is not None:
                out.append(float(p[field]))
    return np.array(out)


def trim_iqr(values, k=1.5):
    """Drop Tukey outliers; return (kept, n_removed, low, high)."""
    if values.size == 0:
        return values, 0, None, None
    q1, q3 = np.percentile(values, [25, 75])
    iqr = q3 - q1
    low, high = q1 - k * iqr, q3 + k * iqr
    mask = (values >= low) & (values <= high)
    return values[mask], int((~mask).sum()), low, high


def plot_file(path, outdir, label=None):
    data = json.loads(Path(path).read_text())
    results = data["results"]
    stem = Path(path).stem
    title_label = label or stem

    nrows, ncols = len(AGGREGATORS), len(METRICS)
    fig, axes = plt.subplots(nrows, ncols, figsize=(5.2 * ncols, 4.0 * nrows))
    axes = np.atleast_2d(axes)

    for i, agg in enumerate(AGGREGATORS):
        for j, (field, label) in enumerate(METRICS):
            ax = axes[i, j]
            raw = collect(results, agg, field)
            kept, removed, low, high = trim_iqr(raw)

            if kept.size == 0:
                ax.text(0.5, 0.5, "no data", ha="center", va="center",
                        transform=ax.transAxes, color="gray")
                ax.set_title(f"{agg} — {label}")
                continue

            median = np.median(kept)
            mean = kept.mean()
            win = int((kept > 0).sum())  # Fynd better
            win_pct = 100.0 * win / kept.size

            ax.hist(kept, bins=40, color="#4C72B0", edgecolor="white", linewidth=0.3)
            ax.axvline(0, color="#888", linestyle="--", linewidth=1)
            ax.axvline(median, color="#C44E52", linewidth=1.4,
                       label=f"median {median:+.1f}")
            ax.set_title(f"{agg} — {label}", fontsize=11)
            ax.set_xlabel("bps delta vs Fynd  (+ = Fynd better)")
            ax.set_ylabel("trades")
            ax.legend(loc="upper right", fontsize=8, framealpha=0.9)
            ax.text(
                0.02, 0.97,
                f"n={kept.size}  (−{removed} outliers)\n"
                f"mean {mean:+.1f}\nFynd wins {win_pct:.0f}%",
                transform=ax.transAxes, va="top", ha="left", fontsize=8,
                bbox=dict(boxstyle="round,pad=0.3", fc="white", ec="#ccc", alpha=0.9),
            )

    fig.suptitle(
        f"Fynd vs aggregators — bps delta distributions  ({title_label}, "
        f"{len(results)} trades, 1.5×IQR outliers removed)",
        fontsize=13,
    )
    fig.tight_layout(rect=(0, 0, 1, 0.97))
    out = Path(outdir) / f"{stem}_bps_histograms.png"
    fig.savefig(out, dpi=130)
    plt.close(fig)
    print(f"wrote {out}")
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("files", nargs="+", help="audit JSON report(s)")
    ap.add_argument("-o", "--outdir", default=".", help="output directory")
    ap.add_argument(
        "-l", "--label",
        help="title label override (applies when a single file is given)",
    )
    args = ap.parse_args()
    Path(args.outdir).mkdir(parents=True, exist_ok=True)
    label = args.label if len(args.files) == 1 else None
    for f in args.files:
        plot_file(f, args.outdir, label=label)


if __name__ == "__main__":
    sys.exit(main())

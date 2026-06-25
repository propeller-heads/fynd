"""Explain Fynd audit win/loss using protocol, route, trade-size, and gas data.

Emits two figures per audit JSON report:

  <stem>_why_winloss.png  - bps delta vs route protocol / length / trade size
  <stem>_gas.png          - gas drag, gas-estimate accuracy, gas by protocol

Convention: a positive bps delta means Fynd produced the better quote. The bps
deltas live on the aggregator participants (vs Fynd); each successful Fynd quote
contributes one observation per aggregator, sharing Fynd's route context.
"""

import argparse
import json
from collections import defaultdict
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
import numpy as np  # noqa: E402

AGGREGATORS = ("nordstern", "kyberswap")
MIN_GROUP = 20  # drop boxplot groups with fewer observations than this
MIN_SCATTER_LEGEND = 8  # protocols below this point count are drawn gray, off-legend
BLUE, RED, GRAY = "#4C72B0", "#C44E52", "#888"


def observations(results):
    """Yield one record per (successful Fynd trade, aggregator) pairing."""
    for r in results:
        fynd = next((p for p in r["participants"] if p["name"] == "fynd"), None)
        if not fynd or fynd["status"] != "success":
            continue
        route = fynd.get("route") or []
        protos = fynd.get("protocols") or []
        ctx = {
            "pct": r.get("amount_percentile_idx"),
            "route_len": len(route),
            "fynd_protos": sorted({p for p, _ in route} or set(protos)),
            "primary_proto": route[0][0] if route else (protos[0] if protos else None),
            "gas_reported": fynd.get("gas_units"),
            "gas_onchain": fynd.get("eth_call_gas_used"),
        }
        for agg in r["participants"]:
            if agg["name"] in AGGREGATORS:
                yield {
                    "agg": agg["name"],
                    "raw": agg.get("raw_diff_bps"),
                    "onchain": agg.get("gas_adjusted_diff_bps_onchain"),
                    "reported": agg.get("gas_adjusted_diff_bps_reported"),
                    **ctx,
                }


def _boxes(ax, groups, title, xlabel, sort_by_median=False):
    """Draw horizontal boxplots for {label: values}, annotating n per group."""
    items = [(k, np.asarray(v)) for k, v in groups.items() if len(v) >= MIN_GROUP]
    if not items:
        ax.text(0.5, 0.5, "no data", ha="center", va="center", transform=ax.transAxes)
        ax.set_title(title)
        return
    if sort_by_median:
        items.sort(key=lambda kv: np.median(kv[1]))
    else:
        items.sort(key=lambda kv: kv[0])
    labels = [f"{k}  (n={len(v)})" for k, v in items]
    data = [v for _, v in items]
    ax.boxplot(data, orientation="horizontal", showfliers=False, widths=0.6,
               medianprops=dict(color=RED, linewidth=1.6))
    ax.set_yticklabels(labels, fontsize=8)
    ax.axvline(0, color=GRAY, linestyle="--", linewidth=1)
    ax.set_title(title, fontsize=11)
    ax.set_xlabel(xlabel)


def fig_why(records, stem, label, outdir):
    """Figure A: bps delta vs protocol presence, route length, and trade size."""
    fig, axes = plt.subplots(2, 2, figsize=(15, 10))

    by_proto = defaultdict(list)
    by_len = defaultdict(list)
    by_pct = defaultdict(list)
    winrate = {a: defaultdict(list) for a in (*AGGREGATORS, "pooled")}
    for o in records:
        if o["onchain"] is None:
            continue
        for p in o["fynd_protos"]:
            by_proto[p].append(o["onchain"])
        by_len[o["route_len"]].append(o["onchain"])
        if o["pct"] is not None:
            by_pct[o["pct"]].append(o["onchain"])
            won = o["onchain"] > 0
            winrate[o["agg"]][o["pct"]].append(won)
            winrate["pooled"][o["pct"]].append(won)

    _boxes(axes[0, 0], by_proto, "bps delta by Fynd protocol in route",
           "gas-adj on-chain bps  (+ = Fynd better)", sort_by_median=True)
    _boxes(axes[0, 1], {f"{k} hops": v for k, v in by_len.items()},
           "bps delta by route length", "gas-adj on-chain bps  (+ = Fynd better)")
    _boxes(axes[1, 0], {f"p{k}": v for k, v in by_pct.items()},
           "bps delta by trade-size percentile", "gas-adj on-chain bps  (+ = Fynd better)")

    ax = axes[1, 1]
    for name, style in (("nordstern", "-o"), ("kyberswap", "-s"), ("pooled", "-^")):
        xs = sorted(winrate[name])
        ys = [100 * np.mean(winrate[name][x]) for x in xs]
        ax.plot(xs, ys, style, label=name, linewidth=1.4, markersize=5)
    ax.axhline(50, color=GRAY, linestyle="--", linewidth=1)
    ax.set_title("Fynd win-rate by trade-size percentile", fontsize=11)
    ax.set_xlabel("amount percentile (0 = smallest, 9 = largest)")
    ax.set_ylabel("Fynd win-rate %  (on-chain)")
    ax.set_ylim(0, 100)
    ax.legend(fontsize=9)

    fig.suptitle(f"Why Fynd wins/loses — {label}", fontsize=14)
    fig.tight_layout(rect=(0, 0, 1, 0.97))
    out = Path(outdir) / f"{stem}_why_winloss.png"
    fig.savefig(out, dpi=130)
    plt.close(fig)
    print(f"wrote {out}")


def _gas_drag_panel(ax, records):
    groups = defaultdict(list)
    for o in records:
        if o["raw"] is not None and o["onchain"] is not None:
            groups[o["agg"]].append(o["raw"] - o["onchain"])
            groups["pooled"].append(o["raw"] - o["onchain"])
    _boxes(ax, groups, "Gas drag = raw − on-chain-adjusted (bps)",
           "bps of Fynd edge lost to gas  (+ = gas hurts Fynd)")


def _gas_drag_by_size(ax, records):
    by_pct = defaultdict(list)
    for o in records:
        if o["raw"] is not None and o["onchain"] is not None and o["pct"] is not None:
            by_pct[o["pct"]].append(o["raw"] - o["onchain"])
    _boxes(ax, {f"p{k}": v for k, v in by_pct.items()},
           "Gas drag by trade-size percentile", "bps lost to gas  (+ = gas hurts Fynd)")


def _gas_accuracy_panel(ax, records):
    # Each trade appears once per aggregator; dedup, then group by first-leg protocol.
    seen = set()
    by_proto = defaultdict(lambda: ([], []))
    for o in records:
        key = (o["pct"], o["route_len"], o["gas_reported"], o["gas_onchain"])
        if o["gas_reported"] is None or o["gas_onchain"] is None or key in seen:
            continue
        seen.add(key)
        xs, ys = by_proto[o.get("primary_proto") or "unknown"]
        xs.append(o["gas_onchain"]); ys.append(o["gas_reported"])

    ordered = sorted(by_proto.items(), key=lambda kv: len(kv[1][0]), reverse=True)
    cmap = plt.get_cmap("tab20")
    hi, color_idx = 1, 0
    for _, (xs, ys) in ordered:
        hi = max(hi, max(xs), max(ys))
    for proto, (xs, ys) in ordered:
        if len(xs) >= MIN_SCATTER_LEGEND:
            ax.scatter(xs, ys, s=12, alpha=0.6, color=cmap(color_idx % 20),
                       label=f"{proto} (n={len(xs)})")
            color_idx += 1
        else:
            ax.scatter(xs, ys, s=12, alpha=0.4, color="#bbbbbb")
    ax.plot([0, hi], [0, hi], color=GRAY, linestyle="--", linewidth=1, label="reported = actual")
    ax.set_title("Fynd gas estimate vs on-chain (by first-leg protocol)", fontsize=11)
    ax.set_xlabel("on-chain gas used")
    ax.set_ylabel("reported gas units")
    ax.legend(fontsize=7, ncol=2, loc="upper left")


def _gas_by_proto_panel(ax, records):
    by_proto = defaultdict(list)
    seen = set()
    for o in records:
        key = (o["pct"], o["route_len"], o["gas_onchain"])
        if o["gas_onchain"] is None or key in seen:
            continue
        seen.add(key)
        for p in o["fynd_protos"]:
            by_proto[p].append(o["gas_onchain"])
    _boxes(ax, by_proto, "On-chain gas by Fynd protocol in route",
           "on-chain gas used", sort_by_median=True)


def fig_gas(records, stem, label, outdir):
    """Figure B: gas drag, gas-estimate accuracy, and gas by protocol."""
    records = list(records)
    fig, axes = plt.subplots(2, 2, figsize=(15, 10))
    _gas_drag_panel(axes[0, 0], records)
    _gas_drag_by_size(axes[0, 1], records)
    _gas_accuracy_panel(axes[1, 0], records)
    _gas_by_proto_panel(axes[1, 1], records)
    fig.suptitle(f"Gas economics — {label}", fontsize=14)
    fig.tight_layout(rect=(0, 0, 1, 0.97))
    out = Path(outdir) / f"{stem}_gas.png"
    fig.savefig(out, dpi=130)
    plt.close(fig)
    print(f"wrote {out}")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("file", help="audit JSON report")
    ap.add_argument("-o", "--outdir", default=".", help="output directory")
    ap.add_argument("-l", "--label", help="title label (defaults to filename stem)")
    args = ap.parse_args()
    Path(args.outdir).mkdir(parents=True, exist_ok=True)
    data = json.loads(Path(args.file).read_text())
    stem = Path(args.file).stem
    label = args.label or stem
    records = list(observations(data["results"]))
    fig_why(records, stem, label, args.outdir)
    fig_gas(records, stem, label, args.outdir)


if __name__ == "__main__":
    main()
